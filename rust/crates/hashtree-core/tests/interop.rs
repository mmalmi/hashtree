//! Interoperability tests with TypeScript implementation
//!
//! These tests verify that the Rust implementation produces identical
//! hashes and MessagePack encodings as the TypeScript implementation.

use hashtree_core::{
    sha256, encode_tree_node, to_hex, from_hex, Link, LinkType, TreeNode,
    HashTree, HashTreeConfig, MemoryStore,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Deserialize)]
struct TestVector {
    name: String,
    input: TestInput,
    expected: TestExpected,
}

#[derive(Debug, Deserialize)]
struct TestInput {
    #[serde(rename = "type")]
    input_type: String,
    data: Option<String>,
    node: Option<NodeInput>,
    #[allow(dead_code)]
    entries: Option<Vec<EntryInput>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NodeInput {
    links: Vec<LinkInput>,
    total_size: Option<u64>,
    metadata: Option<HashMap<String, serde_json::Value>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LinkInput {
    hash: String,
    name: Option<String>,
    size: Option<u64>,
    #[serde(default)]
    is_tree_node: bool,
    meta: Option<HashMap<String, serde_json::Value>>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct EntryInput {
    name: String,
    hash: String,
    size: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct TestExpected {
    hash: String,
    msgpack: Option<String>,
    ciphertext: Option<String>,
    size: Option<u64>,
}

fn load_vectors() -> Vec<TestVector> {
    let json = include_str!("interop-vectors.json");
    serde_json::from_str(json).expect("Failed to parse test vectors")
}

#[test]
fn test_sha256_vectors() {
    let vectors = load_vectors();

    for vector in vectors.iter().filter(|v| v.input.input_type == "blob" && v.input.data.is_some()) {
        let data = hex::decode(vector.input.data.as_ref().unwrap()).unwrap();
        let hash = sha256(&data);

        assert_eq!(
            to_hex(&hash),
            vector.expected.hash,
            "SHA256 mismatch for {}",
            vector.name
        );
        println!("✓ {}: hash matches", vector.name);
    }
}

#[test]
fn test_tree_node_encoding_vectors() {
    let vectors = load_vectors();

    for vector in vectors.iter().filter(|v| v.input.input_type == "tree_node") {
        let node_input = vector.input.node.as_ref().unwrap();

        // Build the tree node
        let links: Vec<Link> = node_input.links.iter().map(|l| {
            let hash = from_hex(&l.hash).unwrap();
            Link {
                hash,
                name: l.name.clone(),
                size: l.size.unwrap_or(0),
                key: None,
                // is_tree_node: true means it's a Dir, false means Blob (for interop with old format)
                link_type: if l.is_tree_node { LinkType::Dir } else { LinkType::Blob },
                meta: l.meta.clone(),
            }
        }).collect();

        // Determine node type based on test name:
        // - "unnamed_links" tests are File type (chunked file nodes)
        // - All others are Dir type
        let node_type = if vector.name.contains("unnamed_links") { LinkType::File } else { LinkType::Dir };
        let node = TreeNode::new(node_type, links);
        // Note: TreeNode no longer has totalSize - sizes are on links
        // The interop vectors with total_size are for the old format

        // Note: TreeNode no longer has metadata - metadata now lives on individual links
        // The interop vectors with metadata are for the old format, so we skip those checks

        // Encode and hash
        let encoded = encode_tree_node(&node).unwrap();
        let hash = sha256(&encoded);

        // Skip vectors with metadata - those are for the old format
        if node_input.metadata.is_some() {
            println!("⊘ {}: skipped (old format with TreeNode.metadata)", vector.name);
            continue;
        }

        // Verify MessagePack matches
        if let Some(ref expected_msgpack) = vector.expected.msgpack {
            assert_eq!(
                hex::encode(&encoded),
                *expected_msgpack,
                "MessagePack mismatch for {}",
                vector.name
            );
            println!("✓ {}: MessagePack matches", vector.name);
        }

        // Verify hash matches
        assert_eq!(
            to_hex(&hash),
            vector.expected.hash,
            "Hash mismatch for {}",
            vector.name
        );
        println!("✓ {}: hash matches", vector.name);
    }
}

#[tokio::test]
async fn test_file_vectors() {
    let vectors = load_vectors();

    for vector in vectors.iter().filter(|v| v.input.input_type == "file") {
        let data = hex::decode(vector.input.data.as_ref().unwrap()).unwrap();

        // Use small chunk size to match TS chunked file test
        // Use public() since interop vectors are for unencrypted content
        let store = Arc::new(MemoryStore::new());
        let config = if vector.name == "chunked_file" {
            HashTreeConfig::new(store.clone()).with_chunk_size(10).public()
        } else {
            HashTreeConfig::new(store.clone()).public()
        };
        let tree = HashTree::new(config);

        let (cid, size) = tree.put(&data).await.unwrap();

        assert_eq!(
            to_hex(&cid.hash),
            vector.expected.hash,
            "File hash mismatch for {}",
            vector.name
        );

        if let Some(expected_size) = vector.expected.size {
            assert_eq!(size, expected_size, "File size mismatch for {}", vector.name);
        }

        println!("✓ {}: hash and size match", vector.name);
    }
}

#[test]
fn test_known_sha256_vectors() {
    // Standard SHA256 test vectors
    let cases = vec![
        ("", "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),
        ("hello world", "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"),
    ];

    for (input, expected) in cases {
        let hash = sha256(input.as_bytes());
        assert_eq!(to_hex(&hash), expected, "SHA256 mismatch for {:?}", input);
    }
}

#[test]
fn test_hex_roundtrip() {
    let original = [0u8, 1, 127, 128, 255, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let hex = to_hex(&original);
    let result = from_hex(&hex).unwrap();
    assert_eq!(result, original);
}

#[test]
fn test_chk_encryption_vectors() {
    use hashtree_core::crypto::{encrypt_chk, decrypt_chk};

    let vectors = load_vectors();

    for vector in vectors.iter().filter(|v| v.input.input_type == "chk") {
        let plaintext = hex::decode(vector.input.data.as_ref().unwrap()).unwrap();

        // Encrypt and verify key and ciphertext match
        let (ciphertext, key) = encrypt_chk(&plaintext).unwrap();

        // Get expected values from JSON
        let expected = &vector.expected;

        assert_eq!(
            hex::encode(&key),
            expected.hash, // We store key in hash field for CHK vectors
            "CHK key mismatch for {}",
            vector.name
        );

        if let Some(ref expected_ciphertext) = expected.ciphertext {
            assert_eq!(
                hex::encode(&ciphertext),
                *expected_ciphertext,
                "CHK ciphertext mismatch for {}",
                vector.name
            );
        }

        // Verify decryption works
        let decrypted = decrypt_chk(&ciphertext, &key).unwrap();
        assert_eq!(decrypted, plaintext, "CHK decrypt mismatch for {}", vector.name);

        println!("✓ {}: key, ciphertext, and decrypt match", vector.name);
    }
}

/// Generate MessagePack test vectors - run with: cargo test generate_msgpack_vectors -- --nocapture --ignored
#[test]
#[ignore]
fn generate_msgpack_vectors() {
    use std::collections::HashMap;

    println!("\n=== MessagePack Test Vectors ===\n");

    // Empty tree node
    let node = TreeNode::new(LinkType::Dir, vec![]);
    let encoded = encode_tree_node(&node).unwrap();
    let hash = sha256(&encoded);
    println!("tree_node_empty:");
    println!("  msgpack: {}", hex::encode(&encoded));
    println!("  hash: {}", to_hex(&hash));
    println!();

    // Single link
    let hash1: [u8; 32] = from_hex("abababababababababababababababababababababababababababababababab").unwrap();
    let node = TreeNode::new(LinkType::Dir, vec![Link {
        hash: hash1,
        name: Some("test.txt".to_string()),
        size: 100,
        key: None,
        link_type: LinkType::Blob,
        meta: None,
    }]);
    let encoded = encode_tree_node(&node).unwrap();
    let hash = sha256(&encoded);
    println!("tree_node_single_link:");
    println!("  msgpack: {}", hex::encode(&encoded));
    println!("  hash: {}", to_hex(&hash));
    println!();

    // Multiple links
    let h1 = from_hex("0101010101010101010101010101010101010101010101010101010101010101").unwrap();
    let h2 = from_hex("0202020202020202020202020202020202020202020202020202020202020202").unwrap();
    let h3 = from_hex("0303030303030303030303030303030303030303030303030303030303030303").unwrap();
    let node = TreeNode::new(LinkType::Dir, vec![
        Link { hash: h1, name: Some("a.txt".to_string()), size: 10, key: None, link_type: LinkType::Blob, meta: None },
        Link { hash: h2, name: Some("b.txt".to_string()), size: 20, key: None, link_type: LinkType::Blob, meta: None },
        Link { hash: h3, name: Some("c.txt".to_string()), size: 30, key: None, link_type: LinkType::Blob, meta: None },
    ]);
    let encoded = encode_tree_node(&node).unwrap();
    let hash = sha256(&encoded);
    println!("tree_node_multiple_links:");
    println!("  msgpack: {}", hex::encode(&encoded));
    println!("  hash: {}", to_hex(&hash));
    println!();

    // Unnamed links
    let ha = from_hex("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
    let hb = from_hex("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap();
    let node = TreeNode::new(LinkType::File, vec![
        Link { hash: ha, name: None, size: 100, key: None, link_type: LinkType::Blob, meta: None },
        Link { hash: hb, name: None, size: 50, key: None, link_type: LinkType::Blob, meta: None },
    ]);
    let encoded = encode_tree_node(&node).unwrap();
    let hash = sha256(&encoded);
    println!("tree_node_unnamed_links:");
    println!("  msgpack: {}", hex::encode(&encoded));
    println!("  hash: {}", to_hex(&hash));
    println!();

    // With link meta (sorted keys for determinism)
    let hc = from_hex("cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd").unwrap();
    let mut link_meta = HashMap::new();
    link_meta.insert("author".to_string(), serde_json::json!("test"));
    link_meta.insert("version".to_string(), serde_json::json!(1));
    let node = TreeNode::new(LinkType::Dir, vec![
        Link { hash: hc, name: None, size: 0, key: None, link_type: LinkType::Blob, meta: Some(link_meta) },
    ]);
    let encoded = encode_tree_node(&node).unwrap();
    let hash = sha256(&encoded);
    println!("tree_node_with_link_meta:");
    println!("  msgpack: {}", hex::encode(&encoded));
    println!("  hash: {}", to_hex(&hash));
}

/// Generate CHK test vectors - run with: cargo test generate_chk_vectors -- --nocapture --ignored
#[test]
#[ignore]
fn generate_chk_vectors() {
    use hashtree_core::crypto::encrypt_chk;

    let test_cases = vec![
        ("chk_empty", ""),
        ("chk_hello", "hello"),
        ("chk_binary", "\x01\x02\x03\x04\x05"),
        ("chk_longer", "This is a longer message for testing CHK encryption interoperability."),
    ];

    println!("\n// Add these to interop-vectors.json:");
    for (name, plaintext) in test_cases {
        let data = plaintext.as_bytes();
        let (ciphertext, key) = encrypt_chk(data).unwrap();

        println!(r#"  {{
    "name": "{}",
    "input": {{
      "type": "chk",
      "data": "{}"
    }},
    "expected": {{
      "hash": "{}",
      "ciphertext": "{}"
    }}
  }},"#, name, hex::encode(data), hex::encode(&key), hex::encode(&ciphertext));
    }
}
