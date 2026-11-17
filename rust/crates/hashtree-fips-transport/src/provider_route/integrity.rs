use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use fips_core::PeerIdentity;
use hashtree_core::{BlobReply, StoreError};
use tokio::time::Instant;

use crate::TcpBlobTransportError;

const COOLDOWN: Duration = Duration::from_secs(30);
const MAX_PEERS: usize = 256;

#[derive(Default)]
pub(super) struct IntegrityCooldowns {
    until: Mutex<HashMap<[u8; 32], Instant>>,
}

impl IntegrityCooldowns {
    pub(super) fn check_reply(
        &self,
        peer: PeerIdentity,
        reply: Result<BlobReply, TcpBlobTransportError>,
    ) -> Result<BlobReply, StoreError> {
        if matches!(reply, Err(TcpBlobTransportError::HashMismatch)) {
            let mut until = self.until.lock().unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            let key = peer.pubkey().serialize();
            until.retain(|_, expiry| *expiry > now);
            if until.len() == MAX_PEERS && !until.contains_key(&key) {
                if let Some(oldest) = until
                    .iter()
                    .min_by_key(|(id, expiry)| (*expiry, *id))
                    .map(|(id, _)| *id)
                {
                    until.remove(&oldest);
                }
            }
            until.insert(key, now + COOLDOWN);
        }
        reply.map_err(|error| StoreError::Other(error.to_string()))
    }

    /// Leave the requester in its reserved position until the route removes the return hop.
    pub(super) fn defer(
        &self,
        peers: &mut Vec<PeerIdentity>,
        requester: Option<PeerIdentity>,
    ) -> usize {
        let until = self.until.lock().unwrap_or_else(|e| e.into_inner());
        let count = peers.len();
        if !until.is_empty() {
            let requester = requester.map(|peer| peer.pubkey().serialize());
            let now = Instant::now();
            peers.retain(|peer| {
                let key = peer.pubkey().serialize();
                Some(key) == requester || until.get(&key).is_none_or(|expiry| *expiry <= now)
            });
        }
        count - peers.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_route::completed_misses;
    use crate::provider_route::tests::{test_peers, test_route};

    #[tokio::test(start_paused = true)]
    async fn only_direct_corruption_defers_a_provider_and_the_search_remains_incomplete() {
        let peer = test_peers(1)[0];
        let route = test_route(vec![peer]);
        let hash = [7; 32];
        for reply in [
            Ok(BlobReply::NoResult),
            Err(TcpBlobTransportError::Timeout),
            Err(TcpBlobTransportError::Store(StoreError::Other(
                "downstream hash mismatch".into(),
            ))),
        ] {
            let _ = route.integrity.check_reply(peer, reply);
            assert_eq!(route.provider_attempts(hash, 1).unwrap(), (vec![peer], 0));
        }
        assert!(route
            .integrity
            .check_reply(peer, Err(TcpBlobTransportError::HashMismatch))
            .is_err());
        let (eligible, deferred) = route.provider_attempts(hash, 1).unwrap();
        assert!(eligible.is_empty());
        assert!(completed_misses(deferred).is_err());
        crate::tcp_blob::INBOUND_BLOB_PEER
            .scope((hashtree_core::BlobRequest { hash, htl: 1 }, peer), async {
                assert_eq!(route.provider_attempts(hash, 4).unwrap(), (vec![], 0));
            })
            .await;
        tokio::time::advance(COOLDOWN).await;
        assert_eq!(route.provider_attempts(hash, 1).unwrap(), (vec![peer], 0));
    }

    #[tokio::test(start_paused = true)]
    async fn repeated_corruption_renews_one_entry_and_tracking_stays_bounded() {
        let peers = test_peers(MAX_PEERS + 1);
        let cooldowns = IntegrityCooldowns::default();
        for &peer in &peers[..MAX_PEERS] {
            let _ = cooldowns.check_reply(peer, Err(TcpBlobTransportError::HashMismatch));
            tokio::time::advance(Duration::from_millis(1)).await;
        }
        for &peer in &[peers[0], peers[MAX_PEERS]] {
            let _ = cooldowns.check_reply(peer, Err(TcpBlobTransportError::HashMismatch));
        }
        let mut eligible = peers.clone();
        assert_eq!(cooldowns.defer(&mut eligible, None), MAX_PEERS);
        assert_eq!(eligible, vec![peers[1]]);
    }
}
