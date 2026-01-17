//! Integration tests for WebRTC peer connectivity
//!
//! These tests use a local in-memory Nostr relay for signaling to ensure
//! deterministic test behavior without external relay dependencies.
//!
//! Note: The WebRTC peer connection tests (test_peer_discovery, test_data_transfer)
//! require ICE/STUN connectivity which may not work in all environments.
//! They are marked as #[ignore] and can be run manually with --ignored.

use hashtree_core::MemoryStore;
use hashtree_sim::WsRelay;
use hashtree_webrtc::{
    classifier_channel, PeerPool, PoolConfig, PoolSettings, WebRTCStore, WebRTCStoreConfig,
};
use nostr_sdk::prelude::*;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Helper to run classifier that treats specific pubkeys as "follows"
async fn run_classifier(mut rx: hashtree_webrtc::ClassifierRx, follows: Arc<RwLock<HashSet<String>>>) {
    while let Some(req) = rx.recv().await {
        let is_follow = follows.read().await.contains(&req.pubkey);
        let pool = if is_follow {
            PeerPool::Follows
        } else {
            PeerPool::Other
        };
        let _ = req.response.send(pool);
    }
}

/// Test that we can connect to the local relay and send hello messages.
/// This verifies the relay infrastructure works without requiring WebRTC.
#[tokio::test]
async fn test_connect_to_local_relay() {
    // Start local relay
    let mut relay = WsRelay::new();
    let _addr = relay.start().await.expect("Failed to start relay");
    let relay_url = relay.url().expect("Relay URL should be available");

    let local_store = Arc::new(MemoryStore::new());
    let config = WebRTCStoreConfig {
        relays: vec![relay_url],
        debug: false,
        hello_interval_ms: 5000,
        ..Default::default()
    };

    let mut store = WebRTCStore::new(local_store, config);
    let keys = Keys::generate();

    // Should connect without error
    let result = store.start(keys).await;
    assert!(result.is_ok(), "Failed to connect: {:?}", result.err());

    // Give it time to send hello
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Local relay should have received the hello event
    let event_count = relay.event_count().await;
    assert!(event_count >= 1, "Relay should have received at least one hello event");

    store.stop().await;
    relay.stop().await;
}

/// Test that two stores can discover each other via signaling.
#[tokio::test]
#[ignore = "requires ICE/STUN connectivity - run manually with --ignored"]
async fn test_peer_discovery() {
    // Start local relay
    let mut relay = WsRelay::new();
    let _addr = relay.start().await.expect("Failed to start relay");
    let relay_url = relay.url().expect("Relay URL should be available");

    // Create two stores that should discover each other
    let store1_local = Arc::new(MemoryStore::new());
    let store2_local = Arc::new(MemoryStore::new());

    // Generate keys first so we can set up the classifier
    let keys1 = Keys::generate();
    let keys2 = Keys::generate();
    let pubkey1 = keys1.public_key().to_hex();
    let pubkey2 = keys2.public_key().to_hex();
    println!("Store1 pubkey: {}", pubkey1);
    println!("Store2 pubkey: {}", pubkey2);

    // Set up classifiers - each store treats the other as a "follow"
    let (classifier_tx1, classifier_rx1) = classifier_channel(10);
    let (classifier_tx2, classifier_rx2) = classifier_channel(10);

    // Store1 follows store2
    let follows1: Arc<RwLock<HashSet<String>>> =
        Arc::new(RwLock::new(HashSet::from([pubkey2.clone()])));
    // Store2 follows store1
    let follows2: Arc<RwLock<HashSet<String>>> =
        Arc::new(RwLock::new(HashSet::from([pubkey1.clone()])));

    // Start classifier tasks
    tokio::spawn(run_classifier(classifier_rx1, follows1));
    tokio::spawn(run_classifier(classifier_rx2, follows2));

    // Configure pools: only connect to follows (other pool has 0 max)
    let pools = PoolSettings {
        follows: PoolConfig {
            max_connections: 10,
            satisfied_connections: 1,
        },
        other: PoolConfig {
            max_connections: 0, // Don't connect to non-follows
            satisfied_connections: 0,
        },
    };

    let config1 = WebRTCStoreConfig {
        relays: vec![relay_url.clone()],
        debug: true,
        hello_interval_ms: 500, // Fast hellos for testing
        pools: pools.clone(),
        classifier_tx: Some(classifier_tx1),
        ..Default::default()
    };

    let config2 = WebRTCStoreConfig {
        relays: vec![relay_url.clone()],
        debug: true,
        hello_interval_ms: 500,
        pools: pools.clone(),
        classifier_tx: Some(classifier_tx2),
        ..Default::default()
    };

    let mut store1 = WebRTCStore::new(store1_local, config1);
    let mut store2 = WebRTCStore::new(store2_local, config2);

    // Start both stores with longer delay between them
    store1.start(keys1).await.expect("Store1 failed to start");
    println!("Store1 started");

    // Give store1 time to fully connect and subscribe
    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

    store2.start(keys2).await.expect("Store2 failed to start");
    println!("Store2 started");

    // Give store2 time to fully connect and subscribe before any signaling begins
    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
    println!("Both stores ready for peer discovery");

    // Wait for peer discovery
    // WebRTC connection establishment can take several seconds
    let mut found_peer = false;
    for i in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        println!("Discovery attempt {}/20", i + 1);

        let count1 = store1.peer_count().await;
        let count2 = store2.peer_count().await;

        println!("Peer counts: store1={}, store2={}", count1, count2);

        if count1 > 0 || count2 > 0 {
            found_peer = true;
            break;
        }
    }

    store1.stop().await;
    store2.stop().await;
    relay.stop().await;

    assert!(found_peer, "Peers should discover and connect to each other");
}

/// Test HTL-based forwarding across 3 nodes.
/// Topology: A <-> B <-> C (A and C are not directly connected)
/// A has data, C requests it, B forwards the request to A
#[tokio::test]
#[ignore = "requires ICE/STUN connectivity - run manually with --ignored"]
async fn test_three_node_forwarding() {
    use hashtree_core::{sha256, Store};

    // Start local relay
    let mut relay = WsRelay::new();
    let _addr = relay.start().await.expect("Failed to start relay");
    let relay_url = relay.url().expect("Relay URL should be available");

    // Three stores: A has data, B is relay node, C is requester
    let store_a_local = Arc::new(MemoryStore::new());
    let store_b_local = Arc::new(MemoryStore::new());
    let store_c_local = Arc::new(MemoryStore::new());

    // Put test data in store A only
    let test_data = b"Data from A, forwarded via B to C!";
    let hash = sha256(test_data);
    store_a_local.put(hash, test_data.to_vec()).await.unwrap();

    // Generate keys
    let keys_a = Keys::generate();
    let keys_b = Keys::generate();
    let keys_c = Keys::generate();
    let pubkey_a = keys_a.public_key().to_hex();
    let pubkey_b = keys_b.public_key().to_hex();
    let pubkey_c = keys_c.public_key().to_hex();
    println!("Store A pubkey: {}", pubkey_a);
    println!("Store B pubkey: {}", pubkey_b);
    println!("Store C pubkey: {}", pubkey_c);

    // Set up classifiers:
    // A connects to B only
    // B connects to both A and C
    // C connects to B only
    let (classifier_tx_a, classifier_rx_a) = classifier_channel(10);
    let (classifier_tx_b, classifier_rx_b) = classifier_channel(10);
    let (classifier_tx_c, classifier_rx_c) = classifier_channel(10);

    let follows_a: Arc<RwLock<HashSet<String>>> =
        Arc::new(RwLock::new(HashSet::from([pubkey_b.clone()])));
    let follows_b: Arc<RwLock<HashSet<String>>> =
        Arc::new(RwLock::new(HashSet::from([pubkey_a.clone(), pubkey_c.clone()])));
    let follows_c: Arc<RwLock<HashSet<String>>> =
        Arc::new(RwLock::new(HashSet::from([pubkey_b.clone()])));

    tokio::spawn(run_classifier(classifier_rx_a, follows_a));
    tokio::spawn(run_classifier(classifier_rx_b, follows_b));
    tokio::spawn(run_classifier(classifier_rx_c, follows_c));

    // Configure pools: only connect to follows
    let pools = PoolSettings {
        follows: PoolConfig {
            max_connections: 10,
            satisfied_connections: 2,
        },
        other: PoolConfig {
            max_connections: 0,
            satisfied_connections: 0,
        },
    };

    let config_a = WebRTCStoreConfig {
        relays: vec![relay_url.clone()],
        debug: true,
        hello_interval_ms: 500,
        pools: pools.clone(),
        classifier_tx: Some(classifier_tx_a),
        ..Default::default()
    };

    let config_b = WebRTCStoreConfig {
        relays: vec![relay_url.clone()],
        debug: true,
        hello_interval_ms: 500,
        pools: pools.clone(),
        classifier_tx: Some(classifier_tx_b),
        ..Default::default()
    };

    let config_c = WebRTCStoreConfig {
        relays: vec![relay_url.clone()],
        debug: true,
        hello_interval_ms: 500,
        pools: pools.clone(),
        classifier_tx: Some(classifier_tx_c),
        ..Default::default()
    };

    let mut store_a = WebRTCStore::new(store_a_local.clone(), config_a);
    let mut store_b = WebRTCStore::new(store_b_local.clone(), config_b);
    let mut store_c = WebRTCStore::new(store_c_local.clone(), config_c);

    // Start all stores with delays
    store_a.start(keys_a).await.expect("Store A failed to start");
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    store_b.start(keys_b).await.expect("Store B failed to start");
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    store_c.start(keys_c).await.expect("Store C failed to start");

    // Wait for all connections to establish
    // A-B and B-C should connect
    let mut connected = false;
    for i in 0..30 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let count_a = store_a.peer_count().await;
        let count_b = store_b.peer_count().await;
        let count_c = store_c.peer_count().await;
        println!(
            "Connection attempt {}/30: A={}, B={}, C={}",
            i + 1, count_a, count_b, count_c
        );
        // B should have 2 peers (A and C), A and C should have 1 each (B)
        if count_a >= 1 && count_b >= 2 && count_c >= 1 {
            connected = true;
            break;
        }
    }

    if !connected {
        println!("Warning: Not all peers connected as expected, attempting forwarding anyway");
    }
    println!("Attempting data transfer via forwarding...");

    // Store C should be able to fetch data from store A via B
    // C doesn't have the data, B doesn't have it either, so B should forward to A
    let result = store_c.get(&hash).await;

    store_a.stop().await;
    store_b.stop().await;
    store_c.stop().await;
    relay.stop().await;

    match result {
        Ok(Some(data)) => {
            assert_eq!(data, test_data.to_vec());
            println!("Three-node forwarding successful! Data received at C from A via B");
        }
        Ok(None) => {
            panic!("Data not found - forwarding may have failed");
        }
        Err(e) => {
            panic!("Error fetching data: {:?}", e);
        }
    }
}

/// Test that data can be transferred between peers via WebRTC.
#[tokio::test]
#[ignore = "requires ICE/STUN connectivity - run manually with --ignored"]
async fn test_data_transfer_between_peers() {
    use hashtree_core::{sha256, Store};

    // Start local relay
    let mut relay = WsRelay::new();
    let _addr = relay.start().await.expect("Failed to start relay");
    let relay_url = relay.url().expect("Relay URL should be available");

    // Store1 has data, store2 fetches it via WebRTC
    let store1_local = Arc::new(MemoryStore::new());
    let store2_local = Arc::new(MemoryStore::new());

    // Put test data in store1
    let test_data = b"Hello from peer 1 via WebRTC!";
    let hash = sha256(test_data);
    store1_local.put(hash, test_data.to_vec()).await.unwrap();

    // Generate keys first so we can set up the classifier
    let keys1 = Keys::generate();
    let keys2 = Keys::generate();
    let pubkey1 = keys1.public_key().to_hex();
    let pubkey2 = keys2.public_key().to_hex();

    // Set up classifiers - each store treats the other as a "follow"
    let (classifier_tx1, classifier_rx1) = classifier_channel(10);
    let (classifier_tx2, classifier_rx2) = classifier_channel(10);

    let follows1: Arc<RwLock<HashSet<String>>> =
        Arc::new(RwLock::new(HashSet::from([pubkey2.clone()])));
    let follows2: Arc<RwLock<HashSet<String>>> =
        Arc::new(RwLock::new(HashSet::from([pubkey1.clone()])));

    tokio::spawn(run_classifier(classifier_rx1, follows1));
    tokio::spawn(run_classifier(classifier_rx2, follows2));

    // Configure pools: only connect to follows
    let pools = PoolSettings {
        follows: PoolConfig {
            max_connections: 10,
            satisfied_connections: 1,
        },
        other: PoolConfig {
            max_connections: 0,
            satisfied_connections: 0,
        },
    };

    let config1 = WebRTCStoreConfig {
        relays: vec![relay_url.clone()],
        debug: true,
        hello_interval_ms: 500,
        pools: pools.clone(),
        classifier_tx: Some(classifier_tx1),
        ..Default::default()
    };

    let config2 = WebRTCStoreConfig {
        relays: vec![relay_url.clone()],
        debug: true,
        hello_interval_ms: 500,
        pools: pools.clone(),
        classifier_tx: Some(classifier_tx2),
        ..Default::default()
    };

    let mut store1 = WebRTCStore::new(store1_local.clone(), config1);
    let mut store2 = WebRTCStore::new(store2_local.clone(), config2);

    // Start both stores
    store1.start(keys1).await.expect("Store1 failed to start");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    store2.start(keys2).await.expect("Store2 failed to start");

    // Wait for peer connection
    let mut connected = false;
    for i in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let count1 = store1.peer_count().await;
        let count2 = store2.peer_count().await;
        println!("Connection attempt {}/20: store1={}, store2={}", i + 1, count1, count2);
        if count1 > 0 && count2 > 0 {
            connected = true;
            break;
        }
    }

    if !connected {
        store1.stop().await;
        store2.stop().await;
        relay.stop().await;
        panic!("Peers did not connect - WebRTC connection failed");
    }
    println!("Peers connected, attempting data transfer...");

    // Store2 should be able to fetch data from store1
    let result = store2.get(&hash).await;

    store1.stop().await;
    store2.stop().await;
    relay.stop().await;

    match result {
        Ok(Some(data)) => {
            assert_eq!(data, test_data.to_vec());
            println!("Data transfer successful!");
        }
        Ok(None) => {
            panic!("Data not found - peer did not respond");
        }
        Err(e) => {
            panic!("Error fetching data: {:?}", e);
        }
    }
}
