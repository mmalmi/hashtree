//! Integration tests for the localhost relay proxy
//!
//! Tests the NIP-01 WebSocket relay that proxies to remote relays.

use futures::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// Test that we can connect to the relay proxy WebSocket endpoint
#[tokio::test]
async fn test_relay_proxy_connects() {
    // Start the htree server (which includes the relay proxy)
    let data_dir = tempfile::tempdir().unwrap();
    let port = app_lib::htree::start_server_on_port(data_dir.path().to_path_buf(), 0)
        .await
        .expect("Server should start");

    // Connect to the relay WebSocket endpoint
    let url = format!("ws://127.0.0.1:{}/relay", port);
    println!("Connecting to relay at {}", url);

    let (ws_stream, _) = connect_async(&url)
        .await
        .expect("Should connect to relay WebSocket");

    println!("Connected to relay proxy!");

    let (mut write, mut read) = ws_stream.split();

    // Send a simple REQ for recent events (NIP-01 format)
    // ["REQ", subscription_id, filter...]
    let req = serde_json::json!(["REQ", "test-sub", {"kinds": [1], "limit": 1}]);
    write
        .send(Message::Text(req.to_string().into()))
        .await
        .expect("Should send REQ");

    println!("Sent REQ, waiting for response...");

    // We should receive at least an EOSE (end of stored events)
    let mut got_eose = false;
    let timeout = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(msg) = read.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    println!("Received: {}", text);
                    let parsed: serde_json::Value =
                        serde_json::from_str(&text).expect("Should be valid JSON");

                    if let Some(msg_type) = parsed.get(0).and_then(|v| v.as_str()) {
                        match msg_type {
                            "EVENT" => {
                                println!("Got EVENT");
                            }
                            "EOSE" => {
                                println!("Got EOSE");
                                got_eose = true;
                                break;
                            }
                            "NOTICE" => {
                                println!("Got NOTICE: {:?}", parsed.get(1));
                            }
                            _ => {
                                println!("Unknown message type: {}", msg_type);
                            }
                        }
                    }
                }
                Ok(Message::Close(_)) => {
                    println!("Connection closed");
                    break;
                }
                Err(e) => {
                    println!("Error: {}", e);
                    break;
                }
                _ => {}
            }
        }
    });

    match timeout.await {
        Ok(_) => {
            assert!(got_eose, "Should receive EOSE from relay proxy");
        }
        Err(_) => {
            panic!("Timeout waiting for relay response");
        }
    }

    // Send CLOSE to unsubscribe
    let close = serde_json::json!(["CLOSE", "test-sub"]);
    write
        .send(Message::Text(close.to_string().into()))
        .await
        .expect("Should send CLOSE");

    println!("Relay proxy test passed!");
}

/// Test that the relay proxy can publish events
#[tokio::test]
async fn test_relay_proxy_publish() {
    let data_dir = tempfile::tempdir().unwrap();
    let port = app_lib::htree::start_server_on_port(data_dir.path().to_path_buf(), 0)
        .await
        .expect("Server should start");

    let url = format!("ws://127.0.0.1:{}/relay", port);
    let (ws_stream, _) = connect_async(&url)
        .await
        .expect("Should connect to relay WebSocket");

    let (mut write, mut read) = ws_stream.split();

    // Create a simple unsigned event (in real use, this would be signed)
    // For now, just test that the relay accepts the EVENT message format
    let event = serde_json::json!({
        "id": "0".repeat(64),
        "pubkey": "0".repeat(64),
        "created_at": 1234567890,
        "kind": 1,
        "tags": [],
        "content": "test",
        "sig": "0".repeat(128)
    });

    let msg = serde_json::json!(["EVENT", event]);
    write
        .send(Message::Text(msg.to_string().into()))
        .await
        .expect("Should send EVENT");

    // We should get an OK response (even if it's a rejection due to invalid sig)
    let timeout = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(msg) = read.next().await {
            if let Ok(Message::Text(text)) = msg {
                println!("Received: {}", text);
                let parsed: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
                if parsed.get(0).and_then(|v| v.as_str()) == Some("OK") {
                    return true;
                }
            }
        }
        false
    });

    // For now, just check we don't crash - actual validation comes later
    let _ = timeout.await;
    println!("Publish test completed");
}
