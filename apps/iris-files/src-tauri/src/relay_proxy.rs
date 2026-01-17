//! Localhost Nostr relay proxy
//!
//! Provides a NIP-01 WebSocket relay endpoint that proxies to remote relays.
//! Apps can connect to ws://localhost:{port}/relay and use standard Nostr protocol.

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
};
use futures::{SinkExt, StreamExt};
use nostr_sdk::{Client, Event, Filter, Kind, RelayPoolNotification};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

/// Default relays to proxy to
const DEFAULT_RELAYS: &[&str] = &[
    "wss://relay.damus.io",
    "wss://relay.primal.net",
    "wss://nos.lol",
];

/// State for the relay proxy
#[derive(Clone)]
pub struct RelayProxyState {
    client: Arc<RwLock<Option<Client>>>,
}

impl RelayProxyState {
    pub fn new() -> Self {
        Self {
            client: Arc::new(RwLock::new(None)),
        }
    }

    /// Ensure the Nostr client is initialized
    async fn ensure_client(&self) -> Result<Client, String> {
        let mut guard = self.client.write().await;
        if guard.is_none() {
            info!("Initializing relay proxy client...");
            let client = Client::default();
            for relay in DEFAULT_RELAYS {
                if let Err(e) = client.add_relay(*relay).await {
                    warn!("Failed to add relay {}: {}", relay, e);
                }
            }
            client.connect().await;
            info!("Relay proxy connected to {} relays", DEFAULT_RELAYS.len());
            *guard = Some(client.clone());
            Ok(client)
        } else {
            Ok(guard.as_ref().unwrap().clone())
        }
    }
}

impl Default for RelayProxyState {
    fn default() -> Self {
        Self::new()
    }
}

/// Handle WebSocket upgrade for /relay endpoint
pub async fn handle_relay_websocket(
    ws: WebSocketUpgrade,
    State(state): State<RelayProxyState>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_connection(socket, state))
}

/// Handle a single WebSocket connection
async fn handle_connection(socket: WebSocket, state: RelayProxyState) {
    info!("New relay proxy connection");

    let client = match state.ensure_client().await {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to initialize client: {}", e);
            return;
        }
    };

    let (mut sender, mut receiver) = socket.split();

    // Track subscriptions for this connection: sub_id -> nostr-sdk subscription handle
    let subscriptions: Arc<RwLock<HashMap<String, nostr_sdk::SubscriptionId>>> =
        Arc::new(RwLock::new(HashMap::new()));

    // Spawn a task to forward events from nostr-sdk to the WebSocket
    let client_clone = client.clone();
    let subs_clone = subscriptions.clone();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(100);
    let tx_forwarder = tx.clone();

    // Event forwarder task
    let forwarder = tokio::spawn(async move {
        let tx = tx_forwarder;
        let mut notifications = client_clone.notifications();

        while let Ok(notification) = notifications.recv().await {
            match notification {
                RelayPoolNotification::Event { event, .. } => {
                    // Check if this event matches any of our subscriptions
                    // For now, forward all events (proper filtering would check subscription filters)
                    let subs = subs_clone.read().await;
                    for (sub_id, _) in subs.iter() {
                        let msg = serde_json::json!(["EVENT", sub_id, event]);
                        if tx.send(msg.to_string()).await.is_err() {
                            return;
                        }
                    }
                }
                RelayPoolNotification::Message { message, .. } => {
                    // Handle relay messages (EOSE, OK, etc.)
                    match message {
                        nostr_sdk::RelayMessage::EndOfStoredEvents(sdk_sub_id) => {
                            let subs = subs_clone.read().await;
                            // Find the sub_id that matches this SDK subscription
                            for (sub_id, stored_sdk_id) in subs.iter() {
                                if stored_sdk_id == &sdk_sub_id {
                                    let msg = serde_json::json!(["EOSE", sub_id]);
                                    if tx.send(msg.to_string()).await.is_err() {
                                        return;
                                    }
                                    break;
                                }
                            }
                        }
                        nostr_sdk::RelayMessage::Ok {
                            event_id, status, ..
                        } => {
                            let msg = serde_json::json!([
                                "OK",
                                event_id.to_hex(),
                                status,
                                ""
                            ]);
                            if tx.send(msg.to_string()).await.is_err() {
                                return;
                            }
                        }
                        _ => {}
                    }
                }
                RelayPoolNotification::Shutdown => {
                    return;
                }
                _ => {}
            }
        }
    });

    // Forward messages from mpsc to WebSocket
    let forward_to_ws = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if sender.send(Message::Text(msg.into())).await.is_err() {
                break;
            }
        }
    });

    // Process incoming WebSocket messages
    while let Some(msg) = receiver.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                let text_str: &str = text.as_ref();
                debug!("Relay proxy received: {}", text_str);

                if let Err(e) = handle_message(text_str, &client, &subscriptions, &tx).await {
                    warn!("Error handling message: {}", e);
                    let notice = serde_json::json!(["NOTICE", format!("Error: {}", e)]);
                    let _ = tx.send(notice.to_string()).await;
                }
            }
            Ok(Message::Close(_)) => {
                info!("Relay proxy connection closed");
                break;
            }
            Err(e) => {
                error!("WebSocket error: {}", e);
                break;
            }
            _ => {}
        }
    }

    // Cleanup: unsubscribe all
    let subs = subscriptions.read().await;
    for (_, sdk_sub_id) in subs.iter() {
        client.unsubscribe(sdk_sub_id.clone()).await;
    }
    drop(subs);

    forwarder.abort();
    forward_to_ws.abort();

    info!("Relay proxy connection ended");
}

/// Handle a single NIP-01 message
async fn handle_message(
    text: &str,
    client: &Client,
    subscriptions: &Arc<RwLock<HashMap<String, nostr_sdk::SubscriptionId>>>,
    tx: &tokio::sync::mpsc::Sender<String>,
) -> Result<(), String> {
    let parsed: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("Invalid JSON: {}", e))?;

    let arr = parsed
        .as_array()
        .ok_or_else(|| "Message must be an array".to_string())?;

    let msg_type = arr
        .first()
        .and_then(|v| v.as_str())
        .ok_or_else(|| "Message type must be a string".to_string())?;

    match msg_type {
        "REQ" => {
            let sub_id = arr
                .get(1)
                .and_then(|v| v.as_str())
                .ok_or_else(|| "REQ requires subscription ID".to_string())?
                .to_string();

            // Parse filters (rest of array elements)
            let mut filters = Vec::new();
            for filter_value in arr.iter().skip(2) {
                let filter = parse_filter(filter_value)?;
                filters.push(filter);
            }

            if filters.is_empty() {
                return Err("REQ requires at least one filter".to_string());
            }

            debug!("Subscribing with ID: {} and {} filters", sub_id, filters.len());

            // Subscribe via nostr-sdk
            let output = client
                .subscribe(filters, None)
                .await
                .map_err(|e| format!("Subscribe error: {}", e))?;

            // Store mapping (extract SubscriptionId from Output)
            subscriptions.write().await.insert(sub_id, output.val);

            Ok(())
        }
        "CLOSE" => {
            let sub_id = arr
                .get(1)
                .and_then(|v| v.as_str())
                .ok_or_else(|| "CLOSE requires subscription ID".to_string())?;

            if let Some(sdk_sub_id) = subscriptions.write().await.remove(sub_id) {
                client.unsubscribe(sdk_sub_id).await;
            }

            Ok(())
        }
        "EVENT" => {
            let event_value = arr
                .get(1)
                .ok_or_else(|| "EVENT requires event object".to_string())?;

            // Parse and publish event
            let event: Event = serde_json::from_value(event_value.clone())
                .map_err(|e| format!("Invalid event: {}", e))?;

            match client.send_event(event.clone()).await {
                Ok(_output) => {
                    let msg = serde_json::json!(["OK", event.id.to_hex(), true, ""]);
                    let _ = tx.send(msg.to_string()).await;
                }
                Err(e) => {
                    let msg = serde_json::json!(["OK", event.id.to_hex(), false, e.to_string()]);
                    let _ = tx.send(msg.to_string()).await;
                }
            }

            Ok(())
        }
        _ => Err(format!("Unknown message type: {}", msg_type)),
    }
}

/// Parse a NIP-01 filter from JSON
fn parse_filter(value: &serde_json::Value) -> Result<Filter, String> {
    let obj = value
        .as_object()
        .ok_or_else(|| "Filter must be an object".to_string())?;

    let mut filter = Filter::new();

    if let Some(ids) = obj.get("ids").and_then(|v| v.as_array()) {
        for id in ids {
            if let Some(id_str) = id.as_str() {
                if let Ok(event_id) = nostr_sdk::EventId::from_hex(id_str) {
                    filter = filter.id(event_id);
                }
            }
        }
    }

    if let Some(authors) = obj.get("authors").and_then(|v| v.as_array()) {
        for author in authors {
            if let Some(author_str) = author.as_str() {
                if let Ok(pubkey) = nostr_sdk::PublicKey::from_hex(author_str) {
                    filter = filter.author(pubkey);
                }
            }
        }
    }

    if let Some(kinds) = obj.get("kinds").and_then(|v| v.as_array()) {
        for kind in kinds {
            if let Some(k) = kind.as_u64() {
                filter = filter.kind(Kind::from(k as u16));
            }
        }
    }

    if let Some(since) = obj.get("since").and_then(|v| v.as_u64()) {
        filter = filter.since(nostr_sdk::Timestamp::from(since));
    }

    if let Some(until) = obj.get("until").and_then(|v| v.as_u64()) {
        filter = filter.until(nostr_sdk::Timestamp::from(until));
    }

    if let Some(limit) = obj.get("limit").and_then(|v| v.as_u64()) {
        filter = filter.limit(limit as usize);
    }

    // Handle tag filters (#e, #p, etc.)
    for (key, val) in obj {
        if key.starts_with('#') && key.len() == 2 {
            let tag_char = key.chars().nth(1).unwrap();
            if let Some(values) = val.as_array() {
                for v in values {
                    if let Some(s) = v.as_str() {
                        filter = filter.custom_tag(
                            nostr_sdk::SingleLetterTag::from_char(tag_char).unwrap(),
                            [s],
                        );
                    }
                }
            }
        }
    }

    Ok(filter)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_filter_basic() {
        let json = serde_json::json!({
            "kinds": [1],
            "limit": 10
        });

        let _filter = parse_filter(&json).unwrap();
        // Filter was created successfully
    }

    #[test]
    fn test_parse_filter_with_authors() {
        let json = serde_json::json!({
            "authors": ["0".repeat(64)],
            "kinds": [1, 6, 7]
        });

        let _filter = parse_filter(&json).unwrap();
    }
}
