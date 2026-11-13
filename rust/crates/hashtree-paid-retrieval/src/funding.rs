//! The client's payment step: resend, top up, reclaim.

use std::sync::Arc;
use std::time::Instant;

use hashtree_core::StoreError;

use crate::account::{PaymentIntent, PendingPayment};
use crate::buyer::Inner;
use crate::message::{ErrorCode, PaymentId, Quote, Reply, Request, METHOD_CASHU};
use crate::price::sat_ceil;
use crate::seller::normalize_mint;
use crate::wallet::{PaymentRequest, WalletError};

/// Answers of "not yet" after which a payment stops holding up new ones.
const MAX_ATTEMPTS: u32 = 3;
/// Tokens of unknown fate kept per provider; beyond it nothing more is paid.
const MAX_UNRESOLVED: usize = 8;

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
        carry: bool,
        deadline: Instant,
    ) -> Result<bool, StoreError> {
        let owner = Arc::clone(inner);
        let task = tokio::spawn(async move { owner.fund(quote, carry).await });
        match tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), task).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(StoreError::Other("paid route payment task failed".into())),
            Err(_) => Err(StoreError::Other(
                "paid route payment is still in progress; availability is unknown".into(),
            )),
        }
    }
}

/// What became of one token handed to the provider.
enum Sent {
    Credited,
    /// Refused for good, and the wallet took the token back.
    Reclaimed(String),
    /// Refused for good, and the wallet could not take it back.
    Refused(String),
    /// The provider cannot take it yet.
    NotYet(String),
}

impl Inner {
    /// With `carry`, a new token is left pending for the caller to carry in
    /// its next request instead of being sent on its own.
    async fn fund(&self, quote: Option<Quote>, carry: bool) -> Result<bool, StoreError> {
        let _paying = self.paying.lock().await;
        self.recover_intent().await?;
        self.settle_pending().await?;
        self.retry_unresolved().await;

        let (balance_msat, unresolved_msat, terms) = {
            let mut state = self.state();
            // Requests still in flight may explain part of a low balance,
            // but never more than they authorised.
            let allowed_msat = self
                .policy
                .max_loss_msat
                .saturating_add(state.in_flight_msat);
            let loss_msat = state.account.loss_msat();
            if state.account.blocked.is_none() && !state.resync && loss_msat > allowed_msat {
                state.account.blocked = Some(format!(
                    "provider balance is {loss_msat} msat lower than verified deliveries explain"
                ));
            }
            if state.account.blocked.is_some() {
                return Ok(false);
            }
            (
                state.account.balance_msat,
                state.account.unresolved_msat(),
                state.terms.clone(),
            )
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

        // Tokens of unknown fate may still be redeemed by the provider, so
        // they count as money it holds.
        let room_msat = i128::from(self.policy.max_prepaid_msat)
            - i128::from(balance_msat)
            - i128::from(unresolved_msat);
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

        let id: PaymentId = rand::random();
        let mut amount_sat = self.policy.topup_sat.clamp(min_sat, max_sat);
        let payment = loop {
            // The intent is on disk before the wallet is asked: a crash after
            // this point is followed by asking the wallet about this id.
            self.state().account.intent = Some(PaymentIntent {
                id,
                amount_sat,
                mints: mints.clone(),
            });
            if let Err(error) = self.persist() {
                self.state().account.intent = None;
                return Err(error);
            }
            let request = PaymentRequest {
                provider: self.provider.clone(),
                mints: mints.clone(),
                amount_sat,
                max_fee_sat: self.policy.max_token_fee_sat,
                lock: terms.lock,
                payment_id: id,
            };
            let declined = match self.wallet.pay(&request).await {
                Ok(payment) => break payment,
                // The preferred top-up did not fit; the bare minimum may.
                Err(error) if error.is_decline() && amount_sat > min_sat => {
                    amount_sat = min_sat;
                    continue;
                }
                Err(error) if error.is_decline() => error,
                // The wallet may have created a token all the same: the
                // intent stays, and the wallet is asked about it next time.
                Err(error) => return Err(StoreError::Other(format!("paid route {error}"))),
            };
            {
                let mut state = self.state();
                state.account.intent = None;
                match declined {
                    WalletError::NoAcceptedMint => state.stats.declined_mint += 1,
                    _ => state.stats.declined_limit += 1,
                }
            }
            let _ = self.persist();
            return Ok(false);
        };
        self.adopt(id, payment);
        if !carry {
            self.settle_pending().await?;
        }
        Ok(true)
    }

    /// Make a token the wallet produced the pending payment, and save it
    /// before it is sent anywhere.
    fn adopt(&self, id: PaymentId, payment: crate::wallet::WalletPayment) {
        {
            let mut state = self.state();
            state.stats.paid_sat += payment.amount_sat;
            state.stats.fee_sat += payment.fee_sat;
            state.account.intent = None;
            state.account.pending = Some(PendingPayment {
                id,
                payment,
                attempts: 0,
            });
        }
        // If this save fails the older record still names the intent, and the
        // wallet is asked for the token again after a restart.
        let _ = self.persist();
    }

    /// After a restart: a payment the wallet was asked for may exist without
    /// this account knowing its token.
    async fn recover_intent(&self) -> Result<(), StoreError> {
        let intent = {
            let state = self.state();
            match (&state.account.intent, &state.account.pending) {
                (Some(intent), None) => intent.clone(),
                _ => return Ok(()),
            }
        };
        let found = self.wallet.recover(&intent.id).await;
        let found = found.map_err(|error| StoreError::Other(format!("paid route {error}")))?;
        match found {
            Some(payment) => self.adopt(intent.id, payment),
            None => {
                // The wallet made no token it can hand over; if it made one
                // at all, it is in the wallet's own records.
                self.state().account.intent = None;
                let _ = self.persist();
            }
        }
        Ok(())
    }

    /// Hand the pending token to the provider. It stays pending until the
    /// provider credits it, the wallet takes it back, or it is set aside as
    /// unresolved; it is never dropped.
    async fn settle_pending(&self) -> Result<(), StoreError> {
        let Some(pending) = self.state().account.pending.clone() else {
            return Ok(());
        };
        let sent = self.send(&pending).await?;
        self.conclude(pending, sent)
    }

    /// A payment carried by a `get`: settle it from the reply, if the reply
    /// says what became of it. Returns whether it was credited. The caller
    /// holds the payment lock from before the request was sent.
    pub(crate) async fn settle_carried(
        &self,
        pending: &PendingPayment,
        reply: &Reply,
    ) -> Result<bool, StoreError> {
        let credit = match reply {
            Reply::Data {
                credit,
                balance_msat,
                ..
            } => credit.map(|credit| (credit, *balance_msat)),
            Reply::Quote(quote) => quote.credit.map(|credit| (credit, quote.balance_msat)),
            _ => None,
        };
        let answer = match (credit, reply) {
            (Some((credit, balance_msat)), _) if credit.payment == pending.id => Reply::Paid {
                payment: credit.payment,
                credited_msat: credit.credited_msat,
                balance_msat,
            },
            (_, Reply::Error { payment, .. }) if *payment == Some(pending.id) => reply.clone(),
            _ => return Ok(false),
        };
        // A concurrent request may have settled the same token already.
        let current = self
            .state()
            .account
            .pending
            .as_ref()
            .map(|current| current.id);
        if current != Some(pending.id) {
            return Ok(false);
        }
        let sent = self.outcome(pending, answer).await?;
        let credited = matches!(sent, Sent::Credited);
        self.conclude(pending.clone(), sent)?;
        Ok(credited)
    }

    /// Record what became of the pending token.
    fn conclude(&self, pending: PendingPayment, sent: Sent) -> Result<(), StoreError> {
        let outcome = {
            let mut state = self.state();
            let has_room = state.account.unresolved.len() < MAX_UNRESOLVED;
            let set_aside = |state: &mut crate::buyer::State, pending: PendingPayment| {
                state.account.pending = None;
                state.account.unresolved.push(pending);
            };
            match sent {
                Sent::Credited => {
                    state.account.pending = None;
                    Ok(())
                }
                Sent::Reclaimed(refusal) => {
                    state.account.pending = None;
                    Err(refusal)
                }
                Sent::Refused(refusal) => {
                    // Neither credited nor back in the wallet. A provider
                    // that refuses a token may still redeem it.
                    if has_room {
                        set_aside(&mut state, pending);
                    }
                    Err(refusal)
                }
                Sent::NotYet(detail) => {
                    let attempts = pending.attempts.saturating_add(1);
                    if attempts >= MAX_ATTEMPTS && has_room {
                        // Stop holding up new payments; this one is tried
                        // again later and still counts as money held.
                        set_aside(
                            &mut state,
                            PendingPayment {
                                attempts,
                                ..pending
                            },
                        );
                        Ok(())
                    } else {
                        if let Some(current) = &mut state.account.pending {
                            current.attempts = attempts;
                        }
                        Err(format!(
                            "paid route provider could not take the payment yet: {detail}"
                        ))
                    }
                }
            }
        };
        let _ = self.persist();
        outcome.map_err(StoreError::Other)
    }

    /// Try the oldest unresolved token once more. Failures leave it in place.
    async fn retry_unresolved(&self) {
        let Some(oldest) = self.state().account.unresolved.first().cloned() else {
            return;
        };
        let sent = self.send(&oldest).await;
        {
            let mut state = self.state();
            let unresolved = &mut state.account.unresolved;
            let Some(index) = unresolved
                .iter()
                .position(|payment| payment.id == oldest.id)
            else {
                return;
            };
            let payment = unresolved.remove(index);
            if !matches!(sent, Ok(Sent::Credited | Sent::Reclaimed(_))) {
                unresolved.push(payment);
            }
        }
        let _ = self.persist();
    }

    /// One `pay` exchange for one token, with its accounting.
    async fn send(&self, pending: &PendingPayment) -> Result<Sent, StoreError> {
        let request = Request::Pay {
            account: self.wire_account(),
            payment: pending.wire(),
        };
        let deadline = Instant::now() + self.policy.request_timeout;
        let reply = self.exchange(&request, deadline).await?;
        self.outcome(pending, reply).await
    }

    /// What one reply says about one token handed to the provider, with its
    /// accounting.
    async fn outcome(&self, pending: &PendingPayment, reply: Reply) -> Result<Sent, StoreError> {
        match reply {
            Reply::Paid {
                payment,
                credited_msat,
                balance_msat,
            } if payment == pending.id => {
                let mut state = self.state();
                // A provider may keep a little of a token for its mint's
                // redemption fee. Anything beyond the allowance is owed: it
                // is added here, missing from the reported balance, and so
                // counted as a loss.
                let paid_msat = pending.payment.amount_sat.saturating_mul(1_000);
                let shortfall_msat = paid_msat.saturating_sub(credited_msat);
                let fee_msat = shortfall_msat.min(self.policy.max_redeem_fee_msat);
                state.stats.redeem_fee_msat += fee_msat;
                state.account.credited_msat = state
                    .account
                    .credited_msat
                    .saturating_add(credited_msat)
                    .saturating_add(shortfall_msat - fee_msat);
                state.credits += 1;
                self.note_balance(&mut state, balance_msat, None);
                Ok(Sent::Credited)
            }
            Reply::Error {
                code: ErrorCode::Retry,
                detail,
                ..
            } => Ok(Sent::NotYet(detail)),
            Reply::Error { code, detail, .. } => {
                let refusal = format!(
                    "paid route provider refused the payment ({}): {detail}",
                    code.as_str()
                );
                let reclaimed = self.wallet.reclaim(&pending.payment).await.is_ok();
                let mut state = self.state();
                state.stats.rejected_payments += 1;
                if reclaimed {
                    state.stats.paid_sat = state
                        .stats
                        .paid_sat
                        .saturating_sub(pending.payment.amount_sat);
                    Ok(Sent::Reclaimed(refusal))
                } else {
                    Ok(Sent::Refused(refusal))
                }
            }
            _ => Err(StoreError::Other(
                "paid route provider sent an unexpected reply to a payment".into(),
            )),
        }
    }
}
