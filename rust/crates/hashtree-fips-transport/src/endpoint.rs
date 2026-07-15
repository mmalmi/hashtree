//! Native embedded FIPS endpoint configuration shared by blob and legacy users.

use fips_core::config::{
    EthernetConfig, NostrDiscoveryPolicy, PeerAddress, RoutingMode, TransportInstances,
};
use hashtree_core::StoreError;
use std::collections::HashMap;
use std::net::SocketAddrV4;
use std::sync::Arc;
use thiserror::Error;

pub const DEFAULT_FIPS_DISCOVERY_SCOPE: &str = "fips-overlay-v1";
pub const DEFAULT_FIPS_WEBRTC_MAX_CONNECTIONS: usize = 8;

#[derive(Debug, Error)]
pub enum FipsTransportError {
    #[error("endpoint failed: {0}")]
    Endpoint(String),
    #[error("endpoint send failed: {0}")]
    Send(String),
    #[error("wire decode failed: {0}")]
    Wire(String),
    #[error("store error: {0}")]
    Store(#[from] StoreError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FipsPeerConfig {
    pub npub: String,
    pub udp_addresses: Vec<String>,
}

impl FipsPeerConfig {
    pub fn new(npub: impl Into<String>) -> Self {
        Self {
            npub: npub.into(),
            udp_addresses: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FipsEndpointOptions {
    pub identity_nsec: String,
    pub discovery_scope: String,
    pub relays: Vec<String>,
    pub enable_udp: bool,
    pub enable_webrtc: bool,
    /// Join the ordinary fixed-loopback FIPS rendezvous transport.
    pub enable_local_rendezvous: bool,
    /// Host-local Ethernet interfaces used as the only underlay when ordinary
    /// network transports are disabled (for example, a browser VM virtio NIC).
    pub ethernet_interfaces: Vec<String>,
    pub enable_lan_discovery: bool,
    pub udp_bind_addr: Option<String>,
    pub udp_public: bool,
    pub udp_external_addr: Option<String>,
    pub share_local_candidates: bool,
    pub webrtc_auto_connect: bool,
    pub webrtc_max_connections: usize,
    pub open_discovery_max_pending: usize,
    pub packet_channel_capacity: usize,
}

impl FipsEndpointOptions {
    pub fn new(identity_nsec: impl Into<String>) -> Self {
        Self {
            identity_nsec: identity_nsec.into(),
            discovery_scope: DEFAULT_FIPS_DISCOVERY_SCOPE.to_string(),
            relays: Vec::new(),
            enable_udp: true,
            enable_webrtc: true,
            enable_local_rendezvous: false,
            ethernet_interfaces: Vec::new(),
            enable_lan_discovery: true,
            udp_bind_addr: None,
            udp_public: false,
            udp_external_addr: None,
            share_local_candidates: true,
            webrtc_auto_connect: false,
            webrtc_max_connections: DEFAULT_FIPS_WEBRTC_MAX_CONNECTIONS,
            open_discovery_max_pending: 0,
            packet_channel_capacity: 1024,
        }
    }
}

pub struct BoundFipsEndpoint {
    #[cfg(feature = "legacy-mesh")]
    pub endpoint: Arc<dyn crate::legacy_mesh::FipsEndpointIo>,
    pub native_endpoint: Arc<fips_core::FipsEndpoint>,
    pub local_peer_id: String,
    pub discovery_scope: String,
}

pub async fn bind_fips_endpoint(
    options: FipsEndpointOptions,
) -> Result<BoundFipsEndpoint, FipsTransportError> {
    bind_fips_endpoint_inner(options, None).await
}

/// Bind the ordinary FIPS endpoint against an alternate fixed loopback
/// rendezvous address. Production callers should normally use
/// [`bind_fips_endpoint`]; this seam isolates multi-process tests and stacks.
pub async fn bind_fips_endpoint_at_local_rendezvous(
    options: FipsEndpointOptions,
    rendezvous_addr: SocketAddrV4,
) -> Result<BoundFipsEndpoint, FipsTransportError> {
    bind_fips_endpoint_inner(options, Some(rendezvous_addr)).await
}

async fn bind_fips_endpoint_inner(
    options: FipsEndpointOptions,
    rendezvous_addr: Option<SocketAddrV4>,
) -> Result<BoundFipsEndpoint, FipsTransportError> {
    if !options.enable_udp
        && !options.enable_webrtc
        && !options.enable_local_rendezvous
        && options.ethernet_interfaces.is_empty()
    {
        return Err(FipsTransportError::Endpoint(
            "at least one FIPS transport must be enabled".to_string(),
        ));
    }

    let discovery_scope = if options.discovery_scope.trim().is_empty() {
        DEFAULT_FIPS_DISCOVERY_SCOPE.to_string()
    } else {
        options.discovery_scope.trim().to_string()
    };
    let packet_channel_capacity = options.packet_channel_capacity;
    let enable_local_rendezvous = options.enable_local_rendezvous;
    let config =
        fips_endpoint_config_with_local_rendezvous(options, &discovery_scope, rendezvous_addr);

    let builder = fips_core::FipsEndpoint::builder()
        .config(config)
        .discovery_scope(discovery_scope.clone())
        .without_system_tun()
        .packet_channel_capacity(packet_channel_capacity);
    let builder = if enable_local_rendezvous {
        builder.local_rendezvous()
    } else {
        builder
    };
    let endpoint = Arc::new(
        builder
            .bind()
            .await
            .map_err(|err| FipsTransportError::Endpoint(err.to_string()))?,
    );
    let local_peer_id = endpoint.npub().to_string();

    Ok(BoundFipsEndpoint {
        #[cfg(feature = "legacy-mesh")]
        endpoint: endpoint.clone(),
        native_endpoint: endpoint,
        local_peer_id,
        discovery_scope,
    })
}

pub async fn set_fips_peer_configs(
    endpoint: &fips_core::FipsEndpoint,
    peer_configs: Vec<FipsPeerConfig>,
) -> Result<(), FipsTransportError> {
    let peers: Vec<fips_core::config::PeerConfig> = peer_configs
        .into_iter()
        .map(|peer| fips_core::config::PeerConfig {
            npub: peer.npub,
            addresses: peer
                .udp_addresses
                .into_iter()
                .filter_map(|addr| peer_address_from_configured_addr(&addr))
                .collect(),
            ..Default::default()
        })
        .collect();
    let peer_count = peers.len();
    match endpoint.update_peers(peers).await {
        Ok(outcome) => {
            tracing::info!(
                peer_count,
                added = outcome.added,
                removed = outcome.removed,
                updated = outcome.updated,
                unchanged = outcome.unchanged,
                "updated FIPS endpoint peer configs"
            );
            Ok(())
        }
        Err(err) => {
            tracing::warn!(
                peer_count,
                error = %err,
                "failed to update FIPS endpoint peer configs"
            );
            Err(FipsTransportError::Endpoint(err.to_string()))
        }
    }
}

#[cfg(test)]
pub(crate) fn fips_endpoint_config(
    options: FipsEndpointOptions,
    discovery_scope: &str,
) -> fips_core::Config {
    fips_endpoint_config_with_local_rendezvous(options, discovery_scope, None)
}

fn fips_endpoint_config_with_local_rendezvous(
    options: FipsEndpointOptions,
    discovery_scope: &str,
    rendezvous_addr: Option<SocketAddrV4>,
) -> fips_core::Config {
    let mut config = fips_core::Config::new();
    config.node.identity = fips_core::IdentityConfig {
        nsec: Some(options.identity_nsec),
        persistent: false,
    };
    config.node.routing.mode = RoutingMode::ReplyLearned;
    config.node.limits.max_peers = options.webrtc_max_connections.max(1);
    config.node.limits.max_links = options.webrtc_max_connections.saturating_mul(2).max(1);
    config.node.limits.max_connections = options.webrtc_max_connections.saturating_mul(2).max(1);
    config.node.limits.max_pending_inbound =
        options.webrtc_max_connections.saturating_mul(4).max(1);
    config.node.control.enabled = false;
    config.tun.enabled = false;
    config.dns.enabled = false;
    config.node.system_files_enabled = false;
    config.node.discovery.lan.enabled = options.enable_lan_discovery;
    config.node.discovery.lan.scope = options
        .enable_lan_discovery
        .then(|| discovery_scope.to_string());
    let external_discovery =
        options.enable_udp || options.enable_webrtc || !options.relays.is_empty();
    config.node.discovery.nostr.enabled = external_discovery;
    config.node.discovery.nostr.advertise = external_discovery;
    config.node.discovery.nostr.policy = if options.open_discovery_max_pending == 0 {
        NostrDiscoveryPolicy::ConfiguredOnly
    } else {
        NostrDiscoveryPolicy::Open
    };
    config.node.discovery.nostr.open_discovery_max_pending = options.open_discovery_max_pending;
    config.node.discovery.nostr.share_local_candidates = options.share_local_candidates;
    config.node.discovery.nostr.app = discovery_scope.to_string();
    config.node.discovery.nostr.advert_relays = options.relays;
    if let Some(rendezvous_addr) = rendezvous_addr {
        config.node.discovery.local.rendezvous_addr = rendezvous_addr;
    }

    let ethernet_configs = options
        .ethernet_interfaces
        .into_iter()
        .enumerate()
        .map(|(index, interface)| {
            (
                format!("local-ethernet-{index}"),
                EthernetConfig {
                    interface,
                    discovery: Some(true),
                    announce: Some(true),
                    auto_connect: Some(true),
                    accept_connections: Some(true),
                    discovery_scope: Some(discovery_scope.to_string()),
                    ..EthernetConfig::default()
                },
            )
        })
        .collect::<HashMap<_, _>>();
    if ethernet_configs.len() == 1 {
        config.transports.ethernet = TransportInstances::Single(
            ethernet_configs
                .into_values()
                .next()
                .expect("one Ethernet configuration"),
        );
    } else if !ethernet_configs.is_empty() {
        config.transports.ethernet = TransportInstances::Named(ethernet_configs);
    }

    if options.enable_udp {
        config.transports.udp = TransportInstances::Single(fips_core::UdpConfig {
            bind_addr: Some(
                options
                    .udp_bind_addr
                    .filter(|addr| !addr.trim().is_empty())
                    .unwrap_or_else(|| "0.0.0.0:0".to_string()),
            ),
            advertise_on_nostr: Some(true),
            public: Some(options.udp_public),
            external_addr: options
                .udp_external_addr
                .filter(|addr| !addr.trim().is_empty()),
            outbound_only: Some(false),
            accept_connections: Some(true),
            ..Default::default()
        });
    }

    #[cfg(feature = "webrtc-endpoint")]
    if options.enable_webrtc {
        config.transports.webrtc = TransportInstances::Single(fips_core::WebRtcConfig {
            advertise_on_nostr: Some(true),
            auto_connect: Some(options.webrtc_auto_connect),
            accept_connections: Some(true),
            max_connections: Some(options.webrtc_max_connections.max(1)),
            ..Default::default()
        });
    }
    #[cfg(not(feature = "webrtc-endpoint"))]
    if options.enable_webrtc {
        tracing::warn!(
            "FIPS WebRTC transport requested but this binary was built without the webrtc feature"
        );
    }

    if options.enable_udp || options.enable_webrtc {
        config.transports.tcp = TransportInstances::Single(Default::default());
    }

    config
}

pub(crate) fn peer_address_from_configured_addr(raw: &str) -> Option<PeerAddress> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let (transport, addr) = split_configured_transport_addr(trimmed);
    Some(PeerAddress::new(transport, addr))
}

fn split_configured_transport_addr(value: &str) -> (&str, &str) {
    let Some((transport, addr)) = value.split_once(':') else {
        return ("udp", value);
    };
    match transport.to_ascii_lowercase().as_str() {
        "udp" | "tcp" | "webrtc" | "tor" | "ethernet" | "ble" => (transport, addr),
        _ => ("udp", value),
    }
}
