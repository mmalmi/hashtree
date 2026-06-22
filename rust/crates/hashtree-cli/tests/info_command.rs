mod common;

use std::ffi::OsString;
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use common::htree_bin;
use hashtree_cli::{Config, HashtreeStore};
use hashtree_core::{from_hex, nhash_encode};
use tempfile::TempDir;

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

struct EnvVarGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
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

fn write_config_file(config_dir: &std::path::Path, config: &Config) {
    std::fs::create_dir_all(config_dir).expect("create config dir");
    let config_toml = toml::to_string_pretty(config).expect("serialize config");
    std::fs::write(config_dir.join("config.toml"), config_toml).expect("write config");
}

fn run_info_command(
    config_dir: &std::path::Path,
    data_dir: &std::path::Path,
    cid: &str,
) -> (String, String) {
    let output = Command::new(htree_bin())
        .arg("--data-dir")
        .arg(data_dir)
        .arg("info")
        .arg(cid)
        .env("HTREE_CONFIG_DIR", config_dir)
        .env("RUST_LOG", "debug")
        .output()
        .expect("run htree info");

    assert!(
        output.status.success(),
        "htree info failed.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    (
        String::from_utf8(output.stdout).expect("stdout utf-8"),
        String::from_utf8(output.stderr).expect("stderr utf-8"),
    )
}

#[test]
fn info_handles_directory_roots() {
    let tmp = TempDir::new().expect("temp dir");
    let config_dir = tmp.path().join("config");
    let data_dir = tmp.path().join("store");
    let store = HashtreeStore::new(&data_dir).expect("store");

    let site_dir = tmp.path().join("site");
    std::fs::create_dir_all(site_dir.join("assets")).expect("asset dir");
    std::fs::write(site_dir.join("index.html"), "<html></html>").expect("index");
    std::fs::write(site_dir.join("assets").join("logo.txt"), "logo").expect("logo");

    let hash_hex = store.upload_dir(&site_dir).expect("upload dir");

    let mut config = Config::default();
    config.server.enable_webrtc = false;
    config.server.enable_auth = false;
    config.server.stun_port = 0;
    config.sync.enabled = false;
    write_config_file(&config_dir, &config);

    let (stdout, _stderr) = run_info_command(&config_dir, &data_dir, &hash_hex);

    assert!(
        stdout.contains("Directory contents:"),
        "expected directory listing.\nstdout:\n{stdout}"
    );
    assert!(
        stdout.contains("index.html"),
        "expected file entry in listing.\nstdout:\n{stdout}"
    );
    assert!(
        !stdout.contains("Hash not found"),
        "directory roots should not be reported missing.\nstdout:\n{stdout}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn info_fetches_missing_hash_via_local_daemon() {
    let _lock = env_lock().lock().expect("env lock");

    let tmp = TempDir::new().expect("temp dir");
    let config_dir = tmp.path().join("config");
    let daemon_data_dir = tmp.path().join("daemon-store");
    let local_data_dir = tmp.path().join("local-store");
    std::fs::create_dir_all(&daemon_data_dir).expect("daemon store dir");
    std::fs::create_dir_all(&local_data_dir).expect("local store dir");

    let _config_env = EnvVarGuard::set("HTREE_CONFIG_DIR", &config_dir);
    let _prefer_local_daemon = EnvVarGuard::set("HTREE_PREFER_LOCAL_DAEMON", "1");

    let mut daemon_config = Config::default();
    daemon_config.storage.data_dir = daemon_data_dir.to_string_lossy().to_string();
    daemon_config.server.enable_auth = false;
    daemon_config.server.enable_webrtc = false;
    daemon_config.server.stun_port = 0;
    daemon_config.sync.enabled = false;

    let daemon =
        hashtree_cli::daemon::start_embedded(hashtree_cli::daemon::EmbeddedDaemonOptions {
            config: daemon_config,
            data_dir: daemon_data_dir.clone(),
            config_dir: Some(config_dir.clone()),
            bind_address: "127.0.0.1:0".to_string(),
            relays: None,
            initial_tree_roots: Vec::new(),
            extra_routes: None,
            cors: None,
        })
        .await
        .expect("start embedded daemon");

    let base = format!("http://127.0.0.1:{}", daemon.port);
    let mut ready = false;
    for _ in 0..20 {
        if let Ok(resp) = reqwest::get(format!("{}/htree/test", base)).await {
            if resp.status().is_success() {
                ready = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(ready, "embedded daemon did not become ready");

    let file_path = tmp.path().join("remote.txt");
    let file_bytes = b"hello from daemon-backed info";
    std::fs::write(&file_path, file_bytes).expect("write source file");
    let hash_hex = daemon.store.upload_file(&file_path).expect("upload file");
    let hash = from_hex(&hash_hex).expect("decode hash");
    let nhash = nhash_encode(&hash).expect("encode nhash");
    let blob_resp = reqwest::get(format!("{}/{}.bin", base, hash_hex))
        .await
        .expect("fetch blob from daemon");
    assert!(
        blob_resp.status().is_success(),
        "daemon should serve uploaded blob, got {}",
        blob_resp.status()
    );
    let blob_bytes = blob_resp.bytes().await.expect("blob body");
    assert_eq!(blob_bytes.as_ref(), file_bytes);

    let mut cli_config = Config::default();
    cli_config.server.bind_address = format!("127.0.0.1:{}", daemon.port);
    cli_config.server.enable_auth = false;
    cli_config.server.enable_webrtc = false;
    cli_config.server.stun_port = 0;
    cli_config.sync.enabled = false;
    write_config_file(&config_dir, &cli_config);

    let (stdout, stderr) = run_info_command(&config_dir, &local_data_dir, &nhash);

    assert!(
        stdout.contains(&format!("Total size: {} bytes", file_bytes.len())),
        "expected info output for remotely fetched content.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !stdout.contains("Hash not found"),
        "expected info to warm content via local daemon.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}
