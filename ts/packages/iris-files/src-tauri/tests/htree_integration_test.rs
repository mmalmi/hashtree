//! Integration test for htree npub resolution
//!
//! Tests the full flow: npub -> Nostr resolution -> Blossom fetch -> file content

use hashtree_blossom::{BlossomClient, BlossomStore};
use hashtree_core::{to_hex, HashTree, HashTreeConfig, Store};
use hashtree_resolver::{
    nostr::{NostrResolverConfig, NostrRootResolver},
    RootResolver,
};
use nostr_sdk::Keys;
use std::sync::Arc;
use std::time::Duration;

const TEST_NPUB: &str = "npub1g53mukxnjkcmr94fhryzkqutdz2ukq4ks0gvy5af25rgmwsl4ngq43drvk";
const TEST_TREE: &str = "media";
const TEST_FILE: &str = "ekiss.jpeg";

/// Default Blossom servers for fetching blobs (must match web app)
const DEFAULT_BLOSSOM_SERVERS: &[&str] = &[
    "https://cdn.iris.to",
];

#[tokio::test]
async fn test_nostr_resolver_connects() {
    let config = NostrResolverConfig {
        relays: vec![
            "wss://relay.damus.io".into(),
            "wss://relay.primal.net".into(),
            "wss://nos.lol".into(),
        ],
        resolve_timeout: Duration::from_secs(10),
        secret_key: None,
    };

    let resolver = NostrRootResolver::new(config).await;
    assert!(resolver.is_ok(), "Resolver should initialize: {:?}", resolver.err());

    println!("Resolver initialized successfully");
}

#[tokio::test]
async fn test_resolve_npub_tree() {
    let config = NostrResolverConfig {
        relays: vec![
            "wss://relay.damus.io".into(),
            "wss://relay.primal.net".into(),
            "wss://nos.lol".into(),
            "wss://relay.nostr.band".into(),
        ],
        resolve_timeout: Duration::from_secs(15),
        secret_key: None,
    };

    let resolver = NostrRootResolver::new(config).await.expect("Resolver should init");

    let key = format!("{}/{}", TEST_NPUB, TEST_TREE);
    println!("Resolving key: {}", key);

    let result = resolver.resolve(&key).await;
    println!("Resolve result: {:?}", result);

    match result {
        Ok(Some(cid)) => {
            println!("Found CID: hash={}", hashtree_core::to_hex(&cid.hash));
            if let Some(key) = cid.key {
                println!("       key={}", hex::encode(key));
            }
        }
        Ok(None) => {
            println!("No tree found for key: {}", key);
            // This is expected if the event doesn't exist
        }
        Err(e) => {
            println!("Error: {:?}", e);
        }
    }
}

#[tokio::test]
async fn test_list_npub_trees() {
    let config = NostrResolverConfig {
        relays: vec![
            "wss://relay.damus.io".into(),
            "wss://relay.primal.net".into(),
            "wss://nos.lol".into(),
        ],
        resolve_timeout: Duration::from_secs(10),
        secret_key: None,
    };

    let resolver = NostrRootResolver::new(config).await.expect("Resolver should init");

    println!("Listing trees for npub: {}", TEST_NPUB);

    let result = resolver.list(TEST_NPUB).await;
    println!("List result: {:?}", result);

    match result {
        Ok(entries) => {
            println!("Found {} trees:", entries.len());
            for entry in &entries {
                println!("  - {}: hash={}", entry.key, hashtree_core::to_hex(&entry.cid.hash));
            }
        }
        Err(e) => {
            println!("Error listing: {:?}", e);
        }
    }
}

/// Test fetching user's Blossom server list from kind 10063
#[tokio::test]
async fn test_fetch_user_blossom_servers() {
    use nostr_sdk::{Client, Filter, Kind, PublicKey, client::EventSource};

    let npub = TEST_NPUB;
    let pubkey = PublicKey::parse(npub).expect("Valid npub");

    let client = Client::default();
    client.add_relay("wss://relay.damus.io").await.expect("add relay");
    client.add_relay("wss://relay.primal.net").await.expect("add relay");
    client.add_relay("wss://relay.nostr.band").await.expect("add relay");
    client.connect().await;

    // Kind 10063 is NIP-96 File Storage Servers List
    let filter = Filter::new()
        .author(pubkey)
        .kind(Kind::Custom(10063))
        .limit(1);

    println!("Fetching kind 10063 (Blossom server list) for {}", npub);

    let source = EventSource::relays(Some(Duration::from_secs(10)));
    let events = client.get_events_of(vec![filter], source).await;
    match events {
        Ok(evts) => {
            if evts.is_empty() {
                println!("No kind 10063 event found - user has no custom Blossom servers");
            } else {
                for evt in evts {
                    println!("Found event: {}", evt.id);
                    println!("Content: {}", evt.content);
                    for tag in evt.tags.iter() {
                        println!("Tag: {:?}", tag);
                    }
                }
            }
        }
        Err(e) => {
            println!("Error fetching: {:?}", e);
        }
    }
}

/// Test direct Blossom fetch with BlossomStore
#[tokio::test]
async fn test_direct_blossom_fetch() {
    let keys = Keys::generate();
    let blossom_client = BlossomClient::new_empty(keys)
        .with_read_servers(vec!["https://cdn.iris.to".to_string()]);
    let store = BlossomStore::new(blossom_client);

    // The tree root hash
    let hash_hex = "e4190b9acd45e5d4675f0a46447a63aa155646d77f734f2c3940184b9a877671";
    let hash = hashtree_core::from_hex(hash_hex).expect("valid hex");

    println!("Fetching hash {} from cdn.iris.to...", hash_hex);

    match store.get(&hash).await {
        Ok(Some(data)) => {
            println!("SUCCESS! Got {} bytes", data.len());
            println!("First 32 bytes: {:02x?}", &data[..data.len().min(32)]);
        }
        Ok(None) => {
            println!("NOT FOUND - blob doesn't exist on server");
        }
        Err(e) => {
            println!("ERROR: {:?}", e);
        }
    }
}

/// E2E test: Resolve npub/media and verify directory is NOT empty
/// This is the specific regression test for the "empty directory" bug
#[tokio::test]
async fn test_media_directory_not_empty() {
    // 1. Set up Nostr resolver
    let config = NostrResolverConfig {
        relays: vec![
            "wss://relay.damus.io".into(),
            "wss://relay.primal.net".into(),
            "wss://nos.lol".into(),
            "wss://relay.nostr.band".into(),
        ],
        resolve_timeout: Duration::from_secs(15),
        secret_key: None,
    };

    let resolver = NostrRootResolver::new(config).await.expect("Resolver should init");

    // 2. Resolve tree root
    let key = format!("{}/{}", TEST_NPUB, TEST_TREE);
    println!("Resolving tree root for {}", key);

    let root_cid = match resolver.resolve(&key).await {
        Ok(Some(cid)) => cid,
        Ok(None) => {
            println!("Tree not found on relays - skipping test");
            return;
        }
        Err(e) => {
            println!("Resolver error (may be transient): {:?}", e);
            return;
        }
    };

    println!("Found tree root: hash={}", to_hex(&root_cid.hash));

    // 3. Set up Blossom store for fetching
    let keys = Keys::generate();
    let blossom_client = BlossomClient::new_empty(keys)
        .with_read_servers(DEFAULT_BLOSSOM_SERVERS.iter().map(|s| s.to_string()).collect());
    let store = Arc::new(BlossomStore::new(blossom_client));
    let tree = HashTree::new(HashTreeConfig::new(store.clone()));

    // 4. List files in tree root - THIS IS THE KEY TEST
    println!("Listing tree root contents...");
    match tree.list(&root_cid).await {
        Ok(entries) => {
            println!("SUCCESS! Tree contains {} entries:", entries.len());
            for entry in &entries {
                println!("  - {}: size={}", entry.name, entry.size);
            }
            // THE CRITICAL ASSERTION: directory must NOT be empty
            assert!(!entries.is_empty(), "Media tree directory should NOT be empty!");
        }
        Err(e) => {
            panic!("Failed to list tree: {:?} - Blossom fallback may not be working", e);
        }
    }
}

/// Test the full flow: resolve npub/tree -> fetch tree root -> resolve path -> fetch file
#[tokio::test]
async fn test_full_file_fetch() {
    // 1. Set up Nostr resolver
    let config = NostrResolverConfig {
        relays: vec![
            "wss://relay.damus.io".into(),
            "wss://relay.primal.net".into(),
            "wss://nos.lol".into(),
            "wss://relay.nostr.band".into(),
        ],
        resolve_timeout: Duration::from_secs(15),
        secret_key: None,
    };

    let resolver = NostrRootResolver::new(config).await.expect("Resolver should init");

    // 2. Resolve tree root
    let key = format!("{}/{}", TEST_NPUB, TEST_TREE);
    println!("Step 1: Resolving tree root for {}", key);

    let root_cid = resolver.resolve(&key).await
        .expect("Resolution should not error")
        .expect("Tree should exist");

    println!("  Found tree root: hash={}", to_hex(&root_cid.hash));
    if let Some(k) = &root_cid.key {
        println!("  Encryption key: {}", hex::encode(k));
    }

    // 3. Set up Blossom store for fetching
    let keys = Keys::generate();
    let blossom_client = BlossomClient::new_empty(keys)
        .with_read_servers(DEFAULT_BLOSSOM_SERVERS.iter().map(|s| s.to_string()).collect());
    let store = Arc::new(BlossomStore::new(blossom_client));
    let tree = HashTree::new(HashTreeConfig::new(store.clone()));

    // 4. List files in tree root (explore what's in the tree)
    println!("Step 2: Listing tree root contents...");
    match tree.list(&root_cid).await {
        Ok(entries) => {
            println!("  Tree root contains {} entries:", entries.len());
            for entry in &entries {
                println!("    - {}: hash={}", entry.name, to_hex(&entry.hash));
            }
        }
        Err(e) => {
            println!("  Error listing tree: {:?}", e);
        }
    }

    // 5. Resolve the file path within the tree
    println!("Step 3: Resolving file path '{}'...", TEST_FILE);
    match tree.resolve_path(&root_cid, TEST_FILE).await {
        Ok(Some(file_cid)) => {
            println!("  Found file CID: hash={}", to_hex(&file_cid.hash));
            if let Some(k) = &file_cid.key {
                println!("  File encryption key: {}", hex::encode(k));
            }

            // 6. Fetch the actual file content
            println!("Step 4: Fetching file content...");
            match tree.get(&file_cid).await {
                Ok(Some(data)) => {
                    println!("  SUCCESS! Got {} bytes of file data", data.len());
                    // Check if it looks like a JPEG (starts with FFD8FF)
                    if data.len() >= 3 && data[0] == 0xFF && data[1] == 0xD8 && data[2] == 0xFF {
                        println!("  File appears to be a valid JPEG image");
                    } else {
                        println!("  File header: {:02X?}", &data[..data.len().min(16)]);
                    }
                }
                Ok(None) => {
                    println!("  ERROR: File CID exists but content not found in Blossom");
                }
                Err(e) => {
                    println!("  ERROR fetching file: {:?}", e);
                }
            }
        }
        Ok(None) => {
            println!("  ERROR: File '{}' not found in tree", TEST_FILE);
        }
        Err(e) => {
            println!("  ERROR resolving path: {:?}", e);
        }
    }
}
