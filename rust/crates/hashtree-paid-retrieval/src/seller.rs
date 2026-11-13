//! The selling side of the exchange: prices, account balances and payments.
//!
//! Nothing here knows where the sold bytes come from or how they travel, so a
//! Hashtree blob provider and a BitTorrent seeder can share it as it is.

use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use hashtree_core::sha256;

use crate::books::Books;
use crate::ledger::{AccountBalance, Charge, Ledger, LedgerConfig, PaymentStart, Settlement};
use crate::message::{
    AccountId, ContentHash, ErrorCode, LockKey, Payment, Quote, Reply, METHOD_CASHU, PROTOCOL,
};
use crate::price::Tariff;
use crate::store::{LedgerEntry, LedgerStore, PendingRedemption};

#[derive(Clone, Debug)]
pub struct ProviderConfig {
    pub tariff: Tariff,
    /// Mints whose tokens are accepted. Tokens of any other mint are refused
    /// before they are redeemed.
    pub mints: Vec<String>,
    /// Smallest top-up worth a mint round trip, in sat.
    pub min_topup_sat: u64,
    /// Msat served before payment to an authenticated peer. Zero means prepay.
    pub grace_msat: u64,
    /// The key tokens should be locked to, named in every quote. Unlocked
    /// tokens are accepted as well.
    pub lock: Option<LockKey>,
    pub max_accounts: usize,
    /// How long a charged unit is served again without a new charge.
    pub redelivery_window: Duration,
}

impl ProviderConfig {
    pub fn new(tariff: Tariff, mints: Vec<String>) -> Self {
        Self {
            tariff,
            mints,
            min_topup_sat: 1,
            grace_msat: 0,
            lock: None,
            max_accounts: 10_000,
            redelivery_window: Duration::from_secs(60),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ReceiveError {
    /// The token is worthless to this provider: spent, malformed, wrong mint.
    #[error("token rejected: {0}")]
    Rejected(String),
    /// The token could not be checked now and may still be good.
    #[error("cannot redeem now: {0}")]
    Unavailable(String),
}

/// The provider's wallet.
#[async_trait]
pub trait PaymentReceiver: Send + Sync {
    /// Redeem `token` at `mint_url` and return the sats actually received.
    /// A token that belongs to any other mint must be rejected.
    async fn receive(&self, mint_url: &str, token: &str) -> Result<u64, ReceiveError>;

    /// The sats this wallet received for `token` in an earlier
    /// [`Self::receive`] whose result never reached the ledger, or `None`
    /// when it has no record of receiving it.
    ///
    /// Asked only when the mint refuses a token whose redemption was started
    /// and left without an outcome, by a crash or a lost mint reply. A wallet
    /// that cannot answer leaves such a token uncredited.
    async fn already_received(
        &self,
        _mint_url: &str,
        _token: &str,
    ) -> Result<Option<u64>, ReceiveError> {
        Ok(None)
    }
}

/// What a seller decided about one unit a buyer asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sale {
    /// The account was charged (or the unit is a free repeat): send the unit.
    Charged {
        charged_msat: u64,
        balance_msat: i64,
        duplicate: bool,
    },
    /// Not authorised or not covered: send this quote instead of the unit.
    Quote(Quote),
    /// No room for another account.
    Full,
}

struct Inner {
    receiver: Arc<dyn PaymentReceiver>,
    config: ProviderConfig,
    books: Arc<Books>,
}

impl Inner {
    /// Ask the mint, and for a token with an earlier open redemption the
    /// wallet as well.
    async fn redeem(&self, mint: &str, token: &str, recover: bool) -> Result<u64, ReceiveError> {
        let detail = match self.receiver.receive(mint, token).await {
            Ok(0) => "token has no value".to_owned(),
            Err(ReceiveError::Rejected(detail)) => detail,
            other => return other,
        };
        if recover {
            if let Some(sat) = self.receiver.already_received(mint, token).await? {
                if sat > 0 {
                    return Ok(sat);
                }
            }
        }
        Err(ReceiveError::Rejected(detail))
    }

    /// Redeem one token and record its credit. Returns the msat credited
    /// and the new balance.
    async fn settle(
        &self,
        redemption: PendingRedemption,
        mint: &str,
        token: &str,
        settlement: Settlement,
    ) -> Result<(u64, i64), ReceiveError> {
        let books = &self.books;
        let unwritten = |what: &str| ReceiveError::Unavailable(format!("{what} was not recorded"));
        let credited_msat = match settlement {
            Settlement::Record { credited_msat } => credited_msat,
            Settlement::Redeem { recover } => {
                if !recover {
                    // On record before the mint is asked: from here on a
                    // crash cannot leave a redeemed token without a trace.
                    let entry = LedgerEntry::Redeeming(redemption);
                    let recorded = books
                        .write_off_executor(vec![entry], move |ledger| {
                            ledger.redemption_recorded(redemption)
                        })
                        .await;
                    if recorded.is_err() {
                        books.ledger().unresolved(&redemption.token);
                        return Err(unwritten("the redemption"));
                    }
                }
                match self.redeem(mint, token, recover).await {
                    Ok(sat) => sat.saturating_mul(1_000),
                    Err(ReceiveError::Rejected(detail)) => {
                        books.ledger().abandon(&redemption.token);
                        books.schedule_flush();
                        return Err(ReceiveError::Rejected(detail));
                    }
                    Err(unavailable) => {
                        books.ledger().unresolved(&redemption.token);
                        return Err(unavailable);
                    }
                }
            }
        };
        // On record before the client is told: `paid` is never sent for a
        // credit that a restart would forget.
        let entry = LedgerEntry::Credited {
            redemption,
            credited_msat,
        };
        let credited = books
            .write_off_executor(vec![entry], move |ledger| {
                ledger.credit(&redemption, credited_msat, Instant::now())
            })
            .await;
        match credited {
            Ok(balance_msat) => Ok((credited_msat, balance_msat)),
            Err(_) => {
                books
                    .ledger()
                    .redeemed_unrecorded(&redemption.token, credited_msat);
                Err(unwritten("the credit"))
            }
        }
    }
}

/// Accounts, pricing and payment handling for one seller. Cheap to clone.
#[derive(Clone)]
pub struct PaidSeller {
    inner: Arc<Inner>,
}

/// The account of a client that its carrier authenticates as `peer`.
pub fn account_for_peer(peer: &str) -> AccountId {
    let mut tagged = format!("{PROTOCOL} peer").into_bytes();
    tagged.push(0);
    tagged.extend_from_slice(peer.as_bytes());
    sha256(&tagged)
}

pub(crate) fn normalize_mint(mint_url: &str) -> &str {
    mint_url.trim().trim_end_matches('/')
}

impl PaidSeller {
    /// A seller whose accounts live in memory only.
    pub fn new(receiver: Arc<dyn PaymentReceiver>, config: ProviderConfig) -> Self {
        Self::with_books(receiver, config, None)
    }

    /// A seller whose accounts, credited payments and tokens are kept in
    /// `store` and restored from it.
    pub fn open(
        receiver: Arc<dyn PaymentReceiver>,
        config: ProviderConfig,
        store: Arc<dyn LedgerStore>,
    ) -> io::Result<Self> {
        let state = store.load()?;
        let seller = Self::with_books(receiver, config, Some(store));
        seller
            .inner
            .books
            .ledger()
            .restore_state(state, Instant::now());
        Ok(seller)
    }

    fn with_books(
        receiver: Arc<dyn PaymentReceiver>,
        config: ProviderConfig,
        store: Option<Arc<dyn LedgerStore>>,
    ) -> Self {
        let ledger = Ledger::new(LedgerConfig {
            grace_msat: config.grace_msat,
            max_accounts: config.max_accounts.max(1),
            redelivery_window: config.redelivery_window,
            max_redeliveries: 2,
            durable: store.is_some(),
        });
        Self {
            inner: Arc::new(Inner {
                receiver,
                config,
                books: Arc::new(Books::new(ledger, store)),
            }),
        }
    }

    pub fn config(&self) -> &ProviderConfig {
        &self.inner.config
    }

    /// Every account as it stands now.
    pub fn accounts(&self) -> Vec<AccountBalance> {
        self.inner.books.ledger().balances()
    }

    /// Load accounts into a seller that has no store.
    pub fn restore_accounts(&self, accounts: &[AccountBalance]) {
        self.inner.books.ledger().restore(accounts, Instant::now());
    }

    /// Write charges that are not in the store yet. They are written within
    /// a second on their own; call this before shutdown. Blocks on disk.
    pub fn flush(&self) -> io::Result<()> {
        self.inner.books.flush()
    }

    /// Charge `account` for one unit of `size` bytes named by `content`, or
    /// say what it would take. Call it before sending the unit and send only
    /// on [`Sale::Charged`]. `peer` is the transport-authenticated identity of
    /// the buyer, if there is one; grace is granted only to such a peer.
    pub fn charge(
        &self,
        account: &AccountId,
        peer: Option<&str>,
        content: ContentHash,
        size: u64,
        max_msat: u64,
    ) -> Sale {
        let config = &self.inner.config;
        let price_msat = config.tariff.price_msat(size);
        let charge = self.inner.books.ledger().charge(
            account,
            peer,
            &content,
            price_msat,
            max_msat,
            Instant::now(),
        );
        self.inner.books.schedule_flush();
        match charge {
            Charge::Done {
                charged_msat,
                balance_msat,
                duplicate,
            } => Sale::Charged {
                charged_msat,
                balance_msat,
                duplicate,
            },
            Charge::Refused {
                balance_msat,
                grace_msat,
            } => Sale::Quote(Quote {
                hash: content,
                size,
                price_msat,
                balance_msat,
                grace_msat,
                tariff: config.tariff,
                mints: config.mints.clone(),
                methods: vec![METHOD_CASHU.to_owned()],
                min_topup_sat: config.min_topup_sat,
                lock: config.lock,
                credit: None,
            }),
            Charge::Full => Sale::Full,
        }
    }

    /// Credit one payment and return the `paid` or `err` reply for it.
    pub async fn pay(&self, account: AccountId, payment: Payment) -> Reply {
        let Payment {
            id: payment,
            method,
            mint,
            token,
        } = payment;
        let refuse = |code, detail: &str| Reply::Error {
            code,
            payment: Some(payment),
            detail: detail.to_owned(),
        };
        if method != METHOD_CASHU {
            return refuse(ErrorCode::Method, "unsupported payment method");
        }
        let mint = normalize_mint(&mint).to_owned();
        let accepted = self.inner.config.mints.iter();
        if !accepted
            .map(|url| normalize_mint(url))
            .any(|url| url == mint)
        {
            return refuse(ErrorCode::Mint, "mint is not accepted");
        }
        let redemption = PendingRedemption {
            token: sha256(token.as_bytes()),
            account,
            payment,
        };
        let start = self.inner.books.ledger().begin_payment(&redemption);
        let settlement = match start {
            PaymentStart::Known {
                credited_msat,
                balance_msat,
            } => {
                return Reply::Paid {
                    payment,
                    credited_msat,
                    balance_msat,
                }
            }
            PaymentStart::Replay => return refuse(ErrorCode::Replay, "token was already credited"),
            PaymentStart::Busy => return refuse(ErrorCode::Retry, "token is being redeemed"),
            PaymentStart::Full => return refuse(ErrorCode::Full, "no room for another account"),
            PaymentStart::Settle(settlement) => settlement,
        };
        // Redeeming must finish even if this connection goes away: a redeemed
        // token that is not credited would be lost to the client.
        let inner = Arc::clone(&self.inner);
        let settled =
            tokio::spawn(async move { inner.settle(redemption, &mint, &token, settlement).await });
        match settled.await {
            Ok(Ok((credited_msat, balance_msat))) => Reply::Paid {
                payment,
                credited_msat,
                balance_msat,
            },
            Ok(Err(ReceiveError::Rejected(detail))) => refuse(ErrorCode::Invalid, &detail),
            Ok(Err(ReceiveError::Unavailable(detail))) => refuse(ErrorCode::Retry, &detail),
            Err(_) => refuse(ErrorCode::Retry, "redemption did not complete"),
        }
    }
}
