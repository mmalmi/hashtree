//! Shared test infrastructure for git-remote-htree integration tests
//!
//! Provides:
//! - TestRelay: In-memory nostr relay
//! - TestServer: Local blossom server
//! - TestEnv: Test environment with config and keys
//! - Helper functions for creating test repos

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use tempfile::TempDir;
use nostr::ToBech32;

/// Minimal in-memory nostr relay for testing with real-time event broadcasting
pub mod test_relay {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::net::TcpListener;
    use tokio::net::TcpStream;
    use tokio::sync::{broadcast, RwLock};
    use tokio_tungstenite::{accept_async, tungstenite::Message};
    use futures::{SinkExt, StreamExt};

    /// Stored filter for matching events
    #[derive(Clone)]
    struct StoredFilter {
        sub_id: String,
        kind: Option<u64>,
        authors: Vec<String>,
        p_tag: Option<String>,  // #p tag for directed messages
        l_tag: Option<String>,  // #l tag for hello messages
    }

    impl StoredFilter {
        fn matches(&self, event: &serde_json::Value) -> bool {
            // Check kind
            if let Some(k) = self.kind {
                if event.get("kind").and_then(|v| v.as_u64()) != Some(k) {
                    return false;
                }
            }

            // Check authors
            if !self.authors.is_empty() {
                let event_author = event.get("pubkey").and_then(|v| v.as_str()).unwrap_or("");
                if !self.authors.iter().any(|a| a == event_author) {
                    return false;
                }
            }

            // Check #p tag
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

            // Check #l tag
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
            // Broadcast channel for new events - larger buffer for busy relays
            let (event_tx, _) = broadcast::channel::<serde_json::Value>(1000);

            let relay = TestRelay {
                port,
                shutdown: shutdown.clone(),
            };

            // Start relay in background
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

            // Wait for relay to start
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
            // Give time for cleanup
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

        // Track active subscriptions for this connection
        let subscriptions: Arc<RwLock<HashMap<String, Vec<StoredFilter>>>> = Arc::new(RwLock::new(HashMap::new()));

        // Spawn task to handle incoming broadcast events
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
                                    break; // Only send once per subscription
                                }
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        // Handle incoming messages from client
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
                            // Store event
                            events.write().await.insert(id.to_string(), event.clone());

                            // Send OK response
                            let ok_msg = serde_json::json!(["OK", id, true, ""]);
                            {
                                let mut w = write.lock().await;
                                let _ = w.send(Message::Text(ok_msg.to_string())).await;
                            }

                            // Broadcast to all connections
                            let _ = event_tx.send(event);
                        }
                    }
                }
                "REQ" => {
                    if parsed.len() >= 3 {
                        let sub_id = parsed[1].as_str().unwrap_or("sub").to_string();

                        // Parse all filters (can have multiple)
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

                        // Store subscription
                        subscriptions.write().await.insert(sub_id.clone(), filters.clone());

                        // Send matching historical events
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

                        // Send EOSE
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

        // Clean up broadcast task
        broadcast_task.abort();
    }
}

/// Local blossom server for testing
pub struct TestServer {
    _data_dir: TempDir,
    _home_dir: TempDir,
    process: Child,
    port: u16,
}

impl TestServer {
    pub fn new(port: u16) -> Option<Self> {
        let htree_bin = find_htree_binary()?;
        let data_dir = TempDir::new().expect("Failed to create temp dir");
        let home_dir = TempDir::new().expect("Failed to create home dir");

        // Create .hashtree config dir for the server
        let config_dir = home_dir.path().join(".hashtree");
        std::fs::create_dir_all(&config_dir).expect("Failed to create config dir");

        // Server config - no auth for testing
        let config_content = r#"
[server]
enable_auth = false
stun_port = 0
enable_webrtc = false
public_writes = true

[nostr]
relays = []
"#;
        std::fs::write(config_dir.join("config.toml"), config_content)
            .expect("Failed to write config");

        // Generate keys for server
        let keys = nostr::Keys::generate();
        let nsec = keys.secret_key().to_bech32().expect("Failed to encode nsec");
        std::fs::write(config_dir.join("keys"), &nsec)
            .expect("Failed to write keys");

        let process = Command::new(&htree_bin)
            .arg("--data-dir")
            .arg(data_dir.path())
            .arg("start")
            .arg("--addr")
            .arg(format!("127.0.0.1:{}", port))
            .env("HOME", home_dir.path())
            .env("RUST_LOG", "warn")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("Failed to start htree server");

        // Wait for server to start
        std::thread::sleep(Duration::from_secs(2));

        Some(TestServer {
            _data_dir: data_dir,
            _home_dir: home_dir,
            process,
            port,
        })
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

fn find_htree_binary() -> Option<PathBuf> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace_root = PathBuf::from(manifest_dir)
        .parent()?
        .parent()?
        .to_path_buf();

    let debug_bin = workspace_root.join("target/debug/htree");
    let release_bin = workspace_root.join("target/release/htree");

    if release_bin.exists() {
        Some(release_bin)
    } else if debug_bin.exists() {
        Some(debug_bin)
    } else {
        None
    }
}

/// Test environment with config and keys
pub struct TestEnv {
    _data_dir: TempDir,
    pub home_dir: PathBuf,
    pub npub: String,
}

impl TestEnv {
    pub fn new(blossom_server: Option<&str>, nostr_relay: Option<&str>) -> Self {
        let data_dir = TempDir::new().expect("Failed to create temp dir");
        let home_dir = data_dir.path().to_path_buf();

        // Create .hashtree config dir
        let config_dir = home_dir.join(".hashtree");
        std::fs::create_dir_all(&config_dir).expect("Failed to create config dir");

        // Build config
        let relays = match nostr_relay {
            Some(url) => format!(r#"relays = ["{}"]"#, url),
            None => r#"relays = ["wss://temp.iris.to", "wss://relay.damus.io", "wss://nos.lol", "wss://relay.primal.net"]"#.to_string(),
        };

        let blossom = match blossom_server {
            Some(url) => format!(r#"
[blossom]
read_servers = ["{url}"]
write_servers = ["{url}"]
"#),
            None => String::new(),
        };

        let config_content = format!(r#"
[server]
enable_auth = false
stun_port = 0

[nostr]
{relays}
crawl_depth = 0

{blossom}
"#);

        std::fs::write(config_dir.join("config.toml"), config_content)
            .expect("Failed to write config");

        // Generate a random test key for isolation
        let keys = nostr::Keys::generate();
        let nsec = keys.secret_key().to_bech32().expect("Failed to encode nsec");
        let npub = keys.public_key().to_bech32().expect("Failed to encode npub");
        let key_line = format!("{} self\n", nsec);
        std::fs::write(config_dir.join("keys"), &key_line)
            .expect("Failed to write keys");
        println!("Using test key: {} (petname: self)", &nsec[..20]);

        TestEnv {
            _data_dir: data_dir,
            home_dir,
            npub,
        }
    }

    pub fn env(&self) -> Vec<(String, String)> {
        let config_dir = self.home_dir.join(".hashtree");
        vec![
            (
                "HOME".to_string(),
                self.home_dir.to_string_lossy().to_string(),
            ),
            (
                "HTREE_CONFIG_DIR".to_string(),
                config_dir.to_string_lossy().to_string(),
            ),
            (
                "PATH".to_string(),
                format!(
                    "{}:{}",
                    find_git_remote_htree_dir()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_default(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            ),
            (
                "HTREE_VERBOSE".to_string(),
                "1".to_string(),
            ),
        ]
    }

    /// Update blossom servers in config (for testing server coverage)
    pub fn update_blossom_servers(&self, servers: &[&str], relay_url: &str) {
        let config_dir = self.home_dir.join(".hashtree");
        let servers_json: Vec<String> = servers.iter().map(|s| format!("\"{}\"", s)).collect();
        let servers_str = servers_json.join(", ");

        let config_content = format!(r#"
[server]
enable_auth = false
stun_port = 0

[nostr]
relays = ["{}"]
crawl_depth = 0

[blossom]
read_servers = [{servers_str}]
write_servers = [{servers_str}]
"#, relay_url);

        std::fs::write(config_dir.join("config.toml"), config_content)
            .expect("Failed to update config");
    }
}

pub fn find_git_remote_htree_dir() -> Option<PathBuf> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace_root = PathBuf::from(manifest_dir)
        .parent()?
        .parent()?
        .to_path_buf();

    let release_dir = workspace_root.join("target/release");
    let debug_dir = workspace_root.join("target/debug");

    if release_dir.join("git-remote-htree").exists() {
        Some(release_dir)
    } else if debug_dir.join("git-remote-htree").exists() {
        Some(debug_dir)
    } else {
        // Binary not found - print helpful message
        eprintln!("WARNING: git-remote-htree binary not found in target/debug or target/release.");
        eprintln!("Run: cargo build -p git-remote-htree");
        None
    }
}

/// Create a test git repository with some files
pub fn create_test_repo() -> TempDir {
    let dir = TempDir::new().expect("Failed to create temp dir");
    let path = dir.path();

    // Init git repo
    let status = Command::new("git")
        .args(["init", "-b", "master"])
        .current_dir(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("Failed to run git init");
    assert!(status.success(), "git init failed");

    // Configure git
    Command::new("git")
        .args(["config", "user.email", "test@example.com"])
        .current_dir(path)
        .status()
        .expect("Failed to configure git");
    Command::new("git")
        .args(["config", "user.name", "Test User"])
        .current_dir(path)
        .status()
        .expect("Failed to configure git");

    // Create test files
    std::fs::write(path.join("README.md"), "# Test Repository\n\nThis is a test.\n").unwrap();
    std::fs::write(path.join("hello.txt"), "Hello, World!\n").unwrap();
    std::fs::create_dir_all(path.join("src")).unwrap();
    std::fs::write(
        path.join("src/main.rs"),
        r#"fn main() {
    println!("Hello from test repo!");
}
"#,
    )
    .unwrap();

    // Commit
    Command::new("git")
        .args(["add", "-A"])
        .current_dir(path)
        .status()
        .expect("Failed to git add");
    Command::new("git")
        .args(["commit", "-m", "Initial commit"])
        .current_dir(path)
        .stdout(Stdio::null())
        .status()
        .expect("Failed to git commit");

    dir
}

/// Check if prerequisites are met for tests
pub fn check_prerequisites() -> bool {
    find_git_remote_htree_dir().is_some()
}

/// Print skip message if prerequisites not met
pub fn skip_if_no_binary() -> bool {
    if find_git_remote_htree_dir().is_none() {
        println!(
            "SKIP: git-remote-htree binary not found. Run `cargo build -p git-remote-htree` first."
        );
        true
    } else {
        false
    }
}
