mod common;

use common::htree_bin;
use futures::{SinkExt, StreamExt};
use hashtree_cli::{Config, HashtreeStore};
use hashtree_config::StorageBackend;
use nostr::{Event, EventBuilder, Keys, Kind, Tag, Timestamp, ToBech32};
use serde_json::{json, Value};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::sync::watch;
use tokio_tungstenite::{accept_async, tungstenite::Message};

#[derive(Default)]
struct Observations {
    requests: Vec<Value>,
    closes: Vec<String>,
    disconnected: bool,
}

/// Sends an empty EOSE immediately, then delayed events on the still-open REQ.
/// Deliberately does not filter events: the consumer must verify its own root.
struct DelayedRelay {
    url: String,
    observations: Arc<Mutex<Observations>>,
    stop: watch::Sender<bool>,
    worker: Option<JoinHandle<()>>,
}

impl DelayedRelay {
    fn new(events: Vec<Value>, unrelated: Option<Value>) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let observations = Arc::new(Mutex::new(Observations::default()));
        let observed = Arc::clone(&observations);
        let (stop, mut stopped) = watch::channel(false);
        let worker = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let stream = tokio::select! {
                    _ = stopped.changed() => return,
                    accepted = listener.accept() => accepted.unwrap().0,
                };
                let mut socket = accept_async(stream).await.unwrap();
                let mut delayed: Option<(String, tokio::time::Instant)> = None;
                loop {
                    let deadline = delayed.as_ref().map(|(_, deadline)| *deadline);
                    tokio::select! {
                        _ = stopped.changed() => break,
                        _ = async {
                            match deadline {
                                Some(deadline) => tokio::time::sleep_until(deadline).await,
                                None => std::future::pending().await,
                            }
                        } => {
                            let (id, _) = delayed.take().unwrap();
                            // A correctly signed but unrelated subscription must not win.
                            if let Some(event) = events.first() {
                                let frame = json!(["EVENT", "unrelated-subscription", event]);
                                let _ = socket.send(Message::Text(frame.to_string())).await;
                            }
                            if let Some(event) = &unrelated {
                                let frame = json!(["EVENT", "unrelated-subscription", event]);
                                let _ = socket.send(Message::Text(frame.to_string())).await;
                            }
                            for event in &events {
                                let frame = json!(["EVENT", id, event]);
                                if socket.send(Message::Text(frame.to_string())).await.is_err() {
                                    break;
                                }
                            }
                        }
                        message = socket.next() => {
                            let Some(Ok(message)) = message else { break };
                            match message {
                                Message::Text(text) => {
                                    let value: Value = serde_json::from_str(&text).unwrap();
                                    match value[0].as_str() {
                                        Some("REQ") => {
                                            let id = value[1].as_str().unwrap().to_string();
                                            observed.lock().unwrap().requests.push(value);
                                            socket.send(Message::Text(json!(["EOSE", id]).to_string()))
                                                .await.unwrap();
                                            delayed = Some((id, tokio::time::Instant::now()
                                                + Duration::from_millis(750)));
                                        }
                                        Some("CLOSE") => {
                                            observed.lock().unwrap().closes.push(
                                                value[1].as_str().unwrap().to_string());
                                        }
                                        _ => {}
                                    }
                                }
                                Message::Ping(data) => {
                                    let _ = socket.send(Message::Pong(data)).await;
                                }
                                Message::Close(_) => break,
                                _ => {}
                            }
                        }
                    }
                }
                observed.lock().unwrap().disconnected = true;
            });
        });
        Self {
            url,
            observations,
            stop,
            worker: Some(worker),
        }
    }

    fn assert_closed(&self, author: &Keys) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !self.observations.lock().unwrap().disconnected && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let observed = self.observations.lock().unwrap();
        assert_eq!(observed.requests.len(), 1, "one retained subscription");
        assert_eq!(
            observed.requests[0][2]["authors"],
            json!([author.public_key().to_hex()])
        );
        assert_eq!(observed.requests[0][2]["#d"], json!(["tap.git"]));
        // The SDK queues CLOSE, but stopping Get's owned client may close the
        // socket first. Either ends the server subscription; require actual
        // connection termination below and reject any unrelated CLOSE.
        assert!(
            observed.closes.is_empty()
                || observed.closes == vec![observed.requests[0][1].as_str().unwrap()],
            "unexpected subscription closure: {:?}",
            observed.closes
        );
        assert!(
            observed.disconnected,
            "Get must stop its owned relay connection"
        );
    }
}

impl Drop for DelayedRelay {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn root_event(keys: &Keys, name: &str, hash: &str, timestamp: u64) -> Event {
    EventBuilder::new(Kind::Custom(30064), "")
        .tags([
            Tag::identifier(name),
            Tag::parse(["l", "hashtree"]).unwrap(),
            Tag::parse(["hash", hash]).unwrap(),
        ])
        .custom_created_at(Timestamp::from_secs(timestamp))
        .sign_with_keys(keys)
        .unwrap()
}

fn write_config(path: &Path, relay: Option<&str>) {
    let mut config = Config::default();
    config.nostr.enabled = relay.is_some();
    config.nostr.relays = relay.into_iter().map(str::to_string).collect();
    config.sync.enabled = false;
    config.updater.auto_check = false;
    let mut config = toml::Value::try_from(config).unwrap();
    config["storage"]
        .as_table_mut()
        .unwrap()
        .insert("backend".into(), toml::Value::String("fs".into()));
    std::fs::create_dir_all(path).unwrap();
    std::fs::write(path.join("config.toml"), toml::to_string(&config).unwrap()).unwrap();
}

fn seed_directory(temp: &TempDir, name: &str) -> String {
    let source = temp.path().join(name);
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("marker"), name).unwrap();
    let store = HashtreeStore::new_with_backend(
        temp.path().join("data"),
        StorageBackend::Fs,
        64 * 1024 * 1024,
    )
    .unwrap();
    store.upload_dir(&source).unwrap()
}

fn get(temp: &TempDir, input: &str) -> Output {
    Command::new(htree_bin())
        .args([
            "--data-dir",
            temp.path().join("data").to_str().unwrap(),
            "get",
            input,
            "--output",
            temp.path().join("output").to_str().unwrap(),
        ])
        .env("HTREE_CONFIG_DIR", temp.path().join("config"))
        .env("HTREE_DATA_DIR", temp.path().join("data"))
        .output()
        .unwrap()
}

#[test]
fn get_observes_configured_relay_after_empty_eose_and_selects_verified_latest_root() {
    let temp = TempDir::new().unwrap();
    let author = Keys::generate();
    let old_hash = seed_directory(&temp, "old");
    let first_hash = seed_directory(&temp, "first");
    let second_hash = seed_directory(&temp, "second");
    let first = root_event(&author, "tap.git", &first_hash, 2);
    let second = root_event(&author, "tap.git", &second_hash, 2);
    let expected = if first.id < second.id {
        "first"
    } else {
        "second"
    };
    let (winner, loser) = if first.id < second.id {
        (first, second)
    } else {
        (second, first)
    };
    let mut invalid_signature =
        serde_json::to_value(root_event(&author, "tap.git", &old_hash, 9)).unwrap();
    invalid_signature["content"] = json!("tampered after signing");
    let relay = DelayedRelay::new(
        vec![
            serde_json::to_value(root_event(&author, "tap.git", &old_hash, 1)).unwrap(),
            serde_json::to_value(winner).unwrap(),
            serde_json::to_value(loser).unwrap(),
            serde_json::to_value(root_event(&author, "tap.git", &old_hash, 1)).unwrap(),
            serde_json::to_value(root_event(&Keys::generate(), "tap.git", &old_hash, 9)).unwrap(),
            serde_json::to_value(root_event(&author, "other-tree", &old_hash, 9)).unwrap(),
            serde_json::to_value(root_event(&author, "tap.git", "not-a-content-hash", 9)).unwrap(),
            invalid_signature,
        ],
        Some(serde_json::to_value(root_event(&author, "tap.git", &old_hash, 10)).unwrap()),
    );
    write_config(&temp.path().join("config"), Some(&relay.url));
    let output = get(
        &temp,
        &format!(
            "htree://{}/tap.git",
            author.public_key().to_bech32().unwrap()
        ),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("output/marker")).unwrap(),
        expected
    );
    relay.assert_closed(&author);
}

#[test]
fn get_empty_open_subscription_times_out_and_closes_without_creating_output() {
    let temp = TempDir::new().unwrap();
    let author = Keys::generate();
    let relay = DelayedRelay::new(Vec::new(), None);
    write_config(&temp.path().join("config"), Some(&relay.url));
    let started = Instant::now();
    let output = get(
        &temp,
        &format!(
            "htree://{}/tap.git",
            author.public_key().to_bech32().unwrap()
        ),
    );
    assert!(
        started.elapsed() >= Duration::from_secs(9),
        "EOSE must not end the observation window"
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Timed out"), "{stderr}");
    assert!(
        !stderr.contains("No content found"),
        "quiet is not absence: {stderr}"
    );
    assert!(!temp.path().join("output").exists());
    relay.assert_closed(&author);
}

#[test]
fn get_immutable_root_works_with_nostr_disabled() {
    let temp = TempDir::new().unwrap();
    let hash = seed_directory(&temp, "immutable");
    write_config(&temp.path().join("config"), None);
    let output = get(&temp, &hash);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("output/marker")).unwrap(),
        "immutable"
    );
}
