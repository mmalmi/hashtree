//! Integration test for htree profile command
//!
//! Tests publishing and fetching Nostr profile (kind 0) events.
//!
//! Run with: cargo test --package hashtree-cli --test profile -- --nocapture

use anyhow::Result;
use nostr::{Keys, ToBech32, EventBuilder, Kind, Filter};
use nostr_sdk::{ClientBuilder, EventSource};
use std::time::Duration;

mod test_relay {
    use std::collections::HashMap;
    use std::net::TcpListener;
    use nostr::Filter;
    use std::sync::Arc;
    use futures::{SinkExt, StreamExt};
    use tokio::net::TcpStream;
    use tokio::sync::{broadcast, RwLock};
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    pub struct TestRelay {
        port: u16,
        shutdown: broadcast::Sender<()>,
    }

    impl TestRelay {
        pub fn new(port: u16) -> Self {
            let events: Arc<RwLock<HashMap<String, serde_json::Value>>> = Arc::new(RwLock::new(HashMap::new()));
            let (shutdown, _) = broadcast::channel(1);
            let (event_tx, _) = broadcast::channel::<serde_json::Value>(1000);

            let relay = TestRelay {
                port,
                shutdown: shutdown.clone(),
            };

            let events_clone = events.clone();
            let mut shutdown_rx = shutdown.subscribe();
            let event_tx_clone = event_tx.clone();

            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .unwrap();

                rt.block_on(async move {
                    let listener = TcpListener::bind(format!("127.0.0.1:{}", port)).unwrap();
                    listener.set_nonblocking(true).unwrap();
                    let listener = tokio::net::TcpListener::from_std(listener).unwrap();

                    loop {
                        tokio::select! {
                            _ = shutdown_rx.recv() => break,
                            result = listener.accept() => {
                                if let Ok((stream, _)) = result {
                                    let events = events_clone.clone();
                                    let event_tx = event_tx_clone.clone();
                                    let event_rx = event_tx_clone.subscribe();
                                    tokio::spawn(handle_connection(stream, events, event_tx, event_rx));
                                }
                            }
                        }
                    }
                });
            });

            std::thread::sleep(std::time::Duration::from_millis(100));
            relay
        }

        pub fn url(&self) -> String {
            format!("ws://127.0.0.1:{}", self.port)
        }
    }

    impl Drop for TestRelay {
        fn drop(&mut self) {
            let _ = self.shutdown.send(());
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    async fn handle_connection(
        stream: TcpStream,
        events: Arc<RwLock<HashMap<String, serde_json::Value>>>,
        event_tx: broadcast::Sender<serde_json::Value>,
        mut event_rx: broadcast::Receiver<serde_json::Value>,
    ) {
        let ws_stream = match accept_async(stream).await {
            Ok(s) => s,
            Err(_) => return,
        };

        let (write, mut read) = ws_stream.split();
        let write = Arc::new(tokio::sync::Mutex::new(write));

        let subscriptions: Arc<RwLock<HashMap<String, Filter>>> = Arc::new(RwLock::new(HashMap::new()));

        // Broadcast task for live events
        let write_clone = write.clone();
        let subs_clone = subscriptions.clone();
        let broadcast_task = tokio::spawn(async move {
            loop {
                match event_rx.recv().await {
                    Ok(event) => {
                        let subs = subs_clone.read().await;
                        for (sub_id, _filter) in subs.iter() {
                            // Simple: broadcast all events to all subs
                            let event_msg = serde_json::json!(["EVENT", sub_id, &event]);
                            let mut w = write_clone.lock().await;
                            let _ = w.send(Message::Text(event_msg.to_string())).await;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        while let Some(msg) = read.next().await {
            let msg = match msg {
                Ok(Message::Text(t)) => t,
                Ok(Message::Close(_)) => break,
                Ok(Message::Ping(data)) => {
                    let mut w = write.lock().await;
                    let _ = w.send(Message::Pong(data)).await;
                    continue;
                }
                _ => continue,
            };

            let parsed: Result<Vec<serde_json::Value>, _> = serde_json::from_str(&msg);
            let parsed = match parsed {
                Ok(p) => p,
                Err(_) => continue,
            };

            if parsed.is_empty() {
                continue;
            }

            let msg_type = parsed[0].as_str().unwrap_or("");

            match msg_type {
                "EVENT" => {
                    if parsed.len() >= 2 {
                        let event = parsed[1].clone();
                        if let Some(id) = event.get("id").and_then(|v| v.as_str()) {
                            events.write().await.insert(id.to_string(), event.clone());

                            let ok_msg = serde_json::json!(["OK", id, true, ""]);
                            {
                                let mut w = write.lock().await;
                                let _ = w.send(Message::Text(ok_msg.to_string())).await;
                            }

                            let _ = event_tx.send(event);
                        }
                    }
                }
                "REQ" => {
                    if parsed.len() >= 3 {
                        let sub_id = parsed[1].as_str().unwrap_or("sub").to_string();

                        // Store subscription
                        subscriptions.write().await.insert(sub_id.clone(), Filter::new());

                        // Send matching stored events
                        let stored = events.read().await;
                        for (_id, event) in stored.iter() {
                            let event_msg = serde_json::json!(["EVENT", &sub_id, event]);
                            let mut w = write.lock().await;
                            let _ = w.send(Message::Text(event_msg.to_string())).await;
                        }

                        // Send EOSE
                        let eose = serde_json::json!(["EOSE", &sub_id]);
                        let mut w = write.lock().await;
                        let _ = w.send(Message::Text(eose.to_string())).await;
                    }
                }
                "CLOSE" => {
                    if parsed.len() >= 2 {
                        if let Some(sub_id) = parsed[1].as_str() {
                            subscriptions.write().await.remove(sub_id);
                        }
                    }
                }
                _ => {}
            }
        }

        broadcast_task.abort();
    }
}

#[tokio::test]
async fn test_profile_publish_and_fetch() -> Result<()> {
    // Start test relay
    let relay = test_relay::TestRelay::new(14850);
    let relay_url = relay.url();

    // Generate test keys
    let keys = Keys::generate();
    let npub = keys.public_key().to_bech32()?;

    // Create and publish profile event
    let profile = serde_json::json!({
        "name": "Test User",
        "display_name": "Test User",
        "about": "A test profile"
    });

    let event = EventBuilder::new(Kind::Metadata, profile.to_string(), [])
        .to_event(&keys)?;

    // Publish using nostr-sdk client
    let client = ClientBuilder::default().build();
    client.add_relay(&relay_url).await?;
    client.connect().await;

    // Send event
    client.send_event(event).await?;

    // Small delay for relay to process
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Now fetch it back
    let filter = Filter::new()
        .author(keys.public_key())
        .kind(Kind::Metadata)
        .limit(1);

    let events = tokio::time::timeout(
        Duration::from_secs(5),
        client.get_events_of(vec![filter], EventSource::relays(None))
    ).await??;

    client.disconnect().await?;

    // Verify we got the profile back
    assert!(!events.is_empty(), "Should have received the profile event");

    let fetched = events.into_iter().next().unwrap();
    let fetched_profile: serde_json::Value = serde_json::from_str(&fetched.content)?;

    assert_eq!(fetched_profile.get("name").and_then(|v| v.as_str()), Some("Test User"));
    assert_eq!(fetched_profile.get("about").and_then(|v| v.as_str()), Some("A test profile"));

    println!("Profile published and fetched successfully for {}", npub);
    Ok(())
}

#[tokio::test]
async fn test_profile_update_merges_fields() -> Result<()> {
    // Start test relay
    let relay = test_relay::TestRelay::new(14851);
    let relay_url = relay.url();

    // Generate test keys
    let keys = Keys::generate();

    // Create initial profile with name and about
    let profile1 = serde_json::json!({
        "name": "Original Name",
        "about": "Original bio"
    });

    let event1 = EventBuilder::new(Kind::Metadata, profile1.to_string(), [])
        .to_event(&keys)?;

    let client = ClientBuilder::default().build();
    client.add_relay(&relay_url).await?;
    client.connect().await;

    // Publish initial profile
    client.send_event(event1).await?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Now update with just picture (simulating merge)
    // First fetch existing
    let filter = Filter::new()
        .author(keys.public_key())
        .kind(Kind::Metadata)
        .limit(1);

    let events = tokio::time::timeout(
        Duration::from_secs(5),
        client.get_events_of(vec![filter.clone()], EventSource::relays(None))
    ).await??;

    let existing: serde_json::Map<String, serde_json::Value> = events
        .into_iter()
        .next()
        .and_then(|e| serde_json::from_str(&e.content).ok())
        .unwrap_or_default();

    // Merge: keep existing fields, add picture
    let mut updated = existing.clone();
    updated.insert("picture".to_string(), serde_json::Value::String("https://example.com/pic.jpg".to_string()));

    // Wait to ensure different timestamp
    tokio::time::sleep(Duration::from_secs(1)).await;

    let event2 = EventBuilder::new(Kind::Metadata, serde_json::to_string(&updated)?, [])
        .to_event(&keys)?;

    client.send_event(event2).await?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Fetch final profile - get all events and take the most recent one
    let filter = Filter::new()
        .author(keys.public_key())
        .kind(Kind::Metadata);

    let events = tokio::time::timeout(
        Duration::from_secs(5),
        client.get_events_of(vec![filter], EventSource::relays(None))
    ).await??;

    // Take the most recent event (highest created_at)
    let final_profile: serde_json::Value = events
        .into_iter()
        .max_by_key(|e| e.created_at)
        .map(|e| serde_json::from_str(&e.content).unwrap())
        .unwrap();

    // Verify all fields are present
    assert_eq!(final_profile.get("name").and_then(|v| v.as_str()), Some("Original Name"));
    assert_eq!(final_profile.get("about").and_then(|v| v.as_str()), Some("Original bio"));
    assert_eq!(final_profile.get("picture").and_then(|v| v.as_str()), Some("https://example.com/pic.jpg"));

    client.disconnect().await?;
    println!("Profile update merge test passed");
    Ok(())
}

/// Test fetching another user's profile (simulates peer name resolution)
#[tokio::test]
async fn test_fetch_peer_profile_name() -> Result<()> {
    // Start test relay
    let relay = test_relay::TestRelay::new(14852);
    let relay_url = relay.url();

    // Generate keys for a "peer"
    let peer_keys = Keys::generate();
    let peer_pubkey_hex = peer_keys.public_key().to_string();

    // Create peer's profile with display_name
    let profile = serde_json::json!({
        "name": "Alice",
        "display_name": "Alice Wonder",
        "about": "A peer user"
    });

    let event = EventBuilder::new(Kind::Metadata, profile.to_string(), [])
        .to_event(&peer_keys)?;

    // Publish peer's profile
    let client = ClientBuilder::default().build();
    client.add_relay(&relay_url).await?;
    client.connect().await;
    client.send_event(event).await?;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Now fetch the peer's profile as another user would (simulating htree peer)
    let filter = Filter::new()
        .author(peer_keys.public_key())
        .kind(Kind::Metadata)
        .limit(1);

    let events = tokio::time::timeout(
        Duration::from_secs(5),
        client.get_events_of(vec![filter], EventSource::relays(None))
    ).await??;

    // Extract name using same logic as fetch_profile_name
    let profile_name = events
        .into_iter()
        .next()
        .and_then(|e| serde_json::from_str::<serde_json::Value>(&e.content).ok())
        .and_then(|p| {
            p.get("display_name")
                .or_else(|| p.get("name"))
                .or_else(|| p.get("username"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        });

    client.disconnect().await?;

    assert_eq!(profile_name, Some("Alice Wonder".to_string()));
    println!("Peer profile name fetched successfully for {}", peer_pubkey_hex);
    Ok(())
}

/// Test that missing profile returns None gracefully
#[tokio::test]
async fn test_fetch_missing_profile_returns_none() -> Result<()> {
    // Start test relay
    let relay = test_relay::TestRelay::new(14853);
    let relay_url = relay.url();

    // Generate keys for a user with NO profile
    let keys = Keys::generate();

    let client = ClientBuilder::default().build();
    client.add_relay(&relay_url).await?;
    client.connect().await;

    // Try to fetch profile that doesn't exist
    let filter = Filter::new()
        .author(keys.public_key())
        .kind(Kind::Metadata)
        .limit(1);

    let events = tokio::time::timeout(
        Duration::from_secs(2),
        client.get_events_of(vec![filter], EventSource::relays(None))
    ).await??;

    client.disconnect().await?;

    assert!(events.is_empty(), "Should not find profile for new keypair");
    println!("Missing profile correctly returns empty");
    Ok(())
}
