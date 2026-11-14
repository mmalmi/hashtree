#![allow(dead_code)]

use std::fs;
use std::io;
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

pub mod test_relay {
    use super::*;
    use futures::{SinkExt, StreamExt};
    use tokio::net::TcpStream;
    use tokio::sync::broadcast;
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    pub struct TestRelay {
        port: u16,
        shutdown: broadcast::Sender<()>,
    }

    impl TestRelay {
        pub fn new() -> Self {
            Self::with_events(Vec::new())
        }

        pub fn with_events(events: Vec<Value>) -> Self {
            let events = Arc::new(Mutex::new(events));
            let (shutdown, _) = broadcast::channel(1);

            let std_listener = TcpListener::bind("127.0.0.1:0").expect("bind relay listener");
            let port = std_listener.local_addr().expect("relay local addr").port();
            std_listener.set_nonblocking(true).expect("set nonblocking");

            let events_for_thread = Arc::clone(&events);
            let shutdown_for_thread = shutdown.clone();

            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("build tokio runtime");

                rt.block_on(async move {
                    let listener =
                        tokio::net::TcpListener::from_std(std_listener).expect("tokio listener");
                    let mut shutdown_rx = shutdown_for_thread.subscribe();

                    loop {
                        tokio::select! {
                            _ = shutdown_rx.recv() => break,
                            accept = listener.accept() => {
                                if let Ok((stream, _)) = accept {
                                    let events = Arc::clone(&events_for_thread);
                                    tokio::spawn(async move {
                                        handle_connection(stream, events).await;
                                    });
                                }
                            }
                        }
                    }
                });
            });

            std::thread::sleep(Duration::from_millis(100));

            Self { port, shutdown }
        }

        pub fn url(&self) -> String {
            format!("ws://127.0.0.1:{}", self.port)
        }
    }

    impl Drop for TestRelay {
        fn drop(&mut self) {
            let _ = self.shutdown.send(());
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn event_tag_matches(event: &Value, name: &str, accepted: &[String]) -> bool {
        let Some(tags) = event.get("tags").and_then(Value::as_array) else {
            return false;
        };

        tags.iter().any(|tag| {
            let Some(arr) = tag.as_array() else {
                return false;
            };
            if arr.len() < 2 {
                return false;
            }
            let Some(tag_name) = arr.first().and_then(Value::as_str) else {
                return false;
            };
            if tag_name != name {
                return false;
            }
            let Some(tag_value) = arr.get(1).and_then(Value::as_str) else {
                return false;
            };
            accepted.iter().any(|value| value == tag_value)
        })
    }

    fn event_matches_filter(event: &Value, filter: &Value) -> bool {
        let Some(filter_obj) = filter.as_object() else {
            return true;
        };

        if let Some(kinds) = filter_obj.get("kinds").and_then(Value::as_array) {
            let event_kind = event
                .get("kind")
                .and_then(Value::as_i64)
                .unwrap_or_default();
            let kind_match = kinds
                .iter()
                .any(|kind| kind.as_i64().is_some_and(|k| k == event_kind));
            if !kind_match {
                return false;
            }
        }

        if let Some(authors) = filter_obj.get("authors").and_then(Value::as_array) {
            let event_author = event
                .get("pubkey")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let author_match = authors
                .iter()
                .filter_map(Value::as_str)
                .any(|author| author == event_author);
            if !author_match {
                return false;
            }
        }

        if let Some(l_values) = filter_obj.get("#l").and_then(Value::as_array) {
            let accepted: Vec<String> = l_values
                .iter()
                .filter_map(|value| value.as_str().map(ToOwned::to_owned))
                .collect();
            if !accepted.is_empty() && !event_tag_matches(event, "l", &accepted) {
                return false;
            }
        }

        if let Some(d_values) = filter_obj.get("#d").and_then(Value::as_array) {
            let accepted: Vec<String> = d_values
                .iter()
                .filter_map(|value| value.as_str().map(ToOwned::to_owned))
                .collect();
            if !accepted.is_empty() && !event_tag_matches(event, "d", &accepted) {
                return false;
            }
        }

        if let Some(a_values) = filter_obj.get("#a").and_then(Value::as_array) {
            let accepted: Vec<String> = a_values
                .iter()
                .filter_map(|value| value.as_str().map(ToOwned::to_owned))
                .collect();
            if !accepted.is_empty() && !event_tag_matches(event, "a", &accepted) {
                return false;
            }
        }

        if let Some(e_values) = filter_obj.get("#e").and_then(Value::as_array) {
            let accepted: Vec<String> = e_values
                .iter()
                .filter_map(|value| value.as_str().map(ToOwned::to_owned))
                .collect();
            if !accepted.is_empty() && !event_tag_matches(event, "e", &accepted) {
                return false;
            }
        }

        true
    }

    async fn handle_connection(stream: TcpStream, events: Arc<Mutex<Vec<Value>>>) {
        let ws_stream = match accept_async(stream).await {
            Ok(ws) => ws,
            Err(_) => return,
        };

        let (mut write, mut read) = ws_stream.split();

        while let Some(msg) = read.next().await {
            let msg = match msg {
                Ok(Message::Text(text)) => text,
                Ok(Message::Ping(data)) => {
                    let _ = write.send(Message::Pong(data)).await;
                    continue;
                }
                Ok(Message::Close(_)) => break,
                _ => continue,
            };

            let parsed: Vec<Value> = match serde_json::from_str(&msg) {
                Ok(value) => value,
                Err(_) => continue,
            };

            let Some(msg_type) = parsed.first().and_then(Value::as_str) else {
                continue;
            };

            match msg_type {
                "EVENT" => {
                    let Some(event) = parsed.get(1).cloned() else {
                        continue;
                    };
                    let Some(id) = event.get("id").and_then(Value::as_str) else {
                        continue;
                    };
                    let id = id.to_string();
                    events.lock().expect("relay events lock").push(event);
                    let ok = serde_json::json!(["OK", id, true, ""]);
                    let _ = write.send(Message::Text(ok.to_string())).await;
                }
                "REQ" => {
                    let Some(sub_id) = parsed.get(1).and_then(Value::as_str) else {
                        continue;
                    };

                    let filters: Vec<Value> = parsed.iter().skip(2).cloned().collect();
                    let snapshot = events.lock().expect("relay events lock").clone();
                    for event in snapshot {
                        let matched = if filters.is_empty() {
                            true
                        } else {
                            filters
                                .iter()
                                .any(|filter| event_matches_filter(&event, filter))
                        };
                        if matched {
                            let msg = serde_json::json!(["EVENT", sub_id, event]);
                            let _ = write.send(Message::Text(msg.to_string())).await;
                        }
                    }
                    let eose = serde_json::json!(["EOSE", sub_id]);
                    let _ = write.send(Message::Text(eose.to_string())).await;
                }
                "CLOSE" => {}
                _ => {}
            }
        }
    }
}

pub fn htree_bin() -> String {
    std::env::var("CARGO_BIN_EXE_htree").unwrap_or_else(|_| {
        if let Ok(target_dir) = std::env::var("CARGO_TARGET_DIR") {
            return Path::new(&target_dir)
                .join("debug/htree")
                .to_string_lossy()
                .to_string();
        }

        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        manifest_dir
            .parent()
            .and_then(Path::parent)
            .map(|rust_root| rust_root.join("target/debug/htree"))
            .expect("rust workspace root")
            .to_string_lossy()
            .to_string()
    })
}

pub fn run_git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("failed to run git {:?}: {}", args, e));
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

pub fn write_keys_file(config_dir: &Path, nsec: &str) -> io::Result<()> {
    fs::create_dir_all(config_dir)?;
    fs::write(config_dir.join("keys"), format!("{nsec} self\n"))
}
