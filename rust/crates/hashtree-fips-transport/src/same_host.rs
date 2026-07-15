use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use fips_core::discovery::local::rank_capability_providers;
use fips_core::{FipsEndpoint, PeerIdentity};
use hashtree_core::store::StoreStats;
use hashtree_core::{
    BlobReply, BlobRequest, BlobRoute, Hash, Store, StoreError, BLOB_DEFAULT_HTL, BLOB_MAX_HTL,
};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::task::JoinSet;

use crate::tcp_blob::MAX_OUTBOUND_GETS;
use crate::{
    InboundBlobPolicy, TcpBlobPeerRoute, TcpBlobTransport, TcpBlobTransportConfig,
    TcpBlobTransportError, WeakTcpBlobPeerRoute, TCP_BLOB_CAPABILITY, TCP_BLOB_SERVICE_PORT,
};

fn verify_hash(data: &[u8], expected: &Hash) -> bool {
    Sha256::digest(data).as_slice() == expected
}

/// Policy for an optional same-host Hashtree service attachment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SameHostBlobStoreConfig {
    /// Advertise this process's local store. `None` keeps it client-private.
    pub advertise_priority: Option<i16>,
    /// Maximum number of ranked provider hints attempted for one cache miss.
    pub max_provider_attempts: usize,
    /// Search budget sent to same-host providers. Zero keeps lookup provider-local.
    pub provider_htl: u8,
    /// Hashtree search budget for the application's standalone fallback route.
    pub standalone_htl: u8,
    /// TCP/FIPS actor progress limits.
    pub transport: TcpBlobTransportConfig,
}

impl SameHostBlobStoreConfig {
    pub fn provider(priority: i16) -> Self {
        Self {
            advertise_priority: Some(priority),
            ..Self::default()
        }
    }

    pub fn with_max_provider_attempts(mut self, max_provider_attempts: usize) -> Self {
        self.max_provider_attempts = max_provider_attempts;
        self
    }

    pub fn with_transport(mut self, transport: TcpBlobTransportConfig) -> Self {
        self.transport = transport;
        self
    }

    pub fn with_provider_htl(mut self, provider_htl: u8) -> Self {
        self.provider_htl = provider_htl;
        self
    }

    pub fn with_standalone_htl(mut self, standalone_htl: u8) -> Self {
        self.standalone_htl = standalone_htl;
        self
    }
}

impl Default for SameHostBlobStoreConfig {
    fn default() -> Self {
        Self {
            advertise_priority: None,
            max_provider_attempts: 4,
            provider_htl: 0,
            standalone_htl: BLOB_DEFAULT_HTL,
            transport: TcpBlobTransportConfig::default(),
        }
    }
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SameHostBlobStoreError {
    #[error("same-host blob store must attempt at least one provider")]
    NoProviderAttempts,
    #[error("same-host blob store may attempt at most {MAX_OUTBOUND_GETS} providers, got {0}")]
    TooManyProviderAttempts(usize),
    #[error("same-host Hashtree search HTL {0} exceeds the maximum of {BLOB_MAX_HTL}")]
    HtlTooLarge(u8),
    #[error(transparent)]
    Transport(#[from] TcpBlobTransportError),
}

/// An ordinary local Hashtree store with optional verified same-host reads.
///
/// Local mutations never depend on another process. On a local read miss, the
/// wrapper snapshots FIPS's in-memory authenticated capability roster, races a
/// bounded set of `hashtree.blob/1` providers, and caches the first hash-valid
/// response. Provider records remain routing hints; ordinary Noise identity and
/// the TCP blob protocol authenticate the peer and verify the data.
pub struct SameHostBlobStore<S: Store + ?Sized + 'static> {
    endpoint: Arc<FipsEndpoint>,
    local: Arc<S>,
    standalone: RwLock<Option<Arc<dyn BlobRoute>>>,
    transport: Arc<TcpBlobTransport<S>>,
    max_provider_attempts: usize,
    provider_htl: u8,
    standalone_htl: u8,
}

impl<S: Store + ?Sized + 'static> SameHostBlobStore<S> {
    pub async fn bind(
        endpoint: Arc<FipsEndpoint>,
        local: Arc<S>,
        standalone: Option<Arc<dyn BlobRoute>>,
        config: SameHostBlobStoreConfig,
    ) -> Result<Self, SameHostBlobStoreError> {
        let inbound_route = Arc::new(hashtree_core::StoreBlobRoute::new(local.clone()));
        let inbound_policy: InboundBlobPolicy = if config.advertise_priority.is_some() {
            Arc::new(|_| true)
        } else {
            Arc::new(|_| false)
        };
        Self::bind_route_with_policy(
            endpoint,
            local,
            standalone,
            inbound_route,
            inbound_policy,
            config,
        )
        .await
    }

    /// Bind one TCP/FIPS blob service for provider discovery, standalone peer
    /// routes, and an application-authorized inbound route.
    pub async fn bind_route_with_policy(
        endpoint: Arc<FipsEndpoint>,
        local: Arc<S>,
        standalone: Option<Arc<dyn BlobRoute>>,
        inbound_route: Arc<dyn BlobRoute>,
        inbound_policy: InboundBlobPolicy,
        config: SameHostBlobStoreConfig,
    ) -> Result<Self, SameHostBlobStoreError> {
        if config.max_provider_attempts == 0 {
            return Err(SameHostBlobStoreError::NoProviderAttempts);
        }
        if config.max_provider_attempts > MAX_OUTBOUND_GETS {
            return Err(SameHostBlobStoreError::TooManyProviderAttempts(
                config.max_provider_attempts,
            ));
        }
        let requested_htl = config.provider_htl.max(config.standalone_htl);
        if requested_htl > BLOB_MAX_HTL {
            return Err(SameHostBlobStoreError::HtlTooLarge(requested_htl));
        }
        let transport = match config.advertise_priority {
            Some(priority) => {
                TcpBlobTransport::bind_advertised_route_with_config_and_policy(
                    endpoint.clone(),
                    local.clone(),
                    inbound_route,
                    config.transport,
                    priority,
                    inbound_policy,
                )
                .await?
            }
            None => {
                TcpBlobTransport::bind_route_with_config_and_policy(
                    endpoint.clone(),
                    local.clone(),
                    inbound_route,
                    config.transport,
                    inbound_policy,
                )
                .await?
            }
        };
        Ok(Self {
            endpoint,
            local,
            standalone: RwLock::new(standalone),
            transport: Arc::new(transport),
            max_provider_attempts: config.max_provider_attempts,
            provider_htl: config.provider_htl,
            standalone_htl: config.standalone_htl,
        })
    }

    pub fn local_store(&self) -> &Arc<S> {
        &self.local
    }

    pub fn peer_route(&self, peer: PeerIdentity) -> TcpBlobPeerRoute<S> {
        self.transport.route_to(peer)
    }

    pub fn weak_peer_route(&self, peer: PeerIdentity) -> WeakTcpBlobPeerRoute<S> {
        self.transport.weak_route_to(peer)
    }

    pub fn set_standalone_route(&self, route: Option<Arc<dyn BlobRoute>>) {
        *self
            .standalone
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = route;
    }

    fn provider_peers(&self) -> Result<Vec<PeerIdentity>, StoreError> {
        let adverts = self
            .endpoint
            .local_instance_advertisements()
            .map_err(|error| StoreError::Other(format!("same-host discovery failed: {error}")))?;
        let local_npub = self.endpoint.npub();
        Ok(rank_capability_providers(&adverts, TCP_BLOB_CAPABILITY)
            .into_iter()
            .filter(|advert| advert.npub != local_npub)
            .filter(|advert| {
                advert
                    .capability(TCP_BLOB_CAPABILITY)
                    .and_then(|capability| capability.fsp_port)
                    == Some(TCP_BLOB_SERVICE_PORT)
            })
            .filter_map(|advert| PeerIdentity::from_npub(&advert.npub).ok())
            .take(self.max_provider_attempts)
            .collect())
    }

    async fn discovered_get(&self, hash: &Hash) -> Option<Vec<u8>> {
        let peers = self.provider_peers().ok()?;

        let mut attempts = JoinSet::new();
        for peer in peers {
            let route = self.transport.route_to(peer);
            let request = BlobRequest {
                hash: *hash,
                htl: self.provider_htl,
            };
            attempts.spawn(async move { route.route(request).await });
        }

        while let Some(result) = attempts.join_next().await {
            match result {
                Ok(Ok(BlobReply::Data(data))) if verify_hash(&data, hash) => {
                    attempts.abort_all();
                    return Some(data);
                }
                Ok(Ok(BlobReply::Data(_) | BlobReply::NoResult) | Err(_)) | Err(_) => {}
            }
        }
        None
    }

    async fn standalone_get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        let standalone = self
            .standalone
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let Some(standalone) = standalone else {
            return Ok(None);
        };
        let data = match standalone
            .route(BlobRequest {
                hash: *hash,
                htl: self.standalone_htl,
            })
            .await?
        {
            BlobReply::Data(data) if verify_hash(&data, hash) => data,
            BlobReply::Data(_) => {
                return Err(StoreError::Other(
                    "standalone Hashtree route returned a blob with the wrong hash".to_string(),
                ));
            }
            BlobReply::NoResult => return Ok(None),
        };
        self.cache_remote(hash, &data).await?;
        Ok(Some(data))
    }

    async fn cache_remote(&self, hash: &Hash, data: &[u8]) -> Result<(), StoreError> {
        let mut repairing = false;
        let mut preserved_pin_count = 0;
        if let Some(cached) = self.local.get(hash).await? {
            if cached == data {
                return Ok(());
            }
            if Sha256::digest(&cached).as_slice() == hash {
                return Err(StoreError::Other(
                    "distinct local and remote blobs share one SHA-256 hash".to_string(),
                ));
            }
            preserved_pin_count = self.local.pin_count(hash);
            self.local.delete(hash).await?;
            repairing = true;
        }
        let inserted = self.local.put(*hash, data.to_vec()).await?;
        if repairing || !inserted {
            match self.local.get(hash).await? {
                Some(cached) if cached == data => {}
                Some(_) => {
                    return Err(StoreError::Other(
                        "local Hashtree cache remained corrupt after repair".to_string(),
                    ));
                }
                None => {
                    return Err(StoreError::Other(
                        "local Hashtree cache did not retain a remote blob".to_string(),
                    ));
                }
            }
        }
        for _ in 0..preserved_pin_count {
            self.local.pin(hash).await?;
        }
        Ok(())
    }
}

#[async_trait]
impl<S: Store + ?Sized + 'static> Store for SameHostBlobStore<S> {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        self.local.put(hash, data).await
    }

    async fn put_many(&self, items: Vec<(Hash, Vec<u8>)>) -> Result<usize, StoreError> {
        self.local.put_many(items).await
    }

    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        let mut corrupt_local = false;
        if let Some(data) = self.local.get(hash).await? {
            if Sha256::digest(&data).as_slice() == hash {
                return Ok(Some(data));
            }
            corrupt_local = true;
        }
        if let Some(data) = self.discovered_get(hash).await {
            self.cache_remote(hash, &data).await?;
            return Ok(Some(data));
        }
        let standalone = self.standalone_get(hash).await?;
        if standalone.is_none() && corrupt_local {
            return Err(StoreError::Other(
                "local Hashtree blob hash mismatch and no read source repaired it".to_string(),
            ));
        }
        Ok(standalone)
    }

    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        Ok(self.get(hash).await?.is_some())
    }

    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.local.delete(hash).await
    }

    fn set_max_bytes(&self, max: u64) {
        self.local.set_max_bytes(max);
    }

    fn max_bytes(&self) -> Option<u64> {
        self.local.max_bytes()
    }

    async fn stats(&self) -> StoreStats {
        self.local.stats().await
    }

    async fn evict_if_needed(&self) -> Result<u64, StoreError> {
        self.local.evict_if_needed().await
    }

    async fn pin(&self, hash: &Hash) -> Result<(), StoreError> {
        self.local.pin(hash).await
    }

    async fn unpin(&self, hash: &Hash) -> Result<(), StoreError> {
        self.local.unpin(hash).await
    }

    fn pin_count(&self, hash: &Hash) -> u32 {
        self.local.pin_count(hash)
    }
}
