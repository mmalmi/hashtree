use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
};
use futures::{SinkExt, StreamExt};
use hashtree_core::from_hex;
use nostr::{
    ClientMessage as NostrClientMessage, Filter as NostrFilter, JsonUtil as NostrJsonUtil,
    RelayMessage as NostrRelayMessage, SubscriptionId,
};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, time::Duration};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::{connect_async, tungstenite::Message as TungsteniteMessage};

use super::auth::{AppState, PendingRequest, UpstreamNostrSubscription, WsProtocol};
use crate::diagnostics::{
    nostr_filters_summary, process_memory_snapshot, trim_process_allocations,
};
use crate::webrtc::types::{
    encode_request, encode_response, parse_message, DataMessage, DataRequest, DataResponse, MAX_HTL,
};
use hex::encode as hex_encode;

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum WsClientMessage {
    #[serde(rename = "req")]
    Request { id: u32, hash: String },
    #[serde(rename = "res")]
    Response { id: u32, hash: String, found: bool },
}

#[derive(Debug)]
enum WsTextMessage<'a> {
    Hashtree(WsClientMessage),
    Nostr(NostrClientMessage<'a>),
}

#[derive(Debug, Deserialize, Serialize)]
struct WsRequest {
    #[serde(rename = "type")]
    kind: String,
    id: u32,
    hash: String,
}

#[derive(Debug, Serialize)]
struct WsResponse {
    #[serde(rename = "type")]
    kind: &'static str,
    id: u32,
    hash: String,
    found: bool,
}

pub async fn ws_data(State(state): State<AppState>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws_data_with_client_pubkey(state, ws, None)
}

pub fn ws_data_with_client_pubkey(
    state: AppState,
    ws: WebSocketUpgrade,
    client_pubkey: Option<String>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state, client_pubkey))
}

async fn handle_socket(socket: WebSocket, state: AppState, client_pubkey: Option<String>) {
    // Use the Nostr relay's client-id generator when available so `/ws` IDs
    // can't collide with WebRTC relay clients.
    let client_id = state
        .nostr_relay
        .as_ref()
        .map(|relay| relay.next_client_id())
        .unwrap_or_else(|| state.ws_relay.next_id());
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();

    {
        let mut clients = state.ws_relay.clients.lock().await;
        clients.insert(client_id, tx);
    }
    {
        let mut protocols = state.ws_relay.client_protocols.lock().await;
        protocols.insert(client_id, WsProtocol::HashtreeJson);
    }

    let mut nostr_rx = if let Some(relay) = state.nostr_relay.clone() {
        let (nostr_tx, nostr_rx) = mpsc::unbounded_channel::<String>();
        relay
            .register_client(client_id, nostr_tx, client_pubkey.clone())
            .await;
        Some(nostr_rx)
    } else {
        None
    };

    let (mut sender, mut receiver) = socket.split();
    loop {
        tokio::select! {
            maybe_msg = rx.recv() => {
                let Some(msg) = maybe_msg else {
                    break;
                };
                if sender.send(msg).await.is_err() {
                    break;
                }
            }
            maybe_text = async {
                match &mut nostr_rx {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                let Some(text) = maybe_text else {
                    nostr_rx = None;
                    continue;
                };
                if sender.send(Message::Text(text)).await.is_err() {
                    break;
                }
            }
            maybe_incoming = receiver.next() => {
                match maybe_incoming {
                    Some(Ok(msg)) => handle_message(client_id, msg, &state).await,
                    Some(Err(_)) | None => break,
                }
            }
        }
    }

    close_all_upstream_nostr_subscriptions(&state, client_id).await;

    {
        let mut clients = state.ws_relay.clients.lock().await;
        clients.remove(&client_id);
    }
    {
        let mut protocols = state.ws_relay.client_protocols.lock().await;
        protocols.remove(&client_id);
    }
    {
        let mut pending = state.ws_relay.pending.lock().await;
        pending.retain(|(peer_id, _), _| *peer_id != client_id);
    }

    if let Some(relay) = &state.nostr_relay {
        relay.unregister_client(client_id).await;
    }
}

fn parse_ws_text_message(text: &str) -> Option<WsTextMessage<'static>> {
    let trimmed = text.trim_start();
    if trimmed.starts_with('[') {
        if let Ok(msg) = NostrClientMessage::from_json(trimmed) {
            return Some(WsTextMessage::Nostr(msg));
        }
    }

    if let Ok(msg) = serde_json::from_str::<WsClientMessage>(text) {
        return Some(WsTextMessage::Hashtree(msg));
    }

    None
}
async fn close_upstream_nostr_subscription(
    state: &AppState,
    client_id: u64,
    subscription_id: &SubscriptionId,
) {
    let key = (client_id, subscription_id.to_string());
    let subscription = {
        let mut subscriptions = state.ws_relay.upstream_nostr_subscriptions.lock().await;
        subscriptions.remove(&key)
    };
    if let Some(subscription) = subscription {
        let _ = subscription.close_tx.send(true);
        for task in subscription.tasks {
            task.abort();
        }
    }
    state
        .ws_relay
        .upstream_pending_eose
        .lock()
        .await
        .remove(&key);
    state
        .ws_relay
        .upstream_seen_events
        .lock()
        .await
        .remove(&key);
}

async fn close_all_upstream_nostr_subscriptions(state: &AppState, client_id: u64) {
    let keys = {
        let subscriptions = state.ws_relay.upstream_nostr_subscriptions.lock().await;
        subscriptions
            .keys()
            .filter(|(id, _)| *id == client_id)
            .cloned()
            .collect::<Vec<_>>()
    };
    for (_, sub_id) in keys {
        close_upstream_nostr_subscription(state, client_id, &SubscriptionId::new(sub_id)).await;
    }
}

async fn forward_upstream_nostr_message(
    state: &AppState,
    client_id: u64,
    subscription_id: &SubscriptionId,
    text: &str,
) {
    let Ok(message) = NostrRelayMessage::from_json(text) else {
        return;
    };

    match message {
        NostrRelayMessage::Event {
            subscription_id: sid,
            event,
        } if sid.as_ref() == subscription_id => {
            let event = event.into_owned();
            let key = (client_id, subscription_id.to_string());
            let event_id = event.id.to_hex();
            let inserted = {
                let mut seen_events = state.ws_relay.upstream_seen_events.lock().await;
                seen_events.entry(key).or_default().insert(event_id)
            };
            if !inserted {
                return;
            }
            if let Some(relay) = &state.nostr_relay {
                let _ = relay.ingest_trusted_event_silent(event.clone()).await;
            }
            send_nostr(
                state,
                client_id,
                NostrRelayMessage::event(subscription_id.clone(), event),
            )
            .await;
        }
        NostrRelayMessage::Closed {
            subscription_id: sid,
            message,
        } if sid.as_ref() == subscription_id => {
            send_nostr(
                state,
                client_id,
                NostrRelayMessage::closed(subscription_id.clone(), message.into_owned()),
            )
            .await;
        }
        _ => {}
    }
}

async fn mark_upstream_nostr_relay_complete(
    state: &AppState,
    client_id: u64,
    subscription_id: &SubscriptionId,
) {
    let key = (client_id, subscription_id.to_string());
    let should_send_eose = {
        let mut pending = state.ws_relay.upstream_pending_eose.lock().await;
        let Some(remaining) = pending.get_mut(&key) else {
            return;
        };
        if *remaining > 0 {
            *remaining -= 1;
        }
        if *remaining == 0 {
            pending.remove(&key);
            true
        } else {
            false
        }
    };

    if should_send_eose {
        send_nostr(
            state,
            client_id,
            NostrRelayMessage::eose(subscription_id.clone()),
        )
        .await;
    }
}

async fn run_upstream_nostr_subscription(
    state: AppState,
    client_id: u64,
    relay_url: String,
    subscription_id: SubscriptionId,
    filters: Vec<NostrFilter>,
    mut close_rx: watch::Receiver<bool>,
) {
    let mut relay_complete = false;
    let Ok((socket, _)) = connect_async(relay_url.as_str()).await else {
        tracing::warn!(
            "upstream nostr relay connect failed: client_id={} subscription_id={} relay={}",
            client_id,
            subscription_id,
            relay_url,
        );
        mark_upstream_nostr_relay_complete(&state, client_id, &subscription_id).await;
        return;
    };
    let (mut write, mut read) = socket.split();
    let request = NostrClientMessage::req(subscription_id.clone(), filters).as_json();
    state
        .ws_relay
        .note_upstream_relay_send(request.as_bytes().len());
    if write
        .send(TungsteniteMessage::Text(request.into()))
        .await
        .is_err()
    {
        tracing::warn!(
            "upstream nostr relay request send failed: client_id={} subscription_id={} relay={}",
            client_id,
            subscription_id,
            relay_url,
        );
        mark_upstream_nostr_relay_complete(&state, client_id, &subscription_id).await;
        return;
    }

    loop {
        tokio::select! {
            _ = close_rx.changed() => {
                if *close_rx.borrow() {
                    let close = NostrClientMessage::close(subscription_id.clone()).as_json();
                    state
                        .ws_relay
                        .note_upstream_relay_send(close.as_bytes().len());
                    let _ = write.send(TungsteniteMessage::Text(close.into())).await;
                    let _ = write.close().await;
                    break;
                }
            }
            message = read.next() => {
                match message {
                    Some(Ok(TungsteniteMessage::Text(text))) => {
                        state
                            .ws_relay
                            .note_upstream_relay_receive(text.as_bytes().len());
                        if matches!(
                            NostrRelayMessage::from_json(text.as_str()),
                            Ok(NostrRelayMessage::EndOfStoredEvents(sid)) if sid.as_ref() == &subscription_id
                        ) {
                            if !relay_complete {
                                relay_complete = true;
                                mark_upstream_nostr_relay_complete(&state, client_id, &subscription_id).await;
                            }
                            continue;
                        }
                        forward_upstream_nostr_message(&state, client_id, &subscription_id, &text).await;
                    }
                    Some(Ok(TungsteniteMessage::Ping(payload))) => {
                        let _ = write.send(TungsteniteMessage::Pong(payload)).await;
                    }
                    Some(Ok(TungsteniteMessage::Close(_))) | None => {
                        if !relay_complete {
                            mark_upstream_nostr_relay_complete(&state, client_id, &subscription_id).await;
                        }
                        break;
                    }
                    Some(Err(_)) => {
                        if !relay_complete {
                            mark_upstream_nostr_relay_complete(&state, client_id, &subscription_id).await;
                        }
                        break;
                    }
                    _ => {}
                }
            }
        }
    }
}

async fn start_upstream_nostr_subscription(
    state: &AppState,
    client_id: u64,
    subscription_id: SubscriptionId,
    filters: Vec<NostrFilter>,
) -> usize {
    let memory_before = process_memory_snapshot();
    close_upstream_nostr_subscription(state, client_id, &subscription_id).await;
    if state.nostr_relay_urls.is_empty() || filters.is_empty() {
        tracing::info!(
            "upstream nostr relay skipped: client_id={} subscription_id={} relays={} filters={}",
            client_id,
            subscription_id,
            state.nostr_relay_urls.len(),
            filters.len(),
        );
        return 0;
    }

    let mut relay_urls = Vec::new();
    let mut seen = HashSet::new();
    for relay in &state.nostr_relay_urls {
        let relay = relay.trim();
        if relay.is_empty() || !seen.insert(relay.to_string()) {
            continue;
        }
        relay_urls.push(relay.to_string());
    }
    if relay_urls.is_empty() {
        tracing::info!(
            "upstream nostr relay skipped after normalization: client_id={} subscription_id={}",
            client_id,
            subscription_id,
        );
        return 0;
    }

    tracing::info!(
        "upstream nostr relay start: client_id={} subscription_id={} relays={}",
        client_id,
        subscription_id,
        relay_urls.len(),
    );

    let filter_summary = nostr_filters_summary(&filters);
    let key = (client_id, subscription_id.to_string());
    state
        .ws_relay
        .upstream_seen_events
        .lock()
        .await
        .insert(key.clone(), HashSet::new());
    state
        .ws_relay
        .upstream_pending_eose
        .lock()
        .await
        .insert(key.clone(), relay_urls.len());

    let (close_tx, close_rx) = watch::channel(false);
    let mut tasks = Vec::new();
    for relay_url in &relay_urls {
        tasks.push(tokio::spawn(run_upstream_nostr_subscription(
            state.clone(),
            client_id,
            relay_url.clone(),
            subscription_id.clone(),
            filters.clone(),
            close_rx.clone(),
        )));
    }

    tracing::info!(
        target: "hashtree_cli::server::ws_relay::upstream",
        client_id,
        subscription_id = %subscription_id,
        relays = relay_urls.len(),
        tasks = tasks.len(),
        filters = filters.len(),
        filter = %filter_summary,
        memory_before = ?memory_before,
        memory_after = ?process_memory_snapshot(),
        "upstream nostr relay tasks spawned",
    );

    state
        .ws_relay
        .upstream_nostr_subscriptions
        .lock()
        .await
        .insert(key, UpstreamNostrSubscription { close_tx, tasks });
    relay_urls.len()
}

async fn handle_message(client_id: u64, msg: Message, state: &AppState) {
    match msg {
        Message::Text(text) => {
            if let Some(msg) = parse_ws_text_message(&text) {
                match msg {
                    WsTextMessage::Hashtree(msg) => {
                        set_client_protocol(state, client_id, WsProtocol::HashtreeJson).await;
                        match msg {
                            WsClientMessage::Request { id, hash } => {
                                handle_request(
                                    client_id,
                                    id,
                                    hash,
                                    WsProtocol::HashtreeJson,
                                    state,
                                )
                                .await;
                            }
                            WsClientMessage::Response { id, hash, found } => {
                                handle_response(client_id, id, hash, found, state).await;
                            }
                        }
                    }
                    WsTextMessage::Nostr(msg) => {
                        if let Some(relay) = &state.nostr_relay {
                            match msg {
                                NostrClientMessage::Req {
                                    subscription_id,
                                    filters,
                                } => {
                                    let subscription_id = subscription_id.into_owned();
                                    let filters = filters
                                        .into_iter()
                                        .map(|filter| filter.into_owned())
                                        .collect::<Vec<_>>();
                                    let local_events = match relay
                                        .register_subscription_query(
                                            client_id,
                                            subscription_id.clone(),
                                            filters.clone(),
                                        )
                                        .await
                                    {
                                        Ok(events) => events,
                                        Err(message) => {
                                            send_nostr(
                                                state,
                                                client_id,
                                                NostrRelayMessage::closed(subscription_id, message),
                                            )
                                            .await;
                                            return;
                                        }
                                    };

                                    let upstream_relays = start_upstream_nostr_subscription(
                                        state,
                                        client_id,
                                        subscription_id.clone(),
                                        filters,
                                    )
                                    .await;
                                    if upstream_relays > 0 {
                                        let key = (client_id, subscription_id.to_string());
                                        let mut seen_events =
                                            state.ws_relay.upstream_seen_events.lock().await;
                                        seen_events.entry(key).or_default().extend(
                                            local_events.iter().map(|event| event.id.to_hex()),
                                        );
                                    }
                                    for event in local_events {
                                        send_nostr(
                                            state,
                                            client_id,
                                            NostrRelayMessage::event(
                                                subscription_id.clone(),
                                                event,
                                            ),
                                        )
                                        .await;
                                    }
                                    trim_process_allocations();
                                    if upstream_relays == 0 {
                                        send_nostr(
                                            state,
                                            client_id,
                                            NostrRelayMessage::eose(subscription_id),
                                        )
                                        .await;
                                    }
                                }
                                NostrClientMessage::Close(subscription_id) => {
                                    let subscription_id = subscription_id.into_owned();
                                    close_upstream_nostr_subscription(
                                        state,
                                        client_id,
                                        &subscription_id,
                                    )
                                    .await;
                                    relay
                                        .handle_client_message(
                                            client_id,
                                            NostrClientMessage::close(subscription_id.clone()),
                                        )
                                        .await;
                                }
                                other => {
                                    relay.handle_client_message(client_id, other).await;
                                }
                            }
                        } else {
                            handle_nostr_message(client_id, msg, state).await;
                        }
                    }
                }
            }
        }
        Message::Binary(data) => {
            handle_binary(client_id, data, state).await;
        }
        Message::Close(_) => {}
        _ => {}
    }
}

async fn handle_request(
    client_id: u64,
    request_id: u32,
    hash: String,
    origin_protocol: WsProtocol,
    state: &AppState,
) {
    let hash_hex = hash.to_lowercase();
    let hash_bytes = match from_hex(&hash_hex) {
        Ok(bytes) => bytes,
        Err(_) => {
            if origin_protocol == WsProtocol::HashtreeJson {
                send_json(
                    state,
                    client_id,
                    WsResponse {
                        kind: "res",
                        id: request_id,
                        hash,
                        found: false,
                    },
                )
                .await;
            }
            return;
        }
    };

    if let Ok(Some(data)) = state.store.get_blob(&hash_bytes) {
        match origin_protocol {
            WsProtocol::HashtreeJson => {
                send_json(
                    state,
                    client_id,
                    WsResponse {
                        kind: "res",
                        id: request_id,
                        hash: hash.clone(),
                        found: true,
                    },
                )
                .await;
                send_binary(state, client_id, request_id, data).await;
            }
            WsProtocol::HashtreeMsgpack => {
                send_msgpack_response(state, client_id, &hash_bytes, &data).await;
            }
            WsProtocol::Unknown => {}
        }
        return;
    }

    let peers: Vec<(u64, mpsc::UnboundedSender<Message>, WsProtocol)> = {
        let clients = state.ws_relay.clients.lock().await;
        let protocols = state.ws_relay.client_protocols.lock().await;
        clients
            .iter()
            .filter(|(id, _)| **id != client_id)
            .filter_map(|(id, tx)| {
                let protocol = protocols.get(id).copied().unwrap_or(WsProtocol::Unknown);
                match protocol {
                    WsProtocol::HashtreeJson | WsProtocol::HashtreeMsgpack => {
                        Some((*id, tx.clone(), protocol))
                    }
                    WsProtocol::Unknown => None,
                }
            })
            .collect()
    };

    if peers.is_empty() {
        if origin_protocol == WsProtocol::HashtreeJson {
            send_json(
                state,
                client_id,
                WsResponse {
                    kind: "res",
                    id: request_id,
                    hash,
                    found: false,
                },
            )
            .await;
        }
        return;
    }

    {
        let mut pending = state.ws_relay.pending.lock().await;
        for (peer_id, _, _) in &peers {
            pending.insert(
                (*peer_id, request_id),
                PendingRequest {
                    origin_id: client_id,
                    hash: hash.clone(),
                    found: false,
                    origin_protocol,
                },
            );
        }
    }

    let request_text = serde_json::to_string(&WsRequest {
        kind: "req".to_string(),
        id: request_id,
        hash: hash.clone(),
    })
    .unwrap_or_else(|_| String::new());
    for (peer_id, tx, protocol) in peers {
        match protocol {
            WsProtocol::HashtreeMsgpack => {
                let _ = send_msgpack_request(state, peer_id, &hash_bytes).await;
            }
            WsProtocol::HashtreeJson => {
                let _ = tx.send(Message::Text(request_text.clone()));
            }
            WsProtocol::Unknown => {}
        }
    }

    let timeout_state = state.clone();
    let timeout_hash = hash.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let mut pending = timeout_state.ws_relay.pending.lock().await;
        let still_pending = pending
            .iter()
            .any(|((_, id), p)| *id == request_id && p.origin_id == client_id);
        let already_found = pending
            .iter()
            .any(|((_, id), p)| *id == request_id && p.origin_id == client_id && p.found);
        if !still_pending || already_found {
            return;
        }
        let origin_protocol = pending
            .iter()
            .find(|((_, id), p)| *id == request_id && p.origin_id == client_id)
            .map(|(_, p)| p.origin_protocol)
            .unwrap_or(WsProtocol::HashtreeJson);
        pending.retain(|(_, id), p| !(*id == request_id && p.origin_id == client_id));
        drop(pending);
        if origin_protocol == WsProtocol::HashtreeJson {
            send_json(
                &timeout_state,
                client_id,
                WsResponse {
                    kind: "res",
                    id: request_id,
                    hash: timeout_hash,
                    found: false,
                },
            )
            .await;
        }
    });
}

async fn handle_response(
    client_id: u64,
    request_id: u32,
    _hash: String,
    found: bool,
    state: &AppState,
) {
    let pending_entry = {
        let pending = state.ws_relay.pending.lock().await;
        pending
            .get(&(client_id, request_id))
            .map(|p| (p.origin_id, p.hash.clone(), p.found, p.origin_protocol))
    };

    let Some((origin_id, pending_hash, already_found, origin_protocol)) = pending_entry else {
        return;
    };

    if already_found && !found {
        let mut pending = state.ws_relay.pending.lock().await;
        pending.remove(&(client_id, request_id));
        return;
    }

    if found {
        let mut pending = state.ws_relay.pending.lock().await;
        for ((_, id), p) in pending.iter_mut() {
            if *id == request_id && p.origin_id == origin_id {
                p.found = true;
            }
        }
        drop(pending);
        if origin_protocol == WsProtocol::HashtreeJson {
            send_json(
                state,
                origin_id,
                WsResponse {
                    kind: "res",
                    id: request_id,
                    hash: pending_hash,
                    found: true,
                },
            )
            .await;
        }
        return;
    }

    let mut pending = state.ws_relay.pending.lock().await;
    pending.remove(&(client_id, request_id));
    let has_remaining = pending
        .iter()
        .any(|((_, id), p)| *id == request_id && p.origin_id == origin_id);
    drop(pending);

    if !has_remaining && origin_protocol == WsProtocol::HashtreeJson {
        send_json(
            state,
            origin_id,
            WsResponse {
                kind: "res",
                id: request_id,
                hash: pending_hash,
                found: false,
            },
        )
        .await;
    }
}

async fn handle_binary(client_id: u64, data: Vec<u8>, state: &AppState) {
    if let Some(msg) = parse_msgpack_message(&data) {
        set_client_protocol(state, client_id, WsProtocol::HashtreeMsgpack).await;
        match msg {
            DataMessage::Request(req) => {
                let hash_hex = hex_encode(&req.h);
                let request_id = state.ws_relay.next_request_id();
                handle_request(
                    client_id,
                    request_id,
                    hash_hex,
                    WsProtocol::HashtreeMsgpack,
                    state,
                )
                .await;
            }
            DataMessage::Response(res) => {
                handle_msgpack_response(client_id, res, state).await;
            }
            DataMessage::QuoteRequest(_)
            | DataMessage::QuoteResponse(_)
            | DataMessage::Payment(_)
            | DataMessage::PaymentAck(_)
            | DataMessage::Chunk(_)
            | DataMessage::PeerHints(_)
            | DataMessage::PubsubInterest(_)
            | DataMessage::PubsubFrame(_)
            | DataMessage::PubsubInventory(_)
            | DataMessage::PubsubWant(_) => {}
        }
        return;
    }

    // Legacy binary: [4-byte LE request_id][data]
    if data.len() < 4 {
        return;
    }
    let request_id = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    let pending_entry = {
        let pending = state.ws_relay.pending.lock().await;
        pending
            .get(&(client_id, request_id))
            .map(|p| (p.origin_id, p.hash.clone(), p.origin_protocol))
    };
    let Some((origin_id, hash_hex, origin_protocol)) = pending_entry else {
        return;
    };

    match origin_protocol {
        WsProtocol::HashtreeJson => {
            send_binary(state, origin_id, request_id, data[4..].to_vec()).await;
        }
        WsProtocol::HashtreeMsgpack => {
            let Ok(hash_bytes) = from_hex(&hash_hex) else {
                return;
            };
            send_msgpack_response(state, origin_id, &hash_bytes, &data[4..]).await;
        }
        WsProtocol::Unknown => {}
    }

    let mut pending = state.ws_relay.pending.lock().await;
    pending.retain(|(_, id), p| !(*id == request_id && p.origin_id == origin_id));
}

async fn handle_nostr_message(client_id: u64, msg: NostrClientMessage<'_>, state: &AppState) {
    let replies = nostr_responses_for(&msg);
    for reply in replies {
        send_nostr(state, client_id, reply).await;
    }
}

fn nostr_responses_for(msg: &NostrClientMessage<'_>) -> Vec<NostrRelayMessage<'static>> {
    match msg {
        NostrClientMessage::Event(event) => {
            let ok = event.verify().is_ok();
            let message = if ok { "" } else { "invalid: signature" };
            vec![NostrRelayMessage::ok(event.id, ok, message)]
        }
        NostrClientMessage::Req {
            subscription_id, ..
        } => {
            vec![NostrRelayMessage::eose(
                subscription_id.clone().into_owned(),
            )]
        }
        NostrClientMessage::Count {
            subscription_id, ..
        } => {
            vec![NostrRelayMessage::count(
                subscription_id.clone().into_owned(),
                0,
            )]
        }
        NostrClientMessage::Close(_) => Vec::new(),
        NostrClientMessage::Auth(event) => {
            let ok = event.verify().is_ok();
            let message = if ok { "" } else { "invalid auth" };
            vec![NostrRelayMessage::ok(event.id, ok, message)]
        }
        NostrClientMessage::NegOpen { .. }
        | NostrClientMessage::NegMsg { .. }
        | NostrClientMessage::NegClose { .. } => {
            vec![NostrRelayMessage::notice("negentropy not supported")]
        }
    }
}

async fn send_nostr(state: &AppState, client_id: u64, response: NostrRelayMessage<'_>) {
    let text = response.as_json();
    send_to_client(state, client_id, Message::Text(text)).await;
}

fn parse_msgpack_message(data: &[u8]) -> Option<DataMessage> {
    let msg = parse_message(data).ok()?;
    match msg {
        DataMessage::Request(req) => {
            if req.h.len() == 32 {
                Some(DataMessage::Request(req))
            } else {
                None
            }
        }
        DataMessage::Response(res) => {
            if res.h.len() == 32 {
                Some(DataMessage::Response(res))
            } else {
                None
            }
        }
        DataMessage::QuoteRequest(req) => {
            if req.h.len() == 32 {
                Some(DataMessage::QuoteRequest(req))
            } else {
                None
            }
        }
        DataMessage::QuoteResponse(res) => {
            if res.h.len() == 32 {
                Some(DataMessage::QuoteResponse(res))
            } else {
                None
            }
        }
        DataMessage::Payment(req) => {
            if req.h.len() == 32 {
                Some(DataMessage::Payment(req))
            } else {
                None
            }
        }
        DataMessage::PaymentAck(res) => {
            if res.h.len() == 32 {
                Some(DataMessage::PaymentAck(res))
            } else {
                None
            }
        }
        DataMessage::Chunk(chunk) => {
            if chunk.h.len() == 32 {
                Some(DataMessage::Chunk(chunk))
            } else {
                None
            }
        }
        DataMessage::PeerHints(_)
        | DataMessage::PubsubInterest(_)
        | DataMessage::PubsubFrame(_)
        | DataMessage::PubsubInventory(_)
        | DataMessage::PubsubWant(_) => Some(msg),
    }
}

async fn handle_msgpack_response(client_id: u64, res: DataResponse, state: &AppState) {
    let hash_hex = hex_encode(&res.h);
    let data = res.d.clone();
    let hash_bytes = res.h.clone();

    let mut responses: Vec<(u64, u32, WsProtocol)> = Vec::new();
    let mut seen = HashSet::new();
    {
        let pending = state.ws_relay.pending.lock().await;
        for ((peer_id, request_id), p) in pending.iter() {
            if *peer_id != client_id {
                continue;
            }
            if p.hash != hash_hex {
                continue;
            }
            if seen.insert((p.origin_id, *request_id)) {
                responses.push((p.origin_id, *request_id, p.origin_protocol));
            }
        }
    }

    if responses.is_empty() {
        return;
    }

    for (origin_id, request_id, protocol) in &responses {
        match protocol {
            WsProtocol::HashtreeJson => {
                send_json(
                    state,
                    *origin_id,
                    WsResponse {
                        kind: "res",
                        id: *request_id,
                        hash: hash_hex.clone(),
                        found: true,
                    },
                )
                .await;
                send_binary(state, *origin_id, *request_id, data.clone()).await;
            }
            WsProtocol::HashtreeMsgpack => {
                send_msgpack_response(state, *origin_id, &hash_bytes, &data).await;
            }
            WsProtocol::Unknown => {}
        }
    }

    let completed: HashSet<(u64, u32)> = responses
        .into_iter()
        .map(|(origin_id, request_id, _)| (origin_id, request_id))
        .collect();
    let mut pending = state.ws_relay.pending.lock().await;
    pending.retain(|(_, id), p| !completed.contains(&(p.origin_id, *id)));
}

async fn send_json(state: &AppState, client_id: u64, response: WsResponse) {
    if let Ok(text) = serde_json::to_string(&response) {
        send_to_client(state, client_id, Message::Text(text)).await;
    }
}

async fn send_msgpack_request(
    state: &AppState,
    client_id: u64,
    hash: &[u8],
) -> Result<(), rmp_serde::encode::Error> {
    let req = DataRequest {
        h: hash.to_vec(),
        htl: MAX_HTL,
        q: None,
    };
    let wire = encode_request(&req)?;
    send_to_client(state, client_id, Message::Binary(wire)).await;
    Ok(())
}

async fn send_msgpack_response(state: &AppState, client_id: u64, hash: &[u8], data: &[u8]) {
    let res = DataResponse {
        h: hash.to_vec(),
        d: data.to_vec(),
        i: None,
        n: None,
    };
    if let Ok(wire) = encode_response(&res) {
        send_to_client(state, client_id, Message::Binary(wire)).await;
    }
}

async fn send_binary(state: &AppState, client_id: u64, request_id: u32, payload: Vec<u8>) {
    let mut packet = Vec::with_capacity(4 + payload.len());
    packet.extend_from_slice(&request_id.to_le_bytes());
    packet.extend_from_slice(&payload);
    send_to_client(state, client_id, Message::Binary(packet)).await;
}

async fn send_to_client(state: &AppState, client_id: u64, msg: Message) {
    let sender = {
        let clients = state.ws_relay.clients.lock().await;
        clients.get(&client_id).cloned()
    };
    if let Some(tx) = sender {
        let _ = tx.send(msg);
    }
}

async fn set_client_protocol(state: &AppState, client_id: u64, protocol: WsProtocol) {
    let mut protocols = state.ws_relay.client_protocols.lock().await;
    protocols.insert(client_id, protocol);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nostr_relay::{NostrRelay, NostrRelayConfig};
    use anyhow::Result;
    use futures::{SinkExt, StreamExt};
    use nostr::secp256k1::schnorr::Signature;
    use nostr::{EventBuilder, Filter, Keys, Kind, SubscriptionId};
    use std::collections::HashSet;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::net::TcpListener;
    use tokio_tungstenite::{accept_async, tungstenite::Message as TungsteniteMessage};

    #[test]
    fn parse_ws_text_message_detects_nostr_req() {
        let msg = r#"["REQ","sub-1",{"kinds":[1]}]"#;
        match parse_ws_text_message(msg) {
            Some(WsTextMessage::Nostr(_)) => {}
            other => panic!("expected Nostr message, got {:?}", other),
        }
    }

    #[test]
    fn parse_ws_text_message_detects_hashtree_request() {
        let msg = r#"{"type":"req","id":1,"hash":"abcd"}"#;
        match parse_ws_text_message(msg) {
            Some(WsTextMessage::Hashtree(_)) => {}
            other => panic!("expected Hashtree message, got {:?}", other),
        }
    }

    #[test]
    fn nostr_replies_for_req_is_eose() {
        let sub = SubscriptionId::new("sub-1");
        let msg = NostrClientMessage::req(sub.clone(), vec![]);
        let replies = nostr_responses_for(&msg);
        assert_eq!(replies.len(), 1);
        match &replies[0] {
            NostrRelayMessage::EndOfStoredEvents(id) => assert_eq!(id.as_ref(), &sub),
            other => panic!("expected EOSE, got {:?}", other),
        }
    }

    #[test]
    fn nostr_replies_for_event_ok() {
        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::TextNote, "hello")
            .sign_with_keys(&keys)
            .unwrap();
        let msg = NostrClientMessage::event(event.clone());
        let replies = nostr_responses_for(&msg);
        assert_eq!(replies.len(), 1);
        match &replies[0] {
            NostrRelayMessage::Ok {
                event_id, status, ..
            } => {
                assert_eq!(event_id, &event.id);
                assert!(*status);
            }
            other => panic!("expected OK, got {:?}", other),
        }
    }

    #[test]
    fn nostr_replies_for_invalid_event_is_not_ok() {
        let keys = Keys::generate();
        let mut event = EventBuilder::new(Kind::TextNote, "hello")
            .sign_with_keys(&keys)
            .unwrap();
        event.sig = Signature::from_slice(&[0u8; 64]).unwrap();
        let msg = NostrClientMessage::event(event);
        let replies = nostr_responses_for(&msg);
        assert_eq!(replies.len(), 1);
        match &replies[0] {
            NostrRelayMessage::Ok { status, .. } => assert!(!*status),
            other => panic!("expected OK=false, got {:?}", other),
        }
    }

    async fn spawn_mock_upstream_relay(events: Vec<nostr::Event>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind relay");
        let addr = listener.local_addr().expect("relay addr");
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept relay");
            let ws = accept_async(stream).await.expect("accept websocket");
            let (mut write, mut read) = ws.split();

            while let Some(Ok(message)) = read.next().await {
                let TungsteniteMessage::Text(text) = message else {
                    continue;
                };
                let Ok(parsed) = NostrClientMessage::from_json(text.as_bytes()) else {
                    continue;
                };
                if let NostrClientMessage::Req {
                    subscription_id,
                    filters,
                } = parsed
                {
                    let subscription_id = subscription_id.into_owned();
                    let filters = filters
                        .into_iter()
                        .map(|filter| filter.into_owned())
                        .collect::<Vec<_>>();
                    for event in events.iter().filter(|event| {
                        filters
                            .iter()
                            .any(|filter| filter.match_event(event, Default::default()))
                    }) {
                        let _ = write
                            .send(TungsteniteMessage::Text(
                                NostrRelayMessage::event(subscription_id.clone(), event.clone())
                                    .as_json()
                                    .into(),
                            ))
                            .await;
                    }
                    let _ = write
                        .send(TungsteniteMessage::Text(
                            NostrRelayMessage::eose(subscription_id).as_json().into(),
                        ))
                        .await;
                }
            }
        });
        format!("ws://{}", addr)
    }

    fn test_app_state(
        tmp: &TempDir,
        relay: Arc<NostrRelay>,
        relay_url: String,
    ) -> Result<AppState> {
        let store = Arc::new(crate::storage::HashtreeStore::with_options(
            tmp.path(),
            None,
            128 * 1024 * 1024,
        )?);
        Ok(AppState {
            store,
            auth: None,
            daemon_started_at: 1_700_000_000,
            peer_mode: crate::config::ServerMode::Normal,
            hash_get_enabled: true,
            http_webrtc_fetch: true,
            webrtc_peers: None,
            fips_transport: None,
            fetch_from_fips_peers: true,
            ws_relay: Arc::new(super::super::auth::WsRelayState::new()),
            max_upload_bytes: 5 * 1024 * 1024,
            public_writes: true,
            public_plaintext_reads: true,
            require_random_untrusted_ingest: false,
            optimistic_blossom_uploads: false,
            optimistic_upload_queue_bytes: 512 * 1024 * 1024,
            optimistic_upload_queue: Arc::new(tokio::sync::Semaphore::new(512 * 1024 * 1024)),
            allowed_pubkeys: HashSet::new(),
            upstream_blossom: Vec::new(),
            upstream_http_client: super::super::new_upstream_http_client(),
            upstream_blossom_miss_cache: Arc::new(std::sync::Mutex::new(
                super::super::auth::new_lookup_cache(),
            )),
            upstream_blossom_fetch_metrics: Arc::new(
                super::super::auth::UpstreamBlossomFetchMetrics::default(),
            ),
            blossom_upload_replicas: Vec::new(),
            blossom_upload_replica_queue_bytes: 512 * 1024 * 1024,
            blossom_upload_replica_queue: Arc::new(tokio::sync::Semaphore::new(512 * 1024 * 1024)),
            blossom_upload_replica_keys: None,
            blossom_upload_replica_scheduler: Arc::new(
                super::super::blossom::BlossomUploadReplicaScheduler::new(),
            ),
            social_graph: None,
            social_graph_store: None,
            social_graph_root: None,
            socialgraph_snapshot_public: false,
            nostr_relay: Some(relay),
            nostr_relay_urls: vec![relay_url],
            tree_root_cache: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            inflight_blob_fetches: Arc::new(tokio::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            inflight_blob_reads: Arc::new(
                tokio::sync::Mutex::new(std::collections::HashMap::new()),
            ),
            blob_cache: Arc::new(crate::blob_cache::BlobCache::for_tests()),
            directory_listing_cache: Arc::new(std::sync::Mutex::new(
                super::super::auth::new_lookup_cache(),
            )),
            resolved_path_cache: Arc::new(std::sync::Mutex::new(
                super::super::auth::new_lookup_cache(),
            )),
            thumbnail_path_cache: Arc::new(std::sync::Mutex::new(
                super::super::auth::new_lookup_cache(),
            )),
            cid_size_cache: Arc::new(std::sync::Mutex::new(super::super::auth::new_lookup_cache())),
        })
    }

    #[tokio::test]
    async fn upstream_proxy_forwards_events_and_caches_them() -> Result<()> {
        let tmp = TempDir::new()?;
        let graph_store = {
            let _guard = crate::socialgraph::test_lock();
            crate::socialgraph::open_test_social_graph_store_with_mapsize(
                tmp.path(),
                Some(128 * 1024 * 1024),
            )?
        };
        let backend: Arc<dyn crate::socialgraph::SocialGraphBackend> = graph_store.clone();
        let access = Arc::new(crate::socialgraph::SocialGraphAccessControl::new(
            Arc::clone(&backend),
            0,
            HashSet::new(),
        ));

        let keys = Keys::generate();
        let relay = Arc::new(NostrRelay::new(
            Arc::clone(&backend),
            tmp.path().to_path_buf(),
            HashSet::from([keys.public_key().to_hex()]),
            Some(access),
            NostrRelayConfig {
                spambox_db_max_bytes: 0,
                ..Default::default()
            },
        )?);

        let event = EventBuilder::new(Kind::from(30078_u16), "")
            .tags([
                nostr::Tag::parse(["d", "videos/Test"]).expect("d tag"),
                nostr::Tag::parse(["l", "hashtree"]).expect("label tag"),
            ])
            .sign_with_keys(&keys)?;

        let relay_url = spawn_mock_upstream_relay(vec![event.clone()]).await;
        let filter = Filter::new()
            .authors(vec![event.pubkey])
            .kinds(vec![event.kind]);
        let state = test_app_state(&tmp, relay.clone(), relay_url)?;
        let client_id = 7_u64;
        let (tx, mut rx) = mpsc::unbounded_channel();
        state.ws_relay.clients.lock().await.insert(client_id, tx);
        let subscription_id = SubscriptionId::new("sub-1");

        start_upstream_nostr_subscription(
            &state,
            client_id,
            subscription_id.clone(),
            vec![filter.clone()],
        )
        .await;

        let forwarded = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await?
            .expect("forwarded upstream event");
        let Message::Text(text) = forwarded else {
            panic!("expected text event");
        };
        match NostrRelayMessage::from_json(text.as_str())? {
            NostrRelayMessage::Event {
                subscription_id: sid,
                event: forwarded_event,
            } => {
                assert_eq!(sid.as_ref(), &subscription_id);
                assert_eq!(forwarded_event.id, event.id);
            }
            other => panic!("expected forwarded EVENT, got {:?}", other),
        }

        let eose = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await?
            .expect("forwarded upstream eose");
        let Message::Text(eose_text) = eose else {
            panic!("expected text eose");
        };
        match NostrRelayMessage::from_json(eose_text.as_str())? {
            NostrRelayMessage::EndOfStoredEvents(sid) => {
                assert_eq!(sid.as_ref(), &subscription_id);
            }
            other => panic!("expected forwarded EOSE, got {:?}", other),
        }

        let events = relay.query_events(&filter, 10).await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, event.id);

        close_upstream_nostr_subscription(&state, client_id, &subscription_id).await;
        assert!(state
            .ws_relay
            .upstream_nostr_subscriptions
            .lock()
            .await
            .is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn req_waits_for_upstream_event_before_eose() -> Result<()> {
        let tmp = TempDir::new()?;
        let graph_store = {
            let _guard = crate::socialgraph::test_lock();
            crate::socialgraph::open_test_social_graph_store_with_mapsize(
                tmp.path(),
                Some(128 * 1024 * 1024),
            )?
        };
        let backend: Arc<dyn crate::socialgraph::SocialGraphBackend> = graph_store.clone();
        let access = Arc::new(crate::socialgraph::SocialGraphAccessControl::new(
            Arc::clone(&backend),
            0,
            HashSet::new(),
        ));

        let keys = Keys::generate();
        let relay = Arc::new(NostrRelay::new(
            Arc::clone(&backend),
            tmp.path().to_path_buf(),
            HashSet::from([keys.public_key().to_hex()]),
            Some(access),
            NostrRelayConfig {
                spambox_db_max_bytes: 0,
                ..Default::default()
            },
        )?);

        let event = EventBuilder::new(Kind::from(30078_u16), "")
            .tags([
                nostr::Tag::parse(["d", "videos/Test"]).expect("d tag"),
                nostr::Tag::parse(["l", "hashtree"]).expect("label tag"),
            ])
            .sign_with_keys(&keys)?;

        let relay_url = spawn_mock_upstream_relay(vec![event.clone()]).await;
        let state = test_app_state(&tmp, relay.clone(), relay_url)?;
        let client_id = 11_u64;
        let (ws_tx, mut ws_rx) = mpsc::unbounded_channel();
        let (relay_tx, _relay_rx) = mpsc::unbounded_channel();
        state.ws_relay.clients.lock().await.insert(client_id, ws_tx);
        relay.register_client(client_id, relay_tx, None).await;

        let request = NostrClientMessage::req(
            SubscriptionId::new("feed"),
            vec![Filter::new()
                .authors(vec![event.pubkey])
                .kinds(vec![event.kind])],
        )
        .as_json();

        handle_message(client_id, Message::Text(request.into()), &state).await;

        let first = tokio::time::timeout(std::time::Duration::from_secs(2), ws_rx.recv())
            .await?
            .expect("first forwarded message");
        let Message::Text(first_text) = first else {
            panic!("expected text event");
        };
        match NostrRelayMessage::from_json(first_text.as_str())? {
            NostrRelayMessage::Event {
                event: forwarded_event,
                ..
            } => {
                assert_eq!(forwarded_event.id, event.id);
            }
            other => panic!("expected upstream EVENT before EOSE, got {:?}", other),
        }

        let second = tokio::time::timeout(std::time::Duration::from_secs(2), ws_rx.recv())
            .await?
            .expect("second forwarded message");
        let Message::Text(second_text) = second else {
            panic!("expected text eose");
        };
        match NostrRelayMessage::from_json(second_text.as_str())? {
            NostrRelayMessage::EndOfStoredEvents(sid) => {
                assert_eq!(sid.as_ref(), &SubscriptionId::new("feed"));
            }
            other => panic!("expected aggregated EOSE, got {:?}", other),
        }

        Ok(())
    }

    #[tokio::test]
    async fn websocket_publish_returns_ok_for_trusted_event() -> Result<()> {
        let tmp = TempDir::new()?;
        let graph_store = {
            let _guard = crate::socialgraph::test_lock();
            crate::socialgraph::open_test_social_graph_store_with_mapsize(
                tmp.path(),
                Some(128 * 1024 * 1024),
            )?
        };
        let author_keys = Keys::generate();
        let backend: Arc<dyn crate::socialgraph::SocialGraphBackend> = graph_store.clone();
        let access = Arc::new(crate::socialgraph::SocialGraphAccessControl::new(
            Arc::clone(&backend),
            0,
            HashSet::from([author_keys.public_key().to_hex()]),
        ));
        let relay = Arc::new(NostrRelay::new(
            Arc::clone(&backend),
            tmp.path().to_path_buf(),
            HashSet::from([author_keys.public_key().to_hex()]),
            Some(access),
            NostrRelayConfig {
                spambox_db_max_bytes: 0,
                ..Default::default()
            },
        )?);

        let state = test_app_state(&tmp, relay.clone(), String::new())?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let client_pubkey = author_keys.public_key().to_hex();
        let app = axum::Router::new().route(
            "/ws",
            axum::routing::get({
                let state = state.clone();
                let client_pubkey = client_pubkey.clone();
                move |ws: WebSocketUpgrade| {
                    let state = state.clone();
                    let client_pubkey = client_pubkey.clone();
                    async move { ws_data_with_client_pubkey(state, ws, Some(client_pubkey)) }
                }
            }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let (mut socket, _) = connect_async(format!("ws://{addr}/ws")).await?;
        let event = EventBuilder::new(Kind::TextNote, "websocket publish ack")
            .sign_with_keys(&author_keys)?;
        socket
            .send(TungsteniteMessage::Text(
                NostrClientMessage::event(event.clone()).as_json().into(),
            ))
            .await?;

        let reply = tokio::time::timeout(std::time::Duration::from_secs(2), socket.next())
            .await?
            .ok_or_else(|| anyhow::anyhow!("websocket closed before publish ack"))??;
        let TungsteniteMessage::Text(text) = reply else {
            anyhow::bail!("expected text publish ack");
        };

        match NostrRelayMessage::from_json(text.as_str())? {
            NostrRelayMessage::Ok {
                event_id, status, ..
            } => {
                assert_eq!(event_id, event.id);
                assert!(status);
            }
            other => anyhow::bail!("expected OK publish ack, got {:?}", other),
        }

        let stored = relay
            .query_events(
                &Filter::new()
                    .authors(vec![event.pubkey])
                    .kinds(vec![event.kind]),
                10,
            )
            .await;
        assert!(stored.iter().any(|candidate| candidate.id == event.id));
        Ok(())
    }

    #[tokio::test]
    async fn websocket_req_is_rate_limited_after_configured_quota() -> Result<()> {
        let tmp = TempDir::new()?;
        let graph_store = {
            let _guard = crate::socialgraph::test_lock();
            crate::socialgraph::open_test_social_graph_store_with_mapsize(
                tmp.path(),
                Some(128 * 1024 * 1024),
            )?
        };
        let backend: Arc<dyn crate::socialgraph::SocialGraphBackend> = graph_store.clone();
        let access = Arc::new(crate::socialgraph::SocialGraphAccessControl::new(
            Arc::clone(&backend),
            0,
            HashSet::new(),
        ));
        let relay = Arc::new(NostrRelay::new(
            Arc::clone(&backend),
            tmp.path().to_path_buf(),
            HashSet::new(),
            Some(access),
            NostrRelayConfig {
                spambox_db_max_bytes: 0,
                spambox_max_reqs_per_min: 1,
                ..Default::default()
            },
        )?);

        let state = test_app_state(&tmp, relay, String::new())?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let app = axum::Router::new().route(
            "/ws",
            axum::routing::get({
                let state = state.clone();
                move |ws: WebSocketUpgrade| {
                    let state = state.clone();
                    async move { ws_data_with_client_pubkey(state, ws, None) }
                }
            }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let (mut socket, _) = connect_async(format!("ws://{addr}/ws")).await?;
        socket
            .send(TungsteniteMessage::Text(
                NostrClientMessage::req(SubscriptionId::new("sub-1"), vec![Filter::new()])
                    .as_json()
                    .into(),
            ))
            .await?;

        let first = tokio::time::timeout(std::time::Duration::from_secs(2), socket.next())
            .await?
            .ok_or_else(|| anyhow::anyhow!("websocket closed before first relay reply"))??;
        let TungsteniteMessage::Text(first_text) = first else {
            anyhow::bail!("expected text EOSE reply");
        };
        match NostrRelayMessage::from_json(first_text.as_str())? {
            NostrRelayMessage::EndOfStoredEvents(subscription_id) => {
                assert_eq!(subscription_id.as_ref(), &SubscriptionId::new("sub-1"));
            }
            other => anyhow::bail!("expected EOSE for first request, got {:?}", other),
        }

        socket
            .send(TungsteniteMessage::Text(
                NostrClientMessage::req(SubscriptionId::new("sub-2"), vec![Filter::new()])
                    .as_json()
                    .into(),
            ))
            .await?;

        let second = tokio::time::timeout(std::time::Duration::from_secs(2), socket.next())
            .await?
            .ok_or_else(|| anyhow::anyhow!("websocket closed before rate-limit reply"))??;
        let TungsteniteMessage::Text(second_text) = second else {
            anyhow::bail!("expected text CLOSED reply");
        };
        let second_value: serde_json::Value = serde_json::from_str(second_text.as_str())?;
        assert_eq!(
            second_value,
            serde_json::json!(["CLOSED", "sub-2", "rate limited"])
        );

        Ok(())
    }

    #[tokio::test]
    async fn websocket_publish_is_rate_limited_for_untrusted_spambox_events() -> Result<()> {
        let tmp = TempDir::new()?;
        let graph_store = {
            let _guard = crate::socialgraph::test_lock();
            crate::socialgraph::open_test_social_graph_store_with_mapsize(
                tmp.path(),
                Some(128 * 1024 * 1024),
            )?
        };
        let backend: Arc<dyn crate::socialgraph::SocialGraphBackend> = graph_store.clone();
        let access = Arc::new(crate::socialgraph::SocialGraphAccessControl::new(
            Arc::clone(&backend),
            0,
            HashSet::new(),
        ));
        let relay = Arc::new(NostrRelay::new(
            Arc::clone(&backend),
            tmp.path().to_path_buf(),
            HashSet::new(),
            Some(access),
            NostrRelayConfig {
                spambox_db_max_bytes: 0,
                spambox_max_events_per_min: 1,
                ..Default::default()
            },
        )?);

        let state = test_app_state(&tmp, relay, String::new())?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let app = axum::Router::new().route(
            "/ws",
            axum::routing::get({
                let state = state.clone();
                move |ws: WebSocketUpgrade| {
                    let state = state.clone();
                    async move { ws_data_with_client_pubkey(state, ws, None) }
                }
            }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let (mut socket, _) = connect_async(format!("ws://{addr}/ws")).await?;
        let author_keys = Keys::generate();
        let event_a =
            EventBuilder::new(Kind::TextNote, "spambox-a").sign_with_keys(&author_keys)?;
        let event_b =
            EventBuilder::new(Kind::TextNote, "spambox-b").sign_with_keys(&author_keys)?;

        socket
            .send(TungsteniteMessage::Text(
                NostrClientMessage::event(event_a.clone()).as_json().into(),
            ))
            .await?;

        let first = tokio::time::timeout(std::time::Duration::from_secs(2), socket.next())
            .await?
            .ok_or_else(|| anyhow::anyhow!("websocket closed before first publish ack"))??;
        let TungsteniteMessage::Text(first_text) = first else {
            anyhow::bail!("expected text publish ack");
        };
        match NostrRelayMessage::from_json(first_text.as_str())? {
            NostrRelayMessage::Ok {
                event_id,
                status,
                message,
            } => {
                assert_eq!(event_id, event_a.id);
                assert!(status);
                assert_eq!(message, "spambox");
            }
            other => anyhow::bail!("expected OK publish ack, got {:?}", other),
        }

        socket
            .send(TungsteniteMessage::Text(
                NostrClientMessage::event(event_b.clone()).as_json().into(),
            ))
            .await?;

        let second = tokio::time::timeout(std::time::Duration::from_secs(2), socket.next())
            .await?
            .ok_or_else(|| anyhow::anyhow!("websocket closed before rate-limit publish ack"))??;
        let TungsteniteMessage::Text(second_text) = second else {
            anyhow::bail!("expected text publish ack");
        };
        match NostrRelayMessage::from_json(second_text.as_str())? {
            NostrRelayMessage::Ok {
                event_id,
                status,
                message,
            } => {
                assert_eq!(event_id, event_b.id);
                assert!(!status);
                assert_eq!(message, "rate limited");
            }
            other => anyhow::bail!("expected OK=false publish ack, got {:?}", other),
        }

        Ok(())
    }
}
