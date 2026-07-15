//! Hashtree daemon integration for the embedded FIPS endpoint API.
//!
//! This starts `fips_core::FipsEndpoint` inside the htree process; it does not
//! depend on or talk to an external FIPS daemon.

use crate::config::{Config, NostrEventTransport};
#[cfg(feature = "experimental-decentralized-pubsub")]
use crate::nostr_relay::NostrRelay;
use crate::storage::{HashtreeStore, StorageRouter};
use anyhow::{Context, Result};
use hashtree_core::BlobRoute;
use hashtree_fips_transport::{
    bind_fips_endpoint, BoundFipsEndpoint, FipsEndpointOptions, FipsPeerConfig,
    HashtreeFipsTransport, PeerIdentity, TcpBlobTransport, TcpBlobTransportConfig,
    DEFAULT_FIPS_DISCOVERY_SCOPE,
};
use hashtree_network::{
    blob_resolver, BlobResolver, MeshReadSource, MeshRoutingConfig, NamedBlobRoute,
};
use nostr::nips::nip19::ToBech32;
use nostr::PublicKey;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;
#[cfg(feature = "experimental-decentralized-pubsub")]
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

pub type DaemonFipsTransport = HashtreeFipsTransport<StorageRouter>;
pub type DaemonBlobResolver = BlobResolver<StorageRouter>;
type DaemonBlobTransport = TcpBlobTransport<StorageRouter>;

const DAEMON_SAME_HOST_PROVIDER_PRIORITY: i16 = 100;

pub struct DaemonFipsHandle {
    pub transport: Arc<DaemonFipsTransport>,
    pub endpoint_npub: String,
    pub discovery_scope: String,
    pub nostr_provider: Option<Arc<dyn nostr_pubsub::PubsubProvider>>,
    pub blob_resolver: Arc<DaemonBlobResolver>,
    blob_transport: Mutex<Option<Arc<DaemonBlobTransport>>>,
    receiver_task: JoinHandle<()>,
}

impl DaemonFipsHandle {
    pub fn shutdown(&self) {
        self.receiver_task.abort();
        if let Ok(mut transport) = self.blob_transport.lock() {
            transport.take();
        }
    }
}

#[cfg(feature = "experimental-decentralized-pubsub")]
pub struct DaemonNostrPubsubHandle {
    mesh: Arc<hashtree_fips_transport::FipsMeshPubsub<StorageRouter>>,
    relay: Arc<NostrRelay>,
    ingest_task: JoinHandle<()>,
    outbound_task: JoinHandle<()>,
}

#[cfg(feature = "experimental-decentralized-pubsub")]
impl DaemonNostrPubsubHandle {
    pub fn shutdown(&self) {
        self.relay.set_decentralized_pubsub_sender(None);
        self.ingest_task.abort();
        self.outbound_task.abort();
        self.mesh.shutdown();
    }
}

pub async fn start_daemon_fips_transport(
    config: &Config,
    keys: &nostr::Keys,
    store: Arc<HashtreeStore>,
    peer_ids: Vec<String>,
) -> Result<Option<DaemonFipsHandle>> {
    if !config.server.enable_fips || !config.server.mode.hash_get_enabled() {
        return Ok(None);
    }

    let active_relays = config.nostr.active_relays();
    let relays = config.server.resolved_fips_relays(&active_relays);
    let discovery_scope = normalized_discovery_scope(&config.server.fips_discovery_scope);
    let peer_configs = daemon_fips_peer_configs(config, peer_ids);
    let identity_nsec = keys
        .secret_key()
        .to_bech32()
        .context("Failed to encode daemon identity for FIPS endpoint")?;
    let mut options = FipsEndpointOptions::new(identity_nsec);
    options.discovery_scope = discovery_scope.clone();
    options.relays = relays;
    options.enable_udp = config.server.enable_fips_udp;
    options.enable_webrtc = config.server.enable_fips_webrtc;
    options.enable_local_rendezvous = true;
    options.ethernet_interfaces = config.server.fips_ethernet_interfaces.clone();
    options.udp_bind_addr = config.server.fips_udp_bind_addr.clone();
    options.udp_public = config.server.fips_udp_public;
    options.udp_external_addr = config.server.fips_udp_external_addr.clone();
    // A configured/followed peer may be WebRTC-only. Let this endpoint dial
    // discovered WebRTC adverts so connectivity does not depend on the remote
    // side winning the offer race.
    options.webrtc_auto_connect = options.enable_webrtc && !peer_configs.is_empty();
    options.webrtc_max_connections = hashtree_fips_transport::DEFAULT_FIPS_WEBRTC_MAX_CONNECTIONS;
    options.open_discovery_max_pending = 0;
    options.packet_channel_capacity = 1024;
    let endpoint = bind_fips_endpoint(options)
        .await
        .context("Failed to start FIPS endpoint")?;

    let request_timeout = Duration::from_millis(config.server.fips_request_timeout_ms.max(1));
    let nostr_provider: Option<Arc<dyn nostr_pubsub::PubsubProvider>> =
        if config.nostr.event_transport == NostrEventTransport::FipsLocalOnly {
            let options = nostr_pubsub_fips::FipsPubsubClientOptions {
                query_timeout: request_timeout,
                ..Default::default()
            };
            let client = nostr_pubsub_fips::FipsPubsubClient::start(
                endpoint.native_endpoint.clone(),
                options,
            )
            .await
            .context("Failed to start local-only FIPS Nostr pubsub provider")?;
            Some(Arc::new(client))
        } else {
            None
        };
    let transport = Arc::new(
        HashtreeFipsTransport::new(endpoint.endpoint.clone(), store.store_arc())
            .with_request_timeout(request_timeout)
            .with_cache_responses(false),
    );
    let (blob_resolver, blob_transport) =
        bind_daemon_blob_resolver(&endpoint, store.store_arc(), &peer_configs, request_timeout)
            .await?;
    if !peer_configs.is_empty() {
        transport.set_peer_configs(peer_configs).await;
    }
    let receiver_task = transport.start();

    Ok(Some(DaemonFipsHandle {
        transport,
        endpoint_npub: endpoint.local_peer_id,
        discovery_scope: endpoint.discovery_scope,
        nostr_provider,
        blob_resolver,
        blob_transport: Mutex::new(Some(blob_transport)),
        receiver_task,
    }))
}

async fn bind_daemon_blob_resolver(
    endpoint: &BoundFipsEndpoint,
    store: Arc<StorageRouter>,
    peers: &[FipsPeerConfig],
    request_timeout: Duration,
) -> Result<(Arc<DaemonBlobResolver>, Arc<DaemonBlobTransport>)> {
    let resolver = Arc::new(blob_resolver(
        store.clone(),
        endpoint.local_peer_id.clone(),
        request_timeout,
        MeshRoutingConfig::default(),
    ));
    let transport = Arc::new(
        TcpBlobTransport::bind_advertised_route_with_config(
            endpoint.native_endpoint.clone(),
            store,
            resolver.clone(),
            TcpBlobTransportConfig::default(),
            DAEMON_SAME_HOST_PROVIDER_PRIORITY,
        )
        .await
        .context("Failed to advertise htree's blob resolver")?,
    );
    let routes: Vec<Arc<dyn MeshReadSource>> = peers
        .iter()
        .filter_map(|peer| {
            let identity = PeerIdentity::from_npub(&peer.npub).ok()?;
            let route: Arc<dyn BlobRoute> = Arc::new(transport.route_to(identity));
            Some(
                Arc::new(NamedBlobRoute::mesh_peer(peer.npub.clone(), route))
                    as Arc<dyn MeshReadSource>,
            )
        })
        .collect();
    resolver.set_read_sources(routes).await;
    Ok((resolver, transport))
}

pub async fn start_daemon_nostr_provider(
    config: &Config,
    fips_handle: Option<&DaemonFipsHandle>,
) -> Result<Option<Arc<dyn nostr_pubsub::PubsubProvider>>> {
    match config.nostr.event_transport {
        NostrEventTransport::Relay => {
            let relays = config.nostr.active_relays();
            if relays.is_empty() {
                return Ok(None);
            }
            let event_bus = nostr_pubsub_relay::RelayEventBus::new(
                relays,
                Duration::from_millis(config.server.fips_request_timeout_ms.max(1)),
            )
            .await
            .context("Failed to start Nostr relay event provider")?;
            Ok(Some(Arc::new(event_bus)))
        }
        NostrEventTransport::FipsLocalOnly => fips_handle
            .and_then(|handle| handle.nostr_provider.clone())
            .map(Some)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "nostr.event_transport=fips-local-only requires the local FIPS Nostr pubsub provider"
                )
            }),
    }
}

#[cfg(feature = "experimental-decentralized-pubsub")]
pub async fn start_daemon_nostr_pubsub(
    config: &Config,
    fips_handle: Option<&DaemonFipsHandle>,
    store: Arc<HashtreeStore>,
    relay: Option<Arc<NostrRelay>>,
) -> Result<Option<Arc<DaemonNostrPubsubHandle>>> {
    if !daemon_decentralized_pubsub_ready(config, fips_handle.is_some(), relay.is_some()) {
        return Ok(None);
    }

    let fips_handle = fips_handle.expect("checked by daemon_decentralized_pubsub_ready");
    let relay = relay.expect("checked by daemon_decentralized_pubsub_ready");
    let request_timeout = Duration::from_millis(config.server.fips_request_timeout_ms.max(1));
    let mesh = Arc::new(
        fips_handle
            .transport
            .start_mesh_pubsub_with_options(
                store.store_arc(),
                fips_handle.endpoint_npub.clone(),
                request_timeout,
                hashtree_fips_transport::FipsMeshPubsubOptions {
                    forwarding: config.nostr.decentralized_pubsub_forwarding,
                    fanout: config.nostr.decentralized_pubsub_fanout,
                    max_hops: config.nostr.decentralized_pubsub_max_hops,
                },
            )
            .await
            .context("Failed to start decentralized Nostr pubsub over FIPS")?,
    );
    let _ = mesh
        .subscribe_pubsub(crate::nostr_pubsub::NOSTR_EVENT_PUBSUB_STREAM)
        .await;

    let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
    relay.set_decentralized_pubsub_sender(Some(outbound_tx));
    let max_event_bytes = config.nostr.decentralized_pubsub_max_event_bytes;
    let ingest_task =
        spawn_daemon_nostr_pubsub_ingest(Arc::clone(&mesh), Arc::clone(&relay), max_event_bytes);
    let outbound_task =
        spawn_daemon_nostr_pubsub_outbound(Arc::clone(&mesh), outbound_rx, max_event_bytes);

    Ok(Some(Arc::new(DaemonNostrPubsubHandle {
        mesh,
        relay,
        ingest_task,
        outbound_task,
    })))
}

#[cfg(feature = "experimental-decentralized-pubsub")]
fn daemon_decentralized_pubsub_ready(config: &Config, has_fips: bool, has_relay: bool) -> bool {
    config.nostr.decentralized_pubsub_enabled() && has_fips && has_relay
}

#[cfg(feature = "experimental-decentralized-pubsub")]
fn spawn_daemon_nostr_pubsub_ingest(
    mesh: Arc<hashtree_fips_transport::FipsMeshPubsub<StorageRouter>>,
    relay: Arc<NostrRelay>,
    max_event_bytes: usize,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let delivery = mesh.recv_pubsub_event().await;
            let stream_id = delivery.stream_id.clone();
            let origin_peer_id = delivery.origin_peer_id.clone();
            match crate::nostr_pubsub::ingest_nostr_pubsub_payload_with_limit(
                &relay,
                &delivery.stream_id,
                &delivery.payload,
                max_event_bytes,
            )
            .await
            {
                Ok(Some(event)) => {
                    tracing::debug!(
                        stream_id,
                        origin_peer_id,
                        event_id = %event.id.to_hex(),
                        "ingested decentralized Nostr pubsub event"
                    );
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!(
                        stream_id,
                        origin_peer_id,
                        "nostr decentralized pubsub ingest failed: {err:#}"
                    );
                }
            }
        }
    })
}

#[cfg(feature = "experimental-decentralized-pubsub")]
fn spawn_daemon_nostr_pubsub_outbound(
    mesh: Arc<hashtree_fips_transport::FipsMeshPubsub<StorageRouter>>,
    mut outbound_rx: mpsc::UnboundedReceiver<nostr::Event>,
    max_event_bytes: usize,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut seq = 1u64;
        while let Some(event) = outbound_rx.recv().await {
            let event_id = event.id.to_hex();
            match crate::nostr_pubsub::publish_fips_nostr_event_with_limit(
                mesh.as_ref(),
                seq,
                &event,
                max_event_bytes,
            )
            .await
            {
                Ok(stats) => {
                    tracing::debug!(
                        event_id,
                        seq,
                        selected_peers = stats.selected_peers,
                        sent_peers = stats.sent_peers,
                        sent_bytes = stats.sent_bytes,
                        "published Nostr event over decentralized pubsub"
                    );
                }
                Err(err) => {
                    tracing::warn!(
                        event_id,
                        seq,
                        "nostr decentralized pubsub publish failed: {err:#}"
                    );
                }
            }
            seq = seq.wrapping_add(1);
            if seq == 0 {
                seq = 1;
            }
        }
    })
}

pub fn fips_peer_ids_from_pubkeys(pubkeys: Vec<[u8; 32]>) -> Vec<String> {
    pubkeys
        .into_iter()
        .filter_map(|pubkey| PublicKey::from_slice(&pubkey).ok())
        .filter_map(|pubkey| pubkey.to_bech32().ok())
        .collect()
}

pub fn daemon_fips_peer_configs(
    config: &Config,
    discovered_peer_ids: Vec<String>,
) -> Vec<FipsPeerConfig> {
    let mut seen = HashSet::new();
    let mut peers = Vec::new();

    for peer in &config.server.fips_peers {
        let npub = peer.npub.trim().to_string();
        if npub.is_empty() || !seen.insert(npub.clone()) {
            continue;
        }
        let udp_addresses = peer
            .udp_addresses
            .iter()
            .map(|addr| addr.trim().to_string())
            .filter(|addr| !addr.is_empty())
            .collect();
        peers.push(FipsPeerConfig {
            npub,
            udp_addresses,
        });
    }

    for peer_id in discovered_peer_ids {
        let npub = peer_id.trim().to_string();
        if npub.is_empty() || !seen.insert(npub.clone()) {
            continue;
        }
        peers.push(FipsPeerConfig::new(npub));
    }

    peers
}

fn normalized_discovery_scope(scope: &str) -> String {
    let scope = scope.trim();
    if scope.is_empty() {
        DEFAULT_FIPS_DISCOVERY_SCOPE.to_string()
    } else {
        scope.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashtree_core::Store;
    use sha2::{Digest, Sha256};
    use tokio::time::timeout;

    #[test]
    fn fips_peer_ids_from_pubkeys_encodes_npbus() {
        let keys = nostr::Keys::generate();
        let expected = keys.public_key().to_bech32().unwrap();

        assert_eq!(
            fips_peer_ids_from_pubkeys(vec![keys.public_key().to_bytes()]),
            vec![expected]
        );
    }

    #[test]
    fn empty_discovery_scope_uses_hashtree_default() {
        assert_eq!(
            normalized_discovery_scope("  "),
            DEFAULT_FIPS_DISCOVERY_SCOPE.to_string()
        );
    }

    #[tokio::test]
    async fn fips_local_only_event_transport_requires_local_provider() {
        let mut config = Config::default();
        config.nostr.event_transport = NostrEventTransport::FipsLocalOnly;

        let error = start_daemon_nostr_provider(&config, None)
            .await
            .err()
            .expect("missing local FIPS provider must fail");

        assert!(error.to_string().contains("fips-local-only"));
        assert!(error.to_string().contains("requires"));
    }

    #[tokio::test]
    async fn daemon_blob_resolver_advertises_and_serves_storage_router() {
        let provider_endpoint = local_only_endpoint("htree-provider-test").await;
        let observer_endpoint = local_only_endpoint("iris-drive-observer-test").await;
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(HashtreeStore::new(temp.path()).unwrap());
        let data = b"served directly from htree StorageRouter".to_vec();
        let hash = Sha256::digest(&data).into();
        store.store_arc().put(hash, data.clone()).await.unwrap();
        let (blob_resolver, provider) = bind_daemon_blob_resolver(
            &provider_endpoint,
            store.store_arc(),
            &[],
            Duration::from_secs(1),
        )
        .await
        .expect("bind htree provider");
        let transport = Arc::new(HashtreeFipsTransport::new(
            provider_endpoint.endpoint.clone(),
            store.store_arc(),
        ));
        let receiver_task = transport.start();
        let handle = DaemonFipsHandle {
            transport,
            endpoint_npub: provider_endpoint.local_peer_id.clone(),
            discovery_scope: provider_endpoint.discovery_scope.clone(),
            nostr_provider: None,
            blob_resolver,
            blob_transport: Mutex::new(Some(provider)),
            receiver_task,
        };

        timeout(Duration::from_secs(10), async {
            loop {
                if provider_advertised(&observer_endpoint, &provider_endpoint.local_peer_id) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("htree provider capability did not converge");

        let requester = hashtree_fips_transport::SameHostBlobStore::bind(
            observer_endpoint.native_endpoint.clone(),
            Arc::new(hashtree_core::MemoryStore::new()),
            None,
            hashtree_fips_transport::SameHostBlobStoreConfig::default(),
        )
        .await
        .unwrap();
        assert_eq!(requester.get(&hash).await.unwrap(), Some(data));

        drop(requester);
        handle.shutdown();
        timeout(Duration::from_secs(10), async {
            while provider_advertised(&observer_endpoint, &provider_endpoint.local_peer_id) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("htree provider capability survived daemon shutdown");
        drop(handle);
        observer_endpoint.native_endpoint.shutdown().await.unwrap();
        provider_endpoint.native_endpoint.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn daemon_blob_resolvers_honor_multi_hop_htl_and_cache() {
        let (source_endpoint, source_addr) = udp_endpoint("htree-source-test").await;
        let (middle_endpoint, middle_addr) = udp_endpoint("htree-middle-test").await;
        let (requester_endpoint, requester_addr) = udp_endpoint("htree-requester-test").await;
        let source_dir = tempfile::tempdir().unwrap();
        let middle_dir = tempfile::tempdir().unwrap();
        let requester_dir = tempfile::tempdir().unwrap();
        let source_store = Arc::new(HashtreeStore::new(source_dir.path()).unwrap());
        let middle_store = Arc::new(HashtreeStore::new(middle_dir.path()).unwrap());
        let requester_store = Arc::new(HashtreeStore::new(requester_dir.path()).unwrap());
        let data = b"canonical multi-hop blob route".to_vec();
        let hash = Sha256::digest(&data).into();
        source_store
            .store_arc()
            .put(hash, data.clone())
            .await
            .unwrap();

        source_endpoint
            .endpoint
            .set_peer_configs(vec![FipsPeerConfig {
                npub: middle_endpoint.local_peer_id.clone(),
                udp_addresses: vec![middle_addr.clone()],
            }])
            .await
            .unwrap();
        middle_endpoint
            .endpoint
            .set_peer_configs(vec![
                FipsPeerConfig {
                    npub: source_endpoint.local_peer_id.clone(),
                    udp_addresses: vec![source_addr.clone()],
                },
                FipsPeerConfig {
                    npub: requester_endpoint.local_peer_id.clone(),
                    udp_addresses: vec![requester_addr],
                },
            ])
            .await
            .unwrap();
        requester_endpoint
            .endpoint
            .set_peer_configs(vec![FipsPeerConfig {
                npub: middle_endpoint.local_peer_id.clone(),
                udp_addresses: vec![middle_addr.clone()],
            }])
            .await
            .unwrap();

        let (_source_resolver, source_transport) = bind_daemon_blob_resolver(
            &source_endpoint,
            source_store.store_arc(),
            &[],
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        let (middle_resolver, middle_transport) = bind_daemon_blob_resolver(
            &middle_endpoint,
            middle_store.store_arc(),
            &[FipsPeerConfig {
                npub: source_endpoint.local_peer_id.clone(),
                udp_addresses: vec![source_addr],
            }],
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        let (requester_resolver, requester_transport) = bind_daemon_blob_resolver(
            &requester_endpoint,
            requester_store.store_arc(),
            &[FipsPeerConfig {
                npub: middle_endpoint.local_peer_id.clone(),
                udp_addresses: vec![middle_addr],
            }],
            Duration::from_secs(2),
        )
        .await
        .unwrap();

        wait_for_peer(&requester_endpoint, &middle_endpoint.local_peer_id).await;
        wait_for_peer(&middle_endpoint, &source_endpoint.local_peer_id).await;

        assert_eq!(
            requester_resolver
                .route(hashtree_core::BlobRequest { hash, htl: 1 })
                .await
                .expect("exhausted search"),
            hashtree_core::BlobReply::NoResult,
        );
        assert_eq!(
            requester_resolver
                .route(hashtree_core::BlobRequest { hash, htl: 2 })
                .await
                .expect("two-hop search"),
            hashtree_core::BlobReply::Data(data.clone()),
        );
        assert_eq!(
            middle_store.store_arc().get(&hash).await.unwrap(),
            Some(data.clone())
        );
        assert_eq!(
            requester_store.store_arc().get(&hash).await.unwrap(),
            Some(data)
        );

        drop(requester_transport);
        drop(middle_transport);
        drop(source_transport);
        requester_endpoint.native_endpoint.shutdown().await.unwrap();
        middle_endpoint.native_endpoint.shutdown().await.unwrap();
        source_endpoint.native_endpoint.shutdown().await.unwrap();
        drop(middle_resolver);
    }

    fn provider_advertised(endpoint: &BoundFipsEndpoint, npub: &str) -> bool {
        endpoint
            .native_endpoint
            .local_instance_advertisements()
            .unwrap()
            .iter()
            .any(|advert| {
                advert.npub == npub
                    && advert
                        .capability(hashtree_fips_transport::TCP_BLOB_CAPABILITY)
                        .and_then(|capability| capability.fsp_port)
                        == Some(hashtree_fips_transport::TCP_BLOB_SERVICE_PORT)
            })
    }

    async fn local_only_endpoint(scope: &str) -> hashtree_fips_transport::BoundFipsEndpoint {
        let keys = nostr::Keys::generate();
        let mut options = FipsEndpointOptions::new(keys.secret_key().to_bech32().unwrap());
        options.discovery_scope = scope.to_string();
        options.enable_udp = false;
        options.enable_webrtc = false;
        options.enable_local_rendezvous = true;
        options.enable_lan_discovery = false;
        options.share_local_candidates = false;
        bind_fips_endpoint(options).await.unwrap()
    }

    async fn udp_endpoint(scope: &str) -> (BoundFipsEndpoint, String) {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = socket.local_addr().unwrap();
        drop(socket);
        let keys = nostr::Keys::generate();
        let mut options = FipsEndpointOptions::new(keys.secret_key().to_bech32().unwrap());
        options.discovery_scope = scope.to_string();
        options.enable_udp = true;
        options.udp_bind_addr = Some(addr.to_string());
        options.enable_webrtc = false;
        options.enable_local_rendezvous = false;
        options.enable_lan_discovery = false;
        options.share_local_candidates = false;
        let endpoint = bind_fips_endpoint(options).await.unwrap();
        (endpoint, addr.to_string())
    }

    async fn wait_for_peer(endpoint: &BoundFipsEndpoint, npub: &str) {
        let result = timeout(Duration::from_secs(10), async {
            loop {
                if endpoint
                    .native_endpoint
                    .peers()
                    .await
                    .unwrap()
                    .iter()
                    .any(|peer| peer.npub == npub && peer.connected)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        if result.is_err() {
            panic!(
                "FIPS peer {npub} did not connect; peers={:?}",
                endpoint.native_endpoint.peers().await
            );
        }
    }

    #[test]
    fn daemon_fips_peer_configs_prefer_configured_peers() {
        let mut config = Config::default();
        config.server.fips_peers = vec![
            crate::config::ConfiguredFipsPeer {
                npub: " origin ".to_string(),
                udp_addresses: vec![" udp:192.0.2.10:2121 ".to_string(), " ".to_string()],
            },
            crate::config::ConfiguredFipsPeer {
                npub: "origin".to_string(),
                udp_addresses: vec!["udp:ignored:2121".to_string()],
            },
        ];

        let peers = daemon_fips_peer_configs(
            &config,
            vec![
                "origin".to_string(),
                " followed ".to_string(),
                " ".to_string(),
            ],
        );

        assert_eq!(
            peers,
            vec![
                FipsPeerConfig {
                    npub: "origin".to_string(),
                    udp_addresses: vec!["udp:192.0.2.10:2121".to_string()],
                },
                FipsPeerConfig::new("followed"),
            ]
        );
    }

    #[cfg(feature = "experimental-decentralized-pubsub")]
    #[test]
    fn daemon_decentralized_pubsub_requires_config_fips_and_relay() {
        let mut config = Config::default();
        assert!(!daemon_decentralized_pubsub_ready(&config, true, true));

        config.nostr.decentralized_pubsub = true;
        assert!(daemon_decentralized_pubsub_ready(&config, true, true));
        assert!(!daemon_decentralized_pubsub_ready(&config, false, true));
        assert!(!daemon_decentralized_pubsub_ready(&config, true, false));

        config.nostr.enabled = false;
        assert!(!daemon_decentralized_pubsub_ready(&config, true, true));
    }
}
