//! Integration tests for local hashtree CLI and daemon flows.
//!
//! Run with: cargo test --package hashtree-cli --test two_instances -- --nocapture

#![allow(dead_code)]

use anyhow::{Context, Result};
use nostr::{Keys, ToBech32};
use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const TEST_STORAGE_BACKEND: &str = "lmdb";
const TEST_STORAGE_MAX_SIZE_GB: u64 = 10;

mod test_relay {
    use futures::{SinkExt, StreamExt};
    use std::collections::HashMap;
    use std::net::TcpListener;
    use std::sync::Arc;
    use tokio::net::TcpStream;
    use tokio::sync::{broadcast, RwLock};
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    #[derive(Clone)]
    struct StoredFilter {
        sub_id: String,
        kind: Option<u64>,
        authors: Vec<String>,
        p_tag: Option<String>,
        l_tag: Option<String>,
    }

    impl StoredFilter {
        fn matches(&self, event: &serde_json::Value) -> bool {
            if let Some(k) = self.kind {
                if event.get("kind").and_then(|v| v.as_u64()) != Some(k) {
                    return false;
                }
            }

            if !self.authors.is_empty() {
                let event_author = event.get("pubkey").and_then(|v| v.as_str()).unwrap_or("");
                if !self.authors.iter().any(|a| a == event_author) {
                    return false;
                }
            }

            if let Some(ref p) = self.p_tag {
                let has_p = event
                    .get("tags")
                    .and_then(|t| t.as_array())
                    .map(|tags| {
                        tags.iter().any(|tag| {
                            tag.as_array()
                                .map(|arr| {
                                    arr.len() >= 2
                                        && arr[0].as_str() == Some("p")
                                        && arr[1].as_str() == Some(p.as_str())
                                })
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false);
                if !has_p {
                    return false;
                }
            }

            if let Some(ref l) = self.l_tag {
                let has_l = event
                    .get("tags")
                    .and_then(|t| t.as_array())
                    .map(|tags| {
                        tags.iter().any(|tag| {
                            tag.as_array()
                                .map(|arr| {
                                    arr.len() >= 2
                                        && arr[0].as_str() == Some("l")
                                        && arr[1].as_str() == Some(l.as_str())
                                })
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false);
                if !has_l {
                    return false;
                }
            }

            true
        }
    }

    pub struct TestRelay {
        port: u16,
        shutdown: broadcast::Sender<()>,
        stopped: bool,
    }

    impl TestRelay {
        pub fn new(port: u16) -> Self {
            let listener = TcpListener::bind(format!("127.0.0.1:{}", port)).unwrap();
            let port = listener.local_addr().unwrap().port();
            listener.set_nonblocking(true).unwrap();
            let events: Arc<RwLock<HashMap<String, serde_json::Value>>> =
                Arc::new(RwLock::new(HashMap::new()));
            let (shutdown, _) = broadcast::channel(1);
            let (event_tx, _) = broadcast::channel::<serde_json::Value>(1000);

            let relay = TestRelay {
                port,
                shutdown: shutdown.clone(),
                stopped: false,
            };

            let events_clone = events.clone();
            let mut shutdown_rx = shutdown.subscribe();
            let event_tx_clone = event_tx.clone();

            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .unwrap();

                rt.block_on(async move {
                    let listener = tokio::net::TcpListener::from_std(listener).unwrap();

                    loop {
                        tokio::select! {
                            _ = shutdown_rx.recv() => break,
                            result = listener.accept() => {
                                if let Ok((stream, _)) = result {
                                    let events = events_clone.clone();
                                    let event_tx = event_tx_clone.clone();
                                    let event_rx = event_tx_clone.subscribe();
                                    tokio::spawn(handle_connection(stream, events, event_tx, event_rx));
                                }
                            }
                        }
                    }
                });
            });

            std::thread::sleep(std::time::Duration::from_millis(100));
            relay
        }

        pub fn url(&self) -> String {
            format!("ws://127.0.0.1:{}", self.port)
        }

        pub fn stop(&mut self) {
            if self.stopped {
                return;
            }
            self.stopped = true;
            let _ = self.shutdown.send(());
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    impl Drop for TestRelay {
        fn drop(&mut self) {
            self.stop();
        }
    }

    async fn handle_connection(
        stream: TcpStream,
        events: Arc<RwLock<HashMap<String, serde_json::Value>>>,
        event_tx: broadcast::Sender<serde_json::Value>,
        mut event_rx: broadcast::Receiver<serde_json::Value>,
    ) {
        let ws_stream = match accept_async(stream).await {
            Ok(s) => s,
            Err(_) => return,
        };

        let (write, mut read) = ws_stream.split();
        let write = Arc::new(tokio::sync::Mutex::new(write));

        let subscriptions: Arc<RwLock<HashMap<String, Vec<StoredFilter>>>> =
            Arc::new(RwLock::new(HashMap::new()));

        let write_clone = write.clone();
        let subs_clone = subscriptions.clone();
        let broadcast_task = tokio::spawn(async move {
            loop {
                match event_rx.recv().await {
                    Ok(event) => {
                        let subs = subs_clone.read().await;
                        for (_, filters) in subs.iter() {
                            for filter in filters {
                                if filter.matches(&event) {
                                    let event_msg =
                                        serde_json::json!(["EVENT", &filter.sub_id, &event]);
                                    let mut w = write_clone.lock().await;
                                    let _ = w.send(Message::Text(event_msg.to_string())).await;
                                    break;
                                }
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        while let Some(msg) = read.next().await {
            let msg = match msg {
                Ok(Message::Text(t)) => t,
                Ok(Message::Close(_)) => break,
                Ok(Message::Ping(data)) => {
                    let mut w = write.lock().await;
                    let _ = w.send(Message::Pong(data)).await;
                    continue;
                }
                _ => continue,
            };

            let parsed: Result<Vec<serde_json::Value>, _> = serde_json::from_str(&msg);
            let parsed = match parsed {
                Ok(p) => p,
                Err(_) => continue,
            };

            if parsed.is_empty() {
                continue;
            }

            let msg_type = parsed[0].as_str().unwrap_or("");

            match msg_type {
                "EVENT" => {
                    if parsed.len() >= 2 {
                        let event = parsed[1].clone();
                        if let Some(id) = event.get("id").and_then(|v| v.as_str()) {
                            events.write().await.insert(id.to_string(), event.clone());

                            let ok_msg = serde_json::json!(["OK", id, true, ""]);
                            {
                                let mut w = write.lock().await;
                                let _ = w.send(Message::Text(ok_msg.to_string())).await;
                            }

                            let _ = event_tx.send(event);
                        }
                    }
                }
                "REQ" => {
                    if parsed.len() >= 3 {
                        let sub_id = parsed[1].as_str().unwrap_or("sub").to_string();

                        let mut filters = Vec::new();
                        for filter in parsed.iter().skip(2) {
                            let kind = filter
                                .get("kinds")
                                .and_then(|k| k.as_array())
                                .and_then(|a| a.first())
                                .and_then(|v| v.as_u64());

                            let authors: Vec<String> = filter
                                .get("authors")
                                .and_then(|a| a.as_array())
                                .map(|arr| {
                                    arr.iter()
                                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                        .collect()
                                })
                                .unwrap_or_default();

                            let p_tag = filter
                                .get("#p")
                                .and_then(|p| p.as_array())
                                .and_then(|a| a.first())
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());

                            let l_tag = filter
                                .get("#l")
                                .and_then(|l| l.as_array())
                                .and_then(|a| a.first())
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());

                            filters.push(StoredFilter {
                                sub_id: sub_id.clone(),
                                kind,
                                authors,
                                p_tag,
                                l_tag,
                            });
                        }

                        subscriptions
                            .write()
                            .await
                            .insert(sub_id.clone(), filters.clone());

                        let events_lock = events.read().await;
                        let mut w = write.lock().await;

                        for event in events_lock.values() {
                            for filter in &filters {
                                if filter.matches(event) {
                                    let event_msg = serde_json::json!(["EVENT", &sub_id, event]);
                                    let _ = w.send(Message::Text(event_msg.to_string())).await;
                                    break;
                                }
                            }
                        }
                        drop(events_lock);

                        let eose = serde_json::json!(["EOSE", &sub_id]);
                        let _ = w.send(Message::Text(eose.to_string())).await;
                    }
                }
                "CLOSE" => {
                    if parsed.len() >= 2 {
                        if let Some(sub_id) = parsed[1].as_str() {
                            subscriptions.write().await.remove(sub_id);
                        }
                    }
                }
                _ => {}
            }
        }

        broadcast_task.abort();
    }
}

#[allow(dead_code)]
struct TestInstance {
    _data_dir: TempDir,
    process: Option<Child>,
    data_path: PathBuf,
    home_dir: PathBuf,
    config_dir: PathBuf,
    addr: String,
    pubkey_hex: String,
}

impl TestInstance {
    fn new_without_server() -> Self {
        let data_dir = TempDir::new().expect("Failed to create temp dir");
        let data_path = data_dir.path().to_path_buf();
        let home_dir = data_dir.path().to_path_buf();
        let config_dir = home_dir.join(".hashtree");
        std::fs::create_dir_all(&config_dir).expect("Failed to create config dir");
        write_test_config_with_relays(&config_dir, &[]).expect("Failed to write test config");

        TestInstance {
            _data_dir: data_dir,
            process: None,
            data_path,
            home_dir,
            config_dir,
            addr: String::new(),
            pubkey_hex: String::new(),
        }
    }

    fn run_command(&self, htree_bin: &str, args: &[&str]) -> std::process::Output {
        Command::new(htree_bin)
            .arg("--data-dir")
            .arg(&self.data_path)
            .args(args)
            .env("HOME", &self.home_dir)
            .env("HTREE_CONFIG_DIR", &self.config_dir)
            .output()
            .expect("Failed to run htree command")
    }
}

impl Drop for TestInstance {
    fn drop(&mut self) {
        if let Some(ref mut process) = self.process {
            let _ = process.kill();
            let _ = process.wait();
        }
    }
}

struct DaemonInstance {
    _home_dir: TempDir,
    data_path: PathBuf,
    config_dir: PathBuf,
    pid_file: PathBuf,
    log_file: PathBuf,
    pubkey_hex: String,
    addr: String,
    htree_bin: PathBuf,
    pid: i32,
}

impl DaemonInstance {
    fn new(
        port: u16,
        htree_bin: &PathBuf,
        keys: &Keys,
        follow_pubkeys: &[String],
        relay_url: &str,
    ) -> Result<Self> {
        Self::new_with_relays(
            port,
            htree_bin,
            keys,
            follow_pubkeys,
            &[relay_url.to_string()],
        )
    }

    fn new_with_relays(
        port: u16,
        htree_bin: &PathBuf,
        keys: &Keys,
        follow_pubkeys: &[String],
        relay_urls: &[String],
    ) -> Result<Self> {
        let home_dir = TempDir::new().context("Failed to create temp dir")?;
        let home_path = home_dir.path().to_path_buf();
        let data_path = home_path.join("data");
        fs::create_dir_all(&data_path).context("Failed to create data dir")?;

        let config_dir = home_path.join(".hashtree");
        fs::create_dir_all(&config_dir).context("Failed to create config dir")?;
        write_test_config_with_relays(&config_dir, relay_urls)?;

        let nsec = keys
            .secret_key()
            .to_bech32()
            .context("Failed to encode nsec")?;
        fs::write(config_dir.join("keys"), &nsec).context("Failed to write keys")?;

        if !follow_pubkeys.is_empty() {
            let contacts_json =
                serde_json::to_string(follow_pubkeys).context("Failed to serialize contacts")?;
            fs::write(data_path.join("contacts.json"), &contacts_json)
                .context("Failed to write contacts.json")?;
        }

        let addr = format!("127.0.0.1:{}", port);
        let pid_file = home_path.join(format!("htree-{}.pid", port));
        let log_file = home_path.join(format!("htree-{}.log", port));

        let output = start_daemon_process(
            htree_bin,
            &data_path,
            &addr,
            &pid_file,
            &log_file,
            &home_path,
            &config_dir,
        )?;
        if !output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("htree start failed: {}\n{}", stdout, stderr);
        }

        let pid = wait_for_pid_file(&pid_file, Duration::from_secs(5))?;
        wait_for_health(&addr, Duration::from_secs(10))?;

        Ok(Self {
            _home_dir: home_dir,
            data_path,
            config_dir,
            pid_file,
            log_file,
            pubkey_hex: keys.public_key().to_hex(),
            addr,
            htree_bin: htree_bin.clone(),
            pid,
        })
    }

    fn stop(&mut self) {
        let _ = Command::new(&self.htree_bin)
            .arg("stop")
            .arg("--pid-file")
            .arg(&self.pid_file)
            .env("HOME", self._home_dir.path())
            .env("HTREE_CONFIG_DIR", &self.config_dir)
            .output();

        if self.pid > 0 && is_process_running(self.pid) {
            unsafe {
                let _ = libc::kill(self.pid, libc::SIGKILL);
            }
        }
        let _ = fs::remove_file(&self.pid_file);
        self.pid = 0;
    }

    fn start(&mut self) -> Result<()> {
        let output = start_daemon_process(
            &self.htree_bin,
            &self.data_path,
            &self.addr,
            &self.pid_file,
            &self.log_file,
            self._home_dir.path(),
            &self.config_dir,
        )?;

        if !output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("htree restart failed: {}\n{}", stdout, stderr);
        }

        self.pid = wait_for_pid_file(&self.pid_file, Duration::from_secs(5))?;
        wait_for_health(&self.addr, Duration::from_secs(10))?;
        Ok(())
    }

    fn restart(&mut self) -> Result<()> {
        self.stop();
        self.start()
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

fn start_daemon_process(
    htree_bin: &PathBuf,
    data_path: &PathBuf,
    addr: &str,
    pid_file: &PathBuf,
    log_file: &PathBuf,
    home_path: &std::path::Path,
    config_dir: &PathBuf,
) -> Result<std::process::Output> {
    let output = Command::new(htree_bin)
        .arg("--data-dir")
        .arg(data_path)
        .arg("start")
        .arg("--addr")
        .arg(addr)
        .arg("--daemon")
        .arg("--pid-file")
        .arg(pid_file)
        .arg("--log-file")
        .arg(log_file)
        .env("HOME", home_path)
        .env("HTREE_CONFIG_DIR", config_dir)
        .env("RUST_LOG", "warn")
        .output()
        .context("Failed to start htree daemon")?;
    Ok(output)
}

impl Drop for DaemonInstance {
    fn drop(&mut self) {
        self.stop();
    }
}

fn find_htree_binary() -> PathBuf {
    // Try to find the htree binary in target/debug or target/release
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace_root = PathBuf::from(manifest_dir)
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();

    let debug_bin = workspace_root.join("target/debug/htree");
    let release_bin = workspace_root.join("target/release/htree");

    if debug_bin.exists() {
        debug_bin
    } else if release_bin.exists() {
        release_bin
    } else {
        panic!(
            "htree binary not found. Run `cargo build --bin htree` first.\n\
             Looked in:\n  - {:?}\n  - {:?}",
            debug_bin, release_bin
        );
    }
}

fn find_free_port() -> Result<u16> {
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").context("Failed to bind ephemeral test port")?;
    let port = listener
        .local_addr()
        .context("Failed to read ephemeral test port")?
        .port();
    Ok(port)
}

fn find_unique_free_ports(count: usize) -> Result<Vec<u16>> {
    let mut ports = Vec::with_capacity(count);
    while ports.len() < count {
        let port = find_free_port()?;
        if !ports.contains(&port) {
            ports.push(port);
        }
    }
    Ok(ports)
}

fn create_test_directory() -> TempDir {
    let dir = TempDir::new().expect("Failed to create test data dir");

    // Create test files
    let path = dir.path();
    std::fs::create_dir_all(path.join("subdir")).unwrap();
    std::fs::write(path.join("file1.txt"), "Hello from file 1\n").unwrap();
    std::fs::write(path.join("file2.txt"), "Hello from file 2\n").unwrap();
    std::fs::write(path.join("subdir/nested.txt"), "Nested content\n").unwrap();
    std::fs::write(path.join("data.json"), r#"{"key": "value", "number": 42}"#).unwrap();

    dir
}

fn write_test_config_with_relays(
    config_dir: &std::path::Path,
    relay_urls: &[String],
) -> Result<()> {
    let relays = if relay_urls.is_empty() {
        "[]".to_string()
    } else {
        let quoted: Vec<String> = relay_urls
            .iter()
            .map(|url| format!("\"{}\"", url))
            .collect();
        format!("[{}]", quoted.join(", "))
    };
    let config_content = format!(
        r#"
[server]
enable_auth = false
stun_port = 0
enable_webrtc = false
enable_multicast = false
max_multicast_peers = 0
enable_wifi_aware = false
max_wifi_aware_peers = 0
enable_bluetooth = false
max_bluetooth_peers = 0
public_writes = true

[storage]
backend = "{backend}"
max_size_gb = {max_size_gb}

[nostr]
relays = {relays}

[blossom]
servers = []
read_servers = []
write_servers = []

[sync]
enabled = false
"#,
        backend = TEST_STORAGE_BACKEND,
        max_size_gb = TEST_STORAGE_MAX_SIZE_GB,
    );
    fs::write(config_dir.join("config.toml"), config_content).context("Failed to write config")?;
    Ok(())
}

fn wait_for_pid_file(path: &std::path::Path, timeout: Duration) -> Result<i32> {
    let deadline = Instant::now() + timeout;
    loop {
        if path.exists() {
            let pid = read_pid_file(path)?;
            return Ok(pid);
        }
        if Instant::now() >= deadline {
            anyhow::bail!("Timed out waiting for pid file {}", path.display());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn read_pid_file(path: &std::path::Path) -> Result<i32> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("Failed to read pid file {}", path.display()))?;
    let pid: i32 = contents.trim().parse().context("Invalid pid file")?;
    if pid <= 0 {
        anyhow::bail!("PID must be positive");
    }
    Ok(pid)
}

fn is_process_running(pid: i32) -> bool {
    let result = unsafe { libc::kill(pid, 0) };
    if result == 0 {
        return true;
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(code) if code == libc::ESRCH => false,
        Some(code) if code == libc::EPERM => true,
        _ => false,
    }
}

fn wait_for_health(addr: &str, timeout: Duration) -> Result<()> {
    let url = format!("http://{}/health", addr);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .context("Failed to build HTTP client")?;
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        if let Ok(resp) = client.get(&url).send() {
            if resp.status().is_success() {
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    anyhow::bail!("Daemon did not become healthy at {}", addr);
}

fn fetch_bytes(url: &str) -> Result<Vec<u8>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .context("Failed to build HTTP client")?;
    let resp = client.get(url).send().context("HTTP request failed")?;
    let resp = resp
        .error_for_status()
        .context("Non-success HTTP response")?;
    let bytes = resp.bytes().context("Failed to read response body")?;
    Ok(bytes.to_vec())
}

fn extract_cid(text: &str) -> Option<String> {
    // First try to find nhash format (preferred)
    // Note: output may be "nhash1.../filename" URL format, extract just the nhash part
    if let Some(nhash) = text.lines().find_map(|line| {
        line.split_whitespace()
            .find(|word| word.starts_with("nhash1"))
            .map(|s| {
                // Strip /filename suffix if present
                if let Some(slash_pos) = s.find('/') {
                    s[..slash_pos].to_string()
                } else {
                    s.to_string()
                }
            })
    }) {
        return Some(nhash);
    }
    // Fall back to 64-char hex format
    text.lines().find_map(|line| {
        line.split_whitespace()
            .find(|word| word.len() == 64 && word.chars().all(|c| c.is_ascii_hexdigit()))
            .map(|s| s.to_string())
    })
}

#[test]
fn test_status_command_reports_running_daemon() -> Result<()> {
    let htree_bin = find_htree_binary();
    let keys = Keys::generate();
    let port = find_free_port()?;
    let no_follows = Vec::<String>::new();
    let no_relays = Vec::<String>::new();

    let daemon = DaemonInstance::new_with_relays(port, &htree_bin, &keys, &no_follows, &no_relays)
        .context("Failed to start daemon for status test")?;

    let output = Command::new(&htree_bin)
        .arg("status")
        .arg("--addr")
        .arg(&daemon.addr)
        .env("HOME", daemon._home_dir.path())
        .env("HTREE_CONFIG_DIR", &daemon.config_dir)
        .output()
        .context("Failed to run htree status")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "htree status failed\nstdout:\n{}\nstderr:\n{}",
        stdout,
        stderr
    );
    assert!(
        stdout.contains("Daemon Status:"),
        "status output missing header:\n{}",
        stdout
    );
    assert!(
        stdout.contains("Status: running"),
        "status output missing running state:\n{}",
        stdout
    );

    Ok(())
}

#[test]
fn test_local_add_and_get() {
    // Simpler test: just verify add and get work on a single instance (no server)
    let htree_bin = find_htree_binary();
    let htree_bin_str = htree_bin.to_str().unwrap();

    let test_data = create_test_directory();
    let instance = TestInstance::new_without_server();

    // Add directory (--local to skip file server push in tests)
    let add_output = instance.run_command(
        htree_bin_str,
        &[
            "add",
            test_data.path().to_str().unwrap(),
            "--unencrypted",
            "--local",
        ],
    );

    let add_stdout = String::from_utf8_lossy(&add_output.stdout);
    let add_stderr = String::from_utf8_lossy(&add_output.stderr);
    assert!(
        add_output.status.success(),
        "htree add failed\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        add_output.status,
        add_stdout,
        add_stderr
    );
    println!("Add output: {}", add_stdout);

    // Extract CID
    let cid = extract_cid(&add_stdout).expect("Failed to extract CID");
    println!("CID: {}", cid);

    // Get directory
    let output_dir = TempDir::new().expect("Failed to create output dir");
    let output_path = output_dir.path().join("retrieved");

    let get_output = instance.run_command(
        htree_bin_str,
        &["get", &cid, "-o", output_path.to_str().unwrap()],
    );

    println!(
        "Get output: {}",
        String::from_utf8_lossy(&get_output.stdout)
    );
    println!(
        "Get stderr: {}",
        String::from_utf8_lossy(&get_output.stderr)
    );

    // Verify
    assert!(output_path.exists(), "Output path should exist");

    let original = std::fs::read_to_string(test_data.path().join("file1.txt")).unwrap();
    let retrieved = std::fs::read_to_string(output_path.join("file1.txt")).unwrap();
    assert_eq!(original, retrieved, "Content should match");

    println!("Local add/get test PASSED!");
}
