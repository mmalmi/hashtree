use anyhow::{Context, Result};
use hashtree_cli::daemon::{EmbeddedDaemonInfo, EmbeddedDaemonOptions};
use hashtree_cli::Config;
use hashtree_core::Cid;
use serde::Deserialize;
use std::path::{Path, PathBuf};

const BROWSER_STORAGE_MAX_SIZE_GB: u64 = 1;
const BROWSER_SOCIAL_GRAPH_DB_MAX_SIZE_GB: u64 = 1;
const BROWSER_SPAMBOX_DB_MAX_SIZE_GB: u64 = 0;

#[derive(Debug, Clone)]
pub struct HostDaemonOptions {
    pub state_root: PathBuf,
    pub bind_address: String,
    pub initial_tree_roots: Vec<(String, Cid)>,
}

impl HostDaemonOptions {
    pub fn new(state_root: impl Into<PathBuf>) -> Self {
        Self {
            state_root: state_root.into(),
            bind_address: "127.0.0.1:0".to_string(),
            initial_tree_roots: Vec::new(),
        }
    }

    pub fn with_initial_tree_roots(mut self, roots: Vec<(String, Cid)>) -> Self {
        self.initial_tree_roots = roots;
        self
    }
}

#[derive(Debug, Clone)]
pub struct HostDaemonStatus {
    pub base_url: String,
    pub self_npub: String,
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
}

pub struct HostDaemonRuntime {
    runtime: tokio::runtime::Runtime,
    info: Option<EmbeddedDaemonInfo>,
    bind_address: String,
    initial_tree_roots: Vec<(String, Cid)>,
    config_dir: PathBuf,
    data_dir: PathBuf,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
#[serde(rename_all = "camelCase")]
struct BrowserSettings {
    storage_max_size_gb: Option<u64>,
    social_graph_db_max_size_gb: Option<u64>,
    spambox_db_max_size_gb: Option<u64>,
    nostr_relays: Option<Vec<String>>,
    blossom_read_servers: Option<Vec<String>>,
    blossom_write_servers: Option<Vec<String>>,
    enable_webrtc: Option<bool>,
    enable_multicast: Option<bool>,
    max_multicast_peers: Option<usize>,
    enable_fips: Option<bool>,
    enable_fips_udp: Option<bool>,
    enable_fips_webrtc: Option<bool>,
    fetch_from_fips_peers: Option<bool>,
    social_graph_crawl_depth: Option<u32>,
    sync_enabled: Option<bool>,
    sync_own: Option<bool>,
    sync_followed: Option<bool>,
    sync_max_concurrent: Option<usize>,
    public_writes: Option<bool>,
    allowed_npubs: Option<Vec<String>>,
    socialgraph_root: Option<String>,
}

impl HostDaemonRuntime {
    pub fn start(options: HostDaemonOptions) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("build embedded host runtime")?;

        let config_dir = options.state_root.join("config");
        let data_dir = options.state_root.join("data");
        std::fs::create_dir_all(&config_dir).context("create embedded config dir")?;
        std::fs::create_dir_all(&data_dir).context("create embedded data dir")?;

        let config = browser_config(&data_dir, &config_dir);
        let info = runtime
            .block_on(hashtree_cli::daemon::start_embedded(
                EmbeddedDaemonOptions {
                    config,
                    data_dir: data_dir.clone(),
                    config_dir: Some(config_dir.clone()),
                    bind_address: options.bind_address,
                    relays: None,
                    initial_tree_roots: options.initial_tree_roots.clone(),
                    extra_routes: None,
                    cors: None,
                },
            ))
            .context("start embedded hashtree daemon")?;

        let bind_address = info.addr.clone();

        Ok(Self {
            runtime,
            info: Some(info),
            bind_address,
            initial_tree_roots: options.initial_tree_roots,
            config_dir,
            data_dir,
        })
    }

    pub fn status(&self) -> HostDaemonStatus {
        let info = self
            .info
            .as_ref()
            .expect("host daemon status requested after shutdown");
        HostDaemonStatus {
            base_url: format!("http://{}", info.addr),
            self_npub: info.npub.clone(),
            config_dir: self.config_dir.clone(),
            data_dir: self.data_dir.clone(),
        }
    }

    pub fn base_url(&self) -> String {
        self.status().base_url
    }

    pub fn self_npub(&self) -> &str {
        &self
            .info
            .as_ref()
            .expect("host daemon identity requested after shutdown")
            .npub
    }

    pub fn reload(&mut self) -> Result<HostDaemonStatus> {
        self.shutdown_current();

        let config = browser_config(&self.data_dir, &self.config_dir);
        let info = self
            .runtime
            .block_on(hashtree_cli::daemon::start_embedded(
                EmbeddedDaemonOptions {
                    config,
                    data_dir: self.data_dir.clone(),
                    config_dir: Some(self.config_dir.clone()),
                    bind_address: self.bind_address.clone(),
                    relays: None,
                    initial_tree_roots: self.initial_tree_roots.clone(),
                    extra_routes: None,
                    cors: None,
                },
            ))
            .context("reload embedded hashtree daemon")?;
        self.bind_address = info.addr.clone();
        self.info = Some(info);
        Ok(self.status())
    }

    pub fn shutdown(&mut self) {
        self.shutdown_current();
    }

    fn shutdown_current(&mut self) {
        let Some(info) = self.info.take() else {
            return;
        };
        let controller = info.daemon_controller.clone();
        self.runtime.block_on(async move {
            controller.shutdown().await;
            drop(info);
            tokio::task::yield_now().await;
        });
    }
}

impl Drop for HostDaemonRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn browser_config(data_dir: &Path, config_dir: &Path) -> Config {
    let mut config = Config::default();
    let settings = load_browser_settings(config_dir).unwrap_or_else(default_browser_settings);

    config.storage.max_size_gb = settings
        .storage_max_size_gb
        .unwrap_or(BROWSER_STORAGE_MAX_SIZE_GB)
        .max(1);
    config.nostr.db_max_size_gb = settings
        .social_graph_db_max_size_gb
        .unwrap_or(BROWSER_SOCIAL_GRAPH_DB_MAX_SIZE_GB)
        .max(1);
    config.nostr.spambox_max_size_gb = settings
        .spambox_db_max_size_gb
        .unwrap_or(BROWSER_SPAMBOX_DB_MAX_SIZE_GB);

    if let Some(nostr_relays) = settings.nostr_relays {
        config.nostr.relays = normalize_server_list(nostr_relays);
        config.nostr.enabled = !config.nostr.relays.is_empty();
    }

    if let Some(blossom_read_servers) = settings.blossom_read_servers {
        config.blossom.read_servers = normalize_server_list(blossom_read_servers);
        config.blossom.servers.clear();
    }
    if let Some(blossom_write_servers) = settings.blossom_write_servers {
        config.blossom.write_servers = normalize_server_list(blossom_write_servers);
        config.blossom.servers.clear();
    }
    config.blossom.enabled =
        !config.blossom.read_servers.is_empty() || !config.blossom.write_servers.is_empty();

    config.server.enable_webrtc = settings.enable_webrtc.unwrap_or(false);
    if !config.server.enable_webrtc {
        config.server.stun_port = 0;
    }

    config.server.enable_multicast = settings.enable_multicast.unwrap_or(false);
    if let Some(max_multicast_peers) = settings.max_multicast_peers {
        config.server.max_multicast_peers = max_multicast_peers;
    }

    if let Some(enable_fips) = settings.enable_fips {
        config.server.enable_fips = enable_fips;
    }
    if let Some(enable_fips_udp) = settings.enable_fips_udp {
        config.server.enable_fips_udp = enable_fips_udp;
    }
    if let Some(enable_fips_webrtc) = settings.enable_fips_webrtc {
        config.server.enable_fips_webrtc = enable_fips_webrtc;
    }
    if let Some(fetch_from_fips_peers) = settings.fetch_from_fips_peers {
        config.server.fetch_from_fips_peers = fetch_from_fips_peers;
    }
    if let Some(social_graph_crawl_depth) = settings.social_graph_crawl_depth {
        config.nostr.social_graph_crawl_depth = social_graph_crawl_depth;
    }

    config.sync.enabled = settings.sync_enabled.unwrap_or(false);
    if let Some(sync_own) = settings.sync_own {
        config.sync.sync_own = sync_own;
    }
    if let Some(sync_followed) = settings.sync_followed {
        config.sync.sync_followed = sync_followed;
    }
    if let Some(sync_max_concurrent) = settings.sync_max_concurrent {
        config.sync.max_concurrent = sync_max_concurrent.max(1);
    }

    config.server.public_writes = settings.public_writes.unwrap_or(false);
    if let Some(allowed_npubs) = settings.allowed_npubs {
        config.nostr.allowed_npubs = normalize_server_list(allowed_npubs);
    }
    if let Some(socialgraph_root) = settings.socialgraph_root {
        let socialgraph_root = socialgraph_root.trim().to_string();
        config.nostr.socialgraph_root = if socialgraph_root.is_empty() {
            None
        } else {
            Some(socialgraph_root)
        };
    }
    config.storage.data_dir = data_dir.to_string_lossy().to_string();
    config.server.enable_auth = false;
    config.server.enable_bluetooth = false;
    config.server.max_bluetooth_peers = 0;
    config
}

fn load_browser_settings(config_dir: &Path) -> Option<BrowserSettings> {
    let settings_path = config_dir.join("browser_settings.json");
    let raw = std::fs::read_to_string(settings_path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn default_browser_settings() -> BrowserSettings {
    BrowserSettings {
        storage_max_size_gb: Some(BROWSER_STORAGE_MAX_SIZE_GB),
        social_graph_db_max_size_gb: Some(BROWSER_SOCIAL_GRAPH_DB_MAX_SIZE_GB),
        spambox_db_max_size_gb: Some(BROWSER_SPAMBOX_DB_MAX_SIZE_GB),
        nostr_relays: None,
        blossom_read_servers: None,
        blossom_write_servers: None,
        enable_webrtc: Some(false),
        enable_multicast: Some(false),
        max_multicast_peers: None,
        enable_fips: None,
        enable_fips_udp: None,
        enable_fips_webrtc: None,
        fetch_from_fips_peers: None,
        social_graph_crawl_depth: None,
        sync_enabled: Some(false),
        sync_own: Some(true),
        sync_followed: Some(true),
        sync_max_concurrent: Some(3),
        public_writes: Some(false),
        allowed_npubs: None,
        socialgraph_root: None,
    }
}

fn normalize_server_list(values: Vec<String>) -> Vec<String> {
    let mut normalized = values
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    normalized.sort();
    normalized.dedup();
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use hashtree_cli::HashtreeStore;
    use hashtree_core::{from_hex, DirEntry, HashTree, HashTreeConfig, LinkType};
    use nostr::{nips::nip19::ToBech32, EventBuilder, Keys, Kind, Tag, TagKind, Timestamp};
    use reqwest::blocking::Client;
    use serde_json::json;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;
    use std::thread;
    use tempfile::TempDir;

    fn create_blossom_auth(keys: &Keys, action: &str) -> String {
        let expiration = Timestamp::from(Timestamp::now().as_secs() + 300);
        let tags = vec![
            Tag::custom(TagKind::Custom("t".into()), vec![action.to_string()]),
            Tag::custom(
                TagKind::Custom("expiration".into()),
                vec![expiration.to_string()],
            ),
        ];
        let event = EventBuilder::new(Kind::Custom(24242), "")
            .tags(tags)
            .sign_with_keys(keys)
            .expect("sign blossom auth");
        let encoded = base64::engine::general_purpose::STANDARD
            .encode(serde_json::to_string(&event).expect("serialize auth event"));
        format!("Nostr {encoded}")
    }

    fn start_blob_fixture_server(store: Arc<HashtreeStore>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture blob server");
        let addr = listener.local_addr().expect("fixture blob server addr");
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let store = store.clone();
                thread::spawn(move || handle_blob_fixture_request(stream, store));
            }
        });
        format!("http://{addr}")
    }

    fn handle_blob_fixture_request(mut stream: TcpStream, store: Arc<HashtreeStore>) {
        let mut buffer = [0_u8; 2048];
        let bytes_read = match stream.read(&mut buffer) {
            Ok(bytes_read) => bytes_read,
            Err(_) => return,
        };
        let request = String::from_utf8_lossy(&buffer[..bytes_read]);
        let path = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("/");
        let Some(hash_hex) = path
            .trim_start_matches('/')
            .strip_suffix(".bin")
            .filter(|hash_hex| hash_hex.len() == 64)
        else {
            let _ = stream.write_all(
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
            return;
        };
        let Ok(hash) = from_hex(hash_hex) else {
            let _ = stream.write_all(
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
            return;
        };
        let data = match store.get_blob(&hash) {
            Ok(Some(data)) => data,
            _ => {
                let _ = stream.write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                return;
            }
        };
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            data.len()
        );
        let _ = stream.write_all(header.as_bytes());
        let _ = stream.write_all(&data);
    }

    #[test]
    fn host_runtime_starts_and_shuts_down() {
        let temp = TempDir::new().expect("temp dir");
        let mut runtime =
            HostDaemonRuntime::start(HostDaemonOptions::new(temp.path())).expect("start daemon");

        let status = runtime.status();
        assert!(
            status.config_dir.join("keys").exists(),
            "expected host daemon to materialize keys in its config dir"
        );

        let response = reqwest::blocking::get(format!("{}/htree/test", status.base_url))
            .expect("fetch test endpoint");
        assert!(
            response.status().is_success(),
            "embedded daemon should answer"
        );

        runtime.shutdown();

        let stopped = reqwest::blocking::get(format!("{}/htree/test", status.base_url)).is_err();
        assert!(stopped, "expected host daemon shutdown to stop serving");

        runtime.shutdown();
    }

    #[test]
    fn host_runtime_rejects_public_blossom_uploads() {
        let temp = TempDir::new().expect("temp dir");
        let runtime =
            HostDaemonRuntime::start(HostDaemonOptions::new(temp.path())).expect("start daemon");
        let status = runtime.status();

        let keys = Keys::generate();
        let response = Client::new()
            .put(format!("{}/upload", status.base_url))
            .header("Authorization", create_blossom_auth(&keys, "upload"))
            .header("Content-Type", "text/plain")
            .body("browser-mode upload probe")
            .send()
            .expect("upload request");

        assert_eq!(
            response.status(),
            reqwest::StatusCode::FORBIDDEN,
            "browser-mode daemon must not accept public Blossom uploads"
        );
    }

    #[test]
    fn host_runtime_keeps_default_relays_and_file_servers() {
        let temp = TempDir::new().expect("temp dir");
        let runtime =
            HostDaemonRuntime::start(HostDaemonOptions::new(temp.path())).expect("start daemon");
        let status = runtime.status();

        let response = reqwest::blocking::get(format!("{}/api/status", status.base_url))
            .expect("fetch daemon status");
        assert!(
            response.status().is_success(),
            "status endpoint should answer"
        );
        let payload: serde_json::Value = response.json().expect("parse daemon status");

        assert!(
            payload["upstream"]["nostr_relays"]
                .as_u64()
                .unwrap_or_default()
                > 0,
            "browser mode should keep default relays for profile lookups",
        );
        assert!(
            payload["upstream"]["blossom_servers"]
                .as_u64()
                .unwrap_or_default()
                > 0,
            "browser mode should keep default file servers for content fetches",
        );
        assert_eq!(
            payload["mesh"]["enabled"].as_bool(),
            Some(false),
            "browser mode should still keep peer discovery off until enabled",
        );
    }

    #[test]
    fn host_runtime_serves_initial_keyed_root_from_upstream_blossom() {
        let temp = TempDir::new().expect("temp dir");
        let source_dir = TempDir::new().expect("source temp dir");
        let source_store = Arc::new(
            HashtreeStore::new(source_dir.path().join("source-db")).expect("source store"),
        );
        let setup_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("setup runtime");
        let (root_cid, expected_body) = setup_runtime.block_on(async {
            let source_tree = HashTree::new(HashTreeConfig::new(source_store.store_arc()));
            let expected_body =
                b"<!doctype html><title>Iris Apps</title><main>Drive Chat Contacts</main>".to_vec();
            let (index_cid, _) = source_tree.put(&expected_body).await.expect("index cid");
            let root_cid = source_tree
                .put_directory(vec![
                    DirEntry::from_cid("index.html", &index_cid).with_link_type(LinkType::File)
                ])
                .await
                .expect("root directory cid");
            (root_cid, expected_body)
        });
        assert!(
            root_cid.key.is_some(),
            "test must cover a keyed/private site root"
        );

        let upstream = start_blob_fixture_server(source_store);
        let route_keys = Keys::generate();
        let npub = route_keys.public_key().to_bech32().expect("encode npub");
        let config_dir = temp.path().join("config");
        std::fs::create_dir_all(&config_dir).expect("create config dir");
        std::fs::write(
            config_dir.join("browser_settings.json"),
            serde_json::to_vec_pretty(&json!({
                "nostrRelays": [],
                "blossomReadServers": [upstream],
                "blossomWriteServers": [],
                "enableFips": false,
                "enableFipsUdp": false,
                "enableFipsWebrtc": false,
                "fetchFromFipsPeers": false,
                "socialGraphCrawlDepth": 0,
                "syncEnabled": false,
                "allowedNpubs": [npub.clone()]
            }))
            .expect("serialize browser settings"),
        )
        .expect("write browser settings");

        let mut runtime = HostDaemonRuntime::start(
            HostDaemonOptions::new(temp.path())
                .with_initial_tree_roots(vec![(format!("{npub}/sites"), root_cid)]),
        )
        .expect("start daemon");
        let status = runtime.status();

        let response = Client::new()
            .get(format!("{}/htree/{npub}/sites/index.html", status.base_url))
            .send()
            .expect("fetch initial rooted site");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            response.bytes().expect("site body").as_ref(),
            expected_body.as_slice()
        );

        runtime.shutdown();
    }

    #[test]
    fn host_runtime_applies_browser_settings_overrides() {
        let temp = TempDir::new().expect("temp dir");
        let config_dir = temp.path().join("config");
        std::fs::create_dir_all(&config_dir).expect("create config dir");
        std::fs::write(
            config_dir.join("browser_settings.json"),
            serde_json::to_vec_pretty(&json!({
                "nostrRelays": [
                    "wss://relay.example-two",
                    "wss://relay.example-one",
                    "wss://relay.example-two"
                ],
                "blossomReadServers": [
                    "https://cdn.example"
                ],
                "blossomWriteServers": [
                    "https://upload.example"
                ],
                "enableWebrtc": true,
                "enableFips": false,
                "enableFipsUdp": false,
                "enableFipsWebrtc": false,
                "fetchFromFipsPeers": false,
                "socialGraphCrawlDepth": 0,
                "syncEnabled": false,
                "syncOwn": false,
                "syncFollowed": false
            }))
            .expect("serialize browser settings"),
        )
        .expect("write browser settings");

        let runtime =
            HostDaemonRuntime::start(HostDaemonOptions::new(temp.path())).expect("start daemon");
        let status = runtime.status();

        let response = reqwest::blocking::get(format!("{}/api/status", status.base_url))
            .expect("fetch daemon status");
        assert!(
            response.status().is_success(),
            "status endpoint should answer"
        );
        let payload: serde_json::Value = response.json().expect("parse daemon status");

        assert_eq!(payload["upstream"]["nostr_relays"].as_u64(), Some(2));
        assert_eq!(payload["upstream"]["blossom_servers"].as_u64(), Some(2));
        assert_eq!(payload["mesh"]["enabled"].as_bool(), Some(false));
    }

    #[test]
    fn browser_config_applies_background_service_overrides() {
        let temp = TempDir::new().expect("temp dir");
        let config_dir = temp.path().join("config");
        let data_dir = temp.path().join("data");
        std::fs::create_dir_all(&config_dir).expect("create config dir");
        std::fs::write(
            config_dir.join("browser_settings.json"),
            serde_json::to_vec_pretty(&json!({
                "enableFips": false,
                "enableFipsUdp": false,
                "enableFipsWebrtc": false,
                "fetchFromFipsPeers": false,
                "socialGraphCrawlDepth": 0
            }))
            .expect("serialize browser settings"),
        )
        .expect("write browser settings");

        let config = browser_config(&data_dir, &config_dir);

        assert!(!config.server.enable_fips);
        assert!(!config.server.enable_fips_udp);
        assert!(!config.server.enable_fips_webrtc);
        assert!(!config.server.fetch_from_fips_peers);
        assert_eq!(config.nostr.social_graph_crawl_depth, 0);
    }

    #[test]
    fn browser_config_uses_browser_sized_database_limits() {
        let temp = TempDir::new().expect("temp dir");
        let config_dir = temp.path().join("config");
        let data_dir = temp.path().join("data");
        std::fs::create_dir_all(&config_dir).expect("create config dir");

        let config = browser_config(&data_dir, &config_dir);

        assert_eq!(config.storage.max_size_gb, BROWSER_STORAGE_MAX_SIZE_GB);
        assert_eq!(
            config.nostr.db_max_size_gb,
            BROWSER_SOCIAL_GRAPH_DB_MAX_SIZE_GB
        );
        assert_eq!(
            config.nostr.spambox_max_size_gb,
            BROWSER_SPAMBOX_DB_MAX_SIZE_GB
        );
    }

    #[test]
    fn browser_config_applies_database_limit_overrides() {
        let temp = TempDir::new().expect("temp dir");
        let config_dir = temp.path().join("config");
        let data_dir = temp.path().join("data");
        std::fs::create_dir_all(&config_dir).expect("create config dir");
        std::fs::write(
            config_dir.join("browser_settings.json"),
            serde_json::to_vec_pretty(&json!({
                "storageMaxSizeGb": 2,
                "socialGraphDbMaxSizeGb": 2,
                "spamboxDbMaxSizeGb": 1
            }))
            .expect("serialize browser settings"),
        )
        .expect("write browser settings");

        let config = browser_config(&data_dir, &config_dir);

        assert_eq!(config.storage.max_size_gb, 2);
        assert_eq!(config.nostr.db_max_size_gb, 2);
        assert_eq!(config.nostr.spambox_max_size_gb, 1);
    }

    #[test]
    fn host_runtime_reload_applies_updated_browser_settings() {
        let temp = TempDir::new().expect("temp dir");
        let mut runtime =
            HostDaemonRuntime::start(HostDaemonOptions::new(temp.path())).expect("start daemon");
        let initial_status = runtime.status();

        let config_dir = temp.path().join("config");
        std::fs::create_dir_all(&config_dir).expect("create config dir");
        std::fs::write(
            config_dir.join("browser_settings.json"),
            serde_json::to_vec_pretty(&json!({
                "nostrRelays": ["wss://relay.example-one"],
                "blossomReadServers": ["https://cdn.example"],
                "blossomWriteServers": ["https://upload.example"],
                "enableWebrtc": true
            }))
            .expect("serialize browser settings"),
        )
        .expect("write browser settings");

        let reloaded_status = runtime.reload().expect("reload daemon");
        let response = reqwest::blocking::get(format!("{}/api/status", reloaded_status.base_url))
            .expect("fetch daemon status after reload");
        assert!(
            response.status().is_success(),
            "reloaded daemon should answer"
        );
        let payload: serde_json::Value = response.json().expect("parse daemon status");

        assert_eq!(payload["upstream"]["nostr_relays"].as_u64(), Some(1));
        assert_eq!(payload["upstream"]["blossom_servers"].as_u64(), Some(2));
        assert_eq!(
            payload["mesh"]["enabled"].as_bool(),
            Some(false),
            "embedded non-P2P builds keep mesh disabled even when browser settings request WebRTC",
        );
        assert!(
            !reloaded_status.base_url.is_empty(),
            "reload should keep serving from some loopback endpoint"
        );
        assert_eq!(
            reloaded_status.self_npub, initial_status.self_npub,
            "reload should keep the same browser-owned identity"
        );
    }
}
