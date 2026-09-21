use hashtree_core::{
    decode_tree_node_by_cid, encode_and_hash, Cid, DirEntry, HashTree, HashTreeConfig,
    HashTreeError, MemoryStore, TreeNode,
};
use std::sync::Arc;

#[tokio::test]
async fn loaded_node_decoder_preserves_encrypted_and_public_reads() {
    for encrypted in [false, true] {
        let mut config = HashTreeConfig::new(Arc::new(MemoryStore::new()));
        if !encrypted {
            config = config.public();
        }
        let tree = HashTree::new(config);
        let (file, size) = tree.put(b"ordinary file bytes").await.unwrap();
        let root = tree
            .put_directory(vec![DirEntry::from_cid("file", &file).with_size(size)])
            .await
            .unwrap();
        let bytes = tree.get_blob(&root.hash).await.unwrap().unwrap();
        let decoded = decode_tree_node_by_cid(&root, bytes).unwrap().unwrap();
        assert_eq!(decoded.links.len(), 1);
        assert_eq!(decoded.links[0].to_cid(), file);
        let bytes = tree.get_blob(&file.hash).await.unwrap().unwrap();
        assert!(decode_tree_node_by_cid(&file, bytes).unwrap().is_none());
    }
}

#[test]
fn loaded_node_decoder_retains_legacy_keyed_plaintext_and_rejects_bad_ciphertext() {
    let (bytes, hash) = encode_and_hash(&TreeNode::dir(Vec::new())).unwrap();
    let legacy = Cid {
        hash,
        key: Some([7; 32]),
    };
    assert!(decode_tree_node_by_cid(&legacy, bytes).unwrap().is_some());
    assert!(matches!(
        decode_tree_node_by_cid(&legacy, b"invalid encrypted bytes".to_vec()),
        Err(HashTreeError::Decryption(_))
    ));
}
