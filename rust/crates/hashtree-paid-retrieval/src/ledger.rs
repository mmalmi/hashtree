//! The provider's account ledger: pure, synchronous and bounded.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use crate::message::{AccountId, ContentHash, PaymentId};
use crate::store::{
    AccountRecord, LedgerEntry, LedgerState, PendingRedemption, TokenHash,
    MAX_PAYMENTS_PER_ACCOUNT, MAX_PENDING_REDEMPTIONS, MAX_REMEMBERED_TOKENS,
};

const MAX_RECENT_PER_ACCOUNT: usize = 64;

#[derive(Clone, Copy, Debug)]
pub(crate) struct LedgerConfig {
    pub grace_msat: u64,
    pub max_accounts: usize,
    pub redelivery_window: Duration,
    pub max_redeliveries: u8,
    /// Whether a store records this ledger; without one nothing is queued
    /// for writing.
    pub durable: bool,
}

/// One account as a provider persists it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccountBalance {
    pub account: AccountId,
    /// Everything credited from redeemed payments.
    pub paid_msat: u64,
    /// Everything charged for delivered blobs.
    pub charged_msat: u64,
}

impl AccountBalance {
    pub fn balance_msat(&self) -> i64 {
        balance(self.paid_msat, self.charged_msat)
    }
}

fn balance(paid_msat: u64, charged_msat: u64) -> i64 {
    let difference = i128::from(paid_msat) - i128::from(charged_msat);
    difference.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

struct Recent {
    hash: ContentHash,
    price_msat: u64,
    at: Instant,
    left: u8,
}

struct Account {
    paid_msat: u64,
    charged_msat: u64,
    payments: VecDeque<(PaymentId, u64)>,
    recent: VecDeque<Recent>,
    last_used: Instant,
}

impl Account {
    fn new(now: Instant) -> Self {
        Self {
            paid_msat: 0,
            charged_msat: 0,
            payments: VecDeque::new(),
            recent: VecDeque::new(),
            last_used: now,
        }
    }

    fn balance(&self) -> i64 {
        balance(self.paid_msat, self.charged_msat)
    }
}

pub(crate) enum Charge {
    Done {
        charged_msat: u64,
        balance_msat: i64,
        duplicate: bool,
    },
    /// Not authorised or not covered: answer with a quote.
    Refused {
        balance_msat: i64,
        grace_msat: u64,
    },
    Full,
}

pub(crate) enum PaymentStart {
    /// This payment id was already credited.
    Known {
        credited_msat: u64,
        balance_msat: i64,
    },
    /// The token was credited under another payment.
    Replay,
    /// The token is being redeemed right now.
    Busy,
    Full,
    Settle(Settlement),
}

/// What is left to do before a payment is credited.
#[derive(Clone, Copy)]
pub(crate) enum Settlement {
    /// Redeem the token. `recover` means an earlier redemption of it has no
    /// recorded outcome, so the wallet may already hold its value.
    Redeem { recover: bool },
    /// The token was redeemed earlier in this run but the credit could not be
    /// written; only the write is left to do.
    Record { credited_msat: u64 },
}

/// Changes that are written to the store a moment later, in one batch.
#[derive(Default)]
pub(crate) struct Lazy {
    charged: HashSet<AccountId>,
    removed: Vec<AccountId>,
    bound: Vec<(String, AccountId)>,
    abandoned: Vec<TokenHash>,
}

impl Lazy {
    pub(crate) fn is_empty(&self) -> bool {
        self.charged.is_empty()
            && self.removed.is_empty()
            && self.bound.is_empty()
            && self.abandoned.is_empty()
    }
}

pub(crate) struct Ledger {
    config: LedgerConfig,
    accounts: HashMap<AccountId, Account>,
    tokens: HashSet<TokenHash>,
    token_order: VecDeque<TokenHash>,
    redeeming: HashSet<TokenHash>,
    /// Redemptions without a recorded outcome, oldest first.
    pending: VecDeque<PendingRedemption>,
    /// Redeemed tokens whose credit could not be written yet, in msat.
    unrecorded: HashMap<TokenHash, u64>,
    grace_peers: HashMap<String, AccountId>,
    lazy: Option<Lazy>,
}

impl Ledger {
    pub(crate) fn new(config: LedgerConfig) -> Self {
        Self {
            config,
            accounts: HashMap::new(),
            tokens: HashSet::new(),
            token_order: VecDeque::new(),
            redeeming: HashSet::new(),
            pending: VecDeque::new(),
            unrecorded: HashMap::new(),
            grace_peers: HashMap::new(),
            lazy: config.durable.then(Lazy::default),
        }
    }

    pub(crate) fn balance(&self, account: &AccountId) -> i64 {
        self.accounts.get(account).map_or(0, Account::balance)
    }

    /// Grace is granted to one account per authenticated peer.
    fn grace_for(&self, account: &AccountId, peer: Option<&str>) -> u64 {
        let Some(peer) = peer else { return 0 };
        match self.grace_peers.get(peer) {
            Some(bound) if bound != account => 0,
            Some(_) => self.config.grace_msat,
            None if self.grace_peers.len() < self.config.max_accounts => self.config.grace_msat,
            None => 0,
        }
    }

    /// Make room for `account`, never dropping one that holds customer funds.
    fn ensure_account(&mut self, account: &AccountId, now: Instant) -> bool {
        if self.accounts.contains_key(account) {
            return true;
        }
        if self.accounts.len() >= self.config.max_accounts {
            let evict = self
                .accounts
                .iter()
                .filter(|(_, entry)| entry.balance() <= 0)
                .min_by_key(|(_, entry)| (entry.balance() < 0, entry.last_used))
                .map(|(id, _)| *id);
            let Some(evict) = evict else { return false };
            self.accounts.remove(&evict);
            if let Some(lazy) = &mut self.lazy {
                lazy.charged.remove(&evict);
                lazy.removed.push(evict);
            }
        }
        self.accounts.insert(*account, Account::new(now));
        true
    }

    pub(crate) fn charge(
        &mut self,
        account: &AccountId,
        peer: Option<&str>,
        hash: &ContentHash,
        price_msat: u64,
        max_msat: u64,
        now: Instant,
    ) -> Charge {
        let window = self.config.redelivery_window;
        if let Some(entry) = self.accounts.get_mut(account) {
            entry.last_used = now;
            let repeat = entry.recent.iter_mut().find(|recent| {
                recent.hash == *hash && recent.left > 0 && now.duration_since(recent.at) <= window
            });
            if let Some(recent) = repeat {
                recent.left -= 1;
                let charged_msat = recent.price_msat;
                return Charge::Done {
                    charged_msat,
                    balance_msat: entry.balance(),
                    duplicate: true,
                };
            }
        }
        let balance_msat = self.balance(account);
        if price_msat == 0 {
            return Charge::Done {
                charged_msat: 0,
                balance_msat,
                duplicate: false,
            };
        }
        let grace_msat = self.grace_for(account, peer);
        let covered = i128::from(balance_msat) + i128::from(grace_msat) >= i128::from(price_msat);
        if price_msat > max_msat || !covered {
            return Charge::Refused {
                balance_msat,
                grace_msat,
            };
        }
        if !self.ensure_account(account, now) {
            return Charge::Full;
        }
        let max_redeliveries = self.config.max_redeliveries;
        let entry = self.accounts.get_mut(account).expect("account was ensured");
        entry.charged_msat = entry.charged_msat.saturating_add(price_msat);
        entry.recent.push_back(Recent {
            hash: *hash,
            price_msat,
            at: now,
            left: max_redeliveries,
        });
        if entry.recent.len() > MAX_RECENT_PER_ACCOUNT {
            entry.recent.pop_front();
        }
        let balance_msat = entry.balance();
        if let Some(lazy) = &mut self.lazy {
            lazy.charged.insert(*account);
        }
        if balance_msat < 0 {
            if let Some(peer) = peer {
                let bound = self.grace_peers.insert(peer.to_owned(), *account);
                if let Some(lazy) = self.lazy.as_mut().filter(|_| bound != Some(*account)) {
                    lazy.bound.push((peer.to_owned(), *account));
                }
            }
        }
        Charge::Done {
            charged_msat: price_msat,
            balance_msat,
            duplicate: false,
        }
    }

    pub(crate) fn begin_payment(&mut self, redemption: &PendingRedemption) -> PaymentStart {
        let PendingRedemption {
            token,
            account,
            payment,
        } = redemption;
        if let Some(entry) = self.accounts.get(account) {
            if let Some((_, credited_msat)) = entry.payments.iter().find(|(id, _)| id == payment) {
                return PaymentStart::Known {
                    credited_msat: *credited_msat,
                    balance_msat: entry.balance(),
                };
            }
        }
        if self.tokens.contains(token) {
            return PaymentStart::Replay;
        }
        if self.redeeming.contains(token) {
            return PaymentStart::Busy;
        }
        // A token with an open redemption stays with the payment that
        // started it, whoever presents it.
        let open = self.pending.iter().find(|pending| pending.token == *token);
        if open.is_some_and(|pending| pending != redemption) {
            return PaymentStart::Replay;
        }
        let recover = open.is_some();
        let has_room = self.accounts.contains_key(account)
            || self.accounts.len() < self.config.max_accounts
            || self.accounts.values().any(|entry| entry.balance() <= 0);
        if !has_room && !recover {
            return PaymentStart::Full;
        }
        self.redeeming.insert(*token);
        PaymentStart::Settle(match self.unrecorded.get(token) {
            Some(credited_msat) => Settlement::Record {
                credited_msat: *credited_msat,
            },
            None => Settlement::Redeem { recover },
        })
    }

    /// The redemption is on record and the mint may now be asked.
    pub(crate) fn redemption_recorded(&mut self, redemption: PendingRedemption) {
        self.pending.push_back(redemption);
        if self.pending.len() > MAX_PENDING_REDEMPTIONS {
            self.pending.pop_front();
        }
    }

    fn close_redemption(&mut self, token: &TokenHash) {
        self.redeeming.remove(token);
        self.unrecorded.remove(token);
        self.pending.retain(|pending| pending.token != *token);
    }

    /// The token was redeemed and the credit is on record. Returns the new
    /// balance.
    pub(crate) fn credit(
        &mut self,
        redemption: &PendingRedemption,
        credited_msat: u64,
        now: Instant,
    ) -> i64 {
        let PendingRedemption {
            token,
            account,
            payment,
        } = redemption;
        self.close_redemption(token);
        if self.tokens.insert(*token) {
            self.token_order.push_back(*token);
            if self.token_order.len() > MAX_REMEMBERED_TOKENS {
                if let Some(oldest) = self.token_order.pop_front() {
                    self.tokens.remove(&oldest);
                }
            }
        }
        // Redeemed money is always credited, even past the account bound.
        if !self.ensure_account(account, now) {
            self.accounts.insert(*account, Account::new(now));
        }
        let entry = self.accounts.get_mut(account).expect("account was ensured");
        entry.last_used = now;
        entry.paid_msat = entry.paid_msat.saturating_add(credited_msat);
        entry.payments.push_back((*payment, credited_msat));
        if entry.payments.len() > MAX_PAYMENTS_PER_ACCOUNT {
            entry.payments.pop_front();
        }
        entry.balance()
    }

    /// The token is worthless: forget its redemption.
    pub(crate) fn abandon(&mut self, token: &TokenHash) {
        let open = self.pending.iter().any(|pending| pending.token == *token);
        if let Some(lazy) = self.lazy.as_mut().filter(|_| open) {
            lazy.abandoned.push(*token);
        }
        self.close_redemption(token);
    }

    /// The attempt ended without an outcome; the redemption stays open.
    pub(crate) fn unresolved(&mut self, token: &TokenHash) {
        self.redeeming.remove(token);
    }

    /// The token was redeemed for `credited_msat` but the credit is not on
    /// record yet.
    pub(crate) fn redeemed_unrecorded(&mut self, token: &TokenHash, credited_msat: u64) {
        self.redeeming.remove(token);
        self.unrecorded.insert(*token, credited_msat);
    }

    pub(crate) fn balances(&self) -> Vec<AccountBalance> {
        self.accounts
            .iter()
            .map(|(account, entry)| AccountBalance {
                account: *account,
                paid_msat: entry.paid_msat,
                charged_msat: entry.charged_msat,
            })
            .collect()
    }

    pub(crate) fn restore(&mut self, balances: &[AccountBalance], now: Instant) {
        for saved in balances {
            let entry = self
                .accounts
                .entry(saved.account)
                .or_insert_with(|| Account::new(now));
            entry.paid_msat = saved.paid_msat;
            entry.charged_msat = saved.charged_msat;
        }
    }

    /// Load what a store kept across a restart.
    pub(crate) fn restore_state(&mut self, state: LedgerState, now: Instant) {
        for (account, record) in state.accounts {
            let AccountRecord {
                paid_msat,
                charged_msat,
                payments,
            } = record;
            let entry = Account {
                paid_msat,
                charged_msat,
                payments,
                ..Account::new(now)
            };
            self.accounts.insert(account, entry);
        }
        self.tokens = state.tokens.iter().copied().collect();
        self.token_order = state.tokens;
        self.pending = state.redeeming;
        self.grace_peers = state.grace;
    }

    pub(crate) fn has_lazy(&self) -> bool {
        self.lazy.as_ref().is_some_and(|lazy| !lazy.is_empty())
    }

    /// Take the changes not written yet, with the entries that record them.
    pub(crate) fn take_lazy(&mut self) -> (Lazy, Vec<LedgerEntry>) {
        let lazy = self.lazy.replace(Lazy::default()).unwrap_or_default();
        // An account dropped and opened again is removed first, then charged.
        let removed = lazy
            .removed
            .iter()
            .map(|account| LedgerEntry::Removed { account: *account });
        let charged = lazy.charged.iter().filter_map(|account| {
            let entry = self.accounts.get(account)?;
            Some(LedgerEntry::Charged {
                account: *account,
                charged_msat: entry.charged_msat,
            })
        });
        let bound = lazy.bound.iter().map(|(peer, account)| LedgerEntry::Grace {
            peer: peer.clone(),
            account: *account,
        });
        let abandoned = lazy
            .abandoned
            .iter()
            .map(|token| LedgerEntry::Abandoned { token: *token });
        let entries = removed
            .chain(charged)
            .chain(bound)
            .chain(abandoned)
            .collect();
        (lazy, entries)
    }

    /// Put back changes whose write failed, ahead of any made since.
    pub(crate) fn return_lazy(&mut self, mut earlier: Lazy) {
        let later = self.lazy.take().unwrap_or_default();
        earlier.charged.extend(later.charged);
        earlier.removed.extend(later.removed);
        earlier.bound.extend(later.bound);
        earlier.abandoned.extend(later.abandoned);
        self.lazy = Some(earlier);
    }
}
