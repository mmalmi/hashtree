use super::HashtreeStore;
use hashtree_core::{HashTree, HashTreeConfig};
use std::sync::Arc;
use tempfile::TempDir;

/// A plaintext file with more chunks than one tree node holds nests interior
/// nodes under its root. Range reads must return file bytes, not node bytes.
#[test]
fn range_reads_descend_into_nested_file_trees() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let store = Arc::new(HashtreeStore::new(temp.path().join("store"))?);
    let tree = HashTree::new(
        HashTreeConfig::new(store.store_arc())
            .public()
            .with_chunk_size(100)
            .with_max_links(2),
    );
    let data: Vec<u8> = (0..530u32).map(|i| (i * 13 % 251) as u8).collect();
    let (cid, _) = futures::executor::block_on(tree.put(&data))?;

    let metadata = store.get_file_chunk_metadata(&cid.hash)?.expect("metadata");
    assert_eq!(metadata.total_size, data.len() as u64);
    assert_eq!(metadata.chunk_hashes.len(), 6, "one entry per leaf chunk");
    assert_eq!(metadata.chunk_sizes.iter().sum::<u64>(), data.len() as u64);

    for (start, end) in [(0u64, 529u64), (95, 105), (250, 449), (500, 529)] {
        let (bytes, total) = store
            .get_file_range(&cid.hash, start, Some(end))?
            .expect("range");
        assert_eq!(total, data.len() as u64);
        assert_eq!(bytes, data[start as usize..=end as usize].to_vec());

        let streamed: Vec<u8> = Arc::clone(&store)
            .stream_file_range_chunks_owned(&cid.hash, start, end)?
            .expect("chunk stream")
            .collect::<anyhow::Result<Vec<_>>>()?
            .concat();
        assert_eq!(streamed, data[start as usize..=end as usize].to_vec());
    }
    Ok(())
}
