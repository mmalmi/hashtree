use super::{auth::AppState, blob_read, blossom, status_metrics};
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json},
};
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};

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

fn current_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
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

    let upstream_blossom_fetch = state.upstream_blossom_fetch_metrics.snapshot();
    let upstream = json!({
        "blossom_servers": state.upstream_blossom.len(),
        "blossom_fetch": {
            "lookup_attempts": upstream_blossom_fetch.lookup_attempts,
            "hits": upstream_blossom_fetch.hits,
            "hit_bytes": upstream_blossom_fetch.hit_bytes,
            "explicit_misses": upstream_blossom_fetch.explicit_misses,
            "indeterminate_misses": upstream_blossom_fetch.indeterminate_misses,
            "last_indeterminate_reason": upstream_blossom_fetch.last_indeterminate_reason,
            "miss_cache_hits": upstream_blossom_fetch.miss_cache_hits,
        },
        "nostr_relays": state.nostr_relay_urls.len(),
    });
    let fips = if let Some(ref endpoint) = state.fips_endpoint {
        let native_peers = endpoint.peers().await.unwrap_or_default();
        let peers = native_peers
            .iter()
            .map(|peer| peer.npub.clone())
            .collect::<Vec<_>>();
        let connected_peers = native_peers.iter().filter(|peer| peer.connected).count();
        let peer_statuses = native_peers
            .into_iter()
            .map(|peer| {
                json!({
                    "npub": peer.npub,
                    "connected": peer.connected,
                    "transport_type": peer.transport_type,
                    "transport_addr": peer.transport_addr,
                })
            })
            .collect::<Vec<_>>();
        json!({
            "enabled": true,
            "fetch_from_peers": state.fetch_from_fips_peers,
            "http_fetch": state.fetch_from_fips_peers,
            "total_peers": peers.len(),
            "connected_peers": connected_peers,
            "peers": peers,
            "peer_statuses": peer_statuses,
        })
    } else {
        json!({
            "enabled": false,
            "fetch_from_peers": state.fetch_from_fips_peers,
            "http_fetch": state.fetch_from_fips_peers,
            "total_peers": 0,
            "connected_peers": 0,
            "peers": [],
            "peer_statuses": [],
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
    let upload_replicas = blossom::blossom_upload_replica_queue_snapshot(&state);
    let queues = json!({
        "blocking_io": {
            "runtime_threads": blob_io.blocking_threads,
            "reserved_runtime_threads": blob_io.reserved_blocking_threads,
            "limit": blob_io.total_limit,
            "in_use": blob_io.total_in_use,
            "available": blob_io.total_available,
        },
        "blob_reads": {
            "limit": blob_io.read_limit,
            "in_use": blob_io.read_in_use,
            "available": blob_io.read_available,
            "metadata_limit": blob_io.metadata_read_limit,
            "metadata_in_use": blob_io.metadata_read_in_use,
            "metadata_available": blob_io.metadata_read_available,
            "queue_timeout_ms": blob_io.read_queue_timeout_ms,
            "task_timeout_ms": blob_io.read_task_timeout_ms,
        },
        "blob_writes": {
            "limit": blob_io.write_limit,
            "in_use": blob_io.write_in_use,
            "available": blob_io.write_available,
            "queue_timeout_ms": blob_io.write_queue_timeout_ms,
        },
        "optimistic_uploads": {
            "enabled": optimistic_uploads.enabled,
            "max_bytes": optimistic_uploads.max_bytes,
            "available_bytes": optimistic_uploads.available_bytes,
            "reserved_bytes": optimistic_uploads.reserved_bytes,
            "in_flight": optimistic_uploads.in_flight,
            "queue_timeout_ms": optimistic_uploads.queue_timeout_ms,
        },
        "upload_replication": {
            "enabled": upload_replicas.enabled,
            "targets": upload_replicas.target_count,
            "max_bytes": upload_replicas.max_bytes,
            "available_bytes": upload_replicas.available_bytes,
            "reserved_bytes": upload_replicas.reserved_bytes,
            "coalesce_queue_capacity_jobs": upload_replicas.coalesce_queue_capacity_jobs,
            "coalesce_queued_jobs": upload_replicas.coalesce_queued_jobs,
            "coalesce_max_blobs": upload_replicas.coalesce_max_blobs,
            "coalesce_max_bytes": upload_replicas.coalesce_max_bytes,
            "coalesce_flush_ms": upload_replicas.coalesce_flush_ms,
            "upload_concurrency": upload_replicas.upload_concurrency,
            "in_flight_batches": upload_replicas.in_flight_batches,
            "accepted_batches": upload_replicas.accepted_batches,
            "accepted_blobs": upload_replicas.accepted_blobs,
            "uploaded_blobs": upload_replicas.uploaded_blobs,
            "replicated_bytes": upload_replicas.replicated_bytes,
            "failed_batches": upload_replicas.failed_batches,
            "skipped_jobs": upload_replicas.skipped_jobs,
            "fallback_batches": upload_replicas.fallback_batches,
            "fallback_uploaded_blobs": upload_replicas.fallback_uploaded_blobs,
            "fallback_failed_blobs": upload_replicas.fallback_failed_blobs,
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
            "fetch_from_fips_peers": state.fetch_from_fips_peers,
            "http_fips_fetch": state.fetch_from_fips_peers,
            "fips": state.fips_endpoint.is_some(),
        },
        "fips": fips,
        "relay": relay,
        "upstream": upstream,
        "queues": queues,
        "http": http,
    }))
    .into_response()
}
