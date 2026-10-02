//! Streaming tests for HashTree put_stream and get_stream API

use futures::StreamExt;
use hashtree_core::{
    encode_tree_node, encrypt_chk, Cid, CodecError, HashTree, HashTreeConfig, HashTreeError, Link,
    LinkType, MemoryStore, TreeNode,
};
use std::sync::Arc;

#[tokio::test]
async fn test_put_stream_small() {
    let store = Arc::new(MemoryStore::new());
    let tree = HashTree::new(HashTreeConfig::new(store).public().with_chunk_size(100));

    let data: Vec<u8> = (0..50).collect();
    let cursor = std::io::Cursor::new(data.clone());
    let (cid, size) = tree
        .put_stream(futures::io::AllowStdIo::new(cursor))
        .await
        .unwrap();

    assert_eq!(size, 50);
    assert!(cid.key.is_none()); // public mode

    // Verify with get
    let result = tree.get(&cid, None).await.unwrap().unwrap();
    assert_eq!(result, data);
}

#[tokio::test]
async fn test_put_stream_chunked() {
    let store = Arc::new(MemoryStore::new());
    let tree = HashTree::new(HashTreeConfig::new(store).public().with_chunk_size(100));

    // Data larger than chunk size
    let data: Vec<u8> = (0..500).map(|i| (i % 256) as u8).collect();
    let cursor = std::io::Cursor::new(data.clone());
    let (cid, size) = tree
        .put_stream(futures::io::AllowStdIo::new(cursor))
        .await
        .unwrap();

    assert_eq!(size, 500);

    let result = tree.get(&cid, None).await.unwrap().unwrap();
    assert_eq!(result, data);
}

#[tokio::test]
async fn test_put_stream_with_progress_reports_stored_chunks() {
    let store = Arc::new(MemoryStore::new());
    let tree = HashTree::new(HashTreeConfig::new(store).public().with_chunk_size(100));

    let data: Vec<u8> = (0..250).map(|i| (i % 256) as u8).collect();
    let cursor = std::io::Cursor::new(data.clone());
    let mut progress = Vec::new();
    let (cid, size) = tree
        .put_stream_with_progress(futures::io::AllowStdIo::new(cursor), |chunk_len| {
            progress.push(chunk_len);
        })
        .await
        .unwrap();

    assert_eq!(size, 250);
    assert_eq!(progress, vec![100, 100, 50]);

    let result = tree.get(&cid, None).await.unwrap().unwrap();
    assert_eq!(result, data);
}

#[tokio::test]
async fn test_get_stream_small() {
    let store = Arc::new(MemoryStore::new());
    let tree = HashTree::new(HashTreeConfig::new(store).public());

    let data = b"Hello, World!".to_vec();
    let (cid, _size) = tree.put(&data).await.unwrap();

    let mut stream = tree.get_stream(&cid);
    let mut result = Vec::new();
    while let Some(chunk) = stream.next().await {
        result.extend(chunk.unwrap());
    }

    assert_eq!(result, data);
}

#[tokio::test]
async fn test_get_stream_chunked() {
    let store = Arc::new(MemoryStore::new());
    let tree = HashTree::new(HashTreeConfig::new(store).public().with_chunk_size(100));

    let data: Vec<u8> = (0..500).map(|i| (i % 256) as u8).collect();
    let (cid, _size) = tree.put(&data).await.unwrap();

    let mut stream = tree.get_stream(&cid);
    let mut result = Vec::new();
    while let Some(chunk) = stream.next().await {
        result.extend(chunk.unwrap());
    }

    assert_eq!(result, data);
}

#[tokio::test]
async fn test_put_stream_encrypted() {
    let store = Arc::new(MemoryStore::new());
    let tree = HashTree::new(HashTreeConfig::new(store).with_chunk_size(100)); // encrypted by default

    let data: Vec<u8> = (0..500).map(|i| (i % 256) as u8).collect();
    let cursor = std::io::Cursor::new(data.clone());
    let (cid, size) = tree
        .put_stream(futures::io::AllowStdIo::new(cursor))
        .await
        .unwrap();

    assert_eq!(size, 500);
    assert!(cid.key.is_some()); // encrypted

    let result = tree.get(&cid, None).await.unwrap().unwrap();
    assert_eq!(result, data);
}

#[tokio::test]
async fn test_get_stream_encrypted() {
    let store = Arc::new(MemoryStore::new());
    let tree = HashTree::new(HashTreeConfig::new(store).with_chunk_size(100));

    let data: Vec<u8> = (0..500).map(|i| (i % 256) as u8).collect();
    let (cid, _size) = tree.put(&data).await.unwrap();
    assert!(cid.key.is_some());

    let mut stream = tree.get_stream(&cid);
    let mut result = Vec::new();
    while let Some(chunk) = stream.next().await {
        result.extend(chunk.unwrap());
    }

    assert_eq!(result, data);
}

#[tokio::test]
async fn test_put_stream_empty() {
    let store = Arc::new(MemoryStore::new());
    let tree = HashTree::new(HashTreeConfig::new(store).public());

    let data: Vec<u8> = vec![];
    let cursor = std::io::Cursor::new(data.clone());
    let (cid, size) = tree
        .put_stream(futures::io::AllowStdIo::new(cursor))
        .await
        .unwrap();

    assert_eq!(size, 0);

    let result = tree.get(&cid, None).await.unwrap().unwrap();
    assert_eq!(result, data);
}

#[tokio::test]
async fn test_put_stream_large() {
    let store = Arc::new(MemoryStore::new());
    let tree = HashTree::new(HashTreeConfig::new(store).public().with_chunk_size(1024));

    // 1MB of data
    let data: Vec<u8> = (0..1024 * 1024).map(|i| (i % 256) as u8).collect();
    let cursor = std::io::Cursor::new(data.clone());
    let (cid, size) = tree
        .put_stream(futures::io::AllowStdIo::new(cursor))
        .await
        .unwrap();

    assert_eq!(size, 1024 * 1024);

    let result = tree.get(&cid, None).await.unwrap().unwrap();
    assert_eq!(result, data);
}

#[tokio::test]
async fn test_get_respects_max_size_public() {
    let store = Arc::new(MemoryStore::new());
    let tree = HashTree::new(HashTreeConfig::new(store).public().with_chunk_size(100));

    let data: Vec<u8> = (0..500).map(|i| (i % 256) as u8).collect();
    let (cid, _) = tree.put(&data).await.unwrap();

    let too_small = tree.get(&cid, Some((data.len() - 1) as u64)).await;
    assert!(too_small.is_err());

    let ok = tree
        .get(&cid, Some(data.len() as u64))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ok, data);
}

#[tokio::test]
async fn test_get_respects_max_size_encrypted() {
    let store = Arc::new(MemoryStore::new());
    let tree = HashTree::new(HashTreeConfig::new(store).with_chunk_size(100));

    let data: Vec<u8> = (0..500).map(|i| (i % 256) as u8).collect();
    let (cid, _) = tree.put(&data).await.unwrap();

    let too_small = tree.get(&cid, Some((data.len() - 1) as u64)).await;
    assert!(too_small.is_err());

    let ok = tree
        .get(&cid, Some(data.len() as u64))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ok, data);
}

#[tokio::test]
async fn test_get_stream_chunk_by_chunk() {
    let store = Arc::new(MemoryStore::new());
    let tree = HashTree::new(HashTreeConfig::new(store).public().with_chunk_size(100));

    let data: Vec<u8> = (0..350).map(|i| (i % 256) as u8).collect();
    let (cid, _size) = tree.put(&data).await.unwrap();

    let mut stream = tree.get_stream(&cid);
    let mut chunks = vec![];
    while let Some(chunk) = stream.next().await {
        chunks.push(chunk.unwrap());
    }

    // Should have multiple chunks
    assert!(chunks.len() >= 3);

    let result: Vec<u8> = chunks.into_iter().flatten().collect();
    assert_eq!(result, data);
}

async fn assert_stream_preserves_tree_shaped_blobs(encrypted: bool) {
    for node_type in [1, 4] {
        let store = Arc::new(MemoryStore::new());
        let mut config = HashTreeConfig::new(store)
            .with_chunk_size(32)
            .with_max_links(2);
        config.encrypted = encrypted;
        let tree = HashTree::new(config);

        // A raw pack leaf began with MessagePack [[], 4]. A valid-looking
        // [[], 1] must also remain bytes instead of becoming an empty file.
        let mut leaf = vec![0; 32];
        leaf[..3].copy_from_slice(&[0x92, 0x90, node_type]);
        let mut data = leaf.repeat(5);
        data.extend_from_slice(b"tail");
        let (cid, _) = tree.put(&data).await.unwrap();
        assert_eq!(tree.get(&cid, None).await.unwrap().unwrap(), data);

        let mut stream = tree.get_stream(&cid);
        let mut chunks = Vec::new();
        while let Some(chunk) = stream.next().await {
            chunks.push(chunk.unwrap());
        }
        assert_eq!(chunks.len(), 6);
        assert_eq!(chunks.concat(), data);
    }
}

#[tokio::test]
async fn test_get_stream_public_preserves_tree_shaped_blobs() {
    assert_stream_preserves_tree_shaped_blobs(false).await;
}

#[tokio::test]
async fn test_get_stream_encrypted_preserves_tree_shaped_blobs() {
    assert_stream_preserves_tree_shaped_blobs(true).await;
}

#[tokio::test]
async fn test_get_stream_rejects_malformed_file_links() {
    for encrypted in [false, true] {
        let store = Arc::new(MemoryStore::new());
        let tree = HashTree::new(HashTreeConfig::new(store));
        let malformed = vec![0x92, 0x90, 4];
        let (child_bytes, child_key) = if encrypted {
            let (bytes, key) = encrypt_chk(&malformed).unwrap();
            (bytes, Some(key))
        } else {
            (malformed.clone(), None)
        };
        let hash = tree.put_blob(&child_bytes).await.unwrap();
        let mut link = Link::new(hash)
            .with_size(malformed.len() as u64 + 1)
            .with_link_type(LinkType::File);
        link.key = child_key;
        let root = encode_tree_node(&TreeNode::new(LinkType::File, vec![link])).unwrap();
        let (root_bytes, root_key) = if encrypted {
            let (bytes, key) = encrypt_chk(&root).unwrap();
            (bytes, Some(key))
        } else {
            (root, None)
        };
        let cid = Cid {
            hash: tree.put_blob(&root_bytes).await.unwrap(),
            key: root_key,
        };
        let mut stream = tree.get_stream(&cid);
        assert!(matches!(
            stream.next().await.unwrap(),
            Err(HashTreeError::Codec(CodecError::InvalidNodeType(4)))
        ));
        assert!(stream.next().await.is_none());
    }
}
