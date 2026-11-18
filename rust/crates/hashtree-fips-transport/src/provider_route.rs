use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex, RwLock, Weak};

use async_trait::async_trait;
use fips_core::discovery::local::rank_capability_providers;
use fips_core::{FipsEndpoint, PeerIdentity};
use hashtree_core::{BlobReply, BlobRequest, BlobRoute, BlobRouteContext, Hash, Store, StoreError};
use thiserror::Error;

use crate::tcp_blob::{INBOUND_BLOB_PEER, MAX_OUTBOUND_GETS};
use crate::{TcpBlobTransport, TCP_BLOB_CAPABILITY, TCP_BLOB_SERVICE_PORT};

mod integrity;
mod schedule;

use self::integrity::IntegrityCooldowns;
use self::schedule::ProviderWindow;

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
/// A directly observed hash mismatch temporarily defers that provider. A
/// search with deferred providers remains incomplete until it finds data.
pub struct FipsBlobRoute<S: Store + ?Sized + 'static> {
    discovery: Option<Arc<FipsEndpoint>>,
    explicit: RwLock<Vec<PeerIdentity>>,
    transport: Weak<TcpBlobTransport<S>>,
    max_provider_attempts: usize,
    retry_windows: Mutex<VecDeque<(Hash, usize)>>,
    integrity: Arc<IntegrityCooldowns>,
}

impl<S: Store + ?Sized + 'static> FipsBlobRoute<S> {
    pub fn explicit(
        transport: Arc<TcpBlobTransport<S>>,
        peers: Vec<PeerIdentity>,
        max_provider_attempts: usize,
    ) -> Result<Self, FipsBlobRouteError> {
        Self::new(
            None,
            Arc::downgrade(&transport),
            peers,
            max_provider_attempts,
        )
    }

    pub fn discovered(
        endpoint: Arc<FipsEndpoint>,
        transport: Arc<TcpBlobTransport<S>>,
        max_provider_attempts: usize,
    ) -> Result<Self, FipsBlobRouteError> {
        Self::discovered_and_explicit(endpoint, transport, Vec::new(), max_provider_attempts)
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
        Self::new(
            Some(endpoint),
            Arc::downgrade(&transport),
            peers,
            max_provider_attempts,
        )
    }

    fn new(
        discovery: Option<Arc<FipsEndpoint>>,
        transport: Weak<TcpBlobTransport<S>>,
        peers: Vec<PeerIdentity>,
        max_provider_attempts: usize,
    ) -> Result<Self, FipsBlobRouteError> {
        validate_attempts(max_provider_attempts)?;
        Ok(Self {
            discovery,
            explicit: RwLock::new(unique_explicit_peers(peers)),
            transport,
            max_provider_attempts,
            retry_windows: Mutex::default(),
            integrity: Arc::default(),
        })
    }

    pub fn set_explicit_peers(&self, peers: Vec<PeerIdentity>) {
        let peers = unique_explicit_peers(peers);
        *self
            .explicit
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = peers;
    }

    /// Snapshot the bounded roster before request-specific filtering and cooldowns.
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
        if discovered.is_empty() {
            return Ok(explicit);
        }
        Ok(ordered_provider_peers(discovered, explicit))
    }

    #[cfg(test)]
    fn provider_attempts(
        &self,
        hash: Hash,
        attempt_budget: usize,
    ) -> Result<(Vec<PeerIdentity>, usize), StoreError> {
        let window = self.provider_window(hash, attempt_budget)?;
        Ok((window.planned, window.untried))
    }

    fn provider_window(
        &self,
        hash: Hash,
        attempt_budget: usize,
    ) -> Result<ProviderWindow, StoreError> {
        let mut peers = self.provider_candidates()?;
        let budget = self.max_provider_attempts.min(attempt_budget);
        // An early branch can reach multiple further hops. Near the edge,
        // keep the original window to bound how much the frontier expands.
        let requester = INBOUND_BLOB_PEER
            .try_with(|(inbound, requester)| {
                if inbound.hash != hash {
                    return None;
                }
                let key = requester.pubkey();
                if inbound.htl <= 2 && budget > 1 {
                    return Some(*requester);
                }
                peers.retain(|peer| peer.pubkey() != key);
                None
            })
            .ok()
            .flatten();
        let deferred = self.integrity.defer(&mut peers, requester);
        let attempts = budget.min(peers.len());
        let first_window = self.advance_retry_window(hash, &mut peers, attempts);
        let mut limit = attempts;
        if let Some(index) = requester.and_then(|source| {
            peers
                .iter()
                .position(|peer| peer.pubkey() == source.pubkey())
        }) {
            peers.remove(index);
            if index < attempts {
                limit -= 1;
            }
        }
        let untried = peers.len().saturating_sub(limit) + deferred;
        // At most one substitute per planned attempt. Every peer outside the
        // dispatched window still counts toward an incomplete search.
        peers.truncate(if first_window { limit * 2 } else { limit });
        let spares = peers.split_off(limit.min(peers.len()));
        Ok(ProviderWindow {
            planned: peers,
            spares,
            untried,
        })
    }

    // Substitution is useful on the first window. On retries the reserved
    // order takes precedence, so a recent requester can still supply data.
    fn advance_retry_window(
        &self,
        hash: Hash,
        peers: &mut [PeerIdentity],
        attempts: usize,
    ) -> bool {
        let count = peers.len();
        if attempts == 0 || attempts == count {
            return false;
        }
        // Reserve progress before awaiting: concurrent searches for this
        // hash cannot all reserve the same window. No peer identities are
        // retained, so every search observes the current discovery/ACL.
        let mut windows = self
            .retry_windows
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = windows
            .iter()
            .position(|(key, _)| *key == hash)
            .and_then(|position| windows.remove(position));
        let start = previous.map_or(0, |(_, next)| next % count);
        if windows.len() == MAX_RETAINED_RETRY_WINDOWS {
            windows.pop_front();
        }
        windows.push_back((hash, (start + attempts) % count));
        drop(windows);
        peers.rotate_left(start);
        previous.is_none()
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
        let window = self.provider_window(request.hash, budget)?;
        if window.planned.is_empty() {
            return completed_misses(window.untried);
        }
        let transport = self
            .transport
            .upgrade()
            .ok_or_else(|| StoreError::Other("TCP/FIPS blob transport is closed".to_string()))?;
        window
            .race(request, context, &transport.recent_requesters, |peer| {
                let transport = transport.clone();
                let integrity = self.integrity.clone();
                async move {
                    integrity.check_reply(peer, transport.request_from_peer(request, peer).await)
                }
            })
            .await
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
            "FIPS blob provider search was incomplete: {untried} providers were not attempted"
        )))
    }
}

fn unique_explicit_peers(mut peers: Vec<PeerIdentity>) -> Vec<PeerIdentity> {
    let mut seen = HashSet::with_capacity(peers.len());
    peers.retain(|peer| seen.insert(peer.pubkey().serialize()));
    peers
}

// The explicit roster is already unique. Discovery may contain repeats or overlap it.
fn ordered_provider_peers(
    mut discovered: Vec<PeerIdentity>,
    mut explicit: Vec<PeerIdentity>,
) -> Vec<PeerIdentity> {
    let mut seen = HashSet::with_capacity(discovered.len());
    discovered.retain(|peer| seen.insert(peer.pubkey().serialize()));
    explicit.retain(|peer| !seen.contains(&peer.pubkey().serialize()));

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
mod mesh_scale;

#[cfg(test)]
mod tests {
    use super::*;
    use fips_core::Identity;
    use hashtree_core::MemoryStore;

    #[tokio::test(start_paused = true)]
    async fn hedge_uses_new_inbound_evidence_and_retries_keep_their_reserved_peers() {
        use crate::recent_requesters::RecentRequesters;
        let peers = test_peers(4);
        let route = test_route(peers.clone());
        let recent = Arc::new(RecentRequesters::default());
        let request = BlobRequest {
            hash: [7; 32],
            htl: 2,
        };
        let [first, previous_requester, spare] = [peers[0], peers[1], peers[2]];
        let mut sent = Vec::new();
        let reply = route
            .provider_window(request.hash, 2)
            .unwrap()
            .race(request, None, &recent, |peer| {
                sent.push(peer);
                let recent = recent.clone();
                async move {
                    if peer == first {
                        recent.record(request.hash, previous_requester);
                    }
                    Ok(BlobReply::NoResult)
                }
            })
            .await;
        assert!(
            reply.is_err(),
            "the undispatched peers keep this search incomplete"
        );
        assert_eq!(sent, [first, spare]);
        let second = route.provider_window(request.hash, 2).unwrap();
        assert_eq!(second.planned, peers[2..]);
        assert!(second.spares.is_empty());
        let wrapped = route.provider_window(request.hash, 2).unwrap();
        assert_eq!(wrapped.planned, peers[..2]);
        assert!(wrapped.spares.is_empty());
    }

    #[tokio::test]
    async fn forwarded_hash_spends_its_window_on_other_peers() {
        let peers = test_peers(5);
        let route = test_route(peers.clone());
        let hash = [1; 32];
        INBOUND_BLOB_PEER
            .scope((BlobRequest { hash, htl: 3 }, peers[0]), async {
                assert_eq!(
                    route.provider_attempts(hash, 2).unwrap(),
                    (peers[1..3].to_vec(), 2)
                );
                assert_eq!(
                    route.provider_attempts(hash, 2).unwrap(),
                    (peers[3..].to_vec(), 2)
                );
                assert_eq!(
                    route.provider_attempts([2; 32], 1).unwrap(),
                    (vec![peers[0]], 4)
                );
            })
            .await;
        assert_eq!(
            route.provider_attempts(hash, 1).unwrap(),
            (vec![peers[0]], 4)
        );
    }

    #[tokio::test]
    async fn near_the_hop_limit_a_return_edge_does_not_expand_the_reserved_window() {
        let peers = test_peers(5);
        let route = test_route(peers.clone());
        let hash = [9; 32];
        INBOUND_BLOB_PEER
            .scope((BlobRequest { hash, htl: 1 }, peers[0]), async {
                assert_eq!(
                    route.provider_attempts(hash, 2).unwrap(),
                    (vec![peers[1]], 3)
                );
                assert_eq!(
                    route.provider_attempts(hash, 2).unwrap(),
                    (peers[2..4].to_vec(), 2)
                );
            })
            .await;
        let hash = [10; 32];
        INBOUND_BLOB_PEER
            .scope((BlobRequest { hash, htl: 1 }, peers[0]), async {
                assert_eq!(
                    route.provider_attempts(hash, 1).unwrap(),
                    (vec![peers[1]], 3)
                );
                assert_eq!(
                    route.provider_attempts(hash, 1).unwrap(),
                    (vec![peers[2]], 3)
                );
            })
            .await;
    }

    #[test]
    fn explicit_provider_set_deduplicates_and_owns_its_attempt_bound() {
        let first = PeerIdentity::from_npub(&Identity::generate().npub()).unwrap();
        let second = PeerIdentity::from_npub(&Identity::generate().npub()).unwrap();
        let third = PeerIdentity::from_npub(&Identity::generate().npub()).unwrap();
        let route = FipsBlobRoute::<MemoryStore>::new(
            None,
            Weak::new(),
            vec![first, first, second, third],
            2,
        )
        .unwrap();

        assert_eq!(
            route.provider_ids().unwrap(),
            vec![first.npub(), second.npub()]
        );

        route.set_explicit_peers(vec![third, third]);
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

    pub(super) fn test_peers(count: usize) -> Vec<PeerIdentity> {
        (0..count)
            .map(|_| PeerIdentity::from_npub(&Identity::generate().npub()).unwrap())
            .collect()
    }

    pub(super) fn test_route(peers: Vec<PeerIdentity>) -> FipsBlobRoute<MemoryStore> {
        FipsBlobRoute::new(None, Weak::new(), peers, 4).unwrap()
    }
}
