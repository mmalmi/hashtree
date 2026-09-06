use anyhow::{Context, Result};
use futures::stream::{self, StreamExt};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use crate::config::ensure_keys_string;
use crate::fetch::{FetchConfig, Fetcher};
use crate::HashtreeStore;
use hashtree_core::{to_hex, Cid, HashTree, HashTreeConfig, Link, LinkType, Store, TreeNode};

const BLOSSOM_PUSH_CONCURRENCY: usize = 16;
const BLOSSOM_PUSH_PROGRESS_EVERY: usize = 512;

fn parse_root_cid(cid_str: &str) -> Result<Cid> {
    Cid::parse(cid_str).map_err(|e| anyhow::anyhow!("Invalid CID '{}': {}", cid_str, e))
}

fn child_cid(parent: &Cid, link: &Link) -> Cid {
    let inherits_parent_key = link
        .name
        .as_deref()
        .map(|name| {
            name.starts_with("_chunk_")
                || (name.starts_with('_') && name.chars().count() == 2 && link.link_type.is_tree())
        })
        .unwrap_or(false);

    Cid {
        hash: link.hash,
        key: link.key.or(if inherits_parent_key {
            parent.key
        } else {
            None
        }),
    }
}

async fn ensure_local_blob_for_push(
    store: &HashtreeStore,
    fetcher: Option<&Fetcher>,
    cid: &Cid,
) -> Result<()> {
    if store.get_blob(&cid.hash)?.is_some() {
        return Ok(());
    }

    if let Some(fetcher) = fetcher {
        let data = fetcher
            .fetch_chunk(&to_hex(&cid.hash))
            .await
            .with_context(|| format!("failed to hydrate missing local blob {}", cid))?;
        store
            .put_blob(&data)
            .with_context(|| format!("failed to persist hydrated blob {}", cid))?;
        if store.get_blob(&cid.hash)?.is_some() {
            return Ok(());
        }
    }

    anyhow::bail!("missing local blob while pushing DAG: {}", cid);
}

async fn node_for_push<S: Store>(
    tree: &HashTree<S>,
    cid: &Cid,
    blob_size: Option<u64>,
) -> Result<Option<TreeNode>> {
    if let Some(size) = blob_size {
        if let Some(data) = tree.get_blob(&cid.hash).await? {
            // Match the core file reader: a Blob's declared size describes its
            // plaintext leaf, regardless of the writer's chunk size. Authenticate
            // keyed bytes before treating tree-shaped file contents as a leaf.
            let plaintext_size = match cid.key.as_ref() {
                Some(key) => hashtree_core::crypto::decrypt_chk(&data, key)
                    .ok()
                    .map(|plaintext| plaintext.len() as u64),
                None => Some(data.len() as u64),
            };
            if plaintext_size == Some(size) {
                return Ok(None);
            }
        }
    }

    tree.get_node(cid)
        .await
        .map_err(|err| anyhow::anyhow!("Failed to inspect {cid}: {err}"))
}

pub(crate) async fn collect_cids_for_push(
    store: &HashtreeStore,
    root_cid: Cid,
    fetcher: Option<&Fetcher>,
) -> Result<Vec<Cid>> {
    let mut cids_to_push = Vec::new();
    // A shared CID can be both literal file bytes and a structural node.
    // Traverse each interpretation, but upload each block only once.
    let mut visited = HashSet::new();
    let mut collected = HashSet::new();
    let mut queue = vec![(root_cid, None)];
    let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());

    while let Some((cid, blob_size)) = queue.pop() {
        if !visited.insert((cid.hash, blob_size)) {
            continue;
        }

        ensure_local_blob_for_push(store, fetcher, &cid).await?;
        if collected.insert(cid.hash) {
            cids_to_push.push(cid.clone());
        }

        let node = node_for_push(&tree, &cid, blob_size).await?;

        if let Some(node) = node {
            for link in &node.links {
                queue.push((
                    child_cid(&cid, link),
                    (link.link_type == LinkType::Blob).then_some(link.size),
                ));
            }
        }
    }

    Ok(cids_to_push)
}

fn matching_old_child<'a>(
    old_links: &'a [Link],
    new_index: usize,
    new_link: &Link,
) -> Option<&'a Link> {
    if let Some(name) = new_link.name.as_deref() {
        old_links
            .iter()
            .find(|old_link| old_link.name.as_deref() == Some(name))
    } else {
        old_links
            .get(new_index)
            .filter(|old_link| old_link.name.is_none())
    }
}

pub(crate) async fn collect_incremental_cids_for_push(
    store: &HashtreeStore,
    root_cid: Cid,
    previous_root_cid: Cid,
    fetcher: Option<&Fetcher>,
) -> Result<Vec<Cid>> {
    let mut cids_to_push = Vec::new();
    let mut visited_new = HashSet::new();
    let mut collected = HashSet::new();
    let mut queue = vec![(root_cid, Some(previous_root_cid), None)];
    let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());

    while let Some((cid, old_cid, blob_size)) = queue.pop() {
        if old_cid.as_ref().is_some_and(|old| old.hash == cid.hash) {
            continue;
        }
        if !visited_new.insert((cid.hash, blob_size)) {
            continue;
        }

        ensure_local_blob_for_push(store, fetcher, &cid).await?;
        if collected.insert(cid.hash) {
            cids_to_push.push(cid.clone());
        }

        let node = node_for_push(&tree, &cid, blob_size).await?;
        let Some(node) = node else {
            continue;
        };

        let old_node = match old_cid.as_ref() {
            Some(old_cid) => match node_for_push(&tree, old_cid, blob_size).await {
                Ok(old_node) => old_node,
                Err(err) => {
                    tracing::warn!(
                        "Failed to inspect previous Blossom DAG node {}; uploading changed subtree: {}",
                        old_cid,
                        err
                    );
                    None
                }
            },
            None => None,
        };

        for (index, link) in node.links.iter().enumerate() {
            let child = child_cid(&cid, link);
            let old_child = old_node
                .as_ref()
                .and_then(|old_node| matching_old_child(&old_node.links, index, link))
                .filter(|old_link| {
                    (old_link.link_type == LinkType::Blob).then_some(old_link.size)
                        == (link.link_type == LinkType::Blob).then_some(link.size)
                })
                .map(|old_link| child_cid(old_cid.as_ref().expect("old node has cid"), old_link));

            if old_child
                .as_ref()
                .is_some_and(|old_child| old_child.hash == child.hash)
            {
                continue;
            }
            queue.push((
                child,
                old_child,
                (link.link_type == LinkType::Blob).then_some(link.size),
            ));
        }
    }

    Ok(cids_to_push)
}

async fn upload_cids_with_client(
    store: Arc<HashtreeStore>,
    fetcher: Option<Arc<Fetcher>>,
    client: hashtree_blossom::BlossomClient,
    cids_to_push: Vec<Cid>,
    force_upload: bool,
) -> Result<(usize, usize)> {
    let total = cids_to_push.len();
    let mut total_uploaded = 0usize;
    let mut total_skipped = 0usize;
    let mut total_errors = 0usize;
    let mut last_error = None;
    let mut processed = 0usize;

    let mut uploads = stream::iter(cids_to_push.into_iter().map(|cid| {
        let store = Arc::clone(&store);
        let fetcher = fetcher.clone();
        let client = client.clone();
        async move {
            ensure_local_blob_for_push(store.as_ref(), fetcher.as_deref(), &cid).await?;
            let data = store
                .get_blob(&cid.hash)?
                .ok_or_else(|| anyhow::anyhow!("missing local blob while uploading {}", cid))?;
            if force_upload {
                client
                    .upload_to_selected_servers(&data, client.write_servers())
                    .await
                    .map(|(hash, _successes)| (hash, true))
                    .map_err(|error| anyhow::anyhow!(error.to_string()))
            } else {
                client
                    .upload_if_missing(&data)
                    .await
                    .map_err(|error| anyhow::anyhow!(error.to_string()))
            }
        }
    }))
    .buffer_unordered(BLOSSOM_PUSH_CONCURRENCY);

    while let Some(result) = uploads.next().await {
        processed += 1;
        match result {
            Ok((_hash, was_uploaded)) => {
                if was_uploaded {
                    total_uploaded += 1;
                } else {
                    total_skipped += 1;
                }
            }
            Err(error) => {
                tracing::warn!("Blossom upload failed: {}", error);
                total_errors += 1;
                last_error = Some(error.to_string());
            }
        }

        if processed.is_multiple_of(BLOSSOM_PUSH_PROGRESS_EVERY) || processed == total {
            println!(
                "  file servers: {processed}/{total} processed ({total_uploaded} uploaded, {total_skipped} already exist, {total_errors} failed)",
            );
        }
    }

    if total_errors > 0 {
        let detail = last_error
            .as_deref()
            .map(|err| format!(" (last error: {err})"))
            .unwrap_or_default();
        anyhow::bail!(
            "failed to upload {} blob(s) to configured file servers{}",
            total_errors,
            detail
        );
    }

    Ok((total_uploaded, total_skipped))
}

/// Push content to Blossom servers.
pub async fn push_to_blossom(
    data_dir: &Path,
    cid_str: &str,
    server_override: Option<String>,
    force_upload: bool,
    shallow: bool,
) -> Result<()> {
    use hashtree_blossom::BlossomClient;
    use nostr::Keys;

    let (nsec_str, _) = ensure_keys_string()?;
    let keys = Keys::parse(&nsec_str).context("Failed to parse nsec")?;

    let client = if let Some(server) = server_override {
        BlossomClient::new(keys).with_write_servers(vec![server])
    } else {
        BlossomClient::new(keys)
    };

    if client.write_servers().is_empty() {
        anyhow::bail!(
            "No file servers configured. Use --server or add write_servers to config.toml"
        );
    }

    let store = Arc::new(HashtreeStore::new(data_dir)?);
    let fetcher = Arc::new(Fetcher::new(FetchConfig::default()));

    let root_cid = parse_root_cid(cid_str)?;
    let cids_to_push = if shallow {
        ensure_local_blob_for_push(store.as_ref(), Some(fetcher.as_ref()), &root_cid).await?;
        vec![root_cid]
    } else {
        println!("Collecting blocks...");
        collect_cids_for_push(store.as_ref(), root_cid, Some(fetcher.as_ref())).await?
    };

    println!("Found {} blocks to push", cids_to_push.len());
    let (uploaded, skipped) =
        upload_cids_with_client(store, Some(fetcher), client, cids_to_push, force_upload).await?;

    println!("\nUploaded: {uploaded}, Skipped: {skipped}, Errors: 0");
    println!("Done!");
    Ok(())
}

/// Push tree to Blossom servers using BlossomClient.
pub async fn background_blossom_push(
    data_dir: &Path,
    cid_str: &str,
    servers: &[String],
) -> Result<()> {
    let store = Arc::new(HashtreeStore::new(data_dir)?);
    background_blossom_push_with_store(store, cid_str, servers).await
}

pub async fn background_blossom_push_with_store(
    store: Arc<HashtreeStore>,
    cid_str: &str,
    servers: &[String],
) -> Result<()> {
    let root_cid = parse_root_cid(cid_str)?;
    background_blossom_push_incremental_with_store(store, root_cid, None, servers).await
}

pub async fn background_blossom_push_incremental_with_store(
    store: Arc<HashtreeStore>,
    root_cid: Cid,
    previous_root_cid: Option<Cid>,
    servers: &[String],
) -> Result<()> {
    use hashtree_blossom::BlossomClient;
    use nostr::Keys;

    let (nsec_str, _) = ensure_keys_string()?;
    let keys = Keys::parse(&nsec_str).context("Failed to parse nsec")?;

    let fetcher = Arc::new(Fetcher::new(FetchConfig::default()));
    let cids_to_push = if let Some(previous_root_cid) = previous_root_cid.as_ref() {
        println!("Collecting bounded DAG diff for file-server push...");
        match collect_incremental_cids_for_push(
            store.as_ref(),
            root_cid.clone(),
            previous_root_cid.clone(),
            Some(fetcher.as_ref()),
        )
        .await
        {
            Ok(cids) => cids,
            Err(err) => {
                tracing::warn!(
                    "Blossom DAG diff failed; falling back to full push: {}",
                    err
                );
                collect_cids_for_push(store.as_ref(), root_cid, Some(fetcher.as_ref())).await?
            }
        }
    } else {
        println!("Collecting DAG for file-server push...");
        collect_cids_for_push(store.as_ref(), root_cid, Some(fetcher.as_ref())).await?
    };

    if cids_to_push.is_empty() {
        return Ok(());
    }

    let client = if servers.is_empty() {
        BlossomClient::new(keys)
    } else {
        BlossomClient::new(keys).with_write_servers(servers.to_vec())
    };
    upload_cids_with_client(store, Some(fetcher), client, cids_to_push, false).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{collect_cids_for_push, collect_incremental_cids_for_push};
    use crate::HashtreeStore;
    use futures::executor::block_on as sync_block_on;
    use hashtree_core::{DirEntry, HashTree, HashTreeConfig, LinkType};
    use tempfile::tempdir;

    #[tokio::test]
    async fn push_collectors_accept_tree_shaped_raw_blobs() {
        let tmp = tempdir().expect("tempdir");
        let store = HashtreeStore::with_options(tmp.path(), None, 32 * 1024 * 1024).expect("store");
        let payload = [0x82, 0xa1, b'l', 0x90, 0xa1, b't', 0x40];

        for encrypted in [false, true] {
            let mut config = HashTreeConfig::new(store.store_arc());
            if !encrypted {
                config = config.public();
            }
            let tree = HashTree::new(config);
            let old_root = tree.put_directory(vec![]).await.expect("old root");
            let (blob, size) = tree.put(&payload).await.expect("raw blob");
            let root = tree
                .put_directory(vec![
                    DirEntry::from_cid("raw.msgpack", &blob).with_size(size)
                ])
                .await
                .expect("root");

            for previous in [None, Some(old_root)] {
                let cids = match previous {
                    Some(previous) => {
                        collect_incremental_cids_for_push(&store, root.clone(), previous, None)
                            .await
                    }
                    None => collect_cids_for_push(&store, root.clone(), None).await,
                }
                .expect("raw bytes must not be decoded as a structural node");
                let hashes = cids
                    .iter()
                    .map(|cid| cid.hash)
                    .collect::<std::collections::HashSet<_>>();
                assert_eq!(
                    hashes,
                    std::collections::HashSet::from([root.hash, blob.hash])
                );
            }
        }
    }

    #[tokio::test]
    async fn push_collectors_walk_mixed_chunk_sizes_and_reject_missing_chunks() {
        let tmp = tempdir().expect("tempdir");
        let store = HashtreeStore::with_options(tmp.path(), None, 32 * 1024 * 1024).expect("store");
        let tree = HashTree::new(HashTreeConfig::new(store.store_arc()));
        let old_root = tree.put_directory(vec![]).await.expect("old root");
        let mut entries = Vec::new();
        let mut required_hashes = std::collections::HashSet::new();
        let mut missing_chunk = None;

        for chunk_size in [64 * 1024, hashtree_core::DEFAULT_CHUNK_SIZE] {
            let chunk_tree = HashTree::new(
                HashTreeConfig::new(store.store_arc())
                    .with_chunk_size(chunk_size)
                    .with_max_links(2),
            );
            let data = [
                vec![1; chunk_size],
                vec![2; chunk_size],
                vec![3; chunk_size],
                vec![4; 7],
            ]
            .concat();
            let (file, size) = chunk_tree.put(&data).await.expect("chunked file");
            required_hashes.insert(file.hash);
            for chunk in data.chunks(chunk_size) {
                let (leaf, _) = chunk_tree.put(chunk).await.expect("leaf");
                required_hashes.insert(leaf.hash);
                missing_chunk.get_or_insert(leaf.hash);
            }
            entries.push(DirEntry::from_cid(chunk_size.to_string(), &file).with_size(size));
        }
        let root = tree.put_directory(entries).await.expect("mixed root");
        required_hashes.insert(root.hash);

        for previous in [None, Some(old_root.clone())] {
            let cids = match previous {
                Some(previous) => {
                    collect_incremental_cids_for_push(&store, root.clone(), previous, None).await
                }
                None => collect_cids_for_push(&store, root.clone(), None).await,
            }
            .expect("all chunk sizes must be traversed");
            let hashes = cids
                .iter()
                .map(|cid| cid.hash)
                .collect::<std::collections::HashSet<_>>();
            assert!(required_hashes.is_subset(&hashes));
        }

        store
            .router()
            .delete_local_only(&missing_chunk.expect("leaf"))
            .expect("delete chunk");
        for previous in [None, Some(old_root)] {
            let result = match previous {
                Some(previous) => {
                    collect_incremental_cids_for_push(&store, root.clone(), previous, None).await
                }
                None => collect_cids_for_push(&store, root.clone(), None).await,
            };
            assert!(result
                .expect_err("missing chunk must fail")
                .to_string()
                .contains("missing local blob"));
        }
    }

    #[tokio::test]
    async fn push_collectors_reject_invalid_structural_nodes() {
        let tmp = tempdir().expect("tempdir");
        let store = HashtreeStore::with_options(tmp.path(), None, 32 * 1024 * 1024).expect("store");
        let tree = HashTree::new(HashTreeConfig::new(store.store_arc()));
        let old_root = tree.put_directory(vec![]).await.expect("old root");
        let (invalid, size) = tree
            .put(&[0x82, 0xa1, b'l', 0x90, 0xa1, b't', 0x40])
            .await
            .expect("invalid node");

        for kind in [LinkType::Dir, LinkType::File, LinkType::Fanout] {
            let root = tree
                .put_directory(vec![DirEntry::from_cid("invalid", &invalid)
                    .with_size(size)
                    .with_link_type(kind)])
                .await
                .expect("root");
            for previous in [None, Some(old_root.clone())] {
                let result = match previous {
                    Some(previous) => {
                        collect_incremental_cids_for_push(&store, root.clone(), previous, None)
                            .await
                    }
                    None => collect_cids_for_push(&store, root.clone(), None).await,
                };
                assert!(result
                    .expect_err("invalid structural node must fail")
                    .to_string()
                    .contains("Invalid node type: 64"));
            }
        }
    }

    #[tokio::test]
    async fn push_collectors_expand_nodes_also_referenced_as_raw_blobs() {
        let tmp = tempdir().expect("tempdir");
        let store = HashtreeStore::with_options(tmp.path(), None, 32 * 1024 * 1024).expect("store");
        for encrypted in [false, true] {
            let mut config = HashTreeConfig::new(store.store_arc());
            if !encrypted {
                config = config.public();
            }
            let chunk_tree = HashTree::new(HashTreeConfig {
                chunk_size: 4,
                encrypted,
                ..HashTreeConfig::new(store.store_arc())
            });
            let tree = HashTree::new(config);
            let (file, size) = chunk_tree.put(b"multiple chunks").await.expect("file");
            let file_node = tree
                .get_node(&file)
                .await
                .expect("file node")
                .expect("node");
            let encoded = tree
                .get_blob(&file.hash)
                .await
                .expect("encoded file")
                .expect("bytes");
            let encoded = match file.key.as_ref() {
                Some(key) => {
                    hashtree_core::crypto::decrypt_chk(&encoded, key).expect("decrypt file")
                }
                None => encoded,
            };
            let (raw, raw_size) = tree.put(&encoded).await.expect("raw encoded node");
            assert_eq!(file, raw, "both interpretations must share the same CID");
            let old_root = tree.put_directory(vec![]).await.expect("old root");
            let raw_only_root = tree
                .put_directory(vec![
                    DirEntry::from_cid("a-file", &raw).with_size(raw_size),
                    DirEntry::from_cid("z-raw", &raw).with_size(raw_size),
                ])
                .await
                .expect("raw-only root");
            let root = tree
                .put_directory(vec![
                    DirEntry::from_cid("a-file", &file)
                        .with_size(size)
                        .with_link_type(LinkType::File),
                    DirEntry::from_cid("z-raw", &raw).with_size(raw_size),
                ])
                .await
                .expect("root");
            let mut expected = std::collections::HashSet::from([root.hash, file.hash]);
            expected.extend(file_node.links.iter().map(|link| link.hash));

            for previous in [None, Some(old_root), Some(raw_only_root)] {
                let cids = match previous {
                    Some(previous) => {
                        collect_incremental_cids_for_push(&store, root.clone(), previous, None)
                            .await
                    }
                    None => collect_cids_for_push(&store, root.clone(), None).await,
                }
                .expect("structural reference must still expand after raw reference");
                let hashes = cids
                    .iter()
                    .map(|cid| cid.hash)
                    .collect::<std::collections::HashSet<_>>();
                assert_eq!(hashes, expected);
                assert_eq!(
                    cids.len(),
                    hashes.len(),
                    "upload each shared block only once"
                );
            }
        }
    }

    #[tokio::test]
    async fn incremental_push_does_not_prune_children_of_old_raw_blobs() {
        let tmp = tempdir().expect("tempdir");
        let store = HashtreeStore::with_options(tmp.path(), None, 32 * 1024 * 1024).expect("store");
        for encrypted in [false, true] {
            let config = || HashTreeConfig {
                encrypted,
                ..HashTreeConfig::new(store.store_arc())
            };
            let tree = HashTree::new(config());
            let chunk_tree = HashTree::new(config().with_chunk_size(4));
            let (shared, _) = chunk_tree.put(b"abcd").await.expect("shared leaf");
            let (file, _) = chunk_tree.put(b"abcdabcd").await.expect("file node");
            let encoded = tree
                .get_blob(&file.hash)
                .await
                .expect("encoded node")
                .expect("bytes");
            let encoded = match file.key.as_ref() {
                Some(key) => {
                    hashtree_core::crypto::decrypt_chk(&encoded, key).expect("decrypt node")
                }
                None => encoded,
            };
            let (raw, size) = tree.put(&encoded).await.expect("literal node bytes");
            let old_root = tree
                .put_directory(vec![DirEntry::from_cid("file", &raw).with_size(size)])
                .await
                .expect("old root");
            let old_cids = collect_cids_for_push(&store, old_root.clone(), None)
                .await
                .expect("raw-only closure");
            assert!(!old_cids.iter().any(|cid| cid.hash == shared.hash));

            let mut data = vec![b'x'; size as usize];
            data[..4].copy_from_slice(b"abcd");
            let (new_file, new_size) = chunk_tree.put(&data).await.expect("new chunked file");
            assert_eq!(new_size, size);
            assert_ne!(new_file.hash, raw.hash);
            let new_root = tree
                .put_directory(vec![
                    DirEntry::from_cid("file", &new_file).with_size(new_size)
                ])
                .await
                .expect("new root");
            let cids = collect_incremental_cids_for_push(&store, new_root, old_root, None)
                .await
                .expect("changed closure");
            assert!(
                cids.iter().any(|cid| cid.hash == shared.hash),
                "old raw bytes provide no descendant coverage"
            );
        }
    }

    #[tokio::test]
    async fn failed_http_uploads_return_an_error() {
        let tmp = tempdir().expect("tempdir");
        let store = std::sync::Arc::new(
            HashtreeStore::with_options(tmp.path(), None, 32 * 1024 * 1024).expect("store"),
        );
        let tree = HashTree::new(HashTreeConfig::new(store.store_arc()));
        let (cid, _) = tree.put(b"upload rejection fixture").await.expect("blob");
        let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed_requests = requests.clone();
        let app = axum::Router::new().route(
            "/upload",
            axum::routing::put(move |body: bytes::Bytes| async move {
                assert!(!body.is_empty());
                observed_requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                axum::http::StatusCode::BAD_REQUEST
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        let client = hashtree_blossom::BlossomClient::new_empty(nostr::Keys::generate())
            .with_write_servers(vec![format!("http://{address}")]);

        for force_upload in [false, true] {
            let result = super::upload_cids_with_client(
                store.clone(),
                None,
                client.clone(),
                vec![cid.clone()],
                force_upload,
            )
            .await;
            assert!(result
                .expect_err("failed uploads must not report success")
                .to_string()
                .contains("failed to upload 1 blob(s)"));
        }
        assert_eq!(requests.load(std::sync::atomic::Ordering::Relaxed), 2);
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn collect_cids_for_push_fails_on_missing_descendant_blob() {
        let tmp = tempdir().expect("tempdir");
        let store = HashtreeStore::with_options(tmp.path(), None, 32 * 1024 * 1024).expect("store");
        let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());

        let root = sync_block_on(async {
            let (file_cid, _size) = tree.put_file(b"hello").await.expect("file");
            tree.put_directory(vec![hashtree_core::DirEntry::from_cid(
                "greeting.txt",
                &file_cid,
            )])
            .await
            .expect("dir")
        });

        let entries = store
            .get_tree_node(&root.hash)
            .expect("root node")
            .expect("root node present")
            .links;
        let child_hash = entries[0].hash;
        store
            .router()
            .delete_local_only(&child_hash)
            .expect("delete child locally");

        let err = collect_cids_for_push(&store, root, None)
            .await
            .expect_err("missing child should fail");
        assert!(err
            .to_string()
            .contains("missing local blob while pushing DAG"));
    }

    #[tokio::test]
    async fn incremental_push_collects_only_changed_named_subtrees() {
        let tmp = tempdir().expect("tempdir");
        let store = HashtreeStore::with_options(tmp.path(), None, 32 * 1024 * 1024).expect("store");
        let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());

        let stable_file = tree.put_blob(b"stable").await.expect("stable file");
        let old_changed_file = tree.put_blob(b"old").await.expect("old file");
        let old_subdir = tree
            .put_directory(vec![
                DirEntry::new("changed.txt", old_changed_file).with_size(3),
                DirEntry::new("stable.txt", stable_file).with_size(6),
            ])
            .await
            .expect("old subdir");
        let old_root = tree
            .put_directory(vec![
                DirEntry::new("subdir", old_subdir.hash).with_link_type(LinkType::Dir),
                DirEntry::new("stable-root.txt", stable_file).with_size(6),
            ])
            .await
            .expect("old root");

        let new_changed_file = tree.put_blob(b"new").await.expect("new file");
        let new_subdir = tree
            .put_directory(vec![
                DirEntry::new("stable.txt", stable_file).with_size(6),
                DirEntry::new("changed.txt", new_changed_file).with_size(3),
            ])
            .await
            .expect("new subdir");
        let new_root = tree
            .put_directory(vec![
                DirEntry::new("stable-root.txt", stable_file).with_size(6),
                DirEntry::new("subdir", new_subdir.hash).with_link_type(LinkType::Dir),
            ])
            .await
            .expect("new root");

        let cids = collect_incremental_cids_for_push(&store, new_root.clone(), old_root, None)
            .await
            .expect("incremental cids");
        let hashes = cids.iter().map(|cid| cid.hash).collect::<Vec<_>>();

        assert_eq!(hashes.len(), 3);
        assert!(hashes.contains(&new_root.hash));
        assert!(hashes.contains(&new_subdir.hash));
        assert!(hashes.contains(&new_changed_file));
        assert!(!hashes.contains(&stable_file));
    }
}
