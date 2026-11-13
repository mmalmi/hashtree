//! The client's payment step: resend, top up, reclaim.

use std::sync::Arc;
use std::time::Instant;

use hashtree_core::{StoreError, BLOB_MAX_BYTES};

use crate::account::PendingPayment;
use crate::client::Inner;
use crate::message::{ErrorCode, Quote, Reply, Request, METHOD_CASHU};
use crate::price::sat_ceil;
use crate::provider::normalize_mint;
use crate::wallet::{PaymentRequest, WalletError};

pub(crate) struct Funding;

impl Funding {
    /// Make the account able to pay for `quote` (or, without one, settle what
    /// is outstanding). Returns `false` when the client declines to pay.
    ///
    /// The step runs in its own task: a read that reaches its deadline stops
    /// waiting, but a token the wallet already created is never dropped.
    pub(crate) async fn run(
        inner: &Arc<Inner>,
        quote: Option<Quote>,
        deadline: Instant,
    ) -> Result<bool, StoreError> {
        let owner = Arc::clone(inner);
        let task = tokio::spawn(async move { owner.fund(quote).await });
        match tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), task).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(StoreError::Other("paid route payment task failed".into())),
            Err(_) => Err(StoreError::Other(
                "paid route payment is still in progress; availability is unknown".into(),
            )),
        }
    }
}

impl Inner {
    async fn fund(&self, quote: Option<Quote>) -> Result<bool, StoreError> {
        let _paying = self.paying.lock().await;
        self.send_pending().await?;

        let (balance_msat, terms) = {
            let mut state = self.state();
            // Reads still in flight may explain part of a low balance, but
            // never more than each of them authorised.
            let authorised_msat = self.policy.max_tariff.price_msat(BLOB_MAX_BYTES as u64);
            let in_flight_msat = authorised_msat.saturating_mul(state.gets_in_flight as u64);
            let loss_msat = state.account.loss_msat();
            if state.account.blocked.is_none()
                && loss_msat > self.policy.max_loss_msat.saturating_add(in_flight_msat)
            {
                state.account.blocked = Some(format!(
                    "provider balance is {loss_msat} msat lower than verified deliveries explain"
                ));
            }
            if state.account.blocked.is_some() {
                return Ok(false);
            }
            (state.account.balance_msat, state.terms.clone())
        };
        let (price_msat, grace_msat) = quote
            .as_ref()
            .map_or((0, 0), |quote| (quote.price_msat, quote.grace_msat));
        let need_msat = i128::from(price_msat) - i128::from(balance_msat) - i128::from(grace_msat);
        let (Ok(need_msat), Some(terms)) = (u64::try_from(need_msat), terms) else {
            // Already covered, perhaps by a concurrent read's top-up.
            return Ok(true);
        };
        if need_msat == 0 {
            return Ok(true);
        }

        let room_msat = i128::from(self.policy.max_prepaid_msat) - i128::from(balance_msat);
        let max_sat = sat_ceil(u64::try_from(room_msat.max(0)).unwrap_or(u64::MAX));
        let min_sat = sat_ceil(need_msat).max(terms.min_topup_sat);
        if min_sat > max_sat {
            self.state().stats.declined_limit += 1;
            return Ok(false);
        }
        let mints: Vec<String> = terms
            .mints
            .iter()
            .filter(|mint| {
                let own = self.policy.mints.iter();
                self.policy.mints.is_empty()
                    || own
                        .map(|own| normalize_mint(own))
                        .any(|own| own == normalize_mint(mint))
            })
            .cloned()
            .collect();
        if mints.is_empty() || !terms.methods.iter().any(|method| method == METHOD_CASHU) {
            self.state().stats.declined_mint += 1;
            return Ok(false);
        }

        let mut amount_sat = self.policy.topup_sat.clamp(min_sat, max_sat);
        let payment = loop {
            let request = PaymentRequest {
                provider: self.provider.clone(),
                mints: mints.clone(),
                amount_sat,
            };
            match self.wallet.pay(&request).await {
                Ok(payment) => break payment,
                Err(WalletError::NoAcceptedMint) => {
                    self.state().stats.declined_mint += 1;
                    return Ok(false);
                }
                // The preferred top-up did not fit; the bare minimum may.
                Err(error) if error.is_decline() && amount_sat > min_sat => amount_sat = min_sat,
                Err(error) if error.is_decline() => {
                    self.state().stats.declined_limit += 1;
                    return Ok(false);
                }
                Err(error) => {
                    return Err(StoreError::Other(format!("paid route {error}")));
                }
            }
        };
        {
            let mut state = self.state();
            state.stats.paid_sat += payment.amount_sat;
            state.stats.fee_sat += payment.fee_sat;
            state.account.pending = Some(PendingPayment {
                id: rand::random(),
                payment,
            });
        }
        self.send_pending().await?;
        Ok(true)
    }

    /// Hand the pending token to the provider. The token stays pending until
    /// the provider credits it or refuses it for good.
    async fn send_pending(&self) -> Result<(), StoreError> {
        let (account, pending) = {
            let state = self.state();
            match &state.account.pending {
                Some(pending) => (state.account.account, pending.clone()),
                None => return Ok(()),
            }
        };
        let request = Request::Pay {
            account,
            payment: pending.id,
            method: METHOD_CASHU.to_owned(),
            mint: pending.payment.mint_url.clone(),
            token: pending.payment.token.clone(),
        };
        let deadline = Instant::now() + self.policy.request_timeout;
        match self.exchange(&request, deadline).await? {
            Reply::Paid {
                payment,
                credited_msat,
                balance_msat,
            } if payment == pending.id => {
                let mut state = self.state();
                state.account.pending = None;
                state.account.credited_msat =
                    state.account.credited_msat.saturating_add(credited_msat);
                self.note_balance(&mut state, balance_msat, 0);
                Ok(())
            }
            Reply::Error {
                code: ErrorCode::Retry,
                detail,
                ..
            } => Err(StoreError::Other(format!(
                "paid route provider could not take the payment yet: {detail}"
            ))),
            Reply::Error { code, detail, .. } => {
                // Refused for good: the token is ours again if the wallet can
                // take it back. Cleared only afterwards, so an interrupted
                // reclaim is repeated.
                let reclaimed = self.wallet.reclaim(&pending.payment).await.is_ok();
                let mut state = self.state();
                state.account.pending = None;
                state.stats.rejected_payments += 1;
                if reclaimed {
                    state.stats.paid_sat = state
                        .stats
                        .paid_sat
                        .saturating_sub(pending.payment.amount_sat);
                }
                Err(StoreError::Other(format!(
                    "paid route provider refused the payment ({}): {detail}",
                    code.as_str()
                )))
            }
            _ => Err(StoreError::Other(
                "paid route provider sent an unexpected reply to a payment".into(),
            )),
        }
    }
}
