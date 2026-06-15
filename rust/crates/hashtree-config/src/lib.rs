//! Shared configuration for hashtree tools
//!
//! Reads from ~/.hashtree/config.toml

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// Default read-only file servers
pub const DEFAULT_READ_SERVERS: &[&str] = &["https://cdn.iris.to", "https://blossom.primal.net"];

/// Default write-enabled file servers
pub const DEFAULT_WRITE_SERVERS: &[&str] = &["https://upload.iris.to"];

/// Default nostr relays
pub const DEFAULT_RELAYS: &[&str] = &[
    "wss://temp.iris.to",
    "wss://nos.lol",
    "wss://relay.snort.social",
    "wss://relay.primal.net",
];

/// Default social graph entrypoint followed when a new identity is initialized.
pub const DEFAULT_SOCIALGRAPH_ENTRYPOINT_NPUB: &str =
    "npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm";

/// Default read-only alias for the social graph entrypoint.
pub const DEFAULT_SOCIALGRAPH_ENTRYPOINT_ALIAS: &str = "siriusbusiness";

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
    #[serde(default)]
    pub updater: UpdaterConfig,
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StorageBackend {
    /// LMDB storage - requires lmdb feature
    #[default]
    Lmdb,
    /// Filesystem storage - stores in ~/.hashtree/blobs/{prefix}/{subdir}/{hash}
    Fs,
}

/// Storage configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    /// Storage backend: "lmdb" (default) or "fs"
    #[serde(default)]
    pub backend: StorageBackend,
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
    #[serde(default = "default_max_size_gb")]
    pub max_size_gb: u64,
    #[serde(default = "default_storage_evict_orphans")]
    pub evict_orphans: bool,
    #[serde(default)]
    pub s3: Option<S3Config>,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            backend: StorageBackend::default(),
            data_dir: default_data_dir(),
            max_size_gb: default_max_size_gb(),
            evict_orphans: default_storage_evict_orphans(),
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

fn default_storage_evict_orphans() -> bool {
    true
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
    #[serde(default)]
    pub socialgraph_root: Option<String>,
    #[serde(default = "default_nostr_bootstrap_follows")]
    pub bootstrap_follows: Vec<String>,
    #[serde(default = "default_social_graph_crawl_depth", alias = "crawl_depth")]
    pub social_graph_crawl_depth: u32,
    /// Max follow distance to mirror into public event/profile indexes.
    /// Defaults to social_graph_crawl_depth when unset.
    #[serde(default)]
    pub mirror_max_follow_distance: Option<u32>,
    #[serde(default = "default_max_write_distance")]
    pub max_write_distance: u32,
    /// Max size for the trusted social graph store in GB (default: 10)
    #[serde(default = "default_nostr_db_max_size_gb")]
    pub db_max_size_gb: u64,
    /// Max size for the social graph spambox store in GB (default: 1)
    /// Set to 0 for memory-only spambox (no on-disk DB)
    #[serde(default = "default_nostr_spambox_max_size_gb")]
    pub spambox_max_size_gb: u64,
    /// Require relays to support NIP-77 negentropy for mirror history sync.
    #[serde(default)]
    pub negentropy_only: bool,
    /// Threshold for treating a user as overmuted in mirrored profile indexing/search.
    #[serde(default = "default_nostr_overmute_threshold")]
    pub overmute_threshold: f64,
    /// Kinds mirrored from upstream relays for the trusted hashtree index.
    #[serde(default = "default_nostr_mirror_kinds")]
    pub mirror_kinds: Vec<u16>,
    /// How many graph authors to reconcile before checkpointing the mirror root.
    #[serde(default = "default_nostr_history_sync_author_chunk_size")]
    pub history_sync_author_chunk_size: usize,
    /// Run a catch-up history sync after relay reconnects.
    #[serde(default = "default_nostr_history_sync_on_reconnect")]
    pub history_sync_on_reconnect: bool,
}

impl Default for NostrConfig {
    fn default() -> Self {
        Self {
            relays: default_relays(),
            allowed_npubs: vec![],
            socialgraph_root: None,
            bootstrap_follows: default_nostr_bootstrap_follows(),
            social_graph_crawl_depth: default_social_graph_crawl_depth(),
            mirror_max_follow_distance: None,
            max_write_distance: default_max_write_distance(),
            db_max_size_gb: default_nostr_db_max_size_gb(),
            spambox_max_size_gb: default_nostr_spambox_max_size_gb(),
            negentropy_only: false,
            overmute_threshold: default_nostr_overmute_threshold(),
            mirror_kinds: default_nostr_mirror_kinds(),
            history_sync_author_chunk_size: default_nostr_history_sync_author_chunk_size(),
            history_sync_on_reconnect: default_nostr_history_sync_on_reconnect(),
        }
    }
}

fn default_social_graph_crawl_depth() -> u32 {
    2
}

fn default_nostr_bootstrap_follows() -> Vec<String> {
    vec![DEFAULT_SOCIALGRAPH_ENTRYPOINT_NPUB.to_string()]
}

fn default_nostr_overmute_threshold() -> f64 {
    1.0
}

fn default_max_write_distance() -> u32 {
    3
}

fn default_nostr_db_max_size_gb() -> u64 {
    10
}

fn default_nostr_spambox_max_size_gb() -> u64 {
    1
}

fn default_nostr_history_sync_on_reconnect() -> bool {
    true
}

fn default_nostr_mirror_kinds() -> Vec<u16> {
    vec![0, 1, 3, 6, 7, 9_735, 30_023]
}

fn default_nostr_history_sync_author_chunk_size() -> usize {
    5_000
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
    /// Require public Blossom and peer-fetched cached blobs to look encrypted.
    #[serde(default = "default_require_random_untrusted_ingest")]
    pub require_random_untrusted_ingest: bool,
    /// Max number of concurrent blob uploads during push/repair.
    #[serde(default = "default_upload_concurrency")]
    pub upload_concurrency: usize,
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
            require_random_untrusted_ingest: default_require_random_untrusted_ingest(),
            upload_concurrency: default_upload_concurrency(),
            force_upload: false,
        }
    }
}

fn default_read_servers() -> Vec<String> {
    let mut servers: Vec<String> = DEFAULT_READ_SERVERS.iter().map(|s| s.to_string()).collect();
    servers.sort();
    servers
}

fn default_write_servers() -> Vec<String> {
    DEFAULT_WRITE_SERVERS
        .iter()
        .map(|s| s.to_string())
        .collect()
}

fn default_max_upload_mb() -> u64 {
    100
}

fn default_require_random_untrusted_ingest() -> bool {
    true
}

fn default_upload_concurrency() -> usize {
    10
}

impl BlossomConfig {
    /// Get all readable servers (legacy + read_servers + write_servers).
    /// Write servers are included because freshly published immutable content
    /// may be available there before it has replicated to dedicated read tiers.
    pub fn all_read_servers(&self) -> Vec<String> {
        let mut servers = self.servers.clone();
        servers.extend(self.read_servers.clone());
        servers.extend(self.write_servers.clone());
        if servers.is_empty() {
            servers = default_read_servers();
            servers.extend(default_write_servers());
        }
        servers.sort();
        servers.dedup();
        servers
    }

    /// Get all write servers (legacy + write_servers)
    pub fn all_write_servers(&self) -> Vec<String> {
        let mut servers = self.servers.clone();
        servers.extend(self.write_servers.clone());
        if servers.is_empty() {
            servers = default_write_servers();
        }
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

/// htree self-update preferences. Reads `[updater]` from
/// `~/.hashtree/config.toml`. Auto-check is on by default — `htree` will
/// quietly check for a newer published binary at most once per
/// `check_interval_hours` and print a one-liner to stderr when one exists.
/// Auto-install is off by default; flip it on to install the new binary
/// in the background and print the result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdaterConfig {
    #[serde(default = "default_true")]
    pub auto_check: bool,
    #[serde(default)]
    pub auto_install: bool,
    #[serde(default = "default_check_interval_hours")]
    pub check_interval_hours: u32,
}

impl Default for UpdaterConfig {
    fn default() -> Self {
        Self {
            auto_check: true,
            auto_install: false,
            check_interval_hours: default_check_interval_hours(),
        }
    }
}

fn default_check_interval_hours() -> u32 {
    24
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
            let content = fs::read_to_string(&config_path).context("Failed to read config file")?;
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

/// Get the public alias file path (~/.hashtree/aliases)
pub fn get_aliases_path() -> PathBuf {
    get_hashtree_dir().join("aliases")
}

/// A stored key entry from the keys file
#[derive(Debug, Clone)]
pub struct KeyEntry {
    /// The raw identity token (for example nsec, npub, or hex)
    pub secret: String,
    /// Optional alias/petname
    pub alias: Option<String>,
}

/// Parse the keys file content into key entries
/// Format: `<identity> [alias]` per line
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

/// Detect a local hashtree daemon on localhost and return its Blossom base URL.
pub fn detect_local_daemon_url(bind_address: Option<&str>) -> Option<String> {
    use std::net::{SocketAddr, TcpStream};
    use std::time::Duration;

    if !prefer_local_daemon() {
        return None;
    }

    let port = local_daemon_port(bind_address);
    if port == 0 {
        return None;
    }

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let timeout = Duration::from_millis(100);
    TcpStream::connect_timeout(&addr, timeout).ok()?;
    Some(format!("http://127.0.0.1:{}", port))
}

/// Detect local Nostr relay URLs (e.g., hashtree daemon or local relay on common ports).
pub fn detect_local_relay_urls(bind_address: Option<&str>) -> Vec<String> {
    let mut relays = Vec::new();

    if let Some(list) =
        parse_env_list("NOSTR_LOCAL_RELAY").or_else(|| parse_env_list("HTREE_LOCAL_RELAY"))
    {
        for raw in list {
            if let Some(url) = normalize_relay_url(&raw) {
                relays.push(url);
            }
        }
    }

    if let Some(base) = detect_local_daemon_url(bind_address) {
        if let Some(ws) = normalize_relay_url(&base) {
            let ws = ws.trim_end_matches('/');
            let ws = if ws.contains("/ws") {
                ws.to_string()
            } else {
                format!("{}/ws", ws)
            };
            relays.push(ws);
        }
    }

    let mut ports = parse_env_ports("NOSTR_LOCAL_RELAY_PORTS");
    if ports.is_empty() {
        ports.push(4869);
    }

    let daemon_port = local_daemon_port(bind_address);
    for port in ports {
        if port == 0 || port == daemon_port {
            continue;
        }
        if local_port_open(port) {
            relays.push(format!("ws://127.0.0.1:{port}"));
        }
    }

    dedupe_relays(relays)
}

/// Resolve relays using environment overrides and optional local relay discovery.
pub fn resolve_relays(config_relays: &[String], bind_address: Option<&str>) -> Vec<String> {
    let mut base = match parse_env_list("NOSTR_RELAYS") {
        Some(list) => list,
        None => config_relays.to_vec(),
    };

    base = base
        .into_iter()
        .filter_map(|r| normalize_relay_url(&r))
        .collect();

    if !prefer_local_relay() {
        return dedupe_relays(base);
    }

    let mut combined = detect_local_relay_urls(bind_address);
    combined.extend(base);
    dedupe_relays(combined)
}

fn local_daemon_port(bind_address: Option<&str>) -> u16 {
    let default_port = 8080;
    let Some(addr) = bind_address else {
        return default_port;
    };
    if let Ok(sock) = addr.parse::<std::net::SocketAddr>() {
        return sock.port();
    }
    if let Some((_, port_str)) = addr.rsplit_once(':') {
        if let Ok(port) = port_str.parse::<u16>() {
            return port;
        }
    }
    default_port
}

fn prefer_local_relay() -> bool {
    for key in ["NOSTR_PREFER_LOCAL", "HTREE_PREFER_LOCAL_RELAY"] {
        if let Ok(val) = std::env::var(key) {
            let val = val.trim().to_lowercase();
            return !matches!(val.as_str(), "0" | "false" | "no" | "off");
        }
    }
    true
}

fn prefer_local_daemon() -> bool {
    for key in [
        "HTREE_PREFER_LOCAL_DAEMON",
        "NOSTR_PREFER_LOCAL",
        "HTREE_PREFER_LOCAL_RELAY",
    ] {
        if let Ok(val) = std::env::var(key) {
            let val = val.trim().to_lowercase();
            return !matches!(val.as_str(), "0" | "false" | "no" | "off");
        }
    }
    false
}

fn parse_env_list(var: &str) -> Option<Vec<String>> {
    let value = std::env::var(var).ok()?;
    let mut items = Vec::new();
    for part in value.split([',', ';', '\n', '\t', ' ']) {
        let trimmed = part.trim();
        if !trimmed.is_empty() {
            items.push(trimmed.to_string());
        }
    }
    if items.is_empty() {
        None
    } else {
        Some(items)
    }
}

fn parse_env_ports(var: &str) -> Vec<u16> {
    let Some(list) = parse_env_list(var) else {
        return Vec::new();
    };
    list.into_iter()
        .filter_map(|item| item.parse::<u16>().ok())
        .collect()
}

fn normalize_relay_url(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let trimmed = trimmed.trim_end_matches('/');
    let lower = trimmed.to_lowercase();
    if lower.starts_with("ws://") || lower.starts_with("wss://") {
        return Some(trimmed.to_string());
    }
    if lower.starts_with("http://") {
        return Some(format!("ws://{}", &trimmed[7..]));
    }
    if lower.starts_with("https://") {
        return Some(format!("wss://{}", &trimmed[8..]));
    }
    Some(format!("ws://{}", trimmed))
}

fn local_port_open(port: u16) -> bool {
    use std::net::{SocketAddr, TcpStream};
    use std::time::Duration;

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let timeout = Duration::from_millis(100);
    TcpStream::connect_timeout(&addr, timeout).is_ok()
}

fn dedupe_relays(relays: Vec<String>) -> Vec<String> {
    use std::collections::HashSet;
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for relay in relays {
        let key = relay.trim_end_matches('/').to_lowercase();
        if seen.insert(key) {
            out.push(relay);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        key: &'static str,
        prev: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let prev = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, prev }
        }

        fn clear(key: &'static str) -> Self {
            let prev = std::env::var(key).ok();
            std::env::remove_var(key);
            Self { key, prev }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(prev) = &self.prev {
                std::env::set_var(self.key, prev);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert!(!config.blossom.read_servers.is_empty());
        assert!(!config.blossom.write_servers.is_empty());
        assert_eq!(
            config.blossom.upload_concurrency,
            default_upload_concurrency()
        );
        assert!(!config.nostr.relays.is_empty());
        assert!(config
            .nostr
            .relays
            .contains(&"wss://temp.iris.to".to_string()));
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
upload_concurrency = 24
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.blossom.write_servers, vec!["https://custom.server"]);
        assert_eq!(config.blossom.upload_concurrency, 24);
        assert!(!config.blossom.read_servers.is_empty());
    }

    #[test]
    fn test_all_servers() {
        let mut config = BlossomConfig::default();
        config.servers = vec!["https://legacy.server".to_string()];

        let read = config.all_read_servers();
        assert!(read.contains(&"https://legacy.server".to_string()));
        assert!(read.contains(&"https://cdn.iris.to".to_string()));
        assert!(read.contains(&"https://blossom.primal.net".to_string()));
        assert!(read.contains(&"https://upload.iris.to".to_string()));

        let write = config.all_write_servers();
        assert!(write.contains(&"https://legacy.server".to_string()));
        assert!(write.contains(&"https://upload.iris.to".to_string()));
    }

    #[test]
    fn test_all_servers_fall_back_to_defaults_when_explicitly_empty() {
        let config = BlossomConfig {
            servers: vec![],
            read_servers: vec![],
            write_servers: vec![],
            max_upload_mb: default_max_upload_mb(),
            require_random_untrusted_ingest: default_require_random_untrusted_ingest(),
            upload_concurrency: default_upload_concurrency(),
            force_upload: false,
        };

        let mut expected_read = default_read_servers();
        expected_read.extend(default_write_servers());
        expected_read.sort();
        expected_read.dedup();
        assert_eq!(config.all_read_servers(), expected_read);
        assert_eq!(config.all_write_servers(), default_write_servers());
    }

    #[test]
    fn test_storage_backend_default() {
        let config = Config::default();
        assert_eq!(config.storage.backend, StorageBackend::Lmdb);
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
    fn test_storage_orphan_eviction_defaults_on_and_allows_override() {
        assert!(Config::default().storage.evict_orphans);

        let toml = r#"
[storage]
evict_orphans = false
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(!config.storage.evict_orphans);
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

    #[test]
    fn test_local_daemon_port_default() {
        assert_eq!(local_daemon_port(None), 8080);
    }

    #[test]
    fn test_local_daemon_port_parses_ipv4() {
        assert_eq!(local_daemon_port(Some("127.0.0.1:9090")), 9090);
    }

    #[test]
    fn test_local_daemon_port_parses_anyhost() {
        assert_eq!(local_daemon_port(Some("0.0.0.0:7070")), 7070);
    }

    #[test]
    fn test_local_daemon_port_parses_ipv6() {
        assert_eq!(local_daemon_port(Some("[::1]:6060")), 6060);
    }

    #[test]
    fn test_local_daemon_port_parses_hostname() {
        assert_eq!(local_daemon_port(Some("localhost:5050")), 5050);
    }

    #[test]
    fn test_local_daemon_port_invalid() {
        assert_eq!(local_daemon_port(Some("localhost")), 8080);
    }

    #[test]
    fn test_detect_local_daemon_url_respects_prefer_local_flag() {
        let _lock = ENV_LOCK.lock().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let _prefer = EnvGuard::set("NOSTR_PREFER_LOCAL", "0");

        assert_eq!(
            detect_local_daemon_url(Some(&format!("127.0.0.1:{port}"))),
            None
        );
    }

    #[test]
    fn test_detect_local_daemon_url_requires_opt_in() {
        let _lock = ENV_LOCK.lock().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let _prefer = EnvGuard::clear("HTREE_PREFER_LOCAL_DAEMON");
        let _prefer_nostr = EnvGuard::clear("NOSTR_PREFER_LOCAL");
        let _prefer_relay = EnvGuard::clear("HTREE_PREFER_LOCAL_RELAY");

        assert_eq!(
            detect_local_daemon_url(Some(&format!("127.0.0.1:{port}"))),
            None
        );
    }

    #[test]
    fn test_detect_local_daemon_url_uses_opt_in_flag() {
        let _lock = ENV_LOCK.lock().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let _prefer = EnvGuard::set("HTREE_PREFER_LOCAL_DAEMON", "1");

        assert_eq!(
            detect_local_daemon_url(Some(&format!("127.0.0.1:{port}"))),
            Some(format!("http://127.0.0.1:{port}"))
        );
    }

    #[test]
    fn test_resolve_relays_prefers_local() {
        let _lock = ENV_LOCK.lock().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let _prefer = EnvGuard::set("NOSTR_PREFER_LOCAL", "1");
        let _ports = EnvGuard::set("NOSTR_LOCAL_RELAY_PORTS", &port.to_string());
        let _relays = EnvGuard::clear("NOSTR_RELAYS");

        let base = vec!["wss://relay.example".to_string()];
        let resolved = resolve_relays(&base, Some("127.0.0.1:0"));

        assert!(!resolved.is_empty());
        assert_eq!(resolved[0], format!("ws://127.0.0.1:{port}"));
        assert!(resolved.contains(&"wss://relay.example".to_string()));
    }

    #[test]
    fn test_resolve_relays_env_override() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _prefer = EnvGuard::set("NOSTR_PREFER_LOCAL", "0");
        let _relays = EnvGuard::set("NOSTR_RELAYS", "wss://relay.one,wss://relay.two");

        let base = vec!["wss://relay.example".to_string()];
        let resolved = resolve_relays(&base, Some("127.0.0.1:0"));

        assert_eq!(
            resolved,
            vec!["wss://relay.one".to_string(), "wss://relay.two".to_string()]
        );
    }

    #[test]
    fn test_nostr_config_defaults_include_mirror_settings() {
        let config = NostrConfig::default();

        assert_eq!(config.social_graph_crawl_depth, 2);
        assert_eq!(config.mirror_max_follow_distance, None);
        assert_eq!(config.max_write_distance, 3);
        assert!(config.socialgraph_root.is_none());
        assert_eq!(
            config.bootstrap_follows,
            vec![DEFAULT_SOCIALGRAPH_ENTRYPOINT_NPUB.to_string()]
        );
        assert!(!config.negentropy_only);
        assert_eq!(config.overmute_threshold, 1.0);
        assert_eq!(config.mirror_kinds, vec![0, 1, 3, 6, 7, 9_735, 30_023]);
        assert_eq!(config.history_sync_author_chunk_size, 5_000);
        assert!(config.history_sync_on_reconnect);
    }

    #[test]
    fn test_nostr_config_deserializes_mirror_settings() {
        let config: NostrConfig = toml::from_str(
            r#"
relays = ["wss://relay.example"]
socialgraph_root = "npub1test"
bootstrap_follows = []
social_graph_crawl_depth = 6
mirror_max_follow_distance = 2
max_write_distance = 7
negentropy_only = true
overmute_threshold = 1.5
mirror_kinds = [0, 1, 3, 6, 7, 9735]
history_sync_author_chunk_size = 512
history_sync_on_reconnect = false
"#,
        )
        .expect("deserialize nostr config");

        assert_eq!(config.relays, vec!["wss://relay.example".to_string()]);
        assert_eq!(config.socialgraph_root.as_deref(), Some("npub1test"));
        assert!(config.bootstrap_follows.is_empty());
        assert_eq!(config.social_graph_crawl_depth, 6);
        assert_eq!(config.mirror_max_follow_distance, Some(2));
        assert_eq!(config.max_write_distance, 7);
        assert!(config.negentropy_only);
        assert_eq!(config.overmute_threshold, 1.5);
        assert_eq!(config.mirror_kinds, vec![0, 1, 3, 6, 7, 9_735]);
        assert_eq!(config.history_sync_author_chunk_size, 512);
        assert!(!config.history_sync_on_reconnect);
    }

    #[test]
    fn test_nostr_config_deserializes_legacy_crawl_depth_alias() {
        let config: NostrConfig = toml::from_str(
            r#"
relays = ["wss://relay.example"]
crawl_depth = 5
"#,
        )
        .expect("deserialize nostr config");

        assert_eq!(config.social_graph_crawl_depth, 5);
    }
}
