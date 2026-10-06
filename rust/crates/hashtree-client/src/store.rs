use crate::ClientConfig;
use anyhow::{ensure, Result};
use async_trait::async_trait;
use futures::{stream::FuturesUnordered, StreamExt};
use hashtree_core::{Hash, Store, StoreError};
use hashtree_fs::FsBlobStore;
use sha2::{Digest, Sha256};
use std::{path::Path, time::Duration};

const MAX_BLOCK: usize = 4 * 1024 * 1024;

pub struct BlobStore {
    cache: FsBlobStore,
    config: ClientConfig,
    pub(crate) http: reqwest::Client,
}

fn verify(hash: &Hash, bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.len() <= MAX_BLOCK && Sha256::digest(bytes).as_slice() == hash,
        "invalid Hashtree block hash or size"
    );
    Ok(())
}

impl BlobStore {
    pub fn new(cache: &Path, config: ClientConfig) -> Result<Self> {
        Ok(Self {
            cache: FsBlobStore::new(cache)?,
            http: reqwest::Client::builder()
                .timeout(config.request_timeout)
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            config,
        })
    }

    async fn fetch(&self, server: &str, hash: &Hash) -> Result<Option<Vec<u8>>> {
        let response = self
            .http
            .get(format!(
                "{}/{}.bin",
                server.trim_end_matches('/'),
                hex::encode(hash)
            ))
            .send()
            .await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let response = response.error_for_status()?;
        ensure!(
            response.status().is_success(),
            "unexpected Hashtree HTTP status: {}",
            response.status()
        );
        ensure!(
            response
                .content_length()
                .is_none_or(|n| n <= MAX_BLOCK as u64),
            "oversized Hashtree block"
        );
        let mut body = response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(part) = body.next().await {
            let part = part?;
            ensure!(
                bytes.len().saturating_add(part.len()) <= MAX_BLOCK,
                "oversized Hashtree block"
            );
            bytes.extend_from_slice(&part);
        }
        verify(hash, &bytes)?;
        Ok(Some(bytes))
    }

    async fn retrieve(&self, hash: &Hash) -> Result<Option<Vec<u8>>> {
        if let Some(bytes) = self.cache.get(hash).await? {
            verify(hash, &bytes)?;
            return Ok(Some(bytes));
        }
        let mut servers: Vec<&str> = self.config.daemon_url.as_deref().into_iter().collect();
        if !self.config.local_only {
            servers.extend(self.config.read_servers.iter().map(String::as_str));
        }
        ensure!(
            !servers.is_empty(),
            "no Hashtree content sources configured"
        );
        let mut pending = FuturesUnordered::new();
        for (i, server) in servers.into_iter().enumerate() {
            pending.push(async move {
                // Give the existing daemon first chance; slow peers do not block
                // other sources or get misreported as a definitive absence.
                tokio::time::sleep(Duration::from_millis((i as u64).min(8) * 150)).await;
                self.fetch(server, hash).await
            });
        }
        let mut error = None;
        while let Some(result) = pending.next().await {
            match result {
                Ok(Some(bytes)) => {
                    self.cache.put(*hash, bytes.clone()).await?;
                    return Ok(Some(bytes));
                }
                Ok(None) => {}
                Err(e) => error = Some(e),
            }
        }
        if let Some(e) = error {
            return Err(e);
        }
        Ok(None)
    }
}

#[async_trait]
impl Store for BlobStore {
    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        self.retrieve(hash)
            .await
            .map_err(|e| StoreError::Other(e.to_string()))
    }
    async fn put(&self, hash: Hash, bytes: Vec<u8>) -> Result<bool, StoreError> {
        verify(&hash, &bytes).map_err(|e| StoreError::Other(e.to_string()))?;
        self.cache.put(hash, bytes).await
    }
    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        Ok(self.get(hash).await?.is_some())
    }
    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.cache.delete(hash).await
    }
}
