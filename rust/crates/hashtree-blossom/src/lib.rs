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
use sha2::{Digest, Sha256};
use std::time::Duration;
use thiserror::Error;
use tracing::{debug, warn};

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

impl BlossomClient {
    /// Create a new client with the given keys
    /// Automatically loads server config from ~/.hashtree/config.toml
    /// and prioritizes local daemon if running
    #[cfg(feature = "config")]
    pub fn new(keys: Keys) -> Self {
        let config = hashtree_config::Config::load_or_default();
        let mut read_servers = config.blossom.all_read_servers();

        // Prioritize local daemon if running
        if let Some(local_url) = hashtree_config::detect_local_daemon_url(Some(&config.server.bind_address)) {
            if !read_servers.iter().any(|s| s == &local_url) {
                debug!("Local daemon detected at {}, prioritizing for reads", local_url);
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
        self.http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .unwrap();
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
            match self.upload_to_server(server, data, &hash, &auth_header).await {
                Ok(_) => {
                    debug!("Uploaded {} to {}", &hash[..12], server);
                    return Ok(hash);
                }
                Err(e) => {
                    warn!("Upload to {} failed: {}", server, e);
                    continue;
                }
            }
        }

        Err(BlossomError::UploadFailed("all servers failed".to_string()))
    }

    /// Upload data only if it doesn't already exist
    /// Returns (hash, was_uploaded) tuple
    ///
    /// For small files (<256KB), skips existence check and relies on server returning 409.
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

        const MAX_RETRIES: u32 = 3;
        let mut last_error = String::new();

        for attempt in 0..MAX_RETRIES {
            if attempt > 0 {
                // Exponential backoff: 100ms, 200ms, 400ms
                let delay = Duration::from_millis(100 * (1 << (attempt - 1)));
                debug!("Retrying upload {} (attempt {}/{}), waiting {:?}",
                       &hash[..12], attempt + 1, MAX_RETRIES, delay);
                tokio::time::sleep(delay).await;
            }

            // Regenerate auth header for each retry (in case of expiration)
            let auth_header = self.create_upload_auth(&hash).await?;

            for server in &self.write_servers {
                match self.upload_to_server(server, data, &hash, &auth_header).await {
                    Ok(was_new) => {
                        if was_new {
                            debug!("Uploaded {} to {}", &hash[..12], server);
                        } else {
                            debug!("Blob {} already exists on {}", &hash[..12], server);
                        }
                        return Ok((hash, was_new));
                    }
                    Err(e) => {
                        last_error = format!("{}: {}", server, e);
                        warn!("Upload to {} failed: {}", server, e);
                        continue;
                    }
                }
            }
        }

        Err(BlossomError::UploadFailed(format!(
            "all servers failed after {} retries (last: {})",
            MAX_RETRIES, last_error
        )))
    }

    /// Check if a blob exists on any write server
    pub async fn exists(&self, hash: &str) -> bool {
        for server in &self.write_servers {
            if self.exists_on_server(hash, server).await {
                return true;
            }
        }
        false
    }

    /// Check if a blob exists on a specific server
    pub async fn exists_on_server(&self, hash: &str, server: &str) -> bool {
        let url = format!("{}/{}.bin", server.trim_end_matches('/'), hash);
        debug!("Checking exists: {}", url);
        if let Ok(resp) = self.http.head(&url).send().await {
            debug!("  -> status: {}", resp.status());
            if resp.status().is_success() {
                // Verify content-type is binary, not HTML error page
                if let Some(ct) = resp.headers().get("content-type") {
                    if let Ok(ct_str) = ct.to_str() {
                        if ct_str.starts_with("text/html") {
                            return false;
                        }
                    }
                }
                // Verify content-length > 0
                if let Some(cl) = resp.headers().get("content-length") {
                    if let Ok(cl_str) = cl.to_str() {
                        if cl_str == "0" {
                            return false;
                        }
                    }
                }
                return true;
            }
        }
        false
    }

    /// Check if server has a tree by sampling hashes (parallel checks)
    pub async fn server_has_tree_samples(&self, server: &str, hashes: &[&str], sample_size: usize) -> bool {
        use futures::future::join_all;
        if hashes.is_empty() {
            return false;
        }
        // Spread samples across the hash list
        let step = (hashes.len() / sample_size.min(hashes.len())).max(1);
        let samples: Vec<_> = hashes.iter().step_by(step).take(sample_size).collect();
        let checks: Vec<_> = samples.iter().map(|h| self.exists_on_server(h, server)).collect();
        join_all(checks).await.iter().all(|&exists| exists)
    }

    /// Upload to all write servers in parallel, returns (hash, success_count)
    pub async fn upload_to_all_servers(&self, data: &[u8]) -> Result<(String, usize), BlossomError> {
        use futures::future::join_all;
        if self.write_servers.is_empty() {
            return Err(BlossomError::NoServers);
        }
        let hash = compute_sha256(data);
        let auth = self.create_upload_auth(&hash).await?;
        let uploads: Vec<_> = self.write_servers.iter()
            .map(|s| self.upload_to_server(s, data, &hash, &auth))
            .collect();
        let results = join_all(uploads).await;
        let ok_count = results.iter().filter(|r| r.is_ok()).count();
        if ok_count == 0 {
            return Err(BlossomError::UploadFailed("all servers failed".to_string()));
        }
        Ok((hash, ok_count))
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
                    let x_source = resp.headers()
                        .get("x-source")
                        .and_then(|v| v.to_str().ok())
                        .map(|s| s.to_string());

                    match resp.bytes().await {
                        Ok(bytes) => {
                            let computed = compute_sha256(&bytes);
                            if computed == hash {
                                if let Some(source) = x_source {
                                    debug!("Downloaded {} ({} bytes) via {} [source: {}]",
                                        &hash[..12.min(hash.len())], bytes.len(), server, source);
                                } else {
                                    debug!("Downloaded {} ({} bytes) from {}",
                                        &hash[..12.min(hash.len())], bytes.len(), server);
                                }
                                return Ok(bytes.to_vec());
                            } else {
                                last_error = format!("hash mismatch from {}: expected {}, got {} ({} bytes received)",
                                    server, hash, computed, bytes.len());
                                warn!("Hash mismatch downloading {} from {}: got {} ({} bytes)",
                                    hash, server, &computed[..12.min(computed.len())], bytes.len());
                            }
                        }
                        Err(e) => {
                            last_error = e.to_string();
                        }
                    }
                }
                Ok(resp) => {
                    last_error = format!("{} returned {}", server, resp.status());
                    debug!("Download {} from {} returned status {}", hash, server, resp.status());
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
    /// Returns Ok(true) if uploaded, Ok(false) if already exists (409)
    async fn upload_to_server(
        &self,
        server: &str,
        data: &[u8],
        hash: &str,
        auth_header: &str,
    ) -> Result<bool, BlossomError> {
        let url = format!("{}/upload", server.trim_end_matches('/'));

        let resp = self
            .http
            .put(&url)
            .header("Authorization", auth_header)
            .header("Content-Type", "application/octet-stream")
            .header("X-SHA-256", hash)
            .body(data.to_vec())
            .send()
            .await?;

        let status = resp.status();
        if status.is_success() {
            Ok(true) // Actually uploaded
        } else if status.as_u16() == 409 {
            Ok(false) // Already exists
        } else {
            let text = resp.text().await.unwrap_or_default();
            Err(BlossomError::UploadFailed(format!("{}: {}", status, text)))
        }
    }

    async fn create_upload_auth(&self, hash: &str) -> Result<String, BlossomError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let expiration = now + 300; // 5 minutes

        let tags = vec![
            Tag::custom(TagKind::custom("t"), vec!["upload".to_string()]),
            Tag::custom(TagKind::custom("x"), vec![hash.to_string()]),
            Tag::custom(
                TagKind::custom("expiration"),
                vec![expiration.to_string()],
            ),
        ];
        let event = EventBuilder::new(Kind::Custom(24242), "Upload", tags)
            .to_event(&self.keys)
            .map_err(|e| BlossomError::Signing(e.to_string()))?;

        let json = event.as_json();
        let encoded = base64::engine::general_purpose::STANDARD.encode(json);
        Ok(format!("Nostr {}", encoded))
    }
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
        let client = BlossomClient::new(keys)
            .with_servers(vec!["https://example.com".to_string()]);

        // Method should exist and return bool
        let result = client.exists_on_server("abc123", "https://example.com").await;
        assert!(!result); // Non-existent hash
    }

    #[tokio::test]
    async fn test_server_has_tree_samples() {
        let keys = Keys::generate();
        let client = BlossomClient::new(keys)
            .with_servers(vec!["https://example.com".to_string()]);

        let hashes = vec!["hash1", "hash2", "hash3"];
        // Method should exist and check samples
        let result = client.server_has_tree_samples("https://example.com", &hashes, 3).await;
        assert!(!result); // Non-existent hashes
    }

    #[tokio::test]
    async fn test_upload_to_all_servers() {
        let keys = Keys::generate();
        let client = BlossomClient::new(keys)
            .with_servers(vec!["https://example1.com".to_string(), "https://example2.com".to_string()]);

        // Method should exist and return (hash, server_count)
        // Will fail since servers don't exist, but should compile
        let result = client.upload_to_all_servers(b"test data").await;
        assert!(result.is_err()); // Expected to fail - servers don't exist
    }

    #[test]
    fn test_local_daemon_priority() {
        let keys = Keys::generate();
        let client = BlossomClient::new_empty(keys)
            .with_servers(vec!["https://remote1.com".to_string(), "https://remote2.com".to_string()])
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
            .with_servers(vec!["http://127.0.0.1:8080".to_string(), "https://remote.com".to_string()])
            .with_local_daemon("http://127.0.0.1:8080".to_string());

        // Should not duplicate
        assert_eq!(client.read_servers().len(), 2);
        assert_eq!(client.read_servers()[0], "http://127.0.0.1:8080");
    }
}
