//! A fake seller and its carriers, on the fake mint of
//! `hashtree_paid_retrieval::test_support`. No network, no real funds.
#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use hashtree_core::{sha256, Hash, MemoryStore, Store, StoreBlobRoute, StoreError};
#[allow(unused_imports)]
pub use hashtree_paid_retrieval::test_support::{FakeMint, FakeReceiver, FakeWallet, MINT};

/// The payment a request carries: in `pay`, or riding with `get`.
pub fn payment_of(request: &Request) -> Option<&hashtree_paid_retrieval::Payment> {
    match request {
        Request::Pay { payment, .. } => Some(payment),
        Request::Get { payment, .. } => payment.as_ref(),
    }
}
use hashtree_paid_retrieval::{
    BytePrice, LedgerEntry, LedgerState, LedgerStore, PaidBlobProvider, PaidBlobTransport,
    ProviderConfig, Reply, Request, Tariff,
};

/// 1 msat per KiB, at least 1000 msat per blob.
pub const TARIFF: Tariff = Tariff {
    rate: BytePrice::new(1, 1024),
    min_msat: 1_000,
};

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
