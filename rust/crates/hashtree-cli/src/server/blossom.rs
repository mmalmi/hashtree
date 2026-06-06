//! Blossom protocol implementation (BUD-01, BUD-02)
//!
//! Implements blob storage endpoints with Nostr-based authentication.
//! See: https://github.com/hzrd149/blossom

use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderMap, Response, StatusCode},
    response::IntoResponse,
    Json,
};
use base64::Engine;
use hashtree_core::from_hex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::auth::AppState;
use super::blob_read::{acquire_blob_read, acquire_blob_write, blob_read_timeout, BLOB_READ_BUSY};
use super::ingest_filter::{
    content_type_base, is_chk_content_type, validate_untrusted_blob, IngestRejection,
};
use super::mime::get_mime_type;

/// Blossom authorization event kind (NIP-98 style)
const BLOSSOM_AUTH_KIND: u16 = 24242;

/// Cache-Control header for immutable content-addressed data (1 year)
const IMMUTABLE_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";
const NOT_FOUND_CACHE_CONTROL: &str = "no-store";
const IMMUTABLE_NOT_FOUND_CACHE_CONTROL: &str = "public, max-age=0, s-maxage=5";
const OPTIMISTIC_UPLOAD_QUEUE_TIMEOUT_MS_ENV: &str = "HTREE_OPTIMISTIC_UPLOAD_QUEUE_TIMEOUT_MS";
const DEFAULT_OPTIMISTIC_UPLOAD_QUEUE_TIMEOUT_MS: u64 = 15_000;

/// Default maximum upload size in bytes (5 MB)
pub const DEFAULT_MAX_UPLOAD_SIZE: usize = 5 * 1024 * 1024;
const OPTIMISTIC_UPLOAD_MIN_QUEUE_CHARGE_BYTES: usize = 256 * 1024;
const MAX_BATCH_UPLOAD_BLOBS: usize = 1024;
const MAX_BATCH_UPLOAD_BYTES: usize = 64 * 1024 * 1024;
const SLOW_BATCH_UPLOAD_LOG_MS_ENV: &str = "HTREE_SLOW_BATCH_UPLOAD_LOG_MS";

fn slow_batch_upload_log_ms() -> Option<u128> {
    std::env::var(SLOW_BATCH_UPLOAD_LOG_MS_ENV)
        .ok()
        .and_then(|value| value.parse::<u128>().ok())
        .filter(|value| *value > 0)
}

#[derive(Debug, Clone, Copy)]
pub(super) struct OptimisticUploadQueueSnapshot {
    pub enabled: bool,
    pub max_bytes: usize,
    pub available_bytes: usize,
    pub reserved_bytes: usize,
    pub in_flight: usize,
    pub queue_timeout_ms: u64,
}

/// Check if a pubkey has write access based on allowed_npubs config or social graph
/// Returns Ok(()) if allowed, Err with JSON error body if denied
#[allow(clippy::result_large_err)]
fn check_write_access(state: &AppState, pubkey: &str) -> Result<(), Response<Body>> {
    // Check if pubkey is in the allowed list (converted from npub to hex)
    if is_allowed_write_author(state, pubkey) {
        tracing::debug!(
            "Blossom write allowed for {}... (allowed writer)",
            &pubkey[..8.min(pubkey.len())]
        );
        return Ok(());
    }

    // Not in allowed list or social graph
    tracing::info!(
        "Blossom write denied for {}... (not in allowed_npubs or social graph)",
        &pubkey[..8.min(pubkey.len())]
    );
    Err(Response::builder()
        .status(StatusCode::FORBIDDEN)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            r#"{"error":"Write access denied. Your pubkey is not in the allowed list."}"#,
        ))
        .unwrap())
}

fn is_allowed_write_author(state: &AppState, pubkey: &str) -> bool {
    if state.allowed_pubkeys.contains(pubkey) {
        return true;
    }

    state
        .social_graph
        .as_ref()
        .map(|sg| sg.check_write_access(pubkey))
        .unwrap_or(false)
}

fn can_accept_upload_author(state: &AppState, pubkey: &str) -> bool {
    state.public_writes || is_allowed_write_author(state, pubkey)
}

fn validate_upload_payload(
    body: &[u8],
    content_type: &str,
    can_upload_author: bool,
    require_random_untrusted_ingest: bool,
) -> Result<(), (StatusCode, String)> {
    let is_chk_upload = is_chk_content_type(content_type);

    if !is_chk_upload && !can_upload_author {
        return Err((
            StatusCode::FORBIDDEN,
            "Raw media uploads require write access".to_string(),
        ));
    }

    if is_chk_upload {
        let require_random = require_random_untrusted_ingest && !can_upload_author;
        validate_untrusted_blob(body, require_random)
            .map_err(|IngestRejection { status, reason }| (status, reason))?;
    }

    Ok(())
}

fn blossom_json_error(status: StatusCode, reason: impl Into<String>) -> Response<Body> {
    let reason = reason.into();
    Response::builder()
        .status(status)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header("X-Reason", reason.as_str())
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(format!(r#"{{"error":"{}"}}"#, reason)))
        .unwrap()
}

fn blossom_retryable_json_error(
    status: StatusCode,
    reason: impl Into<String>,
    retry_after_seconds: u64,
) -> Response<Body> {
    let reason = reason.into();
    Response::builder()
        .status(status)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header("X-Reason", reason.as_str())
        .header(header::RETRY_AFTER, retry_after_seconds.to_string())
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(format!(r#"{{"error":"{}"}}"#, reason)))
        .unwrap()
}

/// Blob descriptor returned by upload and list endpoints
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlobDescriptor {
    pub url: String,
    pub sha256: String,
    pub size: u64,
    #[serde(rename = "type")]
    pub mime_type: String,
    pub uploaded: u64,
}

#[derive(Debug, Deserialize)]
pub struct BatchUploadBlob {
    pub sha256: String,
    #[serde(default, alias = "contentType")]
    pub content_type: Option<String>,
    pub data: String,
}

#[derive(Debug, Deserialize)]
pub struct BatchUploadRequest {
    pub blobs: Vec<BatchUploadBlob>,
}

#[derive(Debug, Serialize)]
pub struct BatchUploadResponse {
    pub uploaded: usize,
    pub blobs: Vec<BlobDescriptor>,
}

/// Query parameters for list endpoint
#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub since: Option<u64>,
    pub until: Option<u64>,
    pub limit: Option<usize>,
    pub cursor: Option<String>,
}

/// Parsed Nostr authorization event
#[derive(Debug)]
pub struct BlossomAuth {
    pub pubkey: String,
    pub kind: u16,
    pub created_at: u64,
    pub expiration: Option<u64>,
    pub action: Option<String>,   // "upload", "delete", "list", "get"
    pub blob_hashes: Vec<String>, // x tags
    pub server: Option<String>,   // server tag
}

/// Parse and verify Nostr authorization from header
/// Returns the verified auth or an error response
pub fn verify_blossom_auth(
    headers: &HeaderMap,
    required_action: &str,
    required_hash: Option<&str>,
) -> Result<BlossomAuth, (StatusCode, &'static str)> {
    let auth_header = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or((StatusCode::UNAUTHORIZED, "Missing Authorization header"))?;

    let nostr_event = auth_header.strip_prefix("Nostr ").ok_or((
        StatusCode::UNAUTHORIZED,
        "Invalid auth scheme, expected 'Nostr'",
    ))?;

    // Decode base64 event
    let engine = base64::engine::general_purpose::STANDARD;
    let event_bytes = engine
        .decode(nostr_event)
        .map_err(|_| (StatusCode::BAD_REQUEST, "Invalid base64 in auth header"))?;

    let event_json: serde_json::Value = serde_json::from_slice(&event_bytes)
        .map_err(|_| (StatusCode::BAD_REQUEST, "Invalid JSON in auth event"))?;

    // Extract event fields
    let kind = event_json["kind"]
        .as_u64()
        .ok_or((StatusCode::BAD_REQUEST, "Missing kind in event"))?;

    if kind != BLOSSOM_AUTH_KIND as u64 {
        return Err((
            StatusCode::BAD_REQUEST,
            "Invalid event kind, expected 24242",
        ));
    }

    let pubkey = event_json["pubkey"]
        .as_str()
        .ok_or((StatusCode::BAD_REQUEST, "Missing pubkey in event"))?
        .to_string();

    let created_at = event_json["created_at"]
        .as_u64()
        .ok_or((StatusCode::BAD_REQUEST, "Missing created_at in event"))?;

    let sig = event_json["sig"]
        .as_str()
        .ok_or((StatusCode::BAD_REQUEST, "Missing signature in event"))?;

    // Verify signature
    if !verify_nostr_signature(&event_json, &pubkey, sig) {
        return Err((StatusCode::UNAUTHORIZED, "Invalid signature"));
    }

    // Parse tags
    let tags = event_json["tags"]
        .as_array()
        .ok_or((StatusCode::BAD_REQUEST, "Missing tags in event"))?;

    let mut expiration: Option<u64> = None;
    let mut action: Option<String> = None;
    let mut blob_hashes: Vec<String> = Vec::new();
    let mut server: Option<String> = None;

    for tag in tags {
        let tag_arr = tag.as_array();
        if let Some(arr) = tag_arr {
            if arr.len() >= 2 {
                let tag_name = arr[0].as_str().unwrap_or("");
                let tag_value = arr[1].as_str().unwrap_or("");

                match tag_name {
                    "t" => action = Some(tag_value.to_string()),
                    "x" => blob_hashes.push(tag_value.to_lowercase()),
                    "expiration" => expiration = tag_value.parse().ok(),
                    "server" => server = Some(tag_value.to_string()),
                    _ => {}
                }
            }
        }
    }

    // Validate expiration
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    if let Some(exp) = expiration {
        if exp < now {
            return Err((StatusCode::UNAUTHORIZED, "Authorization expired"));
        }
    }

    // Validate created_at is not in the future (with 60s tolerance)
    if created_at > now + 60 {
        return Err((StatusCode::BAD_REQUEST, "Event created_at is in the future"));
    }

    // Validate action matches
    if let Some(ref act) = action {
        if act != required_action {
            return Err((StatusCode::FORBIDDEN, "Action mismatch"));
        }
    } else {
        return Err((StatusCode::BAD_REQUEST, "Missing 't' tag for action"));
    }

    // Validate hash if required
    if let Some(hash) = required_hash {
        if !blob_hashes.is_empty() && !blob_hashes.contains(&hash.to_lowercase()) {
            return Err((StatusCode::FORBIDDEN, "Blob hash not authorized"));
        }
    }

    Ok(BlossomAuth {
        pubkey,
        kind: kind as u16,
        created_at,
        expiration,
        action,
        blob_hashes,
        server,
    })
}

/// Verify Nostr event signature using secp256k1
fn verify_nostr_signature(event: &serde_json::Value, pubkey: &str, sig: &str) -> bool {
    use secp256k1::{schnorr::Signature, Message, Secp256k1, XOnlyPublicKey};

    // Compute event ID (sha256 of serialized event)
    let content = event["content"].as_str().unwrap_or("");
    let full_serialized = format!(
        "[0,\"{}\",{},{},{},\"{}\"]",
        pubkey,
        event["created_at"],
        event["kind"],
        event["tags"],
        escape_json_string(content),
    );

    let mut hasher = Sha256::new();
    hasher.update(full_serialized.as_bytes());
    let event_id = hasher.finalize();

    // Parse pubkey and signature
    let pubkey_bytes = match hex::decode(pubkey) {
        Ok(b) => b,
        Err(_) => return false,
    };

    let sig_bytes = match hex::decode(sig) {
        Ok(b) => b,
        Err(_) => return false,
    };

    let secp = Secp256k1::verification_only();

    let xonly_pubkey = match XOnlyPublicKey::from_slice(&pubkey_bytes) {
        Ok(pk) => pk,
        Err(_) => return false,
    };

    let signature = match Signature::from_slice(&sig_bytes) {
        Ok(s) => s,
        Err(_) => return false,
    };

    let message = match Message::from_digest_slice(&event_id) {
        Ok(m) => m,
        Err(_) => return false,
    };

    secp.verify_schnorr(&signature, &message, &xonly_pubkey)
        .is_ok()
}

/// Escape string for JSON serialization
fn escape_json_string(s: &str) -> String {
    let mut result = String::new();
    for c in s.chars() {
        match c {
            '"' => result.push_str("\\\""),
            '\\' => result.push_str("\\\\"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            c if c.is_control() => {
                result.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => result.push(c),
        }
    }
    result
}

/// CORS preflight handler for all Blossom endpoints
/// Echoes back Access-Control-Request-Headers to allow any headers
pub async fn cors_preflight(headers: HeaderMap) -> impl IntoResponse {
    // Echo back requested headers, or use sensible defaults that cover common Blossom headers
    let allowed_headers = headers
        .get(header::ACCESS_CONTROL_REQUEST_HEADERS)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("Authorization, Content-Type, X-SHA-256, x-sha-256");

    // Always include common headers in addition to what was requested
    let full_allowed = format!(
        "{}, Authorization, Content-Type, X-SHA-256, x-sha-256, Accept, Cache-Control",
        allowed_headers
    );

    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            "GET, HEAD, PUT, DELETE, OPTIONS",
        )
        .header(header::ACCESS_CONTROL_ALLOW_HEADERS, full_allowed)
        .header(header::ACCESS_CONTROL_MAX_AGE, "86400")
        .body(Body::empty())
        .unwrap()
}

/// HEAD /<sha256> - Check if blob exists
pub async fn head_blob(
    State(state): State<AppState>,
    Path(id): Path<String>,
    connect_info: axum::extract::ConnectInfo<std::net::SocketAddr>,
) -> impl IntoResponse {
    let is_localhost = connect_info.0.ip().is_loopback();
    let (hash_part, ext) = parse_hash_and_extension(&id);

    if !is_valid_sha256(hash_part) {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header("X-Reason", "Invalid SHA256 hash")
            .body(Body::empty())
            .unwrap();
    }

    let sha256_hex = hash_part.to_lowercase();
    let sha256_bytes: [u8; 32] = match from_hex(&sha256_hex) {
        Ok(b) => b,
        Err(_) => {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .header("X-Reason", "Invalid SHA256 format")
                .body(Body::empty())
                .unwrap()
        }
    };

    // Blossom HEAD only needs metadata; avoid reading the full blob body just to
    // answer cache probes and CDN revalidation. The read permit keeps CDN probe
    // storms from filling Tokio's blocking thread pool while old blobs without
    // metadata are still being normalized.
    let blob_size = if let Some(cached) = state.blob_cache.get_size(&sha256_hex) {
        Ok(Ok(cached))
    } else {
        let permit = match acquire_blob_read().await {
            Ok(permit) => permit,
            Err(_) => {
                return Response::builder()
                    .status(StatusCode::SERVICE_UNAVAILABLE)
                    .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                    .header(header::CACHE_CONTROL, NOT_FOUND_CACHE_CONTROL)
                    .header("Retry-After", "1")
                    .header("X-Reason", BLOB_READ_BUSY)
                    .body(Body::empty())
                    .unwrap()
            }
        };
        let store = state.store.clone();
        let size_read = tokio::task::spawn_blocking(move || store.blob_size(&sha256_bytes));
        let timed = tokio::time::timeout(blob_read_timeout(), size_read).await;
        drop(permit);
        let result = match timed {
            Ok(result) => result.map_err(|_| ()),
            Err(_) => Err(()),
        };
        if let Ok(Ok(size)) = result {
            state.blob_cache.put_size(sha256_hex.clone(), size);
        }
        result
    };

    match blob_size {
        Ok(Ok(Some(size))) => {
            let mime_type = ext
                .map(|e| get_mime_type(&format!("file{}", e)))
                .unwrap_or("application/octet-stream");

            let mut builder = Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime_type)
                .header(header::CONTENT_LENGTH, size)
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::CACHE_CONTROL, IMMUTABLE_CACHE_CONTROL)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*");
            if is_localhost {
                builder = builder.header("X-Source", "local");
            }
            builder.body(Body::empty()).unwrap()
        }
        Ok(Ok(None)) => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header(header::CACHE_CONTROL, IMMUTABLE_NOT_FOUND_CACHE_CONTROL)
            .header("X-Reason", "Blob not found")
            .body(Body::empty())
            .unwrap(),
        _ => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .body(Body::empty())
            .unwrap(),
    }
}

async fn store_blossom_blob_without_blocking_runtime(
    state: &AppState,
    data: axum::body::Bytes,
    pubkey: [u8; 32],
    track_ownership: bool,
) -> anyhow::Result<()> {
    let mut hasher = Sha256::new();
    hasher.update(&data);
    let hash_hex = hex::encode(hasher.finalize());
    let data_for_cache = data.clone();
    let permit = acquire_blob_write()
        .await
        .map_err(|err| anyhow::anyhow!(err))?;
    let store = state.store.clone();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        if track_ownership {
            store.put_owned_blob(&data, &pubkey)?;
        } else {
            store.put_cached_blob(&data)?;
        }
        Ok::<(), anyhow::Error>(())
    })
    .await
    .map_err(|err| anyhow::anyhow!("blob write task failed: {}", err))??;
    state
        .blob_cache
        .put_size(hash_hex.clone(), Some(data_for_cache.len() as u64));
    state.blob_cache.put_body(hash_hex, &data_for_cache);
    Ok(())
}

fn upload_descriptor_response(status: StatusCode, descriptor: &BlobDescriptor) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_string(descriptor).unwrap()))
        .unwrap()
}

fn optimistic_upload_queue_timeout() -> Duration {
    let millis = std::env::var(OPTIMISTIC_UPLOAD_QUEUE_TIMEOUT_MS_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_OPTIMISTIC_UPLOAD_QUEUE_TIMEOUT_MS);
    Duration::from_millis(millis)
}

fn optimistic_upload_inflight() -> &'static Mutex<HashSet<String>> {
    static INFLIGHT: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    INFLIGHT.get_or_init(|| Mutex::new(HashSet::new()))
}

fn optimistic_upload_is_inflight(hash_hex: &str) -> bool {
    optimistic_upload_inflight()
        .lock()
        .is_ok_and(|inflight| inflight.contains(hash_hex))
}

fn mark_optimistic_upload_inflight(hash_hex: &str) -> bool {
    optimistic_upload_inflight()
        .lock()
        .map(|mut inflight| inflight.insert(hash_hex.to_string()))
        .unwrap_or(true)
}

fn clear_optimistic_upload_inflight(hash_hex: &str) {
    if let Ok(mut inflight) = optimistic_upload_inflight().lock() {
        inflight.remove(hash_hex);
    }
}

pub(super) fn optimistic_upload_queue_snapshot(state: &AppState) -> OptimisticUploadQueueSnapshot {
    let max_bytes = state.optimistic_upload_queue_bytes;
    let available_bytes = state
        .optimistic_upload_queue
        .available_permits()
        .min(max_bytes);
    let in_flight = optimistic_upload_inflight()
        .lock()
        .map(|inflight| inflight.len())
        .unwrap_or(0);

    OptimisticUploadQueueSnapshot {
        enabled: state.optimistic_blossom_uploads,
        max_bytes,
        available_bytes,
        reserved_bytes: max_bytes.saturating_sub(available_bytes),
        in_flight,
        queue_timeout_ms: duration_millis_u64(optimistic_upload_queue_timeout()),
    }
}

fn duration_millis_u64(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

async fn acquire_optimistic_upload_queue(
    state: &AppState,
    permits: u32,
) -> Result<tokio::sync::OwnedSemaphorePermit, &'static str> {
    match tokio::time::timeout(
        optimistic_upload_queue_timeout(),
        state
            .optimistic_upload_queue
            .clone()
            .acquire_many_owned(permits),
    )
    .await
    {
        Ok(Ok(permit)) => Ok(permit),
        Ok(Err(_)) => Err("Optimistic upload queue is closed"),
        Err(_) => Err("Optimistic upload queue is full"),
    }
}

async fn uploaded_blob_already_exists(
    state: &AppState,
    sha256_hash: [u8; 32],
    sha256_hex: &str,
) -> Result<bool, String> {
    if let Some(Some(_)) = state.blob_cache.get_size(sha256_hex) {
        return Ok(true);
    }

    let permit = acquire_blob_read().await.map_err(str::to_string)?;
    let store = state.store.clone();
    let size_read = tokio::task::spawn_blocking(move || {
        store
            .blob_size(&sha256_hash)
            .map_err(|error| error.to_string())
    });

    let result = tokio::time::timeout(blob_read_timeout(), size_read).await;
    drop(permit);
    match result {
        Ok(Ok(Ok(size))) => {
            state.blob_cache.put_size(sha256_hex.to_string(), size);
            Ok(size.is_some())
        }
        Ok(Ok(Err(error))) => Err(error),
        Ok(Err(error)) => Err(format!("blob existence task failed: {}", error)),
        Err(_) => Err("blob existence check timed out".to_string()),
    }
}

async fn set_existing_blob_owner_without_body_write(
    state: AppState,
    sha256_hash: [u8; 32],
    pubkey: [u8; 32],
) -> anyhow::Result<()> {
    tokio::task::spawn_blocking(move || state.store.set_blob_owner(&sha256_hash, &pubkey))
        .await
        .map_err(|error| anyhow::anyhow!("blob owner task failed: {}", error))??;
    Ok(())
}

/// PUT /upload - Upload a new blob (BUD-02)
pub async fn upload_blob(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    // Check size limit first (before auth to save resources)
    let max_size = state.max_upload_bytes;
    if body.len() > max_size {
        return Response::builder()
            .status(StatusCode::PAYLOAD_TOO_LARGE)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(format!(
                r#"{{"error":"Upload size {} bytes exceeds maximum {} bytes ({} MB)"}}"#,
                body.len(),
                max_size,
                max_size / 1024 / 1024
            )))
            .unwrap();
    }

    // Verify authorization
    let auth = match verify_blossom_auth(&headers, "upload", None) {
        Ok(a) => a,
        Err((status, reason)) => {
            return Response::builder()
                .status(status)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .header("X-Reason", reason)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(r#"{{"error":"{}"}}"#, reason)))
                .unwrap();
        }
    };

    // Get content type from header
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();

    // Check write access: either in allowed_npubs/social graph OR public_writes is enabled.
    let is_allowed_author = is_allowed_write_author(&state, &auth.pubkey);
    let can_upload = can_accept_upload_author(&state, &auth.pubkey);
    if !can_upload {
        let _ = check_write_access(&state, &auth.pubkey);
        return Response::builder()
            .status(StatusCode::FORBIDDEN)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"error":"Write access denied. Your pubkey is not in the allowed list and public writes are disabled."}"#))
            .unwrap();
    }

    if let Err((status, reason)) = validate_upload_payload(
        &body,
        &content_type,
        can_upload,
        state.require_random_untrusted_ingest,
    ) {
        return blossom_json_error(status, reason);
    }

    // Compute SHA256 of uploaded data
    let mut hasher = Sha256::new();
    hasher.update(&body);
    let sha256_hash: [u8; 32] = hasher.finalize().into();
    let sha256_hex = hex::encode(sha256_hash);

    // If auth has x tags, verify hash matches
    if !auth.blob_hashes.is_empty() && !auth.blob_hashes.contains(&sha256_hex) {
        return Response::builder()
            .status(StatusCode::FORBIDDEN)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header(
                "X-Reason",
                "Uploaded blob hash does not match authorized hash",
            )
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"error":"Hash mismatch"}"#))
            .unwrap();
    }

    // Convert pubkey hex to bytes
    let pubkey_bytes = match from_hex(&auth.pubkey) {
        Ok(b) => b,
        Err(_) => {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .header("X-Reason", "Invalid pubkey format")
                .body(Body::empty())
                .unwrap();
        }
    };

    let size = body.len() as u64;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let ext = mime_to_extension(&content_type_base(&content_type));
    let descriptor = BlobDescriptor {
        url: format!("/{}{}", sha256_hex, ext),
        sha256: sha256_hex.clone(),
        size,
        mime_type: content_type,
        uploaded: now,
    };

    if state.optimistic_blossom_uploads && optimistic_upload_is_inflight(&sha256_hex) {
        return upload_descriptor_response(StatusCode::ACCEPTED, &descriptor);
    }

    // Store public-write blobs in cache storage unless the writer is explicitly
    // allowed, so untrusted public uploads do not become protected owned data.
    if state.optimistic_blossom_uploads {
        let queued_bytes = body.len().max(OPTIMISTIC_UPLOAD_MIN_QUEUE_CHARGE_BYTES);
        if queued_bytes <= state.optimistic_upload_queue_bytes {
            let permits = queued_bytes as u32;
            let marked_inflight = mark_optimistic_upload_inflight(&sha256_hex);
            if !marked_inflight {
                return upload_descriptor_response(StatusCode::ACCEPTED, &descriptor);
            }
            let permit = match acquire_optimistic_upload_queue(&state, permits).await {
                Ok(permit) => permit,
                Err(_) => {
                    clear_optimistic_upload_inflight(&sha256_hex);
                    return blossom_retryable_json_error(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "Optimistic upload queue is full",
                        2,
                    );
                }
            };
            let state_for_write = state.clone();
            let hash_for_log = sha256_hex.clone();
            tokio::spawn(async move {
                let _permit = permit;
                if let Err(error) = store_blossom_blob_without_blocking_runtime(
                    &state_for_write,
                    body,
                    pubkey_bytes,
                    is_allowed_author,
                )
                .await
                {
                    tracing::error!(
                        "Background Blossom storage failed for {}: {:#}",
                        hash_for_log,
                        error
                    );
                }
                clear_optimistic_upload_inflight(&hash_for_log);
            });

            return upload_descriptor_response(StatusCode::ACCEPTED, &descriptor);
        }

        tracing::warn!(
            "Blossom upload {} is larger than optimistic queue budget {}; storing synchronously",
            queued_bytes,
            state.optimistic_upload_queue_bytes
        );
    }

    match uploaded_blob_already_exists(&state, sha256_hash, &sha256_hex).await {
        Ok(true) => {
            if is_allowed_author {
                if let Err(error) = set_existing_blob_owner_without_body_write(
                    state.clone(),
                    sha256_hash,
                    pubkey_bytes,
                )
                .await
                {
                    return Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                        .header("X-Reason", "Storage error")
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(format!(r#"{{"error":"{}"}}"#, error)))
                        .unwrap();
                }
            }
            return upload_descriptor_response(StatusCode::CONFLICT, &descriptor);
        }
        Ok(false) => {}
        Err(error) => {
            tracing::debug!(
                "Could not preflight Blossom upload {} before synchronous storage: {}",
                sha256_hex,
                error
            );
        }
    }

    let store_result = store_blossom_blob_without_blocking_runtime(
        &state,
        body.clone(),
        pubkey_bytes,
        is_allowed_author,
    )
    .await;

    match store_result {
        Ok(()) => upload_descriptor_response(StatusCode::OK, &descriptor),
        Err(e) => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header("X-Reason", "Storage error")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(format!(r#"{{"error":"{}"}}"#, e)))
            .unwrap(),
    }
}

/// POST /upload/batch - Upload multiple blobs with one auth event and one storage batch.
pub async fn upload_blob_batch(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<BatchUploadRequest>,
) -> impl IntoResponse {
    let started_at = Instant::now();
    let slow_log_ms = slow_batch_upload_log_ms();
    let payload_blobs = payload.blobs.len();
    if payload.blobs.is_empty() {
        return blossom_json_error(StatusCode::BAD_REQUEST, "Batch is empty");
    }
    if payload.blobs.len() > MAX_BATCH_UPLOAD_BLOBS {
        return blossom_json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Batch contains too many blobs",
        );
    }

    let auth = match verify_blossom_auth(&headers, "upload", None) {
        Ok(a) => a,
        Err((status, reason)) => {
            return Response::builder()
                .status(status)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .header("X-Reason", reason)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(r#"{{"error":"{}"}}"#, reason)))
                .unwrap();
        }
    };
    let auth_ms = started_at.elapsed().as_millis();

    let is_allowed_author = is_allowed_write_author(&state, &auth.pubkey);
    let can_upload = can_accept_upload_author(&state, &auth.pubkey);
    if !can_upload {
        let _ = check_write_access(&state, &auth.pubkey);
        return blossom_json_error(StatusCode::FORBIDDEN, "Write access denied");
    }

    let pubkey_bytes = match from_hex(&auth.pubkey) {
        Ok(bytes) => bytes,
        Err(_) => return blossom_json_error(StatusCode::BAD_REQUEST, "Invalid pubkey format"),
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut total_bytes = 0usize;
    let mut items = Vec::with_capacity(payload.blobs.len());
    let mut descriptors = Vec::with_capacity(payload.blobs.len());
    let mut decode_hash_ms = 0u128;
    let mut validate_ms = 0u128;

    for blob in payload.blobs {
        let decode_started = Instant::now();
        let sha256_hex = blob.sha256.to_lowercase();
        let expected_hash: [u8; 32] = match from_hex(&sha256_hex) {
            Ok(hash) => hash,
            Err(_) => return blossom_json_error(StatusCode::BAD_REQUEST, "Invalid blob hash"),
        };
        if !auth.blob_hashes.is_empty() && !auth.blob_hashes.contains(&sha256_hex) {
            return blossom_json_error(
                StatusCode::FORBIDDEN,
                "Uploaded blob hash does not match authorized hash",
            );
        }

        let data = match base64::engine::general_purpose::STANDARD.decode(blob.data.as_bytes()) {
            Ok(data) => data,
            Err(_) => return blossom_json_error(StatusCode::BAD_REQUEST, "Invalid blob data"),
        };
        if data.len() > state.max_upload_bytes {
            return blossom_json_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Blob exceeds maximum upload size",
            );
        }
        total_bytes = total_bytes.saturating_add(data.len());
        if total_bytes > MAX_BATCH_UPLOAD_BYTES {
            return blossom_json_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Batch exceeds maximum upload size",
            );
        }

        let mut hasher = Sha256::new();
        hasher.update(&data);
        let actual_hash: [u8; 32] = hasher.finalize().into();
        if actual_hash != expected_hash {
            return blossom_json_error(StatusCode::FORBIDDEN, "Hash mismatch");
        }
        decode_hash_ms += decode_started.elapsed().as_millis();

        let validate_started = Instant::now();
        let content_type = blob
            .content_type
            .as_deref()
            .map(content_type_base)
            .unwrap_or_else(|| "application/octet-stream".to_string());
        if let Err((status, reason)) = validate_upload_payload(
            &data,
            &content_type,
            can_upload,
            state.require_random_untrusted_ingest,
        ) {
            return blossom_json_error(status, reason);
        }
        validate_ms += validate_started.elapsed().as_millis();

        let ext = mime_to_extension(&content_type);
        descriptors.push(BlobDescriptor {
            url: format!("/{}{}", sha256_hex, ext),
            sha256: sha256_hex,
            size: data.len() as u64,
            mime_type: content_type,
            uploaded: now,
        });
        items.push((actual_hash, data));
    }
    let prepare_ms = started_at.elapsed().as_millis();

    let store = state.store.clone();
    let store_started = Instant::now();
    let stored = tokio::task::spawn_blocking(move || {
        if is_allowed_author {
            store.put_owned_blobs(&items, &pubkey_bytes)
        } else {
            store.put_cached_blobs(&items)
        }
    })
    .await
    .map_err(|error| anyhow::anyhow!("blob batch write task failed: {}", error));
    let store_ms = store_started.elapsed().as_millis();
    let total_ms = started_at.elapsed().as_millis();

    match stored {
        Ok(Ok(uploaded)) => {
            if slow_log_ms.is_some_and(|threshold| total_ms >= threshold) {
                tracing::warn!(
                    blobs = payload_blobs,
                    uploaded,
                    total_bytes,
                    total_ms,
                    auth_ms,
                    prepare_ms,
                    decode_hash_ms,
                    validate_ms,
                    store_ms,
                    allowed_author = is_allowed_author,
                    "slow Blossom batch upload"
                );
            }
            Response::builder()
                .status(StatusCode::OK)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_string(&BatchUploadResponse {
                        uploaded,
                        blobs: descriptors,
                    })
                    .unwrap(),
                ))
                .unwrap()
        }
        Ok(Err(error)) | Err(error) => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header("X-Reason", "Storage error")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(format!(r#"{{"error":"{}"}}"#, error)))
            .unwrap(),
    }
}

/// DELETE /<sha256> - Delete a blob (BUD-02)
/// Note: Blob is only fully deleted when ALL owners have removed it
pub async fn delete_blob(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let (hash_part, _) = parse_hash_and_extension(&id);

    if !is_valid_sha256(hash_part) {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header("X-Reason", "Invalid SHA256 hash")
            .body(Body::empty())
            .unwrap();
    }

    let sha256_hex = hash_part.to_lowercase();

    // Convert hash to bytes
    let sha256_bytes = match from_hex(&sha256_hex) {
        Ok(b) => b,
        Err(_) => {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .header("X-Reason", "Invalid SHA256 hash format")
                .body(Body::empty())
                .unwrap();
        }
    };

    // Verify authorization with hash requirement
    let auth = match verify_blossom_auth(&headers, "delete", Some(&sha256_hex)) {
        Ok(a) => a,
        Err((status, reason)) => {
            return Response::builder()
                .status(status)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .header("X-Reason", reason)
                .body(Body::empty())
                .unwrap();
        }
    };

    // Convert pubkey hex to bytes
    let pubkey_bytes = match from_hex(&auth.pubkey) {
        Ok(b) => b,
        Err(_) => {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .header("X-Reason", "Invalid pubkey format")
                .body(Body::empty())
                .unwrap();
        }
    };

    // Check ownership - user must be one of the owners (O(1) lookup with composite key)
    match state.store.is_blob_owner(&sha256_bytes, &pubkey_bytes) {
        Ok(true) => {
            // User is an owner, proceed with delete
        }
        Ok(false) => {
            // Check if blob exists at all (for proper error message)
            match state.store.blob_has_owners(&sha256_bytes) {
                Ok(true) => {
                    return Response::builder()
                        .status(StatusCode::FORBIDDEN)
                        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                        .header("X-Reason", "Not a blob owner")
                        .body(Body::empty())
                        .unwrap();
                }
                Ok(false) => {
                    return Response::builder()
                        .status(StatusCode::NOT_FOUND)
                        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                        .header(header::CACHE_CONTROL, NOT_FOUND_CACHE_CONTROL)
                        .header("X-Reason", "Blob not found")
                        .body(Body::empty())
                        .unwrap();
                }
                Err(_) => {
                    return Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                        .body(Body::empty())
                        .unwrap();
                }
            }
        }
        Err(_) => {
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::empty())
                .unwrap();
        }
    }

    // Remove this user's ownership (blob only deleted when no owners remain)
    match state
        .store
        .delete_blossom_blob(&sha256_bytes, &pubkey_bytes)
    {
        Ok(fully_deleted) => {
            // Return 200 OK whether blob was fully deleted or just removed from user's list
            // The client doesn't need to know if other owners still exist
            Response::builder()
                .status(StatusCode::OK)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .header(
                    "X-Blob-Deleted",
                    if fully_deleted { "true" } else { "false" },
                )
                .body(Body::empty())
                .unwrap()
        }
        Err(_) => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .body(Body::empty())
            .unwrap(),
    }
}

/// GET /list/<pubkey> - List blobs for a pubkey (BUD-02)
pub async fn list_blobs(
    State(state): State<AppState>,
    Path(pubkey): Path<String>,
    Query(query): Query<ListQuery>,
    headers: HeaderMap,
) -> impl IntoResponse {
    // Validate pubkey format (64 hex chars)
    if pubkey.len() != 64 || !pubkey.chars().all(|c| c.is_ascii_hexdigit()) {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header("X-Reason", "Invalid pubkey format")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("[]"))
            .unwrap();
    }

    let pubkey_hex = pubkey.to_lowercase();
    let pubkey_bytes: [u8; 32] = match from_hex(&pubkey_hex) {
        Ok(b) => b,
        Err(_) => {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .header("X-Reason", "Invalid pubkey format")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("[]"))
                .unwrap()
        }
    };

    let auth = match verify_blossom_auth(&headers, "list", None) {
        Ok(auth) => auth,
        Err((status, reason)) => {
            return Response::builder()
                .status(status)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .header("X-Reason", reason)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("[]"))
                .unwrap();
        }
    };

    if !auth.pubkey.eq_ignore_ascii_case(&pubkey_hex) {
        return Response::builder()
            .status(StatusCode::FORBIDDEN)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header("X-Reason", "Pubkey mismatch")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("[]"))
            .unwrap();
    }

    // Get blobs for this pubkey
    match state.store.list_blobs_by_pubkey(&pubkey_bytes) {
        Ok(blobs) => {
            // Apply filters
            let mut filtered: Vec<_> = blobs
                .into_iter()
                .filter(|b| {
                    if let Some(since) = query.since {
                        if b.uploaded < since {
                            return false;
                        }
                    }
                    if let Some(until) = query.until {
                        if b.uploaded > until {
                            return false;
                        }
                    }
                    true
                })
                .collect();

            // Sort by uploaded descending (most recent first)
            filtered.sort_by(|a, b| b.uploaded.cmp(&a.uploaded));

            // Apply limit
            let limit = query.limit.unwrap_or(100).min(1000);
            filtered.truncate(limit);

            Response::builder()
                .status(StatusCode::OK)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_string(&filtered).unwrap()))
                .unwrap()
        }
        Err(_) => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("[]"))
            .unwrap(),
    }
}

// Helper functions

fn parse_hash_and_extension(id: &str) -> (&str, Option<&str>) {
    if let Some(dot_pos) = id.rfind('.') {
        (&id[..dot_pos], Some(&id[dot_pos..]))
    } else {
        (id, None)
    }
}

fn is_valid_sha256(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
fn store_blossom_blob(
    state: &AppState,
    data: &[u8],
    _sha256: &[u8; 32],
    pubkey: &[u8; 32],
    track_ownership: bool,
) -> anyhow::Result<()> {
    if track_ownership {
        state.store.put_owned_blob(data, pubkey)?;
    } else {
        state.store.put_cached_blob(data)?;
    }

    Ok(())
}

fn mime_to_extension(mime: &str) -> &'static str {
    match mime {
        "image/png" => ".png",
        "image/jpeg" => ".jpg",
        "image/gif" => ".gif",
        "image/webp" => ".webp",
        "image/svg+xml" => ".svg",
        "video/mp4" => ".mp4",
        "video/webm" => ".webm",
        "audio/mpeg" => ".mp3",
        "audio/ogg" => ".ogg",
        "application/pdf" => ".pdf",
        "text/plain" => ".txt",
        "text/html" => ".html",
        "application/json" => ".json",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::auth::WsRelayState;
    use crate::storage::HashtreeStore;
    use axum::response::IntoResponse;
    use base64::Engine;
    use hashtree_core::sha256;
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::Duration;
    use tempfile::TempDir;

    fn test_app_state(store: Arc<HashtreeStore>) -> AppState {
        AppState {
            store,
            auth: None,
            daemon_started_at: 1_700_000_000,
            peer_mode: crate::config::ServerMode::Normal,
            hash_get_enabled: true,
            http_webrtc_fetch: true,
            webrtc_peers: None,
            fips_transport: None,
            fetch_from_fips_peers: true,
            ws_relay: Arc::new(WsRelayState::new()),
            max_upload_bytes: 5 * 1024 * 1024,
            public_writes: true,
            public_plaintext_reads: true,
            require_random_untrusted_ingest: true,
            optimistic_blossom_uploads: false,
            optimistic_upload_queue_bytes: 512 * 1024 * 1024,
            optimistic_upload_queue: Arc::new(tokio::sync::Semaphore::new(512 * 1024 * 1024)),
            allowed_pubkeys: HashSet::new(),
            upstream_blossom: Vec::new(),
            social_graph: None,
            social_graph_store: None,
            social_graph_root: None,
            socialgraph_snapshot_public: false,
            nostr_relay: None,
            nostr_relay_urls: Vec::new(),
            tree_root_cache: Arc::new(StdMutex::new(std::collections::HashMap::new())),
            inflight_blob_fetches: Arc::new(tokio::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            inflight_blob_reads: Arc::new(
                tokio::sync::Mutex::new(std::collections::HashMap::new()),
            ),
            blob_cache: Arc::new(crate::blob_cache::BlobCache::for_tests()),
            directory_listing_cache: Arc::new(StdMutex::new(crate::server::new_lookup_cache())),
            resolved_path_cache: Arc::new(StdMutex::new(crate::server::new_lookup_cache())),
            thumbnail_path_cache: Arc::new(StdMutex::new(crate::server::new_lookup_cache())),
            cid_size_cache: Arc::new(StdMutex::new(crate::server::new_lookup_cache())),
        }
    }

    fn create_upload_auth_header(keys: &nostr::Keys) -> String {
        use nostr::{EventBuilder, Kind, Tag, TagKind, Timestamp};

        let now = Timestamp::now();
        let event = EventBuilder::new(
            Kind::Custom(BLOSSOM_AUTH_KIND),
            "",
            vec![
                Tag::custom(TagKind::Custom("t".into()), vec!["upload".to_string()]),
                Tag::custom(
                    TagKind::Custom("expiration".into()),
                    vec![(now.as_u64() + 300).to_string()],
                ),
            ],
        )
        .custom_created_at(now)
        .to_event(keys)
        .expect("sign blossom auth");
        let json = serde_json::to_vec(&event).expect("serialize auth event");
        format!(
            "Nostr {}",
            base64::engine::general_purpose::STANDARD.encode(json)
        )
    }

    #[test]
    fn test_is_valid_sha256() {
        assert!(is_valid_sha256(
            "e2bab35b5296ec2242ded0a01f6d6723a5cd921239280c0a5f0b5589303336b6"
        ));
        assert!(is_valid_sha256(
            "0000000000000000000000000000000000000000000000000000000000000000"
        ));

        // Too short
        assert!(!is_valid_sha256("e2bab35b5296ec2242ded0a01f6d6723"));
        // Too long
        assert!(!is_valid_sha256(
            "e2bab35b5296ec2242ded0a01f6d6723a5cd921239280c0a5f0b5589303336b6aa"
        ));
        // Invalid chars
        assert!(!is_valid_sha256(
            "zzbab35b5296ec2242ded0a01f6d6723a5cd921239280c0a5f0b5589303336b6"
        ));
        // Empty
        assert!(!is_valid_sha256(""));
    }

    #[test]
    fn test_parse_hash_and_extension() {
        let (hash, ext) = parse_hash_and_extension("abc123.png");
        assert_eq!(hash, "abc123");
        assert_eq!(ext, Some(".png"));

        let (hash2, ext2) = parse_hash_and_extension("abc123");
        assert_eq!(hash2, "abc123");
        assert_eq!(ext2, None);

        let (hash3, ext3) = parse_hash_and_extension("abc.123.jpg");
        assert_eq!(hash3, "abc.123");
        assert_eq!(ext3, Some(".jpg"));
    }

    #[test]
    fn test_mime_to_extension() {
        assert_eq!(mime_to_extension("image/png"), ".png");
        assert_eq!(mime_to_extension("image/jpeg"), ".jpg");
        assert_eq!(mime_to_extension("video/mp4"), ".mp4");
        assert_eq!(mime_to_extension("application/octet-stream"), "");
        assert_eq!(mime_to_extension("unknown/type"), "");
    }

    #[tokio::test]
    async fn optimistic_uploads_return_accepted_and_store_in_background() {
        let temp_dir = TempDir::new().expect("temp dir");
        let store = Arc::new(
            HashtreeStore::with_options(temp_dir.path(), None, 128 * 1024 * 1024).expect("store"),
        );
        let mut state = test_app_state(Arc::clone(&store));
        state.optimistic_blossom_uploads = true;

        let keys = nostr::Keys::generate();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            create_upload_auth_header(&keys)
                .parse()
                .expect("auth header value"),
        );
        headers.insert(
            header::CONTENT_TYPE,
            "application/octet-stream"
                .parse()
                .expect("content type header value"),
        );

        let body = axum::body::Bytes::from((0u8..=255).map(|byte| byte ^ 0x55).collect::<Vec<_>>());
        let hash = sha256(&body);
        let response = upload_blob(State(state), headers, body)
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        for _ in 0..50 {
            if store.blob_exists(&hash).expect("blob exists check") {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        panic!("optimistic upload was not stored in the background");
    }

    #[tokio::test]
    async fn optimistic_upload_existing_blob_skips_queue() {
        let temp_dir = TempDir::new().expect("temp dir");
        let store = Arc::new(
            HashtreeStore::with_options(temp_dir.path(), None, 128 * 1024 * 1024).expect("store"),
        );
        let body = axum::body::Bytes::from(
            (0u16..=255)
                .map(|value| ((value * 73 + 19) % 256) as u8)
                .collect::<Vec<_>>(),
        );
        store.put_cached_blob(&body).expect("seed blob");

        let mut state = test_app_state(Arc::clone(&store));
        state.optimistic_blossom_uploads = true;
        state.optimistic_upload_queue_bytes = 1;
        state.optimistic_upload_queue = Arc::new(tokio::sync::Semaphore::new(1));

        let keys = nostr::Keys::generate();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            create_upload_auth_header(&keys)
                .parse()
                .expect("auth header value"),
        );
        headers.insert(
            header::CONTENT_TYPE,
            "application/octet-stream"
                .parse()
                .expect("content type header value"),
        );

        let response = upload_blob(State(state), headers, body)
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn optimistic_upload_existing_blob_uses_queue_before_preflight_when_queue_has_room() {
        let temp_dir = TempDir::new().expect("temp dir");
        let store = Arc::new(
            HashtreeStore::with_options(temp_dir.path(), None, 128 * 1024 * 1024).expect("store"),
        );
        let body = axum::body::Bytes::from((0u8..=255).rev().collect::<Vec<_>>());
        let hash_hex = hex::encode(sha256(&body));
        store.put_cached_blob(&body).expect("seed blob");

        let mut state = test_app_state(store);
        state.optimistic_blossom_uploads = true;

        let keys = nostr::Keys::generate();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            create_upload_auth_header(&keys)
                .parse()
                .expect("auth header value"),
        );
        headers.insert(
            header::CONTENT_TYPE,
            "application/octet-stream"
                .parse()
                .expect("content type header value"),
        );

        let response = upload_blob(State(state), headers, body)
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        for _ in 0..50 {
            if !optimistic_upload_is_inflight(&hash_hex) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        clear_optimistic_upload_inflight(&hash_hex);
        panic!("optimistic upload in-flight marker was not cleared");
    }

    #[tokio::test]
    async fn optimistic_upload_inflight_duplicate_skips_queue() {
        let temp_dir = TempDir::new().expect("temp dir");
        let store = Arc::new(
            HashtreeStore::with_options(temp_dir.path(), None, 128 * 1024 * 1024).expect("store"),
        );
        let body = axum::body::Bytes::from((0u8..=255).collect::<Vec<_>>());
        let hash_hex = hex::encode(sha256(&body));
        assert!(mark_optimistic_upload_inflight(&hash_hex));

        let mut state = test_app_state(store);
        state.optimistic_blossom_uploads = true;
        state.optimistic_upload_queue_bytes = 1;
        state.optimistic_upload_queue = Arc::new(tokio::sync::Semaphore::new(1));

        let keys = nostr::Keys::generate();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            create_upload_auth_header(&keys)
                .parse()
                .expect("auth header value"),
        );
        headers.insert(
            header::CONTENT_TYPE,
            "application/octet-stream"
                .parse()
                .expect("content type header value"),
        );

        let response = upload_blob(State(state), headers, body)
            .await
            .into_response();
        clear_optimistic_upload_inflight(&hash_hex);
        assert_eq!(response.status(), StatusCode::ACCEPTED);
    }

    #[test]
    fn public_writes_accept_unlisted_authors_for_uploads() {
        let temp_dir = TempDir::new().expect("temp dir");
        let store =
            Arc::new(HashtreeStore::with_options(temp_dir.path(), None, 700).expect("store"));
        let mut state = test_app_state(store);
        let pubkey = "ea4fe79e57f209309bffed2f92f0b95b59d3d1cb4e8892444398aeea7ee317ed";

        state.public_writes = true;
        assert!(can_accept_upload_author(&state, pubkey));
        assert!(!is_allowed_write_author(&state, pubkey));

        state.public_writes = false;
        assert!(!can_accept_upload_author(&state, pubkey));
    }

    #[test]
    fn public_write_trust_allows_octet_stream_and_raw_media_payloads() {
        let encrypted_block: Vec<u8> = (0..=255).collect();

        assert_eq!(
            validate_upload_payload(&encrypted_block, "application/octet-stream", false, true,),
            Ok(())
        );

        assert_eq!(
            validate_upload_payload(b"audio bytes", "audio/mpeg", true, true,),
            Ok(())
        );

        assert_eq!(
            validate_upload_payload(b"audio bytes", "audio/mpeg", false, true,),
            Err((
                StatusCode::FORBIDDEN,
                "Raw media uploads require write access".to_string(),
            ))
        );
    }

    #[test]
    fn authenticated_chk_uploads_skip_entropy_heuristic() {
        let low_unique_block: Vec<u8> = (0..256).map(|i| (i % 139) as u8).collect();

        assert_eq!(
            validate_upload_payload(&low_unique_block, "application/octet-stream", true, true,),
            Ok(())
        );

        assert_eq!(
            validate_upload_payload(&low_unique_block, "application/octet-stream", false, true,),
            Err((
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "Data not encrypted. Unique: 139 (min: 140)".to_string(),
            ))
        );
    }

    #[test]
    fn unowned_public_uploads_use_cache_storage_semantics() {
        let temp_dir = TempDir::new().expect("temp dir");
        let store =
            Arc::new(HashtreeStore::with_options(temp_dir.path(), None, 700).expect("store"));
        let state = test_app_state(Arc::clone(&store));

        let owned = vec![1u8; 280];
        let owned_hash = sha256(&owned);
        store_blossom_blob(&state, &owned, &owned_hash, &[2u8; 32], true).expect("owned upload");

        let public_upload = vec![3u8; 280];
        let public_hash = sha256(&public_upload);
        store_blossom_blob(&state, &public_upload, &public_hash, &[4u8; 32], false)
            .expect("public upload");

        let replacement = vec![5u8; 280];
        let replacement_hash = sha256(&replacement);
        state
            .store
            .put_cached_blob(&replacement)
            .expect("replacement cached blob");

        assert!(state.store.blob_exists(&owned_hash).expect("owned exists"));
        assert!(!state
            .store
            .blob_exists(&public_hash)
            .expect("public upload evicted"));
        assert!(state
            .store
            .blob_exists(&replacement_hash)
            .expect("replacement exists"));
        assert!(state
            .store
            .is_blob_owner(&owned_hash, &[2u8; 32])
            .expect("owned tracked"));
        assert!(!state
            .store
            .blob_has_owners(&public_hash)
            .expect("public upload unowned"));
    }

    #[test]
    fn owned_blossom_uploads_are_rejected_when_storage_limit_is_full() {
        let temp_dir = TempDir::new().expect("temp dir");
        let store =
            Arc::new(HashtreeStore::with_options(temp_dir.path(), None, 500).expect("store"));
        let state = test_app_state(Arc::clone(&store));

        let first = vec![1u8; 300];
        let first_hash = sha256(&first);
        let owner = [2u8; 32];
        store_blossom_blob(&state, &first, &first_hash, &owner, true).expect("first upload");

        let second = vec![3u8; 300];
        let second_hash = sha256(&second);
        let error = store_blossom_blob(&state, &second, &second_hash, &owner, true)
            .expect_err("second owned upload should exceed the storage limit");

        assert!(
            error.to_string().contains("storage limit"),
            "unexpected error: {error}"
        );
        assert!(state
            .store
            .blob_exists(&first_hash)
            .expect("first blob remains"));
        assert!(!state
            .store
            .blob_exists(&second_hash)
            .expect("second blob rejected"));
        assert!(state
            .store
            .is_blob_owner(&first_hash, &owner)
            .expect("first owner tracked"));
        assert!(!state
            .store
            .is_blob_owner(&second_hash, &owner)
            .expect("second owner not tracked"));
    }
}
