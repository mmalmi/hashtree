use std::net::{SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;

use fips_core::config::{RoutingMode, TransportInstances};
use fips_core::{Config, FipsEndpoint, UdpConfig};
use hashtree_core::{BlobReply, BlobRequest, BlobRoute, BlobRouteContext, MemoryStore, Store};
use hashtree_fips_transport::{
    FipsBlobRoute, TcpBlobTransport, TcpBlobTransportConfig, TCP_BLOB_CAPABILITY,
    TCP_BLOB_SERVICE_PORT,
};
use sha2::{Digest, Sha256};
use tokio::time::timeout;

const CONVERGENCE_TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::test]
async fn retrying_hash_reaches_provider_beyond_attempt_window_despite_other_reads() {
    let rendezvous = rendezvous_addr();
    let consumer_endpoint = endpoint(rendezvous, "retry-consumer").await;
    let data = b"only the fifth authorized provider has this verified blob".to_vec();
    let hash = Sha256::digest(&data).into();
    let mut providers = Vec::new();
    let mut endpoints = Vec::new();
    for index in 0..5 {
        let provider_endpoint = endpoint(rendezvous, &format!("retry-provider-{index}")).await;
        let store = Arc::new(MemoryStore::new());
        if index == 4 {
            store.put(hash, data.clone()).await.unwrap();
        }
        providers.push(
            TcpBlobTransport::bind_advertised_with_config(
                provider_endpoint.clone(),
                store,
                TcpBlobTransportConfig::default(),
                100,
            )
            .await
            .unwrap(),
        );
        endpoints.push(provider_endpoint);
    }
    let consumer_store = Arc::new(MemoryStore::new());
    let consumer_transport = Arc::new(
        TcpBlobTransport::bind_client_with_config(
            consumer_endpoint.clone(),
            consumer_store.clone(),
            TcpBlobTransportConfig::default(),
        )
        .await
        .unwrap(),
    );
    for provider_endpoint in &endpoints {
        wait_for_provider(&consumer_endpoint, provider_endpoint.npub()).await;
    }
    // Explicit order makes the excluded holder deterministic; discovery is
    // covered separately and must not accidentally put it in the first four.
    let peers = endpoints
        .iter()
        .map(|endpoint| fips_core::PeerIdentity::from_npub(endpoint.npub()).unwrap())
        .collect();
    let route = FipsBlobRoute::explicit(consumer_transport.clone(), peers, 4).unwrap();
    let context = || BlobRouteContext {
        deadline: std::time::Instant::now() + Duration::from_secs(5),
        attempt_budget: 4,
    };
    let first = route
        .route_with_context(BlobRequest { hash, htl: 0 }, context())
        .await;
    let initially_cached = consumer_store.get(&hash).await.unwrap();
    let mut intervening = Vec::new();
    // A single global cursor advances a complete cycle after these four
    // other hashes and would omit the same holder on the target's retry.
    for index in 0..4 {
        let other_hash = Sha256::digest(format!("unrelated missing blob {index}")).into();
        intervening.push(
            route
                .route_with_context(
                    BlobRequest {
                        hash: other_hash,
                        htl: 0,
                    },
                    context(),
                )
                .await,
        );
    }
    let retried = route
        .route_with_context(BlobRequest { hash, htl: 0 }, context())
        .await;

    // Preserve teardown even for the deliberately failing pre-fix result.
    drop(route);
    // First-valid completion aborts losing hedges. Give their canceled futures
    // a chance to drop their transport references before consuming the actor.
    let consumer_transport = timeout(CONVERGENCE_TIMEOUT, async {
        let mut transport = consumer_transport;
        loop {
            match Arc::try_unwrap(transport) {
                Ok(transport) => break transport,
                Err(pending) => transport = pending,
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("canceled provider hedges retained the client transport");
    consumer_transport.shutdown().await.unwrap();
    for provider in providers {
        provider.shutdown().await.unwrap();
    }
    consumer_endpoint.shutdown().await.unwrap();
    for provider_endpoint in endpoints {
        provider_endpoint.shutdown().await.unwrap();
    }

    assert!(
        first.is_err(),
        "untried providers were reported as a complete miss"
    );
    assert_eq!(initially_cached, None);
    assert!(
        intervening.iter().all(Result::is_err),
        "an incomplete search must not become a miss"
    );
    assert_eq!(retried.unwrap(), BlobReply::Data(data.clone()));
}

#[tokio::test]
async fn discovered_provider_death_and_replacement_recover_on_one_composite_route() {
    let rendezvous = rendezvous_addr();
    let first_endpoint = endpoint(rendezvous, "first-provider-product").await;
    let consumer_endpoint = endpoint(rendezvous, "consumer-product").await;
    let data = b"replacement provider serves the same immutable blob".to_vec();
    let hash = Sha256::digest(&data).into();
    let first_store = Arc::new(MemoryStore::new());
    first_store.put(hash, data.clone()).await.unwrap();
    let first = TcpBlobTransport::bind_advertised_with_config(
        first_endpoint.clone(),
        first_store,
        TcpBlobTransportConfig::default(),
        100,
    )
    .await
    .unwrap();

    let consumer_store = Arc::new(MemoryStore::new());
    let consumer_transport = Arc::new(
        TcpBlobTransport::bind_client_with_config(
            consumer_endpoint.clone(),
            consumer_store,
            TcpBlobTransportConfig::default(),
        )
        .await
        .unwrap(),
    );
    let route = FipsBlobRoute::discovered(consumer_endpoint.clone(), consumer_transport.clone(), 4)
        .unwrap();

    wait_for_provider(&consumer_endpoint, first_endpoint.npub()).await;
    assert_eq!(
        route.route(BlobRequest { hash, htl: 0 }).await.unwrap(),
        BlobReply::Data(data.clone()),
    );

    drop(first);
    wait_for_provider_count(&consumer_endpoint, 0).await;
    assert_eq!(
        route
            .route(BlobRequest {
                hash: [0x55; 32],
                htl: 0,
            })
            .await
            .unwrap(),
        BlobReply::NoResult,
    );

    let replacement_store = Arc::new(MemoryStore::new());
    replacement_store.put(hash, data.clone()).await.unwrap();
    let replacement = TcpBlobTransport::bind_advertised_with_config(
        first_endpoint.clone(),
        replacement_store,
        TcpBlobTransportConfig::default(),
        100,
    )
    .await
    .unwrap();
    wait_for_provider(&consumer_endpoint, first_endpoint.npub()).await;
    assert_eq!(
        route.route(BlobRequest { hash, htl: 0 }).await.unwrap(),
        BlobReply::Data(data),
    );
    assert_eq!(
        route.discovered_provider_ids().unwrap(),
        vec![first_endpoint.npub().to_string()],
    );

    drop(replacement);
    drop(route);
    drop(consumer_transport);
    consumer_endpoint.shutdown().await.unwrap();
    first_endpoint.shutdown().await.unwrap();
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
            .unwrap(),
    )
}

async fn wait_for_provider(endpoint: &FipsEndpoint, npub: &str) {
    timeout(CONVERGENCE_TIMEOUT, async {
        loop {
            let advertised = endpoint
                .local_instance_advertisements()
                .unwrap()
                .iter()
                .any(|advert| {
                    advert.npub == npub
                        && advert
                            .capability(TCP_BLOB_CAPABILITY)
                            .and_then(|capability| capability.fsp_port)
                            == Some(TCP_BLOB_SERVICE_PORT)
                });
            let connected = endpoint
                .peers()
                .await
                .unwrap()
                .iter()
                .any(|peer| peer.npub == npub && peer.connected);
            if advertised && connected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "provider {npub} did not converge; adverts={:?}",
            endpoint.local_instance_advertisements()
        )
    });
}

async fn wait_for_provider_count(endpoint: &FipsEndpoint, count: usize) {
    timeout(CONVERGENCE_TIMEOUT, async {
        loop {
            let matching = endpoint
                .local_instance_advertisements()
                .unwrap()
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
    .unwrap();
}

fn rendezvous_addr() -> SocketAddrV4 {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    match socket.local_addr().unwrap() {
        SocketAddr::V4(addr) => addr,
        SocketAddr::V6(_) => unreachable!(),
    }
}
