//! Streaming tests for HashTree put_stream and get_stream API

use hashtree_core::{HashTree, HashTreeConfig, MemoryStore};
use futures::StreamExt;
use std::sync::Arc;

#[tokio::test]
async fn test_put_stream_small() {
    let store = Arc::new(MemoryStore::new());
    let tree = HashTree::new(HashTreeConfig::new(store).public().with_chunk_size(100));

    let data: Vec<u8> = (0..50).collect();
    let cursor = std::io::Cursor::new(data.clone());
    let (cid, size) = tree.put_stream(futures::io::AllowStdIo::new(cursor)).await.unwrap();

    assert_eq!(size, 50);
    assert!(cid.key.is_none()); // public mode

    // Verify with get
    let result = tree.get(&cid).await.unwrap().unwrap();
    assert_eq!(result, data);
}

#[tokio::test]
async fn test_put_stream_chunked() {
    let store = Arc::new(MemoryStore::new());
    let tree = HashTree::new(HashTreeConfig::new(store).public().with_chunk_size(100));

    // Data larger than chunk size
    let data: Vec<u8> = (0..500).map(|i| (i % 256) as u8).collect();
    let cursor = std::io::Cursor::new(data.clone());
    let (cid, size) = tree.put_stream(futures::io::AllowStdIo::new(cursor)).await.unwrap();

    assert_eq!(size, 500);

    let result = tree.get(&cid).await.unwrap().unwrap();
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
    let (cid, size) = tree.put_stream(futures::io::AllowStdIo::new(cursor)).await.unwrap();

    assert_eq!(size, 500);
    assert!(cid.key.is_some()); // encrypted

    let result = tree.get(&cid).await.unwrap().unwrap();
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
    let (cid, size) = tree.put_stream(futures::io::AllowStdIo::new(cursor)).await.unwrap();

    assert_eq!(size, 0);

    let result = tree.get(&cid).await.unwrap().unwrap();
    assert_eq!(result, data);
}

#[tokio::test]
async fn test_put_stream_large() {
    let store = Arc::new(MemoryStore::new());
    let tree = HashTree::new(HashTreeConfig::new(store).public().with_chunk_size(1024));

    // 1MB of data
    let data: Vec<u8> = (0..1024*1024).map(|i| (i % 256) as u8).collect();
    let cursor = std::io::Cursor::new(data.clone());
    let (cid, size) = tree.put_stream(futures::io::AllowStdIo::new(cursor)).await.unwrap();

    assert_eq!(size, 1024 * 1024);

    let result = tree.get(&cid).await.unwrap().unwrap();
    assert_eq!(result, data);
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
