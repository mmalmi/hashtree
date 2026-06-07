use std::ffi::{OsStr, OsString};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use nostr::nips::nip19::ToBech32;
use nostr::{JsonUtil, RelayMessage as NostrRelayMessage};
use serde_json::Value;
use tempfile::TempDir;
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

struct EnvVarGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        if let Some(value) = &self.previous {
            std::env::set_var(self.key, value);
        } else {
            std::env::remove_var(self.key);
        }
    }
}

#[tokio::test]
async fn embedded_daemon_serves_htree_test() {
    let dir = TempDir::new().expect("temp dir");
    let _lock = env_lock().lock().expect("env lock");
    let _config_env = EnvVarGuard::set("HTREE_CONFIG_DIR", dir.path());
    let _data_env = EnvVarGuard::set("HTREE_DATA_DIR", dir.path());

    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("create data dir");

    let mut config = hashtree_cli::Config::default();
    config.storage.data_dir = data_dir.to_string_lossy().to_string();
    config.server.enable_auth = false;
    config.server.enable_webrtc = false;
    config.server.stun_port = 0;

    let info = hashtree_cli::daemon::start_embedded(hashtree_cli::daemon::EmbeddedDaemonOptions {
        config,
        data_dir: data_dir.clone(),
        config_dir: Some(dir.path().to_path_buf()),
        bind_address: "127.0.0.1:0".to_string(),
        relays: None,
        extra_routes: None,
        cors: None,
    })
    .await
    .expect("start embedded daemon");

    let base = format!("http://127.0.0.1:{}", info.port);
    let mut ok = false;
    for _ in 0..10 {
        if let Ok(resp) = reqwest::get(format!("{}/htree/test", base)).await {
            if resp.status().is_success() {
                ok = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    assert!(ok, "expected /htree/test to respond");
}

#[tokio::test]
async fn embedded_daemon_respects_explicit_config_dir_without_env() {
    let dir = TempDir::new().expect("temp dir");
    let config_dir = dir.path().join("config");
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("create data dir");

    let mut config = hashtree_cli::Config::default();
    config.storage.data_dir = data_dir.to_string_lossy().to_string();
    config.server.enable_auth = false;
    config.server.enable_webrtc = false;
    config.server.stun_port = 0;

    let info = hashtree_cli::daemon::start_embedded(hashtree_cli::daemon::EmbeddedDaemonOptions {
        config,
        data_dir: data_dir.clone(),
        config_dir: Some(config_dir.clone()),
        bind_address: "127.0.0.1:0".to_string(),
        relays: None,
        extra_routes: None,
        cors: None,
    })
    .await
    .expect("start embedded daemon");

    let keys = hashtree_cli::config::read_keys_in(&config_dir).expect("read generated keys");
    assert!(
        config_dir.join("keys").exists(),
        "expected embedded daemon to write keys into explicit config dir"
    );
    assert_eq!(
        info.npub,
        keys.public_key().to_bech32().expect("encode npub"),
        "embedded daemon should report the identity stored in the explicit config dir"
    );

    let base = format!("http://127.0.0.1:{}", info.port);
    info.daemon_controller.shutdown().await;

    let mut stopped = false;
    for _ in 0..10 {
        if reqwest::get(format!("{}/htree/test", base)).await.is_err() {
            stopped = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    assert!(
        stopped,
        "expected daemon controller shutdown to stop HTTP serving"
    );
}

#[tokio::test]
async fn embedded_daemon_uses_default_blossom_servers_when_config_is_empty() {
    let dir = TempDir::new().expect("temp dir");
    let _lock = env_lock().lock().expect("env lock");
    let _config_env = EnvVarGuard::set("HTREE_CONFIG_DIR", dir.path());
    let _data_env = EnvVarGuard::set("HTREE_DATA_DIR", dir.path());

    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("create data dir");

    let mut config = hashtree_cli::Config::default();
    config.storage.data_dir = data_dir.to_string_lossy().to_string();
    config.server.enable_auth = false;
    config.server.enable_webrtc = false;
    config.server.stun_port = 0;
    config.blossom.servers.clear();
    config.blossom.read_servers.clear();
    config.blossom.write_servers.clear();

    let info = hashtree_cli::daemon::start_embedded(hashtree_cli::daemon::EmbeddedDaemonOptions {
        config,
        data_dir: data_dir.clone(),
        config_dir: Some(dir.path().to_path_buf()),
        bind_address: "127.0.0.1:0".to_string(),
        relays: None,
        extra_routes: None,
        cors: None,
    })
    .await
    .expect("start embedded daemon");

    let status: Value = reqwest::get(format!("http://127.0.0.1:{}/api/status", info.port))
        .await
        .expect("fetch daemon status")
        .json()
        .await
        .expect("parse daemon status json");

    let blossom_servers = status["upstream"]["blossom_servers"]
        .as_u64()
        .expect("blossom_servers count");
    assert!(
        blossom_servers >= 2,
        "expected embedded daemon to keep default blossom read servers, got {blossom_servers}"
    );
}

#[tokio::test]
async fn embedded_daemon_accepts_ws_route_with_trailing_slash() {
    let dir = TempDir::new().expect("temp dir");
    let _lock = env_lock().lock().expect("env lock");
    let _config_env = EnvVarGuard::set("HTREE_CONFIG_DIR", dir.path());
    let _data_env = EnvVarGuard::set("HTREE_DATA_DIR", dir.path());

    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("create data dir");

    let mut config = hashtree_cli::Config::default();
    config.storage.data_dir = data_dir.to_string_lossy().to_string();
    config.server.enable_auth = false;
    config.server.enable_webrtc = false;
    config.server.enable_multicast = false;
    config.server.max_multicast_peers = 0;
    config.server.enable_bluetooth = false;
    config.server.max_bluetooth_peers = 0;
    config.server.stun_port = 0;
    config.nostr.relays.clear();

    let info = hashtree_cli::daemon::start_embedded(hashtree_cli::daemon::EmbeddedDaemonOptions {
        config,
        data_dir: data_dir.clone(),
        config_dir: Some(dir.path().to_path_buf()),
        bind_address: "127.0.0.1:0".to_string(),
        relays: None,
        extra_routes: None,
        cors: None,
    })
    .await
    .expect("start embedded daemon");

    let url = format!("ws://127.0.0.1:{}/ws/", info.port);
    let (mut socket, _) = connect_async(url)
        .await
        .expect("connect ws route with trailing slash");

    socket
        .send(WsMessage::Text(
            nostr::ClientMessage::req(
                nostr::SubscriptionId::new("test-sub"),
                vec![nostr::Filter::new()
                    .authors(vec![nostr::Keys::generate().public_key()])
                    .kinds(vec![nostr::Kind::from(30078_u16)])],
            )
            .as_json()
            .into(),
        ))
        .await
        .expect("send req");

    let response = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("response timeout")
        .expect("response present")
        .expect("response websocket");
    let WsMessage::Text(text) = response else {
        panic!("expected text relay response");
    };
    assert!(
        matches!(
            NostrRelayMessage::from_json(text.as_str()).expect("parse relay response"),
            NostrRelayMessage::EndOfStoredEvents(subscription_id)
                if subscription_id.as_ref() == &nostr::SubscriptionId::new("test-sub")
        ),
        "expected EOSE response over /ws/ route"
    );
}

#[tokio::test]
async fn embedded_daemon_reports_assist_mode_in_status() {
    let dir = TempDir::new().expect("temp dir");
    let _lock = env_lock().lock().expect("env lock");
    let _config_env = EnvVarGuard::set("HTREE_CONFIG_DIR", dir.path());
    let _data_env = EnvVarGuard::set("HTREE_DATA_DIR", dir.path());

    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("create data dir");

    let mut config = hashtree_cli::Config::default();
    config.storage.data_dir = data_dir.to_string_lossy().to_string();
    config.server.enable_auth = false;
    config.server.enable_webrtc = false;
    config.server.stun_port = 0;
    config.server.mode = hashtree_cli::config::ServerMode::Assist;

    let info = hashtree_cli::daemon::start_embedded(hashtree_cli::daemon::EmbeddedDaemonOptions {
        config,
        data_dir: data_dir.clone(),
        config_dir: Some(dir.path().to_path_buf()),
        bind_address: "127.0.0.1:0".to_string(),
        relays: None,
        extra_routes: None,
        cors: None,
    })
    .await
    .expect("start embedded daemon");

    let status: Value = reqwest::get(format!("http://127.0.0.1:{}/api/status", info.port))
        .await
        .expect("fetch daemon status")
        .json()
        .await
        .expect("parse daemon status json");

    assert_eq!(status["mode"], "assist");
    assert_eq!(status["capabilities"]["hash_get"], false);
}

#[tokio::test]
async fn embedded_daemon_socialgraph_snapshot_is_not_cors_readable() {
    let dir = TempDir::new().expect("temp dir");
    let _lock = env_lock().lock().expect("env lock");
    let _config_env = EnvVarGuard::set("HTREE_CONFIG_DIR", dir.path());
    let _data_env = EnvVarGuard::set("HTREE_DATA_DIR", dir.path());

    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("create data dir");

    let mut config = hashtree_cli::Config::default();
    config.storage.data_dir = data_dir.to_string_lossy().to_string();
    config.server.enable_auth = false;
    config.server.enable_webrtc = false;
    config.server.enable_multicast = false;
    config.server.max_multicast_peers = 0;
    config.server.enable_bluetooth = false;
    config.server.max_bluetooth_peers = 0;
    config.server.stun_port = 0;
    config.sync.enabled = false;
    config.nostr.relays.clear();

    let info = hashtree_cli::daemon::start_embedded(hashtree_cli::daemon::EmbeddedDaemonOptions {
        config,
        data_dir: data_dir.clone(),
        config_dir: Some(dir.path().to_path_buf()),
        bind_address: "127.0.0.1:0".to_string(),
        relays: None,
        extra_routes: None,
        cors: None,
    })
    .await
    .expect("start embedded daemon");

    let response = reqwest::get(format!(
        "http://127.0.0.1:{}/api/socialgraph/snapshot?maxNodes=1&maxEdges=1",
        info.port
    ))
    .await
    .expect("fetch snapshot");

    assert!(
        response.status().is_success(),
        "expected snapshot request to succeed on loopback, got {}",
        response.status()
    );
    assert!(
        response
            .headers()
            .get(reqwest::header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none(),
        "embedded localhost-only snapshot should not be browser-readable via CORS"
    );
}

#[tokio::test]
async fn embedded_daemon_background_services_follow_live_relay_settings() {
    let dir = TempDir::new().expect("temp dir");
    let _lock = env_lock().lock().expect("env lock");
    let _config_env = EnvVarGuard::set("HTREE_CONFIG_DIR", dir.path());
    let _data_env = EnvVarGuard::set("HTREE_DATA_DIR", dir.path());

    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("create data dir");

    let mut config = hashtree_cli::Config::default();
    config.storage.data_dir = data_dir.to_string_lossy().to_string();
    config.server.enable_auth = false;
    config.server.enable_webrtc = false;
    config.server.enable_multicast = false;
    config.server.max_multicast_peers = 0;
    config.server.enable_bluetooth = false;
    config.server.max_bluetooth_peers = 0;
    config.server.stun_port = 0;
    config.nostr.enabled = true;
    config.nostr.relays = vec!["ws://127.0.0.1:1".to_string()];
    config.nostr.social_graph_crawl_depth = 1;
    config.sync.enabled = true;
    config.sync.sync_own = true;
    config.sync.sync_followed = false;

    let info = hashtree_cli::daemon::start_embedded(hashtree_cli::daemon::EmbeddedDaemonOptions {
        config: config.clone(),
        data_dir: data_dir.clone(),
        config_dir: Some(dir.path().to_path_buf()),
        bind_address: "127.0.0.1:0".to_string(),
        relays: None,
        extra_routes: None,
        cors: None,
    })
    .await
    .expect("start embedded daemon");

    let controller = info
        .background_services_controller
        .clone()
        .expect("background services controller");

    let initial = controller.status().await;
    assert!(
        initial.crawler_active,
        "crawler should start when relays are enabled"
    );
    assert!(
        initial.mirror_active,
        "mirror should start when relays are enabled"
    );
    assert!(
        initial.sync_active,
        "background sync should start when relays are enabled"
    );

    let mut relays_disabled = config.clone();
    relays_disabled.nostr.enabled = false;

    let disabled = controller
        .apply_config(&relays_disabled)
        .await
        .expect("disable relay-backed background services");
    assert!(
        !disabled.crawler_active,
        "crawler should stop when relays are disabled"
    );
    assert!(
        !disabled.mirror_active,
        "mirror should stop when relays are disabled"
    );
    assert!(
        !disabled.sync_active,
        "background sync should stop when relays are disabled"
    );

    let restarted = controller
        .apply_config(&config)
        .await
        .expect("re-enable relay-backed background services");
    assert!(
        restarted.crawler_active,
        "crawler should restart when relays return"
    );
    assert!(
        restarted.mirror_active,
        "mirror should restart when relays return"
    );
    assert!(
        restarted.sync_active,
        "background sync should restart when relays return"
    );

    controller
        .apply_config(&relays_disabled)
        .await
        .expect("shut down background services");
}
