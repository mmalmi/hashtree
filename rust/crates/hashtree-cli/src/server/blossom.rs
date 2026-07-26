//! Blossom protocol implementation (BUD-01, BUD-02)
//!
//! Implements blob storage endpoints with Nostr-based authentication.
//! See: https://github.com/hzrd149/blossom

use axum::{
    body::{Body, Bytes},
    extract::{Path, Query, Request, State},
    http::{header, HeaderMap, Response, StatusCode},
    middleware::Next,
    response::IntoResponse,
    Json,
};
use base64::Engine;
use hashtree_blossom::{
    batch_upload_hash_list_digest, BatchUploadItem, BlossomClient, BATCH_UPLOAD_HASH_LIST_AUTH_TAG,
};
use hashtree_core::from_hex;
use nostr::Keys;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::sync::{
    atomic::{AtomicU64, AtomicUsize, Ordering},
    Arc, Mutex, OnceLock,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};

use super::auth::AppState;
use super::blob_read::{
    run_blob_metadata_read, run_blob_write, BlobIoTaskError, BLOB_READ_BUSY, BLOB_WRITE_BUSY,
};
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
const BLOSSOM_PUBLIC_BASE_URL_ENV: &str = "HTREE_BLOSSOM_PUBLIC_BASE_URL";
const LEGACY_BLOSSOM_PUBLIC_BASE_URL_ENV: &str = "HASHTREE_BLOSSOM_PUBLIC_BASE_URL";

/// Default maximum upload size in bytes (5 MB)
pub const DEFAULT_MAX_UPLOAD_SIZE: usize = 5 * 1024 * 1024;
pub const MAX_SINGLE_UPLOAD_BODY_BYTES: usize = 64 * 1024 * 1024;
const OPTIMISTIC_UPLOAD_MIN_QUEUE_CHARGE_BYTES: usize = 256 * 1024;
const MAX_BATCH_UPLOAD_BLOBS: usize = 1024;
pub const MAX_BATCH_UPLOAD_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_BATCH_UPLOAD_JSON_BODY_BYTES: usize = 96 * 1024 * 1024;
const BINARY_BATCH_UPLOAD_MAGIC: &[u8; 8] = b"HTBBV1\0\0";
const MAX_BINARY_BATCH_CONTENT_TYPE_BYTES: usize = 1024;
pub const MAX_BATCH_UPLOAD_BINARY_BODY_BYTES: usize = MAX_BATCH_UPLOAD_BYTES
    + BINARY_BATCH_UPLOAD_MAGIC.len()
    + 4
    + (MAX_BATCH_UPLOAD_BLOBS * (32 + 2 + MAX_BINARY_BATCH_CONTENT_TYPE_BYTES + 8));
const MAX_UPLOAD_CHECK_HASHES: usize = 10_000;
const SLOW_BATCH_UPLOAD_LOG_MS_ENV: &str = "HTREE_SLOW_BATCH_UPLOAD_LOG_MS";
const BLOSSOM_REPLICA_UPLOAD_CONCURRENCY_ENV: &str = "HTREE_BLOSSOM_REPLICA_UPLOAD_CONCURRENCY";
const DEFAULT_BLOSSOM_REPLICA_UPLOAD_CONCURRENCY: usize = 4;
const BLOSSOM_REPLICA_UPLOAD_ATTEMPTS_ENV: &str = "HTREE_BLOSSOM_REPLICA_UPLOAD_ATTEMPTS";
const DEFAULT_BLOSSOM_REPLICA_UPLOAD_ATTEMPTS: usize = 3;
const BLOSSOM_REPLICA_COALESCE_MAX_BLOBS_ENV: &str = "HTREE_BLOSSOM_REPLICA_COALESCE_MAX_BLOBS";
const DEFAULT_BLOSSOM_REPLICA_COALESCE_MAX_BLOBS: usize = 64;
const BLOSSOM_REPLICA_COALESCE_MAX_BYTES_ENV: &str = "HTREE_BLOSSOM_REPLICA_COALESCE_MAX_BYTES";
const DEFAULT_BLOSSOM_REPLICA_COALESCE_MAX_BYTES: usize = 16 * 1024 * 1024;
const BLOSSOM_REPLICA_COALESCE_FLUSH_MS_ENV: &str = "HTREE_BLOSSOM_REPLICA_COALESCE_FLUSH_MS";
const DEFAULT_BLOSSOM_REPLICA_COALESCE_FLUSH_MS: u64 = 25;
const BLOSSOM_REPLICA_COALESCE_QUEUE_JOBS_ENV: &str = "HTREE_BLOSSOM_REPLICA_COALESCE_QUEUE_JOBS";
const DEFAULT_BLOSSOM_REPLICA_COALESCE_QUEUE_JOBS: usize = 1024;

fn slow_batch_upload_log_ms() -> Option<u128> {
    std::env::var(SLOW_BATCH_UPLOAD_LOG_MS_ENV)
        .ok()
        .and_then(|value| value.parse::<u128>().ok())
        .filter(|value| *value > 0)
}

fn blossom_replica_upload_concurrency() -> usize {
    std::env::var(BLOSSOM_REPLICA_UPLOAD_CONCURRENCY_ENV)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_BLOSSOM_REPLICA_UPLOAD_CONCURRENCY)
}

fn blossom_replica_upload_attempts() -> usize {
    std::env::var(BLOSSOM_REPLICA_UPLOAD_ATTEMPTS_ENV)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_BLOSSOM_REPLICA_UPLOAD_ATTEMPTS)
}

fn blossom_replica_coalesce_max_blobs() -> usize {
    std::env::var(BLOSSOM_REPLICA_COALESCE_MAX_BLOBS_ENV)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_BLOSSOM_REPLICA_COALESCE_MAX_BLOBS)
        .min(MAX_BATCH_UPLOAD_BLOBS)
}

fn blossom_replica_coalesce_max_bytes() -> usize {
    std::env::var(BLOSSOM_REPLICA_COALESCE_MAX_BYTES_ENV)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_BLOSSOM_REPLICA_COALESCE_MAX_BYTES)
        .min(MAX_BATCH_UPLOAD_BYTES)
}

fn blossom_replica_coalesce_flush_delay() -> Duration {
    let millis = std::env::var(BLOSSOM_REPLICA_COALESCE_FLUSH_MS_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_BLOSSOM_REPLICA_COALESCE_FLUSH_MS);
    Duration::from_millis(millis)
}

fn blossom_replica_coalesce_queue_jobs() -> usize {
    std::env::var(BLOSSOM_REPLICA_COALESCE_QUEUE_JOBS_ENV)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_BLOSSOM_REPLICA_COALESCE_QUEUE_JOBS)
}

fn blossom_replica_upload_semaphore() -> Arc<Semaphore> {
    static SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();
    SEMAPHORE
        .get_or_init(|| Arc::new(Semaphore::new(blossom_replica_upload_concurrency())))
        .clone()
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

#[derive(Debug, Clone, Copy)]
pub(super) struct BlossomUploadReplicaQueueSnapshot {
    pub enabled: bool,
    pub target_count: usize,
    pub max_bytes: usize,
    pub available_bytes: usize,
    pub reserved_bytes: usize,
    pub coalesce_queue_capacity_jobs: usize,
    pub coalesce_queued_jobs: usize,
    pub coalesce_max_blobs: usize,
    pub coalesce_max_bytes: usize,
    pub coalesce_flush_ms: u64,
    pub upload_concurrency: usize,
    pub in_flight_batches: usize,
    pub accepted_batches: u64,
    pub accepted_blobs: u64,
    pub uploaded_blobs: u64,
    pub replicated_bytes: u64,
    pub failed_batches: u64,
    pub skipped_jobs: u64,
    pub fallback_batches: u64,
    pub fallback_uploaded_blobs: u64,
    pub fallback_failed_blobs: u64,
}

#[derive(Default)]
struct BlossomUploadReplicaMetrics {
    coalesce_queued_jobs: AtomicUsize,
    in_flight_batches: AtomicUsize,
    accepted_batches: AtomicU64,
    accepted_blobs: AtomicU64,
    uploaded_blobs: AtomicU64,
    replicated_bytes: AtomicU64,
    failed_batches: AtomicU64,
    skipped_jobs: AtomicU64,
    fallback_batches: AtomicU64,
    fallback_uploaded_blobs: AtomicU64,
    fallback_failed_blobs: AtomicU64,
}

fn blossom_upload_replica_metrics() -> &'static BlossomUploadReplicaMetrics {
    static METRICS: OnceLock<BlossomUploadReplicaMetrics> = OnceLock::new();
    METRICS.get_or_init(BlossomUploadReplicaMetrics::default)
}

struct BlossomReplicaInFlightGuard;

impl BlossomReplicaInFlightGuard {
    fn new() -> Self {
        blossom_upload_replica_metrics()
            .in_flight_batches
            .fetch_add(1, Ordering::Relaxed);
        Self
    }
}

impl Drop for BlossomReplicaInFlightGuard {
    fn drop(&mut self) {
        blossom_upload_replica_metrics()
            .in_flight_batches
            .fetch_sub(1, Ordering::Relaxed);
    }
}

struct PreparedBlossomUploadReplication {
    servers: Vec<String>,
    keys: Arc<Keys>,
    permit: OwnedSemaphorePermit,
    total_bytes: usize,
}

struct BlossomReplicaUploadJob {
    prepared: PreparedBlossomUploadReplication,
    items: Vec<BatchUploadItem>,
}

struct BlossomReplicaUploadBatch {
    servers: Vec<String>,
    keys: Arc<Keys>,
    permits: Vec<OwnedSemaphorePermit>,
    total_bytes: usize,
    data_bytes: usize,
    items: Vec<BatchUploadItem>,
}

impl BlossomReplicaUploadBatch {
    fn from_job(job: BlossomReplicaUploadJob) -> Self {
        let PreparedBlossomUploadReplication {
            servers,
            keys,
            permit,
            total_bytes,
        } = job.prepared;
        let data_bytes = job.items.iter().map(|item| item.data.len()).sum();
        Self {
            servers,
            keys,
            permits: vec![permit],
            total_bytes,
            data_bytes,
            items: job.items,
        }
    }

    fn can_append(
        &self,
        job: &BlossomReplicaUploadJob,
        max_blobs: usize,
        max_bytes: usize,
    ) -> bool {
        if self.servers != job.prepared.servers || !Arc::ptr_eq(&self.keys, &job.prepared.keys) {
            return false;
        }
        let job_bytes = job.items.iter().map(|item| item.data.len()).sum::<usize>();
        self.items.len().saturating_add(job.items.len()) <= max_blobs
            && self.data_bytes.saturating_add(job_bytes) <= max_bytes
    }

    fn append(&mut self, job: BlossomReplicaUploadJob) {
        let PreparedBlossomUploadReplication {
            permit,
            total_bytes,
            ..
        } = job.prepared;
        self.permits.push(permit);
        self.total_bytes = self.total_bytes.saturating_add(total_bytes);
        self.data_bytes = self
            .data_bytes
            .saturating_add(job.items.iter().map(|item| item.data.len()).sum::<usize>());
        self.items.extend(job.items);
    }

    fn reached_limits(&self, max_blobs: usize, max_bytes: usize) -> bool {
        self.items.len() >= max_blobs || self.data_bytes >= max_bytes
    }
}

/// Per-server write-behind scheduler for merging adjacent replica uploads.
pub struct BlossomUploadReplicaScheduler {
    sender: Mutex<Option<mpsc::Sender<BlossomReplicaUploadJob>>>,
}

impl BlossomUploadReplicaScheduler {
    pub fn new() -> Self {
        Self {
            sender: Mutex::new(None),
        }
    }

    fn schedule(&self, job: BlossomReplicaUploadJob) -> Result<(), BlossomReplicaUploadJob> {
        let max_blobs = blossom_replica_coalesce_max_blobs();
        let flush_delay = blossom_replica_coalesce_flush_delay();
        if max_blobs <= 1 || flush_delay.is_zero() {
            return Err(job);
        }

        let mut job = job;
        for _ in 0..2 {
            let sender = self.sender();
            blossom_upload_replica_metrics()
                .coalesce_queued_jobs
                .fetch_add(1, Ordering::Relaxed);
            match sender.try_send(job) {
                Ok(()) => return Ok(()),
                Err(mpsc::error::TrySendError::Full(returned)) => {
                    blossom_upload_replica_metrics()
                        .coalesce_queued_jobs
                        .fetch_sub(1, Ordering::Relaxed);
                    return Err(returned);
                }
                Err(mpsc::error::TrySendError::Closed(returned)) => {
                    blossom_upload_replica_metrics()
                        .coalesce_queued_jobs
                        .fetch_sub(1, Ordering::Relaxed);
                    self.clear_sender();
                    job = returned;
                }
            }
        }
        Err(job)
    }

    fn sender(&self) -> mpsc::Sender<BlossomReplicaUploadJob> {
        let mut guard = self.sender.lock().unwrap_or_else(|err| err.into_inner());
        if guard.as_ref().is_some_and(|sender| sender.is_closed()) {
            *guard = None;
        }
        if let Some(sender) = guard.as_ref() {
            return sender.clone();
        }

        let (sender, receiver) = mpsc::channel(blossom_replica_coalesce_queue_jobs());
        tokio::spawn(blossom_replica_coalescer_worker(receiver));
        *guard = Some(sender.clone());
        sender
    }

    fn clear_sender(&self) {
        let mut guard = self.sender.lock().unwrap_or_else(|err| err.into_inner());
        *guard = None;
    }
}

impl Default for BlossomUploadReplicaScheduler {
    fn default() -> Self {
        Self::new()
    }
}

pub(super) fn blossom_upload_replica_queue_snapshot(
    state: &AppState,
) -> BlossomUploadReplicaQueueSnapshot {
    let available_bytes = state.blossom_upload_replica_queue.available_permits();
    let metrics = blossom_upload_replica_metrics();
    BlossomUploadReplicaQueueSnapshot {
        enabled: !state.blossom_upload_replicas.is_empty(),
        target_count: state.blossom_upload_replicas.len(),
        max_bytes: state.blossom_upload_replica_queue_bytes,
        available_bytes,
        reserved_bytes: state
            .blossom_upload_replica_queue_bytes
            .saturating_sub(available_bytes),
        coalesce_queue_capacity_jobs: blossom_replica_coalesce_queue_jobs(),
        coalesce_queued_jobs: metrics.coalesce_queued_jobs.load(Ordering::Relaxed),
        coalesce_max_blobs: blossom_replica_coalesce_max_blobs(),
        coalesce_max_bytes: blossom_replica_coalesce_max_bytes(),
        coalesce_flush_ms: duration_millis_u64(blossom_replica_coalesce_flush_delay()),
        upload_concurrency: blossom_replica_upload_concurrency(),
        in_flight_batches: metrics.in_flight_batches.load(Ordering::Relaxed),
        accepted_batches: metrics.accepted_batches.load(Ordering::Relaxed),
        accepted_blobs: metrics.accepted_blobs.load(Ordering::Relaxed),
        uploaded_blobs: metrics.uploaded_blobs.load(Ordering::Relaxed),
        replicated_bytes: metrics.replicated_bytes.load(Ordering::Relaxed),
        failed_batches: metrics.failed_batches.load(Ordering::Relaxed),
        skipped_jobs: metrics.skipped_jobs.load(Ordering::Relaxed),
        fallback_batches: metrics.fallback_batches.load(Ordering::Relaxed),
        fallback_uploaded_blobs: metrics.fallback_uploaded_blobs.load(Ordering::Relaxed),
        fallback_failed_blobs: metrics.fallback_failed_blobs.load(Ordering::Relaxed),
    }
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

#[derive(Debug)]
enum BlobWriteError {
    Busy(&'static str),
    Storage(anyhow::Error),
}

impl std::fmt::Display for BlobWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy(reason) => f.write_str(reason),
            Self::Storage(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for BlobWriteError {}

impl From<anyhow::Error> for BlobWriteError {
    fn from(error: anyhow::Error) -> Self {
        Self::Storage(error)
    }
}

fn blob_io_write_error(error: BlobIoTaskError) -> BlobWriteError {
    if error.is_busy() {
        BlobWriteError::Busy(BLOB_WRITE_BUSY)
    } else {
        BlobWriteError::Storage(anyhow::anyhow!(error))
    }
}

fn blob_write_error_response(error: BlobWriteError) -> Response<Body> {
    match error {
        BlobWriteError::Busy(reason) => {
            blossom_retryable_json_error(StatusCode::SERVICE_UNAVAILABLE, reason, 2)
        }
        BlobWriteError::Storage(error) => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header("X-Reason", "Storage error")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(format!(r#"{{"error":"{}"}}"#, error)))
            .unwrap(),
    }
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

#[derive(Debug, Serialize, Deserialize)]
pub struct BatchUploadResponse {
    pub uploaded: usize,
    pub blobs: Vec<BlobDescriptor>,
}

#[derive(Debug)]
struct DecodedBatchUploadBlob {
    sha256: String,
    content_type: Option<String>,
    data: Vec<u8>,
}

#[derive(Debug, Deserialize)]
pub struct UploadCheckRequest {
    pub hashes: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UploadCheckResponse {
    pub count: usize,
    pub present: String,
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
    pub action: Option<String>,    // "upload", "delete", "list", "get"
    pub blob_hashes: Vec<String>,  // x tags
    pub batch_hashes: Vec<String>, // x-batch tags
    pub server: Option<String>,    // server tag
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
    let mut batch_hashes: Vec<String> = Vec::new();
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
                    BATCH_UPLOAD_HASH_LIST_AUTH_TAG => batch_hashes.push(tag_value.to_lowercase()),
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
        batch_hashes,
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
            "GET, HEAD, POST, PUT, DELETE, OPTIONS",
        )
        .header(header::ACCESS_CONTROL_ALLOW_HEADERS, full_allowed)
        .header(header::ACCESS_CONTROL_MAX_AGE, "86400")
        .body(Body::empty())
        .unwrap()
}

/// HEAD /upload - BUD-06 upload preflight.
pub async fn head_upload(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let sha256_hex = match first_header_value(&headers, "x-sha-256") {
        Some(hash) if is_valid_sha256(&hash) => hash.to_ascii_lowercase(),
        Some(_) => {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .header("X-Reason", "Invalid X-SHA-256 header")
                .body(Body::empty())
                .unwrap();
        }
        None => {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .header("X-Reason", "Missing X-SHA-256 header")
                .body(Body::empty())
                .unwrap();
        }
    };

    let content_length = match first_header_value(&headers, "x-content-length")
        .or_else(|| first_header_value(&headers, header::CONTENT_LENGTH.as_str()))
        .and_then(|value| value.parse::<usize>().ok())
    {
        Some(length) => length,
        None => {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .header("X-Reason", "Missing or invalid X-Content-Length header")
                .body(Body::empty())
                .unwrap();
        }
    };

    if content_length > state.max_upload_bytes {
        return Response::builder()
            .status(StatusCode::PAYLOAD_TOO_LARGE)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header("X-Reason", "Upload exceeds maximum size")
            .body(Body::empty())
            .unwrap();
    }

    let content_type = first_header_value(&headers, "x-content-type")
        .or_else(|| first_header_value(&headers, header::CONTENT_TYPE.as_str()))
        .unwrap_or_else(|| "application/octet-stream".to_string());

    if !is_chk_content_type(&content_type_base(&content_type)) {
        let auth = verify_blossom_auth(&headers, "upload", Some(&sha256_hex));
        let can_upload_raw = auth
            .as_ref()
            .map(|auth| can_accept_upload_author(&state, &auth.pubkey))
            .unwrap_or(false);
        if !can_upload_raw {
            let status = if auth.is_err() {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::FORBIDDEN
            };
            return Response::builder()
                .status(status)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .header("X-Reason", "Raw media uploads require write access")
                .body(Body::empty())
                .unwrap();
        }
    }

    Response::builder()
        .status(StatusCode::OK)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .body(Body::empty())
        .unwrap()
}

fn encode_upload_check_bitset(bits: &[bool]) -> String {
    let mut bytes = vec![0u8; bits.len().div_ceil(8)];
    for (index, present) in bits.iter().enumerate() {
        if *present {
            bytes[index / 8] |= 1 << (index % 8);
        }
    }
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// POST /upload/check - Batch-check blob presence for upload planning.
pub async fn upload_check(
    State(state): State<AppState>,
    Json(payload): Json<UploadCheckRequest>,
) -> impl IntoResponse {
    if payload.hashes.len() > MAX_UPLOAD_CHECK_HASHES {
        return blossom_json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("Too many hashes; maximum is {}", MAX_UPLOAD_CHECK_HASHES),
        );
    }

    let mut requested = Vec::with_capacity(payload.hashes.len());
    let mut unique = Vec::new();
    for hash in payload.hashes {
        let hash = hash.trim().to_ascii_lowercase();
        if !is_valid_sha256(&hash) {
            return blossom_json_error(StatusCode::BAD_REQUEST, "Invalid SHA256 hash");
        }
        let bytes: [u8; 32] = match from_hex(&hash) {
            Ok(bytes) => bytes,
            Err(_) => return blossom_json_error(StatusCode::BAD_REQUEST, "Invalid SHA256 hash"),
        };
        requested.push(bytes);
        unique.push(bytes);
    }

    unique.sort_unstable();
    unique.dedup();

    let existing = if unique.is_empty() {
        Vec::new()
    } else {
        let store = state.store.clone();
        let lookup_hashes = unique.clone();
        match run_blob_metadata_read(move || {
            store
                .router()
                .existing_local_hashes_in_sorted_candidates(&lookup_hashes)
        })
        .await
        {
            Ok(Ok(existing)) => existing,
            Ok(Err(error)) => {
                tracing::debug!("Blossom upload check failed: {}", error);
                return blossom_json_error(StatusCode::INTERNAL_SERVER_ERROR, "Storage error");
            }
            Err(error) if error.is_busy() => {
                return blossom_retryable_json_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    BLOB_READ_BUSY,
                    1,
                );
            }
            Err(error) if error.is_timeout() => {
                return blossom_retryable_json_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Blob check timed out",
                    1,
                );
            }
            Err(error) => {
                tracing::debug!("Blossom upload check task failed: {}", error);
                return blossom_json_error(StatusCode::INTERNAL_SERVER_ERROR, "Storage error");
            }
        }
    };

    let present_unique: HashSet<[u8; 32]> = unique
        .into_iter()
        .zip(existing)
        .filter_map(|(hash, present)| present.then_some(hash))
        .collect();
    let present_bits: Vec<bool> = requested
        .iter()
        .map(|hash| present_unique.contains(hash))
        .collect();

    Response::builder()
        .status(StatusCode::OK)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::to_string(&UploadCheckResponse {
                count: present_bits.len(),
                present: encode_upload_check_bitset(&present_bits),
            })
            .unwrap(),
        ))
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
                .unwrap();
        }
    };

    // Blossom HEAD only needs metadata; avoid reading the full blob body just to
    // answer cache probes and CDN revalidation. The read permit keeps CDN probe
    // storms from filling Tokio's blocking thread pool while old blobs without
    // metadata are still being normalized.
    let blob_size = if let Some(cached) = state.blob_cache.get_size(&sha256_hex) {
        Ok(Ok(cached))
    } else {
        let store = state.store.clone();
        let result = match run_blob_metadata_read(move || store.blob_size(&sha256_bytes)).await {
            Ok(result) => Ok(result),
            Err(error) if error.is_busy() => {
                return Response::builder()
                    .status(StatusCode::SERVICE_UNAVAILABLE)
                    .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                    .header(header::CACHE_CONTROL, NOT_FOUND_CACHE_CONTROL)
                    .header("Retry-After", "1")
                    .header("X-Reason", BLOB_READ_BUSY)
                    .body(Body::empty())
                    .unwrap();
            }
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
) -> Result<bool, BlobWriteError> {
    let mut hasher = Sha256::new();
    hasher.update(&data);
    let hash_hex = hex::encode(hasher.finalize());
    let data_for_cache = data.clone();
    let store = state.store.clone();
    let inserted = run_blob_write(move || {
        let inserted = if track_ownership {
            store.put_owned_blob_with_inserted(&data, &pubkey)?.1
        } else {
            store.put_cached_blob_with_inserted(&data)?.1
        };
        Ok::<_, anyhow::Error>(inserted)
    })
    .await
    .map_err(blob_io_write_error)??;
    state
        .blob_cache
        .put_size(hash_hex.clone(), Some(data_for_cache.len() as u64));
    state.blob_cache.put_body(hash_hex, &data_for_cache);
    Ok(inserted)
}

fn upload_descriptor_response(status: StatusCode, descriptor: &BlobDescriptor) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_string(descriptor).unwrap()))
        .unwrap()
}

fn make_blob_descriptor(
    headers: &HeaderMap,
    sha256_hex: String,
    size: u64,
    mime_type: String,
    uploaded: u64,
) -> BlobDescriptor {
    let url = blossom_blob_url(headers, &sha256_hex, &mime_type);
    BlobDescriptor {
        url,
        sha256: sha256_hex,
        size,
        mime_type,
        uploaded,
    }
}

fn blossom_blob_url(headers: &HeaderMap, sha256_hex: &str, mime_type: &str) -> String {
    format!(
        "{}/{}{}",
        blossom_public_base_url(headers),
        sha256_hex,
        descriptor_extension_for_mime(mime_type)
    )
}

fn descriptor_extension_for_mime(mime_type: &str) -> &'static str {
    let base = content_type_base(mime_type);
    mime_to_extension(&base)
}

fn blossom_public_base_url(headers: &HeaderMap) -> String {
    if let Some(configured) = configured_public_base_url() {
        return configured;
    }

    let host = first_header_value(headers, "x-forwarded-host")
        .and_then(|value| normalize_host(&value))
        .or_else(|| {
            forwarded_header_param(headers, "host").and_then(|value| normalize_host(&value))
        })
        .or_else(|| {
            first_header_value(headers, header::HOST.as_str())
                .and_then(|value| normalize_host(&value))
        });

    let Some(host) = host else {
        return "http://localhost".to_string();
    };

    let scheme = first_header_value(headers, "x-forwarded-proto")
        .and_then(|value| normalize_scheme(&value))
        .or_else(|| {
            forwarded_header_param(headers, "proto").and_then(|value| normalize_scheme(&value))
        })
        .or_else(|| cloudflare_visitor_scheme(headers))
        .unwrap_or_else(|| default_scheme_for_host(&host).to_string());

    format!("{scheme}://{host}")
}

fn configured_public_base_url() -> Option<String> {
    [
        BLOSSOM_PUBLIC_BASE_URL_ENV,
        LEGACY_BLOSSOM_PUBLIC_BASE_URL_ENV,
    ]
    .into_iter()
    .find_map(|name| {
        std::env::var(name)
            .ok()
            .and_then(|value| normalize_public_base_url(&value))
    })
}

fn normalize_public_base_url(value: &str) -> Option<String> {
    let trimmed = value.trim().trim_end_matches('/');
    if (trimmed.starts_with("https://") || trimmed.starts_with("http://"))
        && !trimmed.contains('?')
        && !trimmed.contains('#')
    {
        Some(trimmed.to_string())
    } else {
        None
    }
}

fn first_header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(name)?.to_str().ok()?;
    first_header_component(raw)
}

fn first_header_component(value: &str) -> Option<String> {
    clean_header_value(value.split(',').next().unwrap_or(value))
}

fn forwarded_header_param(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get("forwarded")?.to_str().ok()?;
    let first = raw.split(',').next().unwrap_or(raw);
    for part in first.split(';') {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case(name) {
            return clean_header_value(value);
        }
    }
    None
}

fn clean_header_value(value: &str) -> Option<String> {
    let trimmed = value.trim().trim_matches('"').trim();
    if trimmed.is_empty() || trimmed.chars().any(|ch| ch.is_control()) {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn normalize_scheme(value: &str) -> Option<String> {
    match value.trim().trim_matches('"').to_ascii_lowercase().as_str() {
        "http" => Some("http".to_string()),
        "https" => Some("https".to_string()),
        _ => None,
    }
}

fn normalize_host(value: &str) -> Option<String> {
    let mut host = value.trim().trim_matches('"').trim();
    if let Some(rest) = host.strip_prefix("http://") {
        host = rest;
    } else if let Some(rest) = host.strip_prefix("https://") {
        host = rest;
    }
    host = host.split('/').next().unwrap_or(host).trim();
    if host.is_empty()
        || host
            .chars()
            .any(|ch| ch.is_control() || ch.is_ascii_whitespace() || ch == '\\')
    {
        None
    } else {
        Some(host.to_string())
    }
}

fn cloudflare_visitor_scheme(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get("cf-visitor")?.to_str().ok()?;
    let parsed: serde_json::Value = serde_json::from_str(raw).ok()?;
    parsed
        .get("scheme")
        .and_then(|value| value.as_str())
        .and_then(normalize_scheme)
}

fn default_scheme_for_host(host: &str) -> &'static str {
    let host_without_port = host
        .trim_start_matches('[')
        .split(']')
        .next()
        .unwrap_or(host)
        .split(':')
        .next()
        .unwrap_or(host)
        .to_ascii_lowercase();
    if host_without_port == "localhost"
        || host_without_port == "::1"
        || host_without_port.starts_with("127.")
    {
        "http"
    } else {
        "https"
    }
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

fn replica_item(hash: String, data: Vec<u8>, content_type: String) -> BatchUploadItem {
    BatchUploadItem {
        hash,
        data,
        content_type: Some(content_type),
    }
}

fn prepare_blossom_upload_replication(
    state: &AppState,
    total_bytes: usize,
) -> Option<PreparedBlossomUploadReplication> {
    if state.blossom_upload_replicas.is_empty() {
        return None;
    }
    let permits = match u32::try_from(total_bytes.max(1)) {
        Ok(permits) => permits,
        Err(_) => {
            blossom_upload_replica_metrics()
                .skipped_jobs
                .fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                total_bytes,
                "Skipping Blossom write-behind replication because batch is too large"
            );
            return None;
        }
    };
    let permit = match state
        .blossom_upload_replica_queue
        .clone()
        .try_acquire_many_owned(permits)
    {
        Ok(permit) => permit,
        Err(error) => {
            blossom_upload_replica_metrics()
                .skipped_jobs
                .fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                total_bytes,
                targets = state.blossom_upload_replicas.len(),
                error = %error,
                "Skipping Blossom write-behind replication because queue is full"
            );
            return None;
        }
    };

    let keys = match state.blossom_upload_replica_keys.clone() {
        Some(keys) => keys,
        None => {
            blossom_upload_replica_metrics()
                .skipped_jobs
                .fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                "Skipping Blossom write-behind replication because server keys are unavailable"
            );
            return None;
        }
    };
    Some(PreparedBlossomUploadReplication {
        servers: state.blossom_upload_replicas.clone(),
        keys,
        permit,
        total_bytes,
    })
}

fn schedule_prepared_blossom_upload_replication(
    state: &AppState,
    prepared: PreparedBlossomUploadReplication,
    items: Vec<BatchUploadItem>,
) {
    if items.is_empty() {
        blossom_upload_replica_metrics()
            .skipped_jobs
            .fetch_add(1, Ordering::Relaxed);
        return;
    }

    let job = BlossomReplicaUploadJob { prepared, items };
    let job = match state.blossom_upload_replica_scheduler.schedule(job) {
        Ok(()) => return,
        Err(job) => job,
    };
    spawn_blossom_replica_upload_batch(BlossomReplicaUploadBatch::from_job(job));
}

async fn blossom_replica_coalescer_worker(mut receiver: mpsc::Receiver<BlossomReplicaUploadJob>) {
    let max_blobs = blossom_replica_coalesce_max_blobs();
    let max_bytes = blossom_replica_coalesce_max_bytes();
    let flush_delay = blossom_replica_coalesce_flush_delay();

    while let Some(job) = receiver.recv().await {
        blossom_upload_replica_metrics()
            .coalesce_queued_jobs
            .fetch_sub(1, Ordering::Relaxed);
        let mut batch = BlossomReplicaUploadBatch::from_job(job);
        loop {
            if batch.reached_limits(max_blobs, max_bytes) {
                break;
            }
            match tokio::time::timeout(flush_delay, receiver.recv()).await {
                Ok(Some(next_job)) => {
                    blossom_upload_replica_metrics()
                        .coalesce_queued_jobs
                        .fetch_sub(1, Ordering::Relaxed);
                    if batch.can_append(&next_job, max_blobs, max_bytes) {
                        batch.append(next_job);
                    } else {
                        spawn_blossom_replica_upload_batch(batch);
                        batch = BlossomReplicaUploadBatch::from_job(next_job);
                    }
                }
                Ok(None) => {
                    spawn_blossom_replica_upload_batch(batch);
                    return;
                }
                Err(_) => break,
            }
        }
        spawn_blossom_replica_upload_batch(batch);
    }
}

fn spawn_blossom_replica_upload_batch(batch: BlossomReplicaUploadBatch) {
    tokio::spawn(async move {
        let _in_flight = BlossomReplicaInFlightGuard::new();
        let BlossomReplicaUploadBatch {
            servers,
            keys,
            permits: _permits,
            total_bytes,
            data_bytes: _,
            items,
        } = batch;
        let upload_limit = blossom_replica_upload_semaphore();
        let _upload_permit = match upload_limit.acquire().await {
            Ok(permit) => permit,
            Err(error) => {
                blossom_upload_replica_metrics()
                    .failed_batches
                    .fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    error = %error,
                    "Skipping Blossom write-behind replication because upload limiter is closed"
                );
                return;
            }
        };
        let attempts = blossom_replica_upload_attempts();
        let client = BlossomClient::new((*keys).clone()).with_write_servers(servers.clone());
        for server in servers {
            for attempt in 1..=attempts {
                match client.upload_batch_to_server(&server, &items).await {
                    Ok(Some(result)) => {
                        let metrics = blossom_upload_replica_metrics();
                        metrics.accepted_batches.fetch_add(1, Ordering::Relaxed);
                        metrics
                            .accepted_blobs
                            .fetch_add(result.accepted as u64, Ordering::Relaxed);
                        metrics
                            .uploaded_blobs
                            .fetch_add(result.uploaded as u64, Ordering::Relaxed);
                        metrics
                            .replicated_bytes
                            .fetch_add(total_bytes as u64, Ordering::Relaxed);
                        tracing::debug!(
                            target = %server,
                            accepted = result.accepted,
                            uploaded = result.uploaded,
                            total = items.len(),
                            bytes = total_bytes,
                            "Replicated Blossom upload batch"
                        );
                        break;
                    }
                    Ok(None) => {
                        replicate_items_individually(&client, &server, &items, total_bytes).await;
                        break;
                    }
                    Err(error) if attempt < attempts => {
                        tracing::warn!(
                            target = %server,
                            error = %error,
                            attempt,
                            attempts,
                            total = items.len(),
                            bytes = total_bytes,
                            "Blossom write-behind replication retrying"
                        );
                        tokio::time::sleep(Duration::from_millis(250 * attempt as u64)).await;
                    }
                    Err(error) => {
                        blossom_upload_replica_metrics()
                            .failed_batches
                            .fetch_add(1, Ordering::Relaxed);
                        tracing::warn!(
                            target = %server,
                            error = %error,
                            attempt,
                            attempts,
                            total = items.len(),
                            bytes = total_bytes,
                            "Blossom write-behind replication failed"
                        );
                        break;
                    }
                }
            }
        }
    });
}

async fn replicate_items_individually(
    client: &BlossomClient,
    server: &str,
    items: &[BatchUploadItem],
    total_bytes: usize,
) {
    let mut uploaded = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;
    let server_list = [server.to_string()];
    for item in items {
        match client
            .upload_to_selected_servers(&item.data, &server_list)
            .await
        {
            Ok((_hash, successes)) if successes > 0 => uploaded += 1,
            Ok((_hash, _)) => skipped += 1,
            Err(error) => {
                failed += 1;
                tracing::warn!(
                    target = %server,
                    hash = %item.hash,
                    error = %error,
                    "Blossom write-behind item replication failed"
                );
            }
        }
    }
    let metrics = blossom_upload_replica_metrics();
    metrics.fallback_batches.fetch_add(1, Ordering::Relaxed);
    metrics
        .fallback_uploaded_blobs
        .fetch_add(uploaded as u64, Ordering::Relaxed);
    metrics
        .fallback_failed_blobs
        .fetch_add(failed as u64, Ordering::Relaxed);
    if failed == 0 {
        metrics.accepted_batches.fetch_add(1, Ordering::Relaxed);
        metrics
            .accepted_blobs
            .fetch_add(items.len() as u64, Ordering::Relaxed);
        metrics
            .uploaded_blobs
            .fetch_add(uploaded as u64, Ordering::Relaxed);
        metrics
            .replicated_bytes
            .fetch_add(total_bytes as u64, Ordering::Relaxed);
    } else {
        metrics.failed_batches.fetch_add(1, Ordering::Relaxed);
    }
    tracing::debug!(
        target = %server,
        uploaded,
        skipped,
        failed,
        total = items.len(),
        bytes = total_bytes,
        "Replicated Blossom upload items without batch support"
    );
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

    let store = state.store.clone();
    let result = run_blob_metadata_read(move || {
        store
            .blob_size(&sha256_hash)
            .map_err(|error| error.to_string())
    })
    .await;
    match result {
        Ok(Ok(size)) => {
            state.blob_cache.put_size(sha256_hex.to_string(), size);
            Ok(size.is_some())
        }
        Ok(Err(error)) => Err(error),
        Err(error) if error.is_busy() => Err(BLOB_READ_BUSY.to_string()),
        Err(error) if error.is_timeout() => Err("blob existence check timed out".to_string()),
        Err(error) => Err(format!("blob existence task failed: {}", error)),
    }
}

async fn set_existing_blob_owner_without_body_write(
    state: AppState,
    sha256_hash: [u8; 32],
    pubkey: [u8; 32],
) -> anyhow::Result<()> {
    run_blob_write(move || state.store.set_blob_owner(&sha256_hash, &pubkey))
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
    let descriptor = make_blob_descriptor(&headers, sha256_hex.clone(), size, content_type, now);

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
            let replica_body = body.clone();
            let replica_content_type = descriptor.mime_type.clone();
            tokio::spawn(async move {
                let _permit = permit;
                match store_blossom_blob_without_blocking_runtime(
                    &state_for_write,
                    body,
                    pubkey_bytes,
                    is_allowed_author,
                )
                .await
                {
                    Ok(inserted) => {
                        if inserted {
                            if let Some(replication) = prepare_blossom_upload_replication(
                                &state_for_write,
                                replica_body.len(),
                            ) {
                                let replication_item = replica_item(
                                    hash_for_log.clone(),
                                    replica_body.to_vec(),
                                    replica_content_type,
                                );
                                schedule_prepared_blossom_upload_replication(
                                    &state_for_write,
                                    replication,
                                    vec![replication_item],
                                );
                            }
                        }
                    }
                    Err(error) => {
                        tracing::error!(
                            "Background Blossom storage failed for {}: {:#}",
                            hash_for_log,
                            error
                        );
                    }
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
            return upload_descriptor_response(StatusCode::OK, &descriptor);
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
        Ok(inserted) => {
            if inserted {
                if let Some(replication) = prepare_blossom_upload_replication(&state, body.len()) {
                    let replication_item = replica_item(
                        sha256_hex.clone(),
                        body.to_vec(),
                        descriptor.mime_type.clone(),
                    );
                    schedule_prepared_blossom_upload_replication(
                        &state,
                        replication,
                        vec![replication_item],
                    );
                }
            }
            upload_descriptor_response(StatusCode::CREATED, &descriptor)
        }
        Err(error) => blob_write_error_response(error),
    }
}

fn take_binary_batch_bytes<'a>(
    body: &'a [u8],
    cursor: &mut usize,
    len: usize,
) -> Result<&'a [u8], (StatusCode, String)> {
    let end = cursor.checked_add(len).ok_or_else(|| {
        (
            StatusCode::PAYLOAD_TOO_LARGE,
            "Binary batch field length overflow".to_string(),
        )
    })?;
    if end > body.len() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Binary batch body is truncated".to_string(),
        ));
    }
    let slice = &body[*cursor..end];
    *cursor = end;
    Ok(slice)
}

fn read_binary_batch_u16(body: &[u8], cursor: &mut usize) -> Result<u16, (StatusCode, String)> {
    let bytes = take_binary_batch_bytes(body, cursor, 2)?;
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn read_binary_batch_u32(body: &[u8], cursor: &mut usize) -> Result<u32, (StatusCode, String)> {
    let bytes = take_binary_batch_bytes(body, cursor, 4)?;
    Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_binary_batch_u64(body: &[u8], cursor: &mut usize) -> Result<u64, (StatusCode, String)> {
    let bytes = take_binary_batch_bytes(body, cursor, 8)?;
    Ok(u64::from_be_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]))
}

fn parse_binary_batch_upload(
    body: &[u8],
) -> Result<Vec<DecodedBatchUploadBlob>, (StatusCode, String)> {
    let mut cursor = 0usize;
    let magic = take_binary_batch_bytes(body, &mut cursor, BINARY_BATCH_UPLOAD_MAGIC.len())?;
    if magic != BINARY_BATCH_UPLOAD_MAGIC {
        return Err((
            StatusCode::BAD_REQUEST,
            "Invalid binary batch magic".to_string(),
        ));
    }

    let count = read_binary_batch_u32(body, &mut cursor)? as usize;
    if count == 0 {
        return Err((StatusCode::BAD_REQUEST, "Batch is empty".to_string()));
    }
    if count > MAX_BATCH_UPLOAD_BLOBS {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            "Batch contains too many blobs".to_string(),
        ));
    }

    let mut total_bytes = 0usize;
    let mut blobs = Vec::with_capacity(count);
    for _ in 0..count {
        let hash = take_binary_batch_bytes(body, &mut cursor, 32)?;
        let content_type_len = read_binary_batch_u16(body, &mut cursor)? as usize;
        if content_type_len > MAX_BINARY_BATCH_CONTENT_TYPE_BYTES {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                "Binary batch content type is too long".to_string(),
            ));
        }
        let data_len =
            usize::try_from(read_binary_batch_u64(body, &mut cursor)?).map_err(|_| {
                (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "Binary batch blob is too large".to_string(),
                )
            })?;
        total_bytes = total_bytes.checked_add(data_len).ok_or_else(|| {
            (
                StatusCode::PAYLOAD_TOO_LARGE,
                "Batch exceeds maximum upload size".to_string(),
            )
        })?;
        if total_bytes > MAX_BATCH_UPLOAD_BYTES {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                "Batch exceeds maximum upload size".to_string(),
            ));
        }

        let content_type = if content_type_len == 0 {
            None
        } else {
            let bytes = take_binary_batch_bytes(body, &mut cursor, content_type_len)?;
            Some(
                std::str::from_utf8(bytes)
                    .map_err(|_| {
                        (
                            StatusCode::BAD_REQUEST,
                            "Binary batch content type is not UTF-8".to_string(),
                        )
                    })?
                    .to_string(),
            )
        };
        let data = take_binary_batch_bytes(body, &mut cursor, data_len)?.to_vec();
        blobs.push(DecodedBatchUploadBlob {
            sha256: hex::encode(hash),
            content_type,
            data,
        });
    }

    if cursor != body.len() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Binary batch body has trailing bytes".to_string(),
        ));
    }

    Ok(blobs)
}

fn blossom_auth_error_response(status: StatusCode, reason: &'static str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header("X-Reason", reason)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(format!(r#"{{"error":"{}"}}"#, reason)))
        .unwrap()
}

fn verify_upload_batch_auth(headers: &HeaderMap) -> Result<BlossomAuth, Box<Response<Body>>> {
    verify_blossom_auth(headers, "upload", None)
        .map_err(|(status, reason)| Box::new(blossom_auth_error_response(status, reason)))
}

pub async fn require_upload_auth_middleware(request: Request, next: Next) -> Response<Body> {
    if let Err(response) = verify_upload_batch_auth(request.headers()) {
        return *response;
    }
    next.run(request).await
}

async fn upload_decoded_blob_batch(
    state: AppState,
    headers: HeaderMap,
    auth: BlossomAuth,
    blobs: Vec<DecodedBatchUploadBlob>,
    started_at: Instant,
    encoding: &'static str,
) -> Response<Body> {
    let slow_log_ms = slow_batch_upload_log_ms();
    let payload_blobs = blobs.len();
    if blobs.is_empty() {
        return blossom_json_error(StatusCode::BAD_REQUEST, "Batch is empty");
    }
    if blobs.len() > MAX_BATCH_UPLOAD_BLOBS {
        return blossom_json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Batch contains too many blobs",
        );
    }

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
    let mut items = Vec::with_capacity(payload_blobs);
    let mut replica_specs = Vec::with_capacity(payload_blobs);
    let mut descriptors = Vec::with_capacity(payload_blobs);
    let mut decode_hash_ms = 0u128;
    let mut validate_ms = 0u128;

    if auth.blob_hashes.is_empty() && !auth.batch_hashes.is_empty() {
        let batch_hashes = blobs
            .iter()
            .map(|blob| blob.sha256.to_lowercase())
            .collect::<Vec<_>>();
        let batch_digest =
            match batch_upload_hash_list_digest(batch_hashes.iter().map(String::as_str)) {
                Ok(digest) => digest,
                Err(_) => return blossom_json_error(StatusCode::BAD_REQUEST, "Invalid blob hash"),
            };
        if !auth.batch_hashes.contains(&batch_digest) {
            return blossom_json_error(
                StatusCode::FORBIDDEN,
                "Batch hash list does not match authorization",
            );
        }
    }

    for blob in blobs {
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

        let data = blob.data;
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

        descriptors.push(make_blob_descriptor(
            &headers,
            sha256_hex.clone(),
            data.len() as u64,
            content_type.clone(),
            now,
        ));
        replica_specs.push((sha256_hex, content_type));
        items.push((actual_hash, data));
    }
    let prepare_ms = started_at.elapsed().as_millis();

    let store = state.store.clone();
    let store_started = Instant::now();
    let stored = run_blob_write(move || {
        let report = if is_allowed_author {
            store.put_owned_blobs_report(&items, &pubkey_bytes)
        } else {
            store.put_cached_blobs_report(&items)
        }?;
        Ok::<_, anyhow::Error>((report, items))
    })
    .await
    .map_err(blob_io_write_error);
    let store_ms = store_started.elapsed().as_millis();
    let total_ms = started_at.elapsed().as_millis();

    match stored {
        Ok(Ok((report, items))) => {
            for ((sha256_hex, _), (_, data)) in replica_specs.iter().zip(&items) {
                state
                    .blob_cache
                    .put_size(sha256_hex.clone(), Some(data.len() as u64));
                state.blob_cache.put_body(sha256_hex.clone(), data);
            }
            let uploaded = report.inserted;
            if uploaded > 0 {
                let inserted: HashSet<_> = report.inserted_hashes.iter().copied().collect();
                let replica_items = replica_specs
                    .iter()
                    .zip(items.iter())
                    .filter(|&((_, _), (hash, _))| inserted.contains(hash))
                    .map(|((sha256_hex, content_type), (_, data))| {
                        replica_item(sha256_hex.clone(), data.clone(), content_type.clone())
                    })
                    .collect::<Vec<_>>();
                let replication =
                    usize::try_from(report.inserted_bytes)
                        .ok()
                        .and_then(|inserted_bytes| {
                            prepare_blossom_upload_replication(&state, inserted_bytes)
                        });
                if let Some(replication) = replication {
                    schedule_prepared_blossom_upload_replication(
                        &state,
                        replication,
                        replica_items,
                    );
                }
            }
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
                    encoding,
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
        Ok(Err(error)) => blob_write_error_response(error.into()),
        Err(error) => blob_write_error_response(error),
    }
}

/// POST /upload/batch - Upload multiple blobs with one auth event and one storage batch.
pub async fn upload_blob_batch(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<BatchUploadRequest>,
) -> impl IntoResponse {
    let started_at = Instant::now();
    let auth = match verify_upload_batch_auth(&headers) {
        Ok(auth) => auth,
        Err(response) => return *response,
    };
    let mut blobs = Vec::with_capacity(payload.blobs.len());
    for blob in payload.blobs {
        let data = match base64::engine::general_purpose::STANDARD.decode(blob.data.as_bytes()) {
            Ok(data) => data,
            Err(_) => return blossom_json_error(StatusCode::BAD_REQUEST, "Invalid blob data"),
        };
        blobs.push(DecodedBatchUploadBlob {
            sha256: blob.sha256,
            content_type: blob.content_type,
            data,
        });
    }
    upload_decoded_blob_batch(state, headers, auth, blobs, started_at, "json").await
}

/// POST /upload/batch-binary - Upload a binary encoded blob batch.
pub async fn upload_blob_batch_binary(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let started_at = Instant::now();
    let auth = match verify_upload_batch_auth(&headers) {
        Ok(auth) => auth,
        Err(response) => return *response,
    };
    let blobs = match parse_binary_batch_upload(&body) {
        Ok(blobs) => blobs,
        Err((status, reason)) => return blossom_json_error(status, reason),
    };
    upload_decoded_blob_batch(state, headers, auth, blobs, started_at, "binary").await
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

    // Check ownership off the async runtime. Keep both lookups in one admitted
    // metadata task so a delete cannot amplify blocking-pool pressure.
    let store = state.store.clone();
    let ownership = run_blob_metadata_read(move || {
        let is_owner = store.is_blob_owner(&sha256_bytes, &pubkey_bytes)?;
        let has_owners = if is_owner {
            true
        } else {
            store.blob_has_owners(&sha256_bytes)?
        };
        Ok::<_, anyhow::Error>((is_owner, has_owners))
    })
    .await;
    match ownership {
        Ok(Ok((true, _))) => {}
        Ok(Ok((false, true))) => {
            return Response::builder()
                .status(StatusCode::FORBIDDEN)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .header("X-Reason", "Not a blob owner")
                .body(Body::empty())
                .unwrap();
        }
        Ok(Ok((false, false))) => {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .header(header::CACHE_CONTROL, NOT_FOUND_CACHE_CONTROL)
                .header("X-Reason", "Blob not found")
                .body(Body::empty())
                .unwrap();
        }
        Ok(Err(_)) | Err(_) => {
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::empty())
                .unwrap();
        }
    }

    // Remove this user's ownership (blob only deleted when no owners remain)
    let store = state.store.clone();
    match run_blob_write(move || store.delete_blossom_blob(&sha256_bytes, &pubkey_bytes)).await {
        Ok(Ok(fully_deleted)) => {
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
        Ok(Err(_)) | Err(_) => Response::builder()
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
                .unwrap();
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

    // Get blobs for this pubkey without blocking an async runtime worker.
    let store = state.store.clone();
    match run_blob_metadata_read(move || store.list_blobs_by_pubkey(&pubkey_bytes)).await {
        Ok(Ok(blobs)) => {
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
            filtered.sort_by_key(|descriptor| std::cmp::Reverse(descriptor.uploaded));

            // Apply limit
            let limit = query.limit.unwrap_or(100).min(1000);
            filtered.truncate(limit);

            let descriptors: Vec<_> = filtered
                .into_iter()
                .map(|mut descriptor| {
                    descriptor.url =
                        blossom_blob_url(&headers, &descriptor.sha256, &descriptor.mime_type);
                    descriptor
                })
                .collect();

            Response::builder()
                .status(StatusCode::OK)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_string(&descriptors).unwrap()))
                .unwrap()
        }
        Ok(Err(_)) | Err(_) => Response::builder()
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
        _ => ".bin",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::auth::WsRelayState;
    use crate::storage::HashtreeStore;
    use crate::test_support::{test_env_lock, EnvVarGuard};
    use axum::response::IntoResponse;
    use axum::{routing::post, Router};
    use base64::Engine;
    use hashtree_config::StorageBackend;
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
            fips_endpoint: None,
            fips_blob_resolver: None,
            fetch_from_fips_peers: true,
            ws_relay: Arc::new(WsRelayState::new()),
            max_upload_bytes: 5 * 1024 * 1024,
            public_writes: true,
            public_plaintext_reads: true,
            require_random_untrusted_ingest: true,
            optimistic_blossom_uploads: false,
            optimistic_upload_queue_bytes: 256 * 1024 * 1024,
            optimistic_upload_queue: Arc::new(tokio::sync::Semaphore::new(256 * 1024 * 1024)),
            allowed_pubkeys: HashSet::new(),
            upstream_blossom: Vec::new(),
            upstream_http_client: super::super::new_upstream_http_client(),
            upstream_blossom_miss_cache: Arc::new(StdMutex::new(crate::server::new_lookup_cache())),
            upstream_blossom_fetch_metrics: Arc::new(
                crate::server::auth::UpstreamBlossomFetchMetrics::default(),
            ),
            blossom_upload_replicas: Vec::new(),
            blossom_upload_replica_queue_bytes: 256 * 1024 * 1024,
            blossom_upload_replica_queue: Arc::new(tokio::sync::Semaphore::new(256 * 1024 * 1024)),
            blossom_upload_replica_keys: None,
            blossom_upload_replica_scheduler: Arc::new(BlossomUploadReplicaScheduler::new()),
            social_graph: None,
            social_graph_store: None,
            social_graph_root: None,
            socialgraph_snapshot_public: false,
            nostr_relay: None,
            nostr_provider: None,
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

    async fn receive_replication<T>(receiver: &mut tokio::sync::mpsc::UnboundedReceiver<T>) -> T {
        tokio::time::timeout(Duration::from_secs(10), receiver.recv())
            .await
            .expect("replication request timed out")
            .expect("replication channel closed")
    }

    fn create_upload_auth_header(keys: &nostr::Keys) -> String {
        use nostr::{EventBuilder, Kind, Tag, TagKind, Timestamp};

        let now = Timestamp::now();
        let event = EventBuilder::new(Kind::Custom(BLOSSOM_AUTH_KIND), "")
            .tags(vec![
                Tag::custom(TagKind::Custom("t".into()), vec!["upload".to_string()]),
                Tag::custom(
                    TagKind::Custom("expiration".into()),
                    vec![(now.as_secs() + 300).to_string()],
                ),
            ])
            .custom_created_at(now)
            .sign_with_keys(keys)
            .expect("sign blossom auth");
        let json = serde_json::to_vec(&event).expect("serialize auth event");
        format!(
            "Nostr {}",
            base64::engine::general_purpose::STANDARD.encode(json)
        )
    }

    fn create_batch_upload_auth_header(keys: &nostr::Keys, hashes: &[String]) -> String {
        use nostr::{EventBuilder, Kind, Tag, TagKind, Timestamp};

        let now = Timestamp::now();
        let event = EventBuilder::new(Kind::Custom(BLOSSOM_AUTH_KIND), "")
            .tags(vec![
                Tag::custom(TagKind::Custom("t".into()), vec!["upload".to_string()]),
                Tag::custom(
                    TagKind::Custom(BATCH_UPLOAD_HASH_LIST_AUTH_TAG.into()),
                    vec![
                        batch_upload_hash_list_digest(hashes.iter().map(String::as_str))
                            .expect("batch hash list digest"),
                    ],
                ),
                Tag::custom(
                    TagKind::Custom("expiration".into()),
                    vec![(now.as_secs() + 300).to_string()],
                ),
            ])
            .custom_created_at(now)
            .sign_with_keys(keys)
            .expect("sign blossom batch auth");
        let json = serde_json::to_vec(&event).expect("serialize batch auth event");
        format!(
            "Nostr {}",
            base64::engine::general_purpose::STANDARD.encode(json)
        )
    }

    fn create_list_auth_header(keys: &nostr::Keys) -> String {
        use nostr::{EventBuilder, Kind, Tag, TagKind, Timestamp};

        let now = Timestamp::now();
        let event = EventBuilder::new(Kind::Custom(BLOSSOM_AUTH_KIND), "")
            .tags(vec![
                Tag::custom(TagKind::Custom("t".into()), vec!["list".to_string()]),
                Tag::custom(
                    TagKind::Custom("expiration".into()),
                    vec![(now.as_secs() + 300).to_string()],
                ),
            ])
            .custom_created_at(now)
            .sign_with_keys(keys)
            .expect("sign blossom list auth");
        let json = serde_json::to_vec(&event).expect("serialize list auth event");
        format!(
            "Nostr {}",
            base64::engine::general_purpose::STANDARD.encode(json)
        )
    }

    fn hosted_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "origin.internal".parse().unwrap());
        headers.insert("x-forwarded-host", "cdn.iris.to".parse().unwrap());
        headers.insert("x-forwarded-proto", "https".parse().unwrap());
        headers
    }

    async fn read_descriptor(response: axum::response::Response) -> BlobDescriptor {
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read descriptor body");
        serde_json::from_slice(&body).expect("parse descriptor")
    }

    fn upload_check_bits(response: UploadCheckResponse) -> Vec<bool> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(response.present)
            .expect("decode upload check bitset");
        (0..response.count)
            .map(|index| bytes[index / 8] & (1 << (index % 8)) != 0)
            .collect()
    }

    fn binary_batch_body(items: &[(&[u8], Option<&str>)]) -> Bytes {
        let mut body = Vec::new();
        body.extend_from_slice(BINARY_BATCH_UPLOAD_MAGIC);
        body.extend_from_slice(&(items.len() as u32).to_be_bytes());
        for (data, content_type) in items {
            body.extend_from_slice(&sha256(data));
            let content_type = content_type.unwrap_or("");
            body.extend_from_slice(&(content_type.len() as u16).to_be_bytes());
            body.extend_from_slice(&(*data).len().to_be_bytes());
            body.extend_from_slice(content_type.as_bytes());
            body.extend_from_slice(data);
        }
        Bytes::from(body)
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
        assert_eq!(mime_to_extension("application/octet-stream"), ".bin");
        assert_eq!(mime_to_extension("unknown/type"), ".bin");
    }

    #[test]
    fn blossom_blob_url_uses_forwarded_public_origin_and_extension() {
        let headers = hosted_headers();
        let hash = "00".repeat(32);

        assert_eq!(
            blossom_blob_url(&headers, &hash, "application/octet-stream"),
            format!("https://cdn.iris.to/{hash}.bin")
        );
        assert_eq!(
            blossom_blob_url(&headers, &hash, "image/png"),
            format!("https://cdn.iris.to/{hash}.png")
        );
    }

    #[tokio::test]
    async fn upload_check_reports_present_hashes_in_request_order() {
        let temp_dir = TempDir::new().expect("temp dir");
        let store = Arc::new(
            HashtreeStore::with_options(temp_dir.path(), None, 128 * 1024 * 1024).expect("store"),
        );

        let present = b"present blob";
        let missing = b"missing blob";
        let present_hash = sha256(present);
        let missing_hash = sha256(missing);
        store.put_cached_blob(present).expect("seed blob");

        let state = test_app_state(store);
        let response = upload_check(
            State(state),
            Json(UploadCheckRequest {
                hashes: vec![
                    hex::encode(missing_hash),
                    hex::encode(present_hash),
                    hex::encode(present_hash),
                ],
            }),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read response body");
        let parsed: UploadCheckResponse =
            serde_json::from_slice(&body).expect("parse upload check response");
        assert_eq!(upload_check_bits(parsed), vec![false, true, true]);
    }

    #[tokio::test]
    async fn upload_check_rejects_invalid_hash() {
        let temp_dir = TempDir::new().expect("temp dir");
        let store = Arc::new(
            HashtreeStore::with_options(temp_dir.path(), None, 128 * 1024 * 1024).expect("store"),
        );
        let state = test_app_state(store);
        let response = upload_check(
            State(state),
            Json(UploadCheckRequest {
                hashes: vec!["not-a-sha256".to_string()],
            }),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn upload_check_rejects_too_many_hashes() {
        let temp_dir = TempDir::new().expect("temp dir");
        let store = Arc::new(
            HashtreeStore::with_options(temp_dir.path(), None, 128 * 1024 * 1024).expect("store"),
        );
        let state = test_app_state(store);
        let response = upload_check(
            State(state),
            Json(UploadCheckRequest {
                hashes: vec!["00".repeat(32); MAX_UPLOAD_CHECK_HASHES + 1],
            }),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn upload_blob_batch_binary_replaces_cached_misses_after_commit() {
        let temp_dir = TempDir::new().expect("temp dir");
        let store = Arc::new(
            HashtreeStore::with_options(temp_dir.path(), None, 128 * 1024 * 1024).expect("store"),
        );
        let state = test_app_state(Arc::clone(&store));
        let keys = nostr::Keys::generate();
        let mut headers = hosted_headers();
        headers.insert(
            header::AUTHORIZATION,
            create_upload_auth_header(&keys)
                .parse()
                .expect("auth header value"),
        );
        headers.insert(
            header::CONTENT_TYPE,
            "application/vnd.hashtree.blossom.batch.v1"
                .parse()
                .expect("content type header value"),
        );

        let first = (0u8..=255).collect::<Vec<_>>();
        let second = (0u8..=255).map(|byte| byte ^ 0xaa).collect::<Vec<_>>();
        let first_hash = sha256(&first);
        let second_hash = sha256(&second);
        let body = binary_batch_body(&[
            (&first, Some("application/octet-stream")),
            (&second, Some("application/octet-stream")),
        ]);

        let first_hash_hex = hex::encode(first_hash);
        let client = axum::extract::ConnectInfo("127.0.0.1:12345".parse().unwrap());
        let miss = head_blob(
            State(state.clone()),
            Path(format!("{first_hash_hex}.bin")),
            client,
        )
        .await
        .into_response();
        assert_eq!(miss.status(), StatusCode::NOT_FOUND);

        let response = upload_blob_batch_binary(State(state.clone()), headers, body)
            .await
            .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read batch response");
        let parsed: BatchUploadResponse =
            serde_json::from_slice(&body).expect("parse batch response");
        assert_eq!(parsed.uploaded, 2);
        assert_eq!(parsed.blobs.len(), 2);
        assert_eq!(parsed.blobs[0].sha256, hex::encode(first_hash));
        assert_eq!(parsed.blobs[1].sha256, hex::encode(second_hash));
        assert!(store.blob_exists(&first_hash).expect("first exists"));
        assert!(store.blob_exists(&second_hash).expect("second exists"));

        let immediate_head = head_blob(State(state), Path(format!("{first_hash_hex}.bin")), client)
            .await
            .into_response();
        assert_eq!(
            immediate_head.status(),
            StatusCode::OK,
            "a committed batch write must replace a cached preflight miss",
        );
    }

    #[tokio::test]
    async fn upload_blob_batch_binary_accepts_compact_batch_auth() {
        let temp_dir = TempDir::new().expect("temp dir");
        let store = Arc::new(
            HashtreeStore::with_options(temp_dir.path(), None, 128 * 1024 * 1024).expect("store"),
        );
        let state = test_app_state(Arc::clone(&store));
        let keys = nostr::Keys::generate();
        let first = (0u8..=255).collect::<Vec<_>>();
        let second = (0u8..=255).map(|byte| byte ^ 0xaa).collect::<Vec<_>>();
        let first_hash = sha256(&first);
        let second_hash = sha256(&second);
        let hashes = vec![hex::encode(first_hash), hex::encode(second_hash)];
        let mut headers = hosted_headers();
        headers.insert(
            header::AUTHORIZATION,
            create_batch_upload_auth_header(&keys, &hashes)
                .parse()
                .expect("auth header value"),
        );
        headers.insert(
            header::CONTENT_TYPE,
            "application/vnd.hashtree.blossom.batch.v1"
                .parse()
                .expect("content type header value"),
        );
        let body = binary_batch_body(&[
            (&first, Some("application/octet-stream")),
            (&second, Some("application/octet-stream")),
        ]);

        let response = upload_blob_batch_binary(State(state), headers, body)
            .await
            .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(store.blob_exists(&first_hash).expect("first exists"));
        assert!(store.blob_exists(&second_hash).expect("second exists"));
    }

    #[tokio::test]
    async fn upload_blob_batch_binary_rejects_mismatched_compact_batch_auth() {
        let temp_dir = TempDir::new().expect("temp dir");
        let store = Arc::new(
            HashtreeStore::with_options(temp_dir.path(), None, 128 * 1024 * 1024).expect("store"),
        );
        let state = test_app_state(Arc::clone(&store));
        let keys = nostr::Keys::generate();
        let first = (0u8..=255).collect::<Vec<_>>();
        let second = (0u8..=255).map(|byte| byte ^ 0xaa).collect::<Vec<_>>();
        let wrong_hashes = vec![hex::encode(sha256(&first)), "00".repeat(32)];
        let mut headers = hosted_headers();
        headers.insert(
            header::AUTHORIZATION,
            create_batch_upload_auth_header(&keys, &wrong_hashes)
                .parse()
                .expect("auth header value"),
        );
        headers.insert(
            header::CONTENT_TYPE,
            "application/vnd.hashtree.blossom.batch.v1"
                .parse()
                .expect("content type header value"),
        );
        let body = binary_batch_body(&[
            (&first, Some("application/octet-stream")),
            (&second, Some("application/octet-stream")),
        ]);

        let response = upload_blob_batch_binary(State(state), headers, body)
            .await
            .into_response();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(!store.blob_exists(&sha256(&first)).expect("first absent"));
        assert!(!store.blob_exists(&sha256(&second)).expect("second absent"));
    }

    #[tokio::test]
    async fn upload_blob_batch_rejects_missing_auth_before_decoding_payload() {
        let temp_dir = TempDir::new().expect("temp dir");
        let store = Arc::new(
            HashtreeStore::with_options(temp_dir.path(), None, 128 * 1024 * 1024).expect("store"),
        );
        let state = test_app_state(store);
        let payload = BatchUploadRequest {
            blobs: vec![BatchUploadBlob {
                sha256: "00".repeat(32),
                content_type: None,
                data: "not-base64".to_string(),
            }],
        };

        let response = upload_blob_batch(State(state), HeaderMap::new(), Json(payload))
            .await
            .into_response();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn upload_blob_batch_binary_rejects_missing_auth_before_parsing_body() {
        let temp_dir = TempDir::new().expect("temp dir");
        let store = Arc::new(
            HashtreeStore::with_options(temp_dir.path(), None, 128 * 1024 * 1024).expect("store"),
        );
        let state = test_app_state(store);

        let response = upload_blob_batch_binary(
            State(state),
            HeaderMap::new(),
            Bytes::from_static(b"not a binary batch"),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn upload_blob_batch_binary_replicates_only_new_blobs() {
        let _lock = test_env_lock().lock().await;
        let config_dir = TempDir::new().expect("config dir");
        let _guard = EnvVarGuard::set("HTREE_CONFIG_DIR", config_dir.path());

        let first = (0u8..=255).collect::<Vec<_>>();
        let second = (0u8..=255).map(|byte| byte ^ 0x55).collect::<Vec<_>>();
        let first_hash = sha256(&first);
        let second_hash = sha256(&second);
        let second_hash_hex = hex::encode(second_hash);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<String>>();
        let replica_router = Router::new().route(
            "/upload/batch-binary",
            post(move |body: Bytes| {
                let tx = tx.clone();
                async move {
                    let blobs = parse_binary_batch_upload(&body).expect("parse replica batch");
                    let hashes = blobs
                        .iter()
                        .map(|blob| blob.sha256.clone())
                        .collect::<Vec<_>>();
                    let _ = tx.send(hashes.clone());
                    Json(serde_json::json!({
                        "uploaded": hashes.len(),
                        "blobs": hashes.into_iter().map(|sha256| serde_json::json!({ "sha256": sha256 })).collect::<Vec<_>>(),
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind replica");
        let replica_addr = listener.local_addr().expect("replica addr");
        let _server_task =
            tokio::spawn(async move { axum::serve(listener, replica_router).await.unwrap() });

        let temp_dir = TempDir::new().expect("temp dir");
        let store = Arc::new(
            HashtreeStore::with_options(temp_dir.path(), None, 128 * 1024 * 1024).expect("store"),
        );
        store
            .put_cached_blobs(&[(first_hash, first.clone())])
            .expect("prestore first blob");
        let mut state = test_app_state(Arc::clone(&store));
        state.require_random_untrusted_ingest = false;
        state.blossom_upload_replicas = vec![format!("http://{replica_addr}")];
        state.blossom_upload_replica_keys = Some(Arc::new(nostr::Keys::generate()));

        let keys = nostr::Keys::generate();
        let mut headers = hosted_headers();
        headers.insert(
            header::AUTHORIZATION,
            create_upload_auth_header(&keys)
                .parse()
                .expect("auth header value"),
        );
        headers.insert(
            header::CONTENT_TYPE,
            "application/vnd.hashtree.blossom.batch.v1"
                .parse()
                .expect("content type header value"),
        );
        let body = binary_batch_body(&[
            (&first, Some("application/octet-stream")),
            (&second, Some("application/octet-stream")),
        ]);

        let response = upload_blob_batch_binary(State(state), headers, body)
            .await
            .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read batch response");
        let parsed: BatchUploadResponse =
            serde_json::from_slice(&body).expect("parse batch response");
        assert_eq!(parsed.uploaded, 1);
        let replicated = receive_replication(&mut rx).await;
        assert_eq!(replicated, vec![second_hash_hex]);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), rx.recv())
                .await
                .is_err(),
            "duplicate blob should not trigger a second replication batch"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn upload_replication_coalesces_adjacent_binary_batches() {
        let _lock = test_env_lock().lock().await;
        let config_dir = TempDir::new().expect("config dir");
        let _guard = EnvVarGuard::set("HTREE_CONFIG_DIR", config_dir.path());
        let _flush_guard = EnvVarGuard::set("HTREE_BLOSSOM_REPLICA_COALESCE_FLUSH_MS", "2000");
        let _blobs_guard = EnvVarGuard::set("HTREE_BLOSSOM_REPLICA_COALESCE_MAX_BLOBS", "8");
        let _bytes_guard = EnvVarGuard::set("HTREE_BLOSSOM_REPLICA_COALESCE_MAX_BYTES", "1048576");

        let first_a = b"coalesced-replication-first-a".to_vec();
        let first_b = b"coalesced-replication-first-b".to_vec();
        let second_a = b"coalesced-replication-second-a".to_vec();
        let second_b = b"coalesced-replication-second-b".to_vec();
        let expected_hashes = [&first_a, &first_b, &second_a, &second_b]
            .into_iter()
            .map(|data| hex::encode(sha256(data)))
            .collect::<HashSet<_>>();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<String>>();
        let replica_router = Router::new().route(
            "/upload/batch-binary",
            post(move |body: Bytes| {
                let tx = tx.clone();
                async move {
                    let blobs = parse_binary_batch_upload(&body).expect("parse replica batch");
                    let hashes = blobs
                        .iter()
                        .map(|blob| blob.sha256.clone())
                        .collect::<Vec<_>>();
                    let _ = tx.send(hashes.clone());
                    Json(serde_json::json!({
                        "uploaded": hashes.len(),
                        "blobs": hashes.into_iter().map(|sha256| serde_json::json!({ "sha256": sha256 })).collect::<Vec<_>>(),
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind replica");
        let replica_addr = listener.local_addr().expect("replica addr");
        let _server_task =
            tokio::spawn(async move { axum::serve(listener, replica_router).await.unwrap() });

        let temp_dir = TempDir::new().expect("temp dir");
        let store = Arc::new(
            HashtreeStore::with_options(temp_dir.path(), None, 128 * 1024 * 1024).expect("store"),
        );
        let mut state = test_app_state(store);
        state.require_random_untrusted_ingest = false;
        state.blossom_upload_replicas = vec![format!("http://{replica_addr}")];
        state.blossom_upload_replica_keys = Some(Arc::new(nostr::Keys::generate()));
        let metrics_before = blossom_upload_replica_queue_snapshot(&state);

        let keys = nostr::Keys::generate();
        let mut headers = hosted_headers();
        headers.insert(
            header::AUTHORIZATION,
            create_upload_auth_header(&keys)
                .parse()
                .expect("auth header value"),
        );
        headers.insert(
            header::CONTENT_TYPE,
            "application/vnd.hashtree.blossom.batch.v1"
                .parse()
                .expect("content type header value"),
        );
        let first_body = binary_batch_body(&[
            (&first_a, Some("application/octet-stream")),
            (&first_b, Some("application/octet-stream")),
        ]);
        let second_body = binary_batch_body(&[
            (&second_a, Some("application/octet-stream")),
            (&second_b, Some("application/octet-stream")),
        ]);

        let first_response =
            upload_blob_batch_binary(State(state.clone()), headers.clone(), first_body)
                .await
                .into_response();
        assert_eq!(first_response.status(), StatusCode::OK);
        let second_response = upload_blob_batch_binary(State(state.clone()), headers, second_body)
            .await
            .into_response();
        assert_eq!(second_response.status(), StatusCode::OK);

        let replicated = receive_replication(&mut rx).await;
        let replicated_hashes = replicated.into_iter().collect::<HashSet<_>>();
        assert_eq!(replicated_hashes, expected_hashes);
        assert!(
            tokio::time::timeout(Duration::from_millis(150), rx.recv())
                .await
                .is_err(),
            "adjacent batches should be merged into one replica request"
        );
        let metrics_after = blossom_upload_replica_queue_snapshot(&state);
        assert!(
            metrics_after.accepted_batches > metrics_before.accepted_batches,
            "coalesced replication should increment accepted batch metrics"
        );
        assert!(
            metrics_after.accepted_blobs >= metrics_before.accepted_blobs + 4,
            "coalesced replication should increment accepted blob metrics"
        );
    }

    #[test]
    fn binary_batch_parser_rejects_trailing_bytes() {
        let data = (0u8..=255).collect::<Vec<_>>();
        let mut body = binary_batch_body(&[(&data, None)]).to_vec();
        body.push(0);

        let error = parse_binary_batch_upload(&body).expect_err("trailing bytes rejected");

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert!(error.1.contains("trailing"));
    }

    #[tokio::test]
    async fn head_upload_accepts_valid_bud06_preflight() {
        let temp_dir = TempDir::new().expect("temp dir");
        let store = Arc::new(
            HashtreeStore::with_options(temp_dir.path(), None, 128 * 1024 * 1024).expect("store"),
        );
        let state = test_app_state(store);
        let mut headers = hosted_headers();
        headers.insert("x-sha-256", "00".repeat(32).parse().unwrap());
        headers.insert("x-content-length", "16".parse().unwrap());
        headers.insert(
            "x-content-type",
            "application/octet-stream".parse().unwrap(),
        );

        let response = head_upload(State(state), headers).await.into_response();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn upload_blob_returns_bud02_statuses_and_public_descriptor_url() {
        let temp_dir = TempDir::new().expect("temp dir");
        let store = Arc::new(
            HashtreeStore::with_options(temp_dir.path(), None, 128 * 1024 * 1024).expect("store"),
        );
        let state = test_app_state(store);
        let keys = nostr::Keys::generate();
        let mut headers = hosted_headers();
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

        let body = axum::body::Bytes::from((0u8..=255).collect::<Vec<_>>());
        let hash_hex = hex::encode(sha256(&body));
        let first = upload_blob(State(state.clone()), headers.clone(), body.clone())
            .await
            .into_response();
        assert_eq!(first.status(), StatusCode::CREATED);
        let first_descriptor = read_descriptor(first).await;
        assert_eq!(
            first_descriptor.url,
            format!("https://cdn.iris.to/{hash_hex}.bin")
        );
        assert_eq!(first_descriptor.sha256, hash_hex);

        let second = upload_blob(State(state), headers, body)
            .await
            .into_response();
        assert_eq!(second.status(), StatusCode::OK);
        let second_descriptor = read_descriptor(second).await;
        assert_eq!(second_descriptor.url, first_descriptor.url);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn upload_blob_replicates_to_configured_blossom_target() {
        let _lock = test_env_lock().lock().await;
        let config_dir = TempDir::new().expect("config dir");
        let _guard = EnvVarGuard::set("HTREE_CONFIG_DIR", config_dir.path());

        let data = Bytes::from_static(b"write-behind-replication-data");
        let expected_hash = hex::encode(sha256(data.as_ref()));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<usize>();
        let response_hash = expected_hash.clone();
        let replica_router = Router::new().route(
            "/upload/batch-binary",
            post(move |body: Bytes| {
                let tx = tx.clone();
                let response_hash = response_hash.clone();
                async move {
                    let _ = tx.send(body.len());
                    Json(serde_json::json!({
                        "uploaded": 1,
                        "blobs": [{"sha256": response_hash}],
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind replica");
        let replica_addr = listener.local_addr().expect("replica addr");
        let _server_task =
            tokio::spawn(async move { axum::serve(listener, replica_router).await.unwrap() });

        let temp = TempDir::new().expect("tempdir");
        let store = Arc::new(HashtreeStore::new(temp.path()).expect("store"));
        let mut state = test_app_state(store);
        state.require_random_untrusted_ingest = false;
        state.blossom_upload_replicas = vec![format!("http://{replica_addr}")];
        state.blossom_upload_replica_keys = Some(Arc::new(nostr::Keys::generate()));

        let keys = nostr::Keys::generate();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            create_upload_auth_header(&keys).parse().unwrap(),
        );
        headers.insert(
            header::CONTENT_TYPE,
            "application/octet-stream".parse().unwrap(),
        );

        let response = upload_blob(State(state), headers, data)
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::CREATED);
        let replicated = receive_replication(&mut rx).await;
        assert!(replicated > 0);
        assert_eq!(expected_hash.len(), 64);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn upload_blob_duplicate_does_not_replicate_to_configured_blossom_target() {
        let _lock = test_env_lock().lock().await;
        let config_dir = TempDir::new().expect("config dir");
        let _guard = EnvVarGuard::set("HTREE_CONFIG_DIR", config_dir.path());

        let data = Bytes::from_static(b"write-behind-duplicate-raw-data");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<usize>();
        let response_hash = hex::encode(sha256(data.as_ref()));
        let replica_router = Router::new().route(
            "/upload/batch-binary",
            post(move |body: Bytes| {
                let tx = tx.clone();
                let response_hash = response_hash.clone();
                async move {
                    let _ = tx.send(body.len());
                    Json(serde_json::json!({
                        "uploaded": 1,
                        "blobs": [{"sha256": response_hash}],
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind replica");
        let replica_addr = listener.local_addr().expect("replica addr");
        let _server_task =
            tokio::spawn(async move { axum::serve(listener, replica_router).await.unwrap() });

        let temp = TempDir::new().expect("tempdir");
        let store = Arc::new(HashtreeStore::new(temp.path()).expect("store"));
        store.put_cached_blob(&data).expect("seed duplicate blob");
        let mut state = test_app_state(store);
        state.require_random_untrusted_ingest = false;
        state.blossom_upload_replicas = vec![format!("http://{replica_addr}")];
        state.blossom_upload_replica_keys = Some(Arc::new(nostr::Keys::generate()));

        let keys = nostr::Keys::generate();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            create_upload_auth_header(&keys).parse().unwrap(),
        );
        headers.insert(
            header::CONTENT_TYPE,
            "application/octet-stream".parse().unwrap(),
        );

        let response = upload_blob(State(state), headers, data)
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), rx.recv())
                .await
                .is_err(),
            "duplicate raw upload should not trigger write-behind replication"
        );
    }

    #[tokio::test]
    async fn list_blobs_returns_public_descriptor_urls_with_extensions() {
        let temp_dir = TempDir::new().expect("temp dir");
        let store = Arc::new(
            HashtreeStore::with_options(temp_dir.path(), None, 128 * 1024 * 1024).expect("store"),
        );
        let keys = nostr::Keys::generate();
        let pubkey_hex = keys.public_key().to_hex();
        let pubkey_bytes: [u8; 32] = from_hex(&pubkey_hex).expect("pubkey bytes");
        let body = (0u8..=255).collect::<Vec<_>>();
        let hash_hex = store
            .put_owned_blob(&body, &pubkey_bytes)
            .expect("store owned blob");
        let state = test_app_state(store);
        let mut headers = hosted_headers();
        headers.insert(
            header::AUTHORIZATION,
            create_list_auth_header(&keys)
                .parse()
                .expect("auth header value"),
        );

        let response = list_blobs(
            State(state),
            Path(pubkey_hex),
            Query(ListQuery {
                since: None,
                until: None,
                limit: None,
                cursor: None,
            }),
            headers,
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read list body");
        let descriptors: Vec<BlobDescriptor> =
            serde_json::from_slice(&body).expect("parse descriptor list");
        assert_eq!(descriptors.len(), 1);
        assert_eq!(
            descriptors[0].url,
            format!("https://cdn.iris.to/{hash_hex}.bin")
        );
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
        assert_eq!(response.status(), StatusCode::OK);
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
    async fn optimistic_upload_existing_blob_does_not_replicate_duplicate() {
        let _lock = test_env_lock().lock().await;
        let config_dir = TempDir::new().expect("config dir");
        let _guard = EnvVarGuard::set("HTREE_CONFIG_DIR", config_dir.path());

        let temp_dir = TempDir::new().expect("temp dir");
        let store = Arc::new(
            HashtreeStore::with_options(temp_dir.path(), None, 128 * 1024 * 1024).expect("store"),
        );
        let body = axum::body::Bytes::from((0u8..=255).map(|byte| byte ^ 0xaa).collect::<Vec<_>>());
        let hash_hex = hex::encode(sha256(&body));
        store.put_cached_blob(&body).expect("seed blob");

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<usize>();
        let response_hash = hash_hex.clone();
        let replica_router = Router::new().route(
            "/upload/batch-binary",
            post(move |body: Bytes| {
                let tx = tx.clone();
                let response_hash = response_hash.clone();
                async move {
                    let _ = tx.send(body.len());
                    Json(serde_json::json!({
                        "uploaded": 1,
                        "blobs": [{"sha256": response_hash}],
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind replica");
        let replica_addr = listener.local_addr().expect("replica addr");
        let _server_task =
            tokio::spawn(async move { axum::serve(listener, replica_router).await.unwrap() });

        let mut state = test_app_state(store);
        state.optimistic_blossom_uploads = true;
        state.require_random_untrusted_ingest = false;
        state.blossom_upload_replicas = vec![format!("http://{replica_addr}")];
        state.blossom_upload_replica_keys = Some(Arc::new(nostr::Keys::generate()));

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
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        clear_optimistic_upload_inflight(&hash_hex);

        assert!(
            tokio::time::timeout(Duration::from_millis(100), rx.recv())
                .await
                .is_err(),
            "optimistic duplicate upload should not trigger write-behind replication"
        );
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
        let store = Arc::new(
            HashtreeStore::with_options_and_backend(
                temp_dir.path(),
                None,
                700,
                true,
                &StorageBackend::Fs,
            )
            .expect("store"),
        );
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
        let store = Arc::new(
            HashtreeStore::with_options_and_backend(
                temp_dir.path(),
                None,
                500,
                true,
                &StorageBackend::Fs,
            )
            .expect("store"),
        );
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
