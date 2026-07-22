use super::*;
use crate::store::{MemoryStore, Store, StoreError};
use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};

fn make_tree() -> (Arc<MemoryStore>, HashTree<MemoryStore>) {
    let store = Arc::new(MemoryStore::new());
    // Use public (unencrypted) mode for these tests
    let tree = HashTree::new(HashTreeConfig::new(store.clone()).public());
    (store, tree)
}

fn invalid_tree_shape_blob() -> Vec<u8> {
    #[derive(serde::Serialize)]
    struct Shape {
        l: Vec<()>,
        t: u8,
    }

    rmp_serde::to_vec_named(&Shape {
        l: Vec::new(),
        t: 98,
    })
    .unwrap()
}

#[tokio::test]
async fn test_put_and_read_blob() {
    let (_store, tree) = make_tree();

    let data = vec![1, 2, 3, 4, 5];
    let hash = tree.put_blob(&data).await.unwrap();

    let result = tree.get_blob(&hash).await.unwrap();
    assert_eq!(result, Some(data));
}

#[tokio::test]
async fn test_put_and_read_file_small() {
    let (_store, tree) = make_tree();

    let data = b"Hello, World!";
    let (cid, size) = tree.put_file(data).await.unwrap();

    assert_eq!(size, data.len() as u64);

    let read_data = tree.read_file(&cid.hash).await.unwrap();
    assert_eq!(read_data, Some(data.to_vec()));
}

#[tokio::test]
async fn test_chunked_file_leaf_can_look_like_invalid_tree_node() {
    let store = Arc::new(MemoryStore::new());
    let mut data = invalid_tree_shape_blob();
    let chunk_size = data.len();
    data.extend_from_slice(b"tail");
    let tree = HashTree::new(
        HashTreeConfig::new(store)
            .with_chunk_size(chunk_size)
            .public(),
    );

    let (cid, size) = tree.put_file(&data).await.unwrap();

    assert_eq!(size, data.len() as u64);
    assert_eq!(tree.read_file(&cid.hash).await.unwrap(), Some(data.clone()));
    assert_eq!(
        tree.read_file_range(&cid.hash, 1, Some((data.len() - 1) as u64))
            .await
            .unwrap(),
        Some(data[1..data.len() - 1].to_vec())
    );
    assert_eq!(
        tree.read_file_chunks(&cid.hash).await.unwrap().concat(),
        data
    );
}

#[tokio::test]
async fn test_put_and_read_directory() {
    let (_store, tree) = make_tree();

    let file1 = tree.put_blob(b"content1").await.unwrap();
    let file2 = tree.put_blob(b"content2").await.unwrap();

    let dir_cid = tree
        .put_directory(vec![
            DirEntry::new("a.txt", file1).with_size(8),
            DirEntry::new("b.txt", file2).with_size(8),
        ])
        .await
        .unwrap();

    let entries = tree.list_directory(&dir_cid).await.unwrap();
    assert_eq!(entries.len(), 2);
    let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"a.txt"));
    assert!(names.contains(&"b.txt"));
}

#[tokio::test]
async fn test_required_directory_does_not_treat_missing_node_as_empty() {
    let (_store, tree) = make_tree();
    let missing = Cid {
        hash: [0x42; 32],
        key: None,
    };

    assert!(tree.list_directory(&missing).await.unwrap().is_empty());
    assert!(matches!(
        tree.list_directory_required(&missing).await,
        Err(HashTreeError::MissingChunk(_))
    ));
}

#[tokio::test]
async fn test_is_directory() {
    let (_store, tree) = make_tree();

    let file_hash = tree.put_blob(b"data").await.unwrap();
    let dir_cid = tree.put_directory(vec![]).await.unwrap();

    assert!(!tree.is_directory(&file_hash).await.unwrap());
    assert!(tree.is_directory(&dir_cid.hash).await.unwrap());
}

#[tokio::test]
async fn test_plaintext_directory_with_stray_key_still_lists_as_directory() {
    let (_store, tree) = make_tree();

    let file_hash = tree.put_blob(b"data").await.unwrap();
    let dir_cid = tree
        .put_directory(vec![DirEntry::new("thumbnail.jpg", file_hash).with_size(4)])
        .await
        .unwrap();
    let legacy_cid = Cid {
        hash: dir_cid.hash,
        key: Some([7u8; 32]),
    };

    assert!(tree.is_dir(&legacy_cid).await.unwrap());
    let entries = tree.list_directory(&legacy_cid).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "thumbnail.jpg");
}

#[tokio::test]
async fn test_resolve_path() {
    let (_store, tree) = make_tree();

    let file_hash = tree.put_blob(b"nested").await.unwrap();
    let sub_dir = tree
        .put_directory(vec![DirEntry::new("file.txt", file_hash).with_size(6)])
        .await
        .unwrap();
    let root_dir = tree
        .put_directory(vec![DirEntry::new("subdir", sub_dir.hash)])
        .await
        .unwrap();

    let resolved = tree
        .resolve_path(&root_dir, "subdir/file.txt")
        .await
        .unwrap();
    assert_eq!(resolved.map(|c| c.hash), Some(file_hash));
}

// ============ UNIFIED API TESTS ============

#[tokio::test]
async fn test_unified_put_get_public() {
    let store = Arc::new(MemoryStore::new());
    // Use .public() to disable encryption
    let tree = HashTree::new(HashTreeConfig::new(store).public());

    let data = b"Hello, public world!";
    let (cid, size) = tree.put(data).await.unwrap();

    assert_eq!(size, data.len() as u64);
    assert!(cid.key.is_none()); // No key for public content

    let retrieved = tree.get(&cid, None).await.unwrap().unwrap();
    assert_eq!(retrieved, data);
}

#[tokio::test]
async fn test_unified_put_get_encrypted() {
    let store = Arc::new(MemoryStore::new());
    // Default config has encryption enabled
    let tree = HashTree::new(HashTreeConfig::new(store));

    let data = b"Hello, encrypted world!";
    let (cid, size) = tree.put(data).await.unwrap();

    assert_eq!(size, data.len() as u64);
    assert!(cid.key.is_some()); // Has encryption key

    let retrieved = tree.get(&cid, None).await.unwrap().unwrap();
    assert_eq!(retrieved, data);
}

#[tokio::test]
async fn test_unified_put_get_encrypted_chunked() {
    let store = Arc::new(MemoryStore::new());
    let tree = HashTree::new(HashTreeConfig::new(store).with_chunk_size(100));

    // Data larger than chunk size
    let data: Vec<u8> = (0..500).map(|i| (i % 256) as u8).collect();
    let (cid, size) = tree.put(&data).await.unwrap();

    assert_eq!(size, data.len() as u64);
    assert!(cid.key.is_some());

    let retrieved = tree.get(&cid, None).await.unwrap().unwrap();
    assert_eq!(retrieved, data);
}

#[tokio::test]
async fn test_put_file_matches_put_at_chunk_boundaries_and_fanout() {
    for encrypted in [false, true] {
        let config = HashTreeConfig::new(Arc::new(MemoryStore::new()))
            .with_chunk_size(4)
            .with_max_links(2);
        let tree = HashTree::new(if encrypted { config } else { config.public() });

        for size in [0, 1, 4, 5, 20] {
            let data = vec![42; size];
            assert_eq!(
                tree.put_file(&data).await.unwrap(),
                tree.put(&data).await.unwrap(),
                "encrypted={encrypted}, size={size}"
            );
        }
    }
}

#[derive(Default)]
struct CountingStore {
    inner: MemoryStore,
    put_calls: AtomicUsize,
    put_many_calls: AtomicUsize,
    put_many_items: AtomicUsize,
    get_calls: AtomicUsize,
    get_range_calls: AtomicUsize,
    get_range_bytes: AtomicUsize,
    blob_size_calls: AtomicUsize,
}

impl CountingStore {
    fn reset_reads(&self) {
        self.get_calls.store(0, Ordering::Relaxed);
        self.get_range_calls.store(0, Ordering::Relaxed);
        self.get_range_bytes.store(0, Ordering::Relaxed);
        self.blob_size_calls.store(0, Ordering::Relaxed);
    }
}

#[async_trait]
impl Store for CountingStore {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        self.put_calls.fetch_add(1, Ordering::Relaxed);
        self.inner.put(hash, data).await
    }

    async fn put_many(&self, items: Vec<(Hash, Vec<u8>)>) -> Result<usize, StoreError> {
        self.put_many_calls.fetch_add(1, Ordering::Relaxed);
        self.put_many_items
            .fetch_add(items.len(), Ordering::Relaxed);
        self.inner.put_many(items).await
    }

    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        self.get_calls.fetch_add(1, Ordering::Relaxed);
        self.inner.get(hash).await
    }

    async fn get_range(
        &self,
        hash: &Hash,
        start: u64,
        end_inclusive: u64,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        self.get_range_calls.fetch_add(1, Ordering::Relaxed);
        let data = self.inner.get_range(hash, start, end_inclusive).await?;
        if let Some(data) = data.as_ref() {
            self.get_range_bytes
                .fetch_add(data.len(), Ordering::Relaxed);
        }
        Ok(data)
    }

    async fn blob_size(&self, hash: &Hash) -> Result<Option<u64>, StoreError> {
        self.blob_size_calls.fetch_add(1, Ordering::Relaxed);
        self.inner.blob_size(hash).await
    }

    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.inner.has(hash).await
    }

    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.inner.delete(hash).await
    }
}

#[tokio::test]
async fn test_public_chunked_range_uses_store_range_for_leaf_bytes() {
    let store = Arc::new(CountingStore::default());
    let tree = HashTree::new(
        HashTreeConfig::new(store.clone())
            .public()
            .with_chunk_size(100),
    );
    let data = (0..350).map(|i| (i % 251) as u8).collect::<Vec<_>>();
    let (cid, _) = tree.put(&data).await.unwrap();

    store.reset_reads();
    let range = tree
        .read_file_range(&cid.hash, 120, Some(180))
        .await
        .unwrap()
        .unwrap();

    assert_eq!(range, data[120..180].to_vec());
    assert_eq!(store.get_calls.load(Ordering::Relaxed), 1);
    assert_eq!(store.get_range_calls.load(Ordering::Relaxed), 1);
    assert_eq!(store.get_range_bytes.load(Ordering::Relaxed), 60);
    assert_eq!(store.blob_size_calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn test_put_stream_batches_leaf_chunk_writes() {
    let store = Arc::new(CountingStore::default());
    let tree = HashTree::new(
        HashTreeConfig::new(store.clone())
            .public()
            .with_chunk_size(4),
    );
    let data = (0..(4 * 130)).map(|i| (i % 251) as u8).collect::<Vec<_>>();
    let mut progressed = 0u64;

    let (cid, size) = tree
        .put_stream_with_progress(futures::io::Cursor::new(data.clone()), |bytes| {
            progressed += bytes;
        })
        .await
        .unwrap();

    assert_eq!(size, data.len() as u64);
    assert_eq!(progressed, data.len() as u64);
    assert_eq!(store.put_many_calls.load(Ordering::Relaxed), 2);
    assert_eq!(store.put_many_items.load(Ordering::Relaxed), 130);
    assert_eq!(store.put_calls.load(Ordering::Relaxed), 1);
    assert_eq!(tree.get(&cid, None).await.unwrap(), Some(data));
}

#[tokio::test]
async fn test_encrypted_range_reads_do_not_require_unrelated_leaf_chunks() {
    let store = Arc::new(MemoryStore::new());
    let tree = HashTree::new(HashTreeConfig::new(store.clone()).with_chunk_size(100));

    let data: Vec<u8> = (0..350).map(|i| (i % 256) as u8).collect();
    let (cid, _) = tree.put(&data).await.unwrap();
    let root = tree.get_node(&cid).await.unwrap().unwrap();

    store.delete(&root.links[3].hash).await.unwrap();

    assert_eq!(tree.get_size_cid(&cid).await.unwrap(), data.len() as u64);
    let range = tree
        .read_file_range_cid(&cid, 0, Some(50))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(range, data[..50].to_vec());
}

#[tokio::test]
async fn test_encrypted_range_reads_do_not_require_unrelated_nested_subtrees() {
    let store = Arc::new(MemoryStore::new());
    let tree = HashTree::new(
        HashTreeConfig::new(store.clone())
            .with_chunk_size(100)
            .with_max_links(2),
    );

    let data: Vec<u8> = (0..500).map(|i| (i % 256) as u8).collect();
    let (cid, _) = tree.put(&data).await.unwrap();
    let root = tree.get_node(&cid).await.unwrap().unwrap();

    store.delete(&root.links[1].hash).await.unwrap();

    assert_eq!(tree.get_size_cid(&cid).await.unwrap(), data.len() as u64);
    let range = tree
        .read_file_range_cid(&cid, 0, Some(50))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(range, data[..50].to_vec());
}

#[tokio::test]
async fn test_cid_deterministic() {
    let store = Arc::new(MemoryStore::new());
    let tree = HashTree::new(HashTreeConfig::new(store));

    let data = b"Same content produces same CID";

    let (cid1, _) = tree.put(data).await.unwrap();
    let (cid2, _) = tree.put(data).await.unwrap();

    // CHK: same content = same hash AND same key
    assert_eq!(cid1.hash, cid2.hash);
    assert_eq!(cid1.key, cid2.key);
    assert_eq!(cid1.to_string(), cid2.to_string());
}

#[tokio::test]
async fn test_cid_to_string_public() {
    let store = Arc::new(MemoryStore::new());
    let tree = HashTree::new(HashTreeConfig::new(store).public());

    let (cid, _) = tree.put(b"test").await.unwrap();
    let s = cid.to_string();

    // Public CID is just the hash (64 hex chars)
    assert_eq!(s.len(), 64);
    assert!(!s.contains(':'));
}

#[tokio::test]
async fn test_cid_to_string_encrypted() {
    let store = Arc::new(MemoryStore::new());
    let tree = HashTree::new(HashTreeConfig::new(store));

    let (cid, _) = tree.put(b"test").await.unwrap();
    let s = cid.to_string();

    // Encrypted CID is "hash:key" (64 + 1 + 64 = 129 chars)
    assert_eq!(s.len(), 129);
    assert!(s.contains(':'));
}
