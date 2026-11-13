//! A fake mint, wallet and receiver. No network, no real funds.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use hashtree_core::{sha256, Hash, MemoryStore, Store, StoreBlobRoute, StoreError};
use hashtree_paid_retrieval::{
    BytePrice, LedgerEntry, LedgerState, LedgerStore, PaidBlobProvider, PaidBlobTransport,
    PaymentId, PaymentReceiver, PaymentRequest, ProviderConfig, ReceiveError, Reply, Request,
    RetrievalWallet, Tariff, WalletError, WalletPayment,
};

/// The payment a request carries: in `pay`, or riding with `get`.
pub fn payment_of(request: &Request) -> Option<&hashtree_paid_retrieval::Payment> {
    match request {
        Request::Pay { payment, .. } => Some(payment),
        Request::Get { payment, .. } => payment.as_ref(),
    }
}

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
        Self::build(mint, receiver, None, configure)
    }

    /// A seller whose ledger is kept in `ledger`. The blobs it sells are
    /// not: add them again after a restart.
    pub fn durable(
        mint: &Arc<FakeMint>,
        receiver: &Arc<FakeReceiver>,
        ledger: Arc<dyn LedgerStore>,
        configure: impl FnOnce(&mut ProviderConfig),
    ) -> Self {
        Self::build(mint.clone(), receiver.clone(), Some(ledger), configure)
    }

    fn build(
        mint: Arc<FakeMint>,
        receiver: Arc<FakeReceiver>,
        ledger: Option<Arc<dyn LedgerStore>>,
        configure: impl FnOnce(&mut ProviderConfig),
    ) -> Self {
        let store = Arc::new(MemoryStore::new());
        let mut config = ProviderConfig::new(TARIFF, vec![MINT.to_owned()]);
        configure(&mut config);
        let source = Arc::new(StoreBlobRoute::new(store.clone()));
        let provider = match ledger {
            Some(ledger) => {
                PaidBlobProvider::open(source, receiver.clone(), config, ledger).unwrap()
            }
            None => PaidBlobProvider::new(source, receiver.clone(), config),
        };
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

type Trigger = Box<dyn Fn(&LedgerEntry) -> bool + Send + Sync>;

/// A ledger store under a process that dies at a chosen write. From then on
/// nothing reaches the disk, as after a crash.
pub struct CrashStore {
    disk: Arc<dyn LedgerStore>,
    dead: AtomicBool,
    /// The entry that kills the process, and whether it is written first.
    trigger: Mutex<Option<(Trigger, bool)>>,
}

impl CrashStore {
    pub fn new(disk: Arc<dyn LedgerStore>) -> Arc<Self> {
        Arc::new(Self {
            disk,
            dead: AtomicBool::new(false),
            trigger: Mutex::new(None),
        })
    }

    /// Die when asked to write a matching entry; it does not reach the disk.
    pub fn crash_before(&self, entry: impl Fn(&LedgerEntry) -> bool + Send + Sync + 'static) {
        *self.trigger.lock().unwrap() = Some((Box::new(entry), false));
    }

    /// Die right after a matching entry is on disk.
    pub fn crash_after(&self, entry: impl Fn(&LedgerEntry) -> bool + Send + Sync + 'static) {
        *self.trigger.lock().unwrap() = Some((Box::new(entry), true));
    }

    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::SeqCst)
    }
}

impl LedgerStore for CrashStore {
    fn load(&self) -> std::io::Result<LedgerState> {
        self.disk.load()
    }

    fn append(&self, entries: &[LedgerEntry]) -> std::io::Result<()> {
        let died = || std::io::Error::other("the process died");
        if self.is_dead() {
            return Err(died());
        }
        let trigger = self.trigger.lock().unwrap();
        let written_first = trigger
            .as_ref()
            .filter(|(matches, _)| entries.iter().any(matches))
            .map(|(_, written_first)| *written_first);
        match written_first {
            None => self.disk.append(entries),
            Some(true) => {
                self.disk.append(entries)?;
                self.dead.store(true, Ordering::SeqCst);
                Ok(())
            }
            Some(false) => {
                self.dead.store(true, Ordering::SeqCst);
                Err(died())
            }
        }
    }
}

/// A ledger store whose disk refuses credits for a while, as when it is full.
pub struct FailingStore {
    disk: Arc<dyn LedgerStore>,
    pub fail_credits: AtomicBool,
}

impl FailingStore {
    pub fn new(disk: Arc<dyn LedgerStore>) -> Self {
        Self {
            disk,
            fail_credits: AtomicBool::new(false),
        }
    }
}

impl LedgerStore for FailingStore {
    fn load(&self) -> std::io::Result<LedgerState> {
        self.disk.load()
    }

    fn append(&self, entries: &[LedgerEntry]) -> std::io::Result<()> {
        let credit = |entry: &LedgerEntry| matches!(entry, LedgerEntry::Credited { .. });
        if self.fail_credits.load(Ordering::SeqCst) && entries.iter().any(credit) {
            return Err(std::io::Error::other("no space left on device"));
        }
        self.disk.append(entries)
    }
}
