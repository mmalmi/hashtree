//! The buying side: one opaque `BlobRoute` that pays one provider.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use async_trait::async_trait;
use hashtree_core::{
    sha256, BlobReply, BlobRequest, BlobRoute, BlobRouteContext, Hash, StoreError, BLOB_MAX_BYTES,
};

use crate::account::{AccountState, ClientPolicy, PaidRouteStats};
use crate::funding::Funding;
use crate::message::{Quote, Reply, Request};
use crate::transport::PaidBlobTransport;
use crate::wallet::RetrievalWallet;

/// A get that keeps asking for money has not converged; give up on the read.
const MAX_ROUNDS: usize = 4;
/// Charges remembered so that a free repeat is not counted a second time.
const MAX_COUNTED: usize = 128;

/// Terms from the provider's latest quote, needed to settle a debt later.
#[derive(Clone, Debug)]
pub(crate) struct Terms {
    pub mints: Vec<String>,
    pub methods: Vec<String>,
    pub min_topup_sat: u64,
}

pub(crate) struct State {
    pub account: AccountState,
    pub terms: Option<Terms>,
    /// Hashes whose charge is already in `charged_msat`, newest last.
    pub counted: VecDeque<Hash>,
    pub gets_in_flight: usize,
    pub stats: PaidRouteStats,
}

pub(crate) struct Inner {
    pub provider: String,
    pub transport: Arc<dyn PaidBlobTransport>,
    pub wallet: Arc<dyn RetrievalWallet>,
    pub policy: ClientPolicy,
    pub state: Mutex<State>,
    /// Serialises payments so that concurrent reads share one top-up.
    pub paying: tokio::sync::Mutex<()>,
}

struct GetInFlight<'a>(&'a Inner);

impl Drop for GetInFlight<'_> {
    fn drop(&mut self) {
        self.0.state().gets_in_flight -= 1;
    }
}

impl Inner {
    pub(crate) fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    pub(crate) async fn exchange(
        &self,
        request: &Request,
        deadline: Instant,
    ) -> Result<Reply, StoreError> {
        let deadline = tokio::time::Instant::from_std(deadline);
        let reply = tokio::time::timeout_at(deadline, self.transport.exchange(request.encode()))
            .await
            .map_err(|_| {
                StoreError::Other("paid route timed out; availability is unknown".into())
            })??;
        Reply::decode(&reply)
            .map_err(|error| StoreError::Other(format!("paid route provider sent an {error}")))
    }

    /// Record the provider's reported balance and stop paying a provider that
    /// took more than verified deliveries explain. `own_gets` is the number of
    /// in-flight gets belonging to the caller; while others are in flight the
    /// reported balance may already include their charges.
    pub(crate) fn note_balance(&self, state: &mut State, balance_msat: i64, own_gets: usize) {
        state.account.balance_msat = balance_msat;
        if state.gets_in_flight > own_gets || state.account.blocked.is_some() {
            return;
        }
        let loss_msat = state.account.loss_msat();
        if loss_msat > self.policy.max_loss_msat {
            state.account.blocked = Some(format!(
                "provider balance is {loss_msat} msat lower than verified deliveries explain"
            ));
        }
    }

    async fn fetch(
        self: &Arc<Self>,
        hash: Hash,
        deadline: Instant,
    ) -> Result<BlobReply, StoreError> {
        let max_msat = self.policy.max_tariff.price_msat(BLOB_MAX_BYTES as u64);
        for _ in 0..MAX_ROUNDS {
            let unsettled = {
                let mut state = self.state();
                if state.account.blocked.is_some() {
                    state.stats.declined_blocked += 1;
                    return Ok(BlobReply::NoResult);
                }
                state.account.pending.is_some() || state.account.balance_msat < 0
            };
            if unsettled {
                // An unacknowledged token or a debt comes before new requests.
                Funding::run(self, None, deadline).await?;
            }
            let reply = {
                self.state().gets_in_flight += 1;
                let _in_flight = GetInFlight(self);
                let request = Request::Get {
                    hash,
                    account: self.state().account.account,
                    max_msat,
                };
                let reply = self.exchange(&request, deadline).await?;
                self.accept(hash, max_msat, reply)?
            };
            match reply {
                Accepted::Reply(reply) => return Ok(reply),
                Accepted::Quote(quote) => {
                    if !Funding::run(self, Some(quote), deadline).await? {
                        return Ok(BlobReply::NoResult);
                    }
                }
            }
        }
        Err(StoreError::Other(
            "paid route provider kept asking for payment".into(),
        ))
    }

    /// Check one reply to a get while that get still counts as in flight.
    fn accept(&self, hash: Hash, max_msat: u64, reply: Reply) -> Result<Accepted, StoreError> {
        let protocol = |what: &str| StoreError::Other(format!("paid route provider {what}"));
        let mut state = self.state();
        match reply {
            Reply::NoResult { hash: replied } if replied == hash => {
                Ok(Accepted::Reply(BlobReply::NoResult))
            }
            Reply::Data {
                hash: replied,
                charged_msat,
                balance_msat,
                duplicate,
                data,
            } if replied == hash => {
                if data.len() > BLOB_MAX_BYTES || sha256(&data) != hash {
                    // Not a delivery: the charge stays unexplained and counts
                    // towards the loss that blocks this provider.
                    state.stats.invalid_blobs += 1;
                    self.note_balance(&mut state, balance_msat, 1);
                    return Err(protocol("returned content with the wrong hash"));
                }
                // A free repeat reports the original charge. It is counted
                // only if that charge's first reply never arrived here.
                if !(duplicate && state.counted.contains(&hash)) {
                    state.account.charged_msat =
                        state.account.charged_msat.saturating_add(charged_msat);
                    state.counted.push_back(hash);
                    if state.counted.len() > MAX_COUNTED {
                        state.counted.pop_front();
                    }
                }
                state.stats.blobs += 1;
                state.stats.bytes += data.len() as u64;
                let fair_msat = self.policy.max_tariff.price_msat(data.len() as u64);
                if charged_msat > max_msat.min(fair_msat) && state.account.blocked.is_none() {
                    state.account.blocked = Some(format!(
                        "provider charged {charged_msat} msat for a blob worth at most {fair_msat}"
                    ));
                }
                self.note_balance(&mut state, balance_msat, 1);
                Ok(Accepted::Reply(BlobReply::Data(data)))
            }
            Reply::Quote(quote) if quote.hash == hash => {
                self.note_balance(&mut state, quote.balance_msat, 1);
                state.terms = Some(Terms {
                    mints: quote.mints.clone(),
                    methods: quote.methods.clone(),
                    min_topup_sat: quote.min_topup_sat,
                });
                if quote.size > BLOB_MAX_BYTES as u64 {
                    return Err(protocol("quoted an oversized blob"));
                }
                if quote.price_msat > self.policy.max_tariff.price_msat(quote.size) {
                    state.stats.declined_price += 1;
                    return Ok(Accepted::Reply(BlobReply::NoResult));
                }
                Ok(Accepted::Quote(quote))
            }
            Reply::Error { code, detail, .. } => Err(protocol(&format!(
                "refused the request ({}): {detail}",
                code.as_str()
            ))),
            _ => Err(protocol("sent an unexpected reply")),
        }
    }
}

enum Accepted {
    Reply(BlobReply),
    Quote(Quote),
}

/// Fetches blobs from one paying-only provider.
///
/// Register it with a `BlobRouter` like any other route. A read the client
/// declines to pay for is `NoResult`; [`PaidBlobRoute::stats`] says why.
#[derive(Clone)]
pub struct PaidBlobRoute {
    inner: Arc<Inner>,
}

impl PaidBlobRoute {
    /// `provider` is the application's name for the seller, passed to the
    /// wallet with every payment.
    pub fn new(
        provider: impl Into<String>,
        transport: Arc<dyn PaidBlobTransport>,
        wallet: Arc<dyn RetrievalWallet>,
        policy: ClientPolicy,
    ) -> Self {
        Self::with_account(provider, transport, wallet, policy, AccountState::new())
    }

    /// Continue with an account the application persisted.
    pub fn with_account(
        provider: impl Into<String>,
        transport: Arc<dyn PaidBlobTransport>,
        wallet: Arc<dyn RetrievalWallet>,
        policy: ClientPolicy,
        account: AccountState,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                provider: provider.into(),
                transport,
                wallet,
                policy,
                state: Mutex::new(State {
                    account,
                    terms: None,
                    counted: VecDeque::new(),
                    gets_in_flight: 0,
                    stats: PaidRouteStats::default(),
                }),
                paying: tokio::sync::Mutex::new(()),
            }),
        }
    }

    /// The account as it should be persisted.
    pub fn account_state(&self) -> AccountState {
        self.inner.state().account.clone()
    }

    pub fn stats(&self) -> PaidRouteStats {
        self.inner.state().stats.clone()
    }

    /// Resend an unacknowledged payment and pay any debt now.
    pub async fn settle(&self) -> Result<(), StoreError> {
        let deadline = Instant::now() + self.inner.policy.request_timeout;
        Funding::run(&self.inner, None, deadline).await.map(|_| ())
    }

    /// Pay this provider again after a block, writing off the recorded loss.
    pub fn clear_block(&self) {
        let mut state = self.inner.state();
        let loss_msat = state.account.loss_msat();
        state.account.charged_msat = state.account.charged_msat.saturating_add(loss_msat);
        state.account.blocked = None;
    }
}

#[async_trait]
impl BlobRoute for PaidBlobRoute {
    async fn route(&self, request: BlobRequest) -> Result<BlobReply, StoreError> {
        let deadline = Instant::now() + self.inner.policy.request_timeout;
        self.inner.fetch(request.hash, deadline).await
    }

    async fn route_with_context(
        &self,
        request: BlobRequest,
        context: BlobRouteContext,
    ) -> Result<BlobReply, StoreError> {
        self.inner.fetch(request.hash, context.deadline).await
    }
}
