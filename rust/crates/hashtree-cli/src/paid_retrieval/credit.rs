//! Operator-authorized peer credit. The journal owner covers retrieval and the
//! private FIPS exchange. Public HTTP clients cannot start a credit purchase.
use super::*;
use fips_credit_link::{CreditLink, LinkPolicy, ServiceHandler, ServiceRequest};
use hashtree_fips_transport::PeerIdentity;
use hashtree_paid_retrieval::credit::{CreditAccount, CreditBlobProvider, CreditBlobRoute};
use peer_payments::{Account, AccountStore, Book, Buyer, Keypair, Limits, SpendingLimits};
use peer_payments::{ControlMessage, Controller};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CreditConfig {
    /// Shared peer-to-peer conversion and wallet spending policy.
    pub exchange: Option<LinkPolicy>,
    /// Optional Core Lightning socket for inspecting local invoices and auto-cashout.
    pub receive_rpc: Option<PathBuf>,
    /// Automatically create local wallet invoices within these explicit limits.
    pub auto_cashout: Option<fips_credit_link::AutoCashout>,
    /// Existing account, or the explicit create destination for `credit init`.
    pub journal: Option<PathBuf>,
    pub providers: Vec<String>,
    pub max_sat_per_blob: Option<u64>,
    pub sell_sat: Option<u64>,
    pub paying_only: bool,
    /// Operator-owned JSON LinkPolicy. Defaults to no forwarding or wallet payouts.
    pub exchange_policy_file: Option<PathBuf>,
}

impl CreditConfig {
    fn path(&self, data_dir: &Path) -> PathBuf {
        self.journal
            .clone()
            .unwrap_or_else(|| data_dir.join(PAID_RETRIEVAL_DIR).join("credit.sqlite"))
    }

    fn policy(&self) -> Result<LinkPolicy> {
        match (&self.exchange, &self.exchange_policy_file) {
            (Some(policy), None) => Ok(policy.clone()),
            (None, Some(path)) => Ok(serde_json::from_reader(File::open(path)?)?),
            (None, None) => Ok(LinkPolicy::default()),
            (Some(_), Some(_)) => anyhow::bail!("Configure exchange or exchange_policy_file once"),
        }
    }

    pub fn balance(
        &self,
        data_dir: &Path,
        keys: &nostr::Keys,
    ) -> Result<peer_payments::AccountBalance> {
        Ok(AccountStore::read_balance(
            self.path(data_dir),
            &keys.public_key().to_hex(),
        )?)
    }

    pub fn open(&self, data_dir: &Path, keys: &nostr::Keys) -> Result<Arc<Mutex<AccountStore>>> {
        let store = AccountStore::open(self.path(data_dir), &keys.public_key().to_hex()).context(
            "Open peer credit (initialize once with htree paid credit init; stop other owners)",
        )?;
        Ok(Arc::new(Mutex::new(store)))
    }

    pub fn initialize(
        &self,
        data_dir: &Path,
        keys: &nostr::Keys,
        accepted: &[String],
        per_peer: u64,
        total: u64,
        lifetime: u64,
        trial: Option<peer_payments::TrialPolicy>,
    ) -> Result<()> {
        let seller = Limits {
            max_peer_exposure_sat: per_peer,
            max_total_exposure_sat: total,
            ..Limits::default()
        };
        let buyer = SpendingLimits {
            max_reserved_sat: lifetime,
            max_lifetime_sat: lifetime,
            ..SpendingLimits::default()
        };
        let peers = accepted
            .iter()
            .map(|s| PeerIdentity::from_npub(s).map(|peer| peer.pubkey().to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        let owner = keys.public_key().to_hex();
        let mut book = Account::new(
            Book::new(&owner, seller, peers)?,
            Buyer::new(&owner, buyer)?,
        )?;
        book.set_trial_policy(trial)?;
        let path = self.path(data_dir);
        if self.journal.is_none() {
            let directory = data_dir.join(PAID_RETRIEVAL_DIR);
            fs::create_dir_all(&directory)?;
            super::keep_private(&directory)?;
        }
        AccountStore::create(path, book)?;
        Ok(())
    }
}

pub fn signing_key(keys: &nostr::Keys) -> Result<Keypair> {
    Ok(Keypair::from_seckey_slice(
        secp256k1::SECP256K1,
        keys.secret_key().as_secret_bytes(),
    )?)
}

pub async fn bind_exchange(
    cfg: &CreditConfig,
    fips: &DaemonFipsHandle,
    keys: &nostr::Keys,
    store: Arc<Mutex<AccountStore>>,
) -> Result<Arc<CreditLink>> {
    let key = signing_key(keys)?;
    let handler = Arc::new(Netting {
        journal: store.clone(),
        key,
    });
    CreditLink::bind(
        fips.endpoint.clone(),
        store,
        key,
        cfg.policy()?,
        Some(handler),
    )
    .await
    .map_err(anyhow::Error::msg)
}

struct Netting {
    journal: Arc<Mutex<AccountStore>>,
    key: Keypair,
}

impl ServiceHandler for Netting {
    fn handle(&self, peer: &str, request: ServiceRequest) -> Result<ControlMessage, String> {
        let ServiceRequest::Control(ControlMessage::NettingProposal(proposal)) = request else {
            return Err("Use the hash-bound blob service for retrieval".into());
        };
        if proposal.payload.left != peer
            || proposal.payload.right != self.key.x_only_public_key().0.to_string()
        {
            return Err("Unexpected netting identities".into());
        }
        let mut db = self.journal.try_lock().map_err(|_| "Credit account busy")?;
        let response = Controller::new(&mut db, &self.key)
            .and_then(|mut c| c.accept_netting(&proposal))
            .map_err(|e| e.to_string())?;
        Ok(ControlMessage::NettingAcknowledgement(response))
    }
}

impl Netting {
    async fn settle(&self, link: &CreditLink) {
        let peers = {
            let Ok(db) = self.journal.try_lock() else {
                return;
            };
            let claims = db
                .book()
                .available_claims()
                .map(|(a, _)| &a.payer)
                .collect::<std::collections::BTreeSet<_>>();
            db.book()
                .available_debts()
                .filter(|(a, _)| claims.contains(&a.provider))
                .map(|(a, _)| a.provider.clone())
                .chain(db.book().pending_nettings().map(|n| n.right.clone()))
                .collect::<std::collections::BTreeSet<_>>()
        };
        for peer in peers {
            let Some(proposal) = self.journal.try_lock().ok().and_then(|mut db| {
                Controller::new(&mut db, &self.key)
                    .ok()?
                    .propose_netting(&peer)
                    .ok()?
            }) else {
                continue;
            };
            let request = ServiceRequest::Control(ControlMessage::NettingProposal(proposal));
            if let Ok(ControlMessage::NettingAcknowledgement(ack)) =
                link.service_call(&peer, request).await
            {
                if let Ok(mut db) = self.journal.try_lock() {
                    let _ = Controller::new(&mut db, &self.key)
                        .and_then(|mut c| c.finish_netting(&ack));
                }
            }
        }
    }
}

pub(super) struct RunningCredit {
    pub(super) router: Option<Arc<BlobRouter>>,
    service: tokio::sync::Mutex<Option<FipsPaidBlobService>>,
    _exchange: Arc<CreditLink>,
    settlement_task: tokio::task::JoinHandle<()>,
    auto_cashout_task: Option<tokio::task::JoinHandle<()>>,
}

impl RunningCredit {
    pub(super) async fn start(
        cfg: &CreditConfig,
        data_dir: &Path,
        bind: &str,
        fips: &DaemonFipsHandle,
        keys: &nostr::Keys,
        store: &Arc<HashtreeStore>,
        fallback: Option<Arc<BlobRouter>>,
    ) -> Result<Self> {
        let journal = cfg.open(data_dir, keys)?;
        let account = CreditAccount::new(journal.clone(), signing_key(keys)?)?;
        let netting = Netting {
            journal: journal.clone(),
            key: signing_key(keys)?,
        };
        let exchange = bind_exchange(cfg, fips, keys, journal).await?;
        let local: Arc<dyn Store> = store.store_arc();
        let seller = cfg
            .sell_sat
            .map(|price| CreditBlobProvider::new(local.clone(), account.clone(), price))
            .transpose()?;
        let service = if seller.is_some() || !cfg.providers.is_empty() {
            Some(
                FipsPaidBlobService::credit(
                    fips.endpoint.clone(),
                    seller,
                    FipsPaidConfig::default(),
                )
                .await?,
            )
        } else {
            None
        };
        let mut entries = Vec::new();
        let mut routes = Vec::new();
        if !cfg.providers.is_empty() {
            anyhow::ensure!(
                super::binds_loopback(bind),
                "Credit buying requires a loopback read listener"
            );
            let max = cfg
                .max_sat_per_blob
                .context("Set max_sat_per_blob for credit buying")?;
            let carrier = service
                .as_ref()
                .context("Missing peer-credit blob carrier")?;
            for npub in &cfg.providers {
                let peer = PeerIdentity::from_npub(npub)?;
                let route = CreditBlobRoute::new(
                    account.clone(),
                    peer.pubkey().to_string(),
                    Arc::new(carrier.transport_to(peer)),
                    max,
                    local.clone(),
                )?;
                let route = Arc::new(route);
                entries.push(BlobRouteEntry::paid(
                    format!("credit:{npub}"),
                    route.clone(),
                ));
                routes.push(route);
            }
            if let Some(cashu) = fallback {
                entries.push(BlobRouteEntry::paid("cashu", cashu));
            }
        }
        let router = if entries.is_empty() {
            None
        } else {
            Some(Arc::new(BlobRouter::new(
                entries,
                Some(local),
                BlobRouterConfig::default(),
            )?))
        };
        let auto_cashout_task = match cfg.auto_cashout.clone() {
            Some(policy) => {
                Some(start_automatic_cashout(&exchange, cfg.receive_rpc.clone(), policy).await?)
            }
            None => None,
        };
        let netting_link = exchange.clone();
        let settlement_task = tokio::spawn(async move {
            // Stagger ordinary settlement from the cash-out worker’s first tick.
            // Both use the same single-writer account.
            let start = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
            let mut interval = tokio::time::interval_at(start, std::time::Duration::from_secs(30));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                netting.settle(&netting_link).await;
                for route in &routes {
                    let _ = route.reconcile_pending().await;
                }
            }
        });
        Ok(Self {
            router,
            service: tokio::sync::Mutex::new(service),
            _exchange: exchange,
            auto_cashout_task,
            settlement_task,
        })
    }

    pub(super) async fn shutdown(&self) {
        self.settlement_task.abort();
        if let Some(task) = &self.auto_cashout_task {
            task.abort();
        }
        if let Some(service) = self.service.lock().await.take() {
            if let Err(err) = service.shutdown().await {
                tracing::warn!("Peer credit service shutdown: {err}");
            }
        }
    }
}

impl Drop for RunningCredit {
    fn drop(&mut self) {
        self.settlement_task.abort();
        if let Some(task) = &self.auto_cashout_task {
            task.abort();
        }
    }
}

/// Connected local wallet metadata. Receiving and automatic payouts use a Unix socket.
pub struct ReceiveWallet {
    pub rpc_socket: PathBuf,
    pub network: String,
}

#[cfg(unix)]
pub async fn detect_receive_wallet(path: Option<PathBuf>) -> Result<ReceiveWallet> {
    let wallet = tokio::task::spawn_blocking(move || {
        fips_credit_link::wallet::detect_wallet(path.as_deref())
    })
    .await?
    .map_err(anyhow::Error::msg)?;
    Ok(ReceiveWallet {
        rpc_socket: wallet.rpc_socket,
        network: wallet.network,
    })
}

#[cfg(not(unix))]
pub async fn detect_receive_wallet(_path: Option<PathBuf>) -> Result<ReceiveWallet> {
    anyhow::bail!("Local Core Lightning wallet connections require a Unix socket")
}

#[cfg(unix)]
async fn start_automatic_cashout(
    exchange: &Arc<CreditLink>,
    path: Option<PathBuf>,
    policy: fips_credit_link::AutoCashout,
) -> Result<tokio::task::JoinHandle<()>> {
    let wallet = detect_receive_wallet(path).await?;
    exchange
        .start_auto_cashout(wallet.rpc_socket, policy)
        .map_err(anyhow::Error::msg)
}

#[cfg(not(unix))]
async fn start_automatic_cashout(
    _exchange: &Arc<CreditLink>,
    _path: Option<PathBuf>,
    _policy: fips_credit_link::AutoCashout,
) -> Result<tokio::task::JoinHandle<()>> {
    anyhow::bail!("Automatic wallet payouts require a local Unix Core Lightning socket")
}
