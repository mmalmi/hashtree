//! Hashtree daemon integration for the embedded FIPS endpoint API.
//!
//! This starts `fips_core::FipsEndpoint` inside the htree process; it does not
//! depend on or talk to an external FIPS daemon.

use crate::config::Config;
use crate::storage::{HashtreeStore, StorageRouter};
use anyhow::{Context, Result};
use hashtree_fips_transport::{
    bind_fips_endpoint, FipsEndpointOptions, FipsPeerConfig, HashtreeFipsTransport,
    DEFAULT_FIPS_DISCOVERY_SCOPE,
};
use nostr::nips::nip19::ToBech32;
use nostr::PublicKey;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;

pub type DaemonFipsTransport = HashtreeFipsTransport<StorageRouter>;

pub struct DaemonFipsHandle {
    pub transport: Arc<DaemonFipsTransport>,
    pub endpoint_npub: String,
    pub discovery_scope: String,
    receiver_task: JoinHandle<()>,
}

impl DaemonFipsHandle {
    pub fn shutdown(&self) {
        self.receiver_task.abort();
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
    let identity_nsec = keys
        .secret_key()
        .to_bech32()
        .context("Failed to encode daemon identity for FIPS endpoint")?;
    let mut options = FipsEndpointOptions::new(identity_nsec);
    options.discovery_scope = discovery_scope.clone();
    options.relays = relays;
    options.enable_udp = config.server.enable_fips_udp;
    options.enable_webrtc = config.server.enable_fips_webrtc;
    options.udp_bind_addr = config.server.fips_udp_bind_addr.clone();
    options.udp_public = config.server.fips_udp_public;
    options.udp_external_addr = config.server.fips_udp_external_addr.clone();
    options.webrtc_auto_connect = false;
    options.webrtc_max_connections = hashtree_fips_transport::DEFAULT_FIPS_WEBRTC_MAX_CONNECTIONS;
    options.open_discovery_max_pending = 0;
    options.packet_channel_capacity = 1024;
    let endpoint = bind_fips_endpoint(options)
        .await
        .context("Failed to start FIPS endpoint")?;

    let request_timeout = Duration::from_millis(config.server.fips_request_timeout_ms.max(1));
    let transport = Arc::new(
        HashtreeFipsTransport::new(endpoint.endpoint, store.store_arc())
            .with_request_timeout(request_timeout)
            .with_cache_responses(false),
    );
    let peer_configs = daemon_fips_peer_configs(config, peer_ids);
    if !peer_configs.is_empty() {
        transport.set_peer_configs(peer_configs).await;
    }
    let receiver_task = transport.start();

    Ok(Some(DaemonFipsHandle {
        transport,
        endpoint_npub: endpoint.local_peer_id,
        discovery_scope: endpoint.discovery_scope,
        receiver_task,
    }))
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
}
