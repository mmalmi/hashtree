use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use fips_core::PeerIdentity;
use hashtree_core::Hash;
use tokio::time::Instant;

const RETENTION: Duration = Duration::from_secs(1);
const MAX_ENTRIES: usize = 128;

struct InboundRequest {
    hash: Hash,
    peer: [u8; 16],
    expires: Instant,
}

/// Local scheduling hints from authenticated requests on this transport.
#[derive(Default)]
pub(crate) struct RecentRequesters(Mutex<VecDeque<InboundRequest>>);

impl RecentRequesters {
    #[cfg(test)]
    pub(crate) fn last_request(&self) -> Option<(Hash, [u8; 16])> {
        self.0
            .lock()
            .unwrap()
            .back()
            .map(|entry| (entry.hash, entry.peer))
    }

    pub(crate) fn record(&self, hash: Hash, peer: PeerIdentity) {
        let mut entries = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        while entries.front().is_some_and(|entry| entry.expires <= now) {
            entries.pop_front();
        }
        if entries.len() == MAX_ENTRIES {
            entries.pop_front();
        }
        entries.push_back(InboundRequest {
            hash,
            peer: *peer.node_addr().as_bytes(),
            expires: now + RETENTION,
        });
    }

    pub(crate) fn choose(
        &self,
        hash: &Hash,
        planned: PeerIdentity,
        spares: &mut Vec<PeerIdentity>,
    ) -> PeerIdentity {
        if spares.is_empty() {
            return planned;
        }
        let entries = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let requested = |peer: &PeerIdentity| {
            entries
                .iter()
                .rev()
                .take_while(|entry| entry.expires > now)
                .any(|entry| entry.hash == *hash && &entry.peer == peer.node_addr().as_bytes())
        };
        if requested(&planned) {
            if let Some(next) = spares.iter().position(|peer| !requested(peer)) {
                return spares.remove(next);
            }
        }
        planned
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fips_core::Identity;

    fn peer() -> PeerIdentity {
        PeerIdentity::from_pubkey(Identity::generate().pubkey())
    }

    #[tokio::test(start_paused = true)]
    async fn substitutes_only_a_fresh_alternative_for_the_same_hash() {
        let recent = RecentRequesters::default();
        let [planned, already_asked, fresh] = std::array::from_fn(|_| peer());
        let mut spares = vec![already_asked, fresh];
        recent.record([1; 32], planned);
        recent.record([1; 32], already_asked);
        assert_eq!(recent.choose(&[2; 32], planned, &mut spares), planned);
        assert_eq!(recent.choose(&[1; 32], planned, &mut spares), fresh);
        assert_eq!(spares, [already_asked]);
        assert_eq!(recent.choose(&[1; 32], planned, &mut spares), planned);

        tokio::time::advance(RETENTION).await;
        spares.push(fresh);
        assert_eq!(recent.choose(&[1; 32], planned, &mut spares), planned);
        assert_eq!(spares.len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn inbound_history_is_bounded_across_hashes() {
        let recent = RecentRequesters::default();
        let planned = peer();
        let fresh = peer();
        for i in 0..=MAX_ENTRIES {
            let mut hash = [0; 32];
            hash[..8].copy_from_slice(&(i as u64).to_le_bytes());
            recent.record(hash, planned);
        }
        assert_eq!(recent.0.lock().unwrap().len(), MAX_ENTRIES);
        assert_eq!(recent.choose(&[0; 32], planned, &mut vec![fresh]), planned);
        let mut retained = [0; 32];
        retained[0] = 1;
        assert_eq!(recent.choose(&retained, planned, &mut vec![fresh]), fresh);
    }
}
