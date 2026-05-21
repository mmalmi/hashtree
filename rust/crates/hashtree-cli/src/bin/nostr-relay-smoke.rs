use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, Mutex};
use tokio_tungstenite::{accept_async, tungstenite::Message};

#[derive(Debug, Parser)]
struct Args {
    #[arg(long, default_value = "0.0.0.0:7777")]
    addr: SocketAddr,
}

#[derive(Clone)]
struct RelayState {
    events: Arc<Mutex<Vec<Value>>>,
    event_tx: broadcast::Sender<Value>,
}

impl RelayState {
    fn new() -> Self {
        let (event_tx, _) = broadcast::channel(1024);
        Self {
            events: Arc::new(Mutex::new(Vec::new())),
            event_tx,
        }
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

fn event_id_matches(event_id: &str, accepted: &[String]) -> bool {
    accepted.iter().any(|id| event_id.starts_with(id))
}

fn accepted_strings(values: &Value) -> Vec<String> {
    values
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str().map(ToOwned::to_owned))
        .collect()
}

fn event_matches_filter(event: &Value, filter: &Value) -> bool {
    let Some(filter_obj) = filter.as_object() else {
        return true;
    };

    if let Some(ids) = filter_obj.get("ids").map(accepted_strings) {
        let event_id = event.get("id").and_then(Value::as_str).unwrap_or_default();
        if !ids.is_empty() && !event_id_matches(event_id, &ids) {
            return false;
        }
    }

    if let Some(kinds) = filter_obj.get("kinds").and_then(Value::as_array) {
        let event_kind = event
            .get("kind")
            .and_then(Value::as_i64)
            .unwrap_or_default();
        let kind_match = kinds
            .iter()
            .any(|kind| kind.as_i64().is_some_and(|kind| kind == event_kind));
        if !kind_match {
            return false;
        }
    }

    if let Some(authors) = filter_obj.get("authors").map(accepted_strings) {
        let event_author = event
            .get("pubkey")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !authors.is_empty() && !authors.iter().any(|author| author == event_author) {
            return false;
        }
    }

    if let Some(since) = filter_obj.get("since").and_then(Value::as_u64) {
        let event_created_at = event
            .get("created_at")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        if event_created_at < since {
            return false;
        }
    }

    if let Some(until) = filter_obj.get("until").and_then(Value::as_u64) {
        let event_created_at = event
            .get("created_at")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        if event_created_at > until {
            return false;
        }
    }

    for (key, values) in filter_obj {
        let Some(tag_name) = key.strip_prefix('#') else {
            continue;
        };
        let accepted = accepted_strings(values);
        if !accepted.is_empty() && !event_tag_matches(event, tag_name, &accepted) {
            return false;
        }
    }

    true
}

fn filter_limit(filter: &Value) -> Option<usize> {
    filter
        .get("limit")
        .and_then(Value::as_u64)
        .and_then(|limit| usize::try_from(limit).ok())
}

async fn handle_req(
    write: &mut futures::stream::SplitSink<tokio_tungstenite::WebSocketStream<TcpStream>, Message>,
    state: &RelayState,
    sub_id: &str,
    filters: &[Value],
) {
    let snapshot = state.events.lock().await.clone();
    let mut sent_ids = HashSet::new();

    if filters.is_empty() {
        for event in snapshot {
            let msg = serde_json::json!(["EVENT", sub_id, event]);
            if write.send(Message::Text(msg.to_string())).await.is_err() {
                return;
            }
        }
    } else {
        for filter in filters {
            let limit = filter_limit(filter).unwrap_or(usize::MAX);
            let mut sent_for_filter = 0usize;
            if limit == 0 {
                continue;
            }

            for event in &snapshot {
                if !event_matches_filter(event, filter) {
                    continue;
                }
                let event_id = event
                    .get("id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
                    .unwrap_or_else(|| event.to_string());
                if !sent_ids.insert(event_id) {
                    continue;
                }
                let msg = serde_json::json!(["EVENT", sub_id, event]);
                if write.send(Message::Text(msg.to_string())).await.is_err() {
                    return;
                }
                sent_for_filter = sent_for_filter.saturating_add(1);
                if sent_for_filter >= limit {
                    break;
                }
            }
        }
    }

    let eose = serde_json::json!(["EOSE", sub_id]);
    let _ = write.send(Message::Text(eose.to_string())).await;
}

async fn handle_connection(stream: TcpStream, state: RelayState) {
    let ws_stream = match accept_async(stream).await {
        Ok(ws) => ws,
        Err(err) => {
            eprintln!("websocket accept failed: {err}");
            return;
        }
    };

    let (mut write, mut read) = ws_stream.split();
    let mut relay_rx = state.event_tx.subscribe();
    let mut subscriptions: HashMap<String, Vec<Value>> = HashMap::new();

    loop {
        tokio::select! {
            incoming = read.next() => {
                let Some(incoming) = incoming else {
                    break;
                };
                let msg = match incoming {
                    Ok(Message::Text(text)) => text,
                    Ok(Message::Ping(data)) => {
                        let _ = write.send(Message::Pong(data)).await;
                        continue;
                    }
                    Ok(Message::Close(_)) => break,
                    Ok(_) => continue,
                    Err(_) => break,
                };

                let parsed: Vec<Value> = match serde_json::from_str(msg.as_str()) {
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
                        state.events.lock().await.push(event.clone());
                        let ok = serde_json::json!(["OK", id, true, ""]);
                        if write.send(Message::Text(ok.to_string())).await.is_err() {
                            break;
                        }
                        let _ = state.event_tx.send(event);
                    }
                    "REQ" => {
                        let Some(sub_id) = parsed.get(1).and_then(Value::as_str) else {
                            continue;
                        };
                        let filters = parsed.iter().skip(2).cloned().collect::<Vec<_>>();
                        subscriptions.insert(sub_id.to_string(), filters.clone());
                        handle_req(&mut write, &state, sub_id, &filters).await;
                    }
                    "CLOSE" => {
                        if let Some(sub_id) = parsed.get(1).and_then(Value::as_str) {
                            subscriptions.remove(sub_id);
                        }
                    }
                    _ => {}
                }
            }
            event = relay_rx.recv() => {
                let Ok(event) = event else {
                    continue;
                };
                for (sub_id, filters) in &subscriptions {
                    let matched = filters.is_empty()
                        || filters.iter().any(|filter| event_matches_filter(&event, filter));
                    if matched {
                        let msg = serde_json::json!(["EVENT", sub_id, event]);
                        if write.send(Message::Text(msg.to_string())).await.is_err() {
                            return;
                        }
                    }
                }
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let listener = TcpListener::bind(args.addr)
        .await
        .with_context(|| format!("bind relay listener {}", args.addr))?;
    let state = RelayState::new();
    eprintln!("nostr-relay-smoke listening on {}", args.addr);

    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(handle_connection(stream, state.clone()));
    }
}
