//! Hashtree daemon integration for the embedded FIPS endpoint API.
//!
//! This starts `fips_core::FipsEndpoint` inside the htree process; it does not
//! depend on or talk to an external FIPS daemon.

use crate::config::{Config, NostrEventTransport};
#[cfg(feature = "experimental-decentralized-pubsub")]
use crate::nostr_relay::NostrRelay;
use crate::storage::{HashtreeStore, StorageRouter};
use anyhow::{Context, Result};
use hashtree_core::{BlobRoute, StoreBlobRoute};
use hashtree_fips_transport::{
    bind_fips_endpoint, bind_fips_endpoint_at_local_rendezvous, set_fips_peer_configs,
    BoundFipsEndpoint, FipsBlobRoute, FipsEndpoint, FipsEndpointOptions, FipsPeerConfig,
    PeerIdentity, TcpBlobTransport, TcpBlobTransportConfig, WebSocketConfig,
    DEFAULT_FIPS_DISCOVERY_SCOPE,
};
use hashtree_network::{BlobRouteEntry, BlobRouter, BlobRouterConfig, MeshForwardingRoute};
use hashtree_nostr_pubsub::HashtreeNostrBoundedEventCache;
use nostr::nips::nip19::ToBech32;
use nostr::PublicKey;
#[cfg(feature = "experimental-decentralized-pubsub")]
use nostr_pubsub::{EventBus, Filter, VerifiedEvent};
use nostr_pubsub::{
    EventPolicyContext, EventRetentionPolicy, EventSource, NostrPubsubRouter, PolicyDecision,
    PubsubPolicy, RouterLiveSource, RouterPublishSource, RouterQuerySource, SourcePolicyContext,
    SourceRoute,
};
use nostr_pubsub_fips::{FipsPubsubClient, FipsPubsubClientOptions};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;
#[cfg(feature = "experimental-decentralized-pubsub")]
use tokio::sync::mpsc;
#[cfg(feature = "experimental-decentralized-pubsub")]
use tokio::task::JoinHandle;

pub type DaemonBlobResolver = BlobRouter;
type DaemonBlobTransport = TcpBlobTransport<StorageRouter>;
pub type DaemonNostrCache = HashtreeNostrBoundedEventCache<StorageRouter>;

const DAEMON_SAME_HOST_PROVIDER_PRIORITY: i16 = 100;
const DAEMON_NOSTR_CACHE_EVENTS: usize = 4_096;
const BLOB_RESOLVER_REPLY_MARGIN: Duration = Duration::from_secs(1);

pub struct DaemonFipsHandle {
    pub endpoint: Arc<FipsEndpoint>,
    pub endpoint_npub: String,
    pub discovery_scope: String,
    pub pubsub_client: Option<Arc<FipsPubsubClient>>,
    pub blob_resolver: Arc<DaemonBlobResolver>,
    blob_transport: Mutex<Option<Arc<DaemonBlobTransport>>>,
}

impl DaemonFipsHandle {
    pub async fn shutdown(&self) {
        if let Ok(mut transport) = self.blob_transport.lock() {
            transport.take();
        }
        if let Err(err) = self.endpoint.shutdown().await {
            tracing::warn!("failed to stop embedded FIPS endpoint: {err}");
        }
    }
}

#[cfg(feature = "experimental-decentralized-pubsub")]
pub struct DaemonNostrPubsubHandle {
    _client: Arc<FipsPubsubClient>,
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
    let websocket_seed_urls = config.server.resolved_fips_websocket_seed_urls();
    if !websocket_seed_urls.is_empty() {
        options.websocket = Some(WebSocketConfig {
            seed_urls: websocket_seed_urls,
            ..Default::default()
        });
    }
    options.enable_local_rendezvous = true;
    options.enable_lan_discovery = config.server.enable_fips_lan_discovery;
    options.ethernet_interfaces = config.server.fips_ethernet_interfaces.clone();
    options.udp_bind_addr = config.server.fips_udp_bind_addr.clone();
    options.udp_public = config.server.fips_udp_public;
    options.udp_external_addr = config.server.fips_udp_external_addr.clone();
    // A configured/followed peer may be WebRTC-only. Let this endpoint dial
    // discovered WebRTC adverts so connectivity does not depend on the remote
    // side winning the offer race.
    options.webrtc_auto_connect = options.enable_webrtc && !peer_configs.is_empty();
    options.webrtc_max_connections = hashtree_fips_transport::DEFAULT_FIPS_WEBRTC_MAX_CONNECTIONS;
    options.open_discovery_max_pending = config.server.fips_open_discovery_max_pending;
    options.packet_channel_capacity = 1024;
    let endpoint = if let Some(raw) = config.server.fips_local_rendezvous_addr.as_deref() {
        let rendezvous_addr = raw
            .parse()
            .context("server.fips_local_rendezvous_addr must be an IPv4 socket address")?;
        bind_fips_endpoint_at_local_rendezvous(options, rendezvous_addr).await
    } else {
        bind_fips_endpoint(options).await
    }
    .context("Failed to start FIPS endpoint")?;
    let request_timeout = Duration::from_millis(config.server.fips_request_timeout_ms.max(1));
    let pubsub_client = if daemon_fips_pubsub_required(config) {
        let options = FipsPubsubClientOptions {
            query_timeout: request_timeout,
            max_frame_bytes: config
                .nostr
                .decentralized_pubsub_max_event_bytes
                .min(nostr_pubsub_fips::FIPS_NOSTR_PUBSUB_MAX_FRAME_BYTES),
            ..Default::default()
        };
        Some(Arc::new(
            FipsPubsubClient::start(endpoint.native_endpoint.clone(), options)
                .await
                .context("Failed to start FIPS Nostr pubsub provider")?,
        ))
    } else {
        None
    };
    if !peer_configs.is_empty() {
        set_fips_peer_configs(endpoint.native_endpoint.as_ref(), peer_configs.clone())
            .await
            .context("Failed to configure FIPS peers")?;
    }
    let (blob_resolver, blob_transport) =
        bind_daemon_blob_resolver(&endpoint, store.store_arc(), &peer_configs, request_timeout)
            .await?;

    Ok(Some(DaemonFipsHandle {
        endpoint: endpoint.native_endpoint,
        endpoint_npub: endpoint.local_peer_id,
        discovery_scope: endpoint.discovery_scope,
        pubsub_client,
        blob_resolver,
        blob_transport: Mutex::new(Some(blob_transport)),
    }))
}

fn daemon_fips_pubsub_required(config: &Config) -> bool {
    config.nostr.event_transport == NostrEventTransport::FipsLocalOnly
        || config.nostr.decentralized_pubsub_enabled()
}

async fn bind_daemon_blob_resolver(
    endpoint: &BoundFipsEndpoint,
    store: Arc<StorageRouter>,
    peers: &[FipsPeerConfig],
    request_timeout: Duration,
) -> Result<(Arc<DaemonBlobResolver>, Arc<DaemonBlobTransport>)> {
    let mut routes = vec![BlobRouteEntry::new(
        "configured-store",
        Arc::new(StoreBlobRoute::new(store.clone())),
    )];
    let resolver = Arc::new(
        BlobRouter::new(
            routes.clone(),
            Some(store.clone()),
            BlobRouterConfig {
                request_timeout,
                ..Default::default()
            },
        )
        .map_err(anyhow::Error::msg)
        .context("Failed to configure the daemon blob router")?,
    );
    // Advertise the same resolver used by in-process daemon reads. The FIPS
    // route added below owns only a weak transport reference, so routing an
    // inbound request through this resolver does not create an Arc cycle.
    let inbound_route: Arc<dyn BlobRoute> = resolver.clone();
    let transport = Arc::new(
        TcpBlobTransport::bind_advertised_route_with_config(
            endpoint.native_endpoint.clone(),
            store.clone(),
            inbound_route,
            blob_transport_config(request_timeout),
            DAEMON_SAME_HOST_PROVIDER_PRIORITY,
        )
        .await
        .context("Failed to advertise htree's blob resolver")?,
    );
    let peer_identities: Vec<_> = peers
        .iter()
        .filter_map(|peer| PeerIdentity::from_npub(&peer.npub).ok())
        .collect();
    if !peer_identities.is_empty() {
        let max_provider_attempts = peer_identities.len().min(4);
        let fips_route =
            FipsBlobRoute::explicit(transport.clone(), peer_identities, max_provider_attempts)
                .map_err(anyhow::Error::msg)
                .context("Failed to configure the daemon FIPS blob route")?;
        routes.push(BlobRouteEntry::new(
            "configured-fips-peers",
            Arc::new(MeshForwardingRoute::new(Arc::new(fips_route))),
        ));
    }
    resolver
        .set_routes(routes)
        .await
        .map_err(anyhow::Error::msg)
        .context("Failed to install daemon blob routes")?;
    Ok((resolver, transport))
}

fn blob_transport_config(resolver_timeout: Duration) -> TcpBlobTransportConfig {
    TcpBlobTransportConfig {
        idle_timeout: resolver_timeout.saturating_add(BLOB_RESOLVER_REPLY_MARGIN),
    }
}

pub fn new_daemon_nostr_cache(store: Arc<StorageRouter>) -> Arc<DaemonNostrCache> {
    Arc::new(
        HashtreeNostrBoundedEventCache::new(
            store,
            None,
            EventSource::local_index("hashtree-nostr-cache"),
            EventRetentionPolicy::new(DAEMON_NOSTR_CACHE_EVENTS, Vec::new()),
        )
        .with_priority(nostr_pubsub::SOURCE_PRIORITY_LOCAL_INDEX),
    )
}

struct AllowDaemonPubsubRoutes;

#[async_trait::async_trait]
impl PubsubPolicy for AllowDaemonPubsubRoutes {
    async fn check_event(
        &self,
        _context: EventPolicyContext<'_>,
    ) -> nostr_pubsub::Result<PolicyDecision> {
        Ok(PolicyDecision::allow_with_priority(0))
    }

    async fn check_source(
        &self,
        _context: SourcePolicyContext<'_>,
    ) -> nostr_pubsub::Result<PolicyDecision> {
        Ok(PolicyDecision::allow_with_priority(0))
    }
}

pub async fn start_daemon_nostr_provider(
    config: &Config,
    fips_handle: Option<&DaemonFipsHandle>,
    cache: Option<Arc<DaemonNostrCache>>,
) -> Result<Option<Arc<dyn nostr_pubsub::PubsubProvider>>> {
    let mut router = NostrPubsubRouter::new(Arc::new(AllowDaemonPubsubRoutes));
    if let Some(cache) = cache {
        let route = cache
            .source_route("hashtree-local-cache")
            .context("Failed to configure Hashtree Nostr cache route")?;
        router = router.with_query_source(RouterQuerySource::new(route, cache));
    }

    let router = match config.nostr.event_transport {
        NostrEventTransport::Relay => {
            let relays = config.nostr.active_relays();
            if relays.is_empty() {
                return Ok(None);
            }
            let route = SourceRoute::relay(relays.join(","))
                .with_dataset("configured-relays")
                .context("Failed to configure Nostr relay route")?;
            let event_bus = Arc::new(
                nostr_pubsub_relay::RelayEventBus::new(
                    relays.clone(),
                    Duration::from_millis(config.server.fips_request_timeout_ms.max(1)),
                )
                .await
                .context("Failed to start Nostr relay event provider")?,
            );
            router
                .with_query_source(RouterQuerySource::new(
                    route.clone(),
                    Arc::clone(&event_bus),
                ))
                .with_publish_source(RouterPublishSource::new(
                    route.clone(),
                    Arc::clone(&event_bus),
                ))
                .with_live_source(RouterLiveSource::new(route, event_bus))
        }
        NostrEventTransport::FipsLocalOnly => {
            let client = fips_handle
                .and_then(|handle| handle.pubsub_client.clone())
                .ok_or_else(|| {
                anyhow::anyhow!(
                    "nostr.event_transport=fips-local-only requires the local FIPS Nostr pubsub provider"
                )
                })?;
            let route = SourceRoute::fips_peer_default("connected-fips-mesh")
                .with_dataset("fips-mesh")
                .context("Failed to configure FIPS Nostr route")?;
            router
                .with_query_source(RouterQuerySource::new(route.clone(), Arc::clone(&client)))
                .with_publish_source(RouterPublishSource::new(route.clone(), Arc::clone(&client)))
                .with_live_source(RouterLiveSource::new(route, client))
        }
    };
    Ok(Some(Arc::new(router)))
}

#[cfg(feature = "experimental-decentralized-pubsub")]
pub async fn start_daemon_nostr_pubsub(
    config: &Config,
    fips_handle: Option<&DaemonFipsHandle>,
    relay: Option<Arc<NostrRelay>>,
    cache: Arc<DaemonNostrCache>,
) -> Result<Option<Arc<DaemonNostrPubsubHandle>>> {
    if !daemon_decentralized_pubsub_ready(config, fips_handle.is_some(), relay.is_some()) {
        return Ok(None);
    }

    let fips_handle = fips_handle.expect("checked by daemon_decentralized_pubsub_ready");
    let relay = relay.expect("checked by daemon_decentralized_pubsub_ready");
    let client = fips_handle.pubsub_client.clone().ok_or_else(|| {
        anyhow::anyhow!("decentralized Nostr pubsub requires the shared FIPS pubsub client")
    })?;

    let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
    relay.set_decentralized_pubsub_sender(Some(outbound_tx));
    let ingest_task = spawn_daemon_nostr_pubsub_ingest(
        Arc::clone(&client),
        Arc::clone(&fips_handle.endpoint),
        Arc::clone(&relay),
        Arc::clone(&cache),
    );
    let outbound_task = spawn_daemon_nostr_pubsub_outbound(Arc::clone(&client), outbound_rx, cache);

    Ok(Some(Arc::new(DaemonNostrPubsubHandle {
        _client: client,
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
    client: Arc<FipsPubsubClient>,
    endpoint: Arc<FipsEndpoint>,
    relay: Arc<NostrRelay>,
    cache: Arc<DaemonNostrCache>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let subscribed_peers = connected_fips_peer_ids(&endpoint).await;
            if subscribed_peers.is_empty() {
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            }
            let mut subscription = match client.subscribe(vec![Filter::new()]).await {
                Ok(subscription) => subscription,
                Err(err) => {
                    tracing::debug!("waiting to subscribe to FIPS Nostr peers: {err}");
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    continue;
                }
            };
            loop {
                tokio::select! {
                    delivery = subscription.recv() => {
                        let Some(delivery) = delivery else {
                            break;
                        };
                        let source = delivery.source.clone();
                        let source_id = source.id.as_str().to_string();
                        let verified = delivery.event;
                        let event = verified.as_event().clone();
                        let event_id = event.id.to_hex();
                        match relay.ingest_peer_event_silent(event).await {
                            Ok(true) => {
                                if let Err(error) = cache.publish(verified, source).await {
                                    tracing::warn!(event_id, %error, "failed to cache decentralized Nostr event");
                                }
                                tracing::debug!(
                                    source = source_id,
                                    event_id,
                                    "ingested decentralized Nostr pubsub event"
                                );
                            }
                            Ok(false) => {}
                            Err(err) => tracing::warn!(
                                source = source_id,
                                event_id,
                                "nostr decentralized pubsub ingest failed: {err:#}"
                            ),
                        }
                    }
                    () = tokio::time::sleep(Duration::from_millis(500)) => {
                        if connected_fips_peer_ids(&endpoint).await != subscribed_peers {
                            break;
                        }
                    }
                }
            }
        }
    })
}

#[cfg(feature = "experimental-decentralized-pubsub")]
async fn connected_fips_peer_ids(endpoint: &FipsEndpoint) -> Vec<String> {
    let mut peers = endpoint
        .peers()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|peer| peer.connected)
        .map(|peer| peer.npub)
        .collect::<Vec<_>>();
    peers.sort_unstable();
    peers.dedup();
    peers
}

#[cfg(feature = "experimental-decentralized-pubsub")]
fn spawn_daemon_nostr_pubsub_outbound(
    client: Arc<FipsPubsubClient>,
    mut outbound_rx: mpsc::UnboundedReceiver<nostr::Event>,
    cache: Arc<DaemonNostrCache>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(event) = outbound_rx.recv().await {
            let event_id = event.id.to_hex();
            let verified = match VerifiedEvent::try_from(event) {
                Ok(event) => event,
                Err(err) => {
                    tracing::warn!(event_id, "invalid outbound Nostr event: {err}");
                    continue;
                }
            };
            let source = EventSource::local_index("htree-relay");
            if let Err(error) = cache.publish(verified.clone(), source.clone()).await {
                tracing::warn!(event_id, %error, "failed to cache outbound Nostr event");
            }
            match client.publish(verified, source).await {
                Ok(report) => tracing::debug!(
                    event_id,
                    accepted = report.accepted,
                    "published Nostr event over decentralized pubsub"
                ),
                Err(err) => {
                    tracing::warn!(event_id, "nostr decentralized pubsub publish failed: {err}")
                }
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
    #[cfg(feature = "experimental-decentralized-pubsub")]
    use nostr::{EventBuilder, Kind, Tag};
    #[cfg(feature = "experimental-decentralized-pubsub")]
    use nostr_pubsub::{PubsubProviderMode, QueryOptions};
    use sha2::{Digest, Sha256};
    use tokio::time::timeout;

    struct RecordingStoreRoute {
        store: Arc<hashtree_core::MemoryStore>,
        requests: Arc<std::sync::Mutex<Vec<hashtree_core::BlobRequest>>>,
    }

    #[async_trait::async_trait]
    impl BlobRoute for RecordingStoreRoute {
        async fn route(
            &self,
            request: hashtree_core::BlobRequest,
        ) -> Result<hashtree_core::BlobReply, hashtree_core::StoreError> {
            self.requests.lock().unwrap().push(request);
            StoreBlobRoute::new(self.store.clone()).route(request).await
        }
    }

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

        let error = start_daemon_nostr_provider(&config, None, None)
            .await
            .err()
            .expect("missing local FIPS provider must fail");

        assert!(error.to_string().contains("fips-local-only"));
        assert!(error.to_string().contains("requires"));
    }

    #[cfg(feature = "experimental-decentralized-pubsub")]
    #[tokio::test]
    async fn one_native_endpoint_serves_roots_and_trusted_decentralized_events() {
        let scope = format!("htree-native-pubsub-{}", uuid::Uuid::new_v4());
        let daemon_addr = reserve_udp_addr();
        let rendezvous_addr = reserve_udp_addr();
        let (remote_endpoint, remote_addr) = udp_endpoint(&scope).await;
        let daemon_keys = nostr::Keys::generate();
        let trusted_keys = nostr::Keys::generate();
        let untrusted_keys = nostr::Keys::generate();
        let outbound_keys = nostr::Keys::generate();
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(HashtreeStore::new(temp.path().join("blobs")).unwrap());
        let relay = trusted_test_relay(
            temp.path(),
            &[trusted_keys.public_key(), outbound_keys.public_key()],
        )
        .await;

        let mut config = Config::default();
        config.server.fips_discovery_scope = scope;
        config.server.fips_relays = Some(Vec::new());
        config.server.fips_udp_bind_addr = Some(daemon_addr.clone());
        config.server.fips_local_rendezvous_addr = Some(rendezvous_addr);
        config.server.enable_fips_udp = true;
        config.server.enable_fips_webrtc = false;
        config.server.fips_request_timeout_ms = 2_000;
        config.server.fips_peers = vec![crate::config::ConfiguredFipsPeer {
            npub: remote_endpoint.local_peer_id.clone(),
            udp_addresses: vec![remote_addr],
        }];
        config.nostr.enabled = true;
        config.nostr.event_transport = NostrEventTransport::FipsLocalOnly;
        config.nostr.decentralized_pubsub = true;

        let cache = new_daemon_nostr_cache(store.store_arc());
        let daemon = start_daemon_fips_transport(&config, &daemon_keys, store, Vec::new())
            .await
            .unwrap()
            .expect("daemon FIPS endpoint");
        let provider =
            start_daemon_nostr_provider(&config, Some(&daemon), Some(Arc::clone(&cache)))
                .await
                .unwrap()
                .expect("FIPS root provider");
        assert_eq!(provider.mode(), PubsubProviderMode::Router);
        let decentralized =
            start_daemon_nostr_pubsub(&config, Some(&daemon), Some(relay.clone()), cache)
                .await
                .unwrap()
                .expect("decentralized pubsub");

        set_fips_peer_configs(
            remote_endpoint.native_endpoint.as_ref(),
            vec![FipsPeerConfig {
                npub: daemon.endpoint_npub.clone(),
                udp_addresses: vec![daemon_addr],
            }],
        )
        .await
        .unwrap();
        let remote_client = Arc::new(
            FipsPubsubClient::start(
                remote_endpoint.native_endpoint.clone(),
                FipsPubsubClientOptions {
                    query_timeout: Duration::from_secs(2),
                    ..Default::default()
                },
            )
            .await
            .unwrap(),
        );

        wait_for_native_peer(&daemon.endpoint, &remote_endpoint.local_peer_id).await;
        wait_for_peer(&remote_endpoint, &daemon.endpoint_npub).await;
        timeout(Duration::from_secs(5), async {
            while remote_client.peer_subscription_count().unwrap() == 0 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("daemon did not subscribe to the authenticated peer");

        // FipsLocalOnly root publication and resolution use the same client
        // that the decentralized relay bridge already owns.
        let published_root = root_event(&outbound_keys, "published-root", "aa");
        let mut published_root_rx = remote_client
            .subscribe(vec![Filter::new().id(published_root.id)])
            .await
            .unwrap();
        provider
            .publish(
                VerifiedEvent::try_from(published_root.clone()).unwrap(),
                EventSource::local_index("root-publisher"),
            )
            .await
            .unwrap();
        let delivered_root = timeout(Duration::from_secs(5), published_root_rx.recv())
            .await
            .unwrap()
            .expect("published root delivery");
        assert_eq!(delivered_root.event.as_event().id, published_root.id);

        let resolved_root = root_event(&trusted_keys, "resolved-root", "bb");
        remote_client
            .publish(
                VerifiedEvent::try_from(resolved_root.clone()).unwrap(),
                EventSource::local_index("remote-root"),
            )
            .await
            .unwrap();
        let resolution = provider
            .query(
                vec![Filter::new().id(resolved_root.id)],
                QueryOptions { limit: Some(1) },
            )
            .await
            .unwrap();
        assert_eq!(resolution.events.len(), 1);
        assert_eq!(resolution.events[0].event.as_event().id, resolved_root.id);

        // The normal Nostr relay policy remains authoritative after wire-level
        // signature verification: an untrusted author is not indexed, while a
        // later trusted event proves the receive path advanced past it.
        let rejected = EventBuilder::new(Kind::TextNote, "untrusted")
            .sign_with_keys(&untrusted_keys)
            .unwrap();
        remote_client
            .publish(
                VerifiedEvent::try_from(rejected.clone()).unwrap(),
                EventSource::local_index("remote-untrusted"),
            )
            .await
            .unwrap();
        let accepted = EventBuilder::new(Kind::TextNote, "trusted")
            .sign_with_keys(&trusted_keys)
            .unwrap();
        remote_client
            .publish(
                VerifiedEvent::try_from(accepted.clone()).unwrap(),
                EventSource::local_index("remote-trusted"),
            )
            .await
            .unwrap();
        wait_for_relay_event(&relay, accepted.id).await;
        assert!(relay
            .query_events(&Filter::new().id(rejected.id), 1)
            .await
            .is_empty());

        // A trusted local relay publication travels back over the same FIPS
        // event bus without opening another service receiver.
        let outbound = EventBuilder::new(Kind::TextNote, "relay outbound")
            .sign_with_keys(&outbound_keys)
            .unwrap();
        let mut outbound_rx = remote_client
            .subscribe(vec![Filter::new().id(outbound.id)])
            .await
            .unwrap();
        let (client_tx, _client_rx) = mpsc::unbounded_channel();
        relay.register_client(1, client_tx, None).await;
        relay
            .handle_client_message(1, nostr::ClientMessage::event(outbound.clone()))
            .await;
        let delivered = timeout(Duration::from_secs(5), outbound_rx.recv())
            .await
            .unwrap()
            .expect("outbound relay publication");
        assert_eq!(delivered.event.as_event().id, outbound.id);

        decentralized.shutdown();
        daemon.shutdown().await;
        remote_endpoint.native_endpoint.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn daemon_blob_resolver_advertises_and_serves_storage_router() {
        let rendezvous_addr = reserve_local_rendezvous_addr();
        let test_id = uuid::Uuid::new_v4();
        let provider_scope = format!("htree-provider-test-{test_id}");
        let observer_scope = format!("iris-drive-observer-test-{test_id}");
        let provider_endpoint = local_only_endpoint(rendezvous_addr, &provider_scope).await;
        let observer_endpoint = local_only_endpoint(rendezvous_addr, &observer_scope).await;
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
        let handle = DaemonFipsHandle {
            endpoint: provider_endpoint.native_endpoint.clone(),
            endpoint_npub: provider_endpoint.local_peer_id.clone(),
            discovery_scope: provider_endpoint.discovery_scope.clone(),
            pubsub_client: None,
            blob_resolver,
            blob_transport: Mutex::new(Some(provider)),
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

        let observer_store = Arc::new(hashtree_core::MemoryStore::new());
        let observer_transport = Arc::new(
            TcpBlobTransport::bind_route(
                observer_endpoint.native_endpoint.clone(),
                observer_store.clone(),
                Arc::new(StoreBlobRoute::new(observer_store.clone())),
            )
            .await
            .unwrap(),
        );
        let observer_fips_route = FipsBlobRoute::discovered(
            observer_endpoint.native_endpoint.clone(),
            observer_transport.clone(),
            4,
        )
        .unwrap();
        let observer_router = BlobRouter::new(
            vec![BlobRouteEntry::new(
                "same-host-fips",
                Arc::new(observer_fips_route),
            )],
            Some(observer_store),
            BlobRouterConfig::default(),
        )
        .unwrap();
        assert_eq!(observer_router.get(&hash, None).await.unwrap(), Some(data));

        drop(observer_router);
        drop(observer_transport);
        handle.shutdown().await;
        timeout(Duration::from_secs(10), async {
            while provider_advertised(&observer_endpoint, &provider_endpoint.local_peer_id) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("htree provider capability survived daemon shutdown");
        drop(handle);
        observer_endpoint.native_endpoint.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn daemon_mesh_forwarding_observes_two_one_zero_and_exhaustion() {
        let scope = format!("htree-inbound-forwarding-{}", uuid::Uuid::new_v4());
        let (upstream_endpoint, upstream_addr) = udp_endpoint(&scope).await;
        let (provider_endpoint, provider_addr) = udp_endpoint(&scope).await;
        let (observer_endpoint, observer_addr) = udp_endpoint(&scope).await;
        let temp = tempfile::tempdir().unwrap();
        let provider_store = Arc::new(HashtreeStore::new(temp.path().join("provider")).unwrap());
        let upstream_store = Arc::new(hashtree_core::MemoryStore::new());
        let data = b"served through the daemon's configured FIPS peer".to_vec();
        let hash = Sha256::digest(&data).into();
        upstream_store.put(hash, data.clone()).await.unwrap();
        let upstream_requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let upstream_transport = Arc::new(
            TcpBlobTransport::bind_route(
                upstream_endpoint.native_endpoint.clone(),
                upstream_store.clone(),
                Arc::new(RecordingStoreRoute {
                    store: upstream_store,
                    requests: upstream_requests.clone(),
                }),
            )
            .await
            .unwrap(),
        );
        let upstream_peer = FipsPeerConfig {
            npub: upstream_endpoint.local_peer_id.clone(),
            udp_addresses: vec![upstream_addr],
        };
        set_fips_peer_configs(
            provider_endpoint.native_endpoint.as_ref(),
            vec![
                upstream_peer.clone(),
                FipsPeerConfig {
                    npub: observer_endpoint.local_peer_id.clone(),
                    udp_addresses: vec![observer_addr],
                },
            ],
        )
        .await
        .unwrap();
        set_fips_peer_configs(
            upstream_endpoint.native_endpoint.as_ref(),
            vec![FipsPeerConfig {
                npub: provider_endpoint.local_peer_id.clone(),
                udp_addresses: vec![provider_addr.clone()],
            }],
        )
        .await
        .unwrap();
        set_fips_peer_configs(
            observer_endpoint.native_endpoint.as_ref(),
            vec![FipsPeerConfig {
                npub: provider_endpoint.local_peer_id.clone(),
                udp_addresses: vec![provider_addr],
            }],
        )
        .await
        .unwrap();
        let (provider_resolver, provider_transport) = bind_daemon_blob_resolver(
            &provider_endpoint,
            provider_store.store_arc(),
            &[upstream_peer],
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        let observer_store = Arc::new(hashtree_core::MemoryStore::new());
        let observer_transport = Arc::new(
            TcpBlobTransport::bind_route(
                observer_endpoint.native_endpoint.clone(),
                observer_store.clone(),
                Arc::new(StoreBlobRoute::new(observer_store)),
            )
            .await
            .unwrap(),
        );
        let observer_fips_route = FipsBlobRoute::explicit(
            observer_transport.clone(),
            vec![PeerIdentity::from_npub(&provider_endpoint.local_peer_id).unwrap()],
            1,
        )
        .unwrap();
        let observer_route = MeshForwardingRoute::new(Arc::new(observer_fips_route));

        wait_for_native_peer(
            provider_endpoint.native_endpoint.as_ref(),
            &upstream_endpoint.local_peer_id,
        )
        .await;
        wait_for_native_peer(
            observer_endpoint.native_endpoint.as_ref(),
            &provider_endpoint.local_peer_id,
        )
        .await;
        assert_eq!(
            observer_route
                .route(hashtree_core::BlobRequest { hash, htl: 0 })
                .await
                .unwrap(),
            hashtree_core::BlobReply::NoResult,
        );
        assert!(upstream_requests.lock().unwrap().is_empty());
        assert_eq!(
            observer_route
                .route(hashtree_core::BlobRequest { hash, htl: 1 })
                .await
                .unwrap(),
            hashtree_core::BlobReply::NoResult,
        );
        assert!(
            upstream_requests.lock().unwrap().is_empty(),
            "HTL 1 reaches the intermediate daemon as 0 and cannot be forwarded"
        );
        assert_eq!(
            observer_route
                .route(hashtree_core::BlobRequest { hash, htl: 2 })
                .await
                .unwrap(),
            hashtree_core::BlobReply::Data(data),
        );
        assert_eq!(
            upstream_requests.lock().unwrap().as_slice(),
            &[hashtree_core::BlobRequest { hash, htl: 0 }],
            "three Hashtree nodes must observe the two-to-one-to-zero forwarding budget while each FIPS carrier preserves its request",
        );

        drop(observer_route);
        drop(observer_transport);
        drop(provider_resolver);
        drop(provider_transport);
        drop(upstream_transport);
        observer_endpoint.native_endpoint.shutdown().await.unwrap();
        provider_endpoint.native_endpoint.shutdown().await.unwrap();
        upstream_endpoint.native_endpoint.shutdown().await.unwrap();
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

    async fn local_only_endpoint(
        rendezvous_addr: std::net::SocketAddrV4,
        scope: &str,
    ) -> hashtree_fips_transport::BoundFipsEndpoint {
        let keys = nostr::Keys::generate();
        let mut options = FipsEndpointOptions::new(keys.secret_key().to_bech32().unwrap());
        options.discovery_scope = scope.to_string();
        options.enable_udp = false;
        options.enable_webrtc = false;
        options.enable_local_rendezvous = true;
        options.enable_lan_discovery = false;
        options.share_local_candidates = false;
        bind_fips_endpoint_at_local_rendezvous(options, rendezvous_addr)
            .await
            .unwrap()
    }

    async fn udp_endpoint(scope: &str) -> (BoundFipsEndpoint, String) {
        let keys = nostr::Keys::generate();
        let mut options = FipsEndpointOptions::new(keys.secret_key().to_bech32().unwrap());
        options.discovery_scope = scope.to_string();
        options.enable_udp = true;
        options.udp_bind_addr = Some("127.0.0.1:0".to_string());
        options.enable_webrtc = false;
        options.enable_local_rendezvous = false;
        options.enable_lan_discovery = false;
        options.share_local_candidates = false;
        let endpoint = bind_fips_endpoint(options).await.unwrap();
        let addrs = endpoint
            .native_endpoint
            .bound_udp_listen_addrs()
            .await
            .unwrap();
        let [addr] = addrs.as_slice() else {
            panic!("expected exactly one bound UDP listener, got {addrs:?}");
        };
        assert!(addr.ip().is_loopback());
        assert_ne!(addr.port(), 0);
        (endpoint, addr.to_string())
    }

    #[cfg(feature = "experimental-decentralized-pubsub")]
    fn reserve_udp_addr() -> String {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.local_addr().unwrap().to_string()
    }

    fn reserve_local_rendezvous_addr() -> std::net::SocketAddrV4 {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        match socket.local_addr().unwrap() {
            std::net::SocketAddr::V4(addr) => addr,
            std::net::SocketAddr::V6(_) => unreachable!("loopback IPv4 bind returned IPv6"),
        }
    }

    #[cfg(feature = "experimental-decentralized-pubsub")]
    async fn wait_for_peer(endpoint: &BoundFipsEndpoint, npub: &str) {
        wait_for_native_peer(endpoint.native_endpoint.as_ref(), npub).await;
    }

    async fn wait_for_native_peer(endpoint: &FipsEndpoint, npub: &str) {
        let result = timeout(Duration::from_secs(10), async {
            loop {
                if endpoint
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
                endpoint.peers().await
            );
        }
    }

    #[cfg(feature = "experimental-decentralized-pubsub")]
    async fn trusted_test_relay(
        root: &std::path::Path,
        allowed: &[nostr::PublicKey],
    ) -> Arc<NostrRelay> {
        let graph_dir = root.join("graph");
        let data_dir = root.join("nostr");
        std::fs::create_dir_all(&data_dir).unwrap();
        let graph = {
            let _guard = crate::socialgraph::test_lock().await;
            crate::socialgraph::open_test_social_graph_store_with_mapsize(
                &graph_dir,
                Some(128 * 1024 * 1024),
            )
            .unwrap()
        };
        let backend: Arc<dyn crate::socialgraph::SocialGraphBackend> = graph;
        let allowed = allowed
            .iter()
            .map(|pubkey| pubkey.to_hex())
            .collect::<HashSet<_>>();
        let access = Arc::new(crate::socialgraph::SocialGraphAccessControl::new(
            Arc::clone(&backend),
            0,
            allowed.clone(),
        ));
        Arc::new(
            NostrRelay::new(
                backend,
                data_dir,
                allowed,
                Some(access),
                crate::nostr_relay::NostrRelayConfig {
                    spambox_db_max_bytes: 0,
                    ..Default::default()
                },
            )
            .unwrap(),
        )
    }

    #[cfg(feature = "experimental-decentralized-pubsub")]
    fn root_event(keys: &nostr::Keys, name: &str, hash: &str) -> nostr::Event {
        EventBuilder::new(Kind::Custom(30_078), hash)
            .tags([
                Tag::parse(["d", name]).unwrap(),
                Tag::parse(["l", "hashtree"]).unwrap(),
                Tag::parse(["hash", hash]).unwrap(),
            ])
            .sign_with_keys(keys)
            .unwrap()
    }

    #[cfg(feature = "experimental-decentralized-pubsub")]
    async fn wait_for_relay_event(relay: &NostrRelay, id: nostr::EventId) {
        timeout(Duration::from_secs(5), async {
            loop {
                if relay
                    .query_events(&Filter::new().id(id), 1)
                    .await
                    .iter()
                    .any(|event| event.id == id)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("trusted decentralized event was not indexed");
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

    #[test]
    fn blob_carrier_deadline_outlives_the_complete_resolver_budget() {
        for resolver_timeout in [Duration::from_millis(1), Duration::from_secs(20)] {
            assert!(blob_transport_config(resolver_timeout).idle_timeout > resolver_timeout);
        }
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
