use super::{fetcher, serve, STORE_BYTES};
use crate::blossom_push::{collect_cids_for_push, collect_incremental_cids_for_push};
use crate::storage::HashtreeStore;
use hashtree_config::StorageBackend;
use hashtree_core::{
    sha256, Cid, DirEntry, Hash, HashTree, HashTreeConfig, LinkType, MemoryStore, Store,
};
use std::{collections::HashSet, sync::Arc};
use tempfile::{tempdir, TempDir};

const CONTENT: &[u8] = b"only the assembled directory points at these bytes";

struct Fixture {
    source: Arc<MemoryStore>,
    root: Cid,
    old: Cid,
    content: Cid,
    literal: Vec<u8>,
    required: HashSet<Hash>,
}

impl Fixture {
    async fn new() -> Self {
        let source = Arc::new(MemoryStore::new());
        let plain = HashTree::new(HashTreeConfig::new(source.clone()).public());
        let chunks = HashTree::new(
            HashTreeConfig::new(source.clone())
                .public()
                .with_chunk_size(16),
        );
        let (content, len) = plain.put(CONTENT).await.unwrap();
        let directory = plain
            .put_directory(vec![DirEntry::from_cid("leaf", &content).with_size(len)])
            .await
            .unwrap();
        let encoded = source.get(&directory.hash).await.unwrap().unwrap();
        let (listing, len) = chunks.put(&encoded).await.unwrap();
        assert!(chunks
            .get_directory_node(&listing)
            .await
            .unwrap()
            .unwrap()
            .is_dir());

        // This ordinary File also assembles to a serialized directory. Its entry
        // refers to absent bytes deliberately; reading that link is a test failure.
        let dangling = sha256(b"not part of the archive");
        let decoy = plain
            .put_directory(vec![DirEntry::new("unrelated", dangling).with_size(23)])
            .await
            .unwrap();
        let literal = source.get(&decoy.hash).await.unwrap().unwrap();
        let (text, text_len) = chunks.put(&literal).await.unwrap();
        let entries = |kind| {
            vec![
                DirEntry::from_cid("archive", &listing)
                    .with_link_type(kind)
                    .with_size(len),
                DirEntry::from_cid("document", &text)
                    .with_link_type(LinkType::File)
                    .with_size(text_len),
            ]
        };
        let old = plain.put_directory(entries(LinkType::File)).await.unwrap();
        let root = plain.put_directory(entries(LinkType::Dir)).await.unwrap();
        assert_ne!(old, root);
        let mut required = HashSet::from([root.hash, listing.hash, text.hash, content.hash]);
        for physical in [&listing, &text] {
            let manifest = chunks.get_node(physical).await.unwrap().unwrap();
            assert!(manifest.is_file() && manifest.links.len() > 1);
            for part in manifest.links {
                required.insert(part.hash);
            }
        }
        assert!(!required.contains(&dangling));
        let result = Self {
            source,
            root,
            old,
            content,
            literal,
            required,
        };
        result.readback(&plain).await;
        result
    }

    async fn readback<S: Store>(&self, tree: &HashTree<S>) {
        let leaf = tree
            .resolve_path(&self.root, "archive/leaf")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(leaf, self.content);
        assert_eq!(
            tree.get(&leaf, None).await.unwrap().as_deref(),
            Some(CONTENT)
        );
        let document = tree
            .resolve_path(&self.root, "document")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            tree.get(&document, None).await.unwrap().as_deref(),
            Some(self.literal.as_slice())
        );
    }

    async fn local_source(&self) -> (TempDir, HashtreeStore) {
        let (dir, local) = empty_store();
        for hash in self.source.keys() {
            local
                .put_cached_blob(&self.source.get(&hash).await.unwrap().unwrap())
                .unwrap();
        }
        (dir, local)
    }

    async fn copy(&self, chosen: Vec<Cid>, target: &MemoryStore) {
        for cid in chosen {
            target
                .put(cid.hash, self.source.get(&cid.hash).await.unwrap().unwrap())
                .await
                .unwrap();
        }
    }
}

fn empty_store() -> (TempDir, HashtreeStore) {
    let dir = tempdir().unwrap();
    let store = HashtreeStore::with_options_and_backend(
        dir.path(),
        None,
        STORE_BYTES as u64,
        false,
        &StorageBackend::Fs,
    )
    .unwrap();
    (dir, store)
}

#[tokio::test]
async fn cold_fetch_restores_physical_chunks_and_logical_directory_children() {
    let fixture = Fixture::new().await;
    let (url, server) = serve(fixture.source.clone()).await;
    let (_dir, dst) = empty_store();
    fetcher(vec![url])
        .fetch_cid_tree(&dst, &fixture.root)
        .await
        .unwrap();
    server.abort();
    let _ = server.await;
    for hash in &fixture.required {
        assert!(
            dst.get_blob(hash).unwrap().is_some(),
            "cold fetch omitted a required block"
        );
    }
    assert_eq!(
        fetcher(vec![])
            .fetch_cid_tree(&dst, &fixture.root)
            .await
            .unwrap(),
        (0, 0)
    );
    fixture
        .readback(&HashTree::new(HashTreeConfig::new(dst.store_arc())))
        .await;
}

#[tokio::test]
async fn full_publication_includes_physical_chunks_and_logical_directory_children() {
    let fixture = Fixture::new().await;
    let (_dir, local) = fixture.local_source().await;
    let selected = collect_cids_for_push(&local, fixture.root.clone(), None)
        .await
        .unwrap();
    let hashes: HashSet<_> = selected.iter().map(|c| c.hash).collect();
    assert_eq!(
        hashes, fixture.required,
        "full publication must transfer the complete directory"
    );
    let copy = Arc::new(MemoryStore::new());
    fixture.copy(selected, &copy).await;
    fixture
        .readback(&HashTree::new(HashTreeConfig::new(copy)))
        .await;
}

#[tokio::test]
async fn delta_from_a_literal_file_adds_newly_reachable_directory_children() {
    let fixture = Fixture::new().await;
    let (_dir, local) = fixture.local_source().await;
    let copy = Arc::new(MemoryStore::new());
    let before = collect_cids_for_push(&local, fixture.old.clone(), None)
        .await
        .unwrap();
    fixture.copy(before, &copy).await;
    assert!(copy.get(&fixture.content.hash).await.unwrap().is_none());
    let changed =
        collect_incremental_cids_for_push(&local, fixture.root.clone(), fixture.old.clone(), None)
            .await
            .unwrap();
    assert!(
        changed.iter().any(|cid| cid.hash == fixture.content.hash),
        "switching a file entry to Dir introduces its logical children"
    );
    fixture.copy(changed, &copy).await;
    for hash in &fixture.required {
        assert!(copy.has(hash).await.unwrap());
    }
    fixture
        .readback(&HashTree::new(HashTreeConfig::new(copy)))
        .await;
}
