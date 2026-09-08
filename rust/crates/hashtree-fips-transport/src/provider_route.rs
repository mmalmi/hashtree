use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex, RwLock, Weak};

use async_trait::async_trait;
use fips_core::discovery::local::rank_capability_providers;
use fips_core::{FipsEndpoint, PeerIdentity};
use hashtree_core::{BlobReply, BlobRequest, BlobRoute, BlobRouteContext, Hash, Store, StoreError};
use thiserror::Error;
use tokio::task::JoinSet;
use tokio::time::Duration;

use crate::tcp_blob::MAX_OUTBOUND_GETS;
use crate::{TCP_BLOB_CAPABILITY, TCP_BLOB_SERVICE_PORT, TcpBlobTransport};

const PROVIDER_HEDGE_DELAY: Duration = Duration::from_millis(100);
const MAX_RETAINED_RETRY_WINDOWS: usize = 256;

#[derive(Debug, Error)]
pub enum FipsBlobRouteError {
    #[error("FIPS blob route must attempt at least one provider")]
    NoProviderAttempts,
    #[error("FIPS blob route may attempt at most {MAX_OUTBOUND_GETS} providers, got {0}")]
    TooManyProviderAttempts(usize),
}

/// One opaque BlobRoute whose sole responsibility is selecting a bounded set
/// of FIPS peers. Discovery-ranked providers and explicit peers are
/// deduplicated and interleaved before a bounded window is raced with first
/// valid data winning. Retries of a retained hash advance through a stable
/// candidate set independently of other hashes. The least recently used hash
/// is evicted at the retention bound; its next search starts at the first
/// window again. FIPS continues to own all transport addresses, reachability,
/// and replacement; no outer route selects these peers again.
pub struct FipsBlobRoute<S: Store + ?Sized + 'static> {
    discovery: Option<Arc<FipsEndpoint>>,
    explicit: RwLock<Vec<PeerIdentity>>,
    transport: Weak<TcpBlobTransport<S>>,
    max_provider_attempts: usize,
    retry_windows: Mutex<VecDeque<(Hash, usize)>>,
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
            retry_windows: Mutex::new(VecDeque::new()),
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
            retry_windows: Mutex::new(VecDeque::new()),
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
            retry_windows: Mutex::new(VecDeque::new()),
        })
    }

    pub fn set_explicit_peers(&self, peers: Vec<PeerIdentity>) {
        *self
            .explicit
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = peers;
    }

    /// Snapshot the first bounded window without advancing any hash's retry.
    pub fn provider_ids(&self) -> Result<Vec<String>, StoreError> {
        Ok(self
            .provider_candidates()?
            .into_iter()
            .take(self.max_provider_attempts)
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

    fn provider_candidates(&self) -> Result<Vec<PeerIdentity>, StoreError> {
        let discovered = self.discovered_provider_peers()?;
        let explicit = self
            .explicit
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        Ok(ordered_provider_peers(discovered, explicit))
    }

    fn provider_attempts(
        &self,
        hash: Hash,
        attempt_budget: usize,
    ) -> Result<(Vec<PeerIdentity>, usize), StoreError> {
        let mut peers = self.provider_candidates()?;
        let count = peers.len();
        let attempts = self.max_provider_attempts.min(attempt_budget).min(count);
        if attempts > 0 && attempts < count {
            // Reserve progress before awaiting: concurrent searches for this
            // hash cannot all reserve the same window. No peer identities are
            // retained, so every search observes the current discovery/ACL.
            let mut windows = self
                .retry_windows
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let start = windows
                .iter()
                .position(|(key, _)| *key == hash)
                .and_then(|position| windows.remove(position))
                .map_or(0, |(_, next)| next % count);
            if windows.len() == MAX_RETAINED_RETRY_WINDOWS {
                windows.pop_front();
            }
            windows.push_back((hash, (start + attempts) % count));
            drop(windows);
            peers.rotate_left(start);
        }
        peers.truncate(attempts);
        Ok((peers, count - attempts))
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
        let budget = context.map_or(self.max_provider_attempts, |context| context.attempt_budget);
        let (peers, untried) = self.provider_attempts(request.hash, budget)?;
        if peers.is_empty() {
            return completed_misses(untried);
        }
        let transport = self
            .transport
            .upgrade()
            .ok_or_else(|| StoreError::Other("TCP/FIPS blob transport is closed".to_string()))?;

        let mut attempts = JoinSet::new();
        for (index, peer) in peers.into_iter().enumerate() {
            let route = transport.route_to(peer);
            attempts.spawn(async move {
                let delay = PROVIDER_HEDGE_DELAY.saturating_mul(index as u32);
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                route.route(request).await
            });
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
            None => completed_misses(untried),
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

fn completed_misses(untried: usize) -> Result<BlobReply, StoreError> {
    if untried == 0 {
        Ok(BlobReply::NoResult)
    } else {
        Err(StoreError::Other(format!(
            "FIPS blob provider search was incomplete: {untried} providers remain outside this retry window"
        )))
    }
}

fn ordered_provider_peers(
    mut discovered: Vec<PeerIdentity>,
    mut explicit: Vec<PeerIdentity>,
) -> Vec<PeerIdentity> {
    let mut discovered_ids = HashSet::new();
    discovered.retain(|peer| discovered_ids.insert(peer.npub()));
    let mut explicit_ids = HashSet::new();
    explicit.retain(|peer| {
        let npub = peer.npub();
        !discovered_ids.contains(&npub) && explicit_ids.insert(npub)
    });

    let mut selected = Vec::with_capacity(discovered.len() + explicit.len());
    let mut discovered = discovered.into_iter();
    let mut explicit = explicit.into_iter();
    loop {
        let mut added = false;
        if let Some(peer) = discovered.next() {
            selected.push(peer);
            added = true;
        }
        if let Some(peer) = explicit.next() {
            selected.push(peer);
            added = true;
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
            retry_windows: Mutex::new(VecDeque::new()),
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
    fn preserved_rank_and_interleave_has_one_owner_per_peer() {
        let discovered = (0..4)
            .map(|_| PeerIdentity::from_npub(&Identity::generate().npub()).unwrap())
            .collect::<Vec<_>>();
        let explicit = (0..2)
            .map(|_| PeerIdentity::from_npub(&Identity::generate().npub()).unwrap())
            .collect::<Vec<_>>();

        assert_eq!(
            ordered_provider_peers(discovered.clone(), explicit.clone()),
            vec![
                discovered[0],
                explicit[0],
                discovered[1],
                explicit[1],
                discovered[2],
                discovered[3]
            ],
        );
        assert_eq!(
            ordered_provider_peers(discovered.clone(), vec![discovered[0], explicit[0]]),
            vec![
                discovered[0],
                explicit[0],
                discovered[1],
                discovered[2],
                discovered[3]
            ],
        );
    }

    #[test]
    fn retry_windows_respect_context_and_refresh_peers_without_introspection_side_effects() {
        let peers = test_peers(5);
        let route = test_route(peers.clone());
        let hash = [0x11; 32];
        assert_eq!(
            route.provider_attempts(hash, 1).unwrap(),
            (vec![peers[0]], 4)
        );
        for byte in 0x20..0x24 {
            assert_eq!(
                route.provider_attempts([byte; 32], 4).unwrap(),
                (peers[..4].to_vec(), 1),
            );
        }
        assert_eq!(
            route.provider_ids().unwrap(),
            peers[..4]
                .iter()
                .map(PeerIdentity::npub)
                .collect::<Vec<_>>(),
        );
        assert_eq!(route.provider_attempts(hash, 0).unwrap(), (Vec::new(), 5));
        assert_eq!(
            route.provider_attempts(hash, 1).unwrap(),
            (vec![peers[1]], 4)
        );
        assert_eq!(
            route.provider_attempts(hash, usize::MAX).unwrap(),
            (vec![peers[2], peers[3], peers[4], peers[0]], 1),
            "context cannot increase the constructor's four-attempt bound",
        );

        route.set_explicit_peers(vec![peers[4], peers[2], peers[4]]);
        assert_eq!(
            route.provider_attempts(hash, 4).unwrap(),
            (vec![peers[4], peers[2]], 0),
            "retry state must never retain a removed provider identity",
        );
    }

    #[test]
    fn least_recently_used_retry_is_evicted_without_unbounded_hash_state() {
        let peers = test_peers(5);
        let route = test_route(peers.clone());
        let hash = |index: usize| {
            let mut hash = [0; 32];
            hash[..8].copy_from_slice(&(index as u64).to_le_bytes());
            hash
        };
        for index in 0..=MAX_RETAINED_RETRY_WINDOWS {
            assert_eq!(
                route.provider_attempts(hash(index), 4).unwrap(),
                (peers[..4].to_vec(), 1),
            );
        }
        assert_eq!(
            route.retry_windows.lock().unwrap().len(),
            MAX_RETAINED_RETRY_WINDOWS
        );
        assert_eq!(
            route
                .provider_attempts(hash(MAX_RETAINED_RETRY_WINDOWS), 1)
                .unwrap(),
            (vec![peers[4]], 4),
            "a retained hash keeps progress despite other searches",
        );
        assert_eq!(
            route.provider_attempts(hash(0), 1).unwrap(),
            (vec![peers[0]], 4),
            "an evicted hash starts at the first window",
        );
        assert_eq!(
            route.retry_windows.lock().unwrap().len(),
            MAX_RETAINED_RETRY_WINDOWS
        );
    }

    fn test_peers(count: usize) -> Vec<PeerIdentity> {
        (0..count)
            .map(|_| PeerIdentity::from_npub(&Identity::generate().npub()).unwrap())
            .collect()
    }

    fn test_route(peers: Vec<PeerIdentity>) -> FipsBlobRoute<MemoryStore> {
        FipsBlobRoute {
            discovery: None,
            explicit: RwLock::new(peers),
            transport: Weak::new(),
            max_provider_attempts: 4,
            retry_windows: Mutex::new(VecDeque::new()),
        }
    }
}
