//! S3-backed storage for hashtree with non-blocking uploads.
//!
//! This crate provides an S3 storage backend that:
//! - Stores data locally first (fast writes)
//! - Syncs to S3 in the background (non-blocking)
//! - Falls back to S3 if data not in local cache
//!
//! # Example
//!
//! ```ignore
//! use hashtree_s3::{S3Store, S3Config};
//! use hashtree_core::store::MemoryStore;
//! use std::sync::Arc;
//!
//! let local_store = Arc::new(MemoryStore::new());
//! let config = S3Config {
//!     bucket: "my-bucket".to_string(),
//!     prefix: Some("blobs/".to_string()),
//!     region: None, // Uses AWS_REGION env var
//!     endpoint: None, // For S3-compatible services
//! };
//!
//! let s3_store = S3Store::new(local_store, config).await?;
//! ```

use async_trait::async_trait;
use aws_sdk_s3::Client as S3Client;
use aws_sdk_s3::primitives::ByteStream;
use hashtree_core::store::{Store, StoreError};
use hashtree_core::types::{to_hex, Hash};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

/// S3 configuration
#[derive(Debug, Clone)]
pub struct S3Config {
    /// S3 bucket name
    pub bucket: String,
    /// Optional prefix for all keys (e.g., "blobs/")
    pub prefix: Option<String>,
    /// AWS region (defaults to AWS_REGION env var)
    pub region: Option<String>,
    /// Custom endpoint URL (for S3-compatible services like MinIO, R2, etc.)
    pub endpoint: Option<String>,
}

/// Background sync task message
enum SyncMessage {
    /// Upload blob to S3
    Upload { hash: Hash, data: Vec<u8> },
    /// Delete blob from S3
    Delete { hash: Hash },
    /// Shutdown the sync task
    Shutdown,
}

/// S3-backed store with local caching and background sync.
///
/// Writes go to the local store first (fast), then are synced to S3 in the background.
/// Reads check local store first, then fall back to S3.
pub struct S3Store<L: Store> {
    /// Local store for fast access
    local: Arc<L>,
    /// S3 client
    s3_client: S3Client,
    /// S3 bucket name
    bucket: String,
    /// Key prefix
    prefix: String,
    /// Channel to send sync messages to background task
    sync_tx: mpsc::UnboundedSender<SyncMessage>,
}

impl<L: Store + 'static> S3Store<L> {
    /// Create a new S3 store wrapping a local store.
    ///
    /// Spawns a background task for non-blocking S3 uploads.
    pub async fn new(local: Arc<L>, config: S3Config) -> Result<Self, S3StoreError> {
        // Build AWS config
        let mut aws_config_loader = aws_config::from_env();

        if let Some(ref region) = config.region {
            aws_config_loader = aws_config_loader.region(aws_sdk_s3::config::Region::new(region.clone()));
        }

        let aws_config = aws_config_loader.load().await;

        // Build S3 client
        let mut s3_config_builder = aws_sdk_s3::config::Builder::from(&aws_config);

        if let Some(ref endpoint) = config.endpoint {
            s3_config_builder = s3_config_builder
                .endpoint_url(endpoint)
                .force_path_style(true); // Required for most S3-compatible services
        }

        let s3_client = S3Client::from_conf(s3_config_builder.build());

        let prefix = config.prefix.unwrap_or_default();
        let bucket = config.bucket.clone();

        // Create channel for background sync
        let (sync_tx, sync_rx) = mpsc::unbounded_channel();

        // Spawn background sync task
        let sync_client = s3_client.clone();
        let sync_bucket = bucket.clone();
        let sync_prefix = prefix.clone();

        tokio::spawn(async move {
            Self::sync_task(sync_rx, sync_client, sync_bucket, sync_prefix).await;
        });

        info!("S3Store initialized with bucket: {}, prefix: {}", bucket, prefix);

        Ok(Self {
            local,
            s3_client,
            bucket,
            prefix,
            sync_tx,
        })
    }

    /// Background task that handles S3 uploads/deletes
    async fn sync_task(
        mut rx: mpsc::UnboundedReceiver<SyncMessage>,
        client: S3Client,
        bucket: String,
        prefix: String,
    ) {
        info!("S3 sync task started");

        while let Some(msg) = rx.recv().await {
            match msg {
                SyncMessage::Upload { hash, data } => {
                    let key = format!("{}{}", prefix, to_hex(&hash));
                    debug!("S3 uploading {} ({} bytes)", &key[..16.min(key.len())], data.len());

                    match client
                        .put_object()
                        .bucket(&bucket)
                        .key(&key)
                        .body(ByteStream::from(data))
                        .send()
                        .await
                    {
                        Ok(_) => {
                            debug!("S3 upload complete: {}", &key[..16.min(key.len())]);
                        }
                        Err(e) => {
                            error!("S3 upload failed for {}: {}", &key[..16.min(key.len())], e);
                        }
                    }
                }
                SyncMessage::Delete { hash } => {
                    let key = format!("{}{}", prefix, to_hex(&hash));
                    debug!("S3 deleting {}", &key[..16.min(key.len())]);

                    match client
                        .delete_object()
                        .bucket(&bucket)
                        .key(&key)
                        .send()
                        .await
                    {
                        Ok(_) => {
                            debug!("S3 delete complete: {}", &key[..16.min(key.len())]);
                        }
                        Err(e) => {
                            error!("S3 delete failed for {}: {}", &key[..16.min(key.len())], e);
                        }
                    }
                }
                SyncMessage::Shutdown => {
                    info!("S3 sync task shutting down");
                    break;
                }
            }
        }
    }

    /// Get the S3 key for a hash
    fn s3_key(&self, hash: &Hash) -> String {
        format!("{}{}", self.prefix, to_hex(hash))
    }

    /// Fetch from S3 directly (used when local miss)
    async fn fetch_from_s3(&self, hash: &Hash) -> Result<Option<Vec<u8>>, S3StoreError> {
        let key = self.s3_key(hash);

        match self.s3_client
            .get_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
        {
            Ok(output) => {
                let data = output.body.collect().await
                    .map_err(|e| S3StoreError::S3(format!("Failed to read body: {}", e)))?;
                Ok(Some(data.into_bytes().to_vec()))
            }
            Err(e) => {
                // Check if it's a "not found" error
                let service_err = e.into_service_error();
                if service_err.is_no_such_key() {
                    Ok(None)
                } else {
                    Err(S3StoreError::S3(format!("S3 get failed: {}", service_err)))
                }
            }
        }
    }

    /// Check if exists in S3 directly
    async fn exists_in_s3(&self, hash: &Hash) -> Result<bool, S3StoreError> {
        let key = self.s3_key(hash);

        match self.s3_client
            .head_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(e) => {
                let service_err = e.into_service_error();
                if service_err.is_not_found() {
                    Ok(false)
                } else {
                    Err(S3StoreError::S3(format!("S3 head failed: {}", service_err)))
                }
            }
        }
    }

    /// Queue a blob for upload to S3 (non-blocking)
    fn queue_upload(&self, hash: Hash, data: Vec<u8>) {
        if let Err(e) = self.sync_tx.send(SyncMessage::Upload { hash, data }) {
            warn!("Failed to queue S3 upload: {}", e);
        }
    }

    /// Queue a blob for deletion from S3 (non-blocking)
    fn queue_delete(&self, hash: Hash) {
        if let Err(e) = self.sync_tx.send(SyncMessage::Delete { hash }) {
            warn!("Failed to queue S3 delete: {}", e);
        }
    }

    /// Shutdown the background sync task
    pub fn shutdown(&self) {
        let _ = self.sync_tx.send(SyncMessage::Shutdown);
    }
}

impl<L: Store> Drop for S3Store<L> {
    fn drop(&mut self) {
        let _ = self.sync_tx.send(SyncMessage::Shutdown);
    }
}

#[async_trait]
impl<L: Store + 'static> Store for S3Store<L> {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        // Store locally first (fast)
        let is_new = self.local.put(hash, data.clone()).await?;

        // Queue for S3 upload in background (non-blocking)
        if is_new {
            self.queue_upload(hash, data);
        }

        Ok(is_new)
    }

    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        // Try local first
        if let Some(data) = self.local.get(hash).await? {
            return Ok(Some(data));
        }

        // Fall back to S3
        match self.fetch_from_s3(hash).await {
            Ok(Some(data)) => {
                // Cache locally for future access
                let _ = self.local.put(*hash, data.clone()).await;
                Ok(Some(data))
            }
            Ok(None) => Ok(None),
            Err(e) => {
                warn!("S3 fetch failed, returning None: {}", e);
                Ok(None)
            }
        }
    }

    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        // Check local first
        if self.local.has(hash).await? {
            return Ok(true);
        }

        // Check S3
        match self.exists_in_s3(hash).await {
            Ok(exists) => Ok(exists),
            Err(e) => {
                warn!("S3 exists check failed, returning false: {}", e);
                Ok(false)
            }
        }
    }

    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        // Delete locally
        let deleted = self.local.delete(hash).await?;

        // Queue S3 deletion in background
        self.queue_delete(*hash);

        Ok(deleted)
    }
}

/// S3 store specific errors
#[derive(Debug, thiserror::Error)]
pub enum S3StoreError {
    #[error("S3 error: {0}")]
    S3(String),
    #[error("Configuration error: {0}")]
    Config(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashtree_core::store::MemoryStore;
    use hashtree_core::hash::sha256;

    #[test]
    fn test_s3_key_generation() {
        // Just test that prefix is applied correctly
        let prefix = "blobs/";
        let hash = sha256(b"test");
        let key = format!("{}{}", prefix, to_hex(&hash));
        assert!(key.starts_with("blobs/"));
        assert_eq!(key.len(), 6 + 64); // "blobs/" + 64 hex chars
    }

    #[test]
    fn test_s3_config() {
        let config = S3Config {
            bucket: "test-bucket".to_string(),
            prefix: Some("data/".to_string()),
            region: Some("us-east-1".to_string()),
            endpoint: None,
        };

        assert_eq!(config.bucket, "test-bucket");
        assert_eq!(config.prefix, Some("data/".to_string()));
    }
}
