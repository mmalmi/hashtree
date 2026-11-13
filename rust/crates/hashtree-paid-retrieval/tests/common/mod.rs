//! A fake mint, wallet and receiver. No network, no real funds.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use hashtree_core::{sha256, Hash, MemoryStore, Store, StoreBlobRoute, StoreError};
use hashtree_paid_retrieval::{
    BytePrice, PaidBlobProvider, PaidBlobTransport, PaymentReceiver, PaymentRequest,
    ProviderConfig, ReceiveError, Reply, Request, RetrievalWallet, Tariff, WalletError,
    WalletPayment,
};

pub const MINT: &str = "https://mint.test";

/// 1 msat per KiB, at least 1000 msat per blob.
pub const TARIFF: Tariff = Tariff {
    rate: BytePrice::new(1, 1024),
    min_msat: 1_000,
};

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
}

impl FakeWallet {
    pub fn funded(mint: &Arc<FakeMint>, balance_sat: u64) -> Arc<Self> {
        Arc::new(Self {
            mint: Arc::clone(mint),
            mint_url: MINT.to_owned(),
            balance_sat: Mutex::new(balance_sat),
            payments: AtomicUsize::new(0),
            short_sat: 0,
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
        let amount_sat = request.amount_sat.saturating_sub(self.short_sat).max(1);
        let mut balance = self.balance_sat.lock().unwrap();
        if *balance < amount_sat {
            return Err(WalletError::InsufficientFunds);
        }
        *balance -= amount_sat;
        self.payments.fetch_add(1, Ordering::SeqCst);
        let token = self.mint.issue(&self.mint_url, amount_sat);
        Ok(WalletPayment {
            mint_url: self.mint_url.clone(),
            token: token.clone(),
            amount_sat,
            fee_sat: 0,
            reclaim_id: Some(token),
        })
    }

    async fn balance_sat(&self) -> Result<u64, WalletError> {
        Ok(self.balance())
    }

    async fn reclaim(&self, payment: &WalletPayment) -> Result<(), WalletError> {
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
}

impl FakeReceiver {
    pub fn new(mint: &Arc<FakeMint>) -> Arc<Self> {
        Arc::new(Self {
            mint: Arc::clone(mint),
            received_sat: Mutex::new(0),
            offline: AtomicBool::new(false),
            gate: None,
        })
    }

    pub fn received(&self) -> u64 {
        *self.received_sat.lock().unwrap()
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
        let amount_sat = self
            .mint
            .redeem(mint_url, token)
            .map_err(ReceiveError::Rejected)?;
        *self.received_sat.lock().unwrap() += amount_sat;
        Ok(amount_sat)
    }
}

pub fn blob(seed: u8, len: usize) -> (Hash, Vec<u8>) {
    let data: Vec<u8> = (0..len).map(|i| seed.wrapping_add(i as u8)).collect();
    (sha256(&data), data)
}

pub struct Seller {
    pub mint: Arc<FakeMint>,
    pub receiver: Arc<FakeReceiver>,
    pub store: Arc<MemoryStore>,
    pub provider: PaidBlobProvider,
}

impl Seller {
    pub fn new() -> Self {
        Self::with(|_| {})
    }

    pub fn with(configure: impl FnOnce(&mut ProviderConfig)) -> Self {
        let mint = Arc::new(FakeMint::default());
        Self::on_mint(mint.clone(), FakeReceiver::new(&mint), configure)
    }

    pub fn on_mint(
        mint: Arc<FakeMint>,
        receiver: Arc<FakeReceiver>,
        configure: impl FnOnce(&mut ProviderConfig),
    ) -> Self {
        let store = Arc::new(MemoryStore::new());
        let mut config = ProviderConfig::new(TARIFF, vec![MINT.to_owned()]);
        configure(&mut config);
        let provider = PaidBlobProvider::new(
            Arc::new(StoreBlobRoute::new(store.clone())),
            receiver.clone(),
            config,
        );
        Self {
            mint,
            receiver,
            store,
            provider,
        }
    }

    pub async fn add(&self, seed: u8, len: usize) -> (Hash, Vec<u8>) {
        let (hash, data) = blob(seed, len);
        self.store.put(hash, data.clone()).await.unwrap();
        (hash, data)
    }

    pub async fn ask(&self, request: Request) -> Reply {
        self.ask_as(None, request).await
    }

    pub async fn ask_as(&self, peer: Option<&str>, request: Request) -> Reply {
        Reply::decode(&self.provider.handle(peer, &request.encode()).await).unwrap()
    }
}

type Tamper = Box<dyn FnMut(&Request, Reply) -> Option<Reply> + Send>;

/// Hands requests straight to a provider. `tamper` may replace a reply or
/// lose it (`None`), after the provider has already acted on the request.
pub struct DirectTransport {
    pub provider: PaidBlobProvider,
    pub peer: Option<String>,
    pub exchanges: AtomicUsize,
    pub tamper: Mutex<Tamper>,
}

impl DirectTransport {
    pub fn new(provider: &PaidBlobProvider) -> Arc<Self> {
        Self::tampering(provider, |_, reply| Some(reply))
    }

    pub fn tampering(
        provider: &PaidBlobProvider,
        tamper: impl FnMut(&Request, Reply) -> Option<Reply> + Send + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            provider: provider.clone(),
            peer: None,
            exchanges: AtomicUsize::new(0),
            tamper: Mutex::new(Box::new(tamper)),
        })
    }

    pub fn exchanges(&self) -> usize {
        self.exchanges.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl PaidBlobTransport for DirectTransport {
    async fn exchange(&self, request: Vec<u8>) -> Result<Vec<u8>, StoreError> {
        self.exchanges.fetch_add(1, Ordering::SeqCst);
        let reply = self.provider.handle(self.peer.as_deref(), &request).await;
        let request = Request::decode(&request).unwrap();
        let reply = Reply::decode(&reply).unwrap();
        let mut tamper = self.tamper.lock().unwrap();
        match (*tamper)(&request, reply) {
            Some(reply) => Ok(reply.encode()),
            None => Err(StoreError::Other("connection lost".into())),
        }
    }
}
