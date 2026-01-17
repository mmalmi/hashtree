use anyhow::{Context, Result};
use nostr::nips::nip19::{FromBech32, ToBech32};
use nostr::{Keys, SecretKey};
use serde::{Deserialize, Serialize};
use std::fs;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub nostr: NostrConfig,
    #[serde(default)]
    pub blossom: BlossomConfig,
    #[serde(default)]
    pub sync: SyncConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_bind_address")]
    pub bind_address: String,
    #[serde(default = "default_enable_auth")]
    pub enable_auth: bool,
    /// Port for the built-in STUN server (0 = disabled)
    #[serde(default = "default_stun_port")]
    pub stun_port: u16,
    /// Enable WebRTC P2P connections
    #[serde(default = "default_enable_webrtc")]
    pub enable_webrtc: bool,
    /// Allow anyone with valid Nostr auth to write (default: true)
    /// When false, only social graph members can write
    #[serde(default = "default_public_writes")]
    pub public_writes: bool,
}

fn default_public_writes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
    #[serde(default = "default_max_size_gb")]
    pub max_size_gb: u64,
    /// Optional S3/R2 backend for blob storage
    #[serde(default)]
    pub s3: Option<S3Config>,
}

/// S3-compatible storage configuration (works with AWS S3, Cloudflare R2, MinIO, etc.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct S3Config {
    /// S3 endpoint URL (e.g., "https://<account_id>.r2.cloudflarestorage.com" for R2)
    pub endpoint: String,
    /// S3 bucket name
    pub bucket: String,
    /// Optional key prefix for all blobs (e.g., "blobs/")
    #[serde(default)]
    pub prefix: Option<String>,
    /// AWS region (use "auto" for R2)
    #[serde(default = "default_s3_region")]
    pub region: String,
    /// Access key ID (can also be set via AWS_ACCESS_KEY_ID env var)
    #[serde(default)]
    pub access_key: Option<String>,
    /// Secret access key (can also be set via AWS_SECRET_ACCESS_KEY env var)
    #[serde(default)]
    pub secret_key: Option<String>,
    /// Public URL for serving blobs (optional, for generating public URLs)
    #[serde(default)]
    pub public_url: Option<String>,
}

fn default_s3_region() -> String {
    "auto".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NostrConfig {
    #[serde(default = "default_relays")]
    pub relays: Vec<String>,
    /// List of npubs allowed to write (blossom uploads). If empty, uses public_writes setting.
    #[serde(default)]
    pub allowed_npubs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlossomConfig {
    /// File servers for push/pull (legacy, both read and write)
    #[serde(default)]
    pub servers: Vec<String>,
    /// Read-only file servers (fallback for fetching content)
    #[serde(default = "default_read_servers")]
    pub read_servers: Vec<String>,
    /// Write-enabled file servers (for uploading)
    #[serde(default = "default_write_servers")]
    pub write_servers: Vec<String>,
    /// Maximum upload size in MB (default: 5)
    #[serde(default = "default_max_upload_mb")]
    pub max_upload_mb: u64,
}

// Keep in sync with hashtree-config/src/lib.rs
fn default_read_servers() -> Vec<String> {
    vec!["https://cdn.iris.to".to_string(), "https://hashtree.iris.to".to_string()]
}

fn default_write_servers() -> Vec<String> {
    vec!["https://upload.iris.to".to_string()]
}

fn default_max_upload_mb() -> u64 {
    5
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncConfig {
    /// Enable background sync (auto-pull trees)
    #[serde(default = "default_sync_enabled")]
    pub enabled: bool,
    /// Sync own trees (subscribed via Nostr)
    #[serde(default = "default_sync_own")]
    pub sync_own: bool,
    /// Sync followed users' public trees
    #[serde(default = "default_sync_followed")]
    pub sync_followed: bool,
    /// Max concurrent sync tasks
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
    /// WebRTC request timeout in milliseconds
    #[serde(default = "default_webrtc_timeout_ms")]
    pub webrtc_timeout_ms: u64,
    /// Blossom request timeout in milliseconds
    #[serde(default = "default_blossom_timeout_ms")]
    pub blossom_timeout_ms: u64,
}


fn default_sync_enabled() -> bool {
    true
}

fn default_sync_own() -> bool {
    true
}

fn default_sync_followed() -> bool {
    true
}

fn default_max_concurrent() -> usize {
    3
}

fn default_webrtc_timeout_ms() -> u64 {
    2000
}

fn default_blossom_timeout_ms() -> u64 {
    10000
}

fn default_relays() -> Vec<String> {
    vec![
        "wss://relay.damus.io".to_string(),
        "wss://relay.snort.social".to_string(),
        "wss://nos.lol".to_string(),
        "wss://temp.iris.to".to_string(),
    ]
}

fn default_bind_address() -> String {
    "127.0.0.1:8080".to_string()
}

fn default_enable_auth() -> bool {
    true
}

fn default_stun_port() -> u16 {
    3478 // Standard STUN port (RFC 5389)
}

fn default_enable_webrtc() -> bool {
    true
}

fn default_data_dir() -> String {
    hashtree_config::get_hashtree_dir()
        .join("data")
        .to_string_lossy()
        .to_string()
}

fn default_max_size_gb() -> u64 {
    10
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind_address: default_bind_address(),
            enable_auth: default_enable_auth(),
            stun_port: default_stun_port(),
            enable_webrtc: default_enable_webrtc(),
            public_writes: default_public_writes(),
        }
    }
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            max_size_gb: default_max_size_gb(),
            s3: None,
        }
    }
}

impl Default for NostrConfig {
    fn default() -> Self {
        Self {
            relays: default_relays(),
            allowed_npubs: Vec::new(),
        }
    }
}

impl Default for BlossomConfig {
    fn default() -> Self {
        Self {
            servers: Vec::new(),
            read_servers: default_read_servers(),
            write_servers: default_write_servers(),
            max_upload_mb: default_max_upload_mb(),
        }
    }
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            enabled: default_sync_enabled(),
            sync_own: default_sync_own(),
            sync_followed: default_sync_followed(),
            max_concurrent: default_max_concurrent(),
            webrtc_timeout_ms: default_webrtc_timeout_ms(),
            blossom_timeout_ms: default_blossom_timeout_ms(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            storage: StorageConfig::default(),
            nostr: NostrConfig::default(),
            blossom: BlossomConfig::default(),
            sync: SyncConfig::default(),
        }
    }
}

impl Config {
    /// Load config from file, or create default if doesn't exist
    pub fn load() -> Result<Self> {
        let config_path = get_config_path();

        if config_path.exists() {
            let content = fs::read_to_string(&config_path)
                .context("Failed to read config file")?;
            toml::from_str(&content).context("Failed to parse config file")
        } else {
            let config = Config::default();
            config.save()?;
            Ok(config)
        }
    }

    /// Save config to file
    pub fn save(&self) -> Result<()> {
        let config_path = get_config_path();

        // Ensure parent directory exists
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let content = toml::to_string_pretty(self)?;
        fs::write(&config_path, content)?;

        Ok(())
    }
}

// Re-export path functions from hashtree_config
pub use hashtree_config::{get_auth_cookie_path, get_config_path, get_hashtree_dir, get_keys_path};

/// Generate and save auth cookie if it doesn't exist
pub fn ensure_auth_cookie() -> Result<(String, String)> {
    let cookie_path = get_auth_cookie_path();

    if cookie_path.exists() {
        read_auth_cookie()
    } else {
        generate_auth_cookie()
    }
}

/// Read existing auth cookie
pub fn read_auth_cookie() -> Result<(String, String)> {
    let cookie_path = get_auth_cookie_path();
    let content = fs::read_to_string(&cookie_path)
        .context("Failed to read auth cookie")?;

    let parts: Vec<&str> = content.trim().split(':').collect();
    if parts.len() != 2 {
        anyhow::bail!("Invalid auth cookie format");
    }

    Ok((parts[0].to_string(), parts[1].to_string()))
}

/// Ensure keys file exists, generating one if not present
/// Returns (Keys, was_generated)
pub fn ensure_keys() -> Result<(Keys, bool)> {
    let keys_path = get_keys_path();

    if keys_path.exists() {
        let content = fs::read_to_string(&keys_path)
            .context("Failed to read keys file")?;
        let entries = hashtree_config::parse_keys_file(&content);
        let nsec_str = entries.into_iter().next()
            .map(|e| e.secret)
            .context("Keys file is empty")?;
        let secret_key = SecretKey::from_bech32(&nsec_str)
            .context("Invalid nsec format")?;
        let keys = Keys::new(secret_key);
        Ok((keys, false))
    } else {
        let keys = generate_keys()?;
        Ok((keys, true))
    }
}

/// Read existing keys
pub fn read_keys() -> Result<Keys> {
    let keys_path = get_keys_path();
    let content = fs::read_to_string(&keys_path)
        .context("Failed to read keys file")?;
    let entries = hashtree_config::parse_keys_file(&content);
    let nsec_str = entries.into_iter().next()
        .map(|e| e.secret)
        .context("Keys file is empty")?;
    let secret_key = SecretKey::from_bech32(&nsec_str)
        .context("Invalid nsec format")?;
    Ok(Keys::new(secret_key))
}

/// Get nsec string, ensuring keys file exists (generate if needed)
/// Returns (nsec_string, was_generated)
pub fn ensure_keys_string() -> Result<(String, bool)> {
    let keys_path = get_keys_path();

    if keys_path.exists() {
        let content = fs::read_to_string(&keys_path)
            .context("Failed to read keys file")?;
        let entries = hashtree_config::parse_keys_file(&content);
        let nsec_str = entries.into_iter().next()
            .map(|e| e.secret)
            .context("Keys file is empty")?;
        Ok((nsec_str, false))
    } else {
        let keys = generate_keys()?;
        let nsec = keys.secret_key().to_bech32()
            .context("Failed to encode nsec")?;
        Ok((nsec, true))
    }
}

/// Generate new keys and save to file
pub fn generate_keys() -> Result<Keys> {
    let keys_path = get_keys_path();

    // Ensure parent directory exists
    if let Some(parent) = keys_path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Generate new keys
    let keys = Keys::generate();
    let nsec = keys.secret_key().to_bech32()
        .context("Failed to encode nsec")?;

    // Save to file
    fs::write(&keys_path, &nsec)?;

    // Set permissions to 0600 (owner read/write only)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o600);
        fs::set_permissions(&keys_path, perms)?;
    }

    Ok(keys)
}

/// Get 32-byte pubkey bytes from Keys (for nostrdb)
pub fn pubkey_bytes(keys: &Keys) -> [u8; 32] {
    keys.public_key().to_bytes()
}

/// Parse npub to 32-byte pubkey
pub fn parse_npub(npub: &str) -> Result<[u8; 32]> {
    use nostr::PublicKey;
    let pk = PublicKey::from_bech32(npub)
        .context("Invalid npub format")?;
    Ok(pk.to_bytes())
}

/// Generate new random auth cookie
pub fn generate_auth_cookie() -> Result<(String, String)> {
    use rand::Rng;

    let cookie_path = get_auth_cookie_path();

    // Ensure parent directory exists
    if let Some(parent) = cookie_path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Generate random credentials
    let mut rng = rand::thread_rng();
    let username = format!("htree_{}", rng.gen::<u32>());
    let password: String = (0..32)
        .map(|_| {
            let idx = rng.gen_range(0..62);
            match idx {
                0..=25 => (b'a' + idx) as char,
                26..=51 => (b'A' + (idx - 26)) as char,
                _ => (b'0' + (idx - 52)) as char,
            }
        })
        .collect();

    // Save to file
    let content = format!("{}:{}", username, password);
    fs::write(&cookie_path, content)?;

    // Set permissions to 0600 (owner read/write only)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o600);
        fs::set_permissions(&cookie_path, perms)?;
    }

    Ok((username, password))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_config_default() {
        let config = Config::default();
        assert_eq!(config.server.bind_address, "127.0.0.1:8080");
        assert_eq!(config.server.enable_auth, true);
        assert_eq!(config.storage.max_size_gb, 10);
    }

    #[test]
    fn test_auth_cookie_generation() -> Result<()> {
        let temp_dir = TempDir::new()?;

        // Mock the cookie path
        std::env::set_var("HOME", temp_dir.path());

        let (username, password) = generate_auth_cookie()?;

        assert!(username.starts_with("htree_"));
        assert_eq!(password.len(), 32);

        // Verify cookie file exists
        let cookie_path = get_auth_cookie_path();
        assert!(cookie_path.exists());

        // Verify reading works
        let (u2, p2) = read_auth_cookie()?;
        assert_eq!(username, u2);
        assert_eq!(password, p2);

        Ok(())
    }
}
