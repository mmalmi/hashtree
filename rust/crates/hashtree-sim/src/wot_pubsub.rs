//! Web-of-trust pubsub simulation for machine-authored peer ratings.
//!
//! The simulation keeps the wire shape boring on purpose: rating records are
//! stored as normal Nostr fact-shaped events, and historic lookup is exercised
//! through the same hashtree-backed Nostr event index used by production code.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use crate::mesh_pubsub::{
    compute_workload_peer_graph, run_mesh_pubsub_htl_baseline_on_graph,
    run_mesh_pubsub_payload_delivery, HtlBaselineMode, MeshPubsubPayloadConfig,
    MeshPubsubWorkloadConfig,
};
use hashtree_core::{sha256, to_hex, Cid, MemoryStore};
use hashtree_network::{PoolConfig, MESH_EVENT_POLICY};
use hashtree_nostr::{
    parse_hashtree_root_event, ListEventsOptions, NostrEventStore, NostrEventStoreError,
    StoredNostrEvent, VerifiedStoredNostrEvent, HASHTREE_LABEL, HASHTREE_ROOT_KIND, TAG_HASH,
    TAG_KEY,
};
use nostr::nips::nip19::ToBech32;
use nostr::{Alphabet, Event, EventBuilder, Filter, Keys, Kind, SingleLetterTag, Tag, Timestamp};
use serde::{Deserialize, Serialize};

const FACT_OP_KIND: u32 = 7368;
const LOCAL_RATER: &str = "peer:local";
const TRUSTED_RATING_SIGNER: &str = "peer:trusted-crawler";
const INDEX_LABEL: &str = "nostr-event-index";
const HISTORY_QUERY_LIMIT: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RatingHistoryLookupMode {
    /// Reply to a normal Nostr filter query with raw matching rating events.
    RawEvents,
    /// Reply to a normal Nostr filter query with a hashtree-root event for the
    /// matching rating-event index; the requester then seeks that index.
    IndexRootEvents,
}

#[derive(Debug, Clone)]
pub struct WotPubsubSimConfig {
    pub scope: String,
    pub trusted_rating_authors: Vec<String>,
    pub good_peer_count: usize,
    pub bad_peer_count: usize,
    pub newcomer_count: usize,
    pub rounds: usize,
    pub event_capacity_per_round: usize,
    pub newcomer_probe_slots: usize,
    pub degradation_round: usize,
    pub rating_ingest_capacity_per_round: usize,
    pub spam_rating_events_per_round: usize,
    pub rating_pubsub_node_count: usize,
    pub rating_pubsub_subscribers: usize,
    pub rating_pubsub_publish_round_cap: usize,
    pub rating_pubsub_payload_bytes: usize,
}

impl Default for WotPubsubSimConfig {
    fn default() -> Self {
        Self {
            scope: "fips.peer".to_string(),
            trusted_rating_authors: vec![pubkey_for_node(TRUSTED_RATING_SIGNER)],
            good_peer_count: 4,
            bad_peer_count: 4,
            newcomer_count: 2,
            rounds: 7,
            event_capacity_per_round: 5,
            newcomer_probe_slots: 1,
            degradation_round: 2,
            rating_ingest_capacity_per_round: 16,
            spam_rating_events_per_round: 24,
            rating_pubsub_node_count: 24,
            rating_pubsub_subscribers: 12,
            rating_pubsub_publish_round_cap: 4,
            rating_pubsub_payload_bytes: 512,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WotPubsubSimReport {
    pub scope: String,
    pub rounds: usize,
    pub trusted_rating_author_count: u64,
    pub rating_events_published: u64,
    pub trusted_rating_events_accepted: u64,
    pub trusted_rating_events_deferred: u64,
    pub spam_rating_events_seen: u64,
    pub spam_rating_events_dropped: u64,
    pub good_delivered: u64,
    pub bad_delivered: u64,
    pub bad_events_limited: u64,
    pub newcomer_delivered_before_rating: u64,
    pub newcomer_delivered_after_rating: u64,
    pub newcomer_positive_ratings: u64,
    pub degraded_delivered_before_degradation: u64,
    pub degraded_bad_deliveries_before_penalty: u64,
    pub degraded_delivered_after_penalty: u64,
    pub degraded_penalty_ratings: u64,
    pub raw_rating_lookup_events: u64,
    pub index_root_lookup_events: u64,
    pub index_root_seek_events: u64,
    pub rating_pubsub_delivery_opportunities: u64,
    pub rating_pubsub_delivered_events: u64,
    pub rating_pubsub_verified_events: u64,
    pub rating_pubsub_history_lookup_events: u64,
    pub rating_pubsub_forwarded_bytes_sent: u64,
    pub rating_pubsub_flood_forwarded_bytes_sent: u64,
    pub latest_index_root: Option<String>,
    pub lookup_modes_exercised: Vec<RatingHistoryLookupMode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WotRatingRecord {
    pub event: StoredNostrEvent,
}

impl WotRatingRecord {
    fn new(
        rater: impl Into<String>,
        subject: impl Into<String>,
        scope: impl Into<String>,
        rating: i64,
        created_at: u64,
        reason: impl Into<String>,
    ) -> Self {
        Self::new_with_signer(
            TRUSTED_RATING_SIGNER,
            rater,
            subject,
            scope,
            rating,
            created_at,
            reason,
            ["machine", "peer"],
        )
    }

    fn new_with_signer<const N: usize>(
        signer: impl AsRef<str>,
        rater: impl Into<String>,
        subject: impl Into<String>,
        scope: impl Into<String>,
        rating: i64,
        created_at: u64,
        reason: impl Into<String>,
        tags: [&str; N],
    ) -> Self {
        let rater = rater.into();
        let subject = subject.into();
        let scope = scope.into();
        let created_at = created_at.max(1);
        let event = signed_rating_event(
            signer.as_ref(),
            &rater,
            &subject,
            &scope,
            rating,
            created_at,
            reason.into(),
            &tags,
        )
        .expect("sim rating event signs and verifies");
        Self { event }
    }

    #[cfg(test)]
    fn signed_by(self, signer: impl AsRef<str>) -> Self {
        let tags = self.tag_values("tag");
        let tag_refs = tags.iter().map(String::as_str).collect::<Vec<_>>();
        let event = signed_rating_event(
            signer.as_ref(),
            &self.rater().expect("rating rater tag exists"),
            &self.subject().expect("rating subject tag exists"),
            &self.scope().expect("rating scope tag exists"),
            self.rating().expect("rating tag exists"),
            self.created_at(),
            self.reason().unwrap_or_else(|| "rating".to_string()),
            &tag_refs,
        )
        .expect("sim rating event re-signs and verifies");
        Self { event }
    }

    fn normalized_score(&self) -> Result<i64, NostrEventStoreError> {
        let rating = self.rating()?;
        let min_rating = self.min_rating()?;
        let max_rating = self.max_rating()?;
        if min_rating >= max_rating {
            return Err(NostrEventStoreError::Validation(format!(
                "rating range must have min_rating < max_rating (got {}..{})",
                min_rating, max_rating
            )));
        }
        if rating < min_rating || rating > max_rating {
            return Err(NostrEventStoreError::Validation(format!(
                "rating {} is outside range {}..{}",
                rating, min_rating, max_rating
            )));
        }
        let rating = i128::from(rating);
        let min = i128::from(min_rating);
        let max = i128::from(max_rating);
        let centered = rating.saturating_mul(2) - min - max;
        Ok(((centered.saturating_mul(100)) / (max - min)) as i64)
    }

    fn to_stored_event(&self) -> Result<StoredNostrEvent, NostrEventStoreError> {
        VerifiedStoredNostrEvent::try_from(self.event.clone())
            .map(VerifiedStoredNostrEvent::into_stored)
    }

    fn signer(&self) -> &str {
        &self.event.pubkey
    }

    #[cfg(test)]
    fn rater(&self) -> Option<String> {
        self.tag_value("rater")
    }

    fn subject(&self) -> Option<String> {
        self.tag_value("subject")
    }

    fn scope(&self) -> Option<String> {
        self.tag_value("scope")
    }

    #[cfg(test)]
    fn reason(&self) -> Option<String> {
        self.tag_value("reason")
    }

    fn created_at(&self) -> u64 {
        self.tag_value("created_at")
            .and_then(|created_at| created_at.parse::<u64>().ok())
            .unwrap_or(self.event.created_at)
    }

    fn rating(&self) -> Result<i64, NostrEventStoreError> {
        self.tag_i64("rating")
    }

    fn min_rating(&self) -> Result<i64, NostrEventStoreError> {
        self.tag_i64("min_rating")
    }

    fn max_rating(&self) -> Result<i64, NostrEventStoreError> {
        self.tag_i64("max_rating")
    }

    fn tag_i64(&self, key: &str) -> Result<i64, NostrEventStoreError> {
        let value = self.tag_value(key).ok_or_else(|| {
            NostrEventStoreError::Validation(format!("rating event is missing {key} tag"))
        })?;
        value.parse::<i64>().map_err(|error| {
            NostrEventStoreError::Validation(format!("rating event has invalid {key}: {error}"))
        })
    }

    fn tag_value(&self, key: &str) -> Option<String> {
        self.tag_values(key).into_iter().next()
    }

    fn tag_values(&self, key: &str) -> Vec<String> {
        tag_values(&self.event, key)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeerRole {
    Good,
    Bad,
    Newcomer,
    Degrading,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SimPeer {
    id: String,
    role: PeerRole,
}

impl SimPeer {
    fn emits_bad_event(&self, round: usize, degradation_round: usize) -> bool {
        matches!(self.role, PeerRole::Bad)
            || (matches!(self.role, PeerRole::Degrading) && round >= degradation_round)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RatingCandidate {
    rating: WotRatingRecord,
}

impl RatingCandidate {
    fn new(rating: WotRatingRecord) -> Self {
        Self { rating }
    }
}

struct WotRatingHistory {
    event_store: NostrEventStore<MemoryStore>,
    rating_root: Option<Cid>,
    index_root: Option<Cid>,
    root_event_sequence: u64,
}

impl WotRatingHistory {
    fn new() -> Self {
        let store = Arc::new(MemoryStore::new());
        Self {
            event_store: NostrEventStore::new(store),
            rating_root: None,
            index_root: None,
            root_event_sequence: 0,
        }
    }

    async fn publish_rating(
        &mut self,
        rating: &WotRatingRecord,
    ) -> Result<(), NostrEventStoreError> {
        let event = rating.to_stored_event()?;
        self.rating_root = Some(
            self.event_store
                .add(self.rating_root.as_ref(), event)
                .await?,
        );
        if let Some(scope) = rating.scope() {
            self.publish_index_root(&scope, rating.created_at()).await?;
        }
        Ok(())
    }

    async fn publish_stored_rating_event(
        &mut self,
        event: StoredNostrEvent,
    ) -> Result<(), NostrEventStoreError> {
        let scope = tag_values(&event, "scope").into_iter().next();
        let created_at = tag_values(&event, "created_at")
            .into_iter()
            .next()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(event.created_at);
        self.rating_root = Some(
            self.event_store
                .add(self.rating_root.as_ref(), event)
                .await?,
        );
        if let Some(scope) = scope {
            self.publish_index_root(&scope, created_at).await?;
        }
        Ok(())
    }

    async fn publish_index_root(
        &mut self,
        scope: &str,
        created_at: u64,
    ) -> Result<(), NostrEventStoreError> {
        let Some(root) = &self.rating_root else {
            return Ok(());
        };
        self.root_event_sequence = self.root_event_sequence.saturating_add(1);
        let event = index_root_event(scope, root, created_at + self.root_event_sequence)?;
        self.index_root = Some(
            self.event_store
                .add(self.index_root.as_ref(), event)
                .await?,
        );
        Ok(())
    }

    async fn query_with_nostr_filter(
        &self,
        mode: RatingHistoryLookupMode,
        scope: &str,
    ) -> Result<Vec<StoredNostrEvent>, NostrEventStoreError> {
        match mode {
            RatingHistoryLookupMode::RawEvents => {
                let filter = scope_filter(FACT_OP_KIND, scope);
                self.event_store
                    .query_events(self.rating_root.as_ref(), &filter, HISTORY_QUERY_LIMIT)
                    .await
            }
            RatingHistoryLookupMode::IndexRootEvents => {
                let filter = scope_filter(HASHTREE_ROOT_KIND, scope);
                self.event_store
                    .query_events(self.index_root.as_ref(), &filter, HISTORY_QUERY_LIMIT)
                    .await
            }
        }
    }

    async fn seek_index_root_event(
        &self,
        event: &StoredNostrEvent,
        scope: &str,
    ) -> Result<(Cid, Vec<StoredNostrEvent>), NostrEventStoreError> {
        let parsed = parse_hashtree_root_event(event)?.ok_or_else(|| {
            NostrEventStoreError::Validation("index-root query did not return a root event".into())
        })?;
        let events = self
            .event_store
            .list_by_tag(
                Some(&parsed.root_cid),
                "scope",
                scope,
                ListEventsOptions::default(),
            )
            .await?;
        Ok((parsed.root_cid, events))
    }
}

pub async fn run_wot_pubsub_simulation(
    config: WotPubsubSimConfig,
) -> Result<WotPubsubSimReport, NostrEventStoreError> {
    let scope = config.scope.trim().to_string();
    if scope.is_empty() {
        return Err(NostrEventStoreError::Validation(
            "rating scope must not be empty".to_string(),
        ));
    }

    let mut report = WotPubsubSimReport {
        scope: scope.clone(),
        rounds: config.rounds,
        trusted_rating_author_count: trusted_rating_authors(&config).len() as u64,
        lookup_modes_exercised: vec![
            RatingHistoryLookupMode::RawEvents,
            RatingHistoryLookupMode::IndexRootEvents,
        ],
        ..Default::default()
    };
    let peers = build_peers(&config);
    let mut scores = HashMap::<String, i64>::new();
    let mut history = WotRatingHistory::new();
    let mut rating_time = 1_u64;
    let mut initial_ratings = Vec::new();

    for peer in &peers {
        let initial = match peer.role {
            PeerRole::Good => Some((90, "good peer observed")),
            PeerRole::Bad => Some((0, "bad peer observed")),
            PeerRole::Degrading => Some((95, "good peer observed")),
            PeerRole::Newcomer => None,
        };
        if let Some((score, reason)) = initial {
            initial_ratings.push(RatingCandidate::new(WotRatingRecord::new(
                local_rater_npub(),
                &peer.id,
                &scope,
                score,
                take_rating_time(&mut rating_time),
                reason,
            )));
        }
    }
    append_spam_rating_candidates(
        &mut initial_ratings,
        &scope,
        "initial",
        config.spam_rating_events_per_round,
        &mut rating_time,
    );
    let trusted_rating_authors = trusted_rating_authors(&config);
    ingest_rating_candidates(
        &mut history,
        &mut scores,
        &mut report,
        initial_ratings,
        config.rating_ingest_capacity_per_round,
        &trusted_rating_authors,
    )
    .await?;

    for round in 0..config.rounds {
        let selected = select_publishers(
            &peers,
            &scores,
            config.event_capacity_per_round,
            config.newcomer_probe_slots,
        );
        let mut pending_ratings = Vec::new();

        for peer in &peers {
            let selected_peer = selected.contains(&peer.id);
            let bad_event = peer.emits_bad_event(round, config.degradation_round);
            if !selected_peer {
                if bad_event {
                    report.bad_events_limited = report.bad_events_limited.saturating_add(1);
                }
                continue;
            }

            match peer.role {
                PeerRole::Good => {
                    report.good_delivered = report.good_delivered.saturating_add(1);
                }
                PeerRole::Bad => {
                    report.bad_delivered = report.bad_delivered.saturating_add(1);
                }
                PeerRole::Newcomer => {
                    if scores.contains_key(&peer.id) {
                        report.newcomer_delivered_after_rating =
                            report.newcomer_delivered_after_rating.saturating_add(1);
                    } else {
                        report.newcomer_delivered_before_rating =
                            report.newcomer_delivered_before_rating.saturating_add(1);
                        report.newcomer_positive_ratings =
                            report.newcomer_positive_ratings.saturating_add(1);
                        pending_ratings.push(RatingCandidate::new(WotRatingRecord::new(
                            local_rater_npub(),
                            &peer.id,
                            &scope,
                            75,
                            take_rating_time(&mut rating_time),
                            "newcomer served useful traffic",
                        )));
                    }
                }
                PeerRole::Degrading => {
                    if round < config.degradation_round {
                        report.degraded_delivered_before_degradation = report
                            .degraded_delivered_before_degradation
                            .saturating_add(1);
                    } else if scores.get(&peer.id).is_some_and(|score| *score < 0) {
                        report.degraded_delivered_after_penalty =
                            report.degraded_delivered_after_penalty.saturating_add(1);
                    } else {
                        report.degraded_bad_deliveries_before_penalty = report
                            .degraded_bad_deliveries_before_penalty
                            .saturating_add(1);
                        report.degraded_penalty_ratings =
                            report.degraded_penalty_ratings.saturating_add(1);
                        pending_ratings.push(RatingCandidate::new(WotRatingRecord::new(
                            local_rater_npub(),
                            &peer.id,
                            &scope,
                            0,
                            take_rating_time(&mut rating_time),
                            "peer degraded after prior good history",
                        )));
                    }
                }
            }
        }

        append_spam_rating_candidates(
            &mut pending_ratings,
            &scope,
            &format!("round:{round}"),
            config.spam_rating_events_per_round,
            &mut rating_time,
        );
        ingest_rating_candidates(
            &mut history,
            &mut scores,
            &mut report,
            pending_ratings,
            config.rating_ingest_capacity_per_round,
            &trusted_rating_authors,
        )
        .await?;
    }

    let raw_events = history
        .query_with_nostr_filter(RatingHistoryLookupMode::RawEvents, &scope)
        .await?;
    report.raw_rating_lookup_events = raw_events.len() as u64;

    let index_root_events = history
        .query_with_nostr_filter(RatingHistoryLookupMode::IndexRootEvents, &scope)
        .await?;
    report.index_root_lookup_events = index_root_events.len() as u64;
    if let Some(root_event) = index_root_events.first() {
        let (root, events) = history.seek_index_root_event(root_event, &scope).await?;
        report.latest_index_root = Some(root.to_string());
        report.index_root_seek_events = events.len() as u64;
    }
    simulate_rating_pubsub_delivery(&config, &scope, &raw_events, &mut report).await?;

    Ok(report)
}

async fn simulate_rating_pubsub_delivery(
    config: &WotPubsubSimConfig,
    scope: &str,
    rating_events: &[StoredNostrEvent],
    report: &mut WotPubsubSimReport,
) -> Result<(), NostrEventStoreError> {
    if report.rating_events_published == 0
        || config.rating_pubsub_node_count < 2
        || config.rating_pubsub_subscribers == 0
        || config.rating_pubsub_publish_round_cap == 0
    {
        return Ok(());
    }

    let payload_events = rating_events
        .iter()
        .take(config.rating_pubsub_publish_round_cap)
        .cloned()
        .collect::<Vec<_>>();
    if payload_events.is_empty() {
        return Ok(());
    }
    let payloads = payload_events
        .iter()
        .map(serde_json::to_vec)
        .collect::<Result<Vec<_>, _>>()?;
    let payload_bytes = payloads.iter().map(Vec::len).max().unwrap_or(1).max(1);
    let publish_rounds = payloads.len();
    let subscriber_count = config
        .rating_pubsub_subscribers
        .min(config.rating_pubsub_node_count.saturating_sub(1));
    if subscriber_count == 0 {
        return Ok(());
    }

    let pubsub_config = MeshPubsubWorkloadConfig {
        seed: 20260704,
        node_count: config.rating_pubsub_node_count,
        author_count: 1,
        subscribers_per_author: subscriber_count,
        publish_rounds,
        payload_bytes,
        pool: PoolConfig {
            max_connections: 8,
            satisfied_connections: 4,
        },
        spam_author_count: 0,
        spam_subscribers_per_author: 0,
        spam_publish_rounds_per_round: 0,
        ..Default::default()
    };
    let payload_report = run_mesh_pubsub_payload_delivery(MeshPubsubPayloadConfig {
        seed: 20260705,
        node_count: config.rating_pubsub_node_count,
        subscriber_count,
        stream_id: format!("nostr-ratings:{scope}"),
        payloads,
        pool: PoolConfig {
            max_connections: 8,
            satisfied_connections: 4,
        },
        pump_steps_after_setup: 192,
        pump_steps_per_publish: 128,
    })
    .await;
    report.rating_pubsub_delivery_opportunities = payload_report.delivery_opportunities;
    report.rating_pubsub_delivered_events = payload_report.delivered_payloads.len() as u64;
    report.rating_pubsub_forwarded_bytes_sent = payload_report.forwarded_bytes_sent;

    let mut subscriber_history = WotRatingHistory::new();
    let mut seen_event_ids = BTreeSet::new();
    for delivery in payload_report.delivered_payloads {
        let event: StoredNostrEvent = serde_json::from_slice(&delivery.payload)?;
        let verified = VerifiedStoredNostrEvent::try_from(event)?;
        if seen_event_ids.insert(verified.as_stored().id.clone()) {
            subscriber_history
                .publish_stored_rating_event(verified.into_stored())
                .await?;
        }
    }
    report.rating_pubsub_verified_events = seen_event_ids.len() as u64;
    report.rating_pubsub_history_lookup_events = subscriber_history
        .query_with_nostr_filter(RatingHistoryLookupMode::RawEvents, scope)
        .await?
        .len() as u64;

    let (graph, topology) = compute_workload_peer_graph(&pubsub_config).await;
    let flood = run_mesh_pubsub_htl_baseline_on_graph(
        &graph,
        &topology,
        &pubsub_config,
        MESH_EVENT_POLICY.max_htl,
        HtlBaselineMode::FloodPayload,
    );

    report.rating_pubsub_flood_forwarded_bytes_sent = flood.forwarded_bytes_sent;
    Ok(())
}

async fn ingest_rating_candidates(
    history: &mut WotRatingHistory,
    scores: &mut HashMap<String, i64>,
    report: &mut WotPubsubSimReport,
    candidates: Vec<RatingCandidate>,
    capacity: usize,
    trusted_rating_authors: &BTreeSet<String>,
) -> Result<(), NostrEventStoreError> {
    let mut trusted = Vec::new();
    let mut untrusted = Vec::new();
    for candidate in candidates {
        if trusted_rating_authors.contains(candidate.rating.signer()) {
            trusted.push(candidate);
        } else {
            untrusted.push(candidate);
        }
    }

    report.spam_rating_events_seen = report
        .spam_rating_events_seen
        .saturating_add(untrusted.len() as u64);

    for (index, candidate) in trusted.into_iter().chain(untrusted).enumerate() {
        if index >= capacity {
            if trusted_rating_authors.contains(candidate.rating.signer()) {
                report.trusted_rating_events_deferred =
                    report.trusted_rating_events_deferred.saturating_add(1);
            } else {
                report.spam_rating_events_dropped =
                    report.spam_rating_events_dropped.saturating_add(1);
            }
            continue;
        }

        if trusted_rating_authors.contains(candidate.rating.signer()) {
            publish_rating(history, scores, report, candidate.rating).await?;
            report.trusted_rating_events_accepted =
                report.trusted_rating_events_accepted.saturating_add(1);
        } else {
            report.spam_rating_events_dropped = report.spam_rating_events_dropped.saturating_add(1);
        }
    }

    Ok(())
}

fn trusted_rating_authors(config: &WotPubsubSimConfig) -> BTreeSet<String> {
    config
        .trusted_rating_authors
        .iter()
        .map(|author| author.trim())
        .filter(|author| !author.is_empty())
        .map(|author| author.to_lowercase())
        .collect()
}

fn append_spam_rating_candidates(
    candidates: &mut Vec<RatingCandidate>,
    scope: &str,
    phase: &str,
    count: usize,
    rating_time: &mut u64,
) {
    for index in 0..count {
        let subject = node_npub(&format!("peer:spam-subject:{phase}:{index:04}"));
        let rating = WotRatingRecord::new_with_signer(
            format!("peer:spam-signer:{phase}:{index:04}"),
            node_npub(&format!("peer:spam-rater:{phase}:{index:04}")),
            subject,
            scope,
            100,
            take_rating_time(rating_time),
            "untrusted rating spam",
            ["machine", "peer", "spam"],
        );
        candidates.push(RatingCandidate::new(rating));
    }
}

fn take_rating_time(rating_time: &mut u64) -> u64 {
    let current = (*rating_time).max(1);
    *rating_time = current.saturating_add(1);
    current
}

async fn publish_rating(
    history: &mut WotRatingHistory,
    scores: &mut HashMap<String, i64>,
    report: &mut WotPubsubSimReport,
    rating: WotRatingRecord,
) -> Result<(), NostrEventStoreError> {
    let score = rating.normalized_score()?;
    let subject = rating.subject().ok_or_else(|| {
        NostrEventStoreError::Validation("rating event is missing subject tag".to_string())
    })?;
    scores.insert(subject, score);
    history.publish_rating(&rating).await?;
    report.rating_events_published = report.rating_events_published.saturating_add(1);
    Ok(())
}

fn build_peers(config: &WotPubsubSimConfig) -> Vec<SimPeer> {
    let mut peers = Vec::new();
    for index in 0..config.good_peer_count {
        peers.push(SimPeer {
            id: node_npub(&format!("peer:good:{index:04}")),
            role: PeerRole::Good,
        });
    }
    for index in 0..config.bad_peer_count {
        peers.push(SimPeer {
            id: node_npub(&format!("peer:bad:{index:04}")),
            role: PeerRole::Bad,
        });
    }
    for index in 0..config.newcomer_count {
        peers.push(SimPeer {
            id: node_npub(&format!("peer:new:{index:04}")),
            role: PeerRole::Newcomer,
        });
    }
    peers.push(SimPeer {
        id: node_npub("peer:degrading:0000"),
        role: PeerRole::Degrading,
    });
    peers
}

fn select_publishers(
    peers: &[SimPeer],
    scores: &HashMap<String, i64>,
    capacity: usize,
    newcomer_probe_slots: usize,
) -> BTreeSet<String> {
    let mut unknown = Vec::new();
    let mut positive = Vec::new();
    let mut negative = Vec::new();

    for peer in peers {
        match scores.get(&peer.id).copied() {
            Some(score) if score < 0 => negative.push((score, peer.id.clone())),
            Some(score) => positive.push((score, peer.id.clone())),
            None => unknown.push(peer.id.clone()),
        }
    }

    unknown.sort();
    positive.sort_by(|left, right| right.cmp(left));
    negative.sort_by(|left, right| right.cmp(left));

    let mut selected = BTreeSet::new();
    for id in unknown.iter().take(newcomer_probe_slots) {
        if selected.len() >= capacity {
            return selected;
        }
        selected.insert(id.clone());
    }
    for (_score, id) in positive {
        if selected.len() >= capacity {
            return selected;
        }
        selected.insert(id);
    }
    for id in unknown.into_iter().skip(newcomer_probe_slots) {
        if selected.len() >= capacity {
            return selected;
        }
        selected.insert(id);
    }
    for (_score, id) in negative {
        if selected.len() >= capacity {
            return selected;
        }
        selected.insert(id);
    }
    selected
}

fn index_root_event(
    scope: &str,
    root: &Cid,
    created_at: u64,
) -> Result<StoredNostrEvent, NostrEventStoreError> {
    let mut tags = vec![
        vec!["d".to_string(), format!("wot-rating-index:{scope}")],
        vec!["l".to_string(), HASHTREE_LABEL.to_string()],
        vec!["l".to_string(), INDEX_LABEL.to_string()],
        vec!["scope".to_string(), scope.to_string()],
        vec!["i".to_string(), scope.to_lowercase()],
        vec!["kind".to_string(), FACT_OP_KIND.to_string()],
        vec![
            "filter".to_string(),
            format!("kind={FACT_OP_KIND};scope={scope}"),
        ],
        vec![TAG_HASH.to_string(), to_hex(&root.hash)],
    ];
    if let Some(key) = root.key {
        tags.push(vec![TAG_KEY.to_string(), to_hex(&key)]);
    }
    signed_stored_event(LOCAL_RATER, created_at.max(1), HASHTREE_ROOT_KIND, tags, "")
}

fn scope_filter(kind: u32, scope: &str) -> Filter {
    Filter::new().kind(Kind::from(kind as u16)).custom_tag(
        SingleLetterTag::lowercase(Alphabet::I),
        scope.to_lowercase(),
    )
}

fn signed_rating_event(
    signer: &str,
    rater: &str,
    subject: &str,
    scope: &str,
    rating: i64,
    created_at: u64,
    reason: String,
    rating_tags: &[&str],
) -> Result<StoredNostrEvent, NostrEventStoreError> {
    let created_at = created_at.max(1);
    let id = format!("rating:{scope}:{subject}:{created_at}");
    let mut tags = vec![
        vec!["d".to_string(), id.clone()],
        vec!["i".to_string(), id, "subject".to_string()],
        vec!["i".to_string(), rater.to_lowercase()],
        vec!["i".to_string(), subject.to_lowercase()],
        vec!["i".to_string(), scope.to_lowercase()],
        vec!["type".to_string(), "rating".to_string()],
        vec!["schema".to_string(), "1".to_string()],
        vec!["created_at".to_string(), created_at.to_string()],
        vec!["rater".to_string(), rater.to_string()],
        vec!["subject".to_string(), subject.to_string()],
        vec!["scope".to_string(), scope.to_string()],
        vec!["rating".to_string(), rating.to_string()],
        vec!["min_rating".to_string(), "0".to_string()],
        vec!["max_rating".to_string(), "100".to_string()],
        vec!["sample_count".to_string(), "1".to_string()],
        vec!["window_end".to_string(), created_at.to_string()],
        vec!["reason".to_string(), reason],
    ];
    for tag in rating_tags {
        tags.push(vec!["tag".to_string(), (*tag).to_string()]);
        tags.push(vec!["i".to_string(), tag.to_lowercase()]);
    }
    signed_stored_event(signer, created_at, FACT_OP_KIND, tags, "")
}

fn signed_stored_event(
    signer: &str,
    created_at: u64,
    kind: u32,
    tags: Vec<Vec<String>>,
    content: impl Into<String>,
) -> Result<StoredNostrEvent, NostrEventStoreError> {
    let content = content.into();
    let parsed_tags = tags
        .into_iter()
        .map(|tag| {
            Tag::parse(tag).map_err(|error| {
                NostrEventStoreError::Validation(format!("invalid Nostr tag: {error}"))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let event = EventBuilder::new(Kind::from(kind as u16), content)
        .tags(parsed_tags)
        .custom_created_at(Timestamp::from(created_at.max(1)))
        .sign_with_keys(&keys_for_node(signer))
        .map_err(|error| {
            NostrEventStoreError::Validation(format!("failed to sign Nostr event: {error}"))
        })?;
    stored_event_from_signed_event(event)
}

fn stored_event_from_signed_event(event: Event) -> Result<StoredNostrEvent, NostrEventStoreError> {
    event.verify().map_err(|error| {
        NostrEventStoreError::Validation(format!("signature verification failed: {error}"))
    })?;
    let event: StoredNostrEvent = serde_json::from_value(serde_json::to_value(event)?)?;
    VerifiedStoredNostrEvent::try_from(event).map(VerifiedStoredNostrEvent::into_stored)
}

fn pubkey_for_node(id: &str) -> String {
    keys_for_node(id).public_key().to_hex()
}

fn node_npub(id: &str) -> String {
    keys_for_node(id)
        .public_key()
        .to_bech32()
        .expect("sim npub")
}

fn local_rater_npub() -> String {
    node_npub(LOCAL_RATER)
}

fn keys_for_node(id: &str) -> Keys {
    let secret = to_hex(&sha256(id.as_bytes()));
    Keys::parse(&secret).expect("deterministic sim key parses")
}

fn tag_values(event: &StoredNostrEvent, key: &str) -> Vec<String> {
    event
        .tags
        .iter()
        .filter_map(|tag| {
            if tag.first().is_some_and(|tag_key| tag_key == key) {
                tag.get(1).cloned()
            } else {
                None
            }
        })
        .filter(|value| !value.trim().is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wot_prioritizes_good_limits_bad_and_recovers_newcomers() {
        let report = run_wot_pubsub_simulation(WotPubsubSimConfig::default())
            .await
            .unwrap();

        assert_eq!(report.scope, "fips.peer");
        assert_eq!(report.trusted_rating_author_count, 1);
        assert!(report.good_delivered > report.bad_delivered);
        assert_eq!(report.bad_delivered, 0);
        assert!(report.bad_events_limited > 0);
        assert!(report.newcomer_delivered_before_rating > 0);
        assert!(report.newcomer_positive_ratings > 0);
        assert!(report.newcomer_delivered_after_rating > report.newcomer_delivered_before_rating);
        assert!(report.degraded_delivered_before_degradation > 0);
        assert_eq!(report.degraded_bad_deliveries_before_penalty, 1);
        assert_eq!(report.degraded_penalty_ratings, 1);
        assert_eq!(report.degraded_delivered_after_penalty, 0);
        assert_eq!(
            report.trusted_rating_events_accepted,
            report.rating_events_published
        );
        assert_eq!(report.trusted_rating_events_deferred, 0);
        assert!(report.spam_rating_events_seen > 0);
        assert_eq!(
            report.spam_rating_events_dropped,
            report.spam_rating_events_seen
        );
        assert!(report.rating_pubsub_delivery_opportunities > 0);
        assert_eq!(
            report.rating_pubsub_delivered_events,
            report.rating_pubsub_delivery_opportunities
        );
        assert!(report.rating_pubsub_verified_events > 0);
        assert!(report.rating_pubsub_verified_events <= report.rating_events_published);
        assert_eq!(
            report.rating_pubsub_history_lookup_events,
            report.rating_pubsub_verified_events
        );
        assert!(
            report.rating_pubsub_forwarded_bytes_sent
                < report.rating_pubsub_flood_forwarded_bytes_sent,
            "inv/want rating stream should spend fewer bytes than full-payload flood"
        );
    }

    #[tokio::test]
    async fn wot_rating_ingest_prioritizes_connected_authors_under_spam() {
        let report = run_wot_pubsub_simulation(WotPubsubSimConfig {
            good_peer_count: 1,
            bad_peer_count: 0,
            newcomer_count: 0,
            rounds: 1,
            event_capacity_per_round: 2,
            rating_ingest_capacity_per_round: 2,
            spam_rating_events_per_round: 50,
            degradation_round: 10,
            ..WotPubsubSimConfig::default()
        })
        .await
        .unwrap();

        assert_eq!(report.rating_events_published, 2);
        assert_eq!(report.trusted_rating_events_accepted, 2);
        assert_eq!(report.trusted_rating_events_deferred, 0);
        assert_eq!(report.spam_rating_events_seen, 100);
        assert_eq!(report.spam_rating_events_dropped, 100);
        assert_eq!(report.raw_rating_lookup_events, 2);
    }

    #[tokio::test]
    async fn wot_rating_ingest_trusts_event_signers_not_rater_facts() {
        let mut history = WotRatingHistory::new();
        let mut scores = HashMap::new();
        let mut report = WotPubsubSimReport::default();
        let trusted_authors = BTreeSet::from([pubkey_for_node(TRUSTED_RATING_SIGNER)]);
        let external_rater = node_npub("peer:external-reviewer");
        let trusted_subject = node_npub("peer:trusted-subject");
        let spam_subject = node_npub("peer:spam-subject");
        let trusted = WotRatingRecord::new(
            &external_rater,
            &trusted_subject,
            "fips.peer",
            90,
            1,
            "trusted crawler fact",
        )
        .signed_by(TRUSTED_RATING_SIGNER);
        let untrusted = WotRatingRecord::new(
            node_npub(TRUSTED_RATING_SIGNER),
            &spam_subject,
            "fips.peer",
            100,
            2,
            "untrusted crawler fact",
        )
        .signed_by("peer:untrusted-crawler");

        ingest_rating_candidates(
            &mut history,
            &mut scores,
            &mut report,
            vec![
                RatingCandidate::new(untrusted),
                RatingCandidate::new(trusted),
            ],
            8,
            &trusted_authors,
        )
        .await
        .unwrap();

        assert_eq!(report.trusted_rating_events_accepted, 1);
        assert_eq!(report.spam_rating_events_seen, 1);
        assert_eq!(report.spam_rating_events_dropped, 1);
        assert_eq!(scores.get(&trusted_subject), Some(&80));
        assert!(!scores.contains_key(&spam_subject));

        let events = history
            .query_with_nostr_filter(RatingHistoryLookupMode::RawEvents, "fips.peer")
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].pubkey, pubkey_for_node(TRUSTED_RATING_SIGNER));
        assert!(has_tag(&events[0], &["rater", &external_rater]));
    }

    #[tokio::test]
    async fn wot_history_lookup_supports_raw_events_and_index_root_events() {
        let report = run_wot_pubsub_simulation(WotPubsubSimConfig::default())
            .await
            .unwrap();

        assert_eq!(
            report.lookup_modes_exercised,
            vec![
                RatingHistoryLookupMode::RawEvents,
                RatingHistoryLookupMode::IndexRootEvents
            ]
        );
        assert_eq!(
            report.raw_rating_lookup_events,
            report.rating_events_published
        );
        assert_eq!(report.index_root_lookup_events, 1);
        assert_eq!(
            report.index_root_seek_events,
            report.raw_rating_lookup_events
        );
        assert!(report.latest_index_root.is_some());
    }

    #[tokio::test]
    async fn relayless_pubsub_delivered_rating_events_index_for_normal_filters() {
        let report = run_wot_pubsub_simulation(WotPubsubSimConfig {
            rounds: 1,
            rating_pubsub_node_count: 10,
            rating_pubsub_subscribers: 4,
            rating_pubsub_publish_round_cap: 3,
            ..WotPubsubSimConfig::default()
        })
        .await
        .unwrap();

        assert!(report.rating_pubsub_delivery_opportunities > 0);
        assert_eq!(
            report.rating_pubsub_delivered_events,
            report.rating_pubsub_delivery_opportunities
        );
        assert_eq!(report.rating_pubsub_verified_events, 3);
        assert_eq!(report.rating_pubsub_history_lookup_events, 3);
        assert_eq!(
            report.raw_rating_lookup_events,
            report.rating_events_published
        );
    }

    #[tokio::test]
    async fn wot_history_lookup_uses_scope_i_tag_filters() {
        let mut history = WotRatingHistory::new();
        let subject = node_npub("peer:subject");
        let rating = WotRatingRecord::new(
            local_rater_npub(),
            &subject,
            "fips.peer",
            80,
            10,
            "healthy test peer",
        );

        history.publish_rating(&rating).await.unwrap();

        let raw_events = history
            .query_with_nostr_filter(RatingHistoryLookupMode::RawEvents, "fips.peer")
            .await
            .unwrap();
        assert_eq!(raw_events.len(), 1);
        assert_eq!(raw_events[0].pubkey, pubkey_for_node(TRUSTED_RATING_SIGNER));
        assert!(has_tag(&raw_events[0], &["rater", &local_rater_npub()]));
        assert!(has_tag(&raw_events[0], &["i", "fips.peer"]));
        assert!(has_tag(&raw_events[0], &["type", "rating"]));
        assert!(has_tag(&raw_events[0], &["schema", "1"]));

        let index_root_events = history
            .query_with_nostr_filter(RatingHistoryLookupMode::IndexRootEvents, "fips.peer")
            .await
            .unwrap();
        assert_eq!(index_root_events.len(), 1);
        assert!(has_tag(&index_root_events[0], &["i", "fips.peer"]));

        assert!(history
            .query_with_nostr_filter(RatingHistoryLookupMode::RawEvents, "other.scope")
            .await
            .unwrap()
            .is_empty());
        assert!(history
            .query_with_nostr_filter(RatingHistoryLookupMode::IndexRootEvents, "other.scope")
            .await
            .unwrap()
            .is_empty());
    }

    fn has_tag(event: &StoredNostrEvent, expected: &[&str]) -> bool {
        event
            .tags
            .iter()
            .any(|tag| tag.iter().map(String::as_str).eq(expected.iter().copied()))
    }
}
