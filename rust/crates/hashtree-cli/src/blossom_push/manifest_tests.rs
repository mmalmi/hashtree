use super::{collect_cids_for_push, collect_incremental_cids_for_push, HashtreeStore};
use hashtree_core::{
    Cid, DirEntry, EncryptionMode, HashTree, HashTreeConfig, LinkType, MemoryStore, Store,
};
use std::sync::Arc;

async fn copy_collected(source: &HashtreeStore, dest: &MemoryStore, selected: Vec<Cid>) {
    for cid in selected {
        dest.put(cid.hash, source.get_blob(&cid.hash).unwrap().unwrap())
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn full_and_delta_restore_equal_length_named_file_manifests() {
    let dir = tempfile::tempdir().unwrap();
    let source = HashtreeStore::with_options(dir.path(), None, 32 * 1024 * 1024).unwrap();
    for (encrypted, chunk_size) in [(false, 64), (true, 128)] {
        let config = HashTreeConfig {
            encrypted,
            ..HashTreeConfig::new(source.store_arc()).with_chunk_size(chunk_size)
        };
        let tree = HashTree::new(config);
        let (probe, _) = tree.put(&vec![7; chunk_size + 4]).await.unwrap();
        let probe_node = tree.get_node(&probe).await.unwrap().unwrap();
        let encoded_len = hashtree_core::codec::encode_tree_node(&probe_node)
            .unwrap()
            .len();
        assert!((chunk_size + 1..=2 * chunk_size).contains(&encoded_len));
        let bytes: Vec<u8> = (0..=250).cycle().take(encoded_len).collect();
        let (file, size) = tree.put(&bytes).await.unwrap();
        let manifest = tree.get_node(&file).await.unwrap().unwrap();
        assert_eq!(
            hashtree_core::codec::encode_tree_node(&manifest)
                .unwrap()
                .len() as u64,
            size
        );
        let root = tree
            .put_directory(vec![DirEntry::from_cid("named", &file).with_size(size)])
            .await
            .unwrap();
        // Keep the very same link, including name and declared size. In the
        // previous File it was literal bytes; in the new directory it starts a file.
        let links = tree.get_node(&root).await.unwrap().unwrap().links;
        let old_bytes =
            hashtree_core::encode_tree_node(&hashtree_core::TreeNode::file(links)).unwrap();
        let previous = Cid::public(tree.put_blob(&old_bytes).await.unwrap());
        for delta in [false, true] {
            let dest = Arc::new(MemoryStore::new());
            let selected = if delta {
                let old_blocks = collect_cids_for_push(&source, previous.clone(), None)
                    .await
                    .unwrap();
                assert_eq!(
                    old_blocks.len(),
                    2,
                    "file contains a literal manifest chunk"
                );
                copy_collected(&source, &dest, old_blocks).await;
                collect_incremental_cids_for_push(&source, root.clone(), previous.clone(), None)
                    .await
            } else {
                collect_cids_for_push(&source, root.clone(), None).await
            }
            .unwrap();
            copy_collected(&source, &dest, selected).await;
            let restored = HashTree::new(HashTreeConfig::new(dest));
            let child = restored
                .resolve_path(&root, "named")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(restored.get(&child, None).await.unwrap().unwrap(), bytes);
        }
    }
}

#[tokio::test]
async fn delta_restores_descendants_newly_reachable_with_an_inherited_key() {
    let dir = tempfile::tempdir().unwrap();
    let source = HashtreeStore::with_options(dir.path(), None, 32 * 1024 * 1024).unwrap();
    let plain = HashTree::new(HashTreeConfig::new(source.store_arc()).public());
    let encrypted = HashTree::new(HashTreeConfig::new(source.store_arc()).with_chunk_size(4))
        .with_encryption_mode(EncryptionMode::LegacyChk);
    let bytes = b"now readable";
    let (file, size) = encrypted.put(bytes).await.unwrap();
    let previous = plain
        .put_directory(vec![DirEntry::from_cid(
            "_chunk_file",
            &Cid::public(file.hash),
        )
        .with_link_type(LinkType::File)
        .with_size(size)])
        .await
        .unwrap();
    // A historical plaintext directory may gain a legacy key via its published
    // reference. The stored root and child hashes are identical in both views.
    let current = Cid {
        hash: previous.hash,
        key: file.key,
    };
    let dest = Arc::new(MemoryStore::new());
    let old_blocks = collect_cids_for_push(&source, previous.clone(), None)
        .await
        .unwrap();
    assert_eq!(old_blocks.len(), 2);
    copy_collected(&source, &dest, old_blocks).await;
    let delta = collect_incremental_cids_for_push(&source, current, previous, None)
        .await
        .unwrap();
    copy_collected(&source, &dest, delta).await;
    let restored = HashTree::new(HashTreeConfig::new(dest));
    assert_eq!(restored.get(&file, None).await.unwrap().unwrap(), bytes);
}

#[tokio::test]
async fn fetch_and_push_preserve_named_messagepack_files_in_nested_directories() {
    let dir = tempfile::tempdir().unwrap();
    let source = HashtreeStore::with_options(dir.path(), None, 32 * 1024 * 1024).unwrap();
    let files = HashTree::new(HashTreeConfig::new(source.store_arc()).public());
    let chunks = HashTree::new(
        HashTreeConfig::new(source.store_arc())
            .public()
            .with_chunk_size(4),
    );
    let (unrelated, _) = chunks.put(b"abcdefgh").await.unwrap();
    let encoded = source.get_blob(&unrelated.hash).unwrap().unwrap();
    let (raw, raw_size) = files.put(&encoded).await.unwrap();
    let (unknown, unknown_size) = files
        .put(&[0x82, 0xa1, b'l', 0x90, 0xa1, b't', 0x40])
        .await
        .unwrap();
    let dir_cid = files
        .put_directory(vec![
            DirEntry::from_cid("encoded", &raw).with_size(raw_size),
            DirEntry::from_cid("messagepack", &unknown).with_size(unknown_size),
        ])
        .await
        .unwrap();
    let root = files
        .put_directory(vec![
            DirEntry::from_cid("folder", &dir_cid).with_link_type(LinkType::Dir)
        ])
        .await
        .unwrap();
    let node = chunks.get_node(&unrelated).await.unwrap().unwrap();
    for link in node.links {
        assert!(source.router().delete_local_only(&link.hash).unwrap());
    }
    let client = hashtree_blossom::BlossomClient::new_empty(nostr::Keys::generate());
    assert_eq!(
        crate::fetch::Fetcher::with_client(client)
            .fetch_cid_tree(&source, &root)
            .await
            .unwrap(),
        (0, 0)
    );
    let selected = collect_cids_for_push(&source, root.clone(), None)
        .await
        .unwrap();
    let hashes = selected
        .into_iter()
        .map(|cid| cid.hash)
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        hashes,
        std::collections::HashSet::from([root.hash, dir_cid.hash, raw.hash, unknown.hash])
    );
}
