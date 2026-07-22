#![allow(dead_code)]
//! Shared test infrastructure for git-remote-htree integration tests
//!
//! Provides:
//! - TestRelay: In-memory nostr relay
//! - TestServer: Local blossom server
//! - TestEnv: Test environment with config and keys
//! - Helper functions for creating test repos

use nostr::ToBech32;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

fn kill_child_process_group(child: &mut Child) {
    #[cfg(unix)]
    unsafe {
        let _ = libc::killpg(child.id() as i32, libc::SIGKILL);
    }
    let _ = child.kill();
}

pub fn command_output_with_timeout(
    command: &mut Command,
    timeout: Duration,
) -> Result<Output, String> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            libc::setpgid(0, 0);
            Ok(())
        });
    }

    let mut child = command
        .spawn()
        .map_err(|err| format!("Failed to start command: {err}"))?;
    let start = Instant::now();
    while child
        .try_wait()
        .map_err(|err| format!("Failed to poll command: {err}"))?
        .is_none()
    {
        if start.elapsed() >= timeout {
            kill_child_process_group(&mut child);
            let output = child
                .wait_with_output()
                .map_err(|err| format!("Failed to collect timed-out command output: {err}"))?;
            return Err(format!(
                "Command timed out after {timeout:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    child
        .wait_with_output()
        .map_err(|err| format!("Failed to collect command output: {err}"))
}

/// Minimal in-memory nostr relay for testing with real-time event broadcasting
pub mod test_relay {
    use futures::{SinkExt, StreamExt};
    use std::collections::{HashMap, HashSet};
    use std::net::TcpListener;
    use std::sync::Arc;
    use tokio::net::TcpStream;
    use tokio::sync::{broadcast, RwLock};
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    /// Stored filter for matching events
    #[derive(Clone)]
    struct StoredFilter {
        sub_id: String,
        kind: Option<u64>,
        kinds: Vec<u64>,
        authors: Vec<String>,
        p_tag: Option<String>, // #p tag for directed messages
        l_tag: Option<String>, // #l tag for hello messages
        a_tag: Option<String>, // #a tag for addressable references
        e_tags: Vec<String>,   // #e tag for event references
        d_tag: Option<String>, // #d tag for replaceable event identifier
    }

    impl StoredFilter {
        fn matches(&self, event: &serde_json::Value) -> bool {
            // Check kind (single)
            if let Some(k) = self.kind {
                if event.get("kind").and_then(|v| v.as_u64()) != Some(k) {
                    return false;
                }
            }

            // Check kinds (multiple)
            if !self.kinds.is_empty() {
                let event_kind = event.get("kind").and_then(|v| v.as_u64()).unwrap_or(0);
                if !self.kinds.contains(&event_kind) {
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

            let tags = event.get("tags").and_then(|t| t.as_array());

            // Check #p tag
            if let Some(ref p) = self.p_tag {
                let has_p = tags
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

            // Check #l tag
            if let Some(ref l) = self.l_tag {
                let has_l = tags
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

            // Check #a tag
            if let Some(ref a) = self.a_tag {
                let has_a = tags
                    .map(|tags| {
                        tags.iter().any(|tag| {
                            tag.as_array()
                                .map(|arr| {
                                    arr.len() >= 2
                                        && arr[0].as_str() == Some("a")
                                        && arr[1].as_str() == Some(a.as_str())
                                })
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false);
                if !has_a {
                    return false;
                }
            }

            // Check #e tags (any of the filter e_tags must match)
            if !self.e_tags.is_empty() {
                let has_e = tags
                    .map(|tags| {
                        tags.iter().any(|tag| {
                            tag.as_array()
                                .map(|arr| {
                                    arr.len() >= 2
                                        && arr[0].as_str() == Some("e")
                                        && self
                                            .e_tags
                                            .iter()
                                            .any(|e| arr[1].as_str() == Some(e.as_str()))
                                })
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false);
                if !has_e {
                    return false;
                }
            }

            // Check #d tag
            if let Some(ref d) = self.d_tag {
                let has_d = tags
                    .map(|tags| {
                        tags.iter().any(|tag| {
                            tag.as_array()
                                .map(|arr| {
                                    arr.len() >= 2
                                        && arr[0].as_str() == Some("d")
                                        && arr[1].as_str() == Some(d.as_str())
                                })
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false);
                if !has_d {
                    return false;
                }
            }

            true
        }
    }

    pub struct TestRelay {
        port: u16,
        shutdown: broadcast::Sender<()>,
        events: Arc<RwLock<HashMap<String, serde_json::Value>>>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    #[derive(Clone, Default)]
    pub struct TestRelayOptions {
        pub reject_event_kinds: Vec<u64>,
        pub ignore_req_kinds: Vec<u64>,
        pub respond_empty_req_kinds_once: Vec<u64>,
    }

    impl TestRelay {
        pub fn new() -> Self {
            Self::with_options(TestRelayOptions::default())
        }

        pub fn with_options(options: TestRelayOptions) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind test relay");
            let port = listener
                .local_addr()
                .expect("read test relay address")
                .port();
            listener
                .set_nonblocking(true)
                .expect("set test relay nonblocking");
            let events: Arc<RwLock<HashMap<String, serde_json::Value>>> =
                Arc::new(RwLock::new(HashMap::new()));
            let (shutdown, _) = broadcast::channel(1);
            // Broadcast channel for new events - larger buffer for busy relays
            let (event_tx, _) = broadcast::channel::<serde_json::Value>(1000);
            let reject_event_kinds: Arc<HashSet<u64>> =
                Arc::new(options.reject_event_kinds.iter().copied().collect());
            let ignore_req_kinds: Arc<HashSet<u64>> =
                Arc::new(options.ignore_req_kinds.iter().copied().collect());
            let respond_empty_req_kinds_once: Arc<tokio::sync::Mutex<HashSet<u64>>> =
                Arc::new(tokio::sync::Mutex::new(
                    options
                        .respond_empty_req_kinds_once
                        .iter()
                        .copied()
                        .collect(),
                ));

            let events_clone = events.clone();
            let mut shutdown_rx = shutdown.subscribe();
            let event_tx_clone = event_tx.clone();
            let ignore_req_kinds_clone = ignore_req_kinds.clone();
            let respond_empty_req_kinds_once_clone = respond_empty_req_kinds_once.clone();

            let thread = std::thread::spawn(move || {
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
                                    let reject_event_kinds = reject_event_kinds.clone();
                                    let ignore_req_kinds = ignore_req_kinds_clone.clone();
                                    let respond_empty_req_kinds_once =
                                        respond_empty_req_kinds_once_clone.clone();
                                    tokio::spawn(handle_connection(
                                        stream,
                                        events,
                                        event_tx,
                                        event_rx,
                                        reject_event_kinds,
                                        ignore_req_kinds,
                                        respond_empty_req_kinds_once,
                                    ));
                                }
                            }
                        }
                    }
                });
            });

            TestRelay {
                port,
                shutdown,
                events,
                thread: Some(thread),
            }
        }

        pub fn url(&self) -> String {
            format!("ws://127.0.0.1:{}", self.port)
        }

        pub fn stored_events(&self) -> Vec<serde_json::Value> {
            let events = self.events.clone();
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to build runtime");
            rt.block_on(async move { events.read().await.values().cloned().collect() })
        }
    }

    impl Drop for TestRelay {
        fn drop(&mut self) {
            let _ = self.shutdown.send(());
            if let Some(thread) = self.thread.take() {
                thread.join().expect("join test relay");
            }
        }
    }

    async fn handle_connection(
        stream: TcpStream,
        events: Arc<RwLock<HashMap<String, serde_json::Value>>>,
        event_tx: broadcast::Sender<serde_json::Value>,
        mut event_rx: broadcast::Receiver<serde_json::Value>,
        reject_event_kinds: Arc<HashSet<u64>>,
        ignore_req_kinds: Arc<HashSet<u64>>,
        respond_empty_req_kinds_once: Arc<tokio::sync::Mutex<HashSet<u64>>>,
    ) {
        let ws_stream = match accept_async(stream).await {
            Ok(s) => s,
            Err(_) => return,
        };

        let (write, mut read) = ws_stream.split();
        let write = Arc::new(tokio::sync::Mutex::new(write));

        // Track active subscriptions for this connection
        let subscriptions: Arc<RwLock<HashMap<String, Vec<StoredFilter>>>> =
            Arc::new(RwLock::new(HashMap::new()));

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
                                    let event_msg =
                                        serde_json::json!(["EVENT", &filter.sub_id, &event]);
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
                "EVENT" if parsed.len() >= 2 => {
                    let event = parsed[1].clone();
                    if let Some(id) = event.get("id").and_then(|v| v.as_str()) {
                        let kind = event.get("kind").and_then(|v| v.as_u64()).unwrap_or(0);
                        if reject_event_kinds.contains(&kind) {
                            let ok_msg = serde_json::json!(["OK", id, false, "rejected for test"]);
                            let mut w = write.lock().await;
                            let _ = w.send(Message::Text(ok_msg.to_string())).await;
                            continue;
                        }

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
                "REQ" if parsed.len() >= 3 => {
                    let sub_id = parsed[1].as_str().unwrap_or("sub").to_string();

                    // Parse all filters (can have multiple)
                    let mut filters = Vec::new();
                    for filter in parsed.iter().skip(2) {
                        let kinds_arr: Vec<u64> = filter
                            .get("kinds")
                            .and_then(|k| k.as_array())
                            .map(|arr| arr.iter().filter_map(|v| v.as_u64()).collect())
                            .unwrap_or_default();

                        // Single kind for backward compat
                        let kind = if kinds_arr.len() == 1 {
                            Some(kinds_arr[0])
                        } else {
                            None
                        };

                        // If more than one kind, use kinds vec
                        let kinds = if kinds_arr.len() > 1 {
                            kinds_arr
                        } else {
                            vec![]
                        };

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

                        let a_tag = filter
                            .get("#a")
                            .and_then(|a| a.as_array())
                            .and_then(|a| a.first())
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());

                        let e_tags: Vec<String> = filter
                            .get("#e")
                            .and_then(|e| e.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                    .collect()
                            })
                            .unwrap_or_default();

                        let d_tag = filter
                            .get("#d")
                            .and_then(|d| d.as_array())
                            .and_then(|a| a.first())
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());

                        filters.push(StoredFilter {
                            sub_id: sub_id.clone(),
                            kind,
                            kinds,
                            authors,
                            p_tag,
                            l_tag,
                            a_tag,
                            e_tags,
                            d_tag,
                        });
                    }

                    let should_ignore_req = filters.iter().any(|filter| {
                        filter
                            .kind
                            .into_iter()
                            .chain(filter.kinds.iter().copied())
                            .any(|kind| ignore_req_kinds.contains(&kind))
                    });
                    if should_ignore_req {
                        continue;
                    }

                    let req_kinds: Vec<u64> = filters
                        .iter()
                        .flat_map(|filter| {
                            filter.kind.into_iter().chain(filter.kinds.iter().copied())
                        })
                        .collect();
                    let should_respond_empty_once = {
                        let mut remaining = respond_empty_req_kinds_once.lock().await;
                        req_kinds.into_iter().any(|kind| remaining.remove(&kind))
                    };
                    if should_respond_empty_once {
                        let eose = serde_json::json!(["EOSE", &sub_id]);
                        let mut w = write.lock().await;
                        let _ = w.send(Message::Text(eose.to_string())).await;
                        continue;
                    }

                    // Store subscription
                    subscriptions
                        .write()
                        .await
                        .insert(sub_id.clone(), filters.clone());

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
                "CLOSE" if parsed.len() >= 2 => {
                    if let Some(sub_id) = parsed[1].as_str() {
                        subscriptions.write().await.remove(sub_id);
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
    pub fn new() -> Option<Self> {
        let htree_bin = find_htree_binary()?;
        let data_dir = TempDir::new().expect("Failed to create temp dir");
        let home_dir = TempDir::new().expect("Failed to create home dir");

        // Create .hashtree config dir for the server
        let config_dir = home_dir.path().join(".hashtree");
        std::fs::create_dir_all(&config_dir).expect("Failed to create config dir");

        // Server config - no auth for testing
        let config_content = r#"
[server]
bind_address = "127.0.0.1:0"
enable_auth = false
public_writes = true

[nostr]
relays = []
"#;
        std::fs::write(config_dir.join("config.toml"), config_content)
            .expect("Failed to write config");

        // Generate keys for server
        let keys = nostr::Keys::generate();
        let nsec = keys
            .secret_key()
            .to_bech32()
            .expect("Failed to encode nsec");
        std::fs::write(config_dir.join("keys"), &nsec).expect("Failed to write keys");

        let (mut process, port) = start_test_server(&htree_bin, data_dir.path(), home_dir.path());
        let addr = format!("127.0.0.1:{port}");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if TcpStream::connect(&addr).is_ok() {
                break;
            }
            if let Some(status) = process.try_wait().expect("poll htree test server") {
                panic!("htree test server exited before startup: {status}");
            }
            if Instant::now() >= deadline {
                let _ = process.kill();
                let _ = process.wait();
                panic!("htree test server did not start on {addr}");
            }
            std::thread::sleep(Duration::from_millis(20));
        }

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

fn start_test_server(
    htree_bin: &std::path::Path,
    data_dir: &std::path::Path,
    home_dir: &std::path::Path,
) -> (Child, u16) {
    for _ in 0..10 {
        let reservation = TcpListener::bind("127.0.0.1:0").expect("reserve htree test port");
        let port = reservation
            .local_addr()
            .expect("read htree test port")
            .port();
        drop(reservation);

        let mut process = Command::new(htree_bin)
            .arg("--data-dir")
            .arg(data_dir)
            .arg("start")
            .arg("--addr")
            .arg(format!("127.0.0.1:{port}"))
            .env("HOME", home_dir)
            .env("RUST_LOG", "warn")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("Failed to start htree server");

        std::thread::sleep(Duration::from_millis(20));
        if process
            .try_wait()
            .expect("poll htree test server startup")
            .is_none()
        {
            return (process, port);
        }
    }
    panic!("failed to reserve a port for htree test server");
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
    let target_dir = cargo_target_dir(&workspace_root);
    let debug_bin = target_dir.join("debug/htree");
    let release_bin = target_dir.join("release/htree");

    if release_bin.exists() {
        Some(release_bin)
    } else if debug_bin.exists() {
        Some(debug_bin)
    } else {
        None
    }
}

fn cargo_target_dir(workspace_root: &std::path::Path) -> PathBuf {
    match std::env::var_os("CARGO_TARGET_DIR") {
        Some(path) => {
            let path = PathBuf::from(path);
            if path.is_absolute() {
                path
            } else {
                workspace_root.join(path)
            }
        }
        None => workspace_root.join("target"),
    }
}

/// Test environment with config and keys
pub struct TestEnv {
    _data_dir: TempDir,
    pub home_dir: PathBuf,
    pub nsec: String,
    pub npub: String,
}

impl TestEnv {
    pub fn new(blossom_server: Option<&str>, nostr_relay: Option<&str>) -> Self {
        let keys = nostr::Keys::generate();
        let nsec = keys
            .secret_key()
            .to_bech32()
            .expect("Failed to encode nsec");
        Self::with_nsec(blossom_server, nostr_relay, &nsec)
    }

    pub fn with_nsec(blossom_server: Option<&str>, nostr_relay: Option<&str>, nsec: &str) -> Self {
        let data_dir = TempDir::new().expect("Failed to create temp dir");
        let home_dir = data_dir.path().to_path_buf();

        // Create .hashtree config dir
        let config_dir = home_dir.join(".hashtree");
        std::fs::create_dir_all(&config_dir).expect("Failed to create config dir");

        // Build config
        let relays = match nostr_relay {
            Some(url) => format!(r#"relays = ["{}"]"#, url),
            None => format!(
                "relays = [{}]",
                hashtree_config::DEFAULT_RELAYS
                    .iter()
                    .map(|url| format!(r#""{}""#, url))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };

        let blossom = match blossom_server {
            Some(url) => format!(
                r#"
[blossom]
read_servers = ["{url}"]
write_servers = ["{url}"]
"#
            ),
            None => String::new(),
        };

        let config_content = format!(
            r#"
[server]
bind_address = "127.0.0.1:0"
enable_auth = false

[nostr]
{relays}
social_graph_crawl_depth = 0

{blossom}
"#
        );

        std::fs::write(config_dir.join("config.toml"), config_content)
            .expect("Failed to write config");

        // Reuse or generate a test key for isolation.
        let secret = nostr::SecretKey::parse(nsec).expect("Failed to parse nsec");
        let keys = nostr::Keys::new(secret);
        let npub = keys
            .public_key()
            .to_bech32()
            .expect("Failed to encode npub");
        let key_line = format!("{} self\n", nsec);
        std::fs::write(config_dir.join("keys"), &key_line).expect("Failed to write keys");
        println!("Using test key: {} (petname: self)", &nsec[..20]);

        TestEnv {
            _data_dir: data_dir,
            home_dir,
            nsec: nsec.to_string(),
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
            ("NOSTR_PREFER_LOCAL".to_string(), "0".to_string()),
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
            ("HTREE_VERBOSE".to_string(), "1".to_string()),
        ]
    }

    /// Update blossom servers in config (for testing server coverage)
    pub fn update_blossom_servers(&self, servers: &[&str], relay_url: &str) {
        let config_dir = self.home_dir.join(".hashtree");
        let servers_json: Vec<String> = servers.iter().map(|s| format!("\"{}\"", s)).collect();
        let servers_str = servers_json.join(", ");

        let config_content = format!(
            r#"
[server]
enable_auth = false

[nostr]
relays = ["{}"]
social_graph_crawl_depth = 0

[blossom]
read_servers = [{servers_str}]
write_servers = [{servers_str}]
"#,
            relay_url
        );

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
    let target_dir = cargo_target_dir(&workspace_root);
    let release_dir = target_dir.join("release");
    let debug_dir = target_dir.join("debug");

    // Prefer debug so `cargo test` uses a freshly built helper binary from current sources.
    if debug_dir.join("git-remote-htree").exists() {
        Some(debug_dir)
    } else if release_dir.join("git-remote-htree").exists() {
        Some(release_dir)
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
    std::fs::write(
        path.join("README.md"),
        "# Test Repository\n\nThis is a test.\n",
    )
    .unwrap();
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
