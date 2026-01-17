//! Nostr-based root resolver
//!
//! Maps npub/treename keys to content identifiers (Cid) using Nostr events.
//!
//! Key format: "npub1.../treename"
//!
//! Uses kind 30078 (APP_DATA) events with:
//! - d-tag: tree name (NIP-33 replaceable)
//! - l-tag: "hashtree" (for filtering)
//! - hash-tag: content hash (always present)
//! - key-tag: CHK decryption key (optional, for private content)
//! - encrypted_key-tag: encrypted key (optional, for shared content)

use crate::{ResolverEntry, ResolverError, RootResolver};
use async_trait::async_trait;
use hashtree_core::{from_hex, to_hex, Cid};
use nostr_sdk::prelude::*;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, RwLock};

use hashtree_core::{decrypt, encrypt};

const HASHTREE_KIND: u16 = 30078;
const HASHTREE_LABEL: &str = "hashtree";

/// Configuration for NostrRootResolver
#[derive(Clone)]
pub struct NostrResolverConfig {
    /// Nostr relays to connect to
    pub relays: Vec<String>,
    /// Timeout for one-shot resolve operations
    pub resolve_timeout: Duration,
    /// Secret key for publishing (optional)
    pub secret_key: Option<Keys>,
}

impl Default for NostrResolverConfig {
    fn default() -> Self {
        Self {
            relays: vec![
                "wss://relay.damus.io".into(),
                "wss://relay.primal.net".into(),
                "wss://nos.lol".into(),
            ],
            resolve_timeout: Duration::from_millis(500),
            secret_key: None,
        }
    }
}

/// Tag names for hashtree events
const TAG_HASH: &str = "hash";
const TAG_KEY: &str = "key";
#[allow(dead_code)]
const TAG_ENCRYPTED_KEY: &str = "encrypted_key";

/// Subscription state
struct Subscription {
    tx: mpsc::Sender<Option<Cid>>,
    current_cid: Option<Cid>,
    latest_created_at: Timestamp,
}

/// NostrRootResolver - Maps npub/treename keys to merkle root hashes
pub struct NostrRootResolver {
    client: Client,
    config: NostrResolverConfig,
    subscriptions: Arc<RwLock<HashMap<String, Subscription>>>,
}

impl NostrRootResolver {
    /// Create a new NostrRootResolver
    pub async fn new(config: NostrResolverConfig) -> Result<Self, ResolverError> {
        let keys = config.secret_key.clone().unwrap_or_else(Keys::generate);
        let client = Client::new(keys);

        // Add relays
        for relay in &config.relays {
            client
                .add_relay(relay)
                .await
                .map_err(|e| ResolverError::Network(e.to_string()))?;
        }

        // Connect
        client.connect().await;

        Ok(Self {
            client,
            config,
            subscriptions: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Parse a key into pubkey and tree name
    fn parse_key(key: &str) -> Result<(PublicKey, String), ResolverError> {
        let parts: Vec<&str> = key.split('/').collect();
        if parts.len() != 2 {
            return Err(ResolverError::InvalidKey(format!(
                "Key must be in format 'npub.../treename', got: {}",
                key
            )));
        }

        let npub_str = parts[0];
        let tree_name = parts[1].to_string();

        let pubkey = PublicKey::from_bech32(npub_str)
            .map_err(|_| ResolverError::InvalidKey(format!("Invalid npub: {}", npub_str)))?;

        Ok((pubkey, tree_name))
    }

    /// Get current user's public key (if we have a secret key)
    pub fn pubkey(&self) -> Option<PublicKey> {
        self.config.secret_key.as_ref().map(|k| k.public_key())
    }

    /// Extract Cid from event tags
    fn cid_from_event(event: &Event) -> Option<Cid> {
        let mut hash_hex: Option<String> = None;
        let mut key_hex: Option<String> = None;

        for tag in event.tags.iter() {
            let tag_vec = tag.as_slice();
            if tag_vec.len() >= 2 {
                match tag_vec[0].as_str() {
                    "hash" => hash_hex = Some(tag_vec[1].clone()),
                    "key" => key_hex = Some(tag_vec[1].clone()),
                    _ => {}
                }
            }
        }

        // hash is required
        let hash = from_hex(&hash_hex?).ok()?;

        // key is optional
        let key = key_hex.and_then(|k| {
            let bytes = hex::decode(&k).ok()?;
            if bytes.len() == 32 {
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&bytes);
                Some(arr)
            } else {
                None
            }
        });

        Some(Cid { hash, key })
    }

    /// Extract Cid from event with encrypted_key decryption
    fn cid_from_event_shared(event: &Event, share_secret: &[u8; 32]) -> Option<Cid> {
        let mut hash_hex: Option<String> = None;
        let mut encrypted_key_hex: Option<String> = None;

        for tag in event.tags.iter() {
            let tag_vec = tag.as_slice();
            if tag_vec.len() >= 2 {
                match tag_vec[0].as_str() {
                    "hash" => hash_hex = Some(tag_vec[1].clone()),
                    "encrypted_key" => encrypted_key_hex = Some(tag_vec[1].clone()),
                    _ => {}
                }
            }
        }

        let hash = from_hex(&hash_hex?).ok()?;

        // Decrypt the encrypted_key with share_secret
        let key = if let Some(ek_hex) = encrypted_key_hex {
            let encrypted_key = hex::decode(&ek_hex).ok()?;
            let decrypted = decrypt(&encrypted_key, share_secret).ok()?;
            if decrypted.len() == 32 {
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&decrypted);
                Some(arr)
            } else {
                None
            }
        } else {
            None
        };

        Some(Cid { hash, key })
    }

    /// Resolve a key, waiting indefinitely until found.
    ///
    /// Unlike `resolve()` which returns `None` after timeout, this method
    /// subscribes and waits until a Cid is found. Caller should apply their
    /// own timeout if needed (e.g., via `tokio::time::timeout`).
    ///
    /// This matches the behavior of hashtree-ts NostrRootResolver.
    pub async fn resolve_wait(&self, key: &str) -> Result<Cid, ResolverError> {
        // First try a quick resolve
        if let Some(cid) = self.resolve(key).await? {
            return Ok(cid);
        }

        // Not found, subscribe and wait
        let mut rx = self.subscribe(key).await?;

        // Wait for first non-None value
        while let Some(maybe_cid) = rx.recv().await {
            if let Some(cid) = maybe_cid {
                return Ok(cid);
            }
        }

        Err(ResolverError::Stopped)
    }
}

#[async_trait]
impl RootResolver for NostrRootResolver {
    async fn resolve(&self, key: &str) -> Result<Option<Cid>, ResolverError> {
        let (pubkey, tree_name) = Self::parse_key(key)?;

        // Create filter for this specific tree
        let filter = Filter::new()
            .kind(Kind::Custom(HASHTREE_KIND))
            .author(pubkey)
            .custom_tag(SingleLetterTag::lowercase(Alphabet::D), vec![tree_name.clone()])
            .custom_tag(SingleLetterTag::lowercase(Alphabet::L), vec![HASHTREE_LABEL]);

        // Fetch events from relays
        let source = EventSource::relays(Some(self.config.resolve_timeout));
        let events = self
            .client
            .get_events_of(vec![filter], source)
            .await
            .map_err(|e| ResolverError::Network(e.to_string()))?;

        // Find the latest event with matching d-tag
        let mut latest_event: Option<&Event> = None;
        let mut latest_created_at = Timestamp::from(0);

        for event in events.iter() {
            // Verify d-tag matches
            let d_tag = event.tags.iter().find_map(|tag| {
                if let Some(TagStandard::Identifier(id)) = tag.as_standardized() {
                    Some(id.clone())
                } else {
                    None
                }
            });

            if d_tag.as_deref() != Some(&tree_name) {
                continue;
            }

            if event.created_at > latest_created_at {
                latest_created_at = event.created_at;
                latest_event = Some(event);
            }
        }

        // Extract Cid from event tags
        match latest_event {
            Some(event) => Ok(Self::cid_from_event(event)),
            None => Ok(None),
        }
    }

    async fn resolve_shared(&self, key: &str, share_secret: &[u8; 32]) -> Result<Option<Cid>, ResolverError> {
        let (pubkey, tree_name) = Self::parse_key(key)?;

        let filter = Filter::new()
            .kind(Kind::Custom(HASHTREE_KIND))
            .author(pubkey)
            .custom_tag(SingleLetterTag::lowercase(Alphabet::D), vec![tree_name.clone()])
            .custom_tag(SingleLetterTag::lowercase(Alphabet::L), vec![HASHTREE_LABEL]);

        let source = EventSource::relays(Some(self.config.resolve_timeout));
        let events = self
            .client
            .get_events_of(vec![filter], source)
            .await
            .map_err(|e| ResolverError::Network(e.to_string()))?;

        let mut latest_event: Option<&Event> = None;
        let mut latest_created_at = Timestamp::from(0);

        for event in events.iter() {
            let d_tag = event.tags.iter().find_map(|tag| {
                if let Some(TagStandard::Identifier(id)) = tag.as_standardized() {
                    Some(id.clone())
                } else {
                    None
                }
            });

            if d_tag.as_deref() != Some(&tree_name) {
                continue;
            }

            if event.created_at > latest_created_at {
                latest_created_at = event.created_at;
                latest_event = Some(event);
            }
        }

        match latest_event {
            Some(event) => Ok(Self::cid_from_event_shared(event, share_secret)),
            None => Ok(None),
        }
    }

    async fn subscribe(&self, key: &str) -> Result<mpsc::Receiver<Option<Cid>>, ResolverError> {
        let (pubkey, tree_name) = Self::parse_key(key)?;

        let (tx, rx) = mpsc::channel(16);

        // Check if we already have a subscription
        {
            let subs = self.subscriptions.read().await;
            if let Some(sub) = subs.get(key) {
                // Send current value
                let _ = tx.send(sub.current_cid.clone()).await;
                // Note: In production, you'd want to share subscriptions
                // For simplicity, we create a new one
            }
        }

        // Create filter
        let filter = Filter::new()
            .kind(Kind::Custom(HASHTREE_KIND))
            .author(pubkey)
            .custom_tag(SingleLetterTag::lowercase(Alphabet::D), vec![tree_name.clone()])
            .custom_tag(SingleLetterTag::lowercase(Alphabet::L), vec![HASHTREE_LABEL]);

        // Store subscription state
        {
            let mut subs = self.subscriptions.write().await;
            subs.insert(
                key.to_string(),
                Subscription {
                    tx: tx.clone(),
                    current_cid: None,
                    latest_created_at: Timestamp::from(0),
                },
            );
        }

        // Subscribe to events
        let subscriptions = self.subscriptions.clone();
        let key_clone = key.to_string();
        let tree_name_clone = tree_name.clone();

        // Spawn subscription handler
        let client = self.client.clone();
        tokio::spawn(async move {
            let sub_id = client
                .subscribe(vec![filter], None)
                .await;

            if sub_id.is_err() {
                return;
            }

            // Handle incoming events via notifications
            let mut notifications = client.notifications();

            while let Ok(notification) = notifications.recv().await {
                if let RelayPoolNotification::Event { event, .. } = notification {
                    // Verify d-tag matches
                    let d_tag = event.tags.iter().find_map(|tag| {
                        if let Some(TagStandard::Identifier(id)) = tag.as_standardized() {
                            Some(id.clone())
                        } else {
                            None
                        }
                    });

                    if d_tag.as_deref() != Some(&tree_name_clone) {
                        continue;
                    }

                    let mut subs = subscriptions.write().await;
                    if let Some(sub) = subs.get_mut(&key_clone) {
                        let new_cid = NostrRootResolver::cid_from_event(&event);
                        if event.created_at >= sub.latest_created_at && new_cid != sub.current_cid {
                            sub.current_cid = new_cid.clone();
                            sub.latest_created_at = event.created_at;

                            if sub.tx.send(new_cid).await.is_err() {
                                // Receiver dropped, clean up
                                subs.remove(&key_clone);
                                break;
                            }
                        }
                    } else {
                        break;
                    }
                }
            }
        });

        Ok(rx)
    }

    async fn publish(&self, key: &str, cid: &Cid) -> Result<bool, ResolverError> {
        let (pubkey, tree_name) = Self::parse_key(key)?;

        // Check we own this key
        let my_pubkey = self.pubkey().ok_or(ResolverError::NotAuthorized)?;
        if pubkey != my_pubkey {
            return Err(ResolverError::NotAuthorized);
        }

        // Build event with tags
        let mut tags = vec![
            Tag::identifier(tree_name.clone()),
            Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::L)),
                vec![HASHTREE_LABEL],
            ),
            Tag::custom(TagKind::Custom(TAG_HASH.into()), vec![to_hex(&cid.hash)]),
        ];

        // Add key tag if present
        if let Some(key) = cid.key {
            tags.push(Tag::custom(TagKind::Custom(TAG_KEY.into()), vec![hex::encode(key)]));
        }

        // Content is empty - all data in tags
        let event = EventBuilder::new(Kind::Custom(HASHTREE_KIND), "", tags);

        // Publish
        let output = self
            .client
            .send_event_builder(event)
            .await
            .map_err(|e| ResolverError::Network(e.to_string()))?;

        // Update local subscription state
        {
            let mut subs = self.subscriptions.write().await;
            if let Some(sub) = subs.get_mut(key) {
                sub.current_cid = Some(cid.clone());
                sub.latest_created_at = Timestamp::now();
                let _ = sub.tx.send(Some(cid.clone())).await;
            }
        }

        Ok(!output.failed.is_empty() || !output.success.is_empty())
    }

    async fn publish_shared(&self, key: &str, cid: &Cid, share_secret: &[u8; 32]) -> Result<bool, ResolverError> {
        let (pubkey, tree_name) = Self::parse_key(key)?;

        let my_pubkey = self.pubkey().ok_or(ResolverError::NotAuthorized)?;
        if pubkey != my_pubkey {
            return Err(ResolverError::NotAuthorized);
        }

        let mut tags = vec![
            Tag::identifier(tree_name.clone()),
            Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::L)),
                vec![HASHTREE_LABEL],
            ),
            Tag::custom(TagKind::Custom(TAG_HASH.into()), vec![to_hex(&cid.hash)]),
        ];

        // Encrypt the key with share_secret
        if let Some(key) = cid.key {
            let encrypted_key = encrypt(&key, share_secret)
                .map_err(|e| ResolverError::Other(format!("Encryption error: {}", e)))?;
            tags.push(Tag::custom(TagKind::Custom(TAG_ENCRYPTED_KEY.into()), vec![hex::encode(encrypted_key)]));
        }

        let event = EventBuilder::new(Kind::Custom(HASHTREE_KIND), "", tags);

        let output = self
            .client
            .send_event_builder(event)
            .await
            .map_err(|e| ResolverError::Network(e.to_string()))?;

        Ok(!output.failed.is_empty() || !output.success.is_empty())
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ResolverEntry>, ResolverError> {
        let parts: Vec<&str> = prefix.split('/').collect();
        if parts.is_empty() {
            return Ok(vec![]);
        }

        let npub_str = parts[0];
        let pubkey = PublicKey::from_bech32(npub_str)
            .map_err(|_| ResolverError::InvalidKey(format!("Invalid npub: {}", npub_str)))?;

        // Filter for all hashtree events from this author
        let filter = Filter::new()
            .kind(Kind::Custom(HASHTREE_KIND))
            .author(pubkey)
            .custom_tag(SingleLetterTag::lowercase(Alphabet::L), vec![HASHTREE_LABEL]);

        let source = EventSource::relays(Some(self.config.resolve_timeout));
        let events = self
            .client
            .get_events_of(vec![filter], source)
            .await
            .map_err(|e| ResolverError::Network(e.to_string()))?;

        // Deduplicate by d-tag, keeping latest event
        let mut entries_by_d_tag: HashMap<String, &Event> = HashMap::new();

        for event in events.iter() {
            let d_tag = event.tags.iter().find_map(|tag| {
                if let Some(TagStandard::Identifier(id)) = tag.as_standardized() {
                    Some(id.clone())
                } else {
                    None
                }
            });

            if let Some(d_tag) = d_tag {
                let existing = entries_by_d_tag.get(&d_tag);
                if existing.is_none() || existing.unwrap().created_at < event.created_at {
                    entries_by_d_tag.insert(d_tag, event);
                }
            }
        }

        // Convert to entries
        let mut result = Vec::new();
        for (d_tag, event) in entries_by_d_tag {
            if let Some(cid) = Self::cid_from_event(event) {
                result.push(ResolverEntry {
                    key: format!("{}/{}", npub_str, d_tag),
                    cid,
                });
            }
        }

        Ok(result)
    }

    async fn stop(&self) -> Result<(), ResolverError> {
        let _ = self.client.disconnect().await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_key_valid() {
        // Generate a valid npub for testing
        let keys = Keys::generate();
        let npub = keys.public_key().to_bech32().unwrap();
        let key = format!("{}/mytree", npub);

        let result = NostrRootResolver::parse_key(&key);
        assert!(result.is_ok());
        let (pubkey, tree_name) = result.unwrap();
        assert_eq!(pubkey, keys.public_key());
        assert_eq!(tree_name, "mytree");
    }

    #[test]
    fn test_parse_key_invalid_format() {
        let key = "notvalid";
        let result = NostrRootResolver::parse_key(key);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_key_invalid_npub() {
        let key = "notannpub/mytree";
        let result = NostrRootResolver::parse_key(key);
        assert!(result.is_err());
    }
}
