use std::net::{SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;

use fips_core::config::{RoutingMode, TransportInstances};
use fips_core::discovery::local::LocalInstanceCapability;
use fips_core::{Config, FipsEndpoint, PeerIdentity, UdpConfig};
use hashtree_core::{Hash, MemoryStore, Store};
use hashtree_fips_transport::{
    SameHostBlobStore, SameHostBlobStoreConfig, SameHostBlobStoreError, TcpBlobTransport,
    TcpBlobTransportConfig, TCP_BLOB_CAPABILITY, TCP_BLOB_SERVICE_PORT,
};
use sha2::{Digest, Sha256};
use tokio::time::timeout;

const CONVERGENCE_TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::test]
async fn standalone_store_needs_no_provider_and_preserves_local_mutations() {
    let endpoint = endpoint(rendezvous_addr(), "iris-chat-device-sync-v1").await;
    let local = Arc::new(MemoryStore::new());
    let store = SameHostBlobStore::bind(
        endpoint.clone(),
        local.clone(),
        SameHostBlobStoreConfig::default(),
    )
    .await
    .expect("bind standalone store");
    let data = b"standalone content remains ordinary Hashtree data".to_vec();
    let content_hash = hash(&data);

    assert!(store.put(content_hash, data.clone()).await.unwrap());
    assert_eq!(store.get(&content_hash).await.unwrap(), Some(data));
    assert!(store.has(&content_hash).await.unwrap());
    assert_eq!(store.get(&[0x55; 32]).await.unwrap(), None);
    assert!(store.delete(&content_hash).await.unwrap());
    assert_eq!(local.get(&content_hash).await.unwrap(), None);

    drop(store);
    endpoint.shutdown().await.expect("shutdown endpoint");
}

#[tokio::test]
async fn rejects_provider_fanout_above_the_actor_bound() {
    let endpoint = endpoint(rendezvous_addr(), "bounded-consumer-v1").await;
    let result = SameHostBlobStore::bind(
        endpoint.clone(),
        Arc::new(MemoryStore::new()),
        SameHostBlobStoreConfig::default().with_max_provider_attempts(5),
    )
    .await;

    assert!(matches!(
        result,
        Err(SameHostBlobStoreError::TooManyProviderAttempts(5))
    ));
    endpoint.shutdown().await.unwrap();
}

#[tokio::test]
async fn accepts_a_platform_erased_local_store_without_an_adapter_enum() {
    let endpoint = endpoint(rendezvous_addr(), "iris-chat-platform-store-v1").await;
    let local: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let store =
        SameHostBlobStore::bind(endpoint.clone(), local, SameHostBlobStoreConfig::default())
            .await
            .expect("bind erased store");
    let data = b"desktop LMDB or mobile memory behind one Store slot".to_vec();
    let content_hash = hash(&data);

    assert!(store.put(content_hash, data.clone()).await.unwrap());
    assert_eq!(store.get(&content_hash).await.unwrap(), Some(data));

    drop(store);
    endpoint.shutdown().await.expect("shutdown endpoint");
}

#[tokio::test]
async fn nonadvertised_client_store_rejects_inbound_blob_reads() {
    let rendezvous = rendezvous_addr();
    let private_endpoint = endpoint(rendezvous, "iris-chat-private-store-v1").await;
    let requester_endpoint = endpoint(rendezvous, "requester-v1").await;
    let private_local = Arc::new(MemoryStore::new());
    let data = b"a private client cache must not become a server".to_vec();
    let content_hash = hash(&data);
    private_local
        .put(content_hash, data)
        .await
        .expect("seed private cache");
    let private = SameHostBlobStore::bind(
        private_endpoint.clone(),
        private_local,
        SameHostBlobStoreConfig::default(),
    )
    .await
    .expect("bind private store");
    let requester = TcpBlobTransport::bind_with_config(
        requester_endpoint.clone(),
        Arc::new(MemoryStore::new()),
        TcpBlobTransportConfig {
            idle_timeout: Duration::from_millis(200),
        },
    )
    .await
    .expect("bind explicit requester");
    wait_for_connection(&requester_endpoint, private_endpoint.npub()).await;

    let private_peer = PeerIdentity::from_npub(private_endpoint.npub()).unwrap();
    assert!(
        requester
            .fetch_from_peer(&content_hash, private_peer)
            .await
            .is_err(),
        "client-private store served an inbound request"
    );

    requester.shutdown().await.unwrap();
    drop(private);
    requester_endpoint.shutdown().await.unwrap();
    private_endpoint.shutdown().await.unwrap();
}

#[tokio::test]
async fn discovers_provider_across_product_scopes_caches_blob_and_falls_back_after_loss() {
    let rendezvous = rendezvous_addr();
    let provider_endpoint = endpoint(rendezvous, "iris-drive-sync-v1").await;
    let consumer_endpoint = endpoint(rendezvous, "iris-chat-device-sync-v1").await;
    let provider_local = Arc::new(MemoryStore::new());
    let consumer_local = Arc::new(MemoryStore::new());
    let data = b"one userspace Hashtree service, two independent products".to_vec();
    let content_hash = hash(&data);
    provider_local
        .put(content_hash, data.clone())
        .await
        .expect("seed provider");

    let provider = SameHostBlobStore::bind(
        provider_endpoint.clone(),
        provider_local,
        SameHostBlobStoreConfig::provider(100),
    )
    .await
    .expect("bind advertised provider");
    let consumer = SameHostBlobStore::bind(
        consumer_endpoint.clone(),
        consumer_local.clone(),
        SameHostBlobStoreConfig::default(),
    )
    .await
    .expect("bind consumer");

    wait_for_provider(&consumer_endpoint, provider_endpoint.npub()).await;
    assert_eq!(
        consumer.get(&content_hash).await.unwrap(),
        Some(data.clone())
    );
    assert_eq!(consumer_local.get(&content_hash).await.unwrap(), Some(data));

    drop(provider);
    wait_for_no_provider(&consumer_endpoint).await;
    assert_eq!(
        consumer.get(&[0x77; 32]).await.unwrap(),
        None,
        "provider withdrawal must restore ordinary standalone missing semantics"
    );
    assert!(consumer.has(&content_hash).await.unwrap());

    drop(consumer);
    consumer_endpoint
        .shutdown()
        .await
        .expect("shutdown consumer endpoint");
    provider_endpoint
        .shutdown()
        .await
        .expect("shutdown provider endpoint");
}

#[tokio::test]
async fn healthy_provider_repairs_a_corrupt_local_cache_entry() {
    let rendezvous = rendezvous_addr();
    let provider_endpoint = endpoint(rendezvous, "iris-drive-repair-provider-v1").await;
    let consumer_endpoint = endpoint(rendezvous, "iris-chat-repair-consumer-v1").await;
    let provider_local = Arc::new(MemoryStore::new());
    let consumer_local = Arc::new(MemoryStore::new());
    let data = b"provider copy repairs corrupt local bytes".to_vec();
    let content_hash = hash(&data);
    provider_local
        .put(content_hash, data.clone())
        .await
        .expect("seed provider");
    consumer_local
        .put(content_hash, b"corrupt".to_vec())
        .await
        .expect("seed corrupt cache");
    consumer_local
        .pin(&content_hash)
        .await
        .expect("pin corrupt cache once");
    consumer_local
        .pin(&content_hash)
        .await
        .expect("pin corrupt cache twice");
    let provider = SameHostBlobStore::bind(
        provider_endpoint.clone(),
        provider_local,
        SameHostBlobStoreConfig::provider(100),
    )
    .await
    .expect("bind provider");
    let consumer = SameHostBlobStore::bind(
        consumer_endpoint.clone(),
        consumer_local.clone(),
        SameHostBlobStoreConfig::default(),
    )
    .await
    .expect("bind consumer");
    wait_for_provider(&consumer_endpoint, provider_endpoint.npub()).await;

    assert_eq!(
        consumer.get(&content_hash).await.unwrap(),
        Some(data.clone())
    );
    assert_eq!(consumer_local.get(&content_hash).await.unwrap(), Some(data));
    assert_eq!(
        consumer_local.pin_count(&content_hash),
        2,
        "repair must not silently unpin a referenced blob"
    );

    drop(consumer);
    drop(provider);
    consumer_endpoint.shutdown().await.unwrap();
    provider_endpoint.shutdown().await.unwrap();
}

#[tokio::test]
async fn provider_withdrawal_during_request_is_failure_then_later_standalone_miss() {
    let rendezvous = rendezvous_addr();
    let provider_endpoint = endpoint(rendezvous, "withdrawing-provider-v1").await;
    let consumer_endpoint = endpoint(rendezvous, "iris-chat-withdrawal-v1").await;
    let stalled_receiver = provider_endpoint
        .register_service_receiver_with_capability(LocalInstanceCapability::service(
            TCP_BLOB_CAPABILITY,
            TCP_BLOB_SERVICE_PORT,
        ))
        .await
        .expect("advertise stalled provider");
    let consumer = Arc::new(
        SameHostBlobStore::bind(
            consumer_endpoint.clone(),
            Arc::new(MemoryStore::new()),
            SameHostBlobStoreConfig::default().with_transport(TcpBlobTransportConfig {
                idle_timeout: Duration::from_millis(250),
            }),
        )
        .await
        .expect("bind consumer"),
    );
    wait_for_provider(&consumer_endpoint, provider_endpoint.npub()).await;
    let missing = [0xa5; 32];
    let request = {
        let consumer = consumer.clone();
        tokio::spawn(async move { consumer.get(&missing).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(stalled_receiver);
    wait_for_no_provider(&consumer_endpoint).await;

    assert!(
        request.await.expect("request task").is_err(),
        "provider death during an attempted read became a false miss"
    );
    assert_eq!(consumer.get(&missing).await.unwrap(), None);

    drop(consumer);
    consumer_endpoint.shutdown().await.unwrap();
    provider_endpoint.shutdown().await.unwrap();
}

#[tokio::test]
async fn healthy_provider_wins_without_hol_and_mixed_miss_failure_is_not_a_false_miss() {
    let rendezvous = rendezvous_addr();
    let stalled_endpoint = endpoint(rendezvous, "stalled-provider").await;
    let healthy_endpoint = endpoint(rendezvous, "healthy-provider").await;
    let consumer_endpoint = endpoint(rendezvous, "iris-drive-sync-v1").await;
    let stalled_receiver = stalled_endpoint
        .register_service_receiver_with_capability(
            LocalInstanceCapability::service(TCP_BLOB_CAPABILITY, TCP_BLOB_SERVICE_PORT)
                .with_priority(200),
        )
        .await
        .expect("advertise deliberately stalled provider");
    let healthy_local = Arc::new(MemoryStore::new());
    let data = b"lower priority healthy provider must not wait behind a stalled hint".to_vec();
    let content_hash = hash(&data);
    healthy_local
        .put(content_hash, data.clone())
        .await
        .expect("seed healthy provider");
    let transport = TcpBlobTransportConfig {
        idle_timeout: Duration::from_millis(750),
    };
    let healthy = SameHostBlobStore::bind(
        healthy_endpoint.clone(),
        healthy_local,
        SameHostBlobStoreConfig::provider(100).with_transport(transport),
    )
    .await
    .expect("bind healthy provider");
    let consumer = SameHostBlobStore::bind(
        consumer_endpoint.clone(),
        Arc::new(MemoryStore::new()),
        SameHostBlobStoreConfig::default().with_transport(transport),
    )
    .await
    .expect("bind consumer");

    wait_for_provider_count(&consumer_endpoint, 2).await;
    assert_eq!(
        timeout(Duration::from_secs(3), consumer.get(&content_hash))
            .await
            .expect("healthy fetch blocked behind stalled provider")
            .expect("healthy provider fetch failed"),
        Some(data)
    );

    let missing = [0x99; 32];
    assert!(
        consumer.get(&missing).await.is_err(),
        "one explicit miss plus one failed advertised provider must remain a failure"
    );
    drop(stalled_receiver);
    wait_for_provider_count(&consumer_endpoint, 1).await;
    assert_eq!(consumer.get(&missing).await.unwrap(), None);

    drop(consumer);
    drop(healthy);
    consumer_endpoint.shutdown().await.unwrap();
    healthy_endpoint.shutdown().await.unwrap();
    stalled_endpoint.shutdown().await.unwrap();
}

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
            .expect("bind FIPS endpoint"),
    )
}

async fn wait_for_provider(endpoint: &FipsEndpoint, npub: &str) {
    timeout(CONVERGENCE_TIMEOUT, async {
        loop {
            let adverts = endpoint
                .local_instance_advertisements()
                .expect("capability snapshot");
            let advertised = adverts.iter().any(|advert| {
                advert.npub == npub
                    && advert
                        .capability(TCP_BLOB_CAPABILITY)
                        .and_then(|capability| capability.fsp_port)
                        == Some(TCP_BLOB_SERVICE_PORT)
            });
            let connected = endpoint
                .peers()
                .await
                .expect("peer snapshot")
                .iter()
                .any(|peer| peer.npub == npub && peer.connected);
            if advertised && connected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("provider did not advertise and connect");
}

fn rendezvous_addr() -> SocketAddrV4 {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("reserve rendezvous port");
    match socket.local_addr().expect("rendezvous address") {
        SocketAddr::V4(addr) => addr,
        SocketAddr::V6(_) => unreachable!("IPv4 loopback bind returned IPv6"),
    }
}

async fn wait_for_connection(endpoint: &FipsEndpoint, npub: &str) {
    timeout(CONVERGENCE_TIMEOUT, async {
        loop {
            if endpoint
                .peers()
                .await
                .expect("peer snapshot")
                .iter()
                .any(|peer| peer.npub == npub && peer.connected)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("local FIPS peers did not connect");
}

async fn wait_for_provider_count(endpoint: &FipsEndpoint, count: usize) {
    timeout(CONVERGENCE_TIMEOUT, async {
        loop {
            let adverts = endpoint
                .local_instance_advertisements()
                .expect("capability snapshot");
            let matching = adverts
                .iter()
                .filter(|advert| {
                    advert
                        .capability(TCP_BLOB_CAPABILITY)
                        .and_then(|capability| capability.fsp_port)
                        == Some(TCP_BLOB_SERVICE_PORT)
                })
                .count();
            if matching == count {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("expected {count} providers"));
}

async fn wait_for_no_provider(endpoint: &FipsEndpoint) {
    wait_for_provider_count(endpoint, 0).await;
}

fn hash(data: &[u8]) -> Hash {
    Sha256::digest(data).into()
}
