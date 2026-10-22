use hashtree_bittorrent::{NetworkOptions, Snapshot};
use hashtree_core::{Cid, DirEntry, HashTree, HashTreeConfig, LinkType, MemoryStore, Store};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

struct CountingStore {
    inner: MemoryStore,
    reads: AtomicUsize,
}
#[async_trait::async_trait]
impl Store for CountingStore {
    async fn get(
        &self,
        hash: &hashtree_core::Hash,
    ) -> Result<Option<Vec<u8>>, hashtree_core::StoreError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.get(hash).await
    }
    async fn put(
        &self,
        hash: hashtree_core::Hash,
        bytes: Vec<u8>,
    ) -> Result<bool, hashtree_core::StoreError> {
        self.inner.put(hash, bytes).await
    }
    async fn delete(&self, hash: &hashtree_core::Hash) -> Result<bool, hashtree_core::StoreError> {
        self.inner.delete(hash).await
    }
    async fn has(&self, hash: &hashtree_core::Hash) -> Result<bool, hashtree_core::StoreError> {
        self.inner.has(hash).await
    }
}

#[tokio::test]
async fn small_peer_requests_reuse_a_bounded_decrypted_window() {
    let store = Arc::new(CountingStore {
        inner: MemoryStore::new(),
        reads: AtomicUsize::new(0),
    });
    let tree = HashTree::new(HashTreeConfig::new(store.clone()));
    let bytes: Vec<_> = (0..2_200_000usize).map(|i| (i % 251) as u8).collect();
    let (cid, _) = tree.put(&bytes).await.unwrap();
    let snapshot = Snapshot::new(store.clone(), cid, "file", true)
        .await
        .unwrap();
    assert_eq!(snapshot.read(0, 0, 16384).await.unwrap(), bytes[..16384]);
    let before = store.reads.load(Ordering::SeqCst);
    assert_eq!(
        snapshot.read(0, 16384, 16384).await.unwrap(),
        bytes[16384..32768]
    );
    assert_eq!(store.reads.load(Ordering::SeqCst), before);
    assert_eq!(
        snapshot.read(0, 2_090_000, 20000).await.unwrap(),
        bytes[2_090_000..2_110_000]
    );
}

#[tokio::test]
async fn encrypted_exports_require_explicit_plaintext_consent() {
    let store = Arc::new(MemoryStore::new());
    let tree = HashTree::new(HashTreeConfig::new(store.clone()));
    let (cid, _) = tree.put(b"private payload").await.unwrap();
    assert!(Snapshot::new(store.clone(), cid.clone(), "file", false)
        .await
        .is_err());
    let snapshot = Snapshot::new(store, cid, "file", true).await.unwrap();
    assert_eq!(snapshot.read(0, 3, 7).await.unwrap(), b"vate pa");
}

#[tokio::test]
async fn public_parent_cannot_silently_publish_encrypted_children() {
    let store = Arc::new(MemoryStore::new());
    let private = HashTree::new(HashTreeConfig::new(store.clone()));
    let (file, size) = private.put(b"secret").await.unwrap();
    let public = HashTree::new(HashTreeConfig::new(store.clone()).public());
    let root = public
        .put_directory(vec![DirEntry::from_cid("child", &file).with_size(size)])
        .await
        .unwrap();
    assert!(Snapshot::new(store, root, "root", false).await.is_err());
}

#[tokio::test]
async fn directory_snapshot_is_sorted_and_reads_across_hashtree_chunks() {
    let store = Arc::new(MemoryStore::new());
    let tree = HashTree::new(
        HashTreeConfig::new(store.clone())
            .public()
            .with_chunk_size(4),
    );
    let (file, size) = tree.put(b"abcdefghijk").await.unwrap();
    let root = tree
        .put_directory(vec![
            DirEntry::from_cid("z", &file)
                .with_size(size)
                .with_link_type(LinkType::File),
            DirEntry::from_cid("a", &file)
                .with_size(size)
                .with_link_type(LinkType::File),
        ])
        .await
        .unwrap();
    let snapshot = Snapshot::new(store, root, "collection", false)
        .await
        .unwrap();
    assert_eq!(snapshot.files()[0].path, "collection/a");
    assert_eq!(snapshot.read(0, 2, 7).await.unwrap(), b"cdefghi");
    assert!(snapshot.read(0, 10, 2).await.is_err());
}

#[tokio::test]
async fn rejects_missing_content_and_unsafe_export_names() {
    let store = Arc::new(MemoryStore::new());
    let missing = Cid {
        hash: [42; 32],
        key: None,
    };
    assert!(Snapshot::new(store.clone(), missing, "missing", false)
        .await
        .is_err());
    let tree = HashTree::new(HashTreeConfig::new(store.clone()).public());
    let (file, _) = tree.put(b"content").await.unwrap();
    for name in ["", ".", "..", "../escape", "a/b", "a\\b", "a\0b", "C:"] {
        assert!(
            Snapshot::new(store.clone(), file.clone(), name, false)
                .await
                .is_err(),
            "{name:?}"
        );
    }
    store.delete(&file.hash).await.unwrap();
    store.put(file.hash, b"corrupt".to_vec()).await.unwrap();
    assert!(Snapshot::new(store, file, "file", false).await.is_err());
}

#[test]
fn networking_and_discovery_are_explicit_and_bounded() {
    let options = NetworkOptions::default();
    assert!(!options.dht);
    assert!(!options.trackers);
    assert!(options.bootstrap.is_empty());
    assert!(options.peers.is_empty());
    assert!(options.validate().is_ok());
    let mut invalid = options;
    invalid.cache_bytes = 0;
    assert!(invalid.validate().is_err());
}
