//! Nostr client for publishing and fetching git repository references
//!
//! Uses kind 30078 (application-specific data) with hashtree structure:
//! {
//!   "kind": 30078,
//!   "tags": [
//!     ["d", "<repo-name>"],
//!     ["l", "hashtree"]
//!   ],
//!   "content": "<merkle-root-hash>"
//! }
//!
//! The merkle tree contains:
//!   root/
//!     refs/heads/main -> <sha>
//!     refs/tags/v1.0 -> <sha>
//!     objects/<sha1> -> data
//!     objects/<sha2> -> data
//!
//! ## Identity file format
//!
//! The secrets file (`~/.hashtree/keys`) supports multiple signing keys with optional
//! petnames:
//! ```text
//! nsec1... default
//! nsec1... work
//! nsec1... personal
//! ```
//!
//! Or hex format:
//! ```text
//! <64-char-hex> default
//! <64-char-hex> work
//! ```
//!
//! Public read-only aliases can be stored in `~/.hashtree/aliases`:
//! ```text
//! npub1... sirius
//! npub1... coworker
//! ```
//!
//! For compatibility, public aliases in `~/.hashtree/keys` are also accepted.
//!
//! Then use: `htree://work/myrepo` or `htree://npub1.../myrepo`

mod identity;
mod repo_metadata;

use crate::runtime::block_on_result;
use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use hashtree_blossom::BlossomClient;
use hashtree_core::{decode_tree_node, decrypt_chk, Cid, LinkType, TreeNode};
use nostr_sdk::prelude::*;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};
use tracing::{debug, info, warn};

pub use identity::{
    load_key_lists, load_keys, resolve_identity, resolve_self_identity, StoredKey, StoredKeyLists,
};
use repo_metadata::{
    append_repo_discovery_labels, build_git_repo_list_filter, build_repo_announcement_filter,
    build_repo_event_filter, extract_repo_announcement_euc, latest_repo_announcement_created_at,
    latest_repo_event_created_at, latest_trusted_pr_status_kinds, list_git_repo_announcements,
    next_replaceable_created_at, pick_latest_event, pick_latest_repo_event,
    validate_repo_publish_relays,
};

/// Event kind for application-specific data (NIP-78)
pub const KIND_APP_DATA: u16 = 30078;

/// NIP-34 event kinds
pub const KIND_PULL_REQUEST: u16 = 1618;
pub const KIND_STATUS_OPEN: u16 = 1630;
pub const KIND_STATUS_APPLIED: u16 = 1631;
pub const KIND_STATUS_CLOSED: u16 = 1632;
pub const KIND_STATUS_DRAFT: u16 = 1633;
pub const KIND_REPO_ANNOUNCEMENT: u16 = 30617;

/// Label for hashtree events
pub const LABEL_HASHTREE: &str = "hashtree";
pub const LABEL_GIT: &str = "git";

/// Pull request status derived from trusted NIP-34 status events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullRequestState {
    Open,
    Applied,
    Closed,
    Draft,
}

impl PullRequestState {
    pub fn as_str(self) -> &'static str {
        match self {
            PullRequestState::Open => "open",
            PullRequestState::Applied => "applied",
            PullRequestState::Closed => "closed",
            PullRequestState::Draft => "draft",
        }
    }

    fn from_status_kind(status_kind: u16) -> Option<Self> {
        match status_kind {
            KIND_STATUS_OPEN => Some(PullRequestState::Open),
            KIND_STATUS_APPLIED => Some(PullRequestState::Applied),
            KIND_STATUS_CLOSED => Some(PullRequestState::Closed),
            KIND_STATUS_DRAFT => Some(PullRequestState::Draft),
            _ => None,
        }
    }

    fn from_latest_status_kind(status_kind: Option<u16>) -> Self {
        status_kind
            .and_then(Self::from_status_kind)
            .unwrap_or(PullRequestState::Open)
    }
}

/// Filter used when listing PRs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PullRequestStateFilter {
    #[default]
    Open,
    Applied,
    Closed,
    Draft,
    All,
}

impl PullRequestStateFilter {
    pub fn as_str(self) -> &'static str {
        match self {
            PullRequestStateFilter::Open => "open",
            PullRequestStateFilter::Applied => "applied",
            PullRequestStateFilter::Closed => "closed",
            PullRequestStateFilter::Draft => "draft",
            PullRequestStateFilter::All => "all",
        }
    }

    fn includes(self, state: PullRequestState) -> bool {
        match self {
            PullRequestStateFilter::All => true,
            PullRequestStateFilter::Open => state == PullRequestState::Open,
            PullRequestStateFilter::Applied => state == PullRequestState::Applied,
            PullRequestStateFilter::Closed => state == PullRequestState::Closed,
            PullRequestStateFilter::Draft => state == PullRequestState::Draft,
        }
    }
}

/// PR metadata used by listing/filtering consumers.
#[derive(Debug, Clone)]
pub struct PullRequestListItem {
    pub event_id: String,
    pub author_pubkey: String,
    pub state: PullRequestState,
    pub subject: Option<String>,
    pub commit_tip: Option<String>,
    pub branch: Option<String>,
    pub target_branch: Option<String>,
    pub created_at: u64,
}

async fn fetch_events_via_raw_relay_query(
    relays: &[String],
    filter: Filter,
    timeout: Duration,
) -> Vec<Event> {
    let request_json = ClientMessage::req(SubscriptionId::generate(), vec![filter]).as_json();
    let mut events_by_id = HashMap::<String, Event>::new();

    for relay_url in relays {
        let relay_events = match tokio::time::timeout(timeout, async {
            let (mut ws, _) = connect_async(relay_url).await?;
            ws.send(WsMessage::Text(request_json.clone())).await?;

            let mut relay_events = Vec::new();
            while let Some(message) = ws.next().await {
                let message = message?;
                let WsMessage::Text(text) = message else {
                    continue;
                };

                match RelayMessage::from_json(text.as_str()) {
                    Ok(RelayMessage::Event { event, .. }) => relay_events.push(event.into_owned()),
                    Ok(RelayMessage::EndOfStoredEvents(_)) => break,
                    Ok(RelayMessage::Closed { message, .. }) => {
                        debug!("Raw relay PR query closed by {}: {}", relay_url, message);
                        break;
                    }
                    Ok(_) => {}
                    Err(err) => {
                        debug!(
                            "Failed to parse raw relay response from {}: {}",
                            relay_url, err
                        );
                    }
                }
            }

            let _ = ws.close(None).await;
            Ok::<Vec<Event>, anyhow::Error>(relay_events)
        })
        .await
        {
            Ok(Ok(events)) => events,
            Ok(Err(err)) => {
                debug!("Raw relay PR query failed for {}: {}", relay_url, err);
                continue;
            }
            Err(_) => {
                debug!("Raw relay PR query timed out for {}", relay_url);
                continue;
            }
        };

        for event in relay_events {
            events_by_id.insert(event.id.to_hex(), event);
        }
    }

    events_by_id.into_values().collect()
}

async fn connected_relay_count(client: &Client) -> (usize, usize) {
    let relays = client.relays().await;
    let total = relays.len();
    let mut connected = 0;
    for relay in relays.values() {
        if relay.is_connected() {
            connected += 1;
        }
    }
    (connected, total)
}

async fn wait_for_any_connected_relay(client: &Client, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    loop {
        if connected_relay_count(client).await.0 > 0 {
            return true;
        }
        if start.elapsed() > timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

type FetchedRefs = (HashMap<String, String>, Option<String>, Option<[u8; 32]>);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum RootResolveSource {
    #[default]
    Relay,
    LocalDaemon,
}

#[derive(Debug, Clone)]
struct ResolvedRoot {
    root_hash: Option<String>,
    encryption_key: Option<[u8; 32]>,
    source: Option<RootResolveSource>,
}
use hashtree_config::Config;

/// Result of publishing to relays
#[derive(Debug, Clone)]
pub struct RelayResult {
    /// Relays that were configured
    #[allow(dead_code)]
    pub configured: Vec<String>,
    /// Relays that connected
    pub connected: Vec<String>,
    /// Relays that failed to connect
    pub failed: Vec<String>,
}

/// Optional NIP-34 metadata to publish alongside the htree root event.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoAnnouncementOptions {
    /// NIP-34 earliest unique commit. Usually the root commit; forks should keep the source euc.
    pub earliest_unique_commit: Option<String>,
    /// Mark the repo announcement as a personal fork (`["t", "personal-fork"]`).
    pub personal_fork: bool,
    /// Iris/htree extension for showing the exact htree source URL.
    pub forked_from: Option<String>,
}

/// Result of uploading to blossom servers
#[derive(Debug, Clone)]
pub struct BlossomResult {
    /// Servers that were configured
    #[allow(dead_code)]
    pub configured: Vec<String>,
    /// Servers that accepted uploads
    pub succeeded: Vec<String>,
    /// Servers that failed
    pub failed: Vec<String>,
    /// Whether the repo tree is complete in the local hashtree store.
    pub local_complete: bool,
    /// Whether remote replication was incomplete.
    pub degraded: bool,
}

/// Nostr client for git operations
pub struct NostrClient {
    pubkey: String,
    /// nostr-sdk Keys for signing
    keys: Option<Keys>,
    relays: Vec<String>,
    blossom: BlossomClient,
    /// Cached refs from remote
    cached_refs: HashMap<String, HashMap<String, String>>,
    /// Cached root hashes (hashtree SHA256)
    cached_root_hash: HashMap<String, String>,
    /// Cached encryption keys
    cached_encryption_key: HashMap<String, [u8; 32]>,
    /// Source of the cached root hash, used to retry tentative daemon roots via relays.
    cached_root_source: HashMap<String, RootResolveSource>,
    /// URL secret for link-visible repos (#k=<hex>)
    /// If set, encryption keys from nostr are XOR-masked and need unmasking
    url_secret: Option<[u8; 32]>,
    /// Whether this is a private (author-only) repo using NIP-44 encryption
    is_private: bool,
    /// Local htree daemon URL for peer-assisted root discovery
    local_daemon_url: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct RootEventData {
    root_hash: String,
    encryption_key: Option<[u8; 32]>,
    key_tag_name: Option<String>,
    self_encrypted_ciphertext: Option<String>,
    source: RootResolveSource,
    daemon_source: Option<String>,
    event_created_at: Option<u64>,
    event_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DaemonResolveResponse {
    hash: Option<String>,
    #[serde(default)]
    cid: Option<String>,
    #[serde(default, rename = "key_tag")]
    key: Option<String>,
    #[serde(default, rename = "encryptedKey")]
    encrypted_key: Option<String>,
    #[serde(default, rename = "selfEncryptedKey")]
    self_encrypted_key: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    created_at: Option<u64>,
    #[serde(default)]
    event_id: Option<String>,
}

impl NostrClient {
    /// Create a new client with pubkey, optional secret key, url secret, is_private flag, and config
    pub fn new(
        pubkey: &str,
        secret_key: Option<String>,
        url_secret: Option<[u8; 32]>,
        is_private: bool,
        config: &Config,
    ) -> Result<Self> {
        // Ensure rustls has a process-wide crypto provider even when used as a library (tests).
        let _ = rustls::crypto::ring::default_provider().install_default();

        // Use provided secret, or try environment variable
        let secret_key = secret_key.or_else(|| std::env::var("NOSTR_SECRET_KEY").ok());

        // Create nostr-sdk Keys if we have a secret
        let keys = if let Some(ref secret_hex) = secret_key {
            let secret_bytes = hex::decode(secret_hex).context("Invalid secret key hex")?;
            let secret = nostr::SecretKey::from_slice(&secret_bytes)
                .map_err(|e| anyhow::anyhow!("Invalid secret key: {}", e))?;
            Some(Keys::new(secret))
        } else {
            None
        };

        // Create BlossomClient (needs keys for upload auth) from the resolved
        // config passed by the helper instead of reloading defaults from disk.
        let blossom_keys = keys.clone().unwrap_or_else(Keys::generate);
        let mut read_servers = config.blossom.all_read_servers();
        if let Some(local_url) =
            hashtree_config::detect_local_daemon_url(Some(config.server.bind_address.as_str()))
        {
            if !read_servers.iter().any(|server| server == &local_url) {
                read_servers.insert(0, local_url);
            }
        }
        let blossom = BlossomClient::new_empty(blossom_keys)
            .with_read_servers(read_servers)
            .with_write_servers(config.blossom.all_write_servers())
            .with_timeout(Duration::from_secs(120));

        tracing::info!(
            "BlossomClient created with read_servers: {:?}, write_servers: {:?}",
            blossom.read_servers(),
            blossom.write_servers()
        );

        let relays = hashtree_config::resolve_relays(
            &config.nostr.relays,
            Some(config.server.bind_address.as_str()),
        );
        let local_daemon_url =
            hashtree_config::detect_local_daemon_url(Some(config.server.bind_address.as_str()))
                .or_else(|| {
                    config
                        .blossom
                        .read_servers
                        .iter()
                        .find(|url| {
                            url.starts_with("http://127.0.0.1:")
                                || url.starts_with("http://localhost:")
                        })
                        .cloned()
                });

        Ok(Self {
            pubkey: pubkey.to_string(),
            keys,
            relays,
            blossom,
            cached_refs: HashMap::new(),
            cached_root_hash: HashMap::new(),
            cached_encryption_key: HashMap::new(),
            cached_root_source: HashMap::new(),
            url_secret,
            is_private,
            local_daemon_url,
        })
    }

    fn format_repo_author(pubkey_hex: &str) -> String {
        PublicKey::from_hex(pubkey_hex)
            .ok()
            .and_then(|pk| pk.to_bech32().ok())
            .unwrap_or_else(|| pubkey_hex.to_string())
    }

    fn daemon_pubkey_identifier(&self) -> String {
        Self::format_repo_author(&self.pubkey)
    }

    /// Check if we can sign (have secret key for this pubkey)
    #[allow(dead_code)]
    pub fn can_sign(&self) -> bool {
        self.keys.is_some()
    }

    pub fn list_repos(&self) -> Result<Vec<String>> {
        block_on_result(self.list_repos_async())
    }

    pub async fn list_repos_async(&self) -> Result<Vec<String>> {
        let client = Client::default();

        for relay in &self.relays {
            if let Err(e) = client.add_relay(relay).await {
                warn!("Failed to add relay {}: {}", relay, e);
            }
        }
        client.connect().await;

        if !wait_for_any_connected_relay(&client, Duration::from_secs(2)).await {
            let _ = client.disconnect().await;
            return Err(anyhow::anyhow!(
                "Failed to connect to any relay while listing repos"
            ));
        }

        let author = PublicKey::from_hex(&self.pubkey)
            .map_err(|e| anyhow::anyhow!("Invalid pubkey: {}", e))?;
        let filter = build_git_repo_list_filter(author);

        let events = match tokio::time::timeout(
            Duration::from_secs(3),
            client.fetch_events(filter, Duration::from_secs(3)),
        )
        .await
        {
            Ok(Ok(events)) => events,
            Ok(Err(e)) => {
                let _ = client.disconnect().await;
                return Err(anyhow::anyhow!(
                    "Failed to fetch git repo events from relays: {}",
                    e
                ));
            }
            Err(_) => {
                let _ = client.disconnect().await;
                return Err(anyhow::anyhow!(
                    "Timed out fetching git repo events from relays"
                ));
            }
        };

        let _ = client.disconnect().await;
        let events = events.to_vec();

        Ok(list_git_repo_announcements(&events)
            .into_iter()
            .map(|repo| repo.repo_name)
            .collect())
    }

    /// Fetch refs for a repository from nostr
    /// Returns refs parsed from the hashtree at the root hash
    pub fn fetch_refs(&mut self, repo_name: &str) -> Result<HashMap<String, String>> {
        let (refs, _, _) = self.fetch_refs_with_timeout(repo_name, 10)?;
        Ok(refs)
    }

    /// Fetch refs with a quick timeout (3s) for push operations
    /// Returns empty if timeout - allows push to proceed
    #[allow(dead_code)]
    pub fn fetch_refs_quick(&mut self, repo_name: &str) -> Result<HashMap<String, String>> {
        let (refs, _, _) = self.fetch_refs_with_timeout(repo_name, 3)?;
        Ok(refs)
    }

    /// Fetch refs and root hash info from nostr
    /// Returns (refs, root_hash, encryption_key)
    #[allow(dead_code)]
    pub fn fetch_refs_with_root(&mut self, repo_name: &str) -> Result<FetchedRefs> {
        self.fetch_refs_with_timeout(repo_name, 10)
    }

    pub(crate) fn refetch_refs_without_local_daemon(
        &mut self,
        repo_name: &str,
        timeout_secs: u64,
    ) -> Result<FetchedRefs> {
        self.clear_cached_remote_state(repo_name);
        self.fetch_refs_with_timeout_uncached(repo_name, timeout_secs, false)
    }

    fn clear_cached_remote_state(&mut self, repo_name: &str) {
        self.cached_refs.remove(repo_name);
        self.cached_root_hash.remove(repo_name);
        self.cached_encryption_key.remove(repo_name);
        self.cached_root_source.remove(repo_name);
    }

    fn cache_resolved_root(&mut self, repo_name: &str, resolved: &ResolvedRoot) {
        if let Some(ref root) = resolved.root_hash {
            self.cached_root_hash
                .insert(repo_name.to_string(), root.clone());
        } else {
            self.cached_root_hash.remove(repo_name);
        }

        if let Some(key) = resolved.encryption_key {
            self.cached_encryption_key
                .insert(repo_name.to_string(), key);
        } else {
            self.cached_encryption_key.remove(repo_name);
        }

        if let Some(source) = resolved.source {
            self.cached_root_source
                .insert(repo_name.to_string(), source);
        } else {
            self.cached_root_source.remove(repo_name);
        }
    }

    /// Fetch refs with configurable timeout
    fn fetch_refs_with_timeout(
        &mut self,
        repo_name: &str,
        timeout_secs: u64,
    ) -> Result<FetchedRefs> {
        debug!(
            "Fetching refs for {} from {} (timeout {}s)",
            repo_name, self.pubkey, timeout_secs
        );

        // Check cache first
        if let Some(refs) = self.cached_refs.get(repo_name) {
            let root = self.cached_root_hash.get(repo_name).cloned();
            let key = self.cached_encryption_key.get(repo_name).cloned();
            return Ok((refs.clone(), root, key));
        }

        self.fetch_refs_with_timeout_uncached(repo_name, timeout_secs, true)
    }

    fn fetch_refs_with_timeout_uncached(
        &mut self,
        repo_name: &str,
        timeout_secs: u64,
        allow_local_daemon: bool,
    ) -> Result<FetchedRefs> {
        let resolved = block_on_result(self.resolve_root_async_with_timeout(
            repo_name,
            timeout_secs,
            allow_local_daemon,
        ))?;

        match self.fetch_refs_for_resolved_root(repo_name, &resolved) {
            Ok(fetched) => Ok(fetched),
            Err(err)
                if resolved.source == Some(RootResolveSource::LocalDaemon)
                    && allow_local_daemon =>
            {
                warn!(
                    "Local daemon root for {} could not be read as a valid git tree: {}. Retrying via relays.",
                    repo_name, err
                );
                self.clear_cached_remote_state(repo_name);
                self.fetch_refs_with_timeout_uncached(repo_name, timeout_secs, false)
            }
            Err(err) => Err(err),
        }
    }

    fn fetch_refs_for_resolved_root(
        &mut self,
        repo_name: &str,
        resolved: &ResolvedRoot,
    ) -> Result<FetchedRefs> {
        if resolved.source != Some(RootResolveSource::LocalDaemon) {
            self.cache_resolved_root(repo_name, resolved);
        }

        let refs = if let Some(ref root) = resolved.root_hash {
            block_on_result(self.fetch_refs_from_hashtree(root, resolved.encryption_key.as_ref()))?
        } else {
            HashMap::new()
        };

        self.cache_resolved_root(repo_name, resolved);
        self.cached_refs.insert(repo_name.to_string(), refs.clone());
        Ok((refs, resolved.root_hash.clone(), resolved.encryption_key))
    }

    fn parse_root_event_data_from_event(event: &Event) -> RootEventData {
        let root_hash = event
            .tags
            .iter()
            .find(|t| t.as_slice().len() >= 2 && t.as_slice()[0].as_str() == "hash")
            .map(|t| t.as_slice()[1].to_string())
            .unwrap_or_else(|| event.content.to_string());

        let (encryption_key, key_tag_name, self_encrypted_ciphertext) = event
            .tags
            .iter()
            .find_map(|t| {
                let slice = t.as_slice();
                if slice.len() < 2 {
                    return None;
                }
                let tag_name = slice[0].as_str();
                let tag_value = slice[1].to_string();
                if tag_name == "selfEncryptedKey" {
                    return Some((None, Some(tag_name.to_string()), Some(tag_value)));
                }
                if tag_name == "key" || tag_name == "encryptedKey" {
                    if let Ok(bytes) = hex::decode(&tag_value) {
                        if bytes.len() == 32 {
                            let mut key = [0u8; 32];
                            key.copy_from_slice(&bytes);
                            return Some((Some(key), Some(tag_name.to_string()), None));
                        }
                    }
                }
                None
            })
            .unwrap_or((None, None, None));

        RootEventData {
            root_hash,
            encryption_key,
            key_tag_name,
            self_encrypted_ciphertext,
            source: RootResolveSource::Relay,
            daemon_source: None,
            event_created_at: Some(event.created_at.as_secs()),
            event_id: Some(event.id.to_hex()),
        }
    }

    fn parse_daemon_response_to_root_data(
        response: DaemonResolveResponse,
    ) -> Option<RootEventData> {
        let parsed_cid = response.cid.as_deref().and_then(|cid| Cid::parse(cid).ok());
        let daemon_source = response.source.clone();
        let event_created_at = response.created_at;
        let event_id = response.event_id.clone();
        let root_hash = response
            .hash
            .or_else(|| parsed_cid.as_ref().map(|cid| hex::encode(cid.hash)))?;
        if root_hash.is_empty() {
            return None;
        }

        let mut data = RootEventData {
            root_hash,
            encryption_key: parsed_cid.and_then(|cid| cid.key),
            key_tag_name: None,
            self_encrypted_ciphertext: None,
            source: RootResolveSource::LocalDaemon,
            daemon_source,
            event_created_at,
            event_id,
        };

        if let Some(ciphertext) = response.self_encrypted_key {
            data.key_tag_name = Some("selfEncryptedKey".to_string());
            data.self_encrypted_ciphertext = Some(ciphertext);
            return Some(data);
        }

        let (tag_name, tag_value) = if let Some(v) = response.encrypted_key {
            ("encryptedKey", v)
        } else if let Some(v) = response.key {
            ("key", v)
        } else {
            return Some(data);
        };

        if let Ok(bytes) = hex::decode(&tag_value) {
            if bytes.len() == 32 {
                let mut key = [0u8; 32];
                key.copy_from_slice(&bytes);
                data.encryption_key = Some(key);
                data.key_tag_name = Some(tag_name.to_string());
            }
        }

        Some(data)
    }

    async fn fetch_root_from_local_daemon(
        &self,
        repo_name: &str,
        timeout: Duration,
    ) -> Option<RootEventData> {
        let base = self.local_daemon_url.as_ref()?;
        let pubkey = self.daemon_pubkey_identifier();
        let url = format!(
            "{}/api/nostr/resolve/{}/{}?refresh=1",
            base.trim_end_matches('/'),
            pubkey,
            repo_name
        );

        let client = reqwest::Client::builder().timeout(timeout).build().ok()?;
        let response = client.get(&url).send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }

        let payload: DaemonResolveResponse = response.json().await.ok()?;
        let source = payload
            .source
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        let parsed = Self::parse_daemon_response_to_root_data(payload)?;
        debug!(
            "Resolved repo {} via local daemon source={}",
            repo_name, source
        );
        Some(parsed)
    }

    fn daemon_root_needs_relay_confirmation(data: &RootEventData) -> bool {
        if data.source != RootResolveSource::LocalDaemon {
            return false;
        }

        !matches!(data.daemon_source.as_deref(), Some("nostr-relay" | "nostr"))
    }

    fn root_event_is_newer(left: &RootEventData, right: &RootEventData) -> bool {
        match (left.event_created_at, right.event_created_at) {
            (Some(left_time), Some(right_time)) => {
                (left_time, left.event_id.as_deref().unwrap_or(""))
                    > (right_time, right.event_id.as_deref().unwrap_or(""))
            }
            (Some(_), None) => true,
            _ => false,
        }
    }

    fn choose_newer_root_data(fallback: RootEventData, relay: RootEventData) -> RootEventData {
        if Self::root_event_is_newer(&fallback, &relay) {
            fallback
        } else {
            relay
        }
    }

    async fn cache_public_root_in_local_daemon(
        &self,
        repo_name: &str,
        root_hash: &str,
        encryption_key: Option<(&[u8; 32], bool, bool)>,
    ) {
        let Some(base) = self.local_daemon_url.as_ref() else {
            return;
        };

        let key = match encryption_key {
            Some((key, false, false)) => Some(hex::encode(key)),
            Some((_key, _is_link_visible, _is_self_private)) => return,
            None => None,
        };

        let url = format!("{}/api/cache-tree-root", base.trim_end_matches('/'));
        let pubkey = self.daemon_pubkey_identifier();
        let payload = serde_json::json!({
            "npub": pubkey,
            "treeName": repo_name,
            "hash": root_hash,
            "key": key,
            "visibility": "public",
        });

        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
        {
            Ok(client) => client,
            Err(err) => {
                debug!("Could not build local daemon cache client: {}", err);
                return;
            }
        };

        match client.post(url).json(&payload).send().await {
            Ok(response) if response.status().is_success() => {
                debug!("Cached repo root for {} in local daemon", repo_name);
            }
            Ok(response) => {
                debug!(
                    "Local daemon root cache returned status {} for {}",
                    response.status(),
                    repo_name
                );
            }
            Err(err) => {
                debug!("Could not cache repo root in local daemon: {}", err);
            }
        }
    }

    async fn resolve_root_async_with_timeout(
        &self,
        repo_name: &str,
        timeout_secs: u64,
        allow_local_daemon: bool,
    ) -> Result<ResolvedRoot> {
        // Create nostr-sdk client
        let client = Client::default();

        // Add relays
        for relay in &self.relays {
            if let Err(e) = client.add_relay(relay).await {
                warn!("Failed to add relay {}: {}", relay, e);
            }
        }

        // Connect to relays - this starts async connection
        client.connect().await;

        let connect_timeout = Duration::from_secs(2);
        let query_timeout = Duration::from_secs(timeout_secs.saturating_sub(2).max(3));
        let local_daemon_timeout = Duration::from_secs(4);
        let retry_delay = Duration::from_millis(300);
        let max_attempts = 2;

        let start = std::time::Instant::now();

        // Build filter for kind 30078 events from this author with matching d-tag
        let author = PublicKey::from_hex(&self.pubkey)
            .map_err(|e| anyhow::anyhow!("Invalid pubkey: {}", e))?;

        let filter = build_repo_event_filter(author, repo_name);

        debug!("Querying relays for repo {} events", repo_name);

        let mut root_data = None;
        let mut daemon_fallback: Option<RootEventData> = None;
        for attempt in 1..=max_attempts {
            if allow_local_daemon {
                if let Some(data) = self
                    .fetch_root_from_local_daemon(repo_name, local_daemon_timeout)
                    .await
                {
                    if Self::daemon_root_needs_relay_confirmation(&data) {
                        debug!(
                            "Local daemon resolved {} via {}; checking relays for fresher root",
                            repo_name,
                            data.daemon_source.as_deref().unwrap_or("unknown")
                        );
                        daemon_fallback = Some(data);
                    } else {
                        root_data = Some(data);
                        break;
                    }
                }
            }

            if !allow_local_daemon && attempt == 1 {
                debug!(
                    "Skipping local daemon while resolving {} because relay retry was requested",
                    repo_name
                );
            }

            if allow_local_daemon && daemon_fallback.is_none() {
                debug!(
                    "Local daemon did not resolve {}; querying relays",
                    repo_name
                );
            }

            // Wait for at least one relay to connect (quick timeout - break immediately when one
            // connects). We retry once because relays and the local daemon can both lag briefly.
            let connect_start = std::time::Instant::now();
            let mut last_log = std::time::Instant::now();
            let mut has_connected_relay = false;
            loop {
                let (connected, total) = connected_relay_count(&client).await;
                if connected > 0 {
                    debug!(
                        "Connected to {}/{} relay(s) in {:?} (attempt {}/{})",
                        connected,
                        total,
                        start.elapsed(),
                        attempt,
                        max_attempts
                    );
                    has_connected_relay = true;
                    break;
                }
                if last_log.elapsed() > Duration::from_millis(500) {
                    debug!(
                        "Connecting to relays... (0/{} after {:?}, attempt {}/{})",
                        total,
                        start.elapsed(),
                        attempt,
                        max_attempts
                    );
                    last_log = std::time::Instant::now();
                }
                if connect_start.elapsed() > connect_timeout {
                    debug!(
                        "Timeout waiting for relay connections - continuing with local-daemon fallback"
                    );
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            // Query with relay-level timeout.
            // Using `EventSource::relays(Some(...))` preserves partial results from responsive
            // relays instead of discarding everything when one relay stalls.
            if !has_connected_relay {
                if let Some(data) = daemon_fallback.take() {
                    debug!(
                        "Using local daemon root for {} because no relay connected",
                        repo_name
                    );
                    root_data = Some(data);
                    break;
                }
            }

            let events = if has_connected_relay {
                match client.fetch_events(filter.clone(), query_timeout).await {
                    Ok(events) => events.to_vec(),
                    Err(e) => {
                        warn!("Failed to fetch events: {}", e);
                        vec![]
                    }
                }
            } else {
                vec![]
            };

            debug!(
                "Got {} events from relays on attempt {}/{}",
                events.len(),
                attempt,
                max_attempts
            );
            let relay_event = pick_latest_repo_event(events.iter(), repo_name);

            if let Some(event) = relay_event {
                debug!(
                    "Found relay event with root hash: {}",
                    &event.content[..12.min(event.content.len())]
                );
                let relay_data = Self::parse_root_event_data_from_event(event);
                root_data = Some(match daemon_fallback.take() {
                    Some(fallback) => Self::choose_newer_root_data(fallback, relay_data),
                    None => relay_data,
                });
                break;
            }

            if attempt < max_attempts {
                debug!(
                    "No relay hashtree event found for {} on attempt {}/{}; retrying",
                    repo_name, attempt, max_attempts
                );
                tokio::time::sleep(retry_delay).await;
            } else if let Some(data) = daemon_fallback.take() {
                debug!(
                    "Using local daemon root for {} after relay lookup returned no event",
                    repo_name
                );
                root_data = Some(data);
                break;
            }
        }

        // Disconnect
        let _ = client.disconnect().await;

        let root_data = match root_data {
            Some(data) => data,
            None => {
                anyhow::bail!(
                    "Repository '{}' not found (no hashtree event published by {})",
                    repo_name,
                    Self::format_repo_author(&self.pubkey)
                );
            }
        };

        let root_hash = root_data.root_hash;

        if root_hash.is_empty() {
            debug!("Empty root hash in event");
            return Ok(ResolvedRoot {
                root_hash: None,
                encryption_key: None,
                source: None,
            });
        }

        let encryption_key = root_data.encryption_key;
        let key_tag_name = root_data.key_tag_name;
        let self_encrypted_ciphertext = root_data.self_encrypted_ciphertext;
        let root_source = root_data.source;

        // Process encryption key based on tag type
        let unmasked_key = match key_tag_name.as_deref() {
            Some("encryptedKey") => {
                // Link-visible: XOR the masked key with url_secret
                if let (Some(masked), Some(secret)) = (encryption_key, self.url_secret) {
                    let mut unmasked = [0u8; 32];
                    for i in 0..32 {
                        unmasked[i] = masked[i] ^ secret[i];
                    }
                    Some(unmasked)
                } else {
                    anyhow::bail!(
                        "This repo is link-visible and requires a secret key.\n\
                         Use: htree://.../{repo_name}#k=<secret>\n\
                         Ask the repo owner for the full URL with the secret."
                    );
                }
            }
            Some("selfEncryptedKey") => {
                // Private: only decrypt if #private is in the URL
                if !self.is_private {
                    anyhow::bail!(
                        "This repo is private (author-only).\n\
                         Use: htree://.../{repo_name}#private\n\
                         Only the author can access this repo."
                    );
                }

                // Decrypt with NIP-44 using our secret key
                if let Some(keys) = &self.keys {
                    if let Some(ciphertext) = self_encrypted_ciphertext {
                        // Decrypt with NIP-44 (encrypted to self)
                        let pubkey = keys.public_key();
                        match nip44::decrypt(keys.secret_key(), &pubkey, &ciphertext) {
                            Ok(key_hex) => {
                                let key_bytes =
                                    hex::decode(&key_hex).context("Invalid decrypted key hex")?;
                                if key_bytes.len() != 32 {
                                    anyhow::bail!("Decrypted key wrong length");
                                }
                                let mut key = [0u8; 32];
                                key.copy_from_slice(&key_bytes);
                                Some(key)
                            }
                            Err(e) => {
                                anyhow::bail!(
                                    "Failed to decrypt private repo: {}\n\
                                     The repo may be corrupted or published with a different key.",
                                    e
                                );
                            }
                        }
                    } else {
                        anyhow::bail!("selfEncryptedKey tag has invalid format");
                    }
                } else {
                    anyhow::bail!(
                        "Cannot access this private repo.\n\
                         Private repos can only be accessed by their author.\n\
                         You don't have the secret key for this repo's owner."
                    );
                }
            }
            Some("key") | None => {
                // Public: use key directly
                encryption_key
            }
            Some(other) => {
                warn!("Unknown key tag type: {}", other);
                encryption_key
            }
        };

        info!(
            "Found root hash {} for {} (encrypted: {}, link_visible: {})",
            &root_hash[..12.min(root_hash.len())],
            repo_name,
            unmasked_key.is_some(),
            self.url_secret.is_some()
        );

        Ok(ResolvedRoot {
            root_hash: Some(root_hash),
            encryption_key: unmasked_key,
            source: Some(root_source),
        })
    }

    /// Decrypt data if encryption key is provided, then decode as tree node
    fn decrypt_and_decode(&self, data: &[u8], key: Option<&[u8; 32]>) -> Result<TreeNode> {
        let decrypted_data: Vec<u8>;
        let data_to_decode = if let Some(k) = key {
            decrypted_data = decrypt_chk(data, k).context("Decryption failed")?;
            &decrypted_data
        } else {
            data
        };

        decode_tree_node(data_to_decode).context("Failed to decode tree node")
    }

    /// Fetch git refs from hashtree structure
    /// Structure: root -> .git/ -> refs/ -> heads/main -> <sha>
    async fn fetch_refs_from_hashtree(
        &self,
        root_hash: &str,
        encryption_key: Option<&[u8; 32]>,
    ) -> Result<HashMap<String, String>> {
        let mut refs = HashMap::new();
        debug!(
            "fetch_refs_from_hashtree: downloading root {}",
            &root_hash[..12]
        );

        // Download root directory from Blossom - propagate errors properly
        let root_data = match self.blossom.download(root_hash).await {
            Ok(data) => {
                debug!("Downloaded {} bytes from blossom", data.len());
                data
            }
            Err(e) => {
                anyhow::bail!(
                    "Failed to download root hash {}: {}",
                    &root_hash[..12.min(root_hash.len())],
                    e
                );
            }
        };

        // Parse root as directory node (decrypt if needed)
        let root_node = self
            .decrypt_and_decode(&root_data, encryption_key)
            .with_context(|| {
                format!(
                    "Failed to decode root node {} (encrypted: {})",
                    &root_hash[..12.min(root_hash.len())],
                    encryption_key.is_some()
                )
            })?;
        debug!("Decoded root node with {} links", root_node.links.len());

        // Find .git directory
        debug!(
            "Root links: {:?}",
            root_node
                .links
                .iter()
                .map(|l| l.name.as_deref())
                .collect::<Vec<_>>()
        );
        let git_link = root_node
            .links
            .iter()
            .find(|l| l.name.as_deref() == Some(".git"));
        let (git_hash, git_key) = match git_link {
            Some(link) => {
                debug!("Found .git link with key: {}", link.key.is_some());
                (hex::encode(link.hash), link.key)
            }
            None => {
                debug!("No .git directory in hashtree root");
                return Ok(refs);
            }
        };

        // Download .git directory
        let git_data = match self.blossom.download(&git_hash).await {
            Ok(data) => data,
            Err(e) => {
                anyhow::bail!(
                    "Failed to download .git directory ({}): {}",
                    &git_hash[..12],
                    e
                );
            }
        };

        let git_node = self
            .decrypt_and_decode(&git_data, git_key.as_ref())
            .with_context(|| {
                format!(
                    "Failed to decode .git directory {} (encrypted: {})",
                    &git_hash[..12.min(git_hash.len())],
                    git_key.is_some()
                )
            })?;
        debug!(
            "Decoded .git node with {} links: {:?}",
            git_node.links.len(),
            git_node
                .links
                .iter()
                .map(|l| l.name.as_deref())
                .collect::<Vec<_>>()
        );

        // Find refs directory
        let refs_link = git_node
            .links
            .iter()
            .find(|l| l.name.as_deref() == Some("refs"));
        let (refs_hash, refs_key) = match refs_link {
            Some(link) => (hex::encode(link.hash), link.key),
            None => {
                debug!("No refs directory in .git");
                return Ok(refs);
            }
        };

        // Download refs directory
        let refs_data =
            self.blossom.download(&refs_hash).await.with_context(|| {
                format!("Failed to download refs directory {}", &refs_hash[..12])
            })?;

        let refs_node = self
            .decrypt_and_decode(&refs_data, refs_key.as_ref())
            .with_context(|| {
                format!(
                    "Failed to decode refs directory {} (encrypted: {})",
                    &refs_hash[..12.min(refs_hash.len())],
                    refs_key.is_some()
                )
            })?;

        // Look for HEAD in .git directory
        if let Some(head_link) = git_node
            .links
            .iter()
            .find(|l| l.name.as_deref() == Some("HEAD"))
        {
            let head_hash = hex::encode(head_link.hash);
            let head_data = self
                .blossom
                .download(&head_hash)
                .await
                .with_context(|| format!("Failed to download HEAD {}", &head_hash[..12]))?;
            // HEAD is a blob, decrypt if needed
            let head_content = if let Some(k) = head_link.key.as_ref() {
                let decrypted = decrypt_chk(&head_data, k)
                    .with_context(|| format!("Failed to decrypt HEAD {}", &head_hash[..12]))?;
                String::from_utf8_lossy(&decrypted).trim().to_string()
            } else {
                String::from_utf8_lossy(&head_data).trim().to_string()
            };
            refs.insert("HEAD".to_string(), head_content);
        }

        // Recursively walk refs/ subdirectories (heads, tags, etc.)
        for subdir_link in &refs_node.links {
            if subdir_link.link_type != LinkType::Dir {
                continue;
            }
            let subdir_name = match &subdir_link.name {
                Some(n) => n.clone(),
                None => continue,
            };
            let subdir_hash = hex::encode(subdir_link.hash);

            self.collect_refs_recursive(
                &subdir_hash,
                subdir_link.key.as_ref(),
                &format!("refs/{}", subdir_name),
                &mut refs,
            )
            .await?;
        }

        debug!("Found {} refs from hashtree", refs.len());
        Ok(refs)
    }

    /// Recursively collect refs from a directory
    async fn collect_refs_recursive(
        &self,
        dir_hash: &str,
        dir_key: Option<&[u8; 32]>,
        prefix: &str,
        refs: &mut HashMap<String, String>,
    ) -> Result<()> {
        let dir_data = self
            .blossom
            .download(dir_hash)
            .await
            .with_context(|| format!("Failed to download refs subtree {}", &dir_hash[..12]))?;

        let dir_node = self
            .decrypt_and_decode(&dir_data, dir_key)
            .with_context(|| {
                format!(
                    "Failed to decode refs subtree {} (encrypted: {})",
                    &dir_hash[..12.min(dir_hash.len())],
                    dir_key.is_some()
                )
            })?;

        for link in &dir_node.links {
            let name = match &link.name {
                Some(n) => n.clone(),
                None => continue,
            };
            let link_hash = hex::encode(link.hash);
            let ref_path = format!("{}/{}", prefix, name);

            if link.link_type == LinkType::Dir {
                // Recurse into subdirectory
                Box::pin(self.collect_refs_recursive(
                    &link_hash,
                    link.key.as_ref(),
                    &ref_path,
                    refs,
                ))
                .await?;
            } else {
                // This is a ref file - read the SHA
                let ref_data = self
                    .blossom
                    .download(&link_hash)
                    .await
                    .with_context(|| format!("Failed to download ref {}", ref_path))?;
                // Decrypt if needed
                let sha = if let Some(k) = link.key.as_ref() {
                    let decrypted = decrypt_chk(&ref_data, k)
                        .with_context(|| format!("Failed to decrypt ref {}", ref_path))?;
                    String::from_utf8_lossy(&decrypted).trim().to_string()
                } else {
                    String::from_utf8_lossy(&ref_data).trim().to_string()
                };
                if !sha.is_empty() {
                    debug!("Found ref {} -> {}", ref_path, sha);
                    refs.insert(ref_path, sha);
                }
            }
        }

        Ok(())
    }

    /// Update a ref in local cache (will be published with publish_repo)
    #[allow(dead_code)]
    pub fn update_ref(&mut self, repo_name: &str, ref_name: &str, sha: &str) -> Result<()> {
        info!("Updating ref {} -> {} for {}", ref_name, sha, repo_name);

        let refs = self.cached_refs.entry(repo_name.to_string()).or_default();
        refs.insert(ref_name.to_string(), sha.to_string());

        Ok(())
    }

    /// Delete a ref from local cache
    pub fn delete_ref(&mut self, repo_name: &str, ref_name: &str) -> Result<()> {
        info!("Deleting ref {} for {}", ref_name, repo_name);

        if let Some(refs) = self.cached_refs.get_mut(repo_name) {
            refs.remove(ref_name);
        }

        Ok(())
    }

    /// Get cached root hash for a repository
    pub fn get_cached_root_hash(&self, repo_name: &str) -> Option<&String> {
        self.cached_root_hash.get(repo_name)
    }

    /// Get cached encryption key for a repository
    pub fn get_cached_encryption_key(&self, repo_name: &str) -> Option<&[u8; 32]> {
        self.cached_encryption_key.get(repo_name)
    }

    pub(crate) fn cached_root_is_from_local_daemon(&self, repo_name: &str) -> bool {
        self.cached_root_source.get(repo_name) == Some(&RootResolveSource::LocalDaemon)
    }

    /// Get the Blossom client for direct downloads
    pub fn blossom(&self) -> &BlossomClient {
        &self.blossom
    }

    /// Get the configured relay URLs
    pub fn relay_urls(&self) -> Vec<String> {
        self.relays.clone()
    }

    /// Get the public key (hex)
    #[allow(dead_code)]
    pub fn pubkey(&self) -> &str {
        &self.pubkey
    }

    /// Get the public key as npub bech32
    pub fn npub(&self) -> String {
        PublicKey::from_hex(&self.pubkey)
            .ok()
            .and_then(|pk| pk.to_bech32().ok())
            .unwrap_or_else(|| self.pubkey.clone())
    }

    /// Publish repository to nostr as kind 30078 event
    /// Format:
    ///   kind: 30078
    ///   tags: [["d", repo_name], ["l", "hashtree"], ["hash", root_hash], ["key"|"encryptedKey", encryption_key]]
    ///   content: <merkle-root-hash>
    /// Returns: (npub URL, relay result with connected/failed details)
    /// If is_private is true, uses "encryptedKey" tag (XOR masked); otherwise uses "key" tag (plaintext CHK)
    pub fn publish_repo(
        &self,
        repo_name: &str,
        root_hash: &str,
        encryption_key: Option<(&[u8; 32], bool, bool)>,
    ) -> Result<(String, RelayResult)> {
        self.publish_repo_with_announcement(repo_name, root_hash, encryption_key, None)
    }

    pub fn publish_repo_with_announcement(
        &self,
        repo_name: &str,
        root_hash: &str,
        encryption_key: Option<(&[u8; 32], bool, bool)>,
        repo_announcement: Option<RepoAnnouncementOptions>,
    ) -> Result<(String, RelayResult)> {
        let keys = self.keys.as_ref().context(format!(
            "Cannot push: no secret key for {}. You can only push to your own repos.",
            &self.pubkey[..16]
        ))?;

        info!(
            "Publishing repo {} with root hash {} (encrypted: {})",
            repo_name,
            root_hash,
            encryption_key.is_some()
        );

        // Create a new multi-threaded runtime for nostr-sdk which spawns background tasks
        block_on_result(self.publish_repo_async(
            keys,
            repo_name,
            root_hash,
            encryption_key,
            repo_announcement,
        ))
    }

    async fn publish_repo_async(
        &self,
        keys: &Keys,
        repo_name: &str,
        root_hash: &str,
        encryption_key: Option<(&[u8; 32], bool, bool)>,
        repo_announcement: Option<RepoAnnouncementOptions>,
    ) -> Result<(String, RelayResult)> {
        // Create nostr-sdk client with our keys
        let client = Client::new(keys.clone());

        let configured: Vec<String> = self.relays.clone();
        let mut connected: Vec<String> = Vec::new();
        let mut failed: Vec<String> = Vec::new();

        // Add relays
        for relay in &self.relays {
            if let Err(e) = client.add_relay(relay).await {
                warn!("Failed to add relay {}: {}", relay, e);
                failed.push(relay.clone());
            }
        }

        // Connect to relays - this starts async connection in background
        client.connect().await;

        // Wait for at least one relay to connect (same pattern as fetch)
        let _ = wait_for_any_connected_relay(&client, Duration::from_secs(3)).await;

        let publish_created_at = next_replaceable_created_at(
            Timestamp::now(),
            latest_repo_event_created_at(
                &client,
                keys.public_key(),
                repo_name,
                Duration::from_secs(2),
            )
            .await,
        );

        // Build event with tags
        let mut tags = vec![
            Tag::custom(TagKind::custom("d"), vec![repo_name.to_string()]),
            Tag::custom(TagKind::custom("l"), vec![LABEL_HASHTREE.to_string()]),
            Tag::custom(TagKind::custom("hash"), vec![root_hash.to_string()]),
        ];

        // Add encryption key if present (required for decryption)
        // Key modes:
        // - selfEncryptedKey: NIP-44 encrypted to self (author-only private)
        // - encryptedKey: XOR masked with URL secret (link-visible)
        // - key: plaintext CHK (public)
        if let Some((key, is_link_visible, is_self_private)) = encryption_key {
            if is_self_private {
                // NIP-44 encrypt to self
                let pubkey = keys.public_key();
                let key_hex = hex::encode(key);
                let encrypted =
                    nip44::encrypt(keys.secret_key(), &pubkey, &key_hex, nip44::Version::V2)
                        .map_err(|e| anyhow::anyhow!("NIP-44 encryption failed: {}", e))?;
                tags.push(Tag::custom(
                    TagKind::custom("selfEncryptedKey"),
                    vec![encrypted],
                ));
            } else if is_link_visible {
                // XOR masked key
                tags.push(Tag::custom(
                    TagKind::custom("encryptedKey"),
                    vec![hex::encode(key)],
                ));
            } else {
                // Public: plaintext CHK
                tags.push(Tag::custom(TagKind::custom("key"), vec![hex::encode(key)]));
            }
        }

        append_repo_discovery_labels(&mut tags, repo_name);

        // Sign the event
        let event = EventBuilder::new(Kind::Custom(KIND_APP_DATA), root_hash)
            .tags(tags)
            .custom_created_at(publish_created_at)
            .sign_with_keys(keys)
            .map_err(|e| anyhow::anyhow!("Failed to sign event: {}", e))?;

        // Send event to connected relays
        match client.send_event(&event).await {
            Ok(output) => {
                // Track which relays confirmed
                for url in output.success.iter() {
                    let url_str = url.to_string();
                    if !connected.contains(&url_str) {
                        connected.push(url_str);
                    }
                }
                // Only mark as failed if we got explicit rejection
                for (url, _err) in output.failed.iter() {
                    let url_str = url.to_string();
                    if !failed.contains(&url_str) && !connected.contains(&url_str) {
                        failed.push(url_str);
                    }
                }
                info!(
                    "Sent event {} to {} relays ({} failed)",
                    output.id(),
                    output.success.len(),
                    output.failed.len()
                );
            }
            Err(e) => {
                warn!("Failed to send event: {}", e);
                // Mark all as failed
                for relay in &self.relays {
                    if !failed.contains(relay) {
                        failed.push(relay.clone());
                    }
                }
            }
        };

        // Build the full htree:// URL with npub
        let npub_url = keys
            .public_key()
            .to_bech32()
            .map(|npub| format!("htree://{}/{}", npub, repo_name))
            .unwrap_or_else(|_| format!("htree://{}/{}", &self.pubkey[..16], repo_name));

        let relay_validation = validate_repo_publish_relays(&configured, &connected);

        if relay_validation.is_ok() {
            if let Some(repo_announcement) = repo_announcement.as_ref() {
                if let Err(err) = self
                    .publish_repo_announcement_async(
                        &client,
                        keys,
                        repo_name,
                        &npub_url,
                        repo_announcement,
                    )
                    .await
                {
                    warn!("Failed to publish NIP-34 repo announcement: {}", err);
                }
            }
        }

        // Disconnect and give time for cleanup
        let _ = client.disconnect().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        relay_validation?;

        self.cache_public_root_in_local_daemon(repo_name, root_hash, encryption_key)
            .await;

        Ok((
            npub_url,
            RelayResult {
                configured,
                connected,
                failed,
            },
        ))
    }

    fn build_repo_announcement_tags(
        repo_name: &str,
        clone_url: &str,
        relays: &[String],
        options: &RepoAnnouncementOptions,
    ) -> Vec<Tag> {
        let mut tags = vec![
            Tag::custom(TagKind::custom("d"), vec![repo_name.to_string()]),
            Tag::custom(TagKind::custom("name"), vec![repo_name.to_string()]),
            Tag::custom(TagKind::custom("clone"), vec![clone_url.to_string()]),
        ];

        if !relays.is_empty() {
            tags.push(Tag::custom(TagKind::custom("relays"), relays.to_vec()));
        }

        if let Some(euc) = options
            .earliest_unique_commit
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            tags.push(Tag::custom(
                TagKind::custom("r"),
                vec![euc.to_string(), "euc".to_string()],
            ));
        }

        if options.personal_fork {
            tags.push(Tag::custom(
                TagKind::custom("t"),
                vec!["personal-fork".to_string()],
            ));
        }

        if let Some(forked_from) = options
            .forked_from
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            tags.push(Tag::custom(
                TagKind::custom("forked-from"),
                vec![forked_from.to_string()],
            ));
        }

        tags
    }

    async fn publish_repo_announcement_async(
        &self,
        client: &Client,
        keys: &Keys,
        repo_name: &str,
        clone_url: &str,
        options: &RepoAnnouncementOptions,
    ) -> Result<()> {
        let created_at = next_replaceable_created_at(
            Timestamp::now(),
            latest_repo_announcement_created_at(
                client,
                keys.public_key(),
                repo_name,
                Duration::from_secs(2),
            )
            .await,
        );
        let tags = Self::build_repo_announcement_tags(repo_name, clone_url, &self.relays, options);
        let event = EventBuilder::new(Kind::Custom(KIND_REPO_ANNOUNCEMENT), "")
            .tags(tags)
            .custom_created_at(created_at)
            .sign_with_keys(keys)
            .map_err(|e| anyhow::anyhow!("Failed to sign NIP-34 repo announcement: {}", e))?;

        let output = client
            .send_event(&event)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to publish NIP-34 repo announcement: {}", e))?;
        if output.success.is_empty() {
            anyhow::bail!("NIP-34 repo announcement was not confirmed by any relay");
        }

        info!(
            "Published NIP-34 repo announcement {} to {} relays",
            output.id(),
            output.success.len()
        );
        Ok(())
    }

    pub fn fetch_repo_announcement_euc(
        &self,
        owner_pubkey_hex: &str,
        repo_name: &str,
    ) -> Result<Option<String>> {
        block_on_result(self.fetch_repo_announcement_euc_async(owner_pubkey_hex, repo_name))
    }

    async fn fetch_repo_announcement_euc_async(
        &self,
        owner_pubkey_hex: &str,
        repo_name: &str,
    ) -> Result<Option<String>> {
        let owner = PublicKey::from_hex(owner_pubkey_hex)
            .map_err(|e| anyhow::anyhow!("Invalid repo owner pubkey: {}", e))?;
        let client = Client::default();

        for relay in &self.relays {
            if let Err(e) = client.add_relay(relay).await {
                debug!(
                    "Failed to add relay {} for repo announcement lookup: {}",
                    relay, e
                );
            }
        }
        client.connect().await;

        if !wait_for_any_connected_relay(&client, Duration::from_secs(2)).await {
            let _ = client.disconnect().await;
            return Ok(None);
        }

        let filter = build_repo_announcement_filter(owner, repo_name);
        let events = match tokio::time::timeout(
            Duration::from_secs(3),
            client.fetch_events(filter, Duration::from_secs(3)),
        )
        .await
        {
            Ok(Ok(events)) => events.to_vec(),
            Ok(Err(err)) => {
                debug!(
                    "Failed to fetch NIP-34 repo announcement for {}: {}",
                    repo_name, err
                );
                Vec::new()
            }
            Err(_) => {
                debug!(
                    "Timed out fetching NIP-34 repo announcement for {}",
                    repo_name
                );
                Vec::new()
            }
        };

        let _ = client.disconnect().await;
        Ok(pick_latest_event(events.iter()).and_then(extract_repo_announcement_euc))
    }

    /// Fetch pull requests targeting this repo, filtered by state.
    pub fn fetch_prs(
        &self,
        repo_name: &str,
        state_filter: PullRequestStateFilter,
    ) -> Result<Vec<PullRequestListItem>> {
        block_on_result(self.fetch_prs_async(repo_name, state_filter))
    }

    pub async fn fetch_prs_async(
        &self,
        repo_name: &str,
        state_filter: PullRequestStateFilter,
    ) -> Result<Vec<PullRequestListItem>> {
        let client = Client::default();

        for relay in &self.relays {
            if let Err(e) = client.add_relay(relay).await {
                warn!("Failed to add relay {}: {}", relay, e);
            }
        }
        client.connect().await;

        // Wait for at least one relay (quick timeout)
        if !wait_for_any_connected_relay(&client, Duration::from_secs(2)).await {
            let _ = client.disconnect().await;
            return Err(anyhow::anyhow!(
                "Failed to connect to any relay while fetching PRs"
            ));
        }

        // Query for kind 1618 PRs targeting this repo
        let repo_address = format!("{}:{}:{}", KIND_REPO_ANNOUNCEMENT, self.pubkey, repo_name);
        let pull_request_filter = Filter::new()
            .kind(Kind::Custom(KIND_PULL_REQUEST))
            .custom_tag(
                SingleLetterTag::lowercase(Alphabet::A),
                repo_address.clone(),
            );

        let mut pr_events = match tokio::time::timeout(
            Duration::from_secs(3),
            client.fetch_events(pull_request_filter.clone(), Duration::from_secs(3)),
        )
        .await
        {
            Ok(Ok(events)) => events.to_vec(),
            Ok(Err(e)) => {
                let _ = client.disconnect().await;
                return Err(anyhow::anyhow!(
                    "Failed to fetch PR events from relays: {}",
                    e
                ));
            }
            Err(_) => {
                let _ = client.disconnect().await;
                return Err(anyhow::anyhow!("Timed out fetching PR events from relays"));
            }
        };

        if pr_events.is_empty() {
            let fallback_events = fetch_events_via_raw_relay_query(
                &self.relays,
                pull_request_filter,
                Duration::from_secs(3),
            )
            .await;
            if !fallback_events.is_empty() {
                debug!(
                    "Raw relay fallback recovered {} PR event(s) for {}",
                    fallback_events.len(),
                    repo_name
                );
                pr_events = fallback_events;
            }
        }

        if pr_events.is_empty() {
            let _ = client.disconnect().await;
            return Ok(Vec::new());
        }

        // Collect PR event IDs for status query
        let pr_ids: Vec<String> = pr_events.iter().map(|e| e.id.to_hex()).collect();

        // Query for status events referencing these PRs
        let status_event_filter = Filter::new()
            .kinds(vec![
                Kind::Custom(KIND_STATUS_OPEN),
                Kind::Custom(KIND_STATUS_APPLIED),
                Kind::Custom(KIND_STATUS_CLOSED),
                Kind::Custom(KIND_STATUS_DRAFT),
            ])
            .custom_tags(
                SingleLetterTag::lowercase(Alphabet::E),
                pr_ids.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            );

        let mut status_events = match tokio::time::timeout(
            Duration::from_secs(3),
            client.fetch_events(status_event_filter.clone(), Duration::from_secs(3)),
        )
        .await
        {
            Ok(Ok(events)) => events.to_vec(),
            Ok(Err(e)) => {
                let _ = client.disconnect().await;
                return Err(anyhow::anyhow!(
                    "Failed to fetch PR status events from relays: {}",
                    e
                ));
            }
            Err(_) => {
                let _ = client.disconnect().await;
                return Err(anyhow::anyhow!(
                    "Timed out fetching PR status events from relays"
                ));
            }
        };

        if status_events.is_empty() {
            let fallback_events = fetch_events_via_raw_relay_query(
                &self.relays,
                status_event_filter,
                Duration::from_secs(3),
            )
            .await;
            if !fallback_events.is_empty() {
                debug!(
                    "Raw relay fallback recovered {} PR status event(s) for {}",
                    fallback_events.len(),
                    repo_name
                );
                status_events = fallback_events;
            }
        }

        let _ = client.disconnect().await;

        // Build map: pr_event_id -> latest trusted status kind
        let latest_status =
            latest_trusted_pr_status_kinds(&pr_events, &status_events, &self.pubkey);

        let mut prs = Vec::new();
        for event in &pr_events {
            let pr_id = event.id.to_hex();
            let state =
                PullRequestState::from_latest_status_kind(latest_status.get(&pr_id).copied());
            if !state_filter.includes(state) {
                continue;
            }

            let mut subject = None;
            let mut commit_tip = None;
            let mut branch = None;
            let mut target_branch = None;

            for tag in event.tags.iter() {
                let slice = tag.as_slice();
                if slice.len() >= 2 {
                    match slice[0].as_str() {
                        "subject" => subject = Some(slice[1].to_string()),
                        "c" => commit_tip = Some(slice[1].to_string()),
                        "branch" => branch = Some(slice[1].to_string()),
                        "target-branch" => target_branch = Some(slice[1].to_string()),
                        _ => {}
                    }
                }
            }

            prs.push(PullRequestListItem {
                event_id: pr_id,
                author_pubkey: event.pubkey.to_hex(),
                state,
                subject,
                commit_tip,
                branch,
                target_branch,
                created_at: event.created_at.as_secs(),
            });
        }

        // Newest first; tie-break by event id for deterministic output.
        prs.sort_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| right.event_id.cmp(&left.event_id))
        });

        debug!(
            "Found {} PRs for {} (filter: {:?})",
            prs.len(),
            repo_name,
            state_filter
        );
        Ok(prs)
    }

    /// Publish a kind 1631 (STATUS_APPLIED) event to mark a PR as merged
    pub fn publish_pr_merged_status(
        &self,
        pr_event_id: &str,
        pr_author_pubkey: &str,
    ) -> Result<()> {
        let keys = self
            .keys
            .as_ref()
            .context("Cannot publish status: no secret key")?;

        block_on_result(self.publish_pr_merged_status_async(keys, pr_event_id, pr_author_pubkey))
    }

    async fn publish_pr_merged_status_async(
        &self,
        keys: &Keys,
        pr_event_id: &str,
        pr_author_pubkey: &str,
    ) -> Result<()> {
        let client = Client::new(keys.clone());

        for relay in &self.relays {
            if let Err(e) = client.add_relay(relay).await {
                warn!("Failed to add relay {}: {}", relay, e);
            }
        }
        client.connect().await;

        // Wait for at least one relay
        if !wait_for_any_connected_relay(&client, Duration::from_secs(3)).await {
            anyhow::bail!("Failed to connect to any relay for status publish");
        }

        let tags = vec![
            Tag::custom(TagKind::custom("e"), vec![pr_event_id.to_string()]),
            Tag::custom(TagKind::custom("p"), vec![pr_author_pubkey.to_string()]),
        ];

        let event = EventBuilder::new(Kind::Custom(KIND_STATUS_APPLIED), "")
            .tags(tags)
            .sign_with_keys(keys)
            .map_err(|e| anyhow::anyhow!("Failed to sign status event: {}", e))?;

        let publish_result = match client.send_event(&event).await {
            Ok(output) => {
                if output.success.is_empty() {
                    Err(anyhow::anyhow!(
                        "PR merged status was not confirmed by any relay"
                    ))
                } else {
                    info!(
                        "Published PR merged status to {} relays",
                        output.success.len()
                    );
                    Ok(())
                }
            }
            Err(e) => Err(anyhow::anyhow!("Failed to publish PR merged status: {}", e)),
        };

        let _ = client.disconnect().await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        publish_result
    }

    /// Upload blob to blossom server
    #[allow(dead_code)]
    pub async fn upload_blob(&self, _hash: &str, data: &[u8]) -> Result<String> {
        let hash = self
            .blossom
            .upload(data)
            .await
            .map_err(|e| anyhow::anyhow!("Blossom upload failed: {}", e))?;
        Ok(hash)
    }

    /// Upload blob only if it doesn't exist
    #[allow(dead_code)]
    pub async fn upload_blob_if_missing(&self, data: &[u8]) -> Result<(String, bool)> {
        self.blossom
            .upload_if_missing(data)
            .await
            .map_err(|e| anyhow::anyhow!("Blossom upload failed: {}", e))
    }

    /// Download blob from blossom server
    #[allow(dead_code)]
    pub async fn download_blob(&self, hash: &str) -> Result<Vec<u8>> {
        self.blossom
            .download(hash)
            .await
            .map_err(|e| anyhow::anyhow!("Blossom download failed: {}", e))
    }

    /// Try to download blob, returns None if not found
    #[allow(dead_code)]
    pub async fn try_download_blob(&self, hash: &str) -> Option<Vec<u8>> {
        self.blossom.try_download(hash).await
    }
}

#[cfg(test)]
mod tests;
