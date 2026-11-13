//! Configuration of paid retrieval, and one purchase between two daemons'
//! FIPS stacks with a fake mint. No network beyond loopback, no real funds.

use std::net::{SocketAddr, SocketAddrV4, UdpSocket};
use std::path::PathBuf;
use std::time::Duration;

use hashtree_core::{sha256, BlobReply, BlobRequest, Store, BLOB_DEFAULT_HTL};
use hashtree_paid_retrieval::discovered_paid_providers;
use hashtree_paid_retrieval::test_support::{FakeMint, FakeReceiver, FakeWallet, MINT};
use nostr::nips::nip19::ToBech32;

use super::*;
use crate::fips_transport::{
    buy_daemon_blob, daemon_fips_peer_configs, read_daemon_blob, start_daemon_fips_transport,
};

const DESIGN: &str = include_str!("../../../../../docs/paid-retrieval.md");

/// The example of the "Configuration" section of the design document.
fn documented_example() -> PaidRetrievalConfig {
    let section = DESIGN.split("## Configuration").nth(1).unwrap();
    let example = section.split("```toml").nth(1).unwrap();
    let example = example.split("```").next().unwrap();
    let config: Config = toml::from_str(example).unwrap();
    config.paid_retrieval
}

fn npub() -> String {
    nostr::Keys::generate().public_key().to_bech32().unwrap()
}

fn settings(config: &PaidRetrievalConfig) -> Result<Option<PaidRetrievalSettings>> {
    PaidRetrievalSettings::from_config(config, std::path::Path::new("/data"))
}

fn paid(toml: &str) -> PaidRetrievalConfig {
    toml::from_str(toml).unwrap()
}

#[test]
fn nothing_is_configured_by_default() {
    let config = Config::default();
    assert!(config.paid_retrieval.is_unset());
    assert!(!config.paid_retrieval.paying_only());
    assert!(settings(&config.paid_retrieval).unwrap().is_none());

    let parsed: Config = toml::from_str("").unwrap();
    assert_eq!(parsed.paid_retrieval, PaidRetrievalConfig::default());
    let saved = toml::to_string_pretty(&config).unwrap();
    assert!(!saved.contains("paid_retrieval"), "{saved}");
    assert!(daemon_fips_peer_configs(&config, Vec::new()).is_empty());
}

#[tokio::test]
async fn nothing_is_started_or_written_without_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(HashtreeStore::new(dir.path().join("store")).unwrap());
    let data_dir = dir.path().join("data");

    let started = start_daemon_paid_retrieval(
        &Config::default(),
        &data_dir,
        "127.0.0.1:8080",
        None,
        &store,
    )
    .await
    .unwrap();

    assert!(started.is_none());
    assert!(!data_dir.join(PAID_RETRIEVAL_DIR).exists());
}

#[tokio::test]
async fn the_daemon_refuses_to_sell_without_a_mint() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(HashtreeStore::new(dir.path().join("store")).unwrap());
    let data_dir = dir.path().join("data");
    let config: Config = toml::from_str(
        r#"[paid_retrieval.provider]
paying_only = true
tariff = { msat = 1, per_bytes = 1024, min_msat = 100 }"#,
    )
    .unwrap();

    let refused = start_daemon_paid_retrieval(&config, &data_dir, "127.0.0.1:8080", None, &store)
        .await
        .err()
        .expect("a provider without a mint must not start");

    assert_eq!(
        format!("{refused:#}"),
        "Paid retrieval is configured but cannot start: \
         paid_retrieval.provider.mints lists no mint; none is chosen by default"
    );
    assert!(!data_dir.join(PAID_RETRIEVAL_DIR).exists());
}

#[test]
fn the_documented_example_is_a_complete_configuration() {
    let mut config = documented_example();
    let peer = npub();
    config.client.as_mut().unwrap().providers[0].npub = peer.clone();

    let settings = settings(&config).unwrap().unwrap();

    let provider = settings.provider.unwrap();
    assert_eq!(provider.config.tariff.rate.msat, 1);
    assert_eq!(provider.config.tariff.rate.per_bytes, 1024);
    assert_eq!(provider.config.tariff.min_msat, 100);
    assert_eq!(provider.config.mints, ["https://mint.example"]);
    assert_eq!(provider.config.min_topup_sat, 1);
    assert_eq!(provider.config.grace_msat, 0);
    assert!(!provider.paying_only);
    assert_eq!(
        provider.ledger_dir,
        PathBuf::from("/data")
            .join(PAID_RETRIEVAL_DIR)
            .join("ledger")
    );
    let client = settings.client.unwrap();
    assert_eq!(client.spending_limit_sat, 1000);
    assert_eq!(client.policy.max_tariff, provider.config.tariff);
    assert_eq!(client.policy.mints, ["https://mint.example"]);
    assert_eq!(client.policy.topup_sat, 20);
    assert_eq!(client.policy.max_prepaid_msat, 100_000);
    assert_eq!(client.policy.max_loss_msat, 20_000);
    assert_eq!(client.policy.max_token_fee_sat, 2);
    assert_eq!(client.policy.max_redeem_fee_msat, 2_000);
    assert!(!client.pay_for_remote_reads);
    assert_eq!(client.providers.len(), 1);
    assert_eq!(client.providers[0].name, "archive");
    assert_eq!(client.providers[0].npub, peer);
}

#[test]
fn a_provider_takes_no_default_for_grace_ledger_or_free_serving_that_costs_money() {
    let config = paid(
        r#"[provider]
mints = [" https://mint.example ", "https://mint.example"]
ledger_dir = "/var/ledger"
tariff = { msat = 2, per_bytes = 4096, min_msat = 0 }"#,
    );

    let provider = settings(&config).unwrap().unwrap().provider.unwrap();

    assert_eq!(provider.config.mints, ["https://mint.example"]);
    assert_eq!(provider.config.grace_msat, 0);
    assert_eq!(provider.config.min_topup_sat, 1);
    assert_eq!(provider.ledger_dir, PathBuf::from("/var/ledger"));
    assert!(!provider.paying_only);
}

#[test]
fn a_provider_without_a_mint_or_a_price_is_refused() {
    let no_mint = paid("[provider]\ntariff = { msat = 1, per_bytes = 1024, min_msat = 100 }");
    let error = settings(&no_mint).unwrap_err().to_string();
    assert_eq!(
        error,
        "paid_retrieval.provider.mints lists no mint; none is chosen by default"
    );

    let no_price = paid("[provider]\nmints = [\"https://mint.example\"]\ntariff = { msat = 1 }");
    let error = settings(&no_price).unwrap_err().to_string();
    assert!(
        error.contains("tariff.per_bytes, tariff.min_msat"),
        "{error}"
    );
    assert!(!error.contains("tariff.msat"), "{error}");

    let nothing = paid("[provider]");
    let error = settings(&nothing).unwrap_err().to_string();
    assert!(error.contains("tariff.msat"), "{error}");
    assert!(error.contains("mints"), "{error}");
}

#[test]
fn a_client_must_name_every_cap_itself() {
    let peer = npub();
    let bare = paid(&format!(
        "[[client.providers]]\nname = \"archive\"\nnpub = \"{peer}\""
    ));
    let error = settings(&bare).unwrap_err().to_string();
    for key in [
        "spending_limit_sat",
        "max_tariff.msat",
        "max_tariff.per_bytes",
        "max_tariff.min_msat",
        "mints",
        "topup_sat",
        "max_prepaid_msat",
        "max_loss_msat",
        "max_token_fee_sat",
        "max_redeem_fee_msat",
    ] {
        assert!(error.contains(key), "{key} is missing from: {error}");
    }

    let mut config = documented_example();
    let client = config.client.as_mut().unwrap();
    client.providers[0].npub = "not an identity".into();
    let error = settings(&config).unwrap_err().to_string();
    assert!(error.contains("archive has no valid npub"), "{error}");

    let client = config.client.as_mut().unwrap();
    client.providers[0].npub = peer;
    client.providers.push(client.providers[0].clone());
    let error = settings(&config).unwrap_err().to_string();
    assert!(error.contains("twice"), "{error}");

    let client = config.client.as_mut().unwrap();
    client.providers.pop();
    client.topup_sat = Some(101);
    let error = settings(&config).unwrap_err().to_string();
    assert!(error.contains("max_prepaid_msat"), "{error}");
}

#[test]
fn a_client_policy_without_providers_registers_nothing() {
    let mut config = documented_example();
    config.provider = None;
    config.client.as_mut().unwrap().providers.clear();
    assert!(settings(&config).unwrap().is_none());
}

#[test]
fn providers_the_client_buys_from_become_fips_peers() {
    let mut config = Config {
        paid_retrieval: documented_example(),
        ..Config::default()
    };
    let peer = npub();
    config.paid_retrieval.client.as_mut().unwrap().providers[0].npub = peer.clone();

    let peers = daemon_fips_peer_configs(&config, vec![peer.clone()]);

    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].npub, peer);
}

#[test]
fn only_a_loopback_listener_pays_for_http_reads_unasked() {
    assert!(binds_loopback("127.0.0.1:8080"));
    assert!(binds_loopback("[::1]:8080"));
    assert!(binds_loopback("localhost:8080"));
    assert!(!binds_loopback("0.0.0.0:8080"));
    assert!(!binds_loopback("192.168.1.5:8080"));
    assert!(!binds_loopback("example.org:8080"));
}

#[tokio::test]
async fn paid_retrieval_without_a_fips_endpoint_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(HashtreeStore::new(dir.path().join("store")).unwrap());
    let mut config = documented_example();
    config.client = None;
    let settings = PaidRetrievalSettings::from_config(&config, dir.path())
        .unwrap()
        .unwrap();
    let mint = Arc::new(FakeMint::default());

    let error = start_paid_retrieval(
        settings,
        "127.0.0.1:8080",
        None,
        &store,
        FakeWallet::funded(&mint, 0),
        FakeReceiver::new(&mint),
    )
    .await
    .err()
    .expect("no endpoint, no paid service");

    assert!(
        error.to_string().contains("FIPS endpoint is off"),
        "{error}"
    );
    assert!(!dir.path().join(PAID_RETRIEVAL_DIR).exists());
}

fn rendezvous_addr() -> SocketAddrV4 {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    match socket.local_addr().unwrap() {
        SocketAddr::V4(addr) => addr,
        SocketAddr::V6(_) => unreachable!("loopback IPv4 bind returned IPv6"),
    }
}

struct TestDaemon {
    dir: tempfile::TempDir,
    config: Config,
    store: Arc<HashtreeStore>,
    fips: DaemonFipsHandle,
}

impl TestDaemon {
    /// A daemon's store and FIPS stack, reachable on this host only.
    async fn start(rendezvous: SocketAddrV4, role: &str, paid_retrieval: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(HashtreeStore::new(dir.path().join("store")).unwrap());
        let mut config = Config::default();
        config.server.fips_discovery_scope = format!("htree-paid-{role}-{}", uuid::Uuid::new_v4());
        config.server.fips_relays = Some(Vec::new());
        config.server.fips_websocket_seed_urls = Some(Vec::new());
        config.server.enable_fips_udp = false;
        config.server.enable_fips_webrtc = false;
        config.server.enable_fips_lan_discovery = false;
        config.server.fips_local_rendezvous_addr = Some(rendezvous.to_string());
        config.paid_retrieval = paid(paid_retrieval);
        let keys = nostr::Keys::generate();
        let fips = start_daemon_fips_transport(&config, &keys, store.clone(), Vec::new())
            .await
            .unwrap()
            .expect("the FIPS endpoint is enabled");
        Self {
            dir,
            config,
            store,
            fips,
        }
    }

    fn data_dir(&self) -> PathBuf {
        self.dir.path().join("data")
    }

    async fn start_paid(
        &self,
        wallet: Arc<FakeWallet>,
        receiver: Arc<FakeReceiver>,
    ) -> Arc<DaemonPaidRetrieval> {
        let settings =
            PaidRetrievalSettings::from_config(&self.config.paid_retrieval, &self.data_dir());
        start_paid_retrieval(
            settings.unwrap().expect("paid retrieval is configured"),
            "127.0.0.1:8080",
            Some(&self.fips),
            &self.store,
            wallet,
            receiver,
        )
        .await
        .unwrap()
    }

    fn request(hash: hashtree_core::Hash) -> BlobRequest {
        BlobRequest {
            hash,
            htl: BLOB_DEFAULT_HTL,
        }
    }

    /// What the daemon does for a peer and before it considers paying.
    async fn read_free(&self, hash: hashtree_core::Hash) -> BlobReply {
        let resolver = &self.fips.blob_resolver;
        let read = read_daemon_blob(resolver, Self::request(hash), None).await;
        read.unwrap()
    }

    /// What the daemon does once every free source finished without data.
    async fn buy(&self, hash: hashtree_core::Hash) -> BlobReply {
        let resolver = &self.fips.blob_resolver;
        buy_daemon_blob(resolver, Self::request(hash))
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn a_daemon_buys_a_blob_from_a_paying_only_daemon_over_fips() {
    let rendezvous = rendezvous_addr();
    let mint = Arc::new(FakeMint::default());

    // The provider sells at 1 msat per KiB, at least 1 sat per blob, and
    // gives nothing away on the free blob service.
    let provider = TestDaemon::start(
        rendezvous,
        "provider",
        &format!(
            r#"[provider]
mints = ["{MINT}"]
paying_only = true
tariff = {{ msat = 1, per_bytes = 1024, min_msat = 1000 }}"#
        ),
    )
    .await;
    let data = vec![7u8; 2048];
    let hash = sha256(&data);
    let provider_store = provider.store.store_arc();
    provider_store.put(hash, data.clone()).await.unwrap();
    let receiver = FakeReceiver::new(&mint);
    let selling = provider
        .start_paid(FakeWallet::funded(&mint, 0), receiver.clone())
        .await;

    let client = TestDaemon::start(
        rendezvous,
        "client",
        &format!(
            r#"[client]
spending_limit_sat = 50
mints = ["{MINT}"]
topup_sat = 5
max_prepaid_msat = 20000
max_loss_msat = 5000
max_token_fee_sat = 0
max_redeem_fee_msat = 0
max_tariff = {{ msat = 1, per_bytes = 1024, min_msat = 1000 }}

[[client.providers]]
name = "archive"
npub = "{}""#,
            provider.fips.endpoint_npub
        ),
    )
    .await;
    let wallet = FakeWallet::funded(&mint, 100);
    let buying = client
        .start_paid(wallet.clone(), FakeReceiver::new(&mint))
        .await;
    assert_eq!(client.fips.blob_resolver.route_count().await, 3);

    // Both of the provider's services are known to the client.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let paid = discovered_paid_providers(&client.fips.endpoint).unwrap();
            if paid
                .iter()
                .any(|peer| peer.npub() == provider.fips.endpoint_npub)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the paid provider was not discovered");

    // The free routes find nothing, and asking them pays nothing.
    let free = client.read_free(hash).await;
    assert_eq!(free, BlobReply::NoResult);
    assert_eq!(wallet.payments(), 0);

    // The paid tier buys the blob, once.
    let bought = client.buy(hash).await;
    assert_eq!(bought, BlobReply::Data(data.clone()));
    assert_eq!(wallet.payments(), 1);
    assert_eq!(wallet.balance(), 95);
    assert_eq!(receiver.received(), 5);
    let cached = client.store.store_arc().get(&hash).await.unwrap();
    assert_eq!(cached, Some(data));

    // What `htree status` is given: the limit, and the account by its
    // numbers only.
    let status = buying.status();
    let client_status = &status["client"];
    assert_eq!(client_status["spending_limit_sat"], 50);
    assert_eq!(client_status["spent_sat"], 5);
    assert_eq!(client_status["remaining_sat"], 45);
    assert_eq!(client_status["pays_for_http_reads"], true);
    let archive = &client_status["providers"][0];
    assert_eq!(archive["name"], "archive");
    assert_eq!(archive["balance_msat"], 4000);
    assert_eq!(archive["paid_sat"], 5);
    assert_eq!(archive["blobs"], 1);
    assert_eq!(archive["unresolved_tokens"], 0);
    assert_eq!(archive["declined"]["price"], 0);
    assert!(archive["blocked"].is_null());
    assert!(status["provider"].is_null());
    let sold = selling.status();
    assert_eq!(sold["provider"]["accounts"], 1);
    assert_eq!(sold["provider"]["held_msat"], 4000);
    assert_eq!(sold["provider"]["paying_only"], true);
    assert!(sold["client"].is_null());
    let account = buying.buying.as_ref().unwrap().routes[0]
        .route
        .account_state();
    let account_id = hex::encode(account.account);
    for shown in [status.to_string(), sold.to_string()] {
        assert!(!shown.contains("cashu"), "a token is shown: {shown}");
        assert!(!shown.contains(&account_id), "an account id is shown");
    }
    let lines = format_paid_status(&status).join("\n");
    assert!(
        lines.contains("Spending limit: 50 sat, 5 spent, 45 remaining"),
        "{lines}"
    );
    assert!(
        lines.contains("archive: balance 4.000 sat, paid 5 sat"),
        "{lines}"
    );

    // The spent total and the account are on disk, privately.
    let state_dir = client.data_dir().join(PAID_RETRIEVAL_DIR);
    assert_eq!(
        FileSpendStore::new(state_dir.join("spent")).load().unwrap(),
        5
    );
    let account_file = state_dir
        .join("accounts")
        .join(format!("{}.account", provider.fips.endpoint_npub));
    let saved = FileAccountStore::new(account_file).load().unwrap().unwrap();
    assert_eq!(saved.account, account.account);
    assert_eq!(saved.credited_msat, 5000);
    let ledger = provider.data_dir().join(PAID_RETRIEVAL_DIR).join("ledger");
    assert!(ledger.join("ledger.log").exists());
    #[cfg(unix)]
    for private in [&ledger, &state_dir, &state_dir.join("accounts")] {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(private).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o077,
            0,
            "{} is readable by others",
            private.display()
        );
    }

    buying.shutdown().await;
    selling.shutdown().await;
    client.fips.shutdown().await;
    provider.fips.shutdown().await;
}
