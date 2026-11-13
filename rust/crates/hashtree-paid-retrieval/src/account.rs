//! What a client knows about its account with one provider.

use std::time::Duration;

use crate::message::{AccountId, Payment, PaymentId, METHOD_CASHU};
use crate::price::{BytePrice, Tariff};
use crate::wallet::WalletPayment;

/// The client's rules for paying one provider.
#[derive(Clone, Debug)]
pub struct ClientPolicy {
    /// The highest price accepted; a dearer quote is declined.
    pub max_tariff: Tariff,
    /// Mints the client is willing to pay with. Empty leaves the choice among
    /// the provider's mints to the wallet.
    pub mints: Vec<String>,
    /// Preferred top-up, so that most reads need no mint round trip.
    pub topup_sat: u64,
    /// The most this provider may hold in advance.
    pub max_prepaid_msat: u64,
    /// Payments stop once the provider's balance is this much lower than
    /// hash-verified deliveries explain.
    pub max_loss_msat: u64,
    /// The most a wallet may spend on creating one token, beyond its value.
    pub max_token_fee_sat: u64,
    /// How much less than a token's value a provider may credit, to cover
    /// what its mint charges for redeeming. A larger shortfall is a loss.
    pub max_redeem_fee_msat: u64,
    /// Deadline of a read without a router context, and of every payment.
    pub request_timeout: Duration,
}

impl Default for ClientPolicy {
    /// Placeholder caps: about 1 sat per MiB, 100 sat held by a provider.
    fn default() -> Self {
        Self {
            max_tariff: Tariff {
                rate: BytePrice::new(1, 1024),
                min_msat: 100,
            },
            mints: Vec::new(),
            topup_sat: 20,
            max_prepaid_msat: 100_000,
            max_loss_msat: 20_000,
            max_token_fee_sat: 2,
            max_redeem_fee_msat: 2_000,
            request_timeout: Duration::from_secs(10),
        }
    }
}

/// A token created by the wallet that the provider has not acknowledged yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingPayment {
    pub id: PaymentId,
    pub payment: WalletPayment,
    /// Times the provider answered that it cannot take the payment yet.
    pub attempts: u32,
}

impl PendingPayment {
    /// The payment as it travels to the provider.
    pub fn wire(&self) -> Payment {
        Payment {
            id: self.id,
            method: METHOD_CASHU.to_owned(),
            mint: self.payment.mint_url.clone(),
            token: self.payment.token.clone(),
        }
    }
}

/// A payment the wallet is about to be asked for. It is saved before the
/// wallet is called, so that a crash in between cannot orphan a token: on the
/// next start the wallet is asked for the payment with this id again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentIntent {
    pub id: PaymentId,
    pub amount_sat: u64,
    pub mints: Vec<String>,
}

/// Where a buyer keeps its account across restarts.
///
/// `save` is called before the wallet is asked for a payment, when a token
/// arrives, and whenever a payment is settled. It must be durable when it
/// returns. Charges for delivered blobs are not saved one by one; after a
/// restart the first balance the provider reports is taken as the baseline.
pub trait AccountStore: Send + Sync {
    fn save(&self, account: &AccountState) -> Result<(), String>;
}

/// The client's side of one account. Keep it in an [`AccountStore`] to carry
/// a prepaid balance and unsettled payments across restarts. It holds the
/// account's bearer id and unredeemed tokens: store it privately.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountState {
    pub account: AccountId,
    /// What the provider owes for payments: the credits it acknowledged,
    /// plus anything it kept of a token beyond `max_redeem_fee_msat`.
    pub credited_msat: u64,
    /// Charges for blobs whose bytes matched their hash.
    pub charged_msat: u64,
    /// The balance the provider last reported.
    pub balance_msat: i64,
    /// A payment the wallet was asked for and has not answered here yet.
    pub intent: Option<PaymentIntent>,
    /// A token the provider has not acknowledged yet.
    pub pending: Option<PendingPayment>,
    /// Tokens neither credited by the provider nor taken back by the wallet.
    /// They are never dropped: each may still be either. Their value counts
    /// against `max_prepaid_msat`.
    pub unresolved: Vec<PendingPayment>,
    /// Why this provider is no longer paid, if it is not.
    pub blocked: Option<String>,
}

impl AccountState {
    /// A fresh account with a random bearer id.
    pub fn new() -> Self {
        Self {
            account: rand::random(),
            credited_msat: 0,
            charged_msat: 0,
            balance_msat: 0,
            intent: None,
            pending: None,
            unresolved: Vec::new(),
            blocked: None,
        }
    }

    /// Msat the provider holds less than verified deliveries explain.
    pub fn loss_msat(&self) -> u64 {
        let expected = i128::from(self.credited_msat) - i128::from(self.charged_msat);
        let loss = expected - i128::from(self.balance_msat);
        u64::try_from(loss.max(0)).unwrap_or(u64::MAX)
    }
}

impl AccountState {
    /// Value of the tokens whose fate is not known.
    pub fn unresolved_msat(&self) -> u64 {
        self.unresolved
            .iter()
            .map(|payment| payment.payment.amount_sat.saturating_mul(1_000))
            .fold(0, u64::saturating_add)
    }

    /// Whether anything was ever paid or charged on this account.
    pub(crate) fn has_history(&self) -> bool {
        self.credited_msat != 0 || self.charged_msat != 0 || self.balance_msat != 0
    }
}

impl Default for AccountState {
    fn default() -> Self {
        Self::new()
    }
}

/// Counters for a wallet screen and for diagnosing declined reads.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PaidRouteStats {
    pub blobs: u64,
    pub bytes: u64,
    /// Sats handed to the provider, less reclaimed tokens.
    pub paid_sat: u64,
    pub fee_sat: u64,
    /// Msat providers kept of tokens within `max_redeem_fee_msat`.
    pub redeem_fee_msat: u64,
    /// Charges made before a restart and not saved, written off when the
    /// provider first reported its balance.
    pub resynced_msat: u64,
    /// Reads declined because the quote was above `max_tariff`.
    pub declined_price: u64,
    /// Reads declined by the wallet limit, its balance or `max_prepaid_msat`.
    pub declined_limit: u64,
    /// Reads declined because no mint or payment method was acceptable.
    pub declined_mint: u64,
    /// Reads not attempted because the provider is blocked.
    pub declined_blocked: u64,
    /// Payments the provider refused.
    pub rejected_payments: u64,
    /// Replies whose bytes did not match the requested hash.
    pub invalid_blobs: u64,
}
