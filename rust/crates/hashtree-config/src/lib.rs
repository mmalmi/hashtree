//! Shared configuration for hashtree tools
//!
//! Reads from ~/.hashtree/config.toml

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// Default read-only file servers
pub const DEFAULT_READ_SERVERS: &[&str] = &[
    "https://cdn.iris.to",
    "https://hashtree.iris.to",
];

/// Default write-enabled file servers
pub const DEFAULT_WRITE_SERVERS: &[&str] = &[
    "https://upload.iris.to",
];

/// Default nostr relays
pub const DEFAULT_RELAYS: &[&str] = &[
    "wss://temp.iris.to",
    "wss://relay.damus.io",
    "wss://nos.lol",
    "wss://relay.primal.net",
];

/// Top-level config structure
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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

/// Server configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_bind_address")]
    pub bind_address: String,
    #[serde(default = "default_true")]
    pub enable_auth: bool,
    #[serde(default)]
    pub public_writes: bool,
    #[serde(default)]
    pub enable_webrtc: bool,
    #[serde(default)]
    pub stun_port: u16,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind_address: default_bind_address(),
            enable_auth: true,
            public_writes: false,
            enable_webrtc: false,
            stun_port: 0,
        }
    }
}

fn default_bind_address() -> String {
    "127.0.0.1:8080".to_string()
}

fn default_true() -> bool {
    true
}

/// Storage backend type
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StorageBackend {
    /// Filesystem storage (default) - stores in ~/.hashtree/blobs/{prefix}/{hash}
    Fs,
    /// LMDB storage - requires lmdb feature
    Lmdb,
}

impl Default for StorageBackend {
    fn default() -> Self {
        Self::Fs
    }
}

/// Storage configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    /// Storage backend: "fs" (default) or "lmdb"
    #[serde(default)]
    pub backend: StorageBackend,
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
    #[serde(default = "default_max_size_gb")]
    pub max_size_gb: u64,
    #[serde(default)]
    pub s3: Option<S3Config>,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            backend: StorageBackend::default(),
            data_dir: default_data_dir(),
            max_size_gb: default_max_size_gb(),
            s3: None,
        }
    }
}

fn default_data_dir() -> String {
    get_hashtree_dir()
        .join("data")
        .to_string_lossy()
        .to_string()
}

fn default_max_size_gb() -> u64 {
    10
}

/// S3-compatible storage configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct S3Config {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    #[serde(default)]
    pub prefix: Option<String>,
}

/// Nostr relay configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NostrConfig {
    #[serde(default = "default_relays")]
    pub relays: Vec<String>,
    #[serde(default)]
    pub allowed_npubs: Vec<String>,
}

impl Default for NostrConfig {
    fn default() -> Self {
        Self {
            relays: default_relays(),
            allowed_npubs: vec![],
        }
    }
}

fn default_relays() -> Vec<String> {
    DEFAULT_RELAYS.iter().map(|s| s.to_string()).collect()
}

/// File server (blossom) configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlossomConfig {
    /// Legacy servers field (both read and write)
    #[serde(default)]
    pub servers: Vec<String>,
    /// Read-only file servers
    #[serde(default = "default_read_servers")]
    pub read_servers: Vec<String>,
    /// Write-enabled file servers
    #[serde(default = "default_write_servers")]
    pub write_servers: Vec<String>,
    /// Max upload size in MB
    #[serde(default = "default_max_upload_mb")]
    pub max_upload_mb: u64,
    /// Force upload all blobs, skipping "server already has" check
    #[serde(default)]
    pub force_upload: bool,
}

impl Default for BlossomConfig {
    fn default() -> Self {
        Self {
            servers: vec![],
            read_servers: default_read_servers(),
            write_servers: default_write_servers(),
            max_upload_mb: default_max_upload_mb(),
            force_upload: false,
        }
    }
}

fn default_read_servers() -> Vec<String> {
    DEFAULT_READ_SERVERS.iter().map(|s| s.to_string()).collect()
}

fn default_write_servers() -> Vec<String> {
    DEFAULT_WRITE_SERVERS.iter().map(|s| s.to_string()).collect()
}

fn default_max_upload_mb() -> u64 {
    100
}

impl BlossomConfig {
    /// Get all read servers (legacy + read_servers)
    pub fn all_read_servers(&self) -> Vec<String> {
        let mut servers = self.servers.clone();
        servers.extend(self.read_servers.clone());
        servers.sort();
        servers.dedup();
        servers
    }

    /// Get all write servers (legacy + write_servers)
    pub fn all_write_servers(&self) -> Vec<String> {
        let mut servers = self.servers.clone();
        servers.extend(self.write_servers.clone());
        servers.sort();
        servers.dedup();
        servers
    }
}

/// Background sync configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub sync_own: bool,
    #[serde(default)]
    pub sync_followed: bool,
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
    #[serde(default = "default_webrtc_timeout_ms")]
    pub webrtc_timeout_ms: u64,
    #[serde(default = "default_blossom_timeout_ms")]
    pub blossom_timeout_ms: u64,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            sync_own: true,
            sync_followed: false,
            max_concurrent: default_max_concurrent(),
            webrtc_timeout_ms: default_webrtc_timeout_ms(),
            blossom_timeout_ms: default_blossom_timeout_ms(),
        }
    }
}

fn default_max_concurrent() -> usize {
    4
}

fn default_webrtc_timeout_ms() -> u64 {
    5000
}

fn default_blossom_timeout_ms() -> u64 {
    10000
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

    /// Load config, returning default on any error (no panic)
    pub fn load_or_default() -> Self {
        Self::load().unwrap_or_default()
    }

    /// Save config to file
    pub fn save(&self) -> Result<()> {
        let config_path = get_config_path();

        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let content = toml::to_string_pretty(self)?;
        fs::write(&config_path, content)?;

        Ok(())
    }
}

/// Get the hashtree directory (~/.hashtree)
pub fn get_hashtree_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("HTREE_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".hashtree")
}

/// Get the config file path (~/.hashtree/config.toml)
pub fn get_config_path() -> PathBuf {
    get_hashtree_dir().join("config.toml")
}

/// Get the keys file path (~/.hashtree/keys)
pub fn get_keys_path() -> PathBuf {
    get_hashtree_dir().join("keys")
}

/// A stored key entry from the keys file
#[derive(Debug, Clone)]
pub struct KeyEntry {
    /// The nsec or hex secret key
    pub secret: String,
    /// Optional alias/petname
    pub alias: Option<String>,
}

/// Parse the keys file content into key entries
/// Format: `nsec1... [alias]` or `hex... [alias]` per line
/// Lines starting with # are comments
pub fn parse_keys_file(content: &str) -> Vec<KeyEntry> {
    let mut entries = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.splitn(2, ' ').collect();
        let secret = parts[0].to_string();
        let alias = parts.get(1).map(|s| s.trim().to_string());
        entries.push(KeyEntry { secret, alias });
    }
    entries
}

/// Read and parse keys file, returning the first key's secret
/// Returns None if file doesn't exist or is empty
pub fn read_first_key() -> Option<String> {
    let keys_path = get_keys_path();
    let content = std::fs::read_to_string(&keys_path).ok()?;
    let entries = parse_keys_file(&content);
    entries.into_iter().next().map(|e| e.secret)
}

/// Get the auth cookie path (~/.hashtree/auth.cookie)
pub fn get_auth_cookie_path() -> PathBuf {
    get_hashtree_dir().join("auth.cookie")
}

/// Get the data directory from config (defaults to ~/.hashtree/data)
/// Can be overridden with HTREE_DATA_DIR environment variable
pub fn get_data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("HTREE_DATA_DIR") {
        return PathBuf::from(dir);
    }
    let config = Config::load_or_default();
    PathBuf::from(&config.storage.data_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert!(!config.blossom.read_servers.is_empty());
        assert!(!config.blossom.write_servers.is_empty());
        assert!(!config.nostr.relays.is_empty());
    }

    #[test]
    fn test_parse_empty_config() {
        let config: Config = toml::from_str("").unwrap();
        assert!(!config.blossom.read_servers.is_empty());
    }

    #[test]
    fn test_parse_partial_config() {
        let toml = r#"
[blossom]
write_servers = ["https://custom.server"]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.blossom.write_servers, vec!["https://custom.server"]);
        assert!(!config.blossom.read_servers.is_empty());
    }

    #[test]
    fn test_all_servers() {
        let mut config = BlossomConfig::default();
        config.servers = vec!["https://legacy.server".to_string()];

        let read = config.all_read_servers();
        assert!(read.contains(&"https://legacy.server".to_string()));
        assert!(read.contains(&"https://cdn.iris.to".to_string()));

        let write = config.all_write_servers();
        assert!(write.contains(&"https://legacy.server".to_string()));
        assert!(write.contains(&"https://upload.iris.to".to_string()));
    }

    #[test]
    fn test_storage_backend_default() {
        let config = Config::default();
        assert_eq!(config.storage.backend, StorageBackend::Fs);
    }

    #[test]
    fn test_storage_backend_lmdb() {
        let toml = r#"
[storage]
backend = "lmdb"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.storage.backend, StorageBackend::Lmdb);
    }

    #[test]
    fn test_storage_backend_fs_explicit() {
        let toml = r#"
[storage]
backend = "fs"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.storage.backend, StorageBackend::Fs);
    }

    #[test]
    fn test_parse_keys_file() {
        let content = r#"
nsec1abc123 self
# comment line
nsec1def456 work

nsec1ghi789
"#;
        let entries = parse_keys_file(content);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].secret, "nsec1abc123");
        assert_eq!(entries[0].alias, Some("self".to_string()));
        assert_eq!(entries[1].secret, "nsec1def456");
        assert_eq!(entries[1].alias, Some("work".to_string()));
        assert_eq!(entries[2].secret, "nsec1ghi789");
        assert_eq!(entries[2].alias, None);
    }
}
