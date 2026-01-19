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
//! ## Secret file format
//!
//! The secrets file (~/.hashtree/keys) supports multiple keys with optional petnames:
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
//! Then use: `htree://work/myrepo` or `htree://npub1.../myrepo`

use anyhow::{Context, Result};
use hashtree_blossom::BlossomClient;
use hashtree_core::{decode_tree_node, decrypt_chk, LinkType};
use nostr_sdk::prelude::*;
use std::collections::HashMap;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Event kind for application-specific data (NIP-78)
pub const KIND_APP_DATA: u16 = 30078;

/// Label for hashtree events
pub const LABEL_HASHTREE: &str = "hashtree";

/// A stored key with optional petname
#[derive(Debug, Clone)]
pub struct StoredKey {
    /// Secret key in hex format
    pub secret_hex: String,
    /// Public key in hex format
    pub pubkey_hex: String,
    /// Optional petname (e.g., "default", "work")
    pub petname: Option<String>,
}

impl StoredKey {
    /// Create from secret key hex, deriving pubkey
    pub fn from_secret_hex(secret_hex: &str, petname: Option<String>) -> Result<Self> {
        use secp256k1::{Secp256k1, SecretKey};

        let sk_bytes = hex::decode(secret_hex).context("Invalid hex in secret key")?;
        let sk = SecretKey::from_slice(&sk_bytes).context("Invalid secret key")?;
        let secp = Secp256k1::new();
        let pk = sk.x_only_public_key(&secp).0;
        let pubkey_hex = hex::encode(pk.serialize());

        Ok(Self {
            secret_hex: secret_hex.to_string(),
            pubkey_hex,
            petname,
        })
    }

    /// Create from nsec bech32 format
    pub fn from_nsec(nsec: &str, petname: Option<String>) -> Result<Self> {
        let secret_key =
            SecretKey::parse(nsec).map_err(|e| anyhow::anyhow!("Invalid nsec format: {}", e))?;
        let secret_hex = hex::encode(secret_key.to_secret_bytes());
        Self::from_secret_hex(&secret_hex, petname)
    }
}

/// Load all keys from config files
pub fn load_keys() -> Vec<StoredKey> {
    let mut keys = Vec::new();

    // Primary: ~/.hashtree/keys (multi-key format)
    let keys_path = hashtree_config::get_keys_path();
    if let Ok(content) = std::fs::read_to_string(&keys_path) {
        for entry in hashtree_config::parse_keys_file(&content) {
            let key = if entry.secret.starts_with("nsec1") {
                StoredKey::from_nsec(&entry.secret, entry.alias)
            } else if entry.secret.len() == 64 {
                StoredKey::from_secret_hex(&entry.secret, entry.alias)
            } else {
                continue;
            };

            if let Ok(k) = key {
                debug!(
                    "Loaded key: pubkey={}, petname={:?}",
                    k.pubkey_hex, k.petname
                );
                keys.push(k);
            }
        }
    }

    keys
}

/// Resolve an identifier to (pubkey_hex, secret_hex)
/// Identifier can be:
/// - "self" (uses default key, auto-generates if needed)
/// - petname (e.g., "work", "default")
/// - pubkey hex (64 chars)
/// - npub bech32
pub fn resolve_identity(identifier: &str) -> Result<(String, Option<String>)> {
    let keys = load_keys();

    // Special "self" alias - use default key or first available, auto-generate if none
    if identifier == "self" {
        // First try to find a key with "self" petname
        if let Some(key) = keys.iter().find(|k| k.petname.as_deref() == Some("self")) {
            return Ok((key.pubkey_hex.clone(), Some(key.secret_hex.clone())));
        }
        // Then try "default"
        if let Some(key) = keys.iter().find(|k| k.petname.as_deref() == Some("default")) {
            return Ok((key.pubkey_hex.clone(), Some(key.secret_hex.clone())));
        }
        // Then use first available key
        if let Some(key) = keys.first() {
            return Ok((key.pubkey_hex.clone(), Some(key.secret_hex.clone())));
        }
        // No keys - auto-generate one with "self" petname
        let new_key = generate_and_save_key("self")?;
        info!("Generated new identity: npub1{}", &new_key.pubkey_hex[..12]);
        return Ok((new_key.pubkey_hex, Some(new_key.secret_hex)));
    }

    // Check if it's a petname
    for key in &keys {
        if key.petname.as_deref() == Some(identifier) {
            return Ok((key.pubkey_hex.clone(), Some(key.secret_hex.clone())));
        }
    }

    // Check if it's an npub
    if identifier.starts_with("npub1") {
        let pk =
            PublicKey::parse(identifier).map_err(|e| anyhow::anyhow!("Invalid npub format: {}", e))?;
        let pubkey_hex = hex::encode(pk.to_bytes());

        // Check if we have the secret for this pubkey
        let secret = keys
            .iter()
            .find(|k| k.pubkey_hex == pubkey_hex)
            .map(|k| k.secret_hex.clone());

        return Ok((pubkey_hex, secret));
    }

    // Check if it's a hex pubkey
    if identifier.len() == 64 && hex::decode(identifier).is_ok() {
        let secret = keys
            .iter()
            .find(|k| k.pubkey_hex == identifier)
            .map(|k| k.secret_hex.clone());

        return Ok((identifier.to_string(), secret));
    }

    // Unknown identifier - might be a petname we don't have
    anyhow::bail!(
        "Unknown identity '{}'. Add it to ~/.hashtree/keys or use a pubkey/npub.",
        identifier
    )
}

/// Generate a new key and save it to ~/.hashtree/keys with the given petname
fn generate_and_save_key(petname: &str) -> Result<StoredKey> {
    use std::fs::{self, OpenOptions};
    use std::io::Write;

    // Generate new key
    let keys = nostr_sdk::Keys::generate();
    let secret_hex = hex::encode(keys.secret_key().to_secret_bytes());
    let pubkey_hex = hex::encode(keys.public_key().to_bytes());

    // Ensure directory exists
    let keys_path = hashtree_config::get_keys_path();
    if let Some(parent) = keys_path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Append to keys file
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&keys_path)?;

    // Write as nsec with petname
    let nsec = keys.secret_key().to_bech32()
        .map_err(|e| anyhow::anyhow!("Failed to encode nsec: {}", e))?;
    writeln!(file, "{} {}", nsec, petname)?;

    info!("Saved new key to {:?} with petname '{}'", keys_path, petname);

    Ok(StoredKey {
        secret_hex,
        pubkey_hex,
        petname: Some(petname.to_string()),
    })
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
    /// URL secret for link-visible repos (#k=<hex>)
    /// If set, encryption keys from nostr are XOR-masked and need unmasking
    url_secret: Option<[u8; 32]>,
    /// Whether this is a private (author-only) repo using NIP-44 encryption
    is_private: bool,
}

impl NostrClient {
    /// Create a new client with pubkey, optional secret key, url secret, is_private flag, and config
    pub fn new(pubkey: &str, secret_key: Option<String>, url_secret: Option<[u8; 32]>, is_private: bool, config: &Config) -> Result<Self> {
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

        // Create BlossomClient (needs keys for upload auth)
        // BlossomClient auto-loads servers from config
        let blossom_keys = keys.clone().unwrap_or_else(Keys::generate);
        let blossom = BlossomClient::new(blossom_keys)
            .with_timeout(Duration::from_secs(30));

        tracing::info!("BlossomClient created with read_servers: {:?}, write_servers: {:?}",
            blossom.read_servers(), blossom.write_servers());

        Ok(Self {
            pubkey: pubkey.to_string(),
            keys,
            relays: config.nostr.relays.clone(),
            blossom,
            cached_refs: HashMap::new(),
            cached_root_hash: HashMap::new(),
            cached_encryption_key: HashMap::new(),
            url_secret,
            is_private,
        })
    }

    /// Check if we can sign (have secret key for this pubkey)
    #[allow(dead_code)]
    pub fn can_sign(&self) -> bool {
        self.keys.is_some()
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
    pub fn fetch_refs_with_root(&mut self, repo_name: &str) -> Result<(HashMap<String, String>, Option<String>, Option<[u8; 32]>)> {
        self.fetch_refs_with_timeout(repo_name, 10)
    }

    /// Fetch refs with configurable timeout
    fn fetch_refs_with_timeout(&mut self, repo_name: &str, timeout_secs: u64) -> Result<(HashMap<String, String>, Option<String>, Option<[u8; 32]>)> {
        debug!("Fetching refs for {} from {} (timeout {}s)", repo_name, self.pubkey, timeout_secs);

        // Check cache first
        if let Some(refs) = self.cached_refs.get(repo_name) {
            let root = self.cached_root_hash.get(repo_name).cloned();
            let key = self.cached_encryption_key.get(repo_name).cloned();
            return Ok((refs.clone(), root, key));
        }

        // Query relays for kind 30078 events
        // Create a new multi-threaded runtime for nostr-sdk which spawns background tasks
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("Failed to create tokio runtime")?;

        let (refs, root_hash, encryption_key) = rt.block_on(self.fetch_refs_async_with_timeout(repo_name, timeout_secs))?;
        self.cached_refs.insert(repo_name.to_string(), refs.clone());
        if let Some(ref root) = root_hash {
            self.cached_root_hash.insert(repo_name.to_string(), root.clone());
        }
        if let Some(key) = encryption_key {
            self.cached_encryption_key.insert(repo_name.to_string(), key);
        }
        Ok((refs, root_hash, encryption_key))
    }

    async fn fetch_refs_async_with_timeout(&self, repo_name: &str, timeout_secs: u64) -> Result<(HashMap<String, String>, Option<String>, Option<[u8; 32]>)> {
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

        // Wait for at least one relay to connect (quick timeout - break immediately when one connects)
        // Use shorter connect timeout (2s max) since we only need 1 relay
        let connect_timeout = Duration::from_secs(2);
        let query_timeout = Duration::from_secs(timeout_secs.saturating_sub(2).max(3));

        let start = std::time::Instant::now();
        let mut last_log = std::time::Instant::now();
        loop {
            let relays = client.relays().await;
            let total = relays.len();
            let mut connected = 0;
            for relay in relays.values() {
                if relay.is_connected().await {
                    connected += 1;
                }
            }
            if connected > 0 {
                debug!("Connected to {}/{} relay(s) in {:?}", connected, total, start.elapsed());
                break;
            }
            // Log progress every 500ms so user knows something is happening
            if last_log.elapsed() > Duration::from_millis(500) {
                debug!("Connecting to relays... (0/{} after {:?})", total, start.elapsed());
                last_log = std::time::Instant::now();
            }
            if start.elapsed() > connect_timeout {
                debug!("Timeout waiting for relay connections - treating as empty repo");
                let _ = client.disconnect().await;
                return Ok((HashMap::new(), None, None));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Build filter for kind 30078 events from this author with matching d-tag
        let author = PublicKey::from_hex(&self.pubkey)
            .map_err(|e| anyhow::anyhow!("Invalid pubkey: {}", e))?;

        let filter = Filter::new()
            .kind(Kind::Custom(KIND_APP_DATA))
            .author(author)
            .custom_tag(SingleLetterTag::lowercase(Alphabet::D), vec![repo_name])
            .custom_tag(SingleLetterTag::lowercase(Alphabet::L), vec![LABEL_HASHTREE])
            .limit(50);

        debug!("Querying relays for repo {} events", repo_name);

        // Query with timeout - treat timeout as "no events found" for new repos
        let events = match tokio::time::timeout(
            query_timeout,
            client.get_events_of(vec![filter], EventSource::relays(None)),
        )
        .await
        {
            Ok(Ok(events)) => events,
            Ok(Err(e)) => {
                warn!("Failed to fetch events: {}", e);
                vec![]
            }
            Err(_) => {
                debug!("Relay query timed out - treating as empty repo");
                vec![]
            }
        };

        // Disconnect
        let _ = client.disconnect().await;

        // Find the most recent event with "hashtree" label
        debug!("Got {} events from relays", events.len());
        let event = events
            .iter()
            .filter(|e| {
                e.tags.iter().any(|t| {
                    t.as_slice().len() >= 2
                        && t.as_slice()[0].as_str() == "l"
                        && t.as_slice()[1].as_str() == LABEL_HASHTREE
                })
            })
            .max_by_key(|e| e.created_at);

        let Some(event) = event else {
            anyhow::bail!("Repository '{}' not found (no hashtree event published by {})", repo_name, &self.pubkey[..12]);
        };
        debug!("Found event with root hash: {}", &event.content[..12.min(event.content.len())]);

        // Get root hash from content or "hash" tag
        let root_hash = event
            .tags
            .iter()
            .find(|t| t.as_slice().len() >= 2 && t.as_slice()[0].as_str() == "hash")
            .map(|t| t.as_slice()[1].to_string())
            .unwrap_or_else(|| event.content.to_string());

        if root_hash.is_empty() {
            debug!("Empty root hash in event");
            return Ok((HashMap::new(), None, None));
        }

        // Get encryption key and determine visibility type from tag name
        // - "key": public repo, key is plaintext CHK (hex)
        // - "encryptedKey": link-visible repo, key is XOR-masked (hex)
        // - "selfEncryptedKey": private repo, key is NIP-44 ciphertext (base64)
        let (encryption_key, key_tag_name, self_encrypted_ciphertext) = event
            .tags
            .iter()
            .find_map(|t| {
                let slice = t.as_slice();
                if slice.len() >= 2 {
                    let tag_name = slice[0].as_str();
                    let tag_value = slice[1].to_string();

                    if tag_name == "selfEncryptedKey" {
                        // NIP-44 ciphertext - don't parse as hex, save for later decryption
                        return Some((None, Some(tag_name.to_string()), Some(tag_value)));
                    } else if tag_name == "key" || tag_name == "encryptedKey" {
                        // Hex-encoded key
                        if let Ok(bytes) = hex::decode(&tag_value) {
                            if bytes.len() == 32 {
                                let mut key = [0u8; 32];
                                key.copy_from_slice(&bytes);
                                return Some((Some(key), Some(tag_name.to_string()), None));
                            }
                        }
                    }
                }
                None
            })
            .unwrap_or((None, None, None));

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
                                let key_bytes = hex::decode(&key_hex)
                                    .context("Invalid decrypted key hex")?;
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

        info!("Found root hash {} for {} (encrypted: {}, link_visible: {})",
              &root_hash[..12.min(root_hash.len())], repo_name, unmasked_key.is_some(), self.url_secret.is_some());

        // Fetch refs from hashtree structure at root_hash
        let refs = self.fetch_refs_from_hashtree(&root_hash, unmasked_key.as_ref()).await?;
        Ok((refs, Some(root_hash), unmasked_key))
    }

    /// Decrypt data if encryption key is provided, then decode as tree node
    fn decrypt_and_decode(&self, data: &[u8], key: Option<&[u8; 32]>) -> Option<hashtree_core::TreeNode> {
        let decrypted_data: Vec<u8>;
        let data_to_decode = if let Some(k) = key {
            match decrypt_chk(data, k) {
                Ok(d) => {
                    decrypted_data = d;
                    &decrypted_data
                },
                Err(e) => {
                    debug!("Decryption failed: {}", e);
                    return None;
                }
            }
        } else {
            data
        };

        match decode_tree_node(data_to_decode) {
            Ok(node) => Some(node),
            Err(e) => {
                debug!("Failed to decode tree node: {}", e);
                None
            }
        }
    }

    /// Fetch git refs from hashtree structure
    /// Structure: root -> .git/ -> refs/ -> heads/main -> <sha>
    async fn fetch_refs_from_hashtree(&self, root_hash: &str, encryption_key: Option<&[u8; 32]>) -> Result<HashMap<String, String>> {
        let mut refs = HashMap::new();
        debug!("fetch_refs_from_hashtree: downloading root {}", &root_hash[..12]);

        // Download root directory from Blossom - propagate errors properly
        let root_data = match self.blossom.download(root_hash).await {
            Ok(data) => {
                debug!("Downloaded {} bytes from blossom", data.len());
                data
            }
            Err(e) => {
                anyhow::bail!("Failed to download root hash {}: {}", &root_hash[..12.min(root_hash.len())], e);
            }
        };

        // Parse root as directory node (decrypt if needed)
        let root_node = match self.decrypt_and_decode(&root_data, encryption_key) {
            Some(node) => {
                debug!("Decoded root node with {} links", node.links.len());
                node
            }
            None => {
                debug!("Failed to decode root node (encryption_key: {})", encryption_key.is_some());
                return Ok(refs);
            }
        };

        // Find .git directory
        debug!("Root links: {:?}", root_node.links.iter().map(|l| l.name.as_deref()).collect::<Vec<_>>());
        let git_link = root_node.links.iter().find(|l| l.name.as_deref() == Some(".git"));
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
                anyhow::bail!("Failed to download .git directory ({}): {}", &git_hash[..12], e);
            }
        };

        let git_node = match self.decrypt_and_decode(&git_data, git_key.as_ref()) {
            Some(node) => {
                debug!("Decoded .git node with {} links: {:?}", node.links.len(), node.links.iter().map(|l| l.name.as_deref()).collect::<Vec<_>>());
                node
            }
            None => {
                debug!("Failed to decode .git node (key: {})", git_key.is_some());
                return Ok(refs);
            }
        };

        // Find refs directory
        let refs_link = git_node.links.iter().find(|l| l.name.as_deref() == Some("refs"));
        let (refs_hash, refs_key) = match refs_link {
            Some(link) => (hex::encode(link.hash), link.key),
            None => {
                debug!("No refs directory in .git");
                return Ok(refs);
            }
        };

        // Download refs directory
        let refs_data = match self.blossom.try_download(&refs_hash).await {
            Some(data) => data,
            None => {
                debug!("Could not download refs directory");
                return Ok(refs);
            }
        };

        let refs_node = match self.decrypt_and_decode(&refs_data, refs_key.as_ref()) {
            Some(node) => node,
            None => {
                return Ok(refs);
            }
        };

        // Look for HEAD in .git directory
        if let Some(head_link) = git_node.links.iter().find(|l| l.name.as_deref() == Some("HEAD")) {
            let head_hash = hex::encode(head_link.hash);
            if let Some(head_data) = self.blossom.try_download(&head_hash).await {
                // HEAD is a blob, decrypt if needed
                let head_content = if let Some(k) = head_link.key.as_ref() {
                    match decrypt_chk(&head_data, k) {
                        Ok(d) => String::from_utf8_lossy(&d).trim().to_string(),
                        Err(_) => String::from_utf8_lossy(&head_data).trim().to_string(),
                    }
                } else {
                    String::from_utf8_lossy(&head_data).trim().to_string()
                };
                refs.insert("HEAD".to_string(), head_content);
            }
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
            ).await;
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
    ) {
        let dir_data = match self.blossom.try_download(dir_hash).await {
            Some(data) => data,
            None => return,
        };

        let dir_node = match self.decrypt_and_decode(&dir_data, dir_key) {
            Some(node) => node,
            None => return,
        };

        for link in &dir_node.links {
            let name = match &link.name {
                Some(n) => n.clone(),
                None => continue,
            };
            let link_hash = hex::encode(link.hash);
            let ref_path = format!("{}/{}", prefix, name);

            if link.link_type == LinkType::Dir {
                // Recurse into subdirectory
                Box::pin(self.collect_refs_recursive(&link_hash, link.key.as_ref(), &ref_path, refs)).await;
            } else {
                // This is a ref file - read the SHA
                if let Some(ref_data) = self.blossom.try_download(&link_hash).await {
                    // Decrypt if needed
                    let sha = if let Some(k) = link.key.as_ref() {
                        match decrypt_chk(&ref_data, k) {
                            Ok(d) => String::from_utf8_lossy(&d).trim().to_string(),
                            Err(_) => String::from_utf8_lossy(&ref_data).trim().to_string(),
                        }
                    } else {
                        String::from_utf8_lossy(&ref_data).trim().to_string()
                    };
                    if !sha.is_empty() {
                        debug!("Found ref {} -> {}", ref_path, sha);
                        refs.insert(ref_path, sha);
                    }
                }
            }
        }
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
    pub fn publish_repo(&self, repo_name: &str, root_hash: &str, encryption_key: Option<(&[u8; 32], bool, bool)>) -> Result<(String, RelayResult)> {
        let keys = self.keys.as_ref().context(format!(
            "Cannot push: no secret key for {}. You can only push to your own repos.",
            &self.pubkey[..16]
        ))?;

        info!("Publishing repo {} with root hash {} (encrypted: {})",
            repo_name, root_hash, encryption_key.is_some());

        // Create a new multi-threaded runtime for nostr-sdk which spawns background tasks
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("Failed to create tokio runtime")?;

        let result = rt.block_on(self.publish_repo_async(keys, repo_name, root_hash, encryption_key));

        // Give nostr-sdk background tasks time to clean up gracefully
        // This prevents "runtime is shutting down" panics from timer tasks
        rt.shutdown_timeout(std::time::Duration::from_millis(500));

        result
    }

    async fn publish_repo_async(
        &self,
        keys: &Keys,
        repo_name: &str,
        root_hash: &str,
        encryption_key: Option<(&[u8; 32], bool, bool)>,
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
        let connect_timeout = Duration::from_secs(3);
        let start = std::time::Instant::now();
        loop {
            let relays = client.relays().await;
            let mut any_connected = false;
            for (_url, relay) in relays.iter() {
                if relay.is_connected().await {
                    any_connected = true;
                    break;
                }
            }
            if any_connected {
                break;
            }
            if start.elapsed() > connect_timeout {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

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
                let encrypted = nip44::encrypt(
                    keys.secret_key(),
                    &pubkey,
                    &key_hex,
                    nip44::Version::V2
                ).map_err(|e| anyhow::anyhow!("NIP-44 encryption failed: {}", e))?;
                tags.push(Tag::custom(TagKind::custom("selfEncryptedKey"), vec![encrypted]));
            } else if is_link_visible {
                // XOR masked key
                tags.push(Tag::custom(TagKind::custom("encryptedKey"), vec![hex::encode(key)]));
            } else {
                // Public: plaintext CHK
                tags.push(Tag::custom(TagKind::custom("key"), vec![hex::encode(key)]));
            }
        }

        // Add directory prefix labels for discoverability
        // e.g. "docs/travel/doc1" -> ["l", "docs"], ["l", "docs/travel"]
        let parts: Vec<&str> = repo_name.split('/').collect();
        for i in 1..parts.len() {
            let prefix = parts[..i].join("/");
            tags.push(Tag::custom(TagKind::custom("l"), vec![prefix]));
        }

        // Sign the event
        let event = EventBuilder::new(Kind::Custom(KIND_APP_DATA), root_hash, tags)
            .to_event(keys)
            .map_err(|e| anyhow::anyhow!("Failed to sign event: {}", e))?;

        // Send event to connected relays
        match client.send_event(event.clone()).await {
            Ok(output) => {
                // Track which relays confirmed
                for url in output.success.iter() {
                    let url_str = url.to_string();
                    if !connected.contains(&url_str) {
                        connected.push(url_str);
                    }
                }
                // Only mark as failed if we got explicit rejection
                for (url, err) in output.failed.iter() {
                    if err.is_some() {
                        let url_str = url.to_string();
                        if !failed.contains(&url_str) && !connected.contains(&url_str) {
                            failed.push(url_str);
                        }
                    }
                }
                info!("Sent event {} to {} relays ({} failed)", output.id(), output.success.len(), output.failed.len());
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
        let npub_url = keys.public_key().to_bech32()
            .map(|npub| format!("htree://{}/{}", npub, repo_name))
            .unwrap_or_else(|_| format!("htree://{}/{}", &self.pubkey[..16], repo_name));

        // Disconnect and give time for cleanup
        let _ = client.disconnect().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        Ok((npub_url, RelayResult { configured, connected, failed }))
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
mod tests {
    use super::*;

    const TEST_PUBKEY: &str =
        "4523be58d395b1b196a9b8c82b038b6895cb02b683d0c253a955068dba1facd0";

    fn test_config() -> Config {
        Config::default()
    }

    #[test]
    fn test_new_client() {
        let config = test_config();
        let client = NostrClient::new(TEST_PUBKEY, None, None, false, &config).unwrap();
        assert!(!client.relays.is_empty());
        assert!(!client.can_sign());
    }

    #[test]
    fn test_new_client_with_secret() {
        let config = test_config();
        let secret = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let client = NostrClient::new(TEST_PUBKEY, Some(secret.to_string()), None, false, &config).unwrap();
        assert!(client.can_sign());
    }

    #[test]
    fn test_fetch_refs_empty() {
        let config = test_config();
        let mut client = NostrClient::new(TEST_PUBKEY, None, None, false, &config).unwrap();
        // This will timeout/return empty without real relays
        let refs = client.cached_refs.get("new-repo");
        assert!(refs.is_none());
    }

    #[test]
    fn test_update_ref() {
        let config = test_config();
        let mut client = NostrClient::new(TEST_PUBKEY, None, None, false, &config).unwrap();

        client
            .update_ref("repo", "refs/heads/main", "abc123")
            .unwrap();

        let refs = client.cached_refs.get("repo").unwrap();
        assert_eq!(refs.get("refs/heads/main"), Some(&"abc123".to_string()));
    }

    #[test]
    fn test_stored_key_from_hex() {
        let secret = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let key = StoredKey::from_secret_hex(secret, Some("test".to_string())).unwrap();
        assert_eq!(key.secret_hex, secret);
        assert_eq!(key.petname, Some("test".to_string()));
        assert_eq!(key.pubkey_hex.len(), 64);
    }

    #[test]
    fn test_stored_key_from_nsec() {
        // This is a test nsec (don't use in production!)
        let nsec = "nsec1vl029mgpspedva04g90vltkh6fvh240zqtv9k0t9af8935ke9laqsnlfe5";
        let key = StoredKey::from_nsec(nsec, None).unwrap();
        assert_eq!(key.secret_hex.len(), 64);
        assert_eq!(key.pubkey_hex.len(), 64);
    }

    #[test]
    fn test_resolve_identity_hex_pubkey() {
        // Hex pubkey without matching secret returns (pubkey, None)
        let result = resolve_identity(TEST_PUBKEY);
        assert!(result.is_ok());
        let (pubkey, secret) = result.unwrap();
        assert_eq!(pubkey, TEST_PUBKEY);
        // No secret unless we have it in config
        assert!(secret.is_none());
    }

    #[test]
    fn test_resolve_identity_npub() {
        // Create a pubkey from our test hex
        let pk_bytes = hex::decode(TEST_PUBKEY).unwrap();
        let pk = PublicKey::from_slice(&pk_bytes).unwrap();
        let npub = pk.to_bech32().unwrap();

        let result = resolve_identity(&npub);
        assert!(result.is_ok(), "Failed: {:?}", result.err());
        let (pubkey, _) = result.unwrap();
        // Should be valid hex pubkey
        assert_eq!(pubkey.len(), 64);
        assert_eq!(pubkey, TEST_PUBKEY);
    }

    #[test]
    fn test_resolve_identity_unknown_petname() {
        let result = resolve_identity("nonexistent_petname_xyz");
        assert!(result.is_err());
    }

    /// Verify that private repo encryption (NIP-44) produces ciphertext, not plaintext CHK
    #[test]
    fn test_private_key_is_nip44_encrypted_not_plaintext() {
        use nostr_sdk::prelude::{Keys, nip44};

        // Create test keys
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        // Test CHK key (32 bytes)
        let chk_key: [u8; 32] = [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,
        ];
        let plaintext_hex = hex::encode(&chk_key);

        // Encrypt with NIP-44 (same as publish_repo does for is_self_private=true)
        let encrypted = nip44::encrypt(
            keys.secret_key(),
            &pubkey,
            &plaintext_hex,
            nip44::Version::V2
        ).expect("NIP-44 encryption should succeed");

        // Critical security check: encrypted value must NOT be plaintext
        assert_ne!(
            encrypted, plaintext_hex,
            "NIP-44 encrypted value must differ from plaintext CHK hex"
        );

        // Encrypted value should not contain the raw hex (even as substring)
        assert!(
            !encrypted.contains(&plaintext_hex),
            "Encrypted value should not contain plaintext hex"
        );

        // Verify we can decrypt it back (round-trip)
        let decrypted = nip44::decrypt(
            keys.secret_key(),
            &pubkey,
            &encrypted
        ).expect("NIP-44 decryption should succeed");

        assert_eq!(
            decrypted, plaintext_hex,
            "Decrypted value should match original plaintext hex"
        );
    }

    /// Verify that different encryption modes produce different tag values
    #[test]
    fn test_encryption_modes_produce_different_values() {
        use nostr_sdk::prelude::{Keys, nip44};

        let keys = Keys::generate();
        let pubkey = keys.public_key();

        // Test CHK key
        let chk_key: [u8; 32] = [0xaa; 32];
        let plaintext_hex = hex::encode(&chk_key);

        // Mode 1: Public (plaintext hex)
        let public_value = plaintext_hex.clone();

        // Mode 2: Link-visible (XOR masked - in practice, the key passed to publish_repo
        // is already XOR'd with url_secret, so we just store hex of that)
        let link_visible_value = plaintext_hex.clone(); // Same format, different actual key

        // Mode 3: Private (NIP-44 encrypted)
        let private_value = nip44::encrypt(
            keys.secret_key(),
            &pubkey,
            &plaintext_hex,
            nip44::Version::V2
        ).expect("NIP-44 encryption should succeed");

        // Private value must be different from public
        assert_ne!(
            private_value, public_value,
            "Private (NIP-44) value must differ from public (plaintext) value"
        );

        // Private value is base64 (NIP-44 output), not hex
        assert!(
            private_value.len() != 64,
            "NIP-44 output should not be 64 chars like hex CHK"
        );
    }
}
