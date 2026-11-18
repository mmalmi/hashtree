use std::future::Future;

use crate::recent_requesters::RecentRequesters;
use fips_core::PeerIdentity;
use hashtree_core::{BlobReply, BlobRequest, BlobRouteContext, StoreError};
use tokio::task::JoinSet;
use tokio::time::{sleep_until, Duration, Instant};

use super::completed_misses;

const PROVIDER_HEDGE_DELAY: Duration = Duration::from_millis(100);
const MIN_PROVIDER_HEDGE_DELAY: Duration = Duration::from_millis(50);

pub(super) struct ProviderWindow {
    pub(super) planned: Vec<PeerIdentity>,
    pub(super) spares: Vec<PeerIdentity>,
    pub(super) untried: usize,
}

impl ProviderWindow {
    pub(super) async fn race<F>(
        self,
        request: BlobRequest,
        context: Option<BlobRouteContext>,
        recent: &RecentRequesters,
        mut attempt: impl FnMut(PeerIdentity) -> F,
    ) -> Result<BlobReply, StoreError>
    where
        F: Future<Output = Result<BlobReply, StoreError>> + Send + 'static,
    {
        let mut spares = self.spares;
        race_providers(self.planned, self.untried, request.htl, context, |peer| {
            attempt(recent.choose(&request.hash, peer, &mut spares))
        })
        .await
    }
}

async fn race_providers<F>(
    peers: Vec<PeerIdentity>,
    untried: usize,
    remaining_htl: u8,
    context: Option<BlobRouteContext>,
    mut attempt: impl FnMut(PeerIdentity) -> F,
) -> Result<BlobReply, StoreError>
where
    F: Future<Output = Result<BlobReply, StoreError>> + Send + 'static,
{
    let deadline_at = context.map(|context| Instant::from_std(context.deadline));
    // Budget half the remaining time for staggered starts along a potential
    // mesh path. Keep normal pacing for long deadlines and a floor for bursts.
    let gaps_per_hop = peers.len().saturating_sub(1).max(1) as u32;
    let hedge_delay = deadline_at.map_or(PROVIDER_HEDGE_DELAY, |deadline| {
        let scheduling_budget = deadline.saturating_duration_since(Instant::now()) / 2;
        let gaps = gaps_per_hop * (u32::from(remaining_htl) + 1);
        (scheduling_budget / gaps).clamp(MIN_PROVIDER_HEDGE_DELAY, PROVIDER_HEDGE_DELAY)
    });
    let deadline = async {
        match deadline_at {
            Some(deadline) => sleep_until(deadline).await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(deadline);
    let mut peers = peers.into_iter();
    let mut attempts = JoinSet::new();
    let mut next_hedge = Instant::now();
    let mut first_error = None;
    while !attempts.is_empty() || peers.len() > 0 {
        let in_flight = !attempts.is_empty();
        let joined = tokio::select! {
            biased;
            _ = &mut deadline => {
                return Err(StoreError::Other(
                    "FIPS blob provider-set deadline expired".to_string(),
                ));
            }
            joined = attempts.join_next(), if in_flight => joined,
            // Pace additional concurrent work. A completed unsuccessful
            // attempt releases its slot even if another provider is stalled.
            _ = async { if in_flight { sleep_until(next_hedge).await } }, if peers.len() > 0 => {
                let peer = peers.next().expect("branch requires an untried peer");
                attempts.spawn(attempt(peer));
                next_hedge = Instant::now() + hedge_delay;
                continue;
            }
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
        next_hedge = Instant::now();
    }
    match first_error {
        Some(error) => Err(StoreError::Other(format!(
            "FIPS blob provider set was incomplete: {error}"
        ))),
        None => completed_misses(untried),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fips_core::Identity;

    #[tokio::test(start_paused = true)]
    async fn provider_miss_releases_the_next_attempt_before_the_hedge() {
        let peers = peers(2);
        let first = peers[0];
        let start = Instant::now();
        let reply = race_providers(peers, 0, 0, None, |peer| {
            let miss = peer == first;
            async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                Ok(if miss {
                    BlobReply::NoResult
                } else {
                    BlobReply::Data(vec![7])
                })
            }
        })
        .await
        .unwrap();
        assert_eq!(reply, BlobReply::Data(vec![7]));
        assert!(start.elapsed() < MIN_PROVIDER_HEDGE_DELAY);
    }

    #[tokio::test(start_paused = true)]
    async fn a_miss_releases_its_slot_while_another_provider_is_pending() {
        let peers = peers(3);
        let stalled = peers[0];
        let missing = peers[1];
        let start = Instant::now();
        let reply = race_providers(peers, 0, 0, None, |peer| async move {
            if peer == stalled {
                return std::future::pending().await;
            }
            if peer == missing {
                tokio::time::sleep(Duration::from_millis(5)).await;
                return Ok(BlobReply::NoResult);
            }
            Ok(BlobReply::Data(vec![7]))
        })
        .await
        .unwrap();
        assert_eq!(reply, BlobReply::Data(vec![7]));
        assert!(start.elapsed() < PROVIDER_HEDGE_DELAY * 2);
    }

    #[tokio::test(start_paused = true)]
    async fn hedge_uses_the_multihop_budget_while_a_provider_is_pending() {
        for (budget, delay) in [
            (Duration::from_millis(700), MIN_PROVIDER_HEDGE_DELAY),
            (Duration::from_secs(10), PROVIDER_HEDGE_DELAY),
        ] {
            let peers = peers(4);
            let first = peers[0];
            let start = Instant::now();
            let context = BlobRouteContext {
                deadline: (start + budget).into_std(),
                attempt_budget: peers.len(),
            };
            let reply = race_providers(peers, 0, 4, Some(context), |peer| {
                let stalls = peer == first;
                async move {
                    if stalls {
                        std::future::pending().await
                    } else {
                        Ok(BlobReply::Data(vec![7]))
                    }
                }
            })
            .await
            .unwrap();
            assert_eq!(reply, BlobReply::Data(vec![7]));
            assert!(
                start.elapsed() >= delay && start.elapsed() <= delay + Duration::from_millis(1)
            );
        }
    }

    fn peers(count: usize) -> Vec<PeerIdentity> {
        (0..count)
            .map(|_| PeerIdentity::from_npub(&Identity::generate().npub()).unwrap())
            .collect()
    }
}
