use crate::ClientConfig;
use anyhow::{bail, ensure, Context, Result};
use async_trait::async_trait;
use futures::{stream::FuturesUnordered, StreamExt};
use hashtree_core::{Hash, Store, StoreError};
use hashtree_fs::FsBlobStore;
use sha2::{Digest, Sha256};
use std::{error::Error, io::ErrorKind, path::Path, time::Duration};

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

fn transient_http_error(error: &reqwest::Error) -> bool {
    if error.is_connect()
        || error.is_timeout()
        || error.is_body()
        || error.is_decode()
        || error.status().is_some_and(|status| {
            status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        })
    {
        return true;
    }
    // A connection can reset after connecting but before response headers.
    // reqwest reports this as a request error, not necessarily is_connect().
    // Inspect typed causes rather than retrying all request/protocol failures.
    let mut cause = error.source();
    while let Some(error) = cause {
        if error.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted | ErrorKind::BrokenPipe
            )
        }) {
            return true;
        }
        cause = error.source();
    }
    false
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
        // Keep credentials, query strings and private URL paths out of diagnostics.
        let origin = reqwest::Url::parse(server)
            .map(|url| url.origin().ascii_serialization())
            .unwrap_or_else(|_| "invalid source URL".into());
        let context = || format!("Hashtree block {} from {origin}", hex::encode(hash));
        match self.fetch_once(server, hash).await {
            Err(error)
                if error
                    .downcast_ref::<reqwest::Error>()
                    .is_some_and(transient_http_error) =>
            {
                // A partial response is never cached. Retry just this source once;
                // the other hedged sources remain free to finish first.
                tokio::time::sleep(Duration::from_millis(250)).await;
                self.fetch_once(server, hash).await.with_context(context)
            }
            result => result.with_context(context),
        }
    }

    async fn fetch_once(&self, server: &str, hash: &Hash) -> Result<Option<Vec<u8>>> {
        let response = self
            .http
            .get(format!(
                "{}/{}.bin",
                server.trim_end_matches('/'),
                hex::encode(hash)
            ))
            .send()
            .await
            .map_err(reqwest::Error::without_url)?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let response = response
            .error_for_status()
            .map_err(reqwest::Error::without_url)?;
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
            let part = part.map_err(reqwest::Error::without_url)?;
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
        let mut errors = Vec::new();
        while let Some(result) = pending.next().await {
            match result {
                Ok(Some(bytes)) => {
                    self.cache.put(*hash, bytes.clone()).await?;
                    return Ok(Some(bytes));
                }
                Ok(None) => {}
                Err(error) => errors.push(format!("{error:#}")),
            }
        }
        if !errors.is_empty() {
            bail!("all Hashtree content sources failed: {}", errors.join("; "));
        }
        Ok(None)
    }
}

#[async_trait]
impl Store for BlobStore {
    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        self.retrieve(hash)
            .await
            .map_err(|e| StoreError::Other(format!("{e:#}")))
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
