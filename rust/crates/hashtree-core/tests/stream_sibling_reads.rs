//! Count storage reads on the file shape produced at a fan-out boundary.
use async_trait::async_trait;
use futures::StreamExt;
use hashtree_core::builder::{BEP52_CHUNK_SIZE, DEFAULT_MAX_LINKS};
use hashtree_core::{
    decode_tree_node, Hash, HashTree, HashTreeConfig, LinkType, MemoryStore, Store, StoreError,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Default)]
struct MeteredStore {
    inner: MemoryStore,
    reads: Mutex<HashMap<Hash, (usize, usize)>>,
}

#[async_trait]
impl Store for MeteredStore {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        self.inner.put(hash, data).await
    }

    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        let data = self.inner.get(hash).await?;
        if let Some(bytes) = &data {
            let mut reads = self.reads.lock().unwrap();
            let entry = reads.entry(*hash).or_default();
            entry.0 += 1;
            entry.1 += bytes.len();
        }
        // A completed backend read may spend time in transit to the reader.
        tokio::time::sleep(Duration::from_millis(2)).await;
        Ok(data)
    }

    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.inner.has(hash).await
    }

    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.inner.delete(hash).await
    }
}

#[tokio::test]
async fn streaming_reads_the_sibling_after_a_subtree_once() {
    let store = Arc::new(MeteredStore::default());
    let tree = HashTree::new(
        HashTreeConfig::new(Arc::clone(&store))
            .public()
            .with_chunk_size(BEP52_CHUNK_SIZE),
    );
    let mut payload = vec![0; (DEFAULT_MAX_LINKS + 1) * BEP52_CHUNK_SIZE];
    for (i, chunk) in payload.chunks_mut(BEP52_CHUNK_SIZE).enumerate() {
        chunk[..8].copy_from_slice(&(i as u64).to_le_bytes());
    }
    let (root, _) = tree.put(&payload).await.unwrap();
    let node = decode_tree_node(&store.inner.get(&root.hash).await.unwrap().unwrap()).unwrap();
    assert_eq!(node.links.len(), 2);
    assert_eq!(node.links[0].link_type, LinkType::File);
    assert_eq!(node.links[1].link_type, LinkType::Blob);

    let start = Instant::now();
    let mut stream = tree.get_stream(&root);
    let mut offset = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.unwrap();
        assert_eq!(&payload[offset..offset + chunk.len()], chunk.as_slice());
        offset += chunk.len();
    }
    assert_eq!(offset, payload.len());
    let reads = store.reads.lock().unwrap();
    let (sibling_reads, sibling_bytes) = reads[&node.links[1].hash];
    let total_reads: usize = reads.values().map(|(n, _)| n).sum();
    let total_bytes: usize = reads.values().map(|(_, n)| n).sum();
    eprintln!("payload_bytes={offset} unique_blocks={} total_reads={total_reads} total_bytes={total_bytes} sibling_reads={sibling_reads} sibling_bytes={sibling_bytes} elapsed_us={}", reads.len(), start.elapsed().as_micros());
    assert_eq!(sibling_reads, 1);
}
