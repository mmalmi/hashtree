use std::future::Future;

use fips_core::PeerIdentity;
use hashtree_core::{BlobReply, BlobRouteContext, StoreError};
use tokio::task::JoinSet;
use tokio::time::Duration;

use super::completed_misses;

const PROVIDER_HEDGE_DELAY: Duration = Duration::from_millis(100);

pub(super) async fn race_providers<F>(
    peers: Vec<PeerIdentity>,
    untried: usize,
    _remaining_htl: u8,
    context: Option<BlobRouteContext>,
    mut attempt: impl FnMut(PeerIdentity) -> F,
) -> Result<BlobReply, StoreError>
where
    F: Future<Output = Result<BlobReply, StoreError>> + Send + 'static,
{
    let mut attempts = JoinSet::new();
    for (index, peer) in peers.into_iter().enumerate() {
        let result = attempt(peer);
        attempts.spawn(async move {
            let delay = PROVIDER_HEDGE_DELAY.saturating_mul(index as u32);
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            result.await
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
