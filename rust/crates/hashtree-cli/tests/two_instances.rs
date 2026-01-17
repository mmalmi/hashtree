//! Integration test: Two hashtree instances with P2P data transfer
//!
//! This test spawns two htree daemons with separate data directories,
//! has instance A add a file, then has instance B retrieve it.
//!
//! The instances are configured to "follow" each other via contacts.json,
//! which puts them in the "Follows" peer pool for priority connection.
//!
//! Run with: cargo test --package hashtree-cli --test two_instances -- --nocapture

use anyhow::{Context, Result};
use hashtree_cli::HashtreeStore;
use nostr::{Keys, ToBech32};
use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

mod test_relay {
    use std::collections::HashMap;
    use std::net::TcpListener;
    use std::sync::Arc;
    use futures::{SinkExt, StreamExt};
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
                let has_p = event.get("tags")
                    .and_then(|t| t.as_array())
                    .map(|tags| {
                        tags.iter().any(|tag| {
                            tag.as_array().map(|arr| {
                                arr.len() >= 2 &&
                                arr[0].as_str() == Some("p") &&
                                arr[1].as_str() == Some(p.as_str())
                            }).unwrap_or(false)
                        })
                    })
                    .unwrap_or(false);
                if !has_p {
                    return false;
                }
            }

            if let Some(ref l) = self.l_tag {
                let has_l = event.get("tags")
                    .and_then(|t| t.as_array())
                    .map(|tags| {
                        tags.iter().any(|tag| {
                            tag.as_array().map(|arr| {
                                arr.len() >= 2 &&
                                arr[0].as_str() == Some("l") &&
                                arr[1].as_str() == Some(l.as_str())
                            }).unwrap_or(false)
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
    }

    impl TestRelay {
        pub fn new(port: u16) -> Self {
            let events: Arc<RwLock<HashMap<String, serde_json::Value>>> = Arc::new(RwLock::new(HashMap::new()));
            let (shutdown, _) = broadcast::channel(1);
            let (event_tx, _) = broadcast::channel::<serde_json::Value>(1000);

            let relay = TestRelay {
                port,
                shutdown: shutdown.clone(),
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
                    let listener = TcpListener::bind(format!("127.0.0.1:{}", port)).unwrap();
                    listener.set_nonblocking(true).unwrap();
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
    }

    impl Drop for TestRelay {
        fn drop(&mut self) {
            let _ = self.shutdown.send(());
            std::thread::sleep(std::time::Duration::from_millis(100));
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

        let subscriptions: Arc<RwLock<HashMap<String, Vec<StoredFilter>>>> = Arc::new(RwLock::new(HashMap::new()));

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
                                    let event_msg = serde_json::json!(["EVENT", &filter.sub_id, &event]);
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
                        for i in 2..parsed.len() {
                            let filter = &parsed[i];

                            let kind = filter.get("kinds")
                                .and_then(|k| k.as_array())
                                .and_then(|a| a.first())
                                .and_then(|v| v.as_u64());

                            let authors: Vec<String> = filter.get("authors")
                                .and_then(|a| a.as_array())
                                .map(|arr| arr.iter()
                                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                    .collect())
                                .unwrap_or_default();

                            let p_tag = filter.get("#p")
                                .and_then(|p| p.as_array())
                                .and_then(|a| a.first())
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());

                            let l_tag = filter.get("#l")
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

                        subscriptions.write().await.insert(sub_id.clone(), filters.clone());

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

struct TestInstance {
    _data_dir: TempDir,
    process: Option<Child>,
    data_path: PathBuf,
    home_dir: PathBuf,
    pubkey_hex: String,
}

impl TestInstance {
    /// Create a new test instance with pre-generated keys
    /// The `follow_pubkeys` parameter specifies other instance pubkeys to follow (for peer classification)
    fn new(port: u16, htree_bin: &str, keys: &Keys, follow_pubkeys: &[String]) -> Self {
        let data_dir = TempDir::new().expect("Failed to create temp dir");
        let data_path = data_dir.path().to_path_buf();
        let home_dir = data_dir.path().to_path_buf();

        // Create .hashtree config dir
        let config_dir = home_dir.join(".hashtree");
        std::fs::create_dir_all(&config_dir).expect("Failed to create config dir");

        // Create config - use relays that don't require PoW
        let config_content = r#"
[server]
enable_auth = false
stun_port = 0

[nostr]
relays = ["wss://temp.iris.to", "wss://relay.damus.io", "wss://relay.snort.social"]
crawl_depth = 0
"#;
        std::fs::write(config_dir.join("config.toml"), config_content)
            .expect("Failed to write config");

        // Write pre-generated keys file
        let nsec = keys.secret_key().to_bech32().expect("Failed to encode nsec");
        std::fs::write(config_dir.join("keys"), &nsec)
            .expect("Failed to write keys");

        // Write contacts.json with follow_pubkeys so peer classifier puts them in Follows pool
        if !follow_pubkeys.is_empty() {
            let contacts_json = serde_json::to_string(&follow_pubkeys)
                .expect("Failed to serialize contacts");
            std::fs::write(data_dir.path().join("contacts.json"), &contacts_json)
                .expect("Failed to write contacts.json");
        }

        let pubkey_hex = keys.public_key().to_hex();

        let process = Command::new(htree_bin)
            .arg("--data-dir")
            .arg(data_dir.path())
            .arg("start")
            .arg("--addr")
            .arg(format!("127.0.0.1:{}", port))
            .env("HOME", &home_dir)
            .env("RUST_LOG", "warn,hashtree_cli::webrtc::signaling=info")
            .stdout(Stdio::null())
            .stderr(Stdio::inherit()) // Show errors on stderr
            .spawn()
            .expect("Failed to start htree instance");

        TestInstance {
            _data_dir: data_dir,
            process: Some(process),
            data_path,
            home_dir,
            pubkey_hex,
        }
    }

    fn new_without_server() -> Self {
        let data_dir = TempDir::new().expect("Failed to create temp dir");
        let data_path = data_dir.path().to_path_buf();
        let home_dir = data_dir.path().to_path_buf();

        TestInstance {
            _data_dir: data_dir,
            process: None,
            data_path,
            home_dir,
            pubkey_hex: String::new(),
        }
    }

    fn run_command(&self, htree_bin: &str, args: &[&str]) -> std::process::Output {
        Command::new(htree_bin)
            .arg("--data-dir")
            .arg(&self.data_path)
            .args(args)
            .env("HOME", &self.home_dir)
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
        let home_dir = TempDir::new().context("Failed to create temp dir")?;
        let home_path = home_dir.path().to_path_buf();
        let data_path = home_path.join("data");
        fs::create_dir_all(&data_path).context("Failed to create data dir")?;

        let config_dir = home_path.join(".hashtree");
        fs::create_dir_all(&config_dir).context("Failed to create config dir")?;
        write_test_config(&config_dir, relay_url)?;

        let nsec = keys.secret_key().to_bech32().context("Failed to encode nsec")?;
        fs::write(config_dir.join("keys"), &nsec).context("Failed to write keys")?;

        if !follow_pubkeys.is_empty() {
            let contacts_json = serde_json::to_string(follow_pubkeys)
                .context("Failed to serialize contacts")?;
            fs::write(data_path.join("contacts.json"), &contacts_json)
                .context("Failed to write contacts.json")?;
        }

        let addr = format!("127.0.0.1:{}", port);
        let pid_file = home_path.join(format!("htree-{}.pid", port));
        let log_file = home_path.join(format!("htree-{}.log", port));

        let output = Command::new(htree_bin)
            .arg("--data-dir")
            .arg(&data_path)
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
            .context("Failed to start htree daemon")?;

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
            pubkey_hex: keys.public_key().to_hex(),
            addr,
            htree_bin: htree_bin.clone(),
            pid,
        })
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for DaemonInstance {
    fn drop(&mut self) {
        let _ = Command::new(&self.htree_bin)
            .arg("stop")
            .arg("--pid-file")
            .arg(&self.pid_file)
            .env("HOME", self._home_dir.path())
            .env("HTREE_CONFIG_DIR", &self.config_dir)
            .output();

        if is_process_running(self.pid) {
            unsafe {
                let _ = libc::kill(self.pid, libc::SIGKILL);
            }
            let _ = fs::remove_file(&self.pid_file);
        }
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

fn create_test_directory() -> TempDir {
    let dir = TempDir::new().expect("Failed to create test data dir");

    // Create test files
    let path = dir.path();
    std::fs::create_dir_all(path.join("subdir")).unwrap();
    std::fs::write(path.join("file1.txt"), "Hello from file 1\n").unwrap();
    std::fs::write(path.join("file2.txt"), "Hello from file 2\n").unwrap();
    std::fs::write(path.join("subdir/nested.txt"), "Nested content\n").unwrap();
    std::fs::write(
        path.join("data.json"),
        r#"{"key": "value", "number": 42}"#,
    )
    .unwrap();

    dir
}

fn write_test_config(config_dir: &std::path::Path, relay_url: &str) -> Result<()> {
    let config_content = format!(
        r#"
[server]
enable_auth = false
stun_port = 0
enable_webrtc = true
public_writes = true

[nostr]
relays = ["{relay_url}"]

[blossom]
servers = []
read_servers = []
write_servers = []

[sync]
enabled = false
"#
    );
    fs::write(config_dir.join("config.toml"), config_content)
        .context("Failed to write config")?;
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

fn wait_for_peer_data_channel(addr: &str, peer_pubkey: &str, timeout: Duration) -> Result<()> {
    let url = format!("http://{}/api/peers", addr);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .context("Failed to build HTTP client")?;
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        if let Ok(resp) = client.get(&url).send() {
            if let Ok(json) = resp.json::<serde_json::Value>() {
                if let Some(peers) = json.get("peers").and_then(|p| p.as_array()) {
                    let matched = peers.iter().any(|peer| {
                        peer.get("pubkey").and_then(|p| p.as_str()) == Some(peer_pubkey)
                            && peer.get("has_data_channel").and_then(|d| d.as_bool()) == Some(true)
                    });
                    if matched {
                        return Ok(());
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    anyhow::bail!("Timed out waiting for peer data channel on {}", addr);
}

fn fetch_bytes(url: &str) -> Result<Vec<u8>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .context("Failed to build HTTP client")?;
    let resp = client.get(url).send().context("HTTP request failed")?;
    let resp = resp.error_for_status().context("Non-success HTTP response")?;
    let bytes = resp.bytes().context("Failed to read response body")?;
    Ok(bytes.to_vec())
}

#[test]
#[ignore = "requires external Nostr relays and network connectivity - run manually with --ignored"]
fn test_two_instances_discover_and_sync() {
    let htree_bin = find_htree_binary();
    let htree_bin_str = htree_bin.to_str().unwrap();

    println!("Using htree binary: {:?}", htree_bin);

    // Create test data
    let test_data = create_test_directory();
    println!("Test data directory: {:?}", test_data.path());

    // Pre-generate keys for both instances so they can follow each other
    let keys_a = Keys::generate();
    let keys_b = Keys::generate();
    let pubkey_a = keys_a.public_key().to_hex();
    let pubkey_b = keys_b.public_key().to_hex();

    println!("Instance A pubkey: {}", pubkey_a);
    println!("Instance B pubkey: {}", pubkey_b);

    // Start two instances with servers for WebRTC (each has its own data directory)
    // Each instance follows the other to prioritize peer connections in "Follows" pool
    println!("\nStarting Instance A on port 18081 (follows B)...");
    let instance_a = TestInstance::new(18081, htree_bin_str, &keys_a, &[pubkey_b.clone()]);
    println!("Instance A data dir: {:?}", instance_a.data_path);
    std::thread::sleep(Duration::from_secs(5));

    println!("\nStarting Instance B on port 18082 (follows A)...");
    let instance_b = TestInstance::new(18082, htree_bin_str, &keys_b, &[pubkey_a.clone()]);
    println!("Instance B data dir: {:?}", instance_b.data_path);
    std::thread::sleep(Duration::from_secs(5));

    // Verify they have different data directories
    assert_ne!(instance_a.data_path, instance_b.data_path,
        "Instances must have different data directories");

    // Add directory on instance A via HTTP upload (not CLI, so server sees it)
    println!("\nAdding directory on Instance A via HTTP upload...");

    // Create a simple file to upload
    let test_file = test_data.path().join("file1.txt");
    let add_output = Command::new("curl")
        .arg("-s")
        .arg("-X").arg("POST")
        .arg("-F").arg(format!("file=@{}", test_file.display()))
        .arg("http://127.0.0.1:18081/upload")
        .output()
        .expect("Failed to upload file");

    let add_stdout = String::from_utf8_lossy(&add_output.stdout);
    let add_stderr = String::from_utf8_lossy(&add_output.stderr);
    println!("Upload response: {}", add_stdout);
    if !add_stderr.is_empty() {
        println!("Upload stderr: {}", add_stderr);
    }

    // Extract CID from JSON response (e.g., {"hash":"abc123..."})
    let cid = add_stdout
        .split('"')
        .find(|s| s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit()))
        .map(|s| s.to_string());

    let cid = match cid {
        Some(c) => c,
        None => {
            println!("Could not extract CID from upload response");
            panic!("Failed to extract CID from upload output: {}", add_stdout);
        }
    };

    println!("Uploaded with CID: {}", cid);

    // Pin on instance A via HTTP API
    println!("\nPinning on Instance A...");
    let pin_output = Command::new("curl")
        .arg("-s")
        .arg("-X").arg("POST")
        .arg(format!("http://127.0.0.1:18081/api/pin/{}", cid))
        .output()
        .expect("Failed to pin");
    println!("Pin response: {}", String::from_utf8_lossy(&pin_output.stdout));

    // Verify data is stored on instance A
    println!("\nVerifying data on Instance A...");
    let pins_a = instance_a.run_command(htree_bin_str, &["pins"]);
    println!("Pins A: {}", String::from_utf8_lossy(&pins_a.stdout));

    // Verify servers are running by checking their /api/stats endpoint
    println!("\nVerifying servers are responding...");
    let check_a = Command::new("curl")
        .arg("-s")
        .arg("http://127.0.0.1:18081/api/stats")
        .output();
    println!("Instance A stats: {}", check_a.map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_else(|e| e.to_string()));

    let check_b = Command::new("curl")
        .arg("-s")
        .arg("http://127.0.0.1:18082/api/stats")
        .output();
    println!("Instance B stats: {}", check_b.map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_else(|e| e.to_string()));

    // Also verify Instance A can serve the content locally
    println!("\nVerifying Instance A can serve content via HTTP...");
    let check_content_a = Command::new("curl")
        .arg("-s")
        .arg("-w").arg("\nHTTP_CODE:%{http_code}")
        .arg(format!("http://127.0.0.1:18081/{}", cid))
        .output();
    println!("Instance A content check: {}", check_content_a.map(|o| {
        format!("stdout={} stderr={}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr))
    }).unwrap_or_else(|e| e.to_string()));

    // Wait for P2P discovery and sync with peer status checking
    // Hello messages sent every 10s, need time for: discovery -> offer/answer -> ICE -> connect
    println!("\nWaiting for P2P discovery and sync...");

    // Wait until Instance B has Instance A connected with data channel
    // This is required for B to fetch content from A via P2P
    let mut b_has_a_datachannel = false;
    for wait_attempt in 1..=24 {
        std::thread::sleep(Duration::from_secs(5));

        // Check peers on both instances
        let peers_a = Command::new("curl")
            .arg("-s")
            .arg("http://127.0.0.1:18081/api/peers")
            .output();
        let peers_b = Command::new("curl")
            .arg("-s")
            .arg("http://127.0.0.1:18082/api/peers")
            .output();

        let peers_a_json = peers_a.map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
        let peers_b_json = peers_b.map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();

        println!("  {} seconds - Instance A peers: {}", wait_attempt * 5, peers_a_json);
        println!("  {} seconds - Instance B peers: {}", wait_attempt * 5, peers_b_json);

        // Parse Instance B's peers to check if A is connected with data channel
        // We need to check that the specific peer entry for A has has_data_channel: true
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&peers_b_json) {
            if let Some(peers) = json.get("peers").and_then(|p| p.as_array()) {
                for peer in peers {
                    let has_pubkey_a = peer.get("pubkey")
                        .and_then(|p| p.as_str())
                        .map(|s| s == pubkey_a)
                        .unwrap_or(false);
                    let has_data_channel = peer.get("has_data_channel")
                        .and_then(|d| d.as_bool())
                        .unwrap_or(false);

                    if has_pubkey_a && has_data_channel {
                        println!("  Instance B has data channel to Instance A!");
                        b_has_a_datachannel = true;
                        break;
                    }
                }
            }
        }

        if b_has_a_datachannel {
            break;
        }
    }

    if !b_has_a_datachannel {
        println!("\nWARNING: Instance B did not establish data channel to Instance A after 120 seconds");
        println!("This may be due to relay issues or network configuration");
    }

    // Try to get from instance B via HTTP API (which uses P2P if not local)
    // The CLI 'get' command only checks local storage, but HTTP server fetches from peers
    let mut success = false;
    let mut retrieved_content = String::new();

    for attempt in 1..=10 {
        println!("\nAttempt {}/10: Fetching via Instance B's HTTP API...", attempt);

        // Use curl to fetch from instance B's server (with verbose HTTP code output)
        let curl_output = Command::new("curl")
            .arg("-s")
            .arg("-w").arg("\n__HTTP_CODE:%{http_code}__")
            .arg(format!("http://127.0.0.1:18082/{}", cid))
            .output();

        match curl_output {
            Ok(output) => {
                let full_output = String::from_utf8_lossy(&output.stdout);
                println!("Response: {}", full_output);

                // Check if we got the content (HTTP 200)
                if full_output.contains("__HTTP_CODE:200__") {
                    retrieved_content = full_output.replace("\n__HTTP_CODE:200__", "").to_string();
                    println!("Got content ({} bytes): {}", retrieved_content.len(), &retrieved_content[..50.min(retrieved_content.len())]);
                    success = true;
                    break;
                }
            }
            Err(e) => {
                println!("curl error: {}", e);
            }
        }

        if attempt < 10 {
            println!("Waiting 5 more seconds...");
            std::thread::sleep(Duration::from_secs(5));
        }
    }

    // MUST succeed - this is the whole point of the test
    assert!(success, "Instance B MUST be able to get content from Instance A via P2P");

    println!("\n=== SUCCESS: Content retrieved via P2P! ===");
    println!("Retrieved {} bytes", retrieved_content.len());

    println!("\nTest completed!");
}

fn extract_cid(text: &str) -> Option<String> {
    // First try to find nhash format (preferred)
    // Note: output may be "nhash1.../filename" URL format, extract just the nhash part
    if let Some(nhash) = text.lines().find_map(|line| {
        line.split_whitespace().find(|word| word.starts_with("nhash1"))
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
        line.split_whitespace().find(|word| {
            word.len() == 64 && word.chars().all(|c| c.is_ascii_hexdigit())
        }).map(|s| s.to_string())
    })
}

#[test]
fn test_two_instances_connect_local_relay() -> Result<()> {
    let htree_bin = find_htree_binary();
    let relay = test_relay::TestRelay::new(19110);
    let relay_url = relay.url();

    let keys_a = Keys::generate();
    let keys_b = Keys::generate();
    let pubkey_a = keys_a.public_key().to_hex();
    let pubkey_b = keys_b.public_key().to_hex();

    let instance_a = DaemonInstance::new(18191, &htree_bin, &keys_a, &[pubkey_b], &relay_url)?;
    let instance_b = DaemonInstance::new(18192, &htree_bin, &keys_b, &[pubkey_a], &relay_url)?;

    assert_ne!(instance_a.pid_file, instance_b.pid_file);
    assert!(is_process_running(instance_a.pid));
    assert!(is_process_running(instance_b.pid));

    wait_for_peer_data_channel(&instance_a.addr, &instance_b.pubkey_hex, Duration::from_secs(30))?;
    wait_for_peer_data_channel(&instance_b.addr, &instance_a.pubkey_hex, Duration::from_secs(30))?;

    let expected = b"hello world\n".to_vec();
    let store = HashtreeStore::new(&instance_a.data_path)
        .context("Failed to open instance A store")?;
    let cid = store.put_blob(&expected).context("Failed to store blob")?;
    let url = format!("{}/{}", instance_b.base_url(), cid);

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(bytes) = fetch_bytes(&url) {
            if bytes == expected {
                break;
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!("Timed out waiting for peer fetch to succeed");
        }
        std::thread::sleep(Duration::from_millis(200));
    }

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
    let add_output = instance.run_command(htree_bin_str, &[
        "add", test_data.path().to_str().unwrap(), "--public", "--local"
    ]);

    let add_stdout = String::from_utf8_lossy(&add_output.stdout);
    println!("Add output: {}", add_stdout);

    // Extract CID
    let cid = extract_cid(&add_stdout).expect("Failed to extract CID");
    println!("CID: {}", cid);

    // Get directory
    let output_dir = TempDir::new().expect("Failed to create output dir");
    let output_path = output_dir.path().join("retrieved");

    let get_output = instance.run_command(htree_bin_str, &[
        "get", &cid, "-o", output_path.to_str().unwrap()
    ]);

    println!("Get output: {}", String::from_utf8_lossy(&get_output.stdout));
    println!("Get stderr: {}", String::from_utf8_lossy(&get_output.stderr));

    // Verify
    assert!(output_path.exists(), "Output path should exist");

    let original = std::fs::read_to_string(test_data.path().join("file1.txt")).unwrap();
    let retrieved = std::fs::read_to_string(output_path.join("file1.txt")).unwrap();
    assert_eq!(original, retrieved, "Content should match");

    println!("Local add/get test PASSED!");
}
