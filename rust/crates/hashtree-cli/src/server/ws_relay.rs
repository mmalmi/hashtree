use axum::{
    extract::{State, ws::{WebSocketUpgrade, WebSocket, Message}},
    response::IntoResponse,
};
use futures::{SinkExt, StreamExt};
use hashtree_core::from_hex;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::sync::mpsc;

use super::auth::{AppState, PendingRequest};

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum WsClientMessage {
    #[serde(rename = "req")]
    Request { id: u32, hash: String },
    #[serde(rename = "res")]
    Response { id: u32, hash: String, found: bool },
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
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: AppState) {
    let client_id = state.ws_relay.next_id();
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();

    {
        let mut clients = state.ws_relay.clients.lock().await;
        clients.insert(client_id, tx);
    }

    let (mut sender, mut receiver) = socket.split();
    let send_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if sender.send(msg).await.is_err() {
                break;
            }
        }
    });

    let recv_state = state.clone();
    let recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = receiver.next().await {
            handle_message(client_id, msg, &recv_state).await;
        }
    });

    tokio::select! {
        _ = send_task => {},
        _ = recv_task => {},
    }

    {
        let mut clients = state.ws_relay.clients.lock().await;
        clients.remove(&client_id);
    }
    {
        let mut pending = state.ws_relay.pending.lock().await;
        pending.retain(|(peer_id, _), _| *peer_id != client_id);
    }
}

async fn handle_message(client_id: u64, msg: Message, state: &AppState) {
    match msg {
        Message::Text(text) => {
            if let Ok(msg) = serde_json::from_str::<WsClientMessage>(&text) {
                match msg {
                    WsClientMessage::Request { id, hash } => {
                        handle_request(client_id, id, hash, state).await;
                    }
                    WsClientMessage::Response { id, hash, found } => {
                        handle_response(client_id, id, hash, found, state).await;
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

async fn handle_request(client_id: u64, request_id: u32, hash: String, state: &AppState) {
    let hash_hex = hash.to_lowercase();

    if let Ok(hash_bytes) = from_hex(&hash_hex) {
        if let Ok(Some(data)) = state.store.get_blob(&hash_bytes) {
            send_json(
                state,
                client_id,
                WsResponse { kind: "res", id: request_id, hash: hash.clone(), found: true },
            ).await;
            send_binary(state, client_id, request_id, data).await;
            return;
        }
    }

    let peers: Vec<(u64, mpsc::UnboundedSender<Message>)> = {
        let clients = state.ws_relay.clients.lock().await;
        clients
            .iter()
            .filter(|(id, _)| **id != client_id)
            .map(|(id, tx)| (*id, tx.clone()))
            .collect()
    };

    if peers.is_empty() {
        send_json(
            state,
            client_id,
            WsResponse { kind: "res", id: request_id, hash, found: false },
        ).await;
        return;
    }

    {
        let mut pending = state.ws_relay.pending.lock().await;
        for (peer_id, _) in &peers {
            pending.insert(
                (*peer_id, request_id),
                PendingRequest { origin_id: client_id, hash: hash.clone(), found: false },
            );
        }
    }

    let request_text = serde_json::to_string(&WsRequest {
        kind: "req".to_string(),
        id: request_id,
        hash: hash.clone(),
    }).unwrap_or_else(|_| String::new());
    for (_, tx) in peers {
        let _ = tx.send(Message::Text(request_text.clone()));
    }

    let timeout_state = state.clone();
    let timeout_hash = hash.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let mut pending = timeout_state.ws_relay.pending.lock().await;
        let still_pending = pending.iter().any(|((_, id), p)| *id == request_id && p.origin_id == client_id);
        let already_found = pending.iter().any(|((_, id), p)| *id == request_id && p.origin_id == client_id && p.found);
        if !still_pending || already_found {
            return;
        }
        pending.retain(|(_, id), p| !(*id == request_id && p.origin_id == client_id));
        drop(pending);
        send_json(
            &timeout_state,
            client_id,
            WsResponse { kind: "res", id: request_id, hash: timeout_hash, found: false },
        ).await;
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
            .map(|p| (p.origin_id, p.hash.clone(), p.found))
    };

    let Some((origin_id, pending_hash, already_found)) = pending_entry else {
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
        send_json(
            state,
            origin_id,
            WsResponse { kind: "res", id: request_id, hash: pending_hash, found: true },
        ).await;
        return;
    }

    let mut pending = state.ws_relay.pending.lock().await;
    pending.remove(&(client_id, request_id));
    let has_remaining = pending
        .iter()
        .any(|((_, id), p)| *id == request_id && p.origin_id == origin_id);
    drop(pending);

    if !has_remaining {
        send_json(
            state,
            origin_id,
            WsResponse { kind: "res", id: request_id, hash: pending_hash, found: false },
        ).await;
    }
}

async fn handle_binary(client_id: u64, data: Vec<u8>, state: &AppState) {
    if data.len() < 4 {
        return;
    }
    let request_id = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    let origin_id = {
        let pending = state.ws_relay.pending.lock().await;
        pending.get(&(client_id, request_id)).map(|p| p.origin_id)
    };
    let Some(origin_id) = origin_id else {
        return;
    };

    send_binary(state, origin_id, request_id, data[4..].to_vec()).await;

    let mut pending = state.ws_relay.pending.lock().await;
    pending.retain(|(_, id), p| !(*id == request_id && p.origin_id == origin_id));
}

async fn send_json(state: &AppState, client_id: u64, response: WsResponse) {
    if let Ok(text) = serde_json::to_string(&response) {
        send_to_client(state, client_id, Message::Text(text)).await;
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
