use super::{auth::AppState, blob_read, blossom, status_metrics};
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json},
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::webrtc::{ConnectionState, PeerEntry, PeerTransport, WebRTCState};
#[cfg(feature = "p2p")]
use hashtree_network::MeshSession;

#[derive(Default)]
struct MeshSnapshot {
    peers: Vec<Value>,
    connected: usize,
    with_data_channel: usize,
    transport_webrtc: usize,
    transport_bluetooth: usize,
    bytes_sent: u64,
    bytes_received: u64,
    mesh_received: u64,
    mesh_forwarded: u64,
    mesh_dropped_duplicate: u64,
}

impl MeshSnapshot {
    fn transport_counts(&self) -> Value {
        json!({
            "webrtc": self.transport_webrtc,
            "bluetooth": self.transport_bluetooth,
        })
    }
}

fn status_counts_json(counts: status_metrics::StatusClassCounts) -> Value {
    json!({
        "total": counts.total,
        "1xx": counts.status_1xx,
        "2xx": counts.status_2xx,
        "3xx": counts.status_3xx,
        "4xx": counts.status_4xx,
        "5xx": counts.status_5xx,
        "other": counts.other,
    })
}

fn bluetooth_transport_enabled() -> bool {
    crate::config::Config::load()
        .map(|config| config.server.enable_bluetooth && config.server.max_bluetooth_peers > 0)
        .unwrap_or(true)
}

fn current_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn peer_transport_visible(entry: &PeerEntry, bluetooth_enabled: bool) -> bool {
    bluetooth_enabled || entry.transport != PeerTransport::Bluetooth
}

fn peer_entry_json(id: &str, entry: &PeerEntry, hash_get_enabled: bool) -> Value {
    let rtc_state = peer_transport_debug_state(entry);
    let signal_paths: Vec<_> = entry
        .signal_paths
        .iter()
        .map(|path| path.to_string())
        .collect();

    json!({
        "id": id,
        "peer_id": entry.peer_id.to_string(),
        "pubkey": entry.peer_id.pubkey.clone(),
        "state": format!("{:?}", entry.state),
        "rtc_state": rtc_state,
        "pool": format!("{:?}", entry.pool),
        "transport": entry.transport.to_string(),
        "signal_paths": signal_paths,
        "connected": entry.state == ConnectionState::Connected,
        "has_data_channel": entry.peer.as_ref().map(|peer| peer.is_ready()).unwrap_or(false),
        "capabilities": {
            "hash_get": hash_get_enabled,
        },
        "bytes_sent": entry.bytes_sent,
        "bytes_received": entry.bytes_received,
    })
}

#[cfg(feature = "p2p")]
fn peer_transport_debug_state(entry: &PeerEntry) -> Option<String> {
    entry
        .peer
        .as_ref()
        .and_then(|peer| peer.transport_debug_state())
}

#[cfg(not(feature = "p2p"))]
fn peer_transport_debug_state(_entry: &PeerEntry) -> Option<String> {
    None
}

#[cfg(feature = "p2p")]
async fn peer_hash_get_snapshot(webrtc_state: &Arc<WebRTCState>) -> HashMap<String, bool> {
    webrtc_state.runtime.peer_hash_get_snapshot().await
}

#[cfg(not(feature = "p2p"))]
async fn peer_hash_get_snapshot(_webrtc_state: &Arc<WebRTCState>) -> HashMap<String, bool> {
    HashMap::new()
}

async fn capture_mesh_snapshot(webrtc_state: &Arc<WebRTCState>) -> MeshSnapshot {
    let peer_hash_get = peer_hash_get_snapshot(webrtc_state).await;
    #[cfg(feature = "p2p")]
    let peers = webrtc_state.runtime.peers.read().await;
    #[cfg(not(feature = "p2p"))]
    let peers = webrtc_state.peers.read().await;
    let bluetooth_enabled = bluetooth_transport_enabled();
    let mut snapshot = MeshSnapshot::default();

    for (id, entry) in peers.iter() {
        if !peer_transport_visible(entry, bluetooth_enabled) {
            continue;
        }

        snapshot.peers.push(peer_entry_json(
            id,
            entry,
            peer_hash_get
                .get(&entry.peer_id.to_string())
                .copied()
                .unwrap_or(true),
        ));
        if entry.state == ConnectionState::Connected {
            snapshot.connected += 1;
            if entry
                .peer
                .as_ref()
                .map(|peer| peer.is_ready())
                .unwrap_or(false)
            {
                snapshot.with_data_channel += 1;
            }
        }

        match entry.transport {
            PeerTransport::WebRtc => snapshot.transport_webrtc += 1,
            PeerTransport::Bluetooth => snapshot.transport_bluetooth += 1,
        }
    }

    (snapshot.bytes_sent, snapshot.bytes_received) = webrtc_state.get_bandwidth();
    (
        snapshot.mesh_received,
        snapshot.mesh_forwarded,
        snapshot.mesh_dropped_duplicate,
    ) = webrtc_state.get_mesh_stats();
    snapshot
}

pub(super) async fn webrtc_peers(State(state): State<AppState>) -> impl IntoResponse {
    let Some(ref webrtc_state) = state.webrtc_peers else {
        return Json(json!({
            "enabled": false,
            "transport_counts": {
                "webrtc": 0,
                "bluetooth": 0
            },
            "peers": []
        }));
    };

    let snapshot = capture_mesh_snapshot(webrtc_state).await;
    Json(json!({
        "enabled": true,
        "total": snapshot.peers.len(),
        "connected": snapshot.connected,
        "with_data_channel": snapshot.with_data_channel,
        "transport_counts": snapshot.transport_counts(),
        "bytes_sent": snapshot.bytes_sent,
        "bytes_received": snapshot.bytes_received,
        "mesh_received": snapshot.mesh_received,
        "mesh_forwarded": snapshot.mesh_forwarded,
        "mesh_dropped_duplicate": snapshot.mesh_dropped_duplicate,
        "peers": snapshot.peers,
    }))
}

pub(super) async fn daemon_status(
    State(state): State<AppState>,
    connect_info: axum::extract::ConnectInfo<std::net::SocketAddr>,
) -> impl IntoResponse {
    let ip = connect_info.0.ip();
    if !ip.is_loopback() {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "localhost only"})),
        )
            .into_response();
    }

    let bluetooth_received_events = match state.nostr_relay.as_ref() {
        Some(relay) => relay.bluetooth_received_events(100).await,
        None => Vec::new(),
    };

    let mesh = if let Some(ref webrtc_state) = state.webrtc_peers {
        let snapshot = capture_mesh_snapshot(webrtc_state).await;
        json!({
            "enabled": true,
            "total_peers": snapshot.peers.len(),
            "connected": snapshot.connected,
            "with_data_channel": snapshot.with_data_channel,
            "transport_counts": snapshot.transport_counts(),
            "bytes_sent": snapshot.bytes_sent,
            "bytes_received": snapshot.bytes_received,
            "mesh_received": snapshot.mesh_received,
            "mesh_forwarded": snapshot.mesh_forwarded,
            "mesh_dropped_duplicate": snapshot.mesh_dropped_duplicate,
            "bluetooth_received_events": bluetooth_received_events,
            "peers": snapshot.peers,
        })
    } else {
        json!({
            "enabled": false,
            "bluetooth_received_events": bluetooth_received_events,
        })
    };

    let upstream = json!({
        "blossom_servers": state.upstream_blossom.len(),
        "nostr_relays": state.nostr_relay_urls.len(),
    });
    let fips = if let Some(ref transport) = state.fips_transport {
        let peers = transport.peer_ids().await;
        json!({
            "enabled": true,
            "http_fetch": state.http_fips_fetch,
            "total_peers": peers.len(),
            "peers": peers,
        })
    } else {
        json!({
            "enabled": false,
            "http_fetch": state.http_fips_fetch,
        })
    };
    let (relay_bytes_sent, relay_bytes_received) = state.ws_relay.upstream_relay_bandwidth();
    let relay = json!({
        "enabled": !state.nostr_relay_urls.is_empty(),
        "bytes_sent": relay_bytes_sent,
        "bytes_received": relay_bytes_received,
    });
    let blob_io = blob_read::blob_io_queue_snapshot();
    let optimistic_uploads = blossom::optimistic_upload_queue_snapshot(&state);
    let queues = json!({
        "blob_reads": {
            "limit": blob_io.read_limit,
            "in_use": blob_io.read_in_use,
            "available": blob_io.read_available,
            "queue_timeout_ms": blob_io.read_queue_timeout_ms,
            "task_timeout_ms": blob_io.read_task_timeout_ms,
        },
        "blob_writes": {
            "limit": blob_io.write_limit,
            "in_use": blob_io.write_in_use,
            "available": blob_io.write_available,
        },
        "optimistic_uploads": {
            "enabled": optimistic_uploads.enabled,
            "max_bytes": optimistic_uploads.max_bytes,
            "available_bytes": optimistic_uploads.available_bytes,
            "reserved_bytes": optimistic_uploads.reserved_bytes,
            "in_flight": optimistic_uploads.in_flight,
            "queue_timeout_ms": optimistic_uploads.queue_timeout_ms,
        },
    });
    let http_status = status_metrics::http_status_snapshot();
    let http = json!({
        "status_classes": {
            "window_seconds": http_status.window_seconds,
            "recent": status_counts_json(http_status.recent),
            "total": status_counts_json(http_status.lifetime),
        }
    });

    Json(json!({
        "status": "running",
        "daemon_started_at": state.daemon_started_at,
        "uptime_seconds": current_unix_secs().saturating_sub(state.daemon_started_at),
        "mode": state.peer_mode.as_str(),
        "capabilities": {
            "hash_get": state.hash_get_enabled,
            "http_webrtc_fetch": state.http_webrtc_fetch,
            "http_fips_fetch": state.http_fips_fetch,
            "fips": state.fips_transport.is_some(),
        },
        "fips": fips,
        "mesh": mesh.clone(),
        "webrtc": mesh,
        "relay": relay,
        "upstream": upstream,
        "queues": queues,
        "http": http,
    }))
    .into_response()
}
