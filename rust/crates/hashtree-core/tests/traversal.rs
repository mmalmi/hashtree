use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use futures::StreamExt;
use hashtree_core::{
    collect_hashes_with_progress, tree_diff, tree_diff_streaming, tree_diff_with_old_hashes, Cid,
    DirEntry, HashTree, HashTreeConfig, HashTreeError, LinkType, MemoryStore,
};

#[tokio::test]
async fn traversal_handles_zero_concurrency_and_duplicate_links() {
    for encrypted in [false, true] {
        let mut config = HashTreeConfig::new(Arc::new(MemoryStore::new()));
        if !encrypted {
            config = config.public();
        }
        let tree = HashTree::new(config);
        let (shared, size) = tree.put(b"shared").await.unwrap();
        let shared_entry = DirEntry::from_cid("shared", &shared).with_size(size);
        let old_root = tree
            .put_directory(vec![shared_entry.clone()])
            .await
            .unwrap();
        let (added, size) = tree.put(b"added").await.unwrap();
        let root = tree
            .put_directory(vec![
                shared_entry,
                DirEntry::from_cid("added", &added).with_size(size),
                DirEntry::from_cid("duplicate", &added).with_size(size),
            ])
            .await
            .unwrap();
        let all_hashes = HashSet::from([root.hash, shared.hash, added.hash]);
        let old_hashes = HashSet::from([old_root.hash, shared.hash]);

        for concurrency in [0, 1, 4] {
            let progress = AtomicUsize::new(0);
            let collected =
                collect_hashes_with_progress(&tree, &root, concurrency, Some(&progress))
                    .await
                    .unwrap();
            assert_eq!(collected, all_hashes);
            assert_eq!(progress.load(Ordering::Relaxed), all_hashes.len());

            let diff = tree_diff(&tree, Some(&old_root), &root, concurrency)
                .await
                .unwrap();
            assert_eq!(diff.added, [root.hash, added.hash]);
            assert_eq!(diff.stats.old_tree_nodes, 2);
            assert_eq!(diff.stats.new_tree_nodes, 2);
            assert_eq!(diff.stats.unchanged_subtrees, 1);
            let cached = tree_diff_with_old_hashes(&tree, &old_hashes, &root, concurrency)
                .await
                .unwrap();
            assert_eq!(cached.added, diff.added);

            let mut streamed = Vec::new();
            let stats = tree_diff_streaming(&tree, &old_hashes, &root, concurrency, |hash| {
                streamed.push(hash);
                true
            })
            .await
            .unwrap();
            assert_eq!(streamed, diff.added);
            assert_eq!(stats.old_tree_nodes, 2);
            assert_eq!(stats.new_tree_nodes, 2);
            assert_eq!(stats.unchanged_subtrees, 1);

            let walked = tree.walk_parallel(&root, "", concurrency).await.unwrap();
            assert_eq!(walked.len(), 4);
            assert_eq!(
                walked
                    .iter()
                    .map(|entry| entry.hash)
                    .collect::<HashSet<_>>(),
                all_hashes
            );
        }
    }
}

#[tokio::test]
async fn walk_decryption_errors_do_not_expose_keys() {
    let tree = HashTree::new(HashTreeConfig::new(Arc::new(MemoryStore::new())));
    let bad_cid = Cid {
        hash: tree.put_blob(b"invalid ciphertext").await.unwrap(),
        key: Some([0x5a; 32]),
    };
    let root = tree
        .put_directory(vec![
            DirEntry::from_cid("child", &bad_cid).with_link_type(LinkType::Dir)
        ])
        .await
        .unwrap();

    for cid in [&bad_cid, &root] {
        let parallel_error = tree.walk_parallel(cid, "root", 4).await.unwrap_err();
        let stream_error = tree
            .walk_stream(cid.clone(), "root".to_owned())
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .find_map(Result::err)
            .unwrap();
        for error in [parallel_error, stream_error] {
            assert!(matches!(error, HashTreeError::Decryption(_)));
            let message = error.to_string();
            assert!(message.contains("root"));
            assert!(!message.contains(&hex::encode(bad_cid.key.unwrap())));
        }
    }
}
