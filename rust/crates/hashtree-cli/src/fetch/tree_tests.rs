use super::{BlossomClient, Fetcher, HashtreeStore, Keys};
use axum::{extract::Path, http::StatusCode, routing::get, Router};
use hashtree_config::StorageBackend;
use hashtree_core::{sha256, DirEntry, HashTree, HashTreeConfig, LinkType, MemoryStore, Store};
use std::sync::Arc;
use tempfile::tempdir;

const STORE_BYTES: usize = 32 * 1024 * 1024;

mod chunked_directory;

async fn serve(source: Arc<MemoryStore>) -> (String, tokio::task::JoinHandle<()>) {
    let app = Router::new().route(
        "/*path",
        get(move |Path(path): Path<String>| {
            let source = source.clone();
            async move {
                let name = path.rsplit('/').next().unwrap().trim_end_matches(".bin");
                if let Ok(hash) = hashtree_core::from_hex(name) {
                    if let Some(data) = source.get(&hash).await.unwrap() {
                        return (StatusCode::OK, data);
                    }
                }
                (StatusCode::NOT_FOUND, Vec::new())
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{address}"), task)
}

fn fetcher(servers: Vec<String>) -> Fetcher {
    Fetcher::with_client(BlossomClient::new_empty(Keys::generate()).with_read_servers(servers))
}

#[tokio::test]
async fn restores_named_manifest_even_when_its_bytes_are_also_a_literal_file_chunk() {
    let source = Arc::new(MemoryStore::new());
    let files = HashTree::new(
        HashTreeConfig::new(source.clone())
            .public()
            .with_chunk_size(64),
    );
    let bytes: Vec<u8> = (0..93).collect();
    let (file, size) = files.put(&bytes).await.unwrap();
    let manifest = source.get(&file.hash).await.unwrap().unwrap();
    assert_eq!(manifest.len() as u64, size);

    let literal_writer = HashTree::new(
        HashTreeConfig::new(source.clone())
            .public()
            .with_chunk_size(manifest.len()),
    );
    let mut literal_bytes = manifest.clone();
    literal_bytes.push(42);
    let (literal, literal_size) = literal_writer.put(&literal_bytes).await.unwrap();
    let root = files
        .put_directory(vec![
            DirEntry::from_cid("literal", &literal)
                .with_size(literal_size)
                .with_link_type(LinkType::File),
            DirEntry::from_cid("named", &file).with_size(size),
        ])
        .await
        .unwrap();
    let (url, server) = serve(source).await;

    for preseed in [false, true] {
        let dir = tempdir().unwrap();
        let dst = HashtreeStore::with_options_and_backend(
            dir.path(),
            None,
            STORE_BYTES as u64,
            false,
            &StorageBackend::Fs,
        )
        .unwrap();
        if preseed {
            dst.put_cached_blob(&manifest).unwrap();
        }
        fetcher(vec![url.clone()])
            .fetch_cid_tree(&dst, &root)
            .await
            .unwrap();
        let restored = HashTree::new(HashTreeConfig::new(dst.store_arc()));
        for (name, expected) in [("literal", &literal_bytes), ("named", &bytes)] {
            let child = restored.resolve_path(&root, name).await.unwrap().unwrap();
            assert_eq!(
                &restored.get(&child, None).await.unwrap().unwrap(),
                expected
            );
        }
        assert_eq!(
            fetcher(vec![]).fetch_cid_tree(&dst, &root).await.unwrap(),
            (0, 0)
        );
    }
    server.abort();
    let _ = server.await;
}

#[cfg(feature = "lmdb")]
fn open_pool(dir: &tempfile::TempDir) -> HashtreeStore {
    HashtreeStore::with_options_and_backend(
        dir.path(),
        None,
        STORE_BYTES as u64,
        false,
        &StorageBackend::Lmdb,
    )
    .unwrap()
}

#[cfg(feature = "lmdb")]
fn remove_member_body(dir: &tempfile::TempDir, hash: &[u8; 32]) {
    // The pool must be closed so only the member changes, leaving its catalog row intact.
    let member = hashtree_lmdb::LmdbBlobStore::with_exact_map_size_and_external_blob_options(
        dir.path().join("blobs"),
        STORE_BYTES,
        None,
    )
    .unwrap();
    assert!(member.delete_sync(hash).unwrap());
}

#[cfg(feature = "lmdb")]
#[tokio::test]
async fn catalog_size_without_member_bytes_requires_a_source_and_is_repaired() {
    let dir = tempdir().unwrap();
    let bytes = b"first block, followed by another";
    let chunk = &bytes[..16];
    let missing = sha256(chunk);
    let root = {
        let dst = open_pool(&dir);
        let tree = HashTree::new(
            HashTreeConfig::new(dst.store_arc())
                .public()
                .with_chunk_size(16),
        );
        tree.put(bytes).await.unwrap().0
    };
    remove_member_body(&dir, &missing);
    let dst = open_pool(&dir);
    assert_eq!(dst.blob_size(&missing).unwrap(), Some(chunk.len() as u64));
    assert!(dst.get_blob(&missing).unwrap().is_none());
    assert!(fetcher(vec![]).fetch_cid_tree(&dst, &root).await.is_err());

    let source = Arc::new(MemoryStore::new());
    source.put(missing, chunk.to_vec()).await.unwrap();
    let (url, server) = serve(source).await;
    assert_eq!(
        fetcher(vec![url])
            .fetch_cid_tree(&dst, &root)
            .await
            .unwrap(),
        (1, chunk.len() as u64)
    );
    let restored = HashTree::new(HashTreeConfig::new(dst.store_arc()));
    assert_eq!(restored.get(&root, None).await.unwrap().unwrap(), bytes);
    server.abort();
    let _ = server.await;
}

#[cfg(feature = "lmdb")]
#[tokio::test]
async fn missing_catalogued_file_manifest_restores_its_chunks_for_offline_read() {
    let source = Arc::new(MemoryStore::new());
    let tree = HashTree::new(
        HashTreeConfig::new(source.clone())
            .public()
            .with_chunk_size(16),
    );
    let bytes = b"0123456789abcdefABCDEFGHIJKLMNOP";
    let (file, size) = tree.put(bytes).await.unwrap();
    let root = tree
        .put_directory(vec![DirEntry::from_cid("note", &file).with_size(size)])
        .await
        .unwrap();
    let manifest = source.get(&file.hash).await.unwrap().unwrap();
    assert_ne!(manifest.len() as u64, size);

    let dir = tempdir().unwrap();
    {
        let dst = open_pool(&dir);
        let root_body = source.get(&root.hash).await.unwrap().unwrap();
        dst.put_cached_blob(&root_body).unwrap();
        dst.put_cached_blob(&manifest).unwrap();
    }
    remove_member_body(&dir, &file.hash);
    let dst = open_pool(&dir);
    assert_eq!(
        dst.blob_size(&file.hash).unwrap(),
        Some(manifest.len() as u64)
    );
    assert!(dst.get_blob(&file.hash).unwrap().is_none());
    assert!(fetcher(vec![]).fetch_cid_tree(&dst, &root).await.is_err());

    let (url, server) = serve(source).await;
    let fetched = fetcher(vec![url])
        .fetch_cid_tree(&dst, &root)
        .await
        .unwrap();
    assert_eq!(fetched, (3, manifest.len() as u64 + size));
    server.abort();
    let _ = server.await;
    drop(dst);

    let offline = open_pool(&dir);
    assert_eq!(
        fetcher(vec![])
            .fetch_cid_tree(&offline, &root)
            .await
            .unwrap(),
        (0, 0)
    );
    let restored = HashTree::new(HashTreeConfig::new(offline.store_arc()));
    let named = restored.resolve_path(&root, "note").await.unwrap().unwrap();
    assert_eq!(named, file);
    assert_eq!(restored.get(&named, None).await.unwrap().unwrap(), bytes);
}

#[cfg(feature = "s3")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restores_archive_only_descendant_before_consulting_blossom() {
    const CHILD: &str = "HTREE_TEST_FETCH_ARCHIVE_CHILD";
    // The SDK loads credentials from process environment. Isolate the fake
    // credentials and config so the parallel test suite never observes them.
    if std::env::var_os(CHILD).is_none() {
        let home = tempdir().unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "fetch::tree_tests::restores_archive_only_descendant_before_consulting_blossom",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("AWS_ACCESS_KEY_ID", "fixture")
            .env("AWS_SECRET_ACCESS_KEY", "fixture")
            .env_remove("AWS_SESSION_TOKEN")
            .env_remove("AWS_PROFILE")
            .env("AWS_CONFIG_FILE", home.path().join("config"))
            .env(
                "AWS_SHARED_CREDENTIALS_FILE",
                home.path().join("credentials"),
            )
            .env("AWS_EC2_METADATA_DISABLED", "true")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }
    let source = Arc::new(MemoryStore::new());
    let tree = HashTree::new(HashTreeConfig::new(source.clone()).public());
    let bytes = b"only in the archive";
    let (child, size) = tree.put(bytes).await.unwrap();
    let root = tree
        .put_directory(vec![DirEntry::from_cid("file", &child).with_size(size)])
        .await
        .unwrap();
    let root_body = source.get(&root.hash).await.unwrap().unwrap();
    let (url, server) = serve(source).await;
    let dir = tempdir().unwrap();
    let config = crate::config::S3Config {
        endpoint: url,
        bucket: "archive".into(),
        prefix: None,
        region: "us-east-1".into(),
        access_key: None,
        secret_key: None,
        public_url: None,
    };
    let dst = HashtreeStore::with_options_and_backend(
        dir.path(),
        Some(&config),
        STORE_BYTES as u64,
        false,
        &StorageBackend::Fs,
    )
    .unwrap();
    let local = dst.router().local_store();
    local.put_sync(root.hash, &root_body).unwrap();
    assert!(local.get_sync(&child.hash).unwrap().is_none());
    assert_eq!(
        fetcher(vec![]).fetch_cid_tree(&dst, &root).await.unwrap(),
        (0, 0)
    );
    assert_eq!(local.get_sync(&child.hash).unwrap().unwrap(), bytes);
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn loading_local_encrypted_file_checks_each_chunk_key() {
    let dir = tempdir().unwrap();
    let store = HashtreeStore::with_options_and_backend(
        dir.path(),
        None,
        STORE_BYTES as u64,
        false,
        &StorageBackend::Fs,
    )
    .unwrap();
    let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).with_chunk_size(4));
    let bytes = b"abcdefgh";
    let (valid, _) = tree.put(bytes).await.unwrap();
    let load = fetcher(vec![]);
    assert_eq!(
        load.fetch_cid_tree_with_progress(&store, &valid, None)
            .await
            .unwrap(),
        (0, 0)
    );
    assert_eq!(tree.get(&valid, None).await.unwrap().unwrap(), bytes);

    // Both ciphertexts are present. Only the first link's key is wrong.
    let mut node = tree.get_node(&valid).await.unwrap().unwrap();
    let other_key = node.links[1].key;
    assert_ne!(node.links[0].key, other_key);
    node.links[0].key = other_key;
    let leaf_hash = hashtree_core::to_hex(&node.links[0].hash);
    let encoded = hashtree_core::encode_tree_node(&node).unwrap();
    let mismatched = hashtree_core::Cid::public(tree.put_blob(&encoded).await.unwrap());
    assert!(tree.get(&mismatched, None).await.is_err());
    let error = load
        .fetch_cid_tree_with_progress(&store, &mismatched, None)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains(&leaf_hash),
        "expected the unreadable chunk: {error}"
    );
}
