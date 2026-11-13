//! End to end between FIPS endpoints: a client with a test wallet reads
//! through a `BlobRouter` from a provider that serves only against payment,
//! over the paid blob service and with the provider's ledger on disk.

mod common;

use std::io::ErrorKind;
use std::net::{SocketAddr, SocketAddrV4};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use common::{FakeMint, FakeReceiver, FakeWallet, Seller, MINT, TARIFF};
use fips_core::config::{RoutingMode, TransportInstances};
use fips_core::{Config, FipsEndpoint, PeerIdentity, UdpConfig};
use hashtree_core::{Hash, MemoryStore, Store};
use hashtree_network::{BlobRouteEntry, BlobRouter, BlobRouterConfig};
use hashtree_paid_retrieval::{
    discovered_paid_providers, AccountId, ClientPolicy, FileLedgerStore, FipsPaidBlobService,
    FipsPaidConfig, FipsPaidError, FipsPaidTransport, LimitedWallet, PaidBlobRoute, Reply, Request,
    SpendingLimit, PAID_BLOB_CAPABILITY, PAID_BLOB_SERVICE_PORT,
};
use tokio::time::timeout;

const CONVERGENCE_TIMEOUT: Duration = Duration::from_secs(10);
const TWO_MIB: usize = 2 * 1024 * 1024;

async fn endpoint(rendezvous_addr: SocketAddrV4, product_scope: &str) -> Arc<FipsEndpoint> {
    let mut config = Config::new();
    config.node.discovery.nostr.enabled = false;
    config.node.discovery.lan.enabled = false;
    config.node.discovery.local.rendezvous_addr = rendezvous_addr;
    config.node.routing.mode = RoutingMode::ReplyLearned;
    config.transports.udp = TransportInstances::Single(UdpConfig {
        bind_addr: Some("127.0.0.1:0".to_string()),
        advertise_on_nostr: Some(false),
        public: Some(false),
        ..UdpConfig::default()
    });
    Arc::new(
        FipsEndpoint::builder()
            .config(config)
            .discovery_scope(product_scope)
            .local_rendezvous()
            .without_system_tun()
            .bind()
            .await
            .unwrap(),
    )
}

fn rendezvous_addr() -> SocketAddrV4 {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    match socket.local_addr().unwrap() {
        SocketAddr::V4(addr) => addr,
        SocketAddr::V6(_) => unreachable!(),
    }
}

/// Wait until `client` sees `provider` advertise the paid blob service and
/// has a session with it.
async fn discover(client: &FipsEndpoint, provider: &FipsEndpoint) -> PeerIdentity {
    let npub = provider.npub().to_owned();
    let found = timeout(CONVERGENCE_TIMEOUT, async {
        loop {
            let providers = discovered_paid_providers(client).unwrap();
            let advertised = providers.into_iter().find(|peer| peer.npub() == npub);
            let peers = client.peers().await.unwrap();
            let connected = peers.iter().any(|peer| peer.npub == npub && peer.connected);
            match advertised.filter(|_| connected) {
                Some(peer) => break peer,
                None => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    });
    found.await.expect("the paid provider was not discovered")
}

/// Start the provider on its ledger. A provider that was stopped a moment
/// ago may still be letting go of it.
async fn open_seller(
    ledger: &Path,
    mint: &Arc<FakeMint>,
    receiver: &Arc<FakeReceiver>,
    grace_msat: u64,
) -> Seller {
    let opened = timeout(CONVERGENCE_TIMEOUT, async {
        loop {
            match FileLedgerStore::open(ledger) {
                Ok(store) => break Arc::new(store),
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    tokio::time::sleep(Duration::from_millis(20)).await
                }
                Err(error) => panic!("the ledger did not open: {error}"),
            }
        }
    });
    let ledger = opened.await.expect("the ledger stayed in use");
    Seller::durable(mint, receiver, ledger, |config| {
        config.grace_msat = grace_msat
    })
}

/// Bind the provider's service. The endpoint releases the port of a service
/// that was stopped a moment ago on its own time.
async fn serve(endpoint: &Arc<FipsEndpoint>, seller: &Seller) -> FipsPaidBlobService {
    let config = FipsPaidConfig::default();
    let bound = timeout(CONVERGENCE_TIMEOUT, async {
        loop {
            let provider = seller.provider.clone();
            match FipsPaidBlobService::serve_advertised(endpoint.clone(), provider, config, 100)
                .await
            {
                Ok(service) => break service,
                Err(FipsPaidError::Transport(_)) => {
                    tokio::time::sleep(Duration::from_millis(20)).await
                }
                Err(error) => panic!("the paid service did not bind: {error}"),
            }
        }
    });
    bound.await.expect("the service port stayed taken")
}

async fn ask(transport: &FipsPaidTransport, account: AccountId, hash: Hash) -> Reply {
    let request = Request::Get {
        hash: hash.into(),
        account,
        max_msat: u64::MAX,
    };
    Reply::decode(&transport.request(request.encode()).await.unwrap()).unwrap()
}

#[tokio::test]
async fn a_client_with_a_wallet_retrieves_blobs_from_a_paying_only_provider_over_fips() {
    let rendezvous = rendezvous_addr();
    let provider_endpoint = endpoint(rendezvous, "paid-provider").await;
    let client_endpoint = endpoint(rendezvous, "paid-client").await;

    let ledger = tempfile::tempdir().unwrap();
    let mint = Arc::new(FakeMint::default());
    let receiver = FakeReceiver::new(&mint);
    let seller = open_seller(ledger.path(), &mint, &receiver, 0).await;
    let (first, first_data) = seller.add(1, TWO_MIB).await;
    let (second, second_data) = seller.add(2, 300_000).await;
    let service = serve(&provider_endpoint, &seller).await;

    // The client finds the provider by its capability and pays through its
    // own wallet, held to a limit.
    let buyer = FipsPaidBlobService::client(client_endpoint.clone(), FipsPaidConfig::default())
        .await
        .unwrap();
    let provider_peer = discover(&client_endpoint, &provider_endpoint).await;
    let adverts = client_endpoint.local_instance_advertisements().unwrap();
    let advert = adverts
        .iter()
        .find(|advert| advert.npub == provider_endpoint.npub())
        .unwrap();
    let capability = advert.capability(PAID_BLOB_CAPABILITY).unwrap();
    assert_eq!(capability.fsp_port, Some(PAID_BLOB_SERVICE_PORT));
    // A paying-only provider does not offer the free blob service.
    assert!(advert.capability("hashtree.blob/1").is_none());

    let funded = FakeWallet::funded(&mint, 50);
    let limit = SpendingLimit::new(10);
    let wallet = Arc::new(LimitedWallet::new(funded.clone(), limit.clone()));
    let policy = ClientPolicy {
        max_tariff: TARIFF,
        mints: vec![MINT.to_owned()],
        topup_sat: 5,
        ..ClientPolicy::default()
    };
    let transport = Arc::new(buyer.transport_to(provider_peer));
    let paid = PaidBlobRoute::new("seller", transport, wallet, policy);
    let cache = Arc::new(MemoryStore::new());
    let router = BlobRouter::new(
        vec![BlobRouteEntry::new("paid:seller", Arc::new(paid.clone()))],
        Some(cache.clone()),
        BlobRouterConfig {
            request_timeout: Duration::from_secs(20),
            ..BlobRouterConfig::default()
        },
    )
    .unwrap();

    assert_eq!(
        router.get(&first, None).await.unwrap(),
        Some(first_data.clone())
    );
    assert_eq!(router.get(&second, None).await.unwrap(), Some(second_data));
    assert_eq!(router.get(&[9; 32], None).await.unwrap(), None);

    // One 5 sat top-up paid for both: 2048 msat and 1000 msat (the minimum).
    assert_eq!(funded.payments(), 1);
    assert_eq!((limit.spent_sat(), limit.remaining_sat()), (5, 5));
    assert_eq!(receiver.received(), 5);
    let account = paid.account_state();
    assert_eq!(
        (account.credited_msat, account.charged_msat),
        (5_000, 3_048)
    );
    assert_eq!((account.balance_msat, account.loss_msat()), (1_952, 0));
    assert_eq!(cache.get(&first).await.unwrap(), Some(first_data));

    // More requests at once than the provider seats one peer for: the
    // client queues them and none fails.
    let transport = buyer.transport_to(provider_peer);
    let mut requests = tokio::task::JoinSet::new();
    for _ in 0..12 {
        let transport = transport.clone();
        requests.spawn(async move { ask(&transport, [5; 32], [9; 32]).await });
    }
    while let Some(reply) = requests.join_next().await {
        assert!(matches!(reply.unwrap(), Reply::NoResult { .. }));
    }

    // The provider goes away without a goodbye and comes back on its ledger.
    drop(service);
    drop(seller);
    let seller = open_seller(ledger.path(), &mint, &receiver, 0).await;
    let sold = seller.provider.accounts();
    assert_eq!((sold[0].paid_msat, sold[0].charged_msat), (5_000, 3_048));
    let (third, third_data) = seller.add(3, 500_000).await;
    let service = serve(&provider_endpoint, &seller).await;

    // The prepaid balance still buys, with no new payment.
    assert_eq!(router.get(&third, None).await.unwrap(), Some(third_data));
    assert_eq!(funded.payments(), 1);
    assert_eq!(paid.account_state().balance_msat, 952);
    assert_eq!(receiver.redemptions(), 1);

    drop(router);
    drop(paid);
    buyer.shutdown().await.unwrap();
    service.shutdown().await.unwrap();
    client_endpoint.shutdown().await.unwrap();
    provider_endpoint.shutdown().await.unwrap();
}

#[tokio::test]
async fn grace_is_bound_to_the_peer_that_fips_authenticated() {
    let rendezvous = rendezvous_addr();
    let provider_endpoint = endpoint(rendezvous, "grace-provider").await;
    let first_endpoint = endpoint(rendezvous, "grace-first").await;
    let second_endpoint = endpoint(rendezvous, "grace-second").await;

    let ledger = tempfile::tempdir().unwrap();
    let mint = Arc::new(FakeMint::default());
    let receiver = FakeReceiver::new(&mint);
    let seller = open_seller(ledger.path(), &mint, &receiver, 1_500).await;
    let (hash, data) = seller.add(6, 4_096).await;
    let (other, _) = seller.add(7, 4_096).await;
    let service = serve(&provider_endpoint, &seller).await;

    let config = FipsPaidConfig::default();
    let first = FipsPaidBlobService::client(first_endpoint.clone(), config)
        .await
        .unwrap();
    let second = FipsPaidBlobService::client(second_endpoint.clone(), config)
        .await
        .unwrap();
    let first_peer = discover(&first_endpoint, &provider_endpoint).await;
    let first = first.transport_to(first_peer);
    let second_peer = discover(&second_endpoint, &provider_endpoint).await;
    let second = second.transport_to(second_peer);

    // An authenticated peer is served before it pays.
    let Reply::Data {
        balance_msat,
        data: served,
        ..
    } = ask(&first, [1; 32], hash).await
    else {
        panic!("grace covers the first blob");
    };
    assert_eq!((balance_msat, served), (-1_000, data));
    // Its debt counts, and a second account gives the same peer nothing.
    let Reply::Quote(quote) = ask(&first, [1; 32], other).await else {
        panic!("grace is exhausted");
    };
    assert_eq!((quote.balance_msat, quote.grace_msat), (-1_000, 1_500));
    let Reply::Quote(quote) = ask(&first, [2; 32], other).await else {
        panic!("one grace per peer");
    };
    assert_eq!(quote.grace_msat, 0);
    // Another peer has its own.
    assert!(matches!(
        ask(&second, [3; 32], other).await,
        Reply::Data {
            balance_msat: -1_000,
            ..
        }
    ));

    // The binding outlives a provider restart: no second grace for a peer.
    service.shutdown().await.unwrap();
    drop(seller);
    let seller = open_seller(ledger.path(), &mint, &receiver, 1_500).await;
    seller.add(7, 4_096).await;
    let service = serve(&provider_endpoint, &seller).await;
    let Reply::Quote(quote) = ask(&first, [2; 32], other).await else {
        panic!("one grace per peer, across restarts");
    };
    assert_eq!(quote.grace_msat, 0);
    let Reply::Quote(quote) = ask(&first, [1; 32], other).await else {
        panic!("the debt is still owed");
    };
    assert_eq!((quote.balance_msat, quote.grace_msat), (-1_000, 1_500));

    service.shutdown().await.unwrap();
    for endpoint in [first_endpoint, second_endpoint, provider_endpoint] {
        endpoint.shutdown().await.unwrap();
    }
}
