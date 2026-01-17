//! Nostr relay management for worker operations
//!
//! Handles subscription and publishing to Nostr relays.

use nostr_sdk::{
    Client, EventId, Filter, Keys, Kind, NostrSigner, PublicKey, RelayPoolNotification, SecretKey,
    SubscriptionId,
};
use nostrdb::Ndb;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use super::types::{RelayStatEntry, WorkerResponse};

/// Default relays for the worker - matches web app defaults in settings.ts
const DEFAULT_RELAYS: &[&str] = &[
    "wss://relay.damus.io",
    "wss://relay.primal.net",
    "wss://nos.lol",
    "wss://relay.nostr.band",
    "wss://relay.snort.social",
    "wss://temp.iris.to",
];

/// Get relays to use - checks IRIS_TEST_RELAY env var first, then falls back to defaults
fn get_initial_relays() -> Vec<String> {
    // Check for test relay override (e.g., "ws://localhost:4736")
    if let Ok(test_relay) = std::env::var("IRIS_TEST_RELAY") {
        info!("Using test relay from IRIS_TEST_RELAY: {}", test_relay);
        return vec![test_relay];
    }
    DEFAULT_RELAYS.iter().map(|s| s.to_string()).collect()
}

use std::collections::HashSet;

/// Active subscription with its filters and relay state
struct ActiveSubscription {
    filters: Vec<Filter>,
    sdk_id: Option<SubscriptionId>,
    sent_to: HashSet<String>, // relay URLs that have received this sub
}

/// Manages Nostr connections and subscriptions
pub struct NostrManager {
    client: Arc<RwLock<Option<Client>>>,
    subscriptions: Arc<RwLock<HashMap<String, ActiveSubscription>>>,
    identity: Arc<RwLock<Option<Keys>>>,
    shutdown_tx: Arc<RwLock<Option<mpsc::Sender<()>>>>,
    ndb: Arc<RwLock<Option<Arc<Ndb>>>>,
}

impl NostrManager {
    /// Get a clone of the Nostr client (if initialized)
    ///
    /// This allows sharing the same relay connection pool with other components
    /// like WebRTC signaling.
    pub fn get_client(&self) -> Option<Client> {
        self.client.read().clone()
    }

    /// Get current identity keys (if set)
    pub fn get_keys(&self) -> Option<Keys> {
        self.identity.read().clone()
    }
}

impl NostrManager {
    pub fn new() -> Self {
        Self {
            client: Arc::new(RwLock::new(None)),
            subscriptions: Arc::new(RwLock::new(HashMap::new())),
            identity: Arc::new(RwLock::new(None)),
            shutdown_tx: Arc::new(RwLock::new(None)),
            ndb: Arc::new(RwLock::new(None)),
        }
    }

    /// Initialize the Nostr client and connect to relays
    pub async fn ensure_client(&self, app_handle: Option<AppHandle>, ndb: Option<Arc<Ndb>>) -> Result<(), String> {
        {
            let guard = self.client.read();
            if guard.is_some() {
                return Ok(());
            }
        }

        info!("Initializing Nostr client...");

        // Use identity if set, otherwise generate ephemeral keys and store them
        let keys = {
            let identity = self.identity.read();
            if let Some(k) = identity.clone() {
                k
            } else {
                drop(identity); // Release read lock before write
                let ephemeral = Keys::generate();
                *self.identity.write() = Some(ephemeral.clone());
                info!("Generated ephemeral identity: {}", ephemeral.public_key().to_hex()[..8].to_string());
                ephemeral
            }
        };

        let client = Client::new(NostrSigner::Keys(keys.clone()));

        // Add relays (uses IRIS_TEST_RELAY env var if set, otherwise defaults)
        let relays = get_initial_relays();
        for relay in &relays {
            if let Err(e) = client.add_relay(relay.as_str()).await {
                warn!("Failed to add relay {}: {}", relay, e);
            }
        }

        // Connect to relays in background - don't block
        let client_clone = client.clone();
        let relays_for_log = relays.clone();
        tokio::spawn(async move {
            info!("Starting relay connections to {:?}...", relays_for_log);
            client_clone.connect().await;
            // Log connection status after connect
            let relay_map = client_clone.relays().await;
            for (url, relay) in relay_map.iter() {
                let status = relay.status().await;
                info!("Relay {} status: {:?}", url, status);
            }
        });
        info!("Connecting to Nostr relays in background...");

        // Store ndb reference for publish
        if let Some(ref ndb_ref) = ndb {
            *self.ndb.write() = Some(ndb_ref.clone());
        }

        // Start event listener if app_handle is provided
        if let Some(handle) = app_handle {
            self.start_event_listener(client.clone(), handle, ndb).await;
        }

        *self.client.write() = Some(client);
        Ok(())
    }

    /// Retry sending subscriptions that haven't reached any relay yet
    async fn retry_pending_subscriptions(client: &Client, subscriptions: &Arc<RwLock<HashMap<String, ActiveSubscription>>>) {
        let pending: Vec<(String, Vec<Filter>, SubscriptionId)> = {
            let subs = subscriptions.read();
            subs.iter()
                .filter(|(_, active)| active.sent_to.is_empty())
                .map(|(id, active)| {
                    let sdk_id = active
                        .sdk_id
                        .clone()
                        .unwrap_or_else(|| SubscriptionId::new(id.clone()));
                    (id.clone(), active.filters.clone(), sdk_id)
                })
                .collect()
        };

        if pending.is_empty() {
            return;
        }

        debug!("Retrying {} pending subscriptions", pending.len());

        for (sub_id, filters, sdk_id) in pending {
            match client.subscribe_with_id(sdk_id.clone(), filters, None).await {
                Ok(output) => {
                    let mut subs = subscriptions.write();
                    if let Some(active) = subs.get_mut(&sub_id) {
                        active.sdk_id = Some(sdk_id);
                        active.sent_to.clear();
                        for url in output.success.iter() {
                            active.sent_to.insert(url.to_string());
                        }
                        if active.sent_to.is_empty() {
                            debug!("Subscription {} retry queued (no relays connected)", sub_id);
                        } else {
                            info!(
                                "Subscription {} retry succeeded, sent to {} relays",
                                sub_id,
                                active.sent_to.len()
                            );
                        }
                    }
                }
                Err(e) => {
                    debug!("Subscription {} retry failed: {}", sub_id, e);
                }
            }
        }
    }

    /// Start listening for relay events and forward to frontend
    async fn start_event_listener(&self, client: Client, app_handle: AppHandle, ndb: Option<Arc<Ndb>>) {
        let subscriptions = self.subscriptions.clone();
        let (tx, mut rx) = mpsc::channel::<()>(1);
        *self.shutdown_tx.write() = Some(tx);

        let client_for_retry = client.clone();
        let subs_for_retry = subscriptions.clone();

        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            loop {
                Self::retry_pending_subscriptions(&client_for_retry, &subs_for_retry).await;
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        });

        tokio::spawn(async move {
            let mut notifications = client.notifications();

            loop {
                tokio::select! {
                    _ = rx.recv() => {
                        info!("Shutting down Nostr event listener");
                        break;
                    }
                    notification = notifications.recv() => {
                        match notification {
                            Ok(notification) => {
                                match notification {
                                    RelayPoolNotification::Event { event, subscription_id, .. } => {
                                        // Store event in nostrdb (handles duplicates internally via ingester)
                                        if let Some(ref ndb) = ndb {
                                            // Format as relay message for nostrdb
                                            let event_json = serde_json::to_string(&*event).unwrap_or_default();
                                            let relay_msg = format!(r#"["EVENT","{}",{}]"#, subscription_id, event_json);
                                            if let Err(e) = ndb.process_event(&relay_msg) {
                                                debug!("nostrdb process_event error: {:?}", e);
                                            }
                                        }

                                        // Find the worker subscription ID from our mapping
                                        let sub_id = {
                                            let subs = subscriptions.read();
                                            let direct_id = subscription_id.to_string();
                                            if subs.contains_key(&direct_id) {
                                                Some(direct_id)
                                            } else {
                                                subs.iter()
                                                    .find(|(_, active)| active.sdk_id.as_ref() == Some(&subscription_id))
                                                    .map(|(k, _)| k.clone())
                                            }
                                        };

                                        if let Some(sub_id) = sub_id {
                                            debug!("Received event for subscription {}", sub_id);
                                            let response = WorkerResponse::Event {
                                                sub_id,
                                                event: serde_json::to_value(&*event).unwrap_or_default(),
                                            };
                                            if let Err(e) = app_handle.emit("worker_response", &response) {
                                                error!("Failed to emit event: {}", e);
                                            }
                                        }
                                    }
                                    RelayPoolNotification::Message { message, .. } => {
                                        // Handle EOSE
                                        if let nostr_sdk::RelayMessage::EndOfStoredEvents(sdk_sub_id) = message {
                                            let worker_sub_id = {
                                                let subs = subscriptions.read();
                                                let direct_id = sdk_sub_id.to_string();
                                                if subs.contains_key(&direct_id) {
                                                    Some(direct_id)
                                                } else {
                                                    subs.iter()
                                                        .find(|(_, active)| active.sdk_id.as_ref() == Some(&sdk_sub_id))
                                                        .map(|(k, _)| k.clone())
                                                }
                                            };

                                            if let Some(sub_id) = worker_sub_id {
                                                debug!("EOSE for subscription {}", sub_id);
                                                let response = WorkerResponse::Eose { sub_id };
                                                if let Err(e) = app_handle.emit("worker_response", &response) {
                                                    error!("Failed to emit EOSE: {}", e);
                                                }
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            Err(e) => {
                                error!("Notification error: {}", e);
                                break;
                            }
                        }
                    }
                }
            }
        });
    }

    /// Subscribe to events with filters
    /// Stores subscription even if relays aren't connected - will be sent when they connect
    pub async fn subscribe(&self, sub_id: String, filters: Vec<Filter>) -> Result<(), String> {
        debug!("Creating subscription {} with {} filters", sub_id, filters.len());

        // Store the subscription with its filters
        let sdk_id = SubscriptionId::new(sub_id.clone());
        let mut active_sub = ActiveSubscription {
            filters: filters.clone(),
            sdk_id: Some(sdk_id.clone()),
            sent_to: HashSet::new(),
        };

        // Try to send to connected relays
        let client = { self.client.read().clone() };
        if let Some(client) = client {
            match client.subscribe_with_id(sdk_id, filters, None).await {
                Ok(output) => {
                    // Track which relays received it
                    for url in output.success.iter() {
                        active_sub.sent_to.insert(url.to_string());
                    }
                    info!("Subscription {} sent to {} relays", sub_id, output.success.len());
                }
                Err(e) => {
                    // Not an error - subscription is queued for when relays connect
                    debug!("Subscription {} queued (no relays connected): {}", sub_id, e);
                }
            }
        } else {
            debug!("Subscription {} queued (client not initialized)", sub_id);
        }

        self.subscriptions.write().insert(sub_id.clone(), active_sub);
        Ok(())
    }

    /// Unsubscribe from a subscription
    pub async fn unsubscribe(&self, sub_id: &str) -> Result<(), String> {
        let active_sub = {
            let mut subs = self.subscriptions.write();
            subs.remove(sub_id)
        };

        if let Some(active_sub) = active_sub {
            if let Some(sdk_id) = active_sub.sdk_id {
                let client = { self.client.read().clone() };
                if let Some(client) = client {
                    client.unsubscribe(sdk_id).await;
                }
            }
            info!("Unsubscribed: {}", sub_id);
        }

        Ok(())
    }

    /// Publish an event
    pub async fn publish(&self, event_json: serde_json::Value) -> Result<EventId, String> {
        let client = {
            let guard = self.client.read();
            guard.clone().ok_or("Nostr client not initialized")?
        };

        // Parse the event JSON - this should be a signed event or event builder
        let event: nostr_sdk::Event =
            serde_json::from_value(event_json.clone()).map_err(|e| format!("Invalid event JSON: {}", e))?;

        // Store in nostrdb before sending (so republishTree can find it)
        if let Some(ndb) = self.ndb.read().as_ref() {
            let event_str = serde_json::to_string(&event_json).unwrap_or_default();
            let relay_msg = format!(r#"["EVENT","_published",{}]"#, event_str);
            if let Err(e) = ndb.process_event(&relay_msg) {
                debug!("nostrdb process_event error on publish: {:?}", e);
            }
        }

        let output = client
            .send_event(event)
            .await
            .map_err(|e| format!("Publish error: {}", e))?;

        let event_id = output.val;
        info!("Published event: {}", event_id);
        Ok(event_id)
    }

    /// Fetch events matching filters (one-shot query, not subscription)
    pub async fn fetch_events(&self, filters: Vec<Filter>) -> Result<Vec<nostr_sdk::Event>, String> {
        let client = {
            let guard = self.client.read();
            guard.clone().ok_or("Nostr client not initialized")?
        };

        let events = client
            .get_events_of(filters, nostr_sdk::EventSource::relays(Some(std::time::Duration::from_secs(3))))
            .await
            .map_err(|e| format!("Fetch error: {}", e))?;

        Ok(events)
    }

    /// Set identity for signing events
    pub fn set_identity(&self, pubkey: &str, nsec: Option<&str>) -> Result<(), String> {
        // Validate pubkey format
        let _public_key = if pubkey.starts_with("npub1") {
            PublicKey::parse(pubkey).map_err(|e| format!("Invalid npub: {}", e))?
        } else {
            PublicKey::from_hex(pubkey).map_err(|e| format!("Invalid hex public key: {}", e))?
        };

        // Only create keys if we have a secret key (for signing)
        if let Some(nsec) = nsec {
            let secret_key = if nsec.starts_with("nsec1") {
                SecretKey::parse(nsec).map_err(|e| format!("Invalid nsec: {}", e))?
            } else {
                SecretKey::from_hex(nsec).map_err(|e| format!("Invalid hex secret key: {}", e))?
            };
            let keys = Keys::new(secret_key);
            *self.identity.write() = Some(keys);
            info!("Identity set with signing capability: {}", pubkey);
        } else {
            // Read-only identity - we validated the pubkey but can't sign
            info!("Read-only identity set: {}", pubkey);
        }
        Ok(())
    }

    /// Get the current public key
    pub fn get_pubkey(&self) -> Option<String> {
        let identity = self.identity.read();
        identity.as_ref().map(|k| k.public_key().to_hex())
    }

    /// Set relays - disconnects from current relays and connects to new ones
    pub async fn set_relays(&self, relays: Vec<String>) -> Result<(), String> {
        // If test relay is configured via env var, ignore frontend relay updates
        if std::env::var("IRIS_TEST_RELAY").is_ok() {
            debug!("Ignoring set_relays (IRIS_TEST_RELAY is set)");
            return Ok(());
        }

        let client = {
            let guard = self.client.read();
            guard.clone()
        };

        if let Some(client) = client {
            // Remove all existing relays
            let existing = client.relays().await;
            for url in existing.keys() {
                if let Err(e) = client.remove_relay(url.as_str()).await {
                    warn!("Failed to remove relay {}: {}", url, e);
                }
            }

            // Add new relays
            for relay in &relays {
                if let Err(e) = client.add_relay(relay.as_str()).await {
                    warn!("Failed to add relay {}: {}", relay, e);
                }
            }

            // Start connection in background - don't block
            let client_clone = client.clone();
            tokio::spawn(async move {
                client_clone.connect().await;
            });
            info!("Updated relays (connecting in background): {:?}", relays);
        } else {
            // Client not initialized yet - it will use these when initialized
            info!("Relays will be set when client initializes: {:?}", relays);
        }

        Ok(())
    }

    /// Get current relay URLs
    pub async fn get_relays(&self) -> Vec<String> {
        let client = {
            let guard = self.client.read();
            guard.clone()
        };

        if let Some(client) = client {
            client
                .relays()
                .await
                .keys()
                .map(|u| u.to_string())
                .collect()
        } else {
            DEFAULT_RELAYS.iter().map(|s| s.to_string()).collect()
        }
    }

    /// Get relay connection statistics
    pub async fn get_relay_stats(&self) -> Vec<RelayStatEntry> {
        let client = {
            let guard = self.client.read();
            guard.clone()
        };

        if let Some(client) = client {
            // Use timeout to avoid hanging if client.relays() blocks
            match tokio::time::timeout(std::time::Duration::from_millis(500), client.relays()).await {
                Ok(relays) => {
                    let mut stats = Vec::new();
                    for (url, relay) in relays.iter() {
                        let status = relay.status().await;
                        let connected = status == nostr_sdk::RelayStatus::Connected;
                        let connecting = status == nostr_sdk::RelayStatus::Connecting
                            || status == nostr_sdk::RelayStatus::Pending;
                        stats.push(RelayStatEntry {
                            url: url.to_string(),
                            connected,
                            connecting,
                        });
                    }
                    return stats;
                }
                Err(_) => {
                    // Timeout - fall through to return defaults
                }
            }
        }

        // Return default relays as disconnected (either no client or timeout)
        DEFAULT_RELAYS
            .iter()
            .map(|url| RelayStatEntry {
                url: url.to_string(),
                connected: false,
                connecting: false,
            })
            .collect()
    }

    /// Disconnect and cleanup
    #[allow(dead_code)]
    pub async fn disconnect(&self) {
        if let Some(tx) = self.shutdown_tx.write().take() {
            let _ = tx.send(()).await;
        }

        if let Some(client) = self.client.write().take() {
            let _ = client.disconnect().await;
            info!("Disconnected from Nostr relays");
        }

        self.subscriptions.write().clear();
    }
}

impl Default for NostrManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Helper to convert serde_json::Value filters to nostr-sdk Filters
pub fn parse_filters(filters_json: Vec<serde_json::Value>) -> Result<Vec<Filter>, String> {
    filters_json
        .into_iter()
        .map(|f| {
            // Parse filter from JSON
            let mut filter = Filter::new();

            // IDs
            if let Some(ids) = f.get("ids").and_then(|v| v.as_array()) {
                for id in ids {
                    if let Some(id_str) = id.as_str() {
                        if let Ok(event_id) = EventId::from_hex(id_str) {
                            filter = filter.id(event_id);
                        }
                    }
                }
            }

            // Authors
            if let Some(authors) = f.get("authors").and_then(|v| v.as_array()) {
                for author in authors {
                    if let Some(author_str) = author.as_str() {
                        if let Ok(pk) = PublicKey::from_hex(author_str) {
                            filter = filter.author(pk);
                        } else if let Ok(pk) = PublicKey::parse(author_str) {
                            filter = filter.author(pk);
                        }
                    }
                }
            }

            // Kinds
            if let Some(kinds) = f.get("kinds").and_then(|v| v.as_array()) {
                for kind in kinds {
                    if let Some(k) = kind.as_u64() {
                        filter = filter.kind(Kind::from(k as u16));
                    }
                }
            }

            // Tags (#e, #p, #t, etc.)
            if let Some(obj) = f.as_object() {
                for (key, value) in obj {
                    if key.starts_with('#') && key.len() == 2 {
                        let tag_name = key.chars().nth(1).unwrap();
                        if let Some(values) = value.as_array() {
                            let tag_values: Vec<String> = values
                                .iter()
                                .filter_map(|v| v.as_str().map(String::from))
                                .collect();
                            if !tag_values.is_empty() {
                                filter = filter.custom_tag(
                                    nostr_sdk::SingleLetterTag::from_char(tag_name).unwrap(),
                                    tag_values,
                                );
                            }
                        }
                    }
                }
            }

            // Since/Until
            if let Some(since) = f.get("since").and_then(|v| v.as_u64()) {
                filter = filter.since(nostr_sdk::Timestamp::from(since));
            }
            if let Some(until) = f.get("until").and_then(|v| v.as_u64()) {
                filter = filter.until(nostr_sdk::Timestamp::from(until));
            }

            // Limit
            if let Some(limit) = f.get("limit").and_then(|v| v.as_u64()) {
                filter = filter.limit(limit as usize);
            }

            Ok(filter)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_filters_basic() {
        let filter_json = serde_json::json!({
            "kinds": [1, 6],
            "limit": 10
        });

        let filters = parse_filters(vec![filter_json]).unwrap();
        assert_eq!(filters.len(), 1);
    }

    #[test]
    fn test_parse_filters_with_authors() {
        let filter_json = serde_json::json!({
            "authors": ["82341f882b6eabcd2ba7f1ef90aad961cf074af15b9ef44a09f9d2a8fbfbe6a2"],
            "kinds": [1]
        });

        let filters = parse_filters(vec![filter_json]).unwrap();
        assert_eq!(filters.len(), 1);
    }

    #[test]
    fn test_parse_filters_with_tags() {
        let filter_json = serde_json::json!({
            "#e": ["event_id_here"],
            "#p": ["pubkey_here"]
        });

        let filters = parse_filters(vec![filter_json]).unwrap();
        assert_eq!(filters.len(), 1);
    }

    #[test]
    fn test_nostr_manager_new() {
        let manager = NostrManager::new();
        assert!(manager.get_pubkey().is_none());
    }

    #[test]
    fn test_set_identity_with_npub() {
        let manager = NostrManager::new();
        // Valid test npub (read-only, no signing capability)
        let result = manager.set_identity(
            "npub180cvv07tjdrrgpa0j7j7tmnyl2yr6yr7l8j4s3evf6u64th6gkwsyjh6w6",
            None,
        );
        assert!(result.is_ok());
        // Read-only identity doesn't set keys (can't sign)
        assert!(manager.get_pubkey().is_none());
    }

    #[test]
    fn test_set_identity_with_nsec() {
        let manager = NostrManager::new();
        // Generate a test keypair
        let keys = Keys::generate();
        let nsec = keys.secret_key().to_secret_hex();
        let npub = keys.public_key().to_hex();

        let result = manager.set_identity(&npub, Some(&nsec));
        assert!(result.is_ok());
        // With nsec, we have a signing identity
        assert!(manager.get_pubkey().is_some());
    }

    #[test]
    fn test_set_identity_with_hex() {
        let manager = NostrManager::new();
        let result = manager.set_identity(
            "82341f882b6eabcd2ba7f1ef90aad961cf074af15b9ef44a09f9d2a8fbfbe6a2",
            None,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_set_identity_invalid() {
        let manager = NostrManager::new();
        let result = manager.set_identity("invalid_pubkey", None);
        assert!(result.is_err());
    }
}
