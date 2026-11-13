//! Paid retrieval in the daemon: sell blobs from the local store, and buy
//! blobs that no free route returned. Both are off unless `[paid_retrieval]`
//! is configured; see `docs/paid-retrieval.md`.
//!
//! Only this module is started with the daemon. One-shot commands that bring
//! up their own FIPS endpoint never open the ledger or pay anyone.

mod config;
mod http;
mod spend_store;
mod status;
#[cfg(test)]
mod tests;

use std::fs;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use tokio::sync::Semaphore;

use anyhow::{bail, Context, Result};
use cashu_service::CashuHelperClient;
use hashtree_core::{BlobRoute, StoreBlobRoute};
use hashtree_network::BlobRouteEntry;
use hashtree_paid_retrieval::{
    CashuReceiver, CashuWallet, FileAccountStore, FileLedgerStore, FipsPaidBlobService,
    FipsPaidConfig, LimitedWallet, PaidBlobProvider, PaidBlobRoute, PaidBlobTransport,
    PaymentReceiver, RetrievalWallet, SpendingLimit,
};

pub use config::{
    ClientSettings, PaidClientConfig, PaidProviderConfig, PaidProviderPeerConfig,
    PaidRetrievalConfig, PaidRetrievalSettings, PaidTariffConfig, ProviderPeer, ProviderPlace,
    ProviderSettings, PAID_RETRIEVAL_DIR,
};
pub use http::{paid_blob_endpoint, HttpPaidTransport, PAID_BLOB_HTTP_PATH};
pub use spend_store::FileSpendStore;
pub use status::format_paid_status;

use crate::config::Config;
use crate::fips_transport::{BlockingStoreRoute, DaemonBlobResolver, DaemonFipsHandle};
use crate::storage::HashtreeStore;

/// Same-host priority of the paid capability, as for the free blob service.
const SAME_HOST_PROVIDER_PRIORITY: i16 = 100;

/// One provider the daemon buys from.
struct ClientRoute {
    name: String,
    place: ProviderPlace,
    route: PaidBlobRoute,
}

struct SellingSide {
    provider: PaidBlobProvider,
    paying_only: bool,
    /// Also sold at `POST /paid-blob`, at most this many exchanges at once.
    http: Option<Arc<Semaphore>>,
}

/// Exchanges the HTTP endpoint runs at once, as many as the FIPS carrier
/// seats connections. A redemption holds one for a mint round trip.
const MAX_HTTP_EXCHANGES: usize = 16;

struct BuyingSide {
    limit: Arc<SpendingLimit>,
    routes: Vec<ClientRoute>,
    /// Whether a read through the HTTP listener may spend money.
    pays_for_http_reads: bool,
}

/// The daemon's running paid retrieval.
pub struct DaemonPaidRetrieval {
    service: tokio::sync::Mutex<Option<FipsPaidBlobService>>,
    selling: Option<SellingSide>,
    buying: Option<BuyingSide>,
}

/// What `POST /paid-blob` answers.
pub enum HttpExchange {
    /// The daemon does not sell over HTTP.
    NotSold,
    /// Every exchange slot is taken; try again shortly.
    Busy,
    /// One encoded reply, whatever its protocol outcome.
    Reply(Vec<u8>),
}

impl DaemonPaidRetrieval {
    /// Answer one encoded request that arrived over HTTP. The buyer there is
    /// not authenticated, so it is nobody's peer and gets no grace.
    pub async fn exchange_over_http(&self, request: &[u8]) -> HttpExchange {
        let Some(selling) = &self.selling else {
            return HttpExchange::NotSold;
        };
        let Some(slots) = &selling.http else {
            return HttpExchange::NotSold;
        };
        let Ok(_slot) = slots.try_acquire() else {
            return HttpExchange::Busy;
        };
        HttpExchange::Reply(selling.provider.handle(None, request).await)
    }

    /// Whether a read that arrives at the HTTP listener may spend money.
    pub fn pays_for_http_reads(&self) -> bool {
        let buying = self.buying.as_ref();
        buying.is_some_and(|buying| buying.pays_for_http_reads)
    }

    /// What `htree status` shows. It holds no token and no account id.
    pub fn status(&self) -> serde_json::Value {
        status::status(self)
    }

    /// Stop serving and write the charges that are not on disk yet. Call it
    /// before the FIPS endpoint is shut down.
    pub async fn shutdown(&self) {
        if let Some(service) = self.service.lock().await.take() {
            if let Err(error) = service.shutdown().await {
                tracing::warn!("failed to stop the paid blob service: {error}");
            }
        }
        if let Some(selling) = &self.selling {
            let provider = selling.provider.clone();
            match tokio::task::spawn_blocking(move || provider.flush()).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::warn!("failed to flush the paid ledger: {error}"),
                Err(error) => tracing::warn!("failed to flush the paid ledger: {error}"),
            }
        }
    }
}

/// Start what `[paid_retrieval]` asks for, with the `htree-cashu` helper as
/// the wallet. `Ok(None)` when nothing is configured: then no service is
/// bound, no file is opened and no route is registered.
pub async fn start_daemon_paid_retrieval(
    config: &Config,
    data_dir: &Path,
    bind_address: &str,
    fips: Option<&DaemonFipsHandle>,
    store: &Arc<HashtreeStore>,
) -> Result<Option<Arc<DaemonPaidRetrieval>>> {
    let settings = PaidRetrievalSettings::from_config(&config.paid_retrieval, data_dir)
        .context("Paid retrieval is configured but cannot start")?;
    let Some(settings) = settings else {
        return Ok(None);
    };
    let helper = CashuHelperClient::discover(data_dir)
        .context("Paid retrieval needs the htree-cashu wallet helper")?;
    let trusted_mints = settings.client.as_ref().map(|client| &client.policy.mints);
    let wallet = CashuWallet::new(helper.clone(), trusted_mints.cloned().unwrap_or_default());
    let started = start_paid_retrieval(
        settings,
        bind_address,
        fips,
        store,
        Arc::new(wallet),
        Arc::new(CashuReceiver::new(helper)),
    )
    .await
    .context("Paid retrieval is configured but cannot start")?;
    Ok(Some(started))
}

/// [`start_daemon_paid_retrieval`] with the wallet and the receiver supplied.
pub(crate) async fn start_paid_retrieval(
    settings: PaidRetrievalSettings,
    bind_address: &str,
    fips: Option<&DaemonFipsHandle>,
    store: &Arc<HashtreeStore>,
    wallet: Arc<dyn RetrievalWallet>,
    receiver: Arc<dyn PaymentReceiver>,
) -> Result<Arc<DaemonPaidRetrieval>> {
    let Some(fips) = fips else {
        bail!("the FIPS endpoint is off (server.enable_fips, or a server mode without hash reads)");
    };
    let selling = match settings.provider {
        Some(provider) => Some(open_provider(provider, store, receiver)?),
        None => None,
    };
    let endpoint = fips.endpoint.clone();
    let carrier = FipsPaidConfig::default();
    let service = match &selling {
        Some(selling) => {
            let provider = selling.provider.clone();
            FipsPaidBlobService::serve_advertised(
                endpoint,
                provider,
                carrier,
                SAME_HOST_PROVIDER_PRIORITY,
            )
            .await
        }
        None => FipsPaidBlobService::client(endpoint, carrier).await,
    }
    .context("Failed to bind the paid blob service")?;
    let buying = match settings.client {
        Some(client) => {
            let buying = open_client(client, bind_address, &service, wallet)?;
            register_paid_routes(&fips.blob_resolver, &buying.routes).await?;
            Some(buying)
        }
        None => None,
    };
    if let Some(selling) = &selling {
        let tariff = selling.provider.seller().config().tariff;
        tracing::info!(
            "Paid retrieval: selling at {} msat per {} bytes, at least {} msat per blob",
            tariff.rate.msat,
            tariff.rate.per_bytes,
            tariff.min_msat
        );
    }
    if let Some(buying) = &buying {
        tracing::info!(
            "Paid retrieval: buying from {} provider(s), {} sat left of {}",
            buying.routes.len(),
            buying.limit.remaining_sat(),
            buying.limit.limit_sat()
        );
    }
    Ok(Arc::new(DaemonPaidRetrieval {
        service: tokio::sync::Mutex::new(Some(service)),
        selling,
        buying,
    }))
}

fn open_provider(
    settings: ProviderSettings,
    store: &Arc<HashtreeStore>,
    receiver: Arc<dyn PaymentReceiver>,
) -> Result<SellingSide> {
    fs::create_dir_all(&settings.ledger_dir).context("Failed to create the paid ledger")?;
    keep_private(&settings.ledger_dir)?;
    let ledger = FileLedgerStore::open(&settings.ledger_dir)
        .context("Failed to open the paid ledger (is another daemon using it?)")?;
    // Only what this node stores is sold; a sale never searches the mesh.
    let local = StoreBlobRoute::new(store.store_arc());
    let source: Arc<dyn BlobRoute> = Arc::new(BlockingStoreRoute(Arc::new(local)));
    let provider = PaidBlobProvider::open(source, receiver, settings.config, Arc::new(ledger))
        .context("Failed to read the paid ledger")?;
    let http = settings
        .http
        .then(|| Arc::new(Semaphore::new(MAX_HTTP_EXCHANGES)));
    Ok(SellingSide {
        provider,
        paying_only: settings.paying_only,
        http,
    })
}

fn open_client(
    settings: ClientSettings,
    bind_address: &str,
    service: &FipsPaidBlobService,
    wallet: Arc<dyn RetrievalWallet>,
) -> Result<BuyingSide> {
    let accounts = settings.state_dir.join("accounts");
    fs::create_dir_all(&accounts).context("Failed to create the paid account directory")?;
    keep_private(&settings.state_dir)?;
    keep_private(&accounts)?;
    let spend_store = Arc::new(FileSpendStore::new(settings.state_dir.join("spent")));
    let spent_sat = spend_store
        .load()
        .context("Failed to read what paid retrieval has spent")?;
    let limit = SpendingLimit::with_store(settings.spending_limit_sat, spent_sat, spend_store);
    let wallet: Arc<dyn RetrievalWallet> = Arc::new(LimitedWallet::new(wallet, limit.clone()));
    let mut routes = Vec::with_capacity(settings.providers.len());
    for peer in settings.providers {
        let account_store = FileAccountStore::new(accounts.join(account_file(&peer.place)));
        let account = account_store
            .load()
            .with_context(|| format!("Failed to read the paid account with {}", peer.name))?
            .unwrap_or_default();
        let transport: Arc<dyn PaidBlobTransport> = match &peer.place {
            ProviderPlace::Fips { identity, .. } => Arc::new(service.transport_to(*identity)),
            ProviderPlace::Http { url } => Arc::new(HttpPaidTransport::at_endpoint(url.clone())?),
        };
        let route = PaidBlobRoute::with_store(
            peer.name.clone(),
            transport,
            wallet.clone(),
            settings.policy.clone(),
            account,
            Arc::new(account_store),
        );
        routes.push(ClientRoute {
            name: peer.name,
            place: peer.place,
            route,
        });
    }
    Ok(BuyingSide {
        limit,
        routes,
        pays_for_http_reads: settings.pay_for_remote_reads || binds_loopback(bind_address),
    })
}

/// Add the paid routes to the daemon's router, in its paid tier.
async fn register_paid_routes(resolver: &DaemonBlobResolver, paid: &[ClientRoute]) -> Result<()> {
    let mut routes = resolver.routes().await;
    for provider in paid {
        routes.push(BlobRouteEntry::paid(
            format!("paid:{}", provider.name),
            Arc::new(provider.route.clone()),
        ));
    }
    resolver
        .set_routes(routes)
        .await
        .map_err(anyhow::Error::msg)
        .context("Failed to register the paid blob routes")
}

/// The file of the account with one provider: named by its npub, or by a
/// hash of its gateway's endpoint.
fn account_file(place: &ProviderPlace) -> String {
    match place {
        ProviderPlace::Fips { npub, .. } => format!("{npub}.account"),
        ProviderPlace::Http { url } => {
            let digest = hashtree_core::sha256(url.as_str().as_bytes());
            format!("http-{}.account", hex::encode(&digest[..16]))
        }
    }
}

/// The account files hold bearer ids and unredeemed tokens, and the ledger
/// the account ids of its buyers.
fn keep_private(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
            .context("Failed to make a paid retrieval directory private")?;
    }
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

fn binds_loopback(bind_address: &str) -> bool {
    match bind_address.parse::<SocketAddr>() {
        Ok(address) => address.ip().is_loopback(),
        Err(_) => bind_address
            .rsplit_once(':')
            .is_some_and(|(host, _)| host.eq_ignore_ascii_case("localhost")),
    }
}
