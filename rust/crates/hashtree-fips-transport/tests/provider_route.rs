use std::net::{SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;

use fips_core::config::{RoutingMode, TransportInstances};
use fips_core::{Config, FipsEndpoint, UdpConfig};
use hashtree_core::{BlobReply, BlobRequest, BlobRoute, MemoryStore, Store};
use hashtree_fips_transport::{
    FipsBlobRoute, TcpBlobTransport, TcpBlobTransportConfig, TCP_BLOB_CAPABILITY,
    TCP_BLOB_SERVICE_PORT,
};
use sha2::{Digest, Sha256};
use tokio::time::timeout;

const CONVERGENCE_TIMEOUT: Duration = Duration::from_secs(10);

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
