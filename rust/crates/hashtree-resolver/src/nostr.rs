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
//! - key-tag: CHK decryption key (public)
//! - encryptedKey-tag: XOR-masked key (link-visible)
//! - selfEncryptedKey-tag: NIP-44 key encrypted to self (private)
//! - encrypted_key-tag: legacy AES-GCM shared key (backwards compat)

use crate::{ResolverEntry, ResolverError, RootResolver};
use async_trait::async_trait;
use hashtree_core::{from_hex, to_hex, Cid};
use nostr_sdk::prelude::nip44;
use nostr_sdk::prelude::*;
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, RwLock};
use tokio::task::JoinSet;

use hashtree_core::{decrypt, xor_keys};

const HASHTREE_KIND: u16 = 30078;
const HASHTREE_LABEL: &str = "hashtree";
const DEFAULT_SUCCESSFUL_RELAY_QUORUM: usize = 2;
const DEFAULT_SOFT_RESOLVE_TIMEOUT: Duration = Duration::from_secs(3);

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
                "wss://relay.snort.social".into(),
            ],
            resolve_timeout: Duration::from_millis(500),
            secret_key: None,
        }
    }
}

/// Tag names for hashtree events
const TAG_HASH: &str = "hash";
const TAG_KEY: &str = "key";
const TAG_ENCRYPTED_KEY: &str = "encryptedKey";
const TAG_SELF_ENCRYPTED_KEY: &str = "selfEncryptedKey";
const TAG_ENCRYPTED_KEY_LEGACY: &str = "encrypted_key";

fn has_label(event: &Event, label: &str) -> bool {
    event.tags.iter().any(|tag| {
        let tag_vec = tag.as_slice();
        tag_vec.len() >= 2 && tag_vec[0].as_str() == "l" && tag_vec[1].as_str() == label
    })
}

fn has_any_label(event: &Event) -> bool {
    event.tags.iter().any(|tag| {
        let tag_vec = tag.as_slice();
        !tag_vec.is_empty() && tag_vec[0].as_str() == "l"
    })
}

fn is_hashtree_event(event: &Event) -> bool {
    has_label(event, HASHTREE_LABEL) || !has_any_label(event)
}

fn event_identifier(event: &Event) -> Option<String> {
    event.tags.iter().find_map(|tag| {
        if let Some(TagStandard::Identifier(id)) = tag.as_standardized() {
            Some(id.clone())
        } else {
            None
        }
    })
}

fn pick_latest_event<'a, I>(events: I) -> Option<&'a Event>
where
    I: IntoIterator<Item = &'a Event>,
{
    // NIP-16/NIP-33 ordering: newest created_at, then larger event id.
    events
        .into_iter()
        .max_by_key(|event| (event.created_at, event.id))
}

fn next_replaceable_created_at(now: Timestamp, latest_existing: Option<Timestamp>) -> Timestamp {
    match latest_existing {
        Some(latest) if latest >= now => Timestamp::from_secs(latest.as_u64().saturating_add(1)),
        _ => now,
    }
}

fn is_newer_event(
    event: &Event,
    current_created_at: Timestamp,
    current_event_id: Option<EventId>,
) -> bool {
    if event.created_at > current_created_at {
        return true;
    }
    if event.created_at < current_created_at {
        return false;
    }
    match current_event_id {
        Some(current_id) => event.id > current_id,
        None => true,
    }
}

fn publish_succeeded(success_count: usize) -> bool {
    success_count > 0
}

async fn await_publish_result<F, T, E>(future: F) -> Result<T, ResolverError>
where
    F: Future<Output = Result<T, E>> + Send + 'static,
    T: Send + 'static,
    E: ToString + Send + 'static,
{
    let handle = tokio::spawn(future);
    match handle.await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(ResolverError::Network(error.to_string())),
        Err(error) if error.is_panic() => {
            Err(ResolverError::Other("Nostr publish task panicked".into()))
        }
        Err(error) => Err(ResolverError::Other(format!(
            "Nostr publish task failed: {}",
            error
        ))),
    }
}

fn upsert_latest_by_d_tag<'a>(entries_by_d_tag: &mut HashMap<String, &'a Event>, event: &'a Event) {
    let Some(d_tag) = event_identifier(event) else {
        return;
    };

    let should_replace = match entries_by_d_tag.get(&d_tag) {
        Some(existing) => is_newer_event(event, existing.created_at, Some(existing.id)),
        None => true,
    };

    if should_replace {
        entries_by_d_tag.insert(d_tag, event);
    }
}

fn parse_legacy_content(content: &str) -> Option<(String, Option<String>)> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        if let Some(hash) = value.get("hash").and_then(|v| v.as_str()) {
            let key = value
                .get("key")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            return Some((hash.to_string(), key));
        }
    }

    if trimmed.len() == 64 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        return Some((trimmed.to_string(), None));
    }

    None
}

/// Subscription state
struct Subscription {
    tx: mpsc::Sender<Option<Cid>>,
    current_cid: Option<Cid>,
    latest_created_at: Timestamp,
    latest_event_id: Option<EventId>,
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
    /// Tree names may contain slashes (e.g., "videos/Music")
    fn parse_key(key: &str) -> Result<(PublicKey, String), ResolverError> {
        // Use splitn(2) to split only on first '/' - tree names can contain '/'
        let parts: Vec<&str> = key.splitn(2, '/').collect();
        if parts.len() != 2 || parts[1].is_empty() {
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

    fn build_tree_filter(pubkey: PublicKey, tree_name: &str) -> Filter {
        Filter::new()
            .kind(Kind::Custom(HASHTREE_KIND))
            .author(pubkey)
            .custom_tag(
                SingleLetterTag::lowercase(Alphabet::D),
                vec![tree_name.to_string()],
            )
    }

    /// Extract Cid from event tags
    fn cid_from_event(&self, event: &Event) -> Option<Cid> {
        Self::cid_from_event_with_keys(event, self.config.secret_key.as_ref())
    }

    async fn fetch_events_from_relays(&self, filter: Filter) -> Result<Vec<Event>, ResolverError> {
        if self.config.relays.is_empty() {
            return Ok(Vec::new());
        }

        let relay_count = self.config.relays.len();
        let successful_relay_quorum = relay_count.clamp(1, DEFAULT_SUCCESSFUL_RELAY_QUORUM);
        let soft_timeout = self
            .config
            .resolve_timeout
            .min(DEFAULT_SOFT_RESOLVE_TIMEOUT);
        let mut join_set = JoinSet::new();
        for relay in self.config.relays.iter().cloned() {
            let client = self.client.clone();
            let filter = filter.clone();
            let timeout = self.config.resolve_timeout;
            join_set.spawn(async move {
                let result = client
                    .get_events_from(vec![relay.clone()], vec![filter], Some(timeout))
                    .await;
                (relay, result)
            });
        }

        let mut events_by_id: HashMap<EventId, Event> = HashMap::new();
        let mut successful_relays = 0usize;
        let mut errors = Vec::new();
        let mut soft_timeout_elapsed = false;
        let soft_timeout_sleep = tokio::time::sleep(soft_timeout);
        tokio::pin!(soft_timeout_sleep);

        while !join_set.is_empty() {
            tokio::select! {
                joined = join_set.join_next() => {
                    let Some(joined) = joined else {
                        break;
                    };
                    match joined {
                        Ok((relay, Ok(events))) => {
                            successful_relays += 1;
                            for event in events {
                                events_by_id.entry(event.id).or_insert(event);
                            }
                            let _ = relay;
                        }
                        Ok((relay, Err(err))) => {
                            errors.push(format!("{relay}: {err}"));
                        }
                        Err(err) => {
                            errors.push(format!("join error: {err}"));
                        }
                    }

                    if successful_relays >= successful_relay_quorum && !events_by_id.is_empty() {
                        return Ok(events_by_id.values().cloned().collect());
                    }
                    if soft_timeout_elapsed && successful_relays > 0 {
                        return Ok(events_by_id.values().cloned().collect());
                    }
                }
                _ = &mut soft_timeout_sleep, if !soft_timeout_elapsed => {
                    soft_timeout_elapsed = true;
                    if successful_relays > 0 {
                        return Ok(events_by_id.values().cloned().collect());
                    }
                }
            }
        }

        if successful_relays == 0 {
            let detail = if errors.is_empty() {
                "no relays succeeded".to_string()
            } else {
                errors.join("; ")
            };
            return Err(ResolverError::Network(format!(
                "Failed to get events from configured relays: {detail}"
            )));
        }

        Ok(events_by_id.into_values().collect())
    }

    async fn latest_existing_created_at(
        &self,
        pubkey: PublicKey,
        tree_name: &str,
    ) -> Result<Option<Timestamp>, ResolverError> {
        let events = self
            .fetch_events_from_relays(Self::build_tree_filter(pubkey, tree_name))
            .await?;
        Ok(pick_latest_event(events.iter().filter(|event| {
            event_identifier(event).as_deref() == Some(tree_name) && is_hashtree_event(event)
        }))
        .map(|event| event.created_at))
    }

    fn cid_from_event_with_keys(event: &Event, keys: Option<&Keys>) -> Option<Cid> {
        let mut hash_hex: Option<String> = None;
        let mut key_hex: Option<String> = None;
        let mut self_encrypted_key: Option<String> = None;

        for tag in event.tags.iter() {
            let tag_vec = tag.as_slice();
            if tag_vec.len() >= 2 {
                match tag_vec[0].as_str() {
                    "hash" => hash_hex = Some(tag_vec[1].clone()),
                    "key" => key_hex = Some(tag_vec[1].clone()),
                    TAG_SELF_ENCRYPTED_KEY => self_encrypted_key = Some(tag_vec[1].clone()),
                    _ => {}
                }
            }
        }

        if hash_hex.is_none() {
            if let Some((legacy_hash, legacy_key)) = parse_legacy_content(&event.content) {
                hash_hex = Some(legacy_hash);
                if key_hex.is_none() {
                    key_hex = legacy_key;
                }
            }
        }

        // hash is required
        let hash = from_hex(&hash_hex?).ok()?;

        // key is optional
        let mut key = key_hex.and_then(|k| {
            let bytes = hex::decode(&k).ok()?;
            if bytes.len() == 32 {
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&bytes);
                Some(arr)
            } else {
                None
            }
        });

        if key.is_none() {
            if let (Some(ciphertext), Some(keys)) = (self_encrypted_key, keys) {
                if let Ok(key_hex) =
                    nip44::decrypt(keys.secret_key(), &keys.public_key(), &ciphertext)
                {
                    if let Ok(bytes) = hex::decode(&key_hex) {
                        if bytes.len() == 32 {
                            let mut arr = [0u8; 32];
                            arr.copy_from_slice(&bytes);
                            key = Some(arr);
                        }
                    }
                }
            }
        }

        Some(Cid { hash, key })
    }

    /// Extract Cid from event with encrypted key decryption
    fn cid_from_event_shared(event: &Event, share_secret: &[u8; 32]) -> Option<Cid> {
        let mut hash_hex: Option<String> = None;
        let mut key_hex: Option<String> = None;
        let mut encrypted_key_hex: Option<String> = None;
        let mut encrypted_key_legacy_hex: Option<String> = None;

        for tag in event.tags.iter() {
            let tag_vec = tag.as_slice();
            if tag_vec.len() >= 2 {
                match tag_vec[0].as_str() {
                    "hash" => hash_hex = Some(tag_vec[1].clone()),
                    "key" => key_hex = Some(tag_vec[1].clone()),
                    TAG_ENCRYPTED_KEY => encrypted_key_hex = Some(tag_vec[1].clone()),
                    TAG_ENCRYPTED_KEY_LEGACY => encrypted_key_legacy_hex = Some(tag_vec[1].clone()),
                    _ => {}
                }
            }
        }

        if hash_hex.is_none() {
            if let Some((legacy_hash, _legacy_key)) = parse_legacy_content(&event.content) {
                hash_hex = Some(legacy_hash);
            }
        }

        let hash = from_hex(&hash_hex?).ok()?;

        let key = if let Some(k_hex) = key_hex {
            let bytes = hex::decode(&k_hex).ok()?;
            if bytes.len() == 32 {
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&bytes);
                Some(arr)
            } else {
                None
            }
        } else if let Some(ek_hex) = encrypted_key_hex {
            let masked = hex::decode(&ek_hex).ok()?;
            if masked.len() == 32 {
                let mut masked_arr = [0u8; 32];
                masked_arr.copy_from_slice(&masked);
                Some(xor_keys(&masked_arr, share_secret))
            } else {
                None
            }
        } else if let Some(ek_hex) = encrypted_key_legacy_hex {
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

    /// Publish a private root (selfEncryptedKey tag, NIP-44 to self)
    pub async fn publish_private(&self, key: &str, cid: &Cid) -> Result<bool, ResolverError> {
        let (pubkey, tree_name) = Self::parse_key(key)?;

        let keys = self
            .config
            .secret_key
            .as_ref()
            .ok_or(ResolverError::NotAuthorized)?;
        if pubkey != keys.public_key() {
            return Err(ResolverError::NotAuthorized);
        }

        let key_bytes = cid
            .key
            .ok_or_else(|| ResolverError::Other("Missing CHK key for private publish".into()))?;
        let key_hex = hex::encode(key_bytes);

        let encrypted = nip44::encrypt(
            keys.secret_key(),
            &keys.public_key(),
            key_hex,
            nip44::Version::V2,
        )
        .map_err(|e| ResolverError::Other(format!("NIP-44 encryption failed: {}", e)))?;

        let tags = vec![
            Tag::identifier(tree_name.clone()),
            Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::L)),
                vec![HASHTREE_LABEL],
            ),
            Tag::custom(TagKind::Custom(TAG_HASH.into()), vec![to_hex(&cid.hash)]),
            Tag::custom(
                TagKind::Custom(TAG_SELF_ENCRYPTED_KEY.into()),
                vec![encrypted],
            ),
        ];

        let created_at = next_replaceable_created_at(
            Timestamp::now(),
            self.latest_existing_created_at(pubkey, &tree_name)
                .await
                .ok()
                .flatten(),
        );
        let event = self
            .client
            .sign_event_builder(
                EventBuilder::new(Kind::Custom(HASHTREE_KIND), "", tags)
                    .custom_created_at(created_at),
            )
            .await
            .map_err(|e| ResolverError::Network(e.to_string()))?;

        let client = self.client.clone();
        let event_for_send = event.clone();
        let output =
            await_publish_result(async move { client.send_event(event_for_send).await }).await?;

        {
            let mut subs = self.subscriptions.write().await;
            if let Some(sub) = subs.get_mut(key) {
                sub.current_cid = Some(cid.clone());
                sub.latest_created_at = event.created_at;
                sub.latest_event_id = Some(event.id);
                let _ = sub.tx.send(Some(cid.clone())).await;
            }
        }

        Ok(publish_succeeded(output.success.len()))
    }
}

#[async_trait]
impl RootResolver for NostrRootResolver {
    async fn resolve(&self, key: &str) -> Result<Option<Cid>, ResolverError> {
        let (pubkey, tree_name) = Self::parse_key(key)?;

        let filter = Self::build_tree_filter(pubkey, &tree_name);

        // Fetch events from relays
        let events = self.fetch_events_from_relays(filter).await?;

        let latest_event = pick_latest_event(events.iter().filter(|event| {
            event_identifier(event).as_deref() == Some(&tree_name) && is_hashtree_event(event)
        }));

        // Extract Cid from event tags
        match latest_event {
            Some(event) => Ok(self.cid_from_event(event)),
            None => Ok(None),
        }
    }

    async fn resolve_shared(
        &self,
        key: &str,
        share_secret: &[u8; 32],
    ) -> Result<Option<Cid>, ResolverError> {
        let (pubkey, tree_name) = Self::parse_key(key)?;

        let filter = Self::build_tree_filter(pubkey, &tree_name);

        let events = self.fetch_events_from_relays(filter).await?;

        let latest_event = pick_latest_event(events.iter().filter(|event| {
            event_identifier(event).as_deref() == Some(&tree_name) && is_hashtree_event(event)
        }));

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
        let filter = Self::build_tree_filter(pubkey, &tree_name);

        // Store subscription state
        {
            let mut subs = self.subscriptions.write().await;
            subs.insert(
                key.to_string(),
                Subscription {
                    tx: tx.clone(),
                    current_cid: None,
                    latest_created_at: Timestamp::from(0),
                    latest_event_id: None,
                },
            );
        }

        // Subscribe to events
        let subscriptions = self.subscriptions.clone();
        let key_clone = key.to_string();
        let tree_name_clone = tree_name.clone();
        let secret_key = self.config.secret_key.clone();

        // Spawn subscription handler
        let client = self.client.clone();
        tokio::spawn(async move {
            let sub_id = client.subscribe(vec![filter], None).await;

            if sub_id.is_err() {
                return;
            }

            // Handle incoming events via notifications
            let mut notifications = client.notifications();

            while let Ok(notification) = notifications.recv().await {
                if let RelayPoolNotification::Event { event, .. } = notification {
                    if event_identifier(&event).as_deref() != Some(&tree_name_clone) {
                        continue;
                    }

                    if !is_hashtree_event(&event) {
                        continue;
                    }

                    let mut subs = subscriptions.write().await;
                    if let Some(sub) = subs.get_mut(&key_clone) {
                        let new_cid = NostrRootResolver::cid_from_event_with_keys(
                            &event,
                            secret_key.as_ref(),
                        );
                        if is_newer_event(&event, sub.latest_created_at, sub.latest_event_id) {
                            sub.latest_created_at = event.created_at;
                            sub.latest_event_id = Some(event.id);

                            if new_cid != sub.current_cid {
                                sub.current_cid = new_cid.clone();
                                if sub.tx.send(new_cid).await.is_err() {
                                    // Receiver dropped, clean up
                                    subs.remove(&key_clone);
                                    break;
                                }
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
            tags.push(Tag::custom(
                TagKind::Custom(TAG_KEY.into()),
                vec![hex::encode(key)],
            ));
        }

        let created_at = next_replaceable_created_at(
            Timestamp::now(),
            self.latest_existing_created_at(pubkey, &tree_name)
                .await
                .ok()
                .flatten(),
        );
        let event = self
            .client
            .sign_event_builder(
                EventBuilder::new(Kind::Custom(HASHTREE_KIND), "", tags)
                    .custom_created_at(created_at),
            )
            .await
            .map_err(|e| ResolverError::Network(e.to_string()))?;

        let client = self.client.clone();
        let event_for_send = event.clone();
        let output =
            await_publish_result(async move { client.send_event(event_for_send).await }).await?;

        {
            let mut subs = self.subscriptions.write().await;
            if let Some(sub) = subs.get_mut(key) {
                sub.current_cid = Some(cid.clone());
                sub.latest_created_at = event.created_at;
                sub.latest_event_id = Some(event.id);
                let _ = sub.tx.send(Some(cid.clone())).await;
            }
        }

        Ok(publish_succeeded(output.success.len()))
    }

    async fn publish_shared(
        &self,
        key: &str,
        cid: &Cid,
        share_secret: &[u8; 32],
    ) -> Result<bool, ResolverError> {
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

        // Mask the key with share_secret (XOR)
        if let Some(key) = cid.key {
            let masked = xor_keys(&key, share_secret);
            tags.push(Tag::custom(
                TagKind::Custom(TAG_ENCRYPTED_KEY.into()),
                vec![hex::encode(masked)],
            ));
        }

        let created_at = next_replaceable_created_at(
            Timestamp::now(),
            self.latest_existing_created_at(pubkey, &tree_name)
                .await
                .ok()
                .flatten(),
        );
        let event = self
            .client
            .sign_event_builder(
                EventBuilder::new(Kind::Custom(HASHTREE_KIND), "", tags)
                    .custom_created_at(created_at),
            )
            .await
            .map_err(|e| ResolverError::Network(e.to_string()))?;

        let client = self.client.clone();
        let event_for_send = event.clone();
        let output =
            await_publish_result(async move { client.send_event(event_for_send).await }).await?;

        {
            let mut subs = self.subscriptions.write().await;
            if let Some(sub) = subs.get_mut(key) {
                sub.current_cid = Some(cid.clone());
                sub.latest_created_at = event.created_at;
                sub.latest_event_id = Some(event.id);
                let _ = sub.tx.send(Some(cid.clone())).await;
            }
        }

        Ok(publish_succeeded(output.success.len()))
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
            .custom_tag(
                SingleLetterTag::lowercase(Alphabet::L),
                vec![HASHTREE_LABEL],
            );

        let events = self.fetch_events_from_relays(filter).await?;

        // Deduplicate by d-tag, keeping latest event
        let mut entries_by_d_tag: HashMap<String, &Event> = HashMap::new();

        for event in events.iter() {
            if !is_hashtree_event(event) {
                continue;
            }
            upsert_latest_by_d_tag(&mut entries_by_d_tag, event);
        }

        // Convert to entries
        let mut result = Vec::new();
        for (d_tag, event) in entries_by_d_tag {
            if let Some(cid) = self.cid_from_event(event) {
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
    use futures::{SinkExt, StreamExt};
    use std::collections::HashMap;
    use std::net::TcpListener;
    use std::sync::Mutex;
    use tokio::net::TcpStream;
    use tokio::sync::broadcast;
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    struct TestRelay {
        port: u16,
        events: Arc<Mutex<Vec<Value>>>,
        shutdown: broadcast::Sender<()>,
    }

    impl TestRelay {
        fn with_events(events: Vec<Event>) -> Self {
            Self::with_events_and_delay(events, Duration::ZERO)
        }

        fn with_events_and_delay(events: Vec<Event>, response_delay: Duration) -> Self {
            let stored_events = Arc::new(Mutex::new(
                events
                    .into_iter()
                    .map(|event| serde_json::to_value(event).expect("event to value"))
                    .collect::<Vec<_>>(),
            ));
            let (shutdown, _) = broadcast::channel(1);

            let std_listener = TcpListener::bind("127.0.0.1:0").expect("bind relay listener");
            let port = std_listener.local_addr().expect("relay local addr").port();
            std_listener
                .set_nonblocking(true)
                .expect("set relay listener nonblocking");

            let relay_events = Arc::clone(&stored_events);
            let shutdown_tx = shutdown.clone();

            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("build relay runtime");

                runtime.block_on(async move {
                    let listener =
                        tokio::net::TcpListener::from_std(std_listener).expect("tokio listener");
                    let mut shutdown_rx = shutdown_tx.subscribe();

                    loop {
                        tokio::select! {
                            _ = shutdown_rx.recv() => break,
                            accepted = listener.accept() => {
                                if let Ok((stream, _)) = accepted {
                                    let relay_events = Arc::clone(&relay_events);
                                    tokio::spawn(async move {
                                        handle_test_relay_connection(
                                            stream,
                                            relay_events,
                                            response_delay,
                                        )
                                        .await;
                                    });
                                }
                            }
                        }
                    }
                });
            });

            std::thread::sleep(Duration::from_millis(100));

            Self {
                port,
                events: stored_events,
                shutdown,
            }
        }

        fn url(&self) -> String {
            format!("ws://127.0.0.1:{}", self.port)
        }

        fn latest_tree_created_at(&self, tree_name: &str) -> Option<u64> {
            self.events
                .lock()
                .expect("relay events")
                .iter()
                .filter(|event| {
                    event.get("kind").and_then(Value::as_u64) == Some(HASHTREE_KIND as u64)
                        && event_tag_matches(event, "d", &[tree_name.to_string()])
                })
                .max_by_key(|event| {
                    (
                        event
                            .get("created_at")
                            .and_then(Value::as_u64)
                            .unwrap_or_default(),
                        event
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    )
                })
                .and_then(|event| event.get("created_at").and_then(Value::as_u64))
        }
    }

    impl Drop for TestRelay {
        fn drop(&mut self) {
            let _ = self.shutdown.send(());
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn event_tag_matches(event: &Value, name: &str, accepted: &[String]) -> bool {
        let Some(tags) = event.get("tags").and_then(Value::as_array) else {
            return false;
        };

        tags.iter().any(|tag| {
            let Some(arr) = tag.as_array() else {
                return false;
            };
            if arr.len() < 2 {
                return false;
            }
            let Some(tag_name) = arr.first().and_then(Value::as_str) else {
                return false;
            };
            if tag_name != name {
                return false;
            }
            let Some(tag_value) = arr.get(1).and_then(Value::as_str) else {
                return false;
            };
            accepted.iter().any(|value| value == tag_value)
        })
    }

    fn event_matches_filter(event: &Value, filter: &Value) -> bool {
        let Some(filter_obj) = filter.as_object() else {
            return true;
        };

        if let Some(kinds) = filter_obj.get("kinds").and_then(Value::as_array) {
            let event_kind = event
                .get("kind")
                .and_then(Value::as_i64)
                .unwrap_or_default();
            if !kinds
                .iter()
                .any(|kind| kind.as_i64().is_some_and(|value| value == event_kind))
            {
                return false;
            }
        }

        if let Some(authors) = filter_obj.get("authors").and_then(Value::as_array) {
            let event_author = event
                .get("pubkey")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !authors
                .iter()
                .filter_map(Value::as_str)
                .any(|author| author == event_author)
            {
                return false;
            }
        }

        if let Some(d_values) = filter_obj.get("#d").and_then(Value::as_array) {
            let accepted: Vec<String> = d_values
                .iter()
                .filter_map(|value| value.as_str().map(ToOwned::to_owned))
                .collect();
            if !accepted.is_empty() && !event_tag_matches(event, "d", &accepted) {
                return false;
            }
        }

        if let Some(l_values) = filter_obj.get("#l").and_then(Value::as_array) {
            let accepted: Vec<String> = l_values
                .iter()
                .filter_map(|value| value.as_str().map(ToOwned::to_owned))
                .collect();
            if !accepted.is_empty() && !event_tag_matches(event, "l", &accepted) {
                return false;
            }
        }

        true
    }

    #[tokio::test]
    async fn publish_task_panic_is_reported_as_error() {
        let error = await_publish_result(async move {
            panic!("boom");
            #[allow(unreachable_code)]
            Ok::<(), &'static str>(())
        })
        .await
        .expect_err("panic should be converted to resolver error");

        match error {
            ResolverError::Other(message) => {
                assert!(message.contains("panicked"));
            }
            other => panic!("expected ResolverError::Other, got {other:?}"),
        }
    }

    async fn handle_test_relay_connection(
        stream: TcpStream,
        events: Arc<Mutex<Vec<Value>>>,
        response_delay: Duration,
    ) {
        let ws_stream = match accept_async(stream).await {
            Ok(ws) => ws,
            Err(_) => return,
        };
        let (mut write, mut read) = ws_stream.split();

        while let Some(message) = read.next().await {
            let text = match message {
                Ok(Message::Text(text)) => text,
                Ok(Message::Ping(data)) => {
                    let _ = write.send(Message::Pong(data)).await;
                    continue;
                }
                Ok(Message::Close(_)) => break,
                _ => continue,
            };

            let parsed: Vec<Value> = match serde_json::from_str(&text) {
                Ok(value) => value,
                Err(_) => continue,
            };

            let Some(message_type) = parsed.first().and_then(Value::as_str) else {
                continue;
            };

            match message_type {
                "REQ" => {
                    let Some(sub_id) = parsed.get(1).and_then(Value::as_str) else {
                        continue;
                    };
                    let filters: Vec<Value> = parsed.iter().skip(2).cloned().collect();
                    let snapshot = events.lock().expect("relay events lock").clone();

                    if !response_delay.is_zero() {
                        tokio::time::sleep(response_delay).await;
                    }

                    for event in snapshot {
                        let matched = if filters.is_empty() {
                            true
                        } else {
                            filters
                                .iter()
                                .any(|filter| event_matches_filter(&event, filter))
                        };
                        if matched {
                            let message = serde_json::json!(["EVENT", sub_id, event]);
                            let _ = write.send(Message::Text(message.to_string())).await;
                        }
                    }

                    let eose = serde_json::json!(["EOSE", sub_id]);
                    let _ = write.send(Message::Text(eose.to_string())).await;
                }
                "EVENT" => {
                    let Some(event) = parsed.get(1).cloned() else {
                        continue;
                    };
                    let Some(id) = event
                        .get("id")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                    else {
                        continue;
                    };
                    events.lock().expect("relay events lock").push(event);
                    let ok = serde_json::json!(["OK", id, true, ""]);
                    let _ = write.send(Message::Text(ok.to_string())).await;
                }
                "CLOSE" => {}
                _ => {}
            }
        }
    }

    fn build_hashtree_event(
        keys: &Keys,
        tree_name: &str,
        created_at: u64,
        hash: &str,
        content: &str,
    ) -> Event {
        let tags = vec![
            Tag::identifier(tree_name.to_string()),
            Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::L)),
                vec![HASHTREE_LABEL.to_string()],
            ),
            Tag::custom(TagKind::Custom(TAG_HASH.into()), vec![hash.to_string()]),
        ];
        EventBuilder::new(Kind::Custom(HASHTREE_KIND), content, tags)
            .custom_created_at(Timestamp::from_secs(created_at))
            .to_event(keys)
            .unwrap()
    }

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

    #[test]
    fn test_resolve_tie_breaks_with_event_id() {
        let keys = Keys::generate();
        let created_at = 1_700_000_000;

        let event_a = build_hashtree_event(
            &keys,
            "tree",
            created_at,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "a",
        );
        let event_b = build_hashtree_event(
            &keys,
            "tree",
            created_at,
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "b",
        );

        let picked = pick_latest_event([&event_a, &event_b]).unwrap();
        let expected = if event_a.id > event_b.id {
            event_a.id
        } else {
            event_b.id
        };
        assert_eq!(picked.id, expected);
    }

    #[test]
    fn test_resolve_shared_tie_breaks_with_event_id() {
        let keys = Keys::generate();
        let created_at = 1_700_000_000;

        let event_old = build_hashtree_event(
            &keys,
            "tree",
            created_at,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "older",
        );
        let event_new = build_hashtree_event(
            &keys,
            "tree",
            created_at,
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "newer",
        );

        let mut events = vec![&event_old, &event_new];
        events.sort_by_key(|e| e.id);
        let picked = pick_latest_event(events).unwrap();
        assert_eq!(picked.id, std::cmp::max(event_old.id, event_new.id));
    }

    #[test]
    fn test_subscribe_tie_breaks_with_event_id() {
        let keys = Keys::generate();
        let created_at = Timestamp::from_secs(1_700_000_000);

        let current = build_hashtree_event(
            &keys,
            "tree",
            created_at.as_u64(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "current",
        );
        let candidate = build_hashtree_event(
            &keys,
            "tree",
            created_at.as_u64(),
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "candidate",
        );

        let should = is_newer_event(&candidate, current.created_at, Some(current.id));
        assert_eq!(should, candidate.id > current.id);
    }

    #[test]
    fn test_list_dedupe_tie_breaks_with_event_id() {
        let keys = Keys::generate();
        let created_at = 1_700_000_000;

        let first = build_hashtree_event(
            &keys,
            "videos",
            created_at,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "first",
        );
        let second = build_hashtree_event(
            &keys,
            "videos",
            created_at,
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "second",
        );

        let mut by_tag: HashMap<String, &Event> = HashMap::new();
        upsert_latest_by_d_tag(&mut by_tag, &first);
        upsert_latest_by_d_tag(&mut by_tag, &second);

        let selected = by_tag.get("videos").unwrap();
        let expected = if first.id > second.id {
            first.id
        } else {
            second.id
        };
        assert_eq!(selected.id, expected);
    }

    #[test]
    fn test_publish_bumps_replaceable_timestamp_past_existing_event() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("build test runtime");

        runtime.block_on(async {
            let keys = Keys::generate();
            let npub = keys.public_key().to_bech32().expect("npub");
            let key = format!("{npub}/tree");
            let stale_created_at = Timestamp::now().as_u64().saturating_add(30);
            let stale = build_hashtree_event(
                &keys,
                "tree",
                stale_created_at,
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "stale",
            );
            let relay = TestRelay::with_events(vec![stale]);
            let resolver = NostrRootResolver::new(NostrResolverConfig {
                relays: vec![relay.url()],
                resolve_timeout: Duration::from_secs(2),
                secret_key: Some(keys.clone()),
            })
            .await
            .expect("resolver");

            let cid = Cid {
                hash: [0x22; 32],
                key: None,
            };
            let published = resolver.publish(&key, &cid).await.expect("publish");
            assert!(published);

            let resolved = resolver
                .resolve(&key)
                .await
                .expect("resolve")
                .expect("published cid");
            assert_eq!(resolved, cid);
            assert!(
                relay
                    .latest_tree_created_at("tree")
                    .is_some_and(|created_at| created_at > stale_created_at),
                "publish should advance created_at beyond the existing replaceable event"
            );
            resolver.stop().await.expect("stop resolver");
        });
    }

    #[test]
    fn test_resolve_succeeds_when_some_relays_fail() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("build test runtime");

        runtime.block_on(async {
            let keys = Keys::generate();
            let tree_name = "video";
            let hash = "c94a1b5bde1d7a32b96df53086a27f4385a631e1e39a5aac97589d20c49c5022";
            let event = build_hashtree_event(&keys, tree_name, 1_774_517_172, hash, "");
            let good_relay = TestRelay::with_events(vec![event]);
            let bad_relay = "ws://127.0.0.1:9".to_string();

            let resolver = NostrRootResolver::new(NostrResolverConfig {
                relays: vec![bad_relay, good_relay.url()],
                resolve_timeout: Duration::from_millis(400),
                secret_key: None,
            })
            .await
            .expect("create resolver");

            let key = format!("{}/{}", keys.public_key().to_bech32().unwrap(), tree_name);
            let resolved = resolver
                .resolve(&key)
                .await
                .expect("resolve via healthy relay");

            assert_eq!(
                resolved,
                Some(Cid {
                    hash: from_hex(hash).unwrap(),
                    key: None,
                })
            );
        });
    }

    #[test]
    fn test_resolve_returns_after_quick_quorum() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("build test runtime");

        runtime.block_on(async {
            let keys = Keys::generate();
            let tree_name = "video";
            let hash = "c94a1b5bde1d7a32b96df53086a27f4385a631e1e39a5aac97589d20c49c5022";
            let event = build_hashtree_event(&keys, tree_name, 1_774_517_172, hash, "");
            let quick_hit = TestRelay::with_events(vec![event]);
            let quick_empty = TestRelay::with_events(Vec::new());
            let slow_empty =
                TestRelay::with_events_and_delay(Vec::new(), Duration::from_millis(1200));

            let resolver = NostrRootResolver::new(NostrResolverConfig {
                relays: vec![quick_hit.url(), quick_empty.url(), slow_empty.url()],
                resolve_timeout: Duration::from_millis(1500),
                secret_key: None,
            })
            .await
            .expect("create resolver");

            let key = format!("{}/{}", keys.public_key().to_bech32().unwrap(), tree_name);
            let started = std::time::Instant::now();
            let resolved = resolver
                .resolve(&key)
                .await
                .expect("resolve from quick quorum");

            assert_eq!(
                resolved,
                Some(Cid {
                    hash: from_hex(hash).unwrap(),
                    key: None,
                })
            );
            assert!(
                started.elapsed() < Duration::from_millis(800),
                "resolve waited too long: {:?}",
                started.elapsed()
            );
        });
    }

    #[test]
    fn test_resolve_uses_soft_deadline_for_partial_results() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("build test runtime");

        runtime.block_on(async {
            let keys = Keys::generate();
            let tree_name = "video";
            let hash = "c94a1b5bde1d7a32b96df53086a27f4385a631e1e39a5aac97589d20c49c5022";
            let event = build_hashtree_event(&keys, tree_name, 1_774_517_172, hash, "");
            let quick_hit = TestRelay::with_events(vec![event]);
            let slow_empty_a = TestRelay::with_events_and_delay(Vec::new(), Duration::from_secs(5));
            let slow_empty_b = TestRelay::with_events_and_delay(Vec::new(), Duration::from_secs(5));

            let resolver = NostrRootResolver::new(NostrResolverConfig {
                relays: vec![quick_hit.url(), slow_empty_a.url(), slow_empty_b.url()],
                resolve_timeout: Duration::from_secs(6),
                secret_key: None,
            })
            .await
            .expect("create resolver");

            let key = format!("{}/{}", keys.public_key().to_bech32().unwrap(), tree_name);
            let started = std::time::Instant::now();
            let resolved = resolver
                .resolve(&key)
                .await
                .expect("resolve from partial results");

            assert_eq!(
                resolved,
                Some(Cid {
                    hash: from_hex(hash).unwrap(),
                    key: None,
                })
            );
            assert!(
                started.elapsed() < Duration::from_secs(4),
                "resolve missed the soft deadline: {:?}",
                started.elapsed()
            );
        });
    }

    #[test]
    fn publish_succeeded_requires_at_least_one_successful_relay() {
        assert!(!publish_succeeded(0));
        assert!(publish_succeeded(1));
        assert!(publish_succeeded(2));
    }
}
