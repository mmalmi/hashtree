use std::collections::HashSet;
use std::sync::{Arc, RwLock, Weak};

use async_trait::async_trait;
use fips_core::discovery::local::rank_capability_providers;
use fips_core::{FipsEndpoint, PeerIdentity};
use hashtree_core::{BlobReply, BlobRequest, BlobRoute, BlobRouteContext, Store, StoreError};
use thiserror::Error;
use tokio::task::JoinSet;

use crate::tcp_blob::MAX_OUTBOUND_GETS;
use crate::{TcpBlobTransport, TCP_BLOB_CAPABILITY, TCP_BLOB_SERVICE_PORT};

#[derive(Debug, Error)]
pub enum FipsBlobRouteError {
    #[error("FIPS blob route must attempt at least one provider")]
    NoProviderAttempts,
    #[error("FIPS blob route may attempt at most {MAX_OUTBOUND_GETS} providers, got {0}")]
    TooManyProviderAttempts(usize),
}

/// One opaque BlobRoute whose sole responsibility is selecting a bounded set
/// of FIPS peers. FIPS continues to own all transport addresses and paths.
pub struct FipsBlobRoute<S: Store + ?Sized + 'static> {
    discovery: Option<Arc<FipsEndpoint>>,
    explicit: RwLock<Vec<PeerIdentity>>,
    transport: Weak<TcpBlobTransport<S>>,
    max_provider_attempts: usize,
}

impl<S: Store + ?Sized + 'static> FipsBlobRoute<S> {
    pub fn explicit(
        transport: Arc<TcpBlobTransport<S>>,
        peers: Vec<PeerIdentity>,
        max_provider_attempts: usize,
    ) -> Result<Self, FipsBlobRouteError> {
        validate_attempts(max_provider_attempts)?;
        Ok(Self {
            discovery: None,
            explicit: RwLock::new(peers),
            transport: Arc::downgrade(&transport),
            max_provider_attempts,
        })
    }

    pub fn discovered(
        endpoint: Arc<FipsEndpoint>,
        transport: Arc<TcpBlobTransport<S>>,
        max_provider_attempts: usize,
    ) -> Result<Self, FipsBlobRouteError> {
        validate_attempts(max_provider_attempts)?;
        Ok(Self {
            discovery: Some(endpoint),
            explicit: RwLock::new(Vec::new()),
            transport: Arc::downgrade(&transport),
            max_provider_attempts,
        })
    }

    /// Own one deduplicated peer set containing both same-host capability
    /// providers and application-configured peers. Registering two outer
    /// routes for these overlapping sets would give a peer two selection
    /// owners, so consumers needing both should use this constructor.
    pub fn discovered_and_explicit(
        endpoint: Arc<FipsEndpoint>,
        transport: Arc<TcpBlobTransport<S>>,
        peers: Vec<PeerIdentity>,
        max_provider_attempts: usize,
    ) -> Result<Self, FipsBlobRouteError> {
        validate_attempts(max_provider_attempts)?;
        Ok(Self {
            discovery: Some(endpoint),
            explicit: RwLock::new(peers),
            transport: Arc::downgrade(&transport),
            max_provider_attempts,
        })
    }

    pub fn set_explicit_peers(&self, peers: Vec<PeerIdentity>) {
        *self
            .explicit
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = peers;
    }

    pub fn provider_ids(&self) -> Result<Vec<String>, StoreError> {
        Ok(self
            .provider_peers()?
            .into_iter()
            .map(|peer| peer.npub())
            .collect())
    }

    /// Snapshot only providers advertised through local FIPS discovery.
    /// This intentionally excludes explicit application roster peers.
    pub fn discovered_provider_ids(&self) -> Result<Vec<String>, StoreError> {
        Ok(self
            .discovered_provider_peers()?
            .into_iter()
            .map(|peer| peer.npub())
            .collect())
    }

    fn provider_peers(&self) -> Result<Vec<PeerIdentity>, StoreError> {
        let discovered = self.discovered_provider_peers()?;
        let explicit = self
            .explicit
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        Ok(select_provider_peers(
            discovered,
            explicit,
            self.max_provider_attempts,
        ))
    }

    fn discovered_provider_peers(&self) -> Result<Vec<PeerIdentity>, StoreError> {
        let Some(endpoint) = &self.discovery else {
            return Ok(Vec::new());
        };
        let adverts = endpoint
            .local_instance_advertisements()
            .map_err(|error| StoreError::Other(format!("same-host discovery failed: {error}")))?;
        let local_npub = endpoint.npub();
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
            .collect())
    }

    async fn route_inner(
        &self,
        request: BlobRequest,
        context: Option<BlobRouteContext>,
    ) -> Result<BlobReply, StoreError> {
        let mut peers = self.provider_peers()?;
        if let Some(context) = context {
            peers.truncate(context.attempt_budget);
        }
        if peers.is_empty() {
            return Ok(BlobReply::NoResult);
        }
        let transport = self
            .transport
            .upgrade()
            .ok_or_else(|| StoreError::Other("TCP/FIPS blob transport is closed".to_string()))?;

        let mut attempts = JoinSet::new();
        for peer in peers {
            let route = transport.route_to(peer);
            attempts.spawn(async move { route.route(request).await });
        }

        let mut first_error = None;
        loop {
            let joined = if let Some(context) = context {
                let deadline = tokio::time::Instant::from_std(context.deadline);
                match tokio::time::timeout_at(deadline, attempts.join_next()).await {
                    Ok(joined) => joined,
                    Err(_) => {
                        attempts.abort_all();
                        return Err(StoreError::Other(
                            "FIPS blob provider-set deadline expired".to_string(),
                        ));
                    }
                }
            } else {
                attempts.join_next().await
            };
            let Some(joined) = joined else {
                break;
            };
            match joined {
                Ok(Ok(BlobReply::Data(data))) => {
                    attempts.abort_all();
                    return Ok(BlobReply::Data(data));
                }
                Ok(Ok(BlobReply::NoResult)) => {}
                Ok(Err(error)) => {
                    first_error.get_or_insert_with(|| error.to_string());
                }
                Err(error) => {
                    first_error.get_or_insert_with(|| error.to_string());
                }
            }
        }
        match first_error {
            Some(error) => Err(StoreError::Other(format!(
                "FIPS blob provider set was incomplete: {error}"
            ))),
            None => Ok(BlobReply::NoResult),
        }
    }
}

#[async_trait]
impl<S: Store + ?Sized + 'static> BlobRoute for FipsBlobRoute<S> {
    async fn route(&self, request: BlobRequest) -> Result<BlobReply, StoreError> {
        self.route_inner(request, None).await
    }

    async fn route_with_context(
        &self,
        request: BlobRequest,
        context: BlobRouteContext,
    ) -> Result<BlobReply, StoreError> {
        self.route_inner(request, Some(context)).await
    }
}

fn validate_attempts(max_provider_attempts: usize) -> Result<(), FipsBlobRouteError> {
    if max_provider_attempts == 0 {
        return Err(FipsBlobRouteError::NoProviderAttempts);
    }
    if max_provider_attempts > MAX_OUTBOUND_GETS {
        return Err(FipsBlobRouteError::TooManyProviderAttempts(
            max_provider_attempts,
        ));
    }
    Ok(())
}

fn select_provider_peers(
    mut discovered: Vec<PeerIdentity>,
    mut explicit: Vec<PeerIdentity>,
    limit: usize,
) -> Vec<PeerIdentity> {
    let mut discovered_ids = HashSet::new();
    discovered.retain(|peer| discovered_ids.insert(peer.npub()));
    let mut explicit_ids = HashSet::new();
    explicit.retain(|peer| {
        let npub = peer.npub();
        !discovered_ids.contains(&npub) && explicit_ids.insert(npub)
    });

    let mut selected = Vec::with_capacity(limit);
    let mut discovered = discovered.into_iter();
    let mut explicit = explicit.into_iter();
    loop {
        let mut added = false;
        if let Some(peer) = discovered.next() {
            selected.push(peer);
            added = true;
            if selected.len() == limit {
                break;
            }
        }
        if let Some(peer) = explicit.next() {
            selected.push(peer);
            added = true;
            if selected.len() == limit {
                break;
            }
        }
        if !added {
            break;
        }
    }
    selected
}

#[cfg(test)]
mod tests {
    use super::*;
    use fips_core::Identity;
    use hashtree_core::MemoryStore;

    #[test]
    fn explicit_provider_set_deduplicates_and_owns_its_attempt_bound() {
        let first = PeerIdentity::from_npub(&Identity::generate().npub()).unwrap();
        let second = PeerIdentity::from_npub(&Identity::generate().npub()).unwrap();
        let third = PeerIdentity::from_npub(&Identity::generate().npub()).unwrap();
        let route = FipsBlobRoute::<MemoryStore> {
            discovery: None,
            explicit: RwLock::new(vec![first, first, second, third]),
            transport: Weak::new(),
            max_provider_attempts: 2,
        };

        assert_eq!(
            route.provider_ids().unwrap(),
            vec![first.npub(), second.npub()]
        );

        route.set_explicit_peers(vec![third]);
        assert_eq!(route.provider_ids().unwrap(), vec![third.npub()]);
        assert!(route.discovered_provider_ids().unwrap().is_empty());
    }

    #[test]
    fn bounded_union_interleaves_explicit_peers_without_duplicate_owners() {
        let discovered = (0..4)
            .map(|_| PeerIdentity::from_npub(&Identity::generate().npub()).unwrap())
            .collect::<Vec<_>>();
        let explicit = (0..2)
            .map(|_| PeerIdentity::from_npub(&Identity::generate().npub()).unwrap())
            .collect::<Vec<_>>();

        assert_eq!(
            select_provider_peers(discovered.clone(), explicit.clone(), 4),
            vec![discovered[0], explicit[0], discovered[1], explicit[1]],
        );
        assert_eq!(
            select_provider_peers(discovered.clone(), vec![discovered[0], explicit[0]], 2,),
            vec![discovered[0], explicit[0]],
        );
    }
}
