use std::collections::HashSet;
use std::sync::Arc;

use hashtree_core::{
    collect_hashes, tree_diff, DirEntry, HashTree, HashTreeConfig, HashTreeError, LinkType,
    MemoryStore,
};
use proptest::prelude::*;

async fn build_directory_from_contents(
    tree: &HashTree<MemoryStore>,
    contents: &[Vec<u8>],
) -> hashtree_core::Cid {
    let mut entries = Vec::with_capacity(contents.len());
    for (idx, bytes) in contents.iter().enumerate() {
        let (cid, size) = tree.put(bytes).await.unwrap();
        entries.push(
            DirEntry::from_cid(format!("file-{idx}.bin"), &cid)
                .with_size(size)
                .with_link_type(LinkType::Blob),
        );
    }
    tree.put_directory(entries).await.unwrap()
}

proptest! {
    #[test]
    fn prop_diff_identity_empty(
        files in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..256), 0..12),
    ) {
        futures::executor::block_on(async {
            let store = Arc::new(MemoryStore::new());
            let tree = HashTree::new(HashTreeConfig::new(store).public());

            let root = build_directory_from_contents(&tree, &files).await;
            let diff = tree_diff(&tree, Some(&root), &root, 8).await.unwrap();
            assert!(diff.is_empty());
            assert!(diff.added.is_empty());
        });
    }

    #[test]
    fn prop_diff_matches_set_difference(
        old_files in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..128), 0..10),
        new_files in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..128), 0..10),
    ) {
        futures::executor::block_on(async {
            let store = Arc::new(MemoryStore::new());
            let tree = HashTree::new(HashTreeConfig::new(store).public());

            let old_root = build_directory_from_contents(&tree, &old_files).await;
            let new_root = build_directory_from_contents(&tree, &new_files).await;

            let old_set = collect_hashes(&tree, &old_root, 8).await.unwrap();
            let new_set = collect_hashes(&tree, &new_root, 8).await.unwrap();
            let expected: HashSet<_> = new_set.difference(&old_set).copied().collect();

            let diff = tree_diff(&tree, Some(&old_root), &new_root, 8).await.unwrap();
            let actual: HashSet<_> = diff.added.iter().copied().collect();

            assert_eq!(actual, expected);
        });
    }

    #[test]
    fn prop_first_push_returns_all_reachable(
        files in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..128), 0..10),
    ) {
        futures::executor::block_on(async {
            let store = Arc::new(MemoryStore::new());
            let tree = HashTree::new(HashTreeConfig::new(store).public());

            let root = build_directory_from_contents(&tree, &files).await;
            let expected = collect_hashes(&tree, &root, 8).await.unwrap();
            let diff = tree_diff(&tree, None, &root, 8).await.unwrap();
            let actual: HashSet<_> = diff.added.iter().copied().collect();

            assert_eq!(actual, expected);
        });
    }
}

#[tokio::test]
async fn test_diff_encrypted_wrong_key_returns_decryption_error() {
    let store = Arc::new(MemoryStore::new());
    let tree = HashTree::new(HashTreeConfig::new(store));

    let (cid, _size) = tree.put(b"encrypted-content").await.unwrap();
    let mut wrong_key = cid.key.unwrap();
    wrong_key[0] ^= 0xff;
    let bad_root = hashtree_core::Cid {
        hash: cid.hash,
        key: Some(wrong_key),
    };

    let err = tree_diff(&tree, None, &bad_root, 4).await.unwrap_err();
    match err {
        HashTreeError::Decryption(_) => {}
        other => panic!("expected HashTreeError::Decryption, got {other:?}"),
    }
}
