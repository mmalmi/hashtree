//! Hashtree daemon integration for the embedded FIPS endpoint API.
//!
//! This starts `fips_core::FipsEndpoint` inside the htree process; it does not
//! depend on or talk to an external FIPS daemon.

use crate::config::Config;
use crate::storage::{HashtreeStore, StorageRouter};
use anyhow::{Context, Result};
use hashtree_fips_transport::{
    bind_fips_endpoint, FipsEndpointOptions, HashtreeFipsTransport, DEFAULT_FIPS_DISCOVERY_SCOPE,
};
use nostr::nips::nip19::ToBech32;
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
    let endpoint = bind_fips_endpoint(FipsEndpointOptions {
        identity_nsec,
        discovery_scope: discovery_scope.clone(),
        relays,
        enable_udp: config.server.enable_fips_udp,
        enable_webrtc: config.server.enable_fips_webrtc,
        udp_bind_addr: config.server.fips_udp_bind_addr.clone(),
        udp_public: config.server.fips_udp_public,
        udp_external_addr: config.server.fips_udp_external_addr.clone(),
        webrtc_auto_connect: false,
        webrtc_max_connections: hashtree_fips_transport::DEFAULT_FIPS_WEBRTC_MAX_CONNECTIONS,
        open_discovery_max_pending: 0,
        packet_channel_capacity: 1024,
    })
    .await
    .context("Failed to start FIPS endpoint")?;

    let request_timeout = Duration::from_millis(config.server.fips_request_timeout_ms.max(1));
    let transport = Arc::new(
        HashtreeFipsTransport::new(endpoint.endpoint, store.store_arc())
            .with_request_timeout(request_timeout)
            .with_cache_responses(false),
    );
    let receiver_task = transport.start();

    Ok(Some(DaemonFipsHandle {
        transport,
        endpoint_npub: endpoint.local_peer_id,
        discovery_scope: endpoint.discovery_scope,
        receiver_task,
    }))
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
    fn empty_discovery_scope_uses_hashtree_default() {
        assert_eq!(
            normalized_discovery_scope("  "),
            DEFAULT_FIPS_DISCOVERY_SCOPE.to_string()
        );
    }
}
