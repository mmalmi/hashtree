//! The provider's account ledger: pure, synchronous and bounded.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use hashtree_core::Hash;

use crate::message::{AccountId, PaymentId};

const MAX_PAYMENTS_PER_ACCOUNT: usize = 32;
const MAX_RECENT_PER_ACCOUNT: usize = 64;
const MAX_REMEMBERED_TOKENS: usize = 16_384;

pub(crate) type TokenHash = [u8; 32];

#[derive(Clone, Copy, Debug)]
pub(crate) struct LedgerConfig {
    pub grace_msat: u64,
    pub max_accounts: usize,
    pub redelivery_window: Duration,
    pub max_redeliveries: u8,
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
    hash: Hash,
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
    Redeem,
}

pub(crate) struct Ledger {
    config: LedgerConfig,
    accounts: HashMap<AccountId, Account>,
    tokens: HashSet<TokenHash>,
    token_order: VecDeque<TokenHash>,
    redeeming: HashSet<TokenHash>,
    grace_peers: HashMap<String, AccountId>,
}

impl Ledger {
    pub(crate) fn new(config: LedgerConfig) -> Self {
        Self {
            config,
            accounts: HashMap::new(),
            tokens: HashSet::new(),
            token_order: VecDeque::new(),
            redeeming: HashSet::new(),
            grace_peers: HashMap::new(),
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
        }
        self.accounts.insert(*account, Account::new(now));
        true
    }

    pub(crate) fn charge(
        &mut self,
        account: &AccountId,
        peer: Option<&str>,
        hash: &Hash,
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
        if balance_msat < 0 {
            if let Some(peer) = peer {
                self.grace_peers.insert(peer.to_owned(), *account);
            }
        }
        Charge::Done {
            charged_msat: price_msat,
            balance_msat,
            duplicate: false,
        }
    }

    pub(crate) fn begin_payment(
        &mut self,
        account: &AccountId,
        payment: &PaymentId,
        token: &TokenHash,
    ) -> PaymentStart {
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
        let has_room = self.accounts.contains_key(account)
            || self.accounts.len() < self.config.max_accounts
            || self.accounts.values().any(|entry| entry.balance() <= 0);
        if !has_room {
            return PaymentStart::Full;
        }
        self.redeeming.insert(*token);
        PaymentStart::Redeem
    }

    /// Record the outcome of a redemption; `credited_msat` is `None` when the
    /// token was not redeemed. Returns the new balance.
    pub(crate) fn finish_payment(
        &mut self,
        account: &AccountId,
        payment: &PaymentId,
        token: &TokenHash,
        credited_msat: Option<u64>,
        now: Instant,
    ) -> i64 {
        self.redeeming.remove(token);
        let Some(credited_msat) = credited_msat else {
            return self.balance(account);
        };
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
}
