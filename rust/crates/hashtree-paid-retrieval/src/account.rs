//! What a client knows about its account with one provider.

use std::time::Duration;

use crate::message::{AccountId, PaymentId};
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
            request_timeout: Duration::from_secs(10),
        }
    }
}

/// A token created by the wallet that the provider has not acknowledged yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingPayment {
    pub id: PaymentId,
    pub payment: WalletPayment,
}

/// The client's side of one account. Persist it to keep a prepaid balance and
/// an unacknowledged payment across restarts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountState {
    pub account: AccountId,
    /// Credits the provider acknowledged.
    pub credited_msat: u64,
    /// Charges for blobs whose bytes matched their hash.
    pub charged_msat: u64,
    /// The balance the provider last reported.
    pub balance_msat: i64,
    pub pending: Option<PendingPayment>,
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
            pending: None,
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
