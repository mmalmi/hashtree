#![cfg(unix)]

use anyhow::{Context, Result};
use nostr::{Keys, ToBech32};
use reqwest::blocking::Client;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct DaemonGuard {
    htree_bin: PathBuf,
    home_path: PathBuf,
    config_dir: PathBuf,
    pid_file: PathBuf,
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = Command::new(&self.htree_bin)
            .arg("stop")
            .arg("--pid-file")
            .arg(&self.pid_file)
            .env("HOME", &self.home_path)
            .env("HTREE_CONFIG_DIR", &self.config_dir)
            .output();

        if let Ok(pid_raw) = fs::read_to_string(&self.pid_file) {
            if let Ok(pid) = pid_raw.trim().parse::<i32>() {
                if is_process_running(pid) {
                    unsafe {
                        let _ = libc::kill(pid, libc::SIGKILL);
                    }
                }
            }
        }

        let _ = fs::remove_file(&self.pid_file);
    }
}

#[test]
fn reload_restarts_daemon_and_applies_updated_config() -> Result<()> {
    let home_dir = TempDir::new().context("create temp home")?;
    let home_path = home_dir.path().to_path_buf();
    let config_dir = home_path.join(".hashtree");
    let data_dir = home_path.join("data");
    fs::create_dir_all(&config_dir).context("create config dir")?;
    fs::create_dir_all(&data_dir).context("create data dir")?;

    write_test_config(&config_dir, &data_dir, "normal")?;
    let keys = Keys::generate();
    fs::write(
        config_dir.join("keys"),
        keys.secret_key()
            .to_bech32()
            .context("encode daemon nsec")?,
    )
    .context("write daemon keys")?;

    let htree_bin = find_htree_binary();
    let addr = format!("127.0.0.1:{}", find_free_port()?);
    let pid_file = home_path.join("htree.pid");
    let log_file = home_path.join("htree.log");

    let _guard = DaemonGuard {
        htree_bin: htree_bin.clone(),
        home_path: home_path.clone(),
        config_dir: config_dir.clone(),
        pid_file: pid_file.clone(),
    };

    let output = Command::new(&htree_bin)
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("start")
        .arg("--addr")
        .arg(&addr)
        .arg("--daemon")
        .arg("--pid-file")
        .arg(&pid_file)
        .arg("--log-file")
        .arg(&log_file)
        .env("HOME", &home_path)
        .env("HTREE_CONFIG_DIR", &config_dir)
        .env("RUST_LOG", "warn")
        .output()
        .context("start daemon")?;
    if !output.status.success() {
        anyhow::bail!(
            "htree start failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let initial_pid = wait_for_pid_file(&pid_file, Duration::from_secs(5))?;
    let initial_status = wait_for_mode(&addr, "normal", Duration::from_secs(10))?;
    assert_eq!(initial_status["capabilities"]["hash_get"], true);

    write_test_config(&config_dir, &data_dir, "assist")?;

    let reload_output = Command::new(&htree_bin)
        .arg("reload")
        .arg("--pid-file")
        .arg(&pid_file)
        .env("HOME", &home_path)
        .env("HTREE_CONFIG_DIR", &config_dir)
        .env("RUST_LOG", "warn")
        .output()
        .context("reload daemon")?;
    if !reload_output.status.success() {
        anyhow::bail!(
            "htree reload failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&reload_output.stdout),
            String::from_utf8_lossy(&reload_output.stderr)
        );
    }

    let reloaded_pid = wait_for_pid_change(&pid_file, initial_pid, Duration::from_secs(10))?;
    assert_ne!(reloaded_pid, initial_pid);

    let reloaded_status = wait_for_mode(&addr, "assist", Duration::from_secs(10))?;
    assert_eq!(reloaded_status["capabilities"]["hash_get"], false);

    Ok(())
}

fn write_test_config(config_dir: &Path, data_dir: &Path, mode: &str) -> Result<()> {
    let config = format!(
        r#"
[server]
mode = "{mode}"
enable_auth = false
enable_webrtc = false
enable_multicast = false
max_multicast_peers = 0
enable_wifi_aware = false
max_wifi_aware_peers = 0
enable_bluetooth = false
max_bluetooth_peers = 0
public_writes = true

[storage]
backend = "lmdb"
data_dir = "{data_dir}"
max_size_gb = 1

[nostr]
relays = []
social_graph_crawl_depth = 0
db_max_size_gb = 1
spambox_max_size_gb = 0

[sync]
enabled = false
"#,
        data_dir = data_dir.display(),
    );
    fs::write(config_dir.join("config.toml"), config).context("write config.toml")
}

fn find_htree_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("CARGO_BIN_EXE_htree") {
        return PathBuf::from(path);
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let target_dir = match std::env::var_os("CARGO_TARGET_DIR") {
        Some(path) => {
            let path = PathBuf::from(path);
            if path.is_absolute() {
                path
            } else {
                workspace_root.join(path)
            }
        }
        None => workspace_root.join("target"),
    };
    let debug_bin = target_dir.join("debug/htree");
    let release_bin = target_dir.join("release/htree");

    if debug_bin.exists() {
        debug_bin
    } else if release_bin.exists() {
        release_bin
    } else {
        panic!(
            "htree binary not found. Run `cargo build --bin htree` first.\nLooked in:\n  - {:?}\n  - {:?}",
            debug_bin, release_bin
        );
    }
}

fn find_free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .context("bind ephemeral port for reload test")?;
    Ok(listener.local_addr().context("read ephemeral port")?.port())
}

fn wait_for_pid_file(path: &Path, timeout: Duration) -> Result<i32> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(raw) = fs::read_to_string(path) {
            let pid = raw.trim().parse::<i32>().context("parse pid")?;
            if is_process_running(pid) {
                return Ok(pid);
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    anyhow::bail!("timed out waiting for pid file {}", path.display())
}

fn wait_for_pid_change(path: &Path, old_pid: i32, timeout: Duration) -> Result<i32> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(raw) = fs::read_to_string(path) {
            if let Ok(pid) = raw.trim().parse::<i32>() {
                if pid != old_pid && is_process_running(pid) {
                    return Ok(pid);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    anyhow::bail!(
        "timed out waiting for pid file {} to change from {}",
        path.display(),
        old_pid
    )
}

fn wait_for_mode(addr: &str, expected_mode: &str, timeout: Duration) -> Result<Value> {
    let client = Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .context("build reqwest client")?;
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(resp) = client.get(format!("http://{addr}/api/status")).send() {
            if resp.status().is_success() {
                let status: Value = resp.json().context("parse daemon status json")?;
                if status["mode"].as_str() == Some(expected_mode) {
                    return Ok(status);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    anyhow::bail!("timed out waiting for daemon mode {expected_mode} at {addr}")
}

fn is_process_running(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}
