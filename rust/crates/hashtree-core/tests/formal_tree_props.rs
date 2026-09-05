use std::sync::Arc;

use futures::io::Cursor;
use hashtree_core::reader::TreeReader;
use hashtree_core::{DirEntry, HashTree, HashTreeConfig, LinkType, MemoryStore};
use proptest::prelude::*;

fn expected_slice(data: &[u8], start: usize, end: Option<usize>) -> Vec<u8> {
    let actual_end = end.unwrap_or(data.len()).min(data.len());
    if start >= actual_end {
        return Vec::new();
    }
    data[start..actual_end].to_vec()
}

proptest! {
    #[test]
    fn prop_put_get_roundtrip(
        data in prop::collection::vec(any::<u8>(), 0..4096),
        chunk_size in 1usize..256,
        encrypted in any::<bool>(),
    ) {
        futures::executor::block_on(async {
            let store = Arc::new(MemoryStore::new());
            let cfg = if encrypted {
                HashTreeConfig::new(store).with_chunk_size(chunk_size)
            } else {
                HashTreeConfig::new(store).with_chunk_size(chunk_size).public()
            };
            let tree = HashTree::new(cfg);

            let (cid, size) = tree.put(&data).await.unwrap();
            assert_eq!(size, data.len() as u64);
            let got = tree.get(&cid, None).await.unwrap().unwrap();
            assert_eq!(got, data);
        });
    }

    #[test]
    fn prop_put_stream_matches_put(
        data in prop::collection::vec(any::<u8>(), 0..4096),
        chunk_size in 1usize..256,
        encrypted in any::<bool>(),
    ) {
        futures::executor::block_on(async {
            let store = Arc::new(MemoryStore::new());
            let cfg = if encrypted {
                HashTreeConfig::new(store).with_chunk_size(chunk_size)
            } else {
                HashTreeConfig::new(store).with_chunk_size(chunk_size).public()
            };
            let tree = HashTree::new(cfg);

            let (cid_put, size_put) = tree.put(&data).await.unwrap();
            let (cid_stream, size_stream) = tree.put_stream(Cursor::new(data.clone())).await.unwrap();

            assert_eq!(size_put, size_stream);
            assert_eq!(cid_put, cid_stream);
            let got = tree.get(&cid_put, None).await.unwrap().unwrap();
            assert_eq!(got, data);
        });
    }

    #[test]
    fn prop_read_file_range_matches_slice(
        data in prop::collection::vec(any::<u8>(), 0..4096),
        chunk_size in 1usize..128,
        start in 0usize..5000,
        end in prop::option::of(0usize..5000),
    ) {
        futures::executor::block_on(async {
            let store = Arc::new(MemoryStore::new());
            let tree = HashTree::new(HashTreeConfig::new(store.clone()).with_chunk_size(chunk_size).public());
            let reader = TreeReader::new(store);

            let (cid, _size) = tree.put(&data).await.unwrap();
            let got = reader
                .read_file_range(&cid.hash, start as u64, end.map(|v| v as u64))
                .await
                .unwrap()
                .unwrap();
            let expected = expected_slice(&data, start, end);
            assert_eq!(got, expected);
        });
    }

    #[test]
    fn prop_directory_resolve_path_consistent(
        nested_dir in "[a-z]{1,8}",
        nested_file in "[a-z]{1,8}\\.[a-z]{1,3}",
        root_file in "[a-z]{1,8}\\.[a-z]{1,3}",
        nested_data in prop::collection::vec(any::<u8>(), 0..512),
        root_data in prop::collection::vec(any::<u8>(), 0..512),
    ) {
        prop_assume!(nested_file != root_file);
        futures::executor::block_on(async {
            let store = Arc::new(MemoryStore::new());
            let tree = HashTree::new(HashTreeConfig::new(store).public());

            let (nested_cid, nested_size) = tree.put(&nested_data).await.unwrap();
            let (root_cid, root_size) = tree.put(&root_data).await.unwrap();

            let sub_dir = tree
                .put_directory(vec![
                    DirEntry::from_cid(&nested_file, &nested_cid).with_size(nested_size),
                ])
                .await
                .unwrap();

            let root = tree
                .put_directory(vec![
                    DirEntry::from_cid(&nested_dir, &sub_dir).with_link_type(LinkType::Dir),
                    DirEntry::from_cid(&root_file, &root_cid).with_size(root_size),
                ])
                .await
                .unwrap();

            let nested_path = format!("{nested_dir}/{nested_file}");
            let resolved_nested = tree.resolve_path(&root, &nested_path).await.unwrap().unwrap();
            let resolved_root = tree.resolve_path(&root, &root_file).await.unwrap().unwrap();

            assert_eq!(resolved_nested.hash, nested_cid.hash);
            assert_eq!(resolved_root.hash, root_cid.hash);
        });
    }
}
