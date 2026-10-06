//! Optional, bounded P2P intake. Query results are observations, never coverage.
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

use anyhow::{ensure, Result};
use hashtree_cli::Config;
use hashtree_fips_transport::{
    bind_fips_endpoint, bind_fips_endpoint_at_local_rendezvous, BoundFipsEndpoint,
    FipsEndpointOptions,
};
use hashtree_nostr::{catchup::CatchupPolicy, stored_event_from_nostr_sdk_event, StoredNostrEvent};
use nostr::{nips::nip19::ToBech32, Filter, Keys, Kind, PublicKey, Timestamp};
use nostr_pubsub::{EventBus, QueryOptions, QueryReport};
use nostr_pubsub_fips::{FipsPubsubClient, FipsPubsubClientOptions};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum Status {
    Observed,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Receipt {
    pub status: Status,
    pub events: usize,
    pub added_events: usize,
    pub sources: BTreeMap<String, usize>,
    /// Commitment to sorted distinct IDs observed over P2P, including duplicates
    /// already found on relays. Does not claim the peer has no other events.
    pub event_ids_sha256: String,
}

pub(super) fn normalize_peers(peers: Vec<String>) -> Result<Vec<String>> {
    let mut peers = peers
        .into_iter()
        .map(|peer| {
            PublicKey::parse(&peer)?
                .to_bech32()
                .map_err(anyhow::Error::from)
        })
        .collect::<Result<Vec<_>>>()?;
    peers.sort();
    peers.dedup();
    ensure!(
        peers.len() <= 16,
        "catchup supports at most 16 pubsub peers"
    );
    Ok(peers)
}

/// Uses the host's existing FIPS mesh via loopback. This process neither opens
/// relay discovery nor reuses the daemon's signing identity or storage writer.
pub(super) struct Runtime {
    endpoint: BoundFipsEndpoint,
    pub client: Arc<FipsPubsubClient>,
}

impl Runtime {
    pub async fn start(config: &Config, peers: Vec<String>, timeout: Duration) -> Result<Self> {
        let peers = normalize_peers(peers)?;
        let peer_count = peers.len();
        ensure!(
            peer_count > 0,
            "pubsub intake requires at least one selected peer"
        );
        let mut options = FipsEndpointOptions::new(Keys::generate().secret_key().to_bech32()?);
        options.enable_udp = false;
        options.enable_webrtc = false;
        options.enable_lan_discovery = false;
        options.share_local_candidates = false;
        options.enable_local_rendezvous = true;
        options.discovery_scope = config.server.fips_discovery_scope.clone();
        let endpoint = if let Some(addr) = config.server.fips_local_rendezvous_addr.as_ref() {
            bind_fips_endpoint_at_local_rendezvous(options, addr.parse()?).await?
        } else {
            bind_fips_endpoint(options).await?
        };
        let client = FipsPubsubClient::start(
            endpoint.native_endpoint.clone(),
            FipsPubsubClientOptions {
                // routed_peers is additive. Fill every outgoing slot with a
                // selected identity so discovered services cannot receive author
                // filters or spend query/dedup budgets before our source checks.
                max_connected_peers: peer_count,
                max_inbound_routed_peers: 0,
                fanout: FipsPubsubClientOptions::default().fanout.min(peer_count),
                routed_peers: peers,
                query_timeout: timeout,
                max_active_subscriptions: 4,
                // At most 128 wire frames (under 8 MiB of payload) per replay/query.
                max_replay_events: 128,
                ..Default::default()
            },
        )
        .await;
        match client {
            Ok(client) => Ok(Self {
                endpoint,
                client: Arc::new(client),
            }),
            Err(error) => {
                let _ = endpoint.native_endpoint.shutdown().await;
                Err(error.into())
            }
        }
    }

    pub async fn shutdown(&self) {
        self.client.shutdown_shared().await;
        let _ = self.endpoint.native_endpoint.shutdown().await;
    }
}

pub(super) fn filter(
    policy: &CatchupPolicy,
    author: &str,
    since: u64,
    until: u64,
) -> Result<Filter> {
    Ok(Filter::new()
        .author(PublicKey::from_hex(author)?)
        .kinds(policy.kinds.iter().copied().map(Kind::from))
        .since(Timestamp::from_secs(since))
        .until(Timestamp::from_secs(until))
        .limit(policy.max_events_per_author.saturating_add(1)))
}

pub(super) async fn query(
    bus: &dyn EventBus,
    filter: Filter,
    timeout: Duration,
) -> Option<QueryReport> {
    // EventBus has no completeness bit. Timeout/errors are explicitly unavailable;
    // even a successful empty report must not satisfy a CatchupSource obligation.
    tokio::time::timeout(
        timeout,
        bus.query(
            vec![filter.clone()],
            QueryOptions {
                limit: filter.limit,
            },
        ),
    )
    .await
    .ok()
    .and_then(Result::ok)
}

pub(super) fn merge(
    relay_events: &mut Vec<StoredNostrEvent>,
    report: Option<QueryReport>,
    filter: &Filter,
    policy: &CatchupPolicy,
) -> Result<Receipt> {
    let Some(report) = report else {
        return Ok(Receipt {
            status: Status::Unavailable,
            events: 0,
            added_events: 0,
            sources: BTreeMap::new(),
            event_ids_sha256: hex::encode(Sha256::digest([])),
        });
    };
    ensure!(
        report.events.len() <= policy.max_events_per_author.saturating_add(1),
        "pubsub event budget exhausted"
    );
    let mut peer_events = BTreeMap::new();
    let mut peer_bytes = 0usize;
    let mut sources = BTreeMap::new();
    for entry in report.events {
        // A local transit daemon may also offer its own history. Only explicitly
        // selected, authenticated service identities belong to this intake policy.
        if entry.source.kind != nostr_pubsub::EventSourceKind::FipsEndpoint
            || !policy
                .pubsub_peers
                .iter()
                .any(|peer| peer == entry.source.id.as_str())
        {
            continue;
        }
        let event = entry.event.as_event();
        ensure!(
            filter.match_event(event, nostr_pubsub::MatchEventOptions::default()),
            "pubsub event outside requested author/kind/time filter"
        );
        // VerifiedEvent is the pubsub trust boundary; conversion preserves the
        // signed body and the normal index append path validates it again.
        let event = stored_event_from_nostr_sdk_event(event);
        if !peer_events.contains_key(&event.id) {
            peer_bytes = peer_bytes.saturating_add(serde_json::to_vec(&event)?.len());
            ensure!(
                peer_events.len() < policy.max_events_per_author
                    && peer_bytes <= policy.max_bytes_per_author,
                "pubsub event/byte budget exhausted"
            );
            *sources
                .entry(entry.source.id.as_str().to_owned())
                .or_insert(0usize) += 1;
            peer_events.insert(event.id.clone(), event);
        }
    }
    let mut digest = Sha256::new();
    for id in peer_events.keys() {
        digest.update(id.as_bytes());
    }
    let mut receipt = Receipt {
        status: Status::Observed,
        events: peer_events.len(),
        added_events: 0,
        sources,
        event_ids_sha256: hex::encode(digest.finalize()),
    };
    // Validate the union before mutating a successful relay result. Keep the
    // payloads owned once: two author slots already retain their bounded batches.
    let existing: BTreeSet<_> = relay_events.iter().map(|event| event.id.as_str()).collect();
    peer_events.retain(|id, _| !existing.contains(id.as_str()));
    let mut bytes = 0usize;
    for event in relay_events.iter().chain(peer_events.values()) {
        bytes = bytes.saturating_add(serde_json::to_vec(event)?.len());
    }
    ensure!(
        relay_events.len().saturating_add(peer_events.len()) <= policy.max_events_per_author
            && bytes <= policy.max_bytes_per_author,
        "combined relay/pubsub event/byte budget exhausted"
    );
    receipt.added_events = peer_events.len();
    relay_events.extend(peer_events.into_values());
    // Relay catch-up returns ID order; keep the same deterministic append order.
    relay_events.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(receipt)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod selection_tests;
