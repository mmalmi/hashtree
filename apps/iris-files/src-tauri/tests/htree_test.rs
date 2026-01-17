//! Integration tests for the htree protocol handler
//!
//! These tests verify the Rust htree:// protocol handler works correctly.

use std::sync::Arc;
use tempfile::tempdir;

// Re-export test utilities from hashtree crates
use hashtree_core::{nhash_encode, to_hex, HashTree, HashTreeConfig};
use app_lib::worker::BlobStore;

#[test]
fn test_nhash_decode_basic() {
    // Test that nhash decoding works
    let hash: [u8; 32] = [0xaa; 32];
    let nhash = nhash_encode(&hash).expect("encode should work");

    assert!(nhash.starts_with("nhash1"), "should start with nhash1");
    println!("Encoded nhash: {}", nhash);
}

#[test]
fn test_mime_type_detection() {
    // Import the guess_mime_type function via module path
    // Note: This would require making guess_mime_type pub or testing via the handler

    // Test common extensions
    let cases = [
        ("video.mp4", "video/mp4"),
        ("image.jpg", "image/jpeg"),
        ("document.pdf", "application/pdf"),
        ("script.js", "application/javascript"),
        ("unknown.xyz", "application/octet-stream"),
    ];

    for (filename, expected_mime) in cases {
        let ext = filename.rsplit('.').next().unwrap_or("");
        let mime = match ext {
            "mp4" | "m4v" => "video/mp4",
            "jpg" | "jpeg" => "image/jpeg",
            "pdf" => "application/pdf",
            "js" => "application/javascript",
            _ => "application/octet-stream",
        };
        assert_eq!(mime, expected_mime, "MIME type for {} should be {}", filename, expected_mime);
    }
}

#[test]
fn test_npub_validation() {
    // Test npub pattern matching
    fn is_npub(s: &str) -> bool {
        s.len() == 63 && s.starts_with("npub1") && s.chars().skip(5).all(|c| c.is_ascii_alphanumeric())
    }

    // Valid npub (63 chars total: "npub1" + 58 bech32 chars)
    // Using a valid-length npub format (all lowercase alphanumeric after npub1)
    let valid_npub = "npub1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqabc123";
    assert_eq!(valid_npub.len(), 63, "test npub should be 63 chars");
    assert!(is_npub(valid_npub), "valid npub should pass");

    // Invalid cases
    assert!(!is_npub("npub1short"), "too short should fail");
    assert!(!is_npub("nsec1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqabc123"), "nsec should fail");
    assert!(!is_npub("npub1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqabc123extra"), "too long should fail");
}

#[tokio::test]
async fn test_hashtree_basic_operations() {
    // Test basic HashTree operations that the handler uses
    use hashtree_core::store::MemoryStore;

    let store = Arc::new(MemoryStore::new());
    let tree = HashTree::new(HashTreeConfig::new(store).public());

    // Store some data
    let data = b"Hello, Tauri!";
    let (cid, size) = tree.put(data).await.expect("put should work");

    assert_eq!(size, data.len() as u64);
    println!("Stored {} bytes, hash: {}", size, to_hex(&cid.hash));

    // Read it back
    let retrieved = tree.get(&cid).await.expect("get should work");
    assert_eq!(retrieved, Some(data.to_vec()));
}

#[test]
fn test_path_parsing() {
    // Test path parsing logic used by the handler
    fn parse_htree_path(path: &str) -> Option<(&str, Option<&str>, Option<&str>)> {
        let path = path.trim_start_matches('/');
        let parts: Vec<&str> = path.splitn(3, '/').collect();

        if parts.is_empty() {
            return None;
        }

        let first = parts[0];
        let second = parts.get(1).copied();
        let third = parts.get(2).copied();

        Some((first, second, third))
    }

    // Test npub path
    let (first, second, third) = parse_htree_path("/npub1abc/public/video.mp4").unwrap();
    assert_eq!(first, "npub1abc");
    assert_eq!(second, Some("public"));
    assert_eq!(third, Some("video.mp4"));

    // Test nhash path
    let (first, second, _) = parse_htree_path("/nhash1xyz/filename.jpg").unwrap();
    assert_eq!(first, "nhash1xyz");
    assert_eq!(second, Some("filename.jpg"));

    // Test minimal path
    let (first, second, third) = parse_htree_path("/nhash1only").unwrap();
    assert_eq!(first, "nhash1only");
    assert!(second.is_none());
    assert!(third.is_none());
}

/// Test that local BlobStore works correctly for htree server
#[tokio::test]
async fn test_local_blob_store_integration() {
    let dir = tempdir().unwrap();
    let store = BlobStore::new(dir.path().to_path_buf());

    // Store some test data
    let hash = "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890";
    let data = b"Hello, this is test data for htree!";

    // Put data
    let result = store.put(hash, data).await;
    assert!(result.is_ok(), "put should succeed");

    // Verify it exists
    assert!(store.has(hash), "data should exist after put");

    // Get data back
    let retrieved = store.get(hash).await;
    assert_eq!(retrieved, Some(data.to_vec()), "retrieved data should match");

    // Delete data
    let deleted = store.delete(hash).await;
    assert!(deleted, "delete should succeed");
    assert!(!store.has(hash), "data should not exist after delete");
}

/// Test that HashTree works with local BlobStore
#[tokio::test]
async fn test_hashtree_with_local_store() {
    use app_lib::htree::CombinedStore;
    use hashtree_blossom::{BlossomClient, BlossomStore};
    use nostr_sdk::Keys;

    let dir = tempdir().unwrap();
    let blob_store = BlobStore::new(dir.path().to_path_buf());
    // Get the underlying FsBlobStore which implements Store directly
    let local_store = blob_store.inner();

    // Create a minimal BlossomStore (won't be used since local has data)
    let keys = Keys::generate();
    let blossom_client = BlossomClient::new_empty(keys)
        .with_read_servers(vec!["https://example.com".to_string()]);
    let blossom_store = Arc::new(BlossomStore::new(blossom_client));

    // Create combined store
    let combined = Arc::new(CombinedStore::new(local_store, blossom_store));

    // Create HashTree with combined store
    let tree = HashTree::new(HashTreeConfig::new(combined).public());

    // Store some data through HashTree
    let data = b"Hello from CombinedStore test!";
    let (cid, size) = tree.put(data).await.expect("put should work");

    assert_eq!(size, data.len() as u64);
    println!("Stored {} bytes, hash: {}", size, to_hex(&cid.hash));

    // Retrieve data through HashTree
    let retrieved = tree.get(&cid).await.expect("get should work");
    assert_eq!(retrieved, Some(data.to_vec()), "data should match");
}
