//! Blossom protocol client for hashtree
//!
//! Provides upload/download of blobs to Blossom servers with NIP-98 authentication.
//!
//! # Example
//!
//! ```rust,no_run
//! use hashtree_blossom::BlossomClient;
//! use nostr::Keys;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let keys = Keys::generate();
//!     let client = BlossomClient::new(keys)
//!         .with_servers(vec!["https://blossom.example.com".to_string()]);
//!
//!     // Upload
//!     let hash = client.upload(b"hello world").await?;
//!     println!("Uploaded: {}", hash);
//!
//!     // Download
//!     let data = client.download(&hash).await?;
//!     assert_eq!(data, b"hello world");
//!
//!     Ok(())
//! }
//! ```

use base64::Engine;
use nostr::prelude::*;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::time::Duration;
use thiserror::Error;
use tracing::{debug, warn};

const UPLOAD_CHECK_BATCH_SIZE: usize = 10_000;
pub const BATCH_UPLOAD_MAX_BLOBS: usize = 1024;
pub const BATCH_UPLOAD_MAX_BYTES: usize = 64 * 1024 * 1024;

#[derive(Error, Debug)]
pub enum BlossomError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("No servers configured")]
    NoServers,

    #[error("Upload failed: {0}")]
    UploadFailed(String),

    #[error("Download failed on all servers: {0}")]
    DownloadFailed(String),

    #[error("Hash mismatch: expected {expected}, got {actual}")]
    HashMismatch { expected: String, actual: String },

    #[error("Signing error: {0}")]
    Signing(String),
}

/// Result of probing a blob's presence on one Blossom server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobAvailability {
    Present,
    Missing,
    Unknown,
}

/// Blossom protocol client
#[derive(Clone)]
pub struct BlossomClient {
    keys: Keys,
    /// Servers for reading (download)
    read_servers: Vec<String>,
    /// Servers for writing (upload)
    write_servers: Vec<String>,
    http: reqwest::Client,
    timeout: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UploadOutcome {
    Uploaded,
    AlreadyExists,
}

impl UploadOutcome {
    fn was_uploaded(self) -> bool {
        matches!(self, Self::Uploaded)
    }
}

#[derive(Debug)]
struct UploadAttemptError {
    detail: String,
    retryable: bool,
}

#[derive(Serialize)]
struct UploadCheckRequest<'a> {
    hashes: &'a [String],
}

#[derive(Deserialize)]
struct UploadCheckResponse {
    count: usize,
    present: String,
}

#[derive(Debug, Clone)]
pub struct BatchUploadItem {
    pub hash: String,
    pub data: Vec<u8>,
    pub content_type: Option<String>,
}

impl BatchUploadItem {
    pub fn new(hash: String, data: Vec<u8>) -> Self {
        Self {
            hash,
            data,
            content_type: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchUploadResult {
    pub accepted: usize,
    pub uploaded: usize,
}

#[derive(Serialize)]
struct BatchUploadRequest {
    blobs: Vec<BatchUploadBlobRequest>,
}

#[derive(Serialize)]
struct BatchUploadBlobRequest {
    sha256: String,
    #[serde(skip_serializing_if = "Option::is_none", rename = "contentType")]
    content_type: Option<String>,
    data: String,
}

#[derive(Deserialize)]
struct BatchUploadResponse {
    uploaded: usize,
    blobs: Vec<BatchUploadDescriptor>,
}

#[derive(Deserialize)]
struct BatchUploadDescriptor {
    sha256: String,
}

impl UploadAttemptError {
    fn retryable(detail: String) -> Self {
        Self {
            detail,
            retryable: true,
        }
    }

    fn fatal(detail: String) -> Self {
        Self {
            detail,
            retryable: false,
        }
    }
}

impl BlossomClient {
    /// Create a new client with the given keys
    /// Automatically loads server config from ~/.hashtree/config.toml
    /// and prioritizes local daemon if running
    #[cfg(feature = "config")]
    pub fn new(keys: Keys) -> Self {
        let config = hashtree_config::Config::load_or_default();
        let mut read_servers = config.blossom.all_read_servers();

        // Prioritize local daemon if running
        if let Some(local_url) =
            hashtree_config::detect_local_daemon_url(Some(&config.server.bind_address))
        {
            if !read_servers.iter().any(|s| s == &local_url) {
                debug!(
                    "Local daemon detected at {}, prioritizing for reads",
                    local_url
                );
                read_servers.insert(0, local_url);
            }
        }

        Self {
            keys,
            read_servers,
            write_servers: config.blossom.all_write_servers(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap(),
            timeout: Duration::from_secs(30),
        }
    }

    /// Create a new client with the given keys (no config loading)
    #[cfg(not(feature = "config"))]
    pub fn new(keys: Keys) -> Self {
        Self {
            keys,
            read_servers: vec![],
            write_servers: vec![],
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap(),
            timeout: Duration::from_secs(30),
        }
    }

    /// Create a new client without loading config (empty servers)
    pub fn new_empty(keys: Keys) -> Self {
        Self {
            keys,
            read_servers: vec![],
            write_servers: vec![],
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap(),
            timeout: Duration::from_secs(30),
        }
    }

    /// Set the Blossom servers to use (for both read and write)
    pub fn with_servers(mut self, servers: Vec<String>) -> Self {
        self.read_servers = servers.clone();
        self.write_servers = servers;
        self
    }

    /// Set read-only servers (for downloads)
    pub fn with_read_servers(mut self, servers: Vec<String>) -> Self {
        self.read_servers = servers;
        self
    }

    /// Set write servers (for uploads)
    pub fn with_write_servers(mut self, servers: Vec<String>) -> Self {
        self.write_servers = servers;
        self
    }

    /// Set request timeout
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self.http = reqwest::Client::builder().timeout(timeout).build().unwrap();
        self
    }

    /// Set local daemon URL (prioritized for reads)
    /// The local daemon is prepended to read_servers if not already present
    pub fn with_local_daemon(mut self, url: String) -> Self {
        // Don't add if already present
        if !self.read_servers.iter().any(|s| s == &url) {
            self.read_servers.insert(0, url);
        }
        self
    }

    /// Get configured read servers
    pub fn read_servers(&self) -> &[String] {
        &self.read_servers
    }

    /// Get configured write servers
    pub fn write_servers(&self) -> &[String] {
        &self.write_servers
    }

    /// Get configured servers (returns read servers for backwards compatibility)
    pub fn servers(&self) -> &[String] {
        &self.read_servers
    }

    /// Upload data to Blossom servers
    /// Returns the SHA256 hash of the uploaded data
    pub async fn upload(&self, data: &[u8]) -> Result<String, BlossomError> {
        if self.write_servers.is_empty() {
            return Err(BlossomError::NoServers);
        }

        let hash = compute_sha256(data);
        let auth_header = self.create_upload_auth(&hash).await?;

        for server in &self.write_servers {
            match self
                .upload_to_server(server, data, &hash, &auth_header)
                .await
            {
                Ok(_) => {
                    debug!("Uploaded {} to {}", &hash[..12], server);
                    return Ok(hash);
                }
                Err(e) => {
                    warn!("Upload to {} failed: {}", server, e.detail);
                    continue;
                }
            }
        }

        Err(BlossomError::UploadFailed("all servers failed".to_string()))
    }

    /// Upload data only if it doesn't already exist
    /// Returns (hash, was_uploaded) tuple
    ///
    /// For small files (<256KB), skips existence check and uses the upload response status.
    /// For large files (>=256KB), does HEAD check first to save bandwidth.
    /// Retries up to 3 times with exponential backoff on transient failures.
    pub async fn upload_if_missing(&self, data: &[u8]) -> Result<(String, bool), BlossomError> {
        if self.write_servers.is_empty() {
            return Err(BlossomError::NoServers);
        }

        let hash = compute_sha256(data);

        // Warn if uploading empty data
        if data.is_empty() {
            warn!("Attempting to upload empty blob with hash {}", hash);
        }

        // For large files, check existence first to save bandwidth
        const HEAD_CHECK_THRESHOLD: usize = 256 * 1024; // 256KB
        if data.len() >= HEAD_CHECK_THRESHOLD && self.exists(&hash).await {
            debug!("Large blob {} already exists (skipped upload)", &hash[..12]);
            return Ok((hash, false));
        }

        let mut last_error = String::new();

        for attempt in 0..Self::max_upload_retries() {
            if attempt > 0 {
                let delay = Self::upload_retry_delay(attempt - 1);
                debug!(
                    "Retrying upload {} (attempt {}/{}), waiting {:?}",
                    &hash[..12],
                    attempt + 1,
                    Self::max_upload_retries(),
                    delay
                );
                tokio::time::sleep(delay).await;
            }

            // Regenerate auth header for each retry (in case of expiration)
            let auth_header = self.create_upload_auth(&hash).await?;
            let mut saw_retryable_error = false;

            for server in &self.write_servers {
                match self
                    .upload_to_server(server, data, &hash, &auth_header)
                    .await
                {
                    Ok(outcome) => {
                        if outcome.was_uploaded() {
                            debug!("Uploaded {} to {}", &hash[..12], server);
                        } else {
                            debug!("Blob {} already exists on {}", &hash[..12], server);
                        }
                        return Ok((hash, outcome.was_uploaded()));
                    }
                    Err(error) => {
                        last_error = format!("{}: {}", server, error.detail);
                        warn!("Upload to {} failed: {}", server, error.detail);
                        saw_retryable_error |= error.retryable;
                        continue;
                    }
                }
            }

            if !saw_retryable_error {
                break;
            }
        }

        Err(BlossomError::UploadFailed(format!(
            "all servers failed after {} retries (last: {})",
            Self::max_upload_retries(),
            last_error
        )))
    }

    /// Check if a blob exists on any write server
    pub async fn exists(&self, hash: &str) -> bool {
        for server in &self.write_servers {
            if self.check_on_server(hash, server).await == BlobAvailability::Present {
                return true;
            }
        }
        false
    }

    /// Check if a blob exists on a specific server
    pub async fn exists_on_server(&self, hash: &str, server: &str) -> bool {
        self.check_on_server(hash, server).await == BlobAvailability::Present
    }

    /// Batch-check upload targets on one server. Returns `None` when the server
    /// does not support the endpoint or the response is inconclusive.
    pub async fn check_uploads_on_server(
        &self,
        hashes: &[String],
        server: &str,
    ) -> Option<HashSet<String>> {
        if hashes.is_empty() {
            return Some(HashSet::new());
        }

        let mut present = HashSet::new();
        for chunk in hashes.chunks(UPLOAD_CHECK_BATCH_SIZE) {
            let chunk_present = self.check_uploads_on_server_chunk(chunk, server).await?;
            present.extend(chunk_present);
        }
        Some(present)
    }

    async fn check_uploads_on_server_chunk(
        &self,
        hashes: &[String],
        server: &str,
    ) -> Option<HashSet<String>> {
        let url = format!("{}/upload/check", server.trim_end_matches('/'));
        let body = match serde_json::to_vec(&UploadCheckRequest { hashes }) {
            Ok(body) => body,
            Err(err) => {
                debug!("Could not encode upload check request: {}", err);
                return None;
            }
        };

        let resp = match self
            .http
            .post(&url)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(err) => {
                debug!("Upload check request to {} failed: {}", server, err);
                return None;
            }
        };

        let status = resp.status();
        if status == StatusCode::NOT_FOUND
            || status == StatusCode::METHOD_NOT_ALLOWED
            || status == StatusCode::NOT_IMPLEMENTED
            || status == StatusCode::TOO_MANY_REQUESTS
        {
            return None;
        }
        if !status.is_success() {
            debug!("Upload check request to {} returned {}", server, status);
            return None;
        }

        let parsed: UploadCheckResponse = match resp.json().await {
            Ok(parsed) => parsed,
            Err(err) => {
                debug!(
                    "Could not parse upload check response from {}: {}",
                    server, err
                );
                return None;
            }
        };
        if parsed.count != hashes.len() {
            debug!(
                "Upload check response from {} had count {}, expected {}",
                server,
                parsed.count,
                hashes.len()
            );
            return None;
        }

        let present_bits = match decode_upload_check_bitset(&parsed.present, parsed.count) {
            Some(bits) => bits,
            None => {
                debug!("Upload check response from {} had invalid bitset", server);
                return None;
            }
        };

        Some(
            hashes
                .iter()
                .zip(present_bits)
                .filter_map(|(hash, present)| present.then_some(hash.clone()))
                .collect(),
        )
    }

    /// Upload a bounded batch to one server. Returns `Ok(None)` when the server
    /// does not support the hashtree batch extension.
    pub async fn upload_batch_to_server(
        &self,
        server: &str,
        items: &[BatchUploadItem],
    ) -> Result<Option<BatchUploadResult>, BlossomError> {
        if items.is_empty() {
            return Ok(Some(BatchUploadResult {
                accepted: 0,
                uploaded: 0,
            }));
        }
        if items.len() > BATCH_UPLOAD_MAX_BLOBS {
            return Err(BlossomError::UploadFailed(format!(
                "batch contains {} blobs, maximum is {}",
                items.len(),
                BATCH_UPLOAD_MAX_BLOBS
            )));
        }

        let total_bytes = items.iter().try_fold(0usize, |total, item| {
            total
                .checked_add(item.data.len())
                .filter(|value| *value <= BATCH_UPLOAD_MAX_BYTES)
        });
        if total_bytes.is_none() {
            return Err(BlossomError::UploadFailed(format!(
                "batch exceeds maximum size of {} bytes",
                BATCH_UPLOAD_MAX_BYTES
            )));
        }

        let auth_header = self
            .create_upload_auth_for_hashes(items.iter().map(|item| item.hash.as_str()))
            .await?;
        let blobs = items
            .iter()
            .map(|item| BatchUploadBlobRequest {
                sha256: item.hash.clone(),
                content_type: item.content_type.clone(),
                data: base64::engine::general_purpose::STANDARD.encode(&item.data),
            })
            .collect();
        let body = serde_json::to_vec(&BatchUploadRequest { blobs })
            .map_err(|err| BlossomError::UploadFailed(err.to_string()))?;
        let url = format!("{}/upload/batch", server.trim_end_matches('/'));

        let resp = self
            .http
            .post(&url)
            .header("Authorization", auth_header)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await?;

        let status = resp.status();
        if status == StatusCode::NOT_FOUND
            || status == StatusCode::METHOD_NOT_ALLOWED
            || status == StatusCode::NOT_IMPLEMENTED
        {
            return Ok(None);
        }
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(BlossomError::UploadFailed(format!("{}: {}", status, text)));
        }

        let parsed: BatchUploadResponse = resp.json().await?;
        if parsed.blobs.len() != items.len() {
            return Err(BlossomError::UploadFailed(format!(
                "batch response contained {} blobs, expected {}",
                parsed.blobs.len(),
                items.len()
            )));
        }

        let requested: HashSet<&str> = items.iter().map(|item| item.hash.as_str()).collect();
        if parsed
            .blobs
            .iter()
            .any(|blob| !requested.contains(blob.sha256.as_str()))
        {
            return Err(BlossomError::UploadFailed(
                "batch response contained unexpected blob hash".to_string(),
            ));
        }

        Ok(Some(BatchUploadResult {
            accepted: parsed.blobs.len(),
            uploaded: parsed.uploaded,
        }))
    }

    /// Probe a blob on a specific server, distinguishing explicit misses from
    /// inconclusive network/server failures.
    pub async fn check_on_server(&self, hash: &str, server: &str) -> BlobAvailability {
        let url = format!("{}/{}.bin", server.trim_end_matches('/'), hash);
        debug!("Checking exists: {}", url);
        match self.http.head(&url).send().await {
            Ok(resp) => {
                let status = resp.status();
                debug!("  -> status: {}", status);
                if status == StatusCode::NOT_FOUND || status == StatusCode::GONE {
                    return BlobAvailability::Missing;
                }
                if !status.is_success() {
                    return BlobAvailability::Unknown;
                }

                // Verify content-type is binary, not HTML error page
                if let Some(ct) = resp.headers().get("content-type") {
                    if let Ok(ct_str) = ct.to_str() {
                        if ct_str.starts_with("text/html") {
                            return BlobAvailability::Unknown;
                        }
                    }
                }
                // Verify content-length > 0
                if let Some(cl) = resp.headers().get("content-length") {
                    if let Ok(cl_str) = cl.to_str() {
                        if cl_str == "0" {
                            return BlobAvailability::Unknown;
                        }
                    }
                }
                BlobAvailability::Present
            }
            Err(err) => {
                debug!("  -> probe failed: {}", err);
                BlobAvailability::Unknown
            }
        }
    }

    /// Check if server has a tree by sampling hashes (parallel checks)
    pub async fn server_has_tree_samples(
        &self,
        server: &str,
        hashes: &[&str],
        sample_size: usize,
    ) -> bool {
        self.server_tree_sample_coverage(server, hashes, sample_size)
            .await
            == BlobAvailability::Present
    }

    /// Probe sampled tree coverage on one server.
    pub async fn server_tree_sample_coverage(
        &self,
        server: &str,
        hashes: &[&str],
        sample_size: usize,
    ) -> BlobAvailability {
        use futures::future::join_all;
        if hashes.is_empty() {
            return BlobAvailability::Missing;
        }
        // Spread samples across the hash list
        let step = (hashes.len() / sample_size.min(hashes.len())).max(1);
        let samples: Vec<_> = hashes.iter().step_by(step).take(sample_size).collect();
        let checks: Vec<_> = samples
            .iter()
            .map(|h| self.check_on_server(h, server))
            .collect();
        let results = join_all(checks).await;
        if results.contains(&BlobAvailability::Missing) {
            BlobAvailability::Missing
        } else if results
            .iter()
            .all(|result| *result == BlobAvailability::Present)
        {
            BlobAvailability::Present
        } else {
            BlobAvailability::Unknown
        }
    }

    /// Upload to all write servers in parallel, returns (hash, success_count)
    pub async fn upload_to_all_servers(
        &self,
        data: &[u8],
    ) -> Result<(String, usize), BlossomError> {
        self.upload_to_selected_servers(data, &self.write_servers)
            .await
    }

    /// Upload to selected servers in parallel and return after the first success.
    ///
    /// This is useful when servers are redundant replication targets and a slow
    /// or broken secondary should not block publishing a root that is already
    /// available from another source.
    pub async fn upload_to_any_selected_server(
        &self,
        data: &[u8],
        servers: &[String],
    ) -> Result<(String, bool), BlossomError> {
        use futures::stream::{FuturesUnordered, StreamExt};
        if servers.is_empty() {
            return Err(BlossomError::NoServers);
        }

        let hash = compute_sha256(data);
        let mut pending: Vec<String> = servers.to_vec();
        let mut last_error = String::new();

        for attempt in 0..Self::max_upload_retries() {
            if pending.is_empty() {
                break;
            }
            if attempt > 0 {
                tokio::time::sleep(Self::upload_retry_delay(attempt - 1)).await;
            }

            let auth = self.create_upload_auth(&hash).await?;
            let mut uploads = FuturesUnordered::new();
            for server in pending.drain(..) {
                let hash = hash.clone();
                let auth = auth.clone();
                uploads.push(async move {
                    let result = self.upload_to_server(&server, data, &hash, &auth).await;
                    (server, result)
                });
            }

            let mut retryable_servers = Vec::new();
            while let Some((server, result)) = uploads.next().await {
                match result {
                    Ok(outcome) => return Ok((hash, outcome.was_uploaded())),
                    Err(error) => {
                        last_error = format!("{server}: {}", error.detail);
                        warn!("Upload to {} failed: {}", server, error.detail);
                        if error.retryable {
                            retryable_servers.push(server);
                        }
                    }
                }
            }

            pending = retryable_servers;
        }

        Err(BlossomError::UploadFailed(format!(
            "all selected servers failed after {} retries{}",
            Self::max_upload_retries(),
            if last_error.is_empty() {
                String::new()
            } else {
                format!(" (last: {last_error})")
            }
        )))
    }

    /// Upload to the selected servers in parallel, returns (hash, success_count)
    pub async fn upload_to_selected_servers(
        &self,
        data: &[u8],
        servers: &[String],
    ) -> Result<(String, usize), BlossomError> {
        use futures::future::join_all;
        if servers.is_empty() {
            return Err(BlossomError::NoServers);
        }
        let hash = compute_sha256(data);
        let mut succeeded = 0usize;
        let mut pending: Vec<String> = servers.to_vec();
        let mut last_error = String::new();

        for attempt in 0..Self::max_upload_retries() {
            if pending.is_empty() {
                break;
            }
            if attempt > 0 {
                tokio::time::sleep(Self::upload_retry_delay(attempt - 1)).await;
            }

            let auth = self.create_upload_auth(&hash).await?;
            let uploads: Vec<_> = pending
                .iter()
                .map(|server| {
                    let server = server.clone();
                    let hash = hash.clone();
                    let auth = auth.clone();
                    async move {
                        (
                            server.clone(),
                            self.upload_to_server(&server, data, &hash, &auth).await,
                        )
                    }
                })
                .collect();
            let results = join_all(uploads).await;
            let mut retryable_servers = Vec::new();

            for (server, result) in results {
                match result {
                    Ok(_) => {
                        succeeded += 1;
                    }
                    Err(error) => {
                        last_error = format!("{server}: {}", error.detail);
                        warn!("Upload to {} failed: {}", server, error.detail);
                        if error.retryable {
                            retryable_servers.push(server);
                        }
                    }
                }
            }

            pending = retryable_servers;
        }

        if succeeded == 0 {
            return Err(BlossomError::UploadFailed(format!(
                "all selected servers failed after {} retries{}",
                Self::max_upload_retries(),
                if last_error.is_empty() {
                    String::new()
                } else {
                    format!(" (last: {last_error})")
                }
            )));
        }
        Ok((hash, succeeded))
    }

    /// Download data from Blossom servers
    /// Verifies the hash matches before returning
    pub async fn download(&self, hash: &str) -> Result<Vec<u8>, BlossomError> {
        if self.read_servers.is_empty() {
            return Err(BlossomError::NoServers);
        }

        let mut last_error = String::new();

        for server in &self.read_servers {
            let url = format!("{}/{}.bin", server.trim_end_matches('/'), hash);
            match self.http.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    // Capture X-Source header before consuming body (if from local daemon)
                    let x_source = resp
                        .headers()
                        .get("x-source")
                        .and_then(|v| v.to_str().ok())
                        .map(|s| s.to_string());

                    match resp.bytes().await {
                        Ok(bytes) => {
                            let computed = compute_sha256(&bytes);
                            if computed == hash {
                                if let Some(source) = x_source {
                                    debug!(
                                        "Downloaded {} ({} bytes) via {} [source: {}]",
                                        &hash[..12.min(hash.len())],
                                        bytes.len(),
                                        server,
                                        source
                                    );
                                } else {
                                    debug!(
                                        "Downloaded {} ({} bytes) from {}",
                                        &hash[..12.min(hash.len())],
                                        bytes.len(),
                                        server
                                    );
                                }
                                return Ok(bytes.to_vec());
                            } else {
                                last_error = format!(
                                    "hash mismatch from {}: expected {}, got {} ({} bytes received)",
                                    server,
                                    hash,
                                    computed,
                                    bytes.len()
                                );
                                warn!(
                                    "Hash mismatch downloading {} from {}: got {} ({} bytes)",
                                    hash,
                                    server,
                                    &computed[..12.min(computed.len())],
                                    bytes.len()
                                );
                            }
                        }
                        Err(e) => {
                            last_error = e.to_string();
                        }
                    }
                }
                Ok(resp) => {
                    last_error = format!("{} returned {}", server, resp.status());
                    debug!(
                        "Download {} from {} returned status {}",
                        hash,
                        server,
                        resp.status()
                    );
                }
                Err(e) => {
                    last_error = e.to_string();
                }
            }
        }

        Err(BlossomError::DownloadFailed(last_error))
    }

    /// Download if available, returns None if not found
    pub async fn try_download(&self, hash: &str) -> Option<Vec<u8>> {
        self.download(hash).await.ok()
    }

    /// Upload to a single server
    /// Returns Ok(Uploaded) if uploaded, Ok(AlreadyExists) if already exists.
    async fn upload_to_server(
        &self,
        server: &str,
        data: &[u8],
        hash: &str,
        auth_header: &str,
    ) -> Result<UploadOutcome, UploadAttemptError> {
        let url = format!("{}/upload", server.trim_end_matches('/'));

        let resp = self
            .http
            .put(&url)
            .header("Authorization", auth_header)
            .header("Content-Type", "application/octet-stream")
            .header("X-SHA-256", hash)
            .body(data.to_vec())
            .send()
            .await
            .map_err(|error| {
                if error.is_timeout() || error.is_connect() || error.is_request() || error.is_body()
                {
                    UploadAttemptError::retryable(error.to_string())
                } else {
                    UploadAttemptError::fatal(error.to_string())
                }
            })?;

        let status = resp.status();
        if status == StatusCode::OK || status.as_u16() == 409 {
            Ok(UploadOutcome::AlreadyExists)
        } else if status.is_success() {
            Ok(UploadOutcome::Uploaded)
        } else {
            let text = resp.text().await.unwrap_or_default();
            let detail = format!("{}: {}", status, text);
            if Self::is_retryable_status(status) {
                Err(UploadAttemptError::retryable(detail))
            } else {
                Err(UploadAttemptError::fatal(detail))
            }
        }
    }

    fn is_retryable_status(status: StatusCode) -> bool {
        status == StatusCode::REQUEST_TIMEOUT
            || status == StatusCode::TOO_MANY_REQUESTS
            || status.is_server_error()
    }

    fn max_upload_retries() -> u32 {
        8
    }

    fn upload_retry_delay(attempt: u32) -> Duration {
        Duration::from_secs(1 << attempt.min(3))
    }

    async fn create_upload_auth(&self, hash: &str) -> Result<String, BlossomError> {
        self.create_upload_auth_for_hashes(std::iter::once(hash))
            .await
    }

    async fn create_upload_auth_for_hashes<'a, I>(&self, hashes: I) -> Result<String, BlossomError>
    where
        I: IntoIterator<Item = &'a str>,
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let expiration = now + 300; // 5 minutes

        let mut tags = vec![Tag::custom(
            TagKind::custom("t"),
            vec!["upload".to_string()],
        )];
        tags.extend(
            hashes
                .into_iter()
                .map(|hash| Tag::custom(TagKind::custom("x"), vec![hash.to_string()])),
        );
        tags.push(Tag::custom(
            TagKind::custom("expiration"),
            vec![expiration.to_string()],
        ));
        let event = EventBuilder::new(Kind::Custom(24242), "Upload")
            .tags(tags)
            .sign_with_keys(&self.keys)
            .map_err(|e| BlossomError::Signing(e.to_string()))?;

        let json = event.as_json();
        let encoded = base64::engine::general_purpose::STANDARD.encode(json);
        Ok(format!("Nostr {}", encoded))
    }
}

fn decode_upload_check_bitset(encoded: &str, count: usize) -> Option<Vec<bool>> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    if bytes.len() < count.div_ceil(8) {
        return None;
    }
    Some(
        (0..count)
            .map(|index| bytes[index / 8] & (1 << (index % 8)) != 0)
            .collect(),
    )
}

/// Compute SHA256 hash of data, returning hex string
pub fn compute_sha256(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

/// Store implementation for Blossom (read-only, fetches from servers on demand)
#[cfg(feature = "store")]
mod store_impl {
    use super::*;
    use async_trait::async_trait;
    use hashtree_core::{to_hex, Hash, Store, StoreError};
    use std::collections::HashMap;
    use std::sync::RwLock;

    /// Blossom-backed store (read-only with local cache)
    ///
    /// Fetches data from Blossom servers on demand and caches locally.
    /// Write operations are no-ops (data should be uploaded separately).
    pub struct BlossomStore {
        client: BlossomClient,
        cache: RwLock<HashMap<String, Vec<u8>>>,
    }

    impl BlossomStore {
        pub fn new(client: BlossomClient) -> Self {
            Self {
                client,
                cache: RwLock::new(HashMap::new()),
            }
        }

        /// Create with servers (convenience constructor)
        pub fn with_servers(keys: nostr::Keys, servers: Vec<String>) -> Self {
            let client = BlossomClient::new(keys).with_servers(servers);
            Self::new(client)
        }

        /// Get underlying client
        pub fn client(&self) -> &BlossomClient {
            &self.client
        }
    }

    #[async_trait]
    impl Store for BlossomStore {
        async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
            // Cache locally, but don't upload (caller should upload explicitly)
            let key = to_hex(&hash);
            let mut cache = self.cache.write().unwrap();
            if cache.contains_key(&key) {
                return Ok(false);
            }
            cache.insert(key, data);
            Ok(true)
        }

        async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
            let key = to_hex(hash);

            // Check cache first
            {
                let cache = self.cache.read().unwrap();
                if let Some(data) = cache.get(&key) {
                    return Ok(Some(data.clone()));
                }
            }

            // Fetch from Blossom
            match self.client.try_download(&key).await {
                Some(data) => {
                    // Cache for future use
                    let mut cache = self.cache.write().unwrap();
                    cache.insert(key, data.clone());
                    Ok(Some(data))
                }
                None => Ok(None),
            }
        }

        async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
            let key = to_hex(hash);

            // Check cache first
            {
                let cache = self.cache.read().unwrap();
                if cache.contains_key(&key) {
                    return Ok(true);
                }
            }

            // Check Blossom
            Ok(self.client.exists(&key).await)
        }

        async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
            // Only delete from local cache (can't delete from Blossom)
            let key = to_hex(hash);
            let mut cache = self.cache.write().unwrap();
            Ok(cache.remove(&key).is_some())
        }
    }
}

#[cfg(feature = "store")]
pub use store_impl::BlossomStore;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;

    struct TestUploadServer {
        url: String,
        request_count: Arc<AtomicUsize>,
        done: Option<tokio::sync::oneshot::Receiver<()>>,
    }

    struct TestBodyServer {
        url: String,
        done: Option<tokio::sync::oneshot::Receiver<()>>,
    }

    impl TestUploadServer {
        fn new(statuses: Vec<u16>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
            let addr = listener.local_addr().expect("local addr");
            let request_count = Arc::new(AtomicUsize::new(0));
            let request_count_for_thread = Arc::clone(&request_count);
            let (done_tx, done_rx) = tokio::sync::oneshot::channel();

            thread::spawn(move || {
                let mut buffer = [0u8; 8192];
                for status in statuses {
                    let (mut stream, _) = listener.accept().expect("accept request");
                    request_count_for_thread.fetch_add(1, Ordering::SeqCst);
                    let mut request = Vec::new();
                    let header_end = loop {
                        let bytes = stream.read(&mut buffer).expect("read request");
                        if bytes == 0 {
                            break None;
                        }
                        request.extend_from_slice(&buffer[..bytes]);
                        if let Some(pos) =
                            request.windows(4).position(|window| window == b"\r\n\r\n")
                        {
                            break Some(pos + 4);
                        }
                    };

                    if let Some(header_end) = header_end {
                        let headers = String::from_utf8_lossy(&request[..header_end]);
                        let content_length = headers.lines().find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            if name.eq_ignore_ascii_case("content-length") {
                                value.trim().parse::<usize>().ok()
                            } else {
                                None
                            }
                        });
                        if let Some(content_length) = content_length {
                            let mut remaining = content_length
                                .saturating_sub(request.len().saturating_sub(header_end));
                            while remaining > 0 {
                                let bytes = stream.read(&mut buffer).expect("drain body");
                                if bytes == 0 {
                                    break;
                                }
                                remaining = remaining.saturating_sub(bytes);
                            }
                        }
                    }

                    let reason = match status {
                        200 => "OK",
                        403 => "Forbidden",
                        201 => "Created",
                        202 => "Accepted",
                        409 => "Conflict",
                        429 => "Too Many Requests",
                        500 => "Internal Server Error",
                        503 => "Service Unavailable",
                        _ => "Test Status",
                    };
                    let body = format!("status {status}");
                    write!(
                        stream,
                        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .expect("write response");
                    stream.flush().expect("flush response");
                }

                let _ = done_tx.send(());
            });

            Self {
                url: format!("http://{}", addr),
                request_count,
                done: Some(done_rx),
            }
        }

        async fn wait_for_requests(&mut self) {
            if let Some(done) = self.done.take() {
                let _ = done.await;
            }
        }

        fn request_count(&self) -> usize {
            self.request_count.load(Ordering::SeqCst)
        }
    }

    impl TestBodyServer {
        fn new<T>(responses: Vec<(u16, T)>) -> Self
        where
            T: Into<String>,
        {
            let responses: Vec<(u16, String)> = responses
                .into_iter()
                .map(|(status, body)| (status, body.into()))
                .collect();
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
            let addr = listener.local_addr().expect("local addr");
            let (done_tx, done_rx) = tokio::sync::oneshot::channel();

            thread::spawn(move || {
                let mut buffer = [0u8; 8192];
                for (status, body) in responses {
                    let (mut stream, _) = listener.accept().expect("accept request");
                    drain_http_request(&mut stream, &mut buffer);
                    let reason = match status {
                        200 => "OK",
                        404 => "Not Found",
                        405 => "Method Not Allowed",
                        429 => "Too Many Requests",
                        500 => "Internal Server Error",
                        _ => "Test Status",
                    };
                    write!(
                        stream,
                        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .expect("write response");
                    stream.flush().expect("flush response");
                }

                let _ = done_tx.send(());
            });

            Self {
                url: format!("http://{}", addr),
                done: Some(done_rx),
            }
        }

        async fn wait_for_requests(&mut self) {
            if let Some(done) = self.done.take() {
                let _ = done.await;
            }
        }
    }

    fn drain_http_request(stream: &mut std::net::TcpStream, buffer: &mut [u8; 8192]) {
        let mut request = Vec::new();
        let header_end = loop {
            let bytes = stream.read(buffer).expect("read request");
            if bytes == 0 {
                break None;
            }
            request.extend_from_slice(&buffer[..bytes]);
            if let Some(pos) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                break Some(pos + 4);
            }
        };

        if let Some(header_end) = header_end {
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                if name.eq_ignore_ascii_case("content-length") {
                    value.trim().parse::<usize>().ok()
                } else {
                    None
                }
            });
            if let Some(content_length) = content_length {
                let mut remaining =
                    content_length.saturating_sub(request.len().saturating_sub(header_end));
                while remaining > 0 {
                    let bytes = stream.read(buffer).expect("drain body");
                    if bytes == 0 {
                        break;
                    }
                    remaining = remaining.saturating_sub(bytes);
                }
            }
        }
    }

    #[test]
    fn test_compute_sha256() {
        let hash = compute_sha256(b"hello world");
        assert_eq!(
            hash,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn test_client_builder() {
        let keys = Keys::generate();
        let client = BlossomClient::new(keys)
            .with_servers(vec!["https://example.com".to_string()])
            .with_timeout(Duration::from_secs(60));

        assert_eq!(client.servers().len(), 1);
    }

    #[tokio::test]
    async fn test_exists_on_server() {
        let keys = Keys::generate();
        let client = BlossomClient::new(keys).with_servers(vec!["https://example.com".to_string()]);

        // Method should exist and return bool
        let result = client
            .exists_on_server("abc123", "https://example.com")
            .await;
        assert!(!result); // Non-existent hash
    }

    #[tokio::test]
    async fn test_check_on_server_distinguishes_missing_from_unknown() {
        let mut missing_server = TestUploadServer::new(vec![404]);
        let mut unknown_server = TestUploadServer::new(vec![503]);
        let keys = Keys::generate();
        let client = BlossomClient::new_empty(keys);

        assert_eq!(
            client.check_on_server("abc123", &missing_server.url).await,
            BlobAvailability::Missing
        );
        assert_eq!(
            client.check_on_server("abc123", &unknown_server.url).await,
            BlobAvailability::Unknown
        );

        missing_server.wait_for_requests().await;
        unknown_server.wait_for_requests().await;
    }

    #[tokio::test]
    async fn test_check_uploads_on_server_decodes_present_hashes() {
        let mut server = TestBodyServer::new(vec![(200, r#"{"count":3,"present":"Ag=="}"#)]);
        let keys = Keys::generate();
        let client = BlossomClient::new_empty(keys);
        let hashes = vec!["00".repeat(32), "11".repeat(32), "22".repeat(32)];

        let present = client
            .check_uploads_on_server(&hashes, &server.url)
            .await
            .expect("upload check response");

        assert_eq!(present.len(), 1);
        assert!(present.contains(&hashes[1]));
        server.wait_for_requests().await;
    }

    #[tokio::test]
    async fn test_check_uploads_on_server_returns_none_when_unsupported() {
        let mut server = TestBodyServer::new(vec![(404, r#"{"error":"not found"}"#)]);
        let keys = Keys::generate();
        let client = BlossomClient::new_empty(keys);
        let hashes = vec!["00".repeat(32)];

        assert!(client
            .check_uploads_on_server(&hashes, &server.url)
            .await
            .is_none());
        server.wait_for_requests().await;
    }

    #[tokio::test]
    async fn test_upload_batch_to_server_accepts_batch_response() {
        let first = b"first batch blob".to_vec();
        let second = b"second batch blob".to_vec();
        let first_hash = compute_sha256(&first);
        let second_hash = compute_sha256(&second);
        let response = format!(
            r#"{{"uploaded":2,"blobs":[{{"sha256":"{}"}},{{"sha256":"{}"}}]}}"#,
            first_hash, second_hash
        );
        let mut server = TestBodyServer::new(vec![(200, response)]);
        let keys = Keys::generate();
        let client = BlossomClient::new_empty(keys);
        let items = vec![
            BatchUploadItem::new(first_hash, first),
            BatchUploadItem::new(second_hash, second),
        ];

        let result = client
            .upload_batch_to_server(&server.url, &items)
            .await
            .expect("batch upload")
            .expect("batch endpoint supported");

        assert_eq!(result.accepted, 2);
        assert_eq!(result.uploaded, 2);
        server.wait_for_requests().await;
    }

    #[tokio::test]
    async fn test_upload_batch_to_server_returns_none_when_unsupported() {
        let mut server = TestBodyServer::new(vec![(404, r#"{"error":"not found"}"#)]);
        let keys = Keys::generate();
        let client = BlossomClient::new_empty(keys);
        let data = b"batch blob".to_vec();
        let hash = compute_sha256(&data);

        assert!(client
            .upload_batch_to_server(&server.url, &[BatchUploadItem::new(hash, data)])
            .await
            .expect("unsupported batch probe")
            .is_none());
        server.wait_for_requests().await;
    }

    #[tokio::test]
    async fn test_server_has_tree_samples() {
        let keys = Keys::generate();
        let client = BlossomClient::new(keys).with_servers(vec!["https://example.com".to_string()]);

        let hashes = vec!["hash1", "hash2", "hash3"];
        // Method should exist and check samples
        let result = client
            .server_has_tree_samples("https://example.com", &hashes, 3)
            .await;
        assert!(!result); // Non-existent hashes
    }

    #[tokio::test]
    async fn test_server_tree_sample_coverage_keeps_unknown_separate_from_missing() {
        let mut server = TestUploadServer::new(vec![200, 503]);
        let keys = Keys::generate();
        let client = BlossomClient::new_empty(keys);
        let hashes = vec!["hash1", "hash2"];

        let result = client
            .server_tree_sample_coverage(&server.url, &hashes, 2)
            .await;

        assert_eq!(result, BlobAvailability::Unknown);
        server.wait_for_requests().await;
    }

    #[tokio::test]
    async fn test_upload_to_all_servers() {
        let keys = Keys::generate();
        let client = BlossomClient::new(keys).with_servers(vec![
            "https://example1.com".to_string(),
            "https://example2.com".to_string(),
        ]);

        // Method should exist and return (hash, server_count)
        // Will fail since servers don't exist, but should compile
        let result = client.upload_to_all_servers(b"test data").await;
        assert!(result.is_err()); // Expected to fail - servers don't exist
    }

    #[tokio::test]
    async fn test_upload_if_missing_retries_transient_server_errors() {
        let mut server = TestUploadServer::new(vec![500, 500, 201]);
        let keys = Keys::generate();
        let client = BlossomClient::new_empty(keys).with_write_servers(vec![server.url.clone()]);

        let (hash, uploaded) = client
            .upload_if_missing(b"test data")
            .await
            .expect("upload");
        assert!(uploaded);
        assert_eq!(hash, compute_sha256(b"test data"));

        server.wait_for_requests().await;
        assert_eq!(server.request_count(), 3);
    }

    #[tokio::test]
    async fn test_upload_if_missing_treats_200_as_already_exists() {
        let mut server = TestUploadServer::new(vec![200]);
        let keys = Keys::generate();
        let client = BlossomClient::new_empty(keys).with_write_servers(vec![server.url.clone()]);

        let (hash, uploaded) = client
            .upload_if_missing(b"test data")
            .await
            .expect("upload");
        assert!(!uploaded);
        assert_eq!(hash, compute_sha256(b"test data"));

        server.wait_for_requests().await;
        assert_eq!(server.request_count(), 1);
    }

    #[tokio::test]
    async fn test_upload_if_missing_accepts_legacy_409_as_already_exists() {
        let mut server = TestUploadServer::new(vec![409]);
        let keys = Keys::generate();
        let client = BlossomClient::new_empty(keys).with_write_servers(vec![server.url.clone()]);

        let (hash, uploaded) = client
            .upload_if_missing(b"test data")
            .await
            .expect("upload");
        assert!(!uploaded);
        assert_eq!(hash, compute_sha256(b"test data"));

        server.wait_for_requests().await;
        assert_eq!(server.request_count(), 1);
    }

    #[tokio::test]
    async fn test_upload_if_missing_stops_retrying_on_non_retryable_error() {
        let mut server = TestUploadServer::new(vec![403]);
        let keys = Keys::generate();
        let client = BlossomClient::new_empty(keys).with_write_servers(vec![server.url.clone()]);

        let err = client
            .upload_if_missing(b"test data")
            .await
            .expect_err("upload should fail");
        assert!(err.to_string().contains("403"));

        server.wait_for_requests().await;
        assert_eq!(server.request_count(), 1);
    }

    #[tokio::test]
    async fn test_upload_to_selected_servers_retries_transient_failures() {
        let mut server = TestUploadServer::new(vec![503, 201]);
        let keys = Keys::generate();
        let client = BlossomClient::new_empty(keys);
        let servers = vec![server.url.clone()];

        let (_hash, success_count) = client
            .upload_to_selected_servers(b"test data", &servers)
            .await
            .expect("selected upload");
        assert_eq!(success_count, 1);

        server.wait_for_requests().await;
        assert_eq!(server.request_count(), 2);
    }

    #[test]
    fn test_local_daemon_priority() {
        let keys = Keys::generate();
        let client = BlossomClient::new_empty(keys)
            .with_servers(vec![
                "https://remote1.com".to_string(),
                "https://remote2.com".to_string(),
            ])
            .with_local_daemon("http://127.0.0.1:8080".to_string());

        // Local daemon should be first in read_servers
        assert_eq!(client.read_servers().len(), 3);
        assert_eq!(client.read_servers()[0], "http://127.0.0.1:8080");
        assert_eq!(client.read_servers()[1], "https://remote1.com");
        assert_eq!(client.read_servers()[2], "https://remote2.com");
    }

    #[test]
    fn test_local_daemon_not_duplicated() {
        let keys = Keys::generate();
        // If local daemon is already in servers, don't add it again
        let client = BlossomClient::new_empty(keys)
            .with_servers(vec![
                "http://127.0.0.1:8080".to_string(),
                "https://remote.com".to_string(),
            ])
            .with_local_daemon("http://127.0.0.1:8080".to_string());

        // Should not duplicate
        assert_eq!(client.read_servers().len(), 2);
        assert_eq!(client.read_servers()[0], "http://127.0.0.1:8080");
    }
}
