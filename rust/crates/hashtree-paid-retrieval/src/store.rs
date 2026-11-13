//! The durable form of the provider ledger: a log of entries and the state
//! they add up to.
//!
//! A [`LedgerStore`] only has to keep the entries it was given, in order. The
//! meaning of an entry is fixed here, in [`LedgerState::apply`], so every
//! store restores the same ledger.

use std::collections::{HashMap, VecDeque};
use std::io;

use crate::message::{AccountId, PaymentId};

pub(crate) const MAX_PAYMENTS_PER_ACCOUNT: usize = 32;
pub(crate) const MAX_REMEMBERED_TOKENS: usize = 16_384;
pub(crate) const MAX_PENDING_REDEMPTIONS: usize = 4_096;

/// SHA-256 of a payment token as the client sent it.
pub type TokenHash = [u8; 32];

/// One account with the payments that were credited to it, oldest first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AccountRecord {
    pub paid_msat: u64,
    pub charged_msat: u64,
    pub payments: VecDeque<(PaymentId, u64)>,
}

/// A token whose redemption was started and has no recorded outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingRedemption {
    pub token: TokenHash,
    pub account: AccountId,
    pub payment: PaymentId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LedgerEntry {
    /// The provider is about to redeem this token. Written before the mint is
    /// asked, so a crash cannot leave a redeemed token without a trace.
    Redeeming(PendingRedemption),
    /// The token turned out to be worthless; forget the redemption.
    Abandoned { token: TokenHash },
    /// The token was redeemed: credit the account, remember the payment id
    /// and the token, and close the redemption. One entry, so all or none.
    Credited {
        redemption: PendingRedemption,
        credited_msat: u64,
    },
    /// The account's total charges so far.
    Charged {
        account: AccountId,
        charged_msat: u64,
    },
    /// The account was dropped to make room.
    Removed { account: AccountId },
    /// Grace of this authenticated peer is bound to this account.
    Grace { peer: String, account: AccountId },
    /// A whole account, as a store writes it when it rewrites its log.
    Account {
        account: AccountId,
        record: AccountRecord,
    },
    /// A credited token, as a store writes it when it rewrites its log.
    Token { token: TokenHash },
}

/// Everything a provider must remember across a restart.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LedgerState {
    pub accounts: HashMap<AccountId, AccountRecord>,
    /// Credited tokens, oldest first.
    pub tokens: VecDeque<TokenHash>,
    /// Redemptions without an outcome, oldest first.
    pub redeeming: VecDeque<PendingRedemption>,
    pub grace: HashMap<String, AccountId>,
}

impl LedgerState {
    pub fn apply(&mut self, entry: &LedgerEntry) {
        match entry {
            LedgerEntry::Redeeming(redemption) => self.begin(*redemption),
            LedgerEntry::Abandoned { token } => self.close(token),
            LedgerEntry::Credited {
                redemption,
                credited_msat,
            } => {
                self.close(&redemption.token);
                self.remember(redemption.token);
                let record = self.accounts.entry(redemption.account).or_default();
                record.paid_msat = record.paid_msat.saturating_add(*credited_msat);
                record
                    .payments
                    .push_back((redemption.payment, *credited_msat));
                if record.payments.len() > MAX_PAYMENTS_PER_ACCOUNT {
                    record.payments.pop_front();
                }
            }
            LedgerEntry::Charged {
                account,
                charged_msat,
            } => self.accounts.entry(*account).or_default().charged_msat = *charged_msat,
            LedgerEntry::Removed { account } => {
                self.accounts.remove(account);
            }
            LedgerEntry::Grace { peer, account } => {
                self.grace.insert(peer.clone(), *account);
            }
            LedgerEntry::Account { account, record } => {
                self.accounts.insert(*account, record.clone());
            }
            LedgerEntry::Token { token } => self.remember(*token),
        }
    }

    fn begin(&mut self, redemption: PendingRedemption) {
        self.close(&redemption.token);
        self.redeeming.push_back(redemption);
        if self.redeeming.len() > MAX_PENDING_REDEMPTIONS {
            self.redeeming.pop_front();
        }
    }

    fn close(&mut self, token: &TokenHash) {
        self.redeeming.retain(|pending| pending.token != *token);
    }

    fn remember(&mut self, token: TokenHash) {
        self.tokens.push_back(token);
        if self.tokens.len() > MAX_REMEMBERED_TOKENS {
            self.tokens.pop_front();
        }
    }

    /// The shortest log that restores this state.
    pub fn entries(&self) -> Vec<LedgerEntry> {
        let accounts = self
            .accounts
            .iter()
            .map(|(account, record)| LedgerEntry::Account {
                account: *account,
                record: record.clone(),
            });
        let tokens = self
            .tokens
            .iter()
            .map(|token| LedgerEntry::Token { token: *token });
        let redeeming = self.redeeming.iter().copied().map(LedgerEntry::Redeeming);
        let grace = self.grace.iter().map(|(peer, account)| LedgerEntry::Grace {
            peer: peer.clone(),
            account: *account,
        });
        accounts
            .chain(tokens)
            .chain(redeeming)
            .chain(grace)
            .collect()
    }
}

/// Keeps the provider ledger across restarts.
///
/// Calls block on disk, so the seller makes them off the async executor. One
/// store serves one seller at a time.
pub trait LedgerStore: Send + Sync {
    /// The state all appended entries add up to.
    fn load(&self) -> io::Result<LedgerState>;

    /// Make `entries` durable before returning, in this order and all or
    /// none. After an error none of them may reappear.
    fn append(&self, entries: &[LedgerEntry]) -> io::Result<()>;
}
