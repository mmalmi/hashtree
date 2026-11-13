//! Configuration of paid retrieval, and one purchase between two daemons'
//! FIPS stacks with a fake mint. No network beyond loopback, no real funds.

use std::net::{SocketAddr, SocketAddrV4, UdpSocket};
use std::path::PathBuf;
use std::time::Duration;

use hashtree_core::{sha256, BlobReply, BlobRequest, Store, BLOB_DEFAULT_HTL};
use hashtree_paid_retrieval::test_support::{FakeMint, FakeReceiver, FakeWallet, MINT};
use hashtree_paid_retrieval::{discovered_paid_providers, BytePrice};
use nostr::nips::nip19::ToBech32;

use super::*;
use crate::fips_transport::{
    daemon_fips_peer_configs, read_daemon_blob, start_daemon_fips_transport,
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

/// The mints of the Cashu wallet in these tests.
const WALLET_MINT: &str = "https://mint.example";

fn settings(config: &PaidRetrievalConfig) -> Result<Option<PaidRetrievalSettings>> {
    let wallet_mints = [WALLET_MINT.to_string()];
    PaidRetrievalSettings::from_config(config, &wallet_mints, std::path::Path::new("/data"))
}

fn settings_without_wallet_mints(
    config: &PaidRetrievalConfig,
) -> Result<Option<PaidRetrievalSettings>> {
    PaidRetrievalSettings::from_config(config, &[], std::path::Path::new("/data"))
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
async fn the_daemon_refuses_to_sell_when_neither_it_nor_its_wallet_names_a_mint() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(HashtreeStore::new(dir.path().join("store")).unwrap());
    let data_dir = dir.path().join("data");
    let config: Config = toml::from_str("[paid_retrieval.provider]\npaying_only = true").unwrap();

    let refused = start_daemon_paid_retrieval(&config, &data_dir, "127.0.0.1:8080", None, &store)
        .await
        .err()
        .expect("a provider without a mint must not start");

    assert_eq!(
        format!("{refused:#}"),
        "Paid retrieval is configured but cannot start: \
         paid_retrieval.provider.mints lists no mint and the Cashu wallet accepts none; \
         add one with `htree cashu mint add <url>`"
    );
    assert!(!data_dir.join(PAID_RETRIEVAL_DIR).exists());

    let config: Config = toml::from_str(
        "[paid_retrieval.client]\n[[paid_retrieval.client.providers]]\n\
         name = \"gateway\"\nurl = \"http://127.0.0.1:1\"",
    )
    .unwrap();
    let refused = start_daemon_paid_retrieval(&config, &data_dir, "127.0.0.1:8080", None, &store)
        .await
        .err()
        .expect("a client without a mint must not start");
    assert!(
        format!("{refused:#}").contains("htree cashu mint add"),
        "{refused:#}"
    );
}

#[test]
fn the_wallet_mints_are_the_default_of_both_sides_and_an_explicit_list_wins() {
    let config: Config = toml::from_str(
        r#"[cashu]
accepted_mints = ["https://wallet-a.example", "https://wallet-b.example"]

[paid_retrieval.provider]

[paid_retrieval.client]
[[paid_retrieval.client.providers]]
name = "gateway"
url = "https://gw.example""#,
    )
    .unwrap();
    let wallet = &config.cashu.accepted_mints;
    let data_dir = std::path::Path::new("/data");
    let parsed = PaidRetrievalSettings::from_config(&config.paid_retrieval, wallet, data_dir);
    let parsed = parsed.unwrap().unwrap();
    assert_eq!(parsed.provider.unwrap().config.mints, *wallet);
    assert_eq!(parsed.client.unwrap().policy.mints, *wallet);

    let mut explicit = config.paid_retrieval.clone();
    explicit.provider.as_mut().unwrap().mints = vec!["https://own.example".into()];
    explicit.client.as_mut().unwrap().mints = vec!["https://trusted.example".into()];
    let parsed = PaidRetrievalSettings::from_config(&explicit, wallet, data_dir);
    let parsed = parsed.unwrap().unwrap();
    assert_eq!(
        parsed.provider.unwrap().config.mints,
        ["https://own.example"]
    );
    assert_eq!(
        parsed.client.unwrap().policy.mints,
        ["https://trusted.example"]
    );

    let error = settings_without_wallet_mints(&config.paid_retrieval)
        .unwrap_err()
        .to_string();
    assert!(error.contains("the Cashu wallet accepts none"), "{error}");
}

#[test]
fn an_empty_section_takes_every_default() {
    let config = paid(
        "[provider]\n[client]\n[[client.providers]]\nname = \"gateway\"\n\
         url = \"https://gw.example\"",
    );
    let parsed = settings(&config).unwrap().unwrap();

    let provider = parsed.provider.unwrap();
    assert_eq!(provider.config.tariff, DEFAULT_TARIFF);
    assert_eq!(provider.config.tariff.rate.msat, 1);
    assert_eq!(provider.config.tariff.rate.per_bytes, 10_240);
    assert_eq!(provider.config.tariff.min_msat, 10);
    assert_eq!(provider.config.mints, [WALLET_MINT]);
    assert_eq!(provider.config.min_topup_sat, 1);
    assert_eq!(provider.config.grace_msat, 0);
    assert_eq!(provider.config.lock, None);
    assert!(!provider.paying_only);
    assert!(!provider.http);

    let client = parsed.client.unwrap();
    assert_eq!(client.policy.max_tariff, DEFAULT_MAX_TARIFF);
    assert_eq!(client.policy.max_tariff.rate.msat, 1);
    assert_eq!(client.policy.max_tariff.rate.per_bytes, 2_048);
    assert_eq!(client.policy.max_tariff.min_msat, 100);
    assert_eq!(client.policy.mints, [WALLET_MINT]);
    assert_eq!(client.policy.topup_sat, 10);
    assert_eq!(client.policy.max_prepaid_msat, 21_000);
    assert_eq!(client.policy.max_loss_msat, 10_000);
    assert_eq!(client.policy.max_token_fee_sat, 2);
    assert_eq!(client.policy.max_redeem_fee_msat, 2_000);
    assert_eq!(client.spending_limit_sat, 1_000);
    assert!(!client.pay_for_remote_reads);

    // The default price is a fifth of the default cap: 2 MiB costs 205 msat
    // and is accepted up to 1024 msat.
    let two_mib = 2 * 1024 * 1024;
    assert_eq!(DEFAULT_TARIFF.price_msat(two_mib), 205);
    assert_eq!(DEFAULT_MAX_TARIFF.price_msat(two_mib), 1_024);
}

#[test]
fn a_value_left_out_takes_its_default_and_one_given_is_kept() {
    let config = paid(
        "[provider]\ntariff = { msat = 3 }\n[client]\nmax_tariff = { per_bytes = 512 }\n\
         topup_sat = 4\nspending_limit_sat = 50\n\
         [[client.providers]]\nname = \"gateway\"\nurl = \"https://gw.example\"",
    );
    let parsed = settings(&config).unwrap().unwrap();
    let tariff = parsed.provider.unwrap().config.tariff;
    assert_eq!(tariff.rate, BytePrice::new(3, 10_240));
    assert_eq!(tariff.min_msat, 10);
    let client = parsed.client.unwrap();
    assert_eq!(client.policy.max_tariff.rate, BytePrice::new(1, 512));
    assert_eq!(client.policy.max_tariff.min_msat, 100);
    assert_eq!(client.policy.topup_sat, 4);
    assert_eq!(client.spending_limit_sat, 50);
    assert_eq!(client.policy.max_prepaid_msat, DEFAULT_MAX_PREPAID_MSAT);
}

#[test]
fn the_documented_example_shows_exactly_the_defaults() {
    let mut config = documented_example();
    let peer = npub();
    config.client.as_mut().unwrap().providers[0].npub = Some(peer.clone());

    let documented = settings(&config).unwrap().unwrap();

    let mut defaults = paid("[provider]\n[client]");
    defaults.client.as_mut().unwrap().providers = config.client.as_ref().unwrap().providers.clone();
    let defaults = settings(&defaults).unwrap().unwrap();
    let (provider, default_provider) = (documented.provider.unwrap(), defaults.provider.unwrap());
    assert_eq!(provider.config.tariff, default_provider.config.tariff);
    assert_eq!(provider.config.mints, default_provider.config.mints);
    assert_eq!(
        provider.config.min_topup_sat,
        default_provider.config.min_topup_sat
    );
    assert_eq!(
        provider.config.grace_msat,
        default_provider.config.grace_msat
    );
    assert_eq!(provider.config.lock, default_provider.config.lock);
    assert_eq!(provider.ledger_dir, default_provider.ledger_dir);
    assert_eq!(
        (provider.paying_only, provider.http),
        (default_provider.paying_only, default_provider.http)
    );
    assert_eq!(
        provider.ledger_dir,
        PathBuf::from("/data")
            .join(PAID_RETRIEVAL_DIR)
            .join("ledger")
    );
    let (client, default_client) = (documented.client.unwrap(), defaults.client.unwrap());
    assert_eq!(client.spending_limit_sat, default_client.spending_limit_sat);
    assert_eq!(
        client.pay_for_remote_reads,
        default_client.pay_for_remote_reads
    );
    let (policy, default_policy) = (&client.policy, &default_client.policy);
    assert_eq!(policy.max_tariff, default_policy.max_tariff);
    assert_eq!(policy.mints, default_policy.mints);
    assert_eq!(policy.topup_sat, default_policy.topup_sat);
    assert_eq!(policy.max_prepaid_msat, default_policy.max_prepaid_msat);
    assert_eq!(policy.max_loss_msat, default_policy.max_loss_msat);
    assert_eq!(policy.max_token_fee_sat, default_policy.max_token_fee_sat);
    assert_eq!(
        policy.max_redeem_fee_msat,
        default_policy.max_redeem_fee_msat
    );
    assert_eq!(client.providers.len(), 2);
    assert_eq!(client.providers[0].name, "archive");
    assert_eq!(client.providers[0].place.label(), peer);
    assert_eq!(client.providers[1].name, "gateway");
    assert_eq!(
        client.providers[1].place.label(),
        "https://gw.example/paid-blob"
    );
}

#[test]
fn a_provider_is_named_by_its_npub_or_by_its_gateway() {
    let mut config = documented_example();
    let client = config.client.as_mut().unwrap();
    client.providers[0].npub = None;
    client.providers[0].url = Some("http://127.0.0.1:8080/".into());
    let parsed = settings(&config).unwrap().unwrap();
    let provider = &parsed.client.unwrap().providers[0];
    assert!(matches!(provider.place, ProviderPlace::Http { .. }));
    assert_eq!(provider.place.label(), "http://127.0.0.1:8080/paid-blob");
    assert_eq!(config.client_provider_npubs().count(), 0);

    let client = config.client.as_mut().unwrap();
    client.providers[0].npub = Some(npub());
    let error = settings(&config).unwrap_err().to_string();
    assert!(
        error.contains("archive needs either npub or url"),
        "{error}"
    );

    let client = config.client.as_mut().unwrap();
    client.providers[0].npub = None;
    client.providers[0].url = None;
    let error = settings(&config).unwrap_err().to_string();
    assert!(
        error.contains("archive needs either npub or url"),
        "{error}"
    );

    let client = config.client.as_mut().unwrap();
    client.providers[0].url = Some("https://user:secret@gw.example".into());
    let error = settings(&config).unwrap_err().to_string();
    assert!(error.contains("user name or password"), "{error}");
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
fn a_provider_may_name_a_lock_key_and_nothing_else() {
    let key = format!("02{}", "ab".repeat(32));
    let config = paid(&format!(
        "[provider]\nmints = [\"https://mint.example\"]\nlock = \"{key}\"\n\
         tariff = {{ msat = 1, per_bytes = 1024, min_msat = 100 }}"
    ));
    let provider = settings(&config).unwrap().unwrap().provider.unwrap();
    assert_eq!(provider.config.lock.map(hex::encode), Some(key));

    for bad in [
        "04".to_string() + &"ab".repeat(32),
        "02ab".into(),
        "zz".into(),
    ] {
        let config = paid(&format!(
            "[provider]\nmints = [\"https://mint.example\"]\nlock = \"{bad}\"\n\
             tariff = {{ msat = 1, per_bytes = 1024, min_msat = 100 }}"
        ));
        let error = settings(&config).unwrap_err().to_string();
        assert!(error.contains("compressed public key"), "{bad}: {error}");
    }
}

#[test]
fn a_client_refuses_bad_providers_and_inconsistent_caps() {
    let peer = npub();
    let mut config = documented_example();
    let client = config.client.as_mut().unwrap();
    client.providers[0].npub = Some("not an identity".into());
    let error = settings(&config).unwrap_err().to_string();
    assert!(error.contains("archive has no valid npub"), "{error}");

    let client = config.client.as_mut().unwrap();
    client.providers[0].npub = Some(peer);
    client.providers.push(client.providers[0].clone());
    let error = settings(&config).unwrap_err().to_string();
    assert!(error.contains("twice"), "{error}");

    let client = config.client.as_mut().unwrap();
    client.providers.pop();
    client.topup_sat = Some(22);
    let error = settings(&config).unwrap_err().to_string();
    assert!(error.contains("max_prepaid_msat"), "{error}");

    let bad_price = paid("[provider]\ntariff = { per_bytes = 0 }");
    let error = settings(&bad_price).unwrap_err().to_string();
    assert!(error.contains("per_bytes must be at least 1"), "{error}");
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
    config.paid_retrieval.client.as_mut().unwrap().providers[0].npub = Some(peer.clone());

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
async fn without_fips_only_http_is_sold_and_only_gateways_are_bought_from() {
    let mint = Arc::new(FakeMint::default());
    let start = |toml: String| {
        let mint = mint.clone();
        async move {
            let dir = tempfile::tempdir().unwrap();
            let store = Arc::new(HashtreeStore::new(dir.path().join("store")).unwrap());
            let parsed = PaidRetrievalSettings::from_config(&paid(&toml), &[], dir.path());
            let started = start_paid_retrieval(
                parsed.unwrap().unwrap(),
                "127.0.0.1:8080",
                None,
                &store,
                FakeWallet::funded(&mint, 0),
                FakeReceiver::new(&mint),
            )
            .await;
            (dir, started)
        }
    };
    let provider = |http: bool| format!("[provider]\nmints = [\"{MINT}\"]\nhttp = {http}");
    let client = |place: String| {
        format!("[client]\nmints = [\"{MINT}\"]\n[[client.providers]]\nname = \"p\"\n{place}")
    };

    let (_dir, refused) = start(provider(false)).await;
    let error = refused.err().expect("nothing to sell over").to_string();
    assert!(error.contains("nowhere to sell"), "{error}");

    let (_dir, refused) = start(client(format!("npub = \"{}\"", npub()))).await;
    let error = refused.err().expect("an npub needs FIPS").to_string();
    assert!(error.contains("p can be reached only over FIPS"), "{error}");

    let (_dir, sells) = start(provider(true)).await;
    let sells = sells.unwrap();
    assert!(sells.status()["provider"]["http"].as_bool().unwrap());
    sells.shutdown().await;

    let (_dir, buys) = start(client("url = \"http://127.0.0.1:1\"".into())).await;
    let buys = buys.unwrap();
    assert!(buys.http_read_router().is_some());
    buys.shutdown().await;
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
            PaidRetrievalSettings::from_config(&self.config.paid_retrieval, &[], &self.data_dir());
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
}

/// What the daemon does once every free source finished without data.
async fn buy(paid: &DaemonPaidRetrieval, hash: hashtree_core::Hash) -> BlobReply {
    paid.buy(TestDaemon::request(hash)).await.unwrap()
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
    // The paid route is the paid retrieval's own: the FIPS resolver, which
    // also answers peers, has only its two free routes.
    assert_eq!(client.fips.blob_resolver.route_count().await, 2);
    assert!(buying.http_read_router().is_some());

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
    let bought = buy(&buying, hash).await;
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
    // FIPS authenticates the buyer: the provider charges its identity, not
    // the bearer id the buyer keeps for carriers that do not.
    let sold_accounts = selling.selling.as_ref().unwrap().provider.accounts();
    assert_eq!(sold_accounts.len(), 1);
    assert_ne!(sold_accounts[0].account, account.account);
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

/// Serve the daemon's HTTP routes for `store` on a loopback port.
pub(super) async fn serve_http(
    store: Arc<HashtreeStore>,
    paid: Option<Arc<DaemonPaidRetrieval>>,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let mut server = crate::server::HashtreeServer::new(store, address.clone());
    if let Some(paid) = paid {
        server = server.with_paid_retrieval(paid);
    }
    let task = tokio::spawn(async move {
        let _ = server.run_with_listener(listener).await;
    });
    (format!("http://{address}"), task)
}

fn selling_config(http: bool) -> String {
    format!(
        r#"[provider]
mints = ["{MINT}"]
http = {http}
tariff = {{ msat = 1, per_bytes = 1024, min_msat = 1000 }}"#
    )
}

#[tokio::test]
async fn a_daemon_buys_blobs_through_a_gateway_over_http() {
    let mint = Arc::new(FakeMint::default());
    // Each daemon has a FIPS rendezvous of its own: nothing reaches the
    // other side but HTTP.
    let provider = TestDaemon::start(rendezvous_addr(), "gateway", &selling_config(true)).await;
    let first = vec![1u8; 2048];
    let second = vec![2u8; 2048];
    let provider_store = provider.store.store_arc();
    for data in [&first, &second] {
        provider_store
            .put(sha256(data), data.clone())
            .await
            .unwrap();
    }
    let receiver = FakeReceiver::new(&mint);
    let selling = provider
        .start_paid(FakeWallet::funded(&mint, 0), receiver.clone())
        .await;
    let (gateway, server) = serve_http(provider.store.clone(), Some(selling.clone())).await;

    let client = TestDaemon::start(
        rendezvous_addr(),
        "http-buyer",
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
name = "gateway"
url = "{gateway}""#
        ),
    )
    .await;
    let wallet = FakeWallet::funded(&mint, 100);
    let buying = client
        .start_paid(wallet.clone(), FakeReceiver::new(&mint))
        .await;

    // The first blob needs a top-up: one token of 5 sat, redeemed by the
    // gateway, then 1 sat charged.
    let bought = buy(&buying, sha256(&first)).await;
    assert_eq!(bought, BlobReply::Data(first.clone()));
    assert_eq!(wallet.payments(), 1);
    assert_eq!(receiver.received(), 5);

    // The second is paid from the balance: no new token.
    let bought = buy(&buying, sha256(&second)).await;
    assert_eq!(bought, BlobReply::Data(second.clone()));
    assert_eq!(wallet.payments(), 1);
    assert_eq!(wallet.balance(), 95);

    let status = buying.status();
    let entry = &status["client"]["providers"][0];
    assert_eq!(entry["name"], "gateway");
    assert_eq!(entry["address"], format!("{gateway}/paid-blob"));
    assert_eq!(entry["balance_msat"], 3000);
    assert_eq!(entry["blobs"], 2);
    let sold = selling.status();
    assert_eq!(sold["provider"]["http"], true);
    assert_eq!(sold["provider"]["accounts"], 1);
    assert_eq!(sold["provider"]["held_msat"], 3000);
    // HTTP authenticates nobody: the account is the buyer's bearer id.
    let bearer = buying.buying.as_ref().unwrap().routes[0]
        .route
        .account_state()
        .account;
    let sold_accounts = selling.selling.as_ref().unwrap().provider.accounts();
    assert_eq!(sold_accounts.len(), 1);
    assert_eq!(sold_accounts[0].account, bearer);

    // The account is kept under a name derived from the gateway.
    let accounts = client.data_dir().join(PAID_RETRIEVAL_DIR).join("accounts");
    let saved: Vec<_> = std::fs::read_dir(&accounts)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(saved.len(), 1);
    assert!(saved[0].starts_with("http-") && saved[0].ends_with(".account"));

    server.abort();
    buying.shutdown().await;
    selling.shutdown().await;
    client.fips.shutdown().await;
    provider.fips.shutdown().await;
}

#[tokio::test]
async fn the_gateway_answers_every_protocol_outcome_with_200_and_nothing_else() {
    use hashtree_paid_retrieval::{Reply, Request};

    let mint = Arc::new(FakeMint::default());
    let provider = TestDaemon::start(rendezvous_addr(), "endpoint", &selling_config(true)).await;
    let stored = vec![3u8; 2048];
    let stored_hash = sha256(&stored);
    let provider_store = provider.store.store_arc();
    provider_store.put(stored_hash, stored).await.unwrap();
    let selling = provider
        .start_paid(FakeWallet::funded(&mint, 0), FakeReceiver::new(&mint))
        .await;
    let (gateway, server) = serve_http(provider.store.clone(), Some(selling.clone())).await;
    let endpoint = format!("{gateway}/paid-blob");
    let http = reqwest::Client::new();
    let exchange = |body: Vec<u8>| http.post(&endpoint).body(body).send();

    // Not a request at all: still a 200, holding an `err` reply.
    let response = exchange(b"not a request".to_vec()).await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let headers = response.headers().clone();
    assert_eq!(headers["cache-control"], "no-store");
    assert_eq!(headers["access-control-allow-origin"], "*");
    assert_eq!(headers["content-type"], "application/octet-stream");
    let reply = Reply::decode(&response.bytes().await.unwrap()).unwrap();
    assert!(matches!(reply, Reply::Error { .. }), "{reply:?}");

    // A blob the gateway does not have, and one it sells.
    let get = |hash: hashtree_core::Hash| Request::Get {
        hash: hash.into(),
        account: Some([7; 32]),
        max_msat: u64::MAX,
        htl: 0,
        payment: None,
    };
    let response = exchange(get(sha256(b"absent")).encode()).await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let reply = Reply::decode(&response.bytes().await.unwrap()).unwrap();
    assert!(matches!(reply, Reply::NoResult { .. }), "{reply:?}");
    let response = exchange(get(stored_hash).encode()).await.unwrap();
    let reply = Reply::decode(&response.bytes().await.unwrap()).unwrap();
    assert!(matches!(reply, Reply::Quote(_)), "{reply:?}");

    // A body larger than any request is an HTTP error.
    let response = exchange(vec![0; 64 * 1024 + 1]).await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(response.headers()["access-control-allow-origin"], "*");
    assert!(response.headers().contains_key("x-reason"));

    // A browser may ask first.
    let preflight = http
        .request(reqwest::Method::OPTIONS, &endpoint)
        .header("Origin", "https://app.example")
        .header("Access-Control-Request-Method", "POST")
        .send()
        .await
        .unwrap();
    assert!(preflight.status().is_success());
    assert_eq!(preflight.headers()["access-control-allow-origin"], "*");

    // A daemon that sells only over FIPS, and one that sells nothing.
    let fips_only = TestDaemon::start(rendezvous_addr(), "fips-only", &selling_config(false)).await;
    let fips_selling = fips_only
        .start_paid(FakeWallet::funded(&mint, 0), FakeReceiver::new(&mint))
        .await;
    let (fips_gateway, fips_server) =
        serve_http(fips_only.store.clone(), Some(fips_selling.clone())).await;
    let (free_gateway, free_server) = serve_http(fips_only.store.clone(), None).await;
    for base in [&fips_gateway, &free_gateway] {
        let response = http
            .post(format!("{base}/paid-blob"))
            .body(get(stored_hash).encode())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND, "{base}");
    }

    for task in [server, fips_server, free_server] {
        task.abort();
    }
    selling.shutdown().await;
    fips_selling.shutdown().await;
    provider.fips.shutdown().await;
    fips_only.fips.shutdown().await;
}
