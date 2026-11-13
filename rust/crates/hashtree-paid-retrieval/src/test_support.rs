//! A fake mint, wallet and receiver for tests of this crate and of the
//! applications that wire it in. No network, no real funds.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::{
    PaymentId, PaymentReceiver, PaymentRequest, ReceiveError, RetrievalWallet, WalletError,
    WalletPayment,
};

pub const MINT: &str = "https://mint.test";

struct Issued {
    mint: String,
    amount_sat: u64,
    spent: bool,
}

/// Issues bearer tokens and lets each be redeemed exactly once.
#[derive(Default)]
pub struct FakeMint {
    tokens: Mutex<HashMap<String, Issued>>,
}

impl FakeMint {
    pub fn issue(&self, mint: &str, amount_sat: u64) -> String {
        let mut tokens = self.tokens.lock().unwrap();
        let token = format!("cashuFake{}", tokens.len());
        let issued = Issued {
            mint: mint.to_owned(),
            amount_sat,
            spent: false,
        };
        tokens.insert(token.clone(), issued);
        token
    }

    pub fn redeem(&self, mint: &str, token: &str) -> Result<u64, String> {
        let mut tokens = self.tokens.lock().unwrap();
        match tokens.get_mut(token) {
            None => Err("unknown token".into()),
            Some(issued) if issued.mint != mint => Err("token belongs to another mint".into()),
            Some(issued) if issued.spent => Err("token already spent".into()),
            Some(issued) => {
                issued.spent = true;
                Ok(issued.amount_sat)
            }
        }
    }
}

/// A funded client wallet on one mint.
pub struct FakeWallet {
    pub mint: Arc<FakeMint>,
    pub mint_url: String,
    pub balance_sat: Mutex<u64>,
    pub payments: AtomicUsize,
    /// Pay this many sats less than asked, to exercise underpayment.
    pub short_sat: u64,
    /// What creating a token costs on top of its value.
    pub fee_sat: u64,
    /// Create the token, then fail as if the process died before answering.
    pub lose_reply: AtomicBool,
    pub cannot_reclaim: AtomicBool,
    /// Tokens by payment id: asked again, the wallet returns the same one.
    pub made: Mutex<HashMap<PaymentId, WalletPayment>>,
}

impl FakeWallet {
    pub fn funded(mint: &Arc<FakeMint>, balance_sat: u64) -> Arc<Self> {
        Arc::new(Self {
            mint: Arc::clone(mint),
            mint_url: MINT.to_owned(),
            balance_sat: Mutex::new(balance_sat),
            payments: AtomicUsize::new(0),
            short_sat: 0,
            fee_sat: 0,
            lose_reply: AtomicBool::new(false),
            cannot_reclaim: AtomicBool::new(false),
            made: Mutex::new(HashMap::new()),
        })
    }

    pub fn balance(&self) -> u64 {
        *self.balance_sat.lock().unwrap()
    }

    pub fn payments(&self) -> usize {
        self.payments.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl RetrievalWallet for FakeWallet {
    async fn pay(&self, request: &PaymentRequest) -> Result<WalletPayment, WalletError> {
        if !request.mints.contains(&self.mint_url) {
            return Err(WalletError::NoAcceptedMint);
        }
        if let Some(made) = self.made.lock().unwrap().get(&request.payment_id) {
            return Ok(made.clone());
        }
        if self.fee_sat > request.max_fee_sat {
            return Err(WalletError::Failed("the fee is above the ceiling".into()));
        }
        let amount_sat = request.amount_sat.saturating_sub(self.short_sat).max(1);
        let cost_sat = amount_sat + self.fee_sat;
        let mut balance = self.balance_sat.lock().unwrap();
        if *balance < cost_sat {
            return Err(WalletError::InsufficientFunds);
        }
        *balance -= cost_sat;
        self.payments.fetch_add(1, Ordering::SeqCst);
        let token = self.mint.issue(&self.mint_url, amount_sat);
        let payment = WalletPayment {
            mint_url: self.mint_url.clone(),
            token: token.clone(),
            amount_sat,
            fee_sat: self.fee_sat,
            reclaim_id: Some(token),
        };
        let mut made = self.made.lock().unwrap();
        made.insert(request.payment_id, payment.clone());
        if self.lose_reply.swap(false, Ordering::SeqCst) {
            return Err(WalletError::Failed("the process died".into()));
        }
        Ok(payment)
    }

    async fn recover(&self, payment_id: &PaymentId) -> Result<Option<WalletPayment>, WalletError> {
        Ok(self.made.lock().unwrap().get(payment_id).cloned())
    }

    async fn balance_sat(&self) -> Result<u64, WalletError> {
        Ok(self.balance())
    }

    async fn reclaim(&self, payment: &WalletPayment) -> Result<(), WalletError> {
        if self.cannot_reclaim.load(Ordering::SeqCst) {
            return Err(WalletError::Failed("the mint is unreachable".into()));
        }
        let amount_sat = self
            .mint
            .redeem(&payment.mint_url, &payment.token)
            .map_err(WalletError::Failed)?;
        *self.balance_sat.lock().unwrap() += amount_sat;
        Ok(())
    }
}

/// The provider's wallet: redeems at the fake mint.
pub struct FakeReceiver {
    pub mint: Arc<FakeMint>,
    pub received_sat: Mutex<u64>,
    pub offline: AtomicBool,
    /// When set, a redemption waits here until the test lets it through.
    pub gate: Option<Arc<tokio::sync::Semaphore>>,
    /// The wallet's own record of the tokens it redeemed. `None` is a wallet
    /// that cannot say what it received earlier.
    pub records: Option<Mutex<HashMap<String, u64>>>,
    pub redemptions: AtomicUsize,
    /// Sats the mint keeps of every redeemed token.
    pub mint_fee_sat: u64,
}

impl FakeReceiver {
    pub fn new(mint: &Arc<FakeMint>) -> Arc<Self> {
        Arc::new(Self {
            mint: Arc::clone(mint),
            received_sat: Mutex::new(0),
            offline: AtomicBool::new(false),
            gate: None,
            records: Some(Mutex::default()),
            redemptions: AtomicUsize::new(0),
            mint_fee_sat: 0,
        })
    }

    pub fn without_records(mint: &Arc<FakeMint>) -> Arc<Self> {
        let mut receiver = Arc::into_inner(Self::new(mint)).unwrap();
        receiver.records = None;
        Arc::new(receiver)
    }

    pub fn received(&self) -> u64 {
        *self.received_sat.lock().unwrap()
    }

    /// Calls that reached the mint.
    pub fn redemptions(&self) -> usize {
        self.redemptions.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl PaymentReceiver for FakeReceiver {
    async fn receive(&self, mint_url: &str, token: &str) -> Result<u64, ReceiveError> {
        if let Some(gate) = &self.gate {
            gate.acquire().await.unwrap().forget();
        }
        if self.offline.load(Ordering::SeqCst) {
            return Err(ReceiveError::Unavailable("mint unreachable".into()));
        }
        self.redemptions.fetch_add(1, Ordering::SeqCst);
        let amount_sat = self
            .mint
            .redeem(mint_url, token)
            .map_err(ReceiveError::Rejected)?;
        let amount_sat = amount_sat.saturating_sub(self.mint_fee_sat);
        *self.received_sat.lock().unwrap() += amount_sat;
        if let Some(records) = &self.records {
            let mut records = records.lock().unwrap();
            records.insert(token.to_owned(), amount_sat);
        }
        Ok(amount_sat)
    }

    async fn already_received(
        &self,
        _mint_url: &str,
        token: &str,
    ) -> Result<Option<u64>, ReceiveError> {
        let records = self.records.as_ref();
        Ok(records.and_then(|records| records.lock().unwrap().get(token).copied()))
    }
}
