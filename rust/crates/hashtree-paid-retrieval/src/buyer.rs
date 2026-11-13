//! The buying side of the exchange: one account with one provider.
//!
//! Nothing here fetches or verifies data. The caller asks for a unit, checks
//! what arrived against its hash, and tells the buyer; the buyer keeps the
//! ledger, decides about quotes and pays. A Hashtree blob route and a
//! BitTorrent downloader can share it as it is.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use hashtree_core::StoreError;

use crate::account::{AccountState, ClientPolicy, PaidRouteStats};
use crate::funding::Funding;
use crate::message::{AccountId, ContentHash, Quote, Reply, Request};
use crate::transport::PaidBlobTransport;
use crate::wallet::RetrievalWallet;

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
    /// Units whose charge is already in `charged_msat`, newest last.
    pub counted: VecDeque<ContentHash>,
    pub in_flight: usize,
    /// What the requests in flight authorised the provider to charge.
    pub in_flight_msat: u64,
    pub stats: PaidRouteStats,
}

pub(crate) struct Inner {
    pub provider: String,
    pub transport: Arc<dyn PaidBlobTransport>,
    pub wallet: Arc<dyn RetrievalWallet>,
    pub policy: ClientPolicy,
    pub state: Mutex<State>,
    /// Serialises payments so that concurrent requests share one top-up.
    pub paying: tokio::sync::Mutex<()>,
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
    /// took more than verified deliveries explain. `own` is the number of
    /// requests in flight that belong to the caller; while others are in
    /// flight the reported balance may already include their charges.
    pub(crate) fn note_balance(&self, state: &mut State, balance_msat: i64, own: usize) {
        state.account.balance_msat = balance_msat;
        if state.in_flight > own || state.account.blocked.is_some() {
            return;
        }
        let loss_msat = state.account.loss_msat();
        if loss_msat > self.policy.max_loss_msat {
            state.account.blocked = Some(format!(
                "provider balance is {loss_msat} msat lower than verified deliveries explain"
            ));
        }
    }
}

/// One request for a unit that the provider has not answered yet.
pub struct InFlight {
    inner: Arc<Inner>,
    authorised_msat: u64,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        let mut state = self.inner.state();
        state.in_flight -= 1;
        state.in_flight_msat = state.in_flight_msat.saturating_sub(self.authorised_msat);
    }
}

/// The client's account with one provider: ledger, quote decisions, payment.
#[derive(Clone)]
pub struct PaidBuyer {
    inner: Arc<Inner>,
}

impl PaidBuyer {
    /// `provider` is the application's name for the seller, passed to the
    /// wallet with every payment.
    pub fn new(
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
                    in_flight: 0,
                    in_flight_msat: 0,
                    stats: PaidRouteStats::default(),
                }),
                paying: tokio::sync::Mutex::new(()),
            }),
        }
    }

    pub fn policy(&self) -> &ClientPolicy {
        &self.inner.policy
    }

    pub fn account_id(&self) -> AccountId {
        self.inner.state().account.account
    }

    /// The account as it should be persisted.
    pub fn account_state(&self) -> AccountState {
        self.inner.state().account.clone()
    }

    pub fn stats(&self) -> PaidRouteStats {
        self.inner.state().stats.clone()
    }

    /// Pay this provider again after a block, writing off the recorded loss.
    pub fn clear_block(&self) {
        let mut state = self.inner.state();
        let loss_msat = state.account.loss_msat();
        state.account.charged_msat = state.account.charged_msat.saturating_add(loss_msat);
        state.account.blocked = None;
    }

    /// Get ready to request a unit: an unacknowledged token or a debt comes
    /// first. Returns `false` when this provider is blocked.
    pub async fn prepare(&self, deadline: Instant) -> Result<bool, StoreError> {
        let unsettled = {
            let mut state = self.inner.state();
            if state.account.blocked.is_some() {
                state.stats.declined_blocked += 1;
                return Ok(false);
            }
            state.account.pending.is_some() || state.account.balance_msat < 0
        };
        if unsettled {
            Funding::run(&self.inner, None, deadline).await?;
        }
        Ok(true)
    }

    /// Mark a request as outstanding until the returned guard is dropped.
    /// `authorised_msat` is the most the request lets the provider charge.
    pub fn begin(&self, authorised_msat: u64) -> InFlight {
        let mut state = self.inner.state();
        state.in_flight += 1;
        state.in_flight_msat = state.in_flight_msat.saturating_add(authorised_msat);
        InFlight {
            inner: Arc::clone(&self.inner),
            authorised_msat,
        }
    }

    /// Send one request over the transport and decode its reply.
    pub async fn exchange(
        &self,
        request: &Request,
        deadline: Instant,
    ) -> Result<Reply, StoreError> {
        self.inner.exchange(request, deadline).await
    }

    /// Take note of a quote. Returns `false` when its price is above
    /// `max_tariff`, in which case it must not be funded.
    pub fn quoted(&self, _request: &InFlight, quote: &Quote) -> bool {
        let mut state = self.inner.state();
        self.inner.note_balance(&mut state, quote.balance_msat, 1);
        state.terms = Some(Terms {
            mints: quote.mints.clone(),
            methods: quote.methods.clone(),
            min_topup_sat: quote.min_topup_sat,
        });
        if quote.price_msat > self.inner.policy.max_tariff.price_msat(quote.size) {
            state.stats.declined_price += 1;
            return false;
        }
        true
    }

    /// Record a unit of `size` bytes that arrived and matched its hash.
    pub fn delivered(
        &self,
        request: &InFlight,
        content: ContentHash,
        size: u64,
        charged_msat: u64,
        balance_msat: i64,
        duplicate: bool,
    ) {
        let mut state = self.inner.state();
        // A free repeat reports the original charge. It is counted only if
        // that charge's first reply never arrived here.
        if !(duplicate && state.counted.contains(&content)) {
            state.account.charged_msat = state.account.charged_msat.saturating_add(charged_msat);
            state.counted.push_back(content);
            if state.counted.len() > MAX_COUNTED {
                state.counted.pop_front();
            }
        }
        state.stats.blobs += 1;
        state.stats.bytes += size;
        let fair_msat = self.inner.policy.max_tariff.price_msat(size);
        if charged_msat > request.authorised_msat.min(fair_msat) && state.account.blocked.is_none()
        {
            state.account.blocked = Some(format!(
                "provider charged {charged_msat} msat for a unit worth at most {fair_msat}"
            ));
        }
        self.inner.note_balance(&mut state, balance_msat, 1);
    }

    /// Record a reply whose bytes did not match the hash. It is not a
    /// delivery: its charge stays unexplained and counts towards the loss
    /// that blocks this provider.
    pub fn undelivered(&self, _request: &InFlight, balance_msat: i64) {
        let mut state = self.inner.state();
        state.stats.invalid_blobs += 1;
        self.inner.note_balance(&mut state, balance_msat, 1);
    }

    /// Make the account able to pay for `quote`; without one, resend an
    /// unacknowledged payment and pay any debt. Returns `false` when the
    /// client declines to pay (limit, mint, blocked provider).
    pub async fn fund(&self, quote: Option<Quote>, deadline: Instant) -> Result<bool, StoreError> {
        Funding::run(&self.inner, quote, deadline).await
    }

    /// Resend an unacknowledged payment and pay any debt now.
    pub async fn settle(&self) -> Result<(), StoreError> {
        let deadline = Instant::now() + self.inner.policy.request_timeout;
        self.fund(None, deadline).await.map(|_| ())
    }
}
