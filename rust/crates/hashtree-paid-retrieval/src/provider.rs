//! The selling side: serves blobs from a source route against account balances.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use hashtree_core::{sha256, BlobReply, BlobRequest, BlobRoute, Hash, BLOB_MAX_BYTES};

use crate::ledger::{AccountBalance, Charge, Ledger, LedgerConfig, PaymentStart};
use crate::message::{AccountId, ErrorCode, PaymentId, Quote, Reply, Request, METHOD_CASHU};
use crate::price::Tariff;

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
    pub max_accounts: usize,
    /// How long a charged blob is served again without a new charge.
    pub redelivery_window: Duration,
}

impl ProviderConfig {
    pub fn new(tariff: Tariff, mints: Vec<String>) -> Self {
        Self {
            tariff,
            mints,
            min_topup_sat: 1,
            grace_msat: 0,
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
}

struct Inner {
    source: Arc<dyn BlobRoute>,
    receiver: Arc<dyn PaymentReceiver>,
    config: ProviderConfig,
    ledger: Mutex<Ledger>,
}

impl Inner {
    fn ledger(&self) -> MutexGuard<'_, Ledger> {
        self.ledger
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

/// Answers paid-retrieval requests. Cheap to clone; one per served store.
///
/// `source` must return hash-verified data, as `StoreBlobRoute` does.
#[derive(Clone)]
pub struct PaidBlobProvider {
    inner: Arc<Inner>,
}

pub(crate) fn normalize_mint(mint_url: &str) -> &str {
    mint_url.trim().trim_end_matches('/')
}

impl PaidBlobProvider {
    pub fn new(
        source: Arc<dyn BlobRoute>,
        receiver: Arc<dyn PaymentReceiver>,
        config: ProviderConfig,
    ) -> Self {
        let ledger = Ledger::new(LedgerConfig {
            grace_msat: config.grace_msat,
            max_accounts: config.max_accounts.max(1),
            redelivery_window: config.redelivery_window,
            max_redeliveries: 2,
        });
        Self {
            inner: Arc::new(Inner {
                source,
                receiver,
                config,
                ledger: Mutex::new(ledger),
            }),
        }
    }

    /// Every account, for the owner to persist.
    pub fn accounts(&self) -> Vec<AccountBalance> {
        self.inner.ledger().balances()
    }

    /// Load persisted accounts before serving.
    pub fn restore_accounts(&self, accounts: &[AccountBalance]) {
        self.inner.ledger().restore(accounts, Instant::now());
    }

    /// Answer one encoded request with one encoded reply. `peer` is the
    /// transport-authenticated identity of the sender, if there is one.
    pub async fn handle(&self, peer: Option<&str>, request: &[u8]) -> Vec<u8> {
        let reply = match Request::decode(request) {
            Err(error) => Reply::error(ErrorCode::Bad, error.0),
            Ok(Request::Get {
                hash,
                account,
                max_msat,
            }) => self.get(peer, hash, account, max_msat).await,
            Ok(Request::Pay {
                account,
                payment,
                method,
                mint,
                token,
            }) => self.pay(account, payment, &method, &mint, token).await,
        };
        reply.encode()
    }

    async fn get(
        &self,
        peer: Option<&str>,
        hash: Hash,
        account: AccountId,
        max_msat: u64,
    ) -> Reply {
        let data = match self.inner.source.route(BlobRequest { hash, htl: 0 }).await {
            Ok(BlobReply::Data(data)) if data.len() <= BLOB_MAX_BYTES => data,
            Ok(_) => return Reply::NoResult { hash },
            Err(_) => return Reply::error(ErrorCode::Retry, "the blob source failed"),
        };
        let config = &self.inner.config;
        let price_msat = config.tariff.price_msat(data.len() as u64);
        let charge =
            self.inner
                .ledger()
                .charge(&account, peer, &hash, price_msat, max_msat, Instant::now());
        match charge {
            Charge::Done {
                charged_msat,
                balance_msat,
                duplicate,
            } => Reply::Data {
                hash,
                charged_msat,
                balance_msat,
                duplicate,
                data,
            },
            Charge::Refused {
                balance_msat,
                grace_msat,
            } => Reply::Quote(Quote {
                hash,
                size: data.len() as u64,
                price_msat,
                balance_msat,
                grace_msat,
                tariff: config.tariff,
                mints: config.mints.clone(),
                methods: vec![METHOD_CASHU.to_owned()],
                min_topup_sat: config.min_topup_sat,
            }),
            Charge::Full => Reply::error(ErrorCode::Full, "no room for another account"),
        }
    }

    async fn pay(
        &self,
        account: AccountId,
        payment: PaymentId,
        method: &str,
        mint: &str,
        token: String,
    ) -> Reply {
        let refuse = |code, detail: &str| Reply::Error {
            code,
            payment: Some(payment),
            detail: detail.to_owned(),
        };
        if method != METHOD_CASHU {
            return refuse(ErrorCode::Method, "unsupported payment method");
        }
        let mint = normalize_mint(mint).to_owned();
        let accepted = self.inner.config.mints.iter();
        if !accepted
            .map(|url| normalize_mint(url))
            .any(|url| url == mint)
        {
            return refuse(ErrorCode::Mint, "mint is not accepted");
        }
        let token_hash = sha256(token.as_bytes());
        let start = self
            .inner
            .ledger()
            .begin_payment(&account, &payment, &token_hash);
        match start {
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
            PaymentStart::Redeem => {}
        }
        // Redeeming must finish even if this connection goes away: a redeemed
        // token that is not credited would be lost to the client.
        let inner = Arc::clone(&self.inner);
        let redeem = tokio::spawn(async move {
            let received = match inner.receiver.receive(&mint, &token).await {
                Ok(0) => Err(ReceiveError::Rejected("token has no value".into())),
                other => other,
            };
            let credited_msat = received.as_ref().ok().map(|sat| sat.saturating_mul(1_000));
            let balance_msat = inner.ledger().finish_payment(
                &account,
                &payment,
                &token_hash,
                credited_msat,
                Instant::now(),
            );
            (received, balance_msat)
        });
        match redeem.await {
            Ok((Ok(sat), balance_msat)) => Reply::Paid {
                payment,
                credited_msat: sat.saturating_mul(1_000),
                balance_msat,
            },
            Ok((Err(ReceiveError::Rejected(detail)), _)) => refuse(ErrorCode::Invalid, &detail),
            Ok((Err(ReceiveError::Unavailable(detail)), _)) => refuse(ErrorCode::Retry, &detail),
            Err(_) => refuse(ErrorCode::Retry, "redemption did not complete"),
        }
    }
}
