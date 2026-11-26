#![doc = include_str!("../README.md")]

use base64::Engine;
use nostr::prelude::*;
use reqwest::{
    header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE, LOCATION},
    Method, StatusCode, Url,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;
use thiserror::Error;
use tracing::{debug, warn};

const UPLOAD_CHECK_BATCH_SIZE: usize = 10_000;
const MAX_UPLOAD_REDIRECTS: usize = 3;
const UPLOAD_REDIRECT_OPT_IN_ENV: &str = "HTREE_BLOSSOM_UPLOAD_REDIRECT_OPT_IN";
const UPLOAD_REDIRECT_OPT_IN_HEADER: &str = "x-hashtree-upload-redirect";
pub const BATCH_UPLOAD_MAX_BLOBS: usize = 1024;
pub const BATCH_UPLOAD_MAX_BYTES: usize = 64 * 1024 * 1024;
pub const BATCH_UPLOAD_BINARY_CONTENT_TYPE: &str = "application/vnd.hashtree.blossom.batch.v1";
pub const BATCH_UPLOAD_HASH_LIST_AUTH_TAG: &str = "x-batch";
pub const BLOB_BATCH_DOWNLOAD_MAGIC: &[u8; 8] = b"HTBDV1\0\0";
pub const BLOB_BATCH_DOWNLOAD_CONTENT_TYPE: &str =
    "application/vnd.hashtree.blob-batch.v1+octet-stream";
const BINARY_BATCH_UPLOAD_MAGIC: &[u8; 8] = b"HTBBV1\0\0";
const BINARY_BATCH_UPLOAD_MAX_CONTENT_TYPE_BYTES: usize = 1024;
const DOWNLOAD_HEDGE_DELAY: Duration = Duration::from_millis(150);
const DOWNLOAD_MAX_ATTEMPTS: usize = 3;

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
    upload_http: reqwest::Client,
    upload_http1_only: bool,
    dns_overrides: Vec<(String, Vec<SocketAddr>)>,
    danger_accept_invalid_certs: bool,
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

#[derive(Clone)]
struct UploadBodyRequest {
    method: Method,
    url: String,
    auth_header: String,
    content_type: String,
    extra_headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
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

fn validate_batch_upload_items(items: &[BatchUploadItem]) -> Result<(), BlossomError> {
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

    Ok(())
}

fn encode_binary_batch_upload(items: &[BatchUploadItem]) -> Result<Vec<u8>, BlossomError> {
    let total_bytes = items.iter().map(|item| item.data.len()).sum::<usize>();
    let mut body = Vec::with_capacity(
        BINARY_BATCH_UPLOAD_MAGIC.len() + 4 + items.len() * (32 + 2 + 8) + total_bytes,
    );
    body.extend_from_slice(BINARY_BATCH_UPLOAD_MAGIC);
    body.extend_from_slice(&(items.len() as u32).to_be_bytes());

    for item in items {
        let hash = hex::decode(&item.hash)
            .map_err(|_| BlossomError::UploadFailed("invalid batch blob hash".to_string()))?;
        let hash: [u8; 32] = hash
            .try_into()
            .map_err(|_| BlossomError::UploadFailed("invalid batch blob hash".to_string()))?;
        let content_type = item.content_type.as_deref().unwrap_or("");
        if content_type.len() > BINARY_BATCH_UPLOAD_MAX_CONTENT_TYPE_BYTES {
            return Err(BlossomError::UploadFailed(
                "batch blob content type is too long".to_string(),
            ));
        }
        let content_type_len = u16::try_from(content_type.len()).map_err(|_| {
            BlossomError::UploadFailed("batch blob content type is too long".to_string())
        })?;

        body.extend_from_slice(&hash);
        body.extend_from_slice(&content_type_len.to_be_bytes());
        body.extend_from_slice(&(item.data.len() as u64).to_be_bytes());
        body.extend_from_slice(content_type.as_bytes());
        body.extend_from_slice(&item.data);
    }

    Ok(body)
}

/// Return the compact authorization digest for an ordered batch upload hash list.
pub fn batch_upload_hash_list_digest<'a, I>(hashes: I) -> Result<String, BlossomError>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut hasher = Sha256::new();
    hasher.update(b"hashtree-blossom-batch-v1\n");
    for hash in hashes {
        let hash_bytes = hex::decode(hash)
            .map_err(|_| BlossomError::UploadFailed("invalid batch blob hash".to_string()))?;
        if hash_bytes.len() != 32 {
            return Err(BlossomError::UploadFailed(
                "invalid batch blob hash".to_string(),
            ));
        }
        hasher.update(hash_bytes);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn default_http_client(
    timeout: Duration,
    dns_overrides: &[(String, Vec<SocketAddr>)],
    danger_accept_invalid_certs: bool,
) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .timeout(timeout)
        .danger_accept_invalid_certs(danger_accept_invalid_certs);
    for (host, addrs) in dns_overrides {
        builder = builder.resolve_to_addrs(host, addrs);
    }
    builder.build().unwrap()
}

fn upload_http_client(
    timeout: Duration,
    http1_only: bool,
    dns_overrides: &[(String, Vec<SocketAddr>)],
    danger_accept_invalid_certs: bool,
) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .danger_accept_invalid_certs(danger_accept_invalid_certs);
    for (host, addrs) in dns_overrides {
        builder = builder.resolve_to_addrs(host, addrs);
    }
    if http1_only {
        builder = builder.http1_only();
    }
    builder.build().unwrap()
}

fn same_origin(left: &Url, right: &Url) -> bool {
    left.scheme() == right.scheme()
        && left.host_str() == right.host_str()
        && left.port_or_known_default() == right.port_or_known_default()
}

fn is_loopback_url(url: &Url) -> bool {
    match url.host_str() {
        Some("localhost") => true,
        Some(host) => host
            .parse::<IpAddr>()
            .map(|address| address.is_loopback())
            .unwrap_or(false),
        None => false,
    }
}

fn trusted_upload_redirect(from: &Url, to: &Url) -> bool {
    if same_origin(from, to) {
        return true;
    }

    if from.scheme() == "http" && to.scheme() == "https" && from.host_str() == to.host_str() {
        return true;
    }

    from.scheme() == "http" && to.scheme() == "http" && is_loopback_url(from) && is_loopback_url(to)
}

fn upload_redirect_extra_headers() -> Vec<(&'static str, String)> {
    match std::env::var(UPLOAD_REDIRECT_OPT_IN_ENV) {
        Ok(value)
            if matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            ) =>
        {
            vec![(UPLOAD_REDIRECT_OPT_IN_HEADER, "1".to_string())]
        }
        _ => Vec::new(),
    }
}

async fn parse_batch_upload_response(
    resp: reqwest::Response,
    items: &[BatchUploadItem],
) -> Result<Option<BatchUploadResult>, BlossomError> {
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
            http: default_http_client(Duration::from_secs(30), &[], false),
            upload_http: upload_http_client(Duration::from_secs(30), true, &[], false),
            upload_http1_only: true,
            dns_overrides: Vec::new(),
            danger_accept_invalid_certs: false,
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
            http: default_http_client(Duration::from_secs(30), &[], false),
            upload_http: upload_http_client(Duration::from_secs(30), true, &[], false),
            upload_http1_only: true,
            dns_overrides: Vec::new(),
            danger_accept_invalid_certs: false,
            timeout: Duration::from_secs(30),
        }
    }

    /// Create a new client without loading config (empty servers)
    pub fn new_empty(keys: Keys) -> Self {
        Self {
            keys,
            read_servers: vec![],
            write_servers: vec![],
            http: default_http_client(Duration::from_secs(30), &[], false),
            upload_http: upload_http_client(Duration::from_secs(30), true, &[], false),
            upload_http1_only: true,
            dns_overrides: Vec::new(),
            danger_accept_invalid_certs: false,
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
        self.http = default_http_client(
            timeout,
            &self.dns_overrides,
            self.danger_accept_invalid_certs,
        );
        self.upload_http = upload_http_client(
            timeout,
            self.upload_http1_only,
            &self.dns_overrides,
            self.danger_accept_invalid_certs,
        );
        self
    }

    /// Force upload requests to use HTTP/1.1 while leaving read requests unchanged.
    pub fn with_upload_http1_only(mut self) -> Self {
        self.upload_http1_only = true;
        self.upload_http = upload_http_client(
            self.timeout,
            true,
            &self.dns_overrides,
            self.danger_accept_invalid_certs,
        );
        self
    }

    /// Allow the HTTP client to negotiate HTTP/2 for uploads.
    pub fn with_upload_http2_auto(mut self) -> Self {
        self.upload_http1_only = false;
        self.upload_http = upload_http_client(
            self.timeout,
            false,
            &self.dns_overrides,
            self.danger_accept_invalid_certs,
        );
        self
    }

    /// Override DNS resolution for a host on both read and upload clients.
    pub fn with_dns_override(mut self, host: impl Into<String>, addrs: Vec<SocketAddr>) -> Self {
        let host = host.into();
        self.dns_overrides.retain(|(existing, _)| existing != &host);
        if !addrs.is_empty() {
            self.dns_overrides.push((host, addrs));
        }
        self.http = default_http_client(
            self.timeout,
            &self.dns_overrides,
            self.danger_accept_invalid_certs,
        );
        self.upload_http = upload_http_client(
            self.timeout,
            self.upload_http1_only,
            &self.dns_overrides,
            self.danger_accept_invalid_certs,
        );
        self
    }

    /// Disable TLS certificate validation for diagnostics against controlled targets.
    ///
    /// This is intentionally named with `danger`: do not use it for normal clients.
    pub fn danger_accept_invalid_certs(mut self, accept: bool) -> Self {
        self.danger_accept_invalid_certs = accept;
        self.http = default_http_client(
            self.timeout,
            &self.dns_overrides,
            self.danger_accept_invalid_certs,
        );
        self.upload_http = upload_http_client(
            self.timeout,
            self.upload_http1_only,
            &self.dns_overrides,
            self.danger_accept_invalid_certs,
        );
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
        if let Some(result) = self.upload_binary_batch_to_server(server, items).await? {
            return Ok(Some(result));
        }
        self.upload_json_batch_to_server(server, items).await
    }

    pub async fn upload_binary_batch_to_server(
        &self,
        server: &str,
        items: &[BatchUploadItem],
    ) -> Result<Option<BatchUploadResult>, BlossomError> {
        validate_batch_upload_items(items)?;
        if items.is_empty() {
            return Ok(Some(BatchUploadResult {
                accepted: 0,
                uploaded: 0,
            }));
        }

        let auth_header = self
            .create_upload_auth_for_hashes(items.iter().map(|item| item.hash.as_str()))
            .await?;
        let body = encode_binary_batch_upload(items)?;
        let url = format!("{}/upload/batch-binary", server.trim_end_matches('/'));

        let resp = self
            .send_upload_body_request(UploadBodyRequest {
                method: Method::POST,
                url,
                auth_header,
                content_type: BATCH_UPLOAD_BINARY_CONTENT_TYPE.to_string(),
                extra_headers: upload_redirect_extra_headers(),
                body,
            })
            .await?;

        parse_batch_upload_response(resp, items).await
    }

    pub async fn upload_json_batch_to_server(
        &self,
        server: &str,
        items: &[BatchUploadItem],
    ) -> Result<Option<BatchUploadResult>, BlossomError> {
        validate_batch_upload_items(items)?;
        if items.is_empty() {
            return Ok(Some(BatchUploadResult {
                accepted: 0,
                uploaded: 0,
            }));
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
            .send_upload_body_request(UploadBodyRequest {
                method: Method::POST,
                url,
                auth_header,
                content_type: "application/json".to_string(),
                extra_headers: upload_redirect_extra_headers(),
                body,
            })
            .await?;

        parse_batch_upload_response(resp, items).await
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

    /// Download data from Blossom servers with multi-server hedging and transient retries.
    /// Verifies the SHA-256 hash matches before returning.
    pub async fn download(&self, hash: &str) -> Result<Vec<u8>, BlossomError> {
        match self.try_download_result(hash).await? {
            Some(bytes) => Ok(bytes),
            None => Err(BlossomError::DownloadFailed(
                "not found on any read server".to_string(),
            )),
        }
    }

    /// Download data while distinguishing explicit misses (`Ok(None)`) from
    /// transport, timeout, server, or integrity failures (`Err`).
    ///
    /// Probes the primary read server immediately and hedges to additional
    /// configured read servers if earlier probes stay pending; the first
    /// hash-verified response wins.
    pub async fn try_download_result(&self, hash: &str) -> Result<Option<Vec<u8>>, BlossomError> {
        if self.read_servers.is_empty() {
            return Err(BlossomError::NoServers);
        }

        match self
            .hedge_read_servers(|server| async move {
                self.download_from_server_with_retry(&server, hash).await
            })
            .await
        {
            Ok(bytes) => Ok(Some(bytes)),
            Err(misses) => match misses.into_iter().flatten().last() {
                Some(err) => Err(BlossomError::DownloadFailed(err)),
                None => Ok(None),
            },
        }
    }

    /// Download if available, returns None if not found or unreachable.
    pub async fn try_download(&self, hash: &str) -> Option<Vec<u8>> {
        self.try_download_result(hash).await.ok().flatten()
    }

    /// Fetch multiple blobs from one server via `POST /blob/batch`.
    /// Returns `Ok(None)` when the server does not support the batch endpoint.
    pub async fn download_batch_from_server(
        &self,
        server: &str,
        hashes: &[String],
    ) -> Result<Option<Vec<([u8; 32], Vec<u8>)>>, BlossomError> {
        if hashes.is_empty() {
            return Ok(Some(Vec::new()));
        }

        let mut requested = HashSet::with_capacity(hashes.len());
        for hash_hex in hashes {
            let bytes = hex::decode(hash_hex).map_err(|_| {
                BlossomError::DownloadFailed(format!("invalid batch hash: {hash_hex}"))
            })?;
            let hash: [u8; 32] = bytes.try_into().map_err(|_| {
                BlossomError::DownloadFailed(format!("invalid batch hash: {hash_hex}"))
            })?;
            requested.insert(hash);
        }

        let url = format!("{}/blob/batch", server.trim_end_matches('/'));
        let body = serde_json::to_vec(&UploadCheckRequest { hashes })
            .map_err(|err| BlossomError::DownloadFailed(err.to_string()))?;

        let mut last_error = String::new();
        for attempt in 0..2 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
            let resp = match self
                .http
                .post(&url)
                .header(CONTENT_TYPE, "application/json")
                .header("Accept", BLOB_BATCH_DOWNLOAD_CONTENT_TYPE)
                .body(body.clone())
                .send()
                .await
            {
                Ok(resp) => resp,
                Err(err) => {
                    last_error = format!("{server}: {err}");
                    if Self::is_retryable_reqwest_error(&err) {
                        continue;
                    }
                    return Err(BlossomError::DownloadFailed(last_error));
                }
            };

            let status = resp.status();
            if status == StatusCode::NOT_FOUND
                || status == StatusCode::METHOD_NOT_ALLOWED
                || status == StatusCode::NOT_IMPLEMENTED
            {
                return Ok(None);
            }
            if !status.is_success() {
                last_error = format!("{server} returned {status}");
                if Self::is_retryable_status(status) {
                    continue;
                }
                return Err(BlossomError::DownloadFailed(last_error));
            }

            let raw = match resp.bytes().await {
                Ok(raw) => raw,
                Err(err) => {
                    last_error = format!("{server}: {err}");
                    continue;
                }
            };
            let decoded = decode_binary_batch_download(&raw)?;
            let mut verified = Vec::with_capacity(decoded.len());
            let mut seen = HashSet::with_capacity(decoded.len());
            for (hash, data) in decoded {
                if !requested.contains(&hash) || !seen.insert(hash) {
                    continue;
                }
                let actual = Sha256::digest(&data);
                if actual.as_slice() != hash.as_slice() {
                    return Err(BlossomError::HashMismatch {
                        expected: hex::encode(hash),
                        actual: hex::encode(actual),
                    });
                }
                verified.push((hash, data));
            }
            return Ok(Some(verified));
        }

        Err(BlossomError::DownloadFailed(last_error))
    }

    /// Fetch multiple blobs across configured read servers via `POST /blob/batch`,
    /// hedging to secondary read servers if earlier servers stall.
    /// Returns `Ok(None)` if no read server supports the batch endpoint.
    pub async fn download_batch(
        &self,
        hashes: &[String],
    ) -> Result<Option<Vec<([u8; 32], Vec<u8>)>>, BlossomError> {
        if self.read_servers.is_empty() {
            return Err(BlossomError::NoServers);
        }
        if hashes.is_empty() {
            return Ok(Some(Vec::new()));
        }

        match self
            .hedge_read_servers(|server| async move {
                match self.download_batch_from_server(&server, hashes).await {
                    Ok(Some(items)) if !items.is_empty() => HedgeOutcome::Win(items),
                    Ok(Some(_)) => HedgeOutcome::Continue(Ok(true)),
                    Ok(None) => HedgeOutcome::Continue(Ok(false)),
                    Err(err) => HedgeOutcome::Continue(Err(err)),
                }
            })
            .await
        {
            Ok(items) => Ok(Some(items)),
            Err(results) => {
                let mut saw_supported_empty = false;
                let mut last_error = None;
                for result in results {
                    match result {
                        Ok(true) => saw_supported_empty = true,
                        Ok(false) => {}
                        Err(err) => last_error = Some(err),
                    }
                }
                if saw_supported_empty {
                    Ok(Some(Vec::new()))
                } else if let Some(err) = last_error {
                    Err(err)
                } else {
                    Ok(None)
                }
            }
        }
    }

    async fn hedge_read_servers<T, M, F, Fut>(&self, probe: F) -> Result<T, Vec<M>>
    where
        F: Fn(String) -> Fut,
        Fut: std::future::Future<Output = HedgeOutcome<T, M>>,
    {
        use futures::stream::{FuturesUnordered, StreamExt};

        let mut in_flight = FuturesUnordered::new();
        let mut next = 0usize;
        let mut misses = Vec::new();

        while !in_flight.is_empty() || next < self.read_servers.len() {
            if in_flight.is_empty() {
                in_flight.push(probe(self.read_servers[next].clone()));
                next += 1;
                continue;
            }
            let outcome = if next < self.read_servers.len() {
                tokio::select! {
                    outcome = in_flight.next() => outcome,
                    _ = tokio::time::sleep(DOWNLOAD_HEDGE_DELAY) => {
                        in_flight.push(probe(self.read_servers[next].clone()));
                        next += 1;
                        continue;
                    }
                }
            } else {
                in_flight.next().await
            };
            match outcome {
                Some(HedgeOutcome::Win(value)) => return Ok(value),
                Some(HedgeOutcome::Continue(miss)) => misses.push(miss),
                None => break,
            }
        }

        Err(misses)
    }

    fn is_retryable_reqwest_error(err: &reqwest::Error) -> bool {
        err.is_timeout() || err.is_connect() || err.is_request() || err.is_body()
    }

    async fn download_from_server_with_retry(
        &self,
        server: &str,
        hash: &str,
    ) -> HedgeOutcome<Vec<u8>, Option<String>> {
        let url = format!("{}/{}.bin", server.trim_end_matches('/'), hash);
        let mut last_error = String::new();

        for attempt in 0..DOWNLOAD_MAX_ATTEMPTS {
            if attempt > 0 {
                let delay_ms = if attempt == 1 { 150 } else { 400 };
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }

            match self.http.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => {
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
                                return HedgeOutcome::Win(bytes.to_vec());
                            }
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
                            return HedgeOutcome::Continue(Some(last_error));
                        }
                        Err(e) => {
                            last_error = format!("{server}: {e}");
                        }
                    }
                }
                Ok(resp)
                    if resp.status() == StatusCode::NOT_FOUND
                        || resp.status() == StatusCode::GONE =>
                {
                    debug!(
                        "Download {} from {} returned status {}",
                        hash,
                        server,
                        resp.status()
                    );
                    return HedgeOutcome::Continue(None);
                }
                Ok(resp) => {
                    let status = resp.status();
                    last_error = format!("{server} returned {status}");
                    debug!(
                        "Download {} from {} returned status {}",
                        hash, server, status
                    );
                    if !Self::is_retryable_status(status) {
                        return HedgeOutcome::Continue(Some(last_error));
                    }
                }
                Err(e) => {
                    last_error = format!("{server}: {e}");
                    if !Self::is_retryable_reqwest_error(&e) {
                        return HedgeOutcome::Continue(Some(last_error));
                    }
                }
            }
        }

        HedgeOutcome::Continue(Some(last_error))
    }

    async fn send_upload_body_request(
        &self,
        request: UploadBodyRequest,
    ) -> Result<reqwest::Response, BlossomError> {
        let mut current_url = Url::parse(&request.url)
            .map_err(|err| BlossomError::UploadFailed(format!("invalid upload URL: {err}")))?;

        for redirect_count in 0..=MAX_UPLOAD_REDIRECTS {
            let response = self
                .send_upload_body_once(current_url.as_str(), &request)
                .await?;
            let status = response.status();
            if status != StatusCode::TEMPORARY_REDIRECT && status != StatusCode::PERMANENT_REDIRECT
            {
                return Ok(response);
            }

            if redirect_count == MAX_UPLOAD_REDIRECTS {
                return Err(BlossomError::UploadFailed(format!(
                    "upload redirected more than {MAX_UPLOAD_REDIRECTS} times"
                )));
            }

            let location = response
                .headers()
                .get(LOCATION)
                .ok_or_else(|| {
                    BlossomError::UploadFailed("upload redirect missing Location".to_string())
                })?
                .to_str()
                .map_err(|err| {
                    BlossomError::UploadFailed(format!("invalid upload redirect Location: {err}"))
                })?;
            let next_url = current_url.join(location).map_err(|err| {
                BlossomError::UploadFailed(format!("invalid upload redirect URL: {err}"))
            })?;

            if !trusted_upload_redirect(&current_url, &next_url) {
                return Err(BlossomError::UploadFailed(format!(
                    "refusing upload redirect from {} to {}",
                    current_url, next_url
                )));
            }

            debug!("Following trusted upload redirect to {}", next_url);
            current_url = next_url;
        }

        Err(BlossomError::UploadFailed(
            "unreachable upload redirect loop".to_string(),
        ))
    }

    async fn send_upload_body_once(
        &self,
        url: &str,
        request: &UploadBodyRequest,
    ) -> Result<reqwest::Response, BlossomError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&request.auth_header).map_err(|err| {
                BlossomError::UploadFailed(format!("invalid Authorization header: {err}"))
            })?,
        );
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_str(&request.content_type).map_err(|err| {
                BlossomError::UploadFailed(format!("invalid Content-Type header: {err}"))
            })?,
        );
        for (name, value) in &request.extra_headers {
            headers.insert(
                *name,
                HeaderValue::from_str(value).map_err(|err| {
                    BlossomError::UploadFailed(format!("invalid {name} header: {err}"))
                })?,
            );
        }

        Ok(self
            .upload_http
            .request(request.method.clone(), url)
            .headers(headers)
            .body(request.body.clone())
            .send()
            .await?)
    }

    fn upload_request_error(error: BlossomError) -> UploadAttemptError {
        match error {
            BlossomError::Http(error) => {
                if error.is_timeout() || error.is_connect() || error.is_request() || error.is_body()
                {
                    UploadAttemptError::retryable(error.to_string())
                } else {
                    UploadAttemptError::fatal(error.to_string())
                }
            }
            error => UploadAttemptError::fatal(error.to_string()),
        }
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
            .send_upload_body_request(UploadBodyRequest {
                method: Method::PUT,
                url,
                auth_header: auth_header.to_string(),
                content_type: "application/octet-stream".to_string(),
                extra_headers: vec![("x-sha-256", hash.to_string())],
                body: data.to_vec(),
            })
            .await
            .map_err(Self::upload_request_error)?;

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
        let hashes: Vec<String> = hashes.into_iter().map(|hash| hash.to_lowercase()).collect();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let expiration = now + 300; // 5 minutes

        let mut tags = vec![Tag::custom(
            TagKind::custom("t"),
            vec!["upload".to_string()],
        )];
        if hashes.len() <= 1 {
            tags.extend(
                hashes
                    .iter()
                    .map(|hash| Tag::custom(TagKind::custom("x"), vec![hash.to_string()])),
            );
        } else {
            tags.push(Tag::custom(
                TagKind::custom(BATCH_UPLOAD_HASH_LIST_AUTH_TAG),
                vec![batch_upload_hash_list_digest(
                    hashes.iter().map(String::as_str),
                )?],
            ));
        }
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

/// Decode a binary `POST /blob/batch` response (`HTBDV1\0\0`).
pub fn decode_binary_batch_download(
    bytes: &[u8],
) -> Result<Vec<([u8; 32], Vec<u8>)>, BlossomError> {
    if bytes.len() < 12 {
        return Err(BlossomError::DownloadFailed(
            "batch response too short".to_string(),
        ));
    }
    if &bytes[..8] != BLOB_BATCH_DOWNLOAD_MAGIC {
        return Err(BlossomError::DownloadFailed(
            "invalid batch response magic".to_string(),
        ));
    }

    let count = u32::from_be_bytes(bytes[8..12].try_into().unwrap()) as usize;
    let mut offset = 12usize;
    let mut items = Vec::with_capacity(count.min(4096));

    for _ in 0..count {
        if bytes.len().saturating_sub(offset) < 40 {
            return Err(BlossomError::DownloadFailed(
                "truncated batch response entry header".to_string(),
            ));
        }
        let hash: [u8; 32] = bytes[offset..offset + 32].try_into().unwrap();
        offset += 32;
        let len = u64::from_be_bytes(bytes[offset..offset + 8].try_into().unwrap());
        offset += 8;
        let len = usize::try_from(len).map_err(|_| {
            BlossomError::DownloadFailed("batch response entry length too large".to_string())
        })?;
        if bytes.len().saturating_sub(offset) < len {
            return Err(BlossomError::DownloadFailed(
                "truncated batch response entry body".to_string(),
            ));
        }
        items.push((hash, bytes[offset..offset + len].to_vec()));
        offset += len;
    }

    if offset != bytes.len() {
        return Err(BlossomError::DownloadFailed(
            "batch response has trailing bytes".to_string(),
        ));
    }

    Ok(items)
}

enum HedgeOutcome<T, M> {
    Win(T),
    Continue(M),
}

/// Store implementation for Blossom (read-only, fetches from servers on demand)
#[cfg(feature = "store")]
mod store_impl {
    use super::*;
    use async_trait::async_trait;
    use hashtree_core::{to_hex, Hash, Store, StoreError};
    use std::collections::{BTreeMap, HashMap};
    use std::sync::RwLock;

    const DEFAULT_STORE_CACHE_MAX_BYTES: usize = 64 * 1024 * 1024;

    struct BoundedBlobCache {
        max_bytes: usize,
        total_bytes: usize,
        tick: u64,
        entries: HashMap<Hash, (Vec<u8>, u64)>,
        order: BTreeMap<u64, Hash>,
    }

    impl BoundedBlobCache {
        fn new(max_bytes: usize) -> Self {
            Self {
                max_bytes,
                total_bytes: 0,
                tick: 0,
                entries: HashMap::new(),
                order: BTreeMap::new(),
            }
        }

        fn next_tick(&mut self) -> u64 {
            self.tick = self.tick.wrapping_add(1);
            self.tick
        }

        fn get(&mut self, hash: &Hash) -> Option<Vec<u8>> {
            if self.max_bytes == 0 {
                return None;
            }
            let new_tick = self.next_tick();
            let (data, old_tick) = self.entries.get_mut(hash)?;
            let prev = std::mem::replace(old_tick, new_tick);
            self.order.remove(&prev);
            self.order.insert(new_tick, *hash);
            Some(data.clone())
        }

        fn contains(&self, hash: &Hash) -> bool {
            self.max_bytes > 0 && self.entries.contains_key(hash)
        }

        fn insert(&mut self, hash: Hash, data: Vec<u8>) -> bool {
            if self.entries.contains_key(&hash) {
                return false;
            }
            if self.max_bytes == 0 || data.len() > self.max_bytes {
                return true;
            }
            while self.total_bytes.saturating_add(data.len()) > self.max_bytes {
                let Some((&oldest_tick, &oldest_hash)) = self.order.iter().next() else {
                    break;
                };
                self.order.remove(&oldest_tick);
                if let Some((removed, _)) = self.entries.remove(&oldest_hash) {
                    self.total_bytes = self.total_bytes.saturating_sub(removed.len());
                }
            }
            let tick = self.next_tick();
            self.total_bytes += data.len();
            self.entries.insert(hash, (data, tick));
            self.order.insert(tick, hash);
            true
        }

        fn remove(&mut self, hash: &Hash) -> bool {
            if let Some((data, tick)) = self.entries.remove(hash) {
                self.order.remove(&tick);
                self.total_bytes = self.total_bytes.saturating_sub(data.len());
                true
            } else {
                false
            }
        }
    }

    /// Blossom-backed store (read-only with bounded in-memory LRU cache)
    ///
    /// Fetches data from Blossom servers on demand and caches recently read blobs
    /// up to `max_cache_bytes` (64 MiB by default).
    /// Write operations are cached in memory only (data should be uploaded separately).
    pub struct BlossomStore {
        client: BlossomClient,
        cache: RwLock<BoundedBlobCache>,
    }

    impl BlossomStore {
        pub fn new(client: BlossomClient) -> Self {
            Self {
                client,
                cache: RwLock::new(BoundedBlobCache::new(DEFAULT_STORE_CACHE_MAX_BYTES)),
            }
        }

        /// Create with servers (convenience constructor)
        pub fn with_servers(keys: nostr::Keys, servers: Vec<String>) -> Self {
            let client = BlossomClient::new(keys).with_servers(servers);
            Self::new(client)
        }

        /// Cap the in-memory blob cache in bytes (`0` disables in-memory caching).
        pub fn with_max_cache_bytes(self, max_bytes: usize) -> Self {
            Self {
                client: self.client,
                cache: RwLock::new(BoundedBlobCache::new(max_bytes)),
            }
        }

        /// Current bytes held in the in-memory blob cache.
        pub fn cached_bytes(&self) -> usize {
            self.cache.read().unwrap().total_bytes
        }

        /// Get underlying client
        pub fn client(&self) -> &BlossomClient {
            &self.client
        }
    }

    #[async_trait]
    impl Store for BlossomStore {
        async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
            Ok(self.cache.write().unwrap().insert(hash, data))
        }

        async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
            if let Some(data) = self.cache.write().unwrap().get(hash) {
                return Ok(Some(data));
            }

            let key = to_hex(hash);
            match self.client.try_download_result(&key).await {
                Ok(Some(data)) => {
                    self.cache.write().unwrap().insert(*hash, data.clone());
                    Ok(Some(data))
                }
                Ok(None) => Ok(None),
                Err(err) => Err(StoreError::Other(err.to_string())),
            }
        }

        async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
            if self.cache.read().unwrap().contains(hash) {
                return Ok(true);
            }
            let key = to_hex(hash);
            Ok(self.client.exists(&key).await)
        }

        async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
            Ok(self.cache.write().unwrap().remove(hash))
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

    struct TestRedirectBodyServer {
        url: String,
        saw_redirect_opt_in: Arc<std::sync::atomic::AtomicBool>,
        saw_authorization: Arc<std::sync::atomic::AtomicBool>,
        done: Option<tokio::sync::oneshot::Receiver<()>>,
    }

    #[test]
    fn upload_transport_preference_survives_timeout_rebuild() {
        let default_client = BlossomClient::new_empty(Keys::generate());
        assert!(default_client.upload_http1_only);

        let client = BlossomClient::new_empty(Keys::generate())
            .with_upload_http1_only()
            .with_timeout(Duration::from_secs(60));

        assert!(client.upload_http1_only);

        let client = BlossomClient::new_empty(Keys::generate())
            .with_upload_http2_auto()
            .with_timeout(Duration::from_secs(60));

        assert!(!client.upload_http1_only);
    }

    #[test]
    fn dns_override_survives_timeout_and_transport_rebuilds() {
        let addr = "127.0.0.1:443".parse().unwrap();
        let client = BlossomClient::new_empty(Keys::generate())
            .with_dns_override("upload.example", vec![addr])
            .danger_accept_invalid_certs(true)
            .with_upload_http2_auto()
            .with_timeout(Duration::from_secs(60))
            .with_upload_http1_only();

        assert_eq!(
            client.dns_overrides,
            vec![("upload.example".to_string(), vec![addr])]
        );
        assert!(client.upload_http1_only);
        assert!(client.danger_accept_invalid_certs);
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

    impl TestRedirectBodyServer {
        fn new(success_body: String) -> Self {
            let redirect_listener =
                TcpListener::bind("127.0.0.1:0").expect("bind redirect test server");
            let redirect_addr = redirect_listener.local_addr().expect("redirect local addr");
            let target_listener =
                TcpListener::bind("127.0.0.1:0").expect("bind redirect target server");
            let target_addr = target_listener.local_addr().expect("target local addr");
            let saw_redirect_opt_in = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let saw_authorization = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let saw_redirect_opt_in_for_thread = Arc::clone(&saw_redirect_opt_in);
            let saw_authorization_for_thread = Arc::clone(&saw_authorization);
            let (done_tx, done_rx) = tokio::sync::oneshot::channel();

            thread::spawn(move || {
                let mut buffer = [0u8; 8192];
                let target_url = format!("http://{target_addr}/upload/batch-binary");

                let (mut redirect_stream, _) =
                    redirect_listener.accept().expect("accept redirect request");
                let redirect_request = read_http_request(&mut redirect_stream, &mut buffer);
                let redirect_header_end = redirect_request
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .map(|index| index + 4)
                    .unwrap_or(redirect_request.len());
                let redirect_headers =
                    String::from_utf8_lossy(&redirect_request[..redirect_header_end]);
                let has_redirect_opt_in = redirect_headers.lines().any(|line| {
                    line.to_ascii_lowercase()
                        .starts_with("x-hashtree-upload-redirect:")
                });
                saw_redirect_opt_in_for_thread
                    .store(has_redirect_opt_in, std::sync::atomic::Ordering::SeqCst);
                write!(
                    redirect_stream,
                    "HTTP/1.1 307 Temporary Redirect\r\nLocation: {target_url}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .expect("write redirect response");
                redirect_stream.flush().expect("flush redirect response");

                let (mut target_stream, _) =
                    target_listener.accept().expect("accept redirected request");
                let request = read_http_request(&mut target_stream, &mut buffer);
                let header_end = request
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .map(|index| index + 4)
                    .unwrap_or(request.len());
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let has_authorization = headers
                    .lines()
                    .any(|line| line.to_ascii_lowercase().starts_with("authorization:"));
                saw_authorization_for_thread
                    .store(has_authorization, std::sync::atomic::Ordering::SeqCst);

                if has_authorization {
                    write!(
                        target_stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        success_body.len(),
                        success_body
                    )
                    .expect("write redirected success response");
                } else {
                    let body = r#"{"error":"missing authorization"}"#;
                    write!(
                        target_stream,
                        "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .expect("write redirected failure response");
                }
                target_stream.flush().expect("flush target response");

                let _ = done_tx.send(());
            });

            Self {
                url: format!("http://{redirect_addr}"),
                saw_redirect_opt_in,
                saw_authorization,
                done: Some(done_rx),
            }
        }

        async fn wait_for_requests(&mut self) {
            if let Some(done) = self.done.take() {
                let _ = done.await;
            }
        }

        fn saw_authorization(&self) -> bool {
            self.saw_authorization
                .load(std::sync::atomic::Ordering::SeqCst)
        }

        fn saw_redirect_opt_in(&self) -> bool {
            self.saw_redirect_opt_in
                .load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    fn read_http_request(stream: &mut std::net::TcpStream, buffer: &mut [u8; 8192]) -> Vec<u8> {
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
                    request.extend_from_slice(&buffer[..bytes]);
                    remaining = remaining.saturating_sub(bytes);
                }
            }
        }

        request
    }

    fn drain_http_request(stream: &mut std::net::TcpStream, buffer: &mut [u8; 8192]) {
        let _ = read_http_request(stream, buffer);
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
    async fn test_upload_binary_batch_to_server_accepts_batch_response() {
        let first = b"first binary batch blob".to_vec();
        let second = b"second binary batch blob".to_vec();
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
            .upload_binary_batch_to_server(&server.url, &items)
            .await
            .expect("binary batch upload")
            .expect("binary batch endpoint supported");

        assert_eq!(result.accepted, 2);
        assert_eq!(result.uploaded, 2);
        server.wait_for_requests().await;
    }

    #[tokio::test]
    async fn test_upload_binary_batch_to_server_preserves_auth_on_trusted_redirect() {
        let data = b"redirected binary batch blob".to_vec();
        let hash = compute_sha256(&data);
        let response = format!(r#"{{"uploaded":1,"blobs":[{{"sha256":"{}"}}]}}"#, hash);
        let mut server = TestRedirectBodyServer::new(response);
        let keys = Keys::generate();
        let client = BlossomClient::new_empty(keys);

        let result = client
            .upload_binary_batch_to_server(&server.url, &[BatchUploadItem::new(hash, data)])
            .await
            .expect("redirected binary batch upload")
            .expect("binary batch endpoint supported");

        assert_eq!(result.accepted, 1);
        assert_eq!(result.uploaded, 1);
        server.wait_for_requests().await;
        assert!(!server.saw_redirect_opt_in());
        assert!(server.saw_authorization());
    }

    #[test]
    fn test_upload_redirect_trust_rejects_cross_host_https() {
        let from = Url::parse("https://upload.example/upload").unwrap();
        let to = Url::parse("https://upload-direct.example/upload").unwrap();

        assert!(!trusted_upload_redirect(&from, &to));
    }

    #[test]
    fn test_upload_redirect_trust_allows_same_host_https_upgrade() {
        let from = Url::parse("http://upload.example/upload").unwrap();
        let to = Url::parse("https://upload.example/upload").unwrap();

        assert!(trusted_upload_redirect(&from, &to));
    }

    #[test]
    fn test_encode_binary_batch_upload_body_shape() {
        let data = b"binary body shape".to_vec();
        let hash = compute_sha256(&data);
        let mut item = BatchUploadItem::new(hash.clone(), data.clone());
        item.content_type = Some("application/octet-stream".to_string());

        let body = encode_binary_batch_upload(&[item]).expect("binary batch body");
        let expected_hash = hex::decode(hash).expect("hash bytes");
        let content_type = b"application/octet-stream";
        let header_len = BINARY_BATCH_UPLOAD_MAGIC.len() + 4;

        assert_eq!(
            &body[..BINARY_BATCH_UPLOAD_MAGIC.len()],
            BINARY_BATCH_UPLOAD_MAGIC
        );
        assert_eq!(
            u32::from_be_bytes(
                body[BINARY_BATCH_UPLOAD_MAGIC.len()..header_len]
                    .try_into()
                    .unwrap()
            ),
            1
        );
        assert_eq!(&body[header_len..header_len + 32], expected_hash.as_slice());
        assert_eq!(
            u16::from_be_bytes(body[header_len + 32..header_len + 34].try_into().unwrap()),
            content_type.len() as u16
        );
        assert_eq!(
            u64::from_be_bytes(body[header_len + 34..header_len + 42].try_into().unwrap()),
            data.len() as u64
        );
        assert_eq!(
            &body[header_len + 42..header_len + 42 + content_type.len()],
            content_type
        );
        assert_eq!(
            &body[header_len + 42 + content_type.len()..],
            data.as_slice()
        );
    }

    #[tokio::test]
    async fn test_batch_upload_auth_uses_compact_hash_list_digest() {
        let keys = Keys::generate();
        let client = BlossomClient::new_empty(keys);
        let hashes = (0..128)
            .map(|index| {
                let mut data = vec![0u8; 32];
                data[0] = index as u8;
                hex::encode(data)
            })
            .collect::<Vec<_>>();

        let auth = client
            .create_upload_auth_for_hashes(hashes.iter().map(String::as_str))
            .await
            .expect("compact upload auth");
        let encoded = auth.strip_prefix("Nostr ").expect("nostr auth scheme");
        let event_bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("decode auth event");
        let event: serde_json::Value =
            serde_json::from_slice(&event_bytes).expect("parse auth event");
        let tags = event["tags"].as_array().expect("event tags");
        let x_tags = tags
            .iter()
            .filter(|tag| tag.get(0).and_then(|value| value.as_str()) == Some("x"))
            .count();
        let batch_tags = tags
            .iter()
            .filter(|tag| {
                tag.get(0).and_then(|value| value.as_str()) == Some(BATCH_UPLOAD_HASH_LIST_AUTH_TAG)
            })
            .collect::<Vec<_>>();
        let expected_digest = batch_upload_hash_list_digest(hashes.iter().map(String::as_str))
            .expect("expected digest");

        assert_eq!(x_tags, 0);
        assert_eq!(batch_tags.len(), 1);
        assert_eq!(
            batch_tags[0].get(1).and_then(|value| value.as_str()),
            Some(expected_digest.as_str())
        );
        assert!(
            auth.len() < 1_500,
            "compact auth header should stay small, got {} bytes",
            auth.len()
        );
    }

    #[tokio::test]
    async fn test_single_upload_auth_keeps_legacy_x_tag() {
        let keys = Keys::generate();
        let client = BlossomClient::new_empty(keys);
        let hash = "11".repeat(32);

        let auth = client
            .create_upload_auth_for_hashes(std::iter::once(hash.as_str()))
            .await
            .expect("single upload auth");
        let encoded = auth.strip_prefix("Nostr ").expect("nostr auth scheme");
        let event_bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("decode auth event");
        let event: serde_json::Value =
            serde_json::from_slice(&event_bytes).expect("parse auth event");
        let tags = event["tags"].as_array().expect("event tags");
        let x_tags = tags
            .iter()
            .filter(|tag| tag.get(0).and_then(|value| value.as_str()) == Some("x"))
            .collect::<Vec<_>>();
        let batch_tags = tags
            .iter()
            .filter(|tag| {
                tag.get(0).and_then(|value| value.as_str()) == Some(BATCH_UPLOAD_HASH_LIST_AUTH_TAG)
            })
            .count();

        assert_eq!(x_tags.len(), 1);
        assert_eq!(
            x_tags[0].get(1).and_then(|value| value.as_str()),
            Some(hash.as_str())
        );
        assert_eq!(batch_tags, 0);
    }

    #[tokio::test]
    async fn test_upload_batch_to_server_returns_none_when_unsupported() {
        let mut server = TestBodyServer::new(vec![
            (404, r#"{"error":"not found"}"#),
            (404, r#"{"error":"not found"}"#),
        ]);
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

    fn spawn_download_server(
        responses: Vec<(u16, Duration, Vec<u8>)>,
    ) -> (String, Arc<AtomicUsize>, tokio::sync::oneshot::Receiver<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let count = Arc::new(AtomicUsize::new(0));
        let count_thread = Arc::clone(&count);
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        thread::spawn(move || {
            let mut buffer = [0u8; 8192];
            for (status, delay, body) in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                count_thread.fetch_add(1, Ordering::SeqCst);
                drain_http_request(&mut stream, &mut buffer);
                if !delay.is_zero() {
                    thread::sleep(delay);
                }
                let reason = match status {
                    200 => "OK",
                    404 => "Not Found",
                    503 => "Service Unavailable",
                    _ => "Test",
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(&body);
                let _ = stream.flush();
            }
            let _ = done_tx.send(());
        });
        (format!("http://{addr}"), count, done_rx)
    }

    #[tokio::test]
    async fn test_download_hedges_past_stalled_primary_server() {
        let payload = b"hedged blossom payload".to_vec();
        let hash = compute_sha256(&payload);
        let (slow_url, _, _) =
            spawn_download_server(vec![(200, Duration::from_secs(3), payload.clone())]);
        let (fast_url, fast_reqs, fast_done) =
            spawn_download_server(vec![(200, Duration::ZERO, payload.clone())]);

        let client = BlossomClient::new_empty(Keys::generate())
            .with_read_servers(vec![slow_url, fast_url])
            .with_timeout(Duration::from_secs(5));

        let started = std::time::Instant::now();
        let got = client.download(&hash).await.expect("hedged hit");
        let elapsed = started.elapsed();

        assert_eq!(got, payload);
        assert!(
            elapsed < Duration::from_secs(1),
            "expected hedge to win well before 3s stall, took {elapsed:?}"
        );
        let _ = fast_done.await;
        assert_eq!(fast_reqs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_download_retries_transient_503_and_distinguishes_404_from_error() {
        let payload = b"recovered after 503".to_vec();
        let hash = compute_sha256(&payload);
        let (flaky_url, flaky_reqs, flaky_done) = spawn_download_server(vec![
            (503, Duration::ZERO, b"busy".to_vec()),
            (200, Duration::ZERO, payload.clone()),
        ]);
        let client = BlossomClient::new_empty(Keys::generate()).with_read_servers(vec![flaky_url]);
        let got = client.download(&hash).await.expect("recovered");
        assert_eq!(got, payload);
        let _ = flaky_done.await;
        assert_eq!(flaky_reqs.load(Ordering::SeqCst), 2);

        let (miss_url, miss_reqs, miss_done) =
            spawn_download_server(vec![(404, Duration::ZERO, b"missing".to_vec())]);
        let miss_client =
            BlossomClient::new_empty(Keys::generate()).with_read_servers(vec![miss_url]);
        assert_eq!(
            miss_client
                .try_download_result(&hash)
                .await
                .expect("explicit 404 is Ok(None)"),
            None
        );
        let _ = miss_done.await;
        assert_eq!(miss_reqs.load(Ordering::SeqCst), 1, "404 must not retry");

        let (err_url, _, err_done) = spawn_download_server(vec![
            (503, Duration::ZERO, b"err".to_vec()),
            (503, Duration::ZERO, b"err".to_vec()),
            (503, Duration::ZERO, b"err".to_vec()),
        ]);
        let err_client =
            BlossomClient::new_empty(Keys::generate()).with_read_servers(vec![err_url]);
        assert!(
            err_client.try_download_result(&hash).await.is_err(),
            "persistent 503 must return Err, not fake Ok(None)"
        );
        let _ = err_done.await;
    }

    #[tokio::test]
    async fn test_download_batch_verifies_hashes_and_detects_unsupported() {
        let a = b"batch-alpha".to_vec();
        let b = b"batch-beta".to_vec();
        let ha: [u8; 32] = Sha256::digest(&a).into();
        let hb: [u8; 32] = Sha256::digest(&b).into();

        let mut frame = Vec::new();
        frame.extend_from_slice(BLOB_BATCH_DOWNLOAD_MAGIC);
        frame.extend_from_slice(&2u32.to_be_bytes());
        for (h, d) in [(ha, &a), (hb, &b)] {
            frame.extend_from_slice(&h);
            frame.extend_from_slice(&(d.len() as u64).to_be_bytes());
            frame.extend_from_slice(d);
        }

        let (batch_url, _, done) = spawn_download_server(vec![(200, Duration::ZERO, frame)]);
        let client = BlossomClient::new_empty(Keys::generate()).with_read_servers(vec![batch_url]);
        let items = client
            .download_batch(&[hex::encode(ha), hex::encode(hb)])
            .await
            .expect("batch ok")
            .expect("batch supported");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0], (ha, a));
        assert_eq!(items[1], (hb, b));
        let _ = done.await;

        let (plain_url, _, plain_done) =
            spawn_download_server(vec![(404, Duration::ZERO, b"no batch".to_vec())]);
        let plain_client =
            BlossomClient::new_empty(Keys::generate()).with_read_servers(vec![plain_url]);
        assert!(plain_client
            .download_batch(&[hex::encode(ha)])
            .await
            .expect("404 is Ok(None)")
            .is_none());
        let _ = plain_done.await;
    }

    #[cfg(feature = "store")]
    #[tokio::test]
    async fn test_blossom_store_cache_is_bounded() {
        use hashtree_core::Store;

        let store =
            BlossomStore::new(BlossomClient::new_empty(Keys::generate())).with_max_cache_bytes(10);
        let h1 = [1u8; 32];
        let h2 = [2u8; 32];
        let h3 = [3u8; 32];
        store.put(h1, vec![1; 6]).await.unwrap();
        store.put(h2, vec![2; 6]).await.unwrap();
        assert!(store.cached_bytes() <= 10);
        assert!(!store.has(&h1).await.unwrap(), "oldest entry evicted");
        assert!(store.has(&h2).await.unwrap());

        let uncached =
            BlossomStore::new(BlossomClient::new_empty(Keys::generate())).with_max_cache_bytes(0);
        uncached.put(h3, vec![3; 6]).await.unwrap();
        assert_eq!(uncached.cached_bytes(), 0);
    }
}
