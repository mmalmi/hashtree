//! Web-of-trust pubsub simulation for machine-authored peer ratings.
//!
//! The simulation keeps the wire shape boring on purpose: rating records are
//! stored as normal Nostr fact-shaped events, and historic lookup is exercised
//! through the same hashtree-backed Nostr event index used by production code.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use crate::mesh_pubsub::{
    compute_workload_peer_graph, run_mesh_pubsub_htl_baseline_on_graph, HtlBaselineMode,
    MeshPubsubWorkloadConfig,
};
use hashtree_core::{sha256, to_hex, Cid, MemoryStore};
use hashtree_network::{PoolConfig, MESH_EVENT_POLICY};
use hashtree_nostr::{
    parse_hashtree_root_event, ListEventsOptions, NostrEventStore, NostrEventStoreError,
    StoredNostrEvent, HASHTREE_LABEL, HASHTREE_ROOT_KIND, TAG_HASH, TAG_KEY,
};
use nostr::{Alphabet, Filter, Kind, SingleLetterTag};
use serde::{Deserialize, Serialize};

const FACT_OP_KIND: u32 = 7368;
const LOCAL_RATER: &str = "peer:local";
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
            scope: "peer".to_string(),
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
    pub rating_pubsub_forwarded_bytes_sent: u64,
    pub rating_pubsub_flood_forwarded_bytes_sent: u64,
    pub latest_index_root: Option<String>,
    pub lookup_modes_exercised: Vec<RatingHistoryLookupMode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WotRatingRecord {
    pub id: String,
    pub rater: String,
    pub subject: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    pub rating: i64,
    pub min_rating: i64,
    pub max_rating: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_start: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_end: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    pub created_at: u64,
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
        let subject = subject.into();
        let created_at = created_at.max(1);
        Self {
            id: format!("rating:{subject}:{created_at}"),
            rater: rater.into(),
            subject,
            scope: Some(scope.into()),
            rating,
            min_rating: 0,
            max_rating: 100,
            sample_count: Some(1),
            window_start: None,
            window_end: Some(created_at),
            evidence: Vec::new(),
            reason: Some(reason.into()),
            tags: vec!["machine".to_string(), "peer".to_string()],
            created_at,
        }
    }

    fn normalized_score(&self) -> Result<i64, NostrEventStoreError> {
        if self.min_rating >= self.max_rating {
            return Err(NostrEventStoreError::Validation(format!(
                "rating range must have min_rating < max_rating (got {}..{})",
                self.min_rating, self.max_rating
            )));
        }
        if self.rating < self.min_rating || self.rating > self.max_rating {
            return Err(NostrEventStoreError::Validation(format!(
                "rating {} is outside range {}..{}",
                self.rating, self.min_rating, self.max_rating
            )));
        }
        let rating = i128::from(self.rating);
        let min = i128::from(self.min_rating);
        let max = i128::from(self.max_rating);
        let centered = rating.saturating_mul(2) - min - max;
        Ok(((centered.saturating_mul(100)) / (max - min)) as i64)
    }

    fn to_stored_event(&self) -> Result<StoredNostrEvent, NostrEventStoreError> {
        let mut tags = vec![
            vec!["d".to_string(), self.id.clone()],
            vec!["i".to_string(), self.id.clone(), "subject".to_string()],
            vec!["i".to_string(), self.rater.to_lowercase()],
            vec!["i".to_string(), self.subject.to_lowercase()],
            vec!["type".to_string(), "rating".to_string()],
            vec!["schema".to_string(), "1".to_string()],
            vec!["created_at".to_string(), self.created_at.to_string()],
            vec!["rater".to_string(), self.rater.clone()],
            vec!["subject".to_string(), self.subject.clone()],
            vec!["rating".to_string(), self.rating.to_string()],
            vec!["min_rating".to_string(), self.min_rating.to_string()],
            vec!["max_rating".to_string(), self.max_rating.to_string()],
        ];
        if let Some(scope) = self.scope.as_ref().filter(|scope| !scope.trim().is_empty()) {
            tags.push(vec!["scope".to_string(), scope.clone()]);
            tags.push(vec!["i".to_string(), scope.to_lowercase()]);
        }
        if let Some(sample_count) = self.sample_count {
            tags.push(vec!["sample_count".to_string(), sample_count.to_string()]);
        }
        if let Some(window_end) = self.window_end {
            tags.push(vec!["window_end".to_string(), window_end.to_string()]);
        }
        if let Some(reason) = &self.reason {
            tags.push(vec!["reason".to_string(), reason.clone()]);
        }
        for tag in &self.tags {
            tags.push(vec!["tag".to_string(), tag.clone()]);
            tags.push(vec!["i".to_string(), tag.to_lowercase()]);
        }

        let pubkey = pubkey_for_node(&self.rater);
        stored_event(pubkey, self.created_at, FACT_OP_KIND, tags, "")
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RatingCandidateTrust {
    Trusted,
    Untrusted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RatingCandidate {
    trust: RatingCandidateTrust,
    rating: WotRatingRecord,
}

impl RatingCandidate {
    fn trusted(rating: WotRatingRecord) -> Self {
        Self {
            trust: RatingCandidateTrust::Trusted,
            rating,
        }
    }

    fn untrusted(rating: WotRatingRecord) -> Self {
        Self {
            trust: RatingCandidateTrust::Untrusted,
            rating,
        }
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
        if let Some(scope) = rating.scope.as_deref() {
            self.publish_index_root(scope, rating.created_at).await?;
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
            initial_ratings.push(RatingCandidate::trusted(WotRatingRecord::new(
                LOCAL_RATER,
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
    ingest_rating_candidates(
        &mut history,
        &mut scores,
        &mut report,
        initial_ratings,
        config.rating_ingest_capacity_per_round,
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
                        pending_ratings.push(RatingCandidate::trusted(WotRatingRecord::new(
                            LOCAL_RATER,
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
                        pending_ratings.push(RatingCandidate::trusted(WotRatingRecord::new(
                            LOCAL_RATER,
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
    simulate_rating_pubsub_delivery(&config, &mut report).await;

    Ok(report)
}

async fn simulate_rating_pubsub_delivery(
    config: &WotPubsubSimConfig,
    report: &mut WotPubsubSimReport,
) {
    if report.rating_events_published == 0
        || config.rating_pubsub_node_count < 2
        || config.rating_pubsub_subscribers == 0
        || config.rating_pubsub_publish_round_cap == 0
    {
        return;
    }

    let publish_rounds = (report.rating_events_published as usize)
        .min(config.rating_pubsub_publish_round_cap)
        .max(1);
    let subscriber_count = config
        .rating_pubsub_subscribers
        .min(config.rating_pubsub_node_count.saturating_sub(1));
    if subscriber_count == 0 {
        return;
    }

    let pubsub_config = MeshPubsubWorkloadConfig {
        seed: 20260704,
        node_count: config.rating_pubsub_node_count,
        author_count: 1,
        subscribers_per_author: subscriber_count,
        publish_rounds,
        payload_bytes: config.rating_pubsub_payload_bytes.max(1),
        pool: PoolConfig {
            max_connections: 8,
            satisfied_connections: 4,
        },
        spam_author_count: 0,
        spam_subscribers_per_author: 0,
        spam_publish_rounds_per_round: 0,
        ..Default::default()
    };
    let (graph, topology) = compute_workload_peer_graph(&pubsub_config).await;
    let inv_want = run_mesh_pubsub_htl_baseline_on_graph(
        &graph,
        &topology,
        &pubsub_config,
        MESH_EVENT_POLICY.max_htl,
        HtlBaselineMode::InvWant,
    );
    let flood = run_mesh_pubsub_htl_baseline_on_graph(
        &graph,
        &topology,
        &pubsub_config,
        MESH_EVENT_POLICY.max_htl,
        HtlBaselineMode::FloodPayload,
    );

    report.rating_pubsub_delivery_opportunities = inv_want.delivery_opportunities;
    report.rating_pubsub_delivered_events = inv_want.delivered_events;
    report.rating_pubsub_forwarded_bytes_sent = inv_want.forwarded_bytes_sent;
    report.rating_pubsub_flood_forwarded_bytes_sent = flood.forwarded_bytes_sent;
}

async fn ingest_rating_candidates(
    history: &mut WotRatingHistory,
    scores: &mut HashMap<String, i64>,
    report: &mut WotPubsubSimReport,
    candidates: Vec<RatingCandidate>,
    capacity: usize,
) -> Result<(), NostrEventStoreError> {
    let mut trusted = Vec::new();
    let mut untrusted = Vec::new();
    for candidate in candidates {
        match candidate.trust {
            RatingCandidateTrust::Trusted => trusted.push(candidate),
            RatingCandidateTrust::Untrusted => untrusted.push(candidate),
        }
    }

    report.spam_rating_events_seen = report
        .spam_rating_events_seen
        .saturating_add(untrusted.len() as u64);

    for (index, candidate) in trusted.into_iter().chain(untrusted).enumerate() {
        if index >= capacity {
            match candidate.trust {
                RatingCandidateTrust::Trusted => {
                    report.trusted_rating_events_deferred =
                        report.trusted_rating_events_deferred.saturating_add(1);
                }
                RatingCandidateTrust::Untrusted => {
                    report.spam_rating_events_dropped =
                        report.spam_rating_events_dropped.saturating_add(1);
                }
            }
            continue;
        }

        match candidate.trust {
            RatingCandidateTrust::Trusted => {
                publish_rating(history, scores, report, candidate.rating).await?;
                report.trusted_rating_events_accepted =
                    report.trusted_rating_events_accepted.saturating_add(1);
            }
            RatingCandidateTrust::Untrusted => {
                report.spam_rating_events_dropped =
                    report.spam_rating_events_dropped.saturating_add(1);
            }
        }
    }

    Ok(())
}

fn append_spam_rating_candidates(
    candidates: &mut Vec<RatingCandidate>,
    scope: &str,
    phase: &str,
    count: usize,
    rating_time: &mut u64,
) {
    for index in 0..count {
        let subject = format!("peer:spam-subject:{phase}:{index:04}");
        let mut rating = WotRatingRecord::new(
            format!("peer:spam-rater:{phase}:{index:04}"),
            subject,
            scope,
            100,
            take_rating_time(rating_time),
            "untrusted rating spam",
        );
        rating.tags.push("spam".to_string());
        candidates.push(RatingCandidate::untrusted(rating));
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
    scores.insert(rating.subject.clone(), score);
    history.publish_rating(&rating).await?;
    report.rating_events_published = report.rating_events_published.saturating_add(1);
    Ok(())
}

fn build_peers(config: &WotPubsubSimConfig) -> Vec<SimPeer> {
    let mut peers = Vec::new();
    for index in 0..config.good_peer_count {
        peers.push(SimPeer {
            id: format!("peer:good:{index:04}"),
            role: PeerRole::Good,
        });
    }
    for index in 0..config.bad_peer_count {
        peers.push(SimPeer {
            id: format!("peer:bad:{index:04}"),
            role: PeerRole::Bad,
        });
    }
    for index in 0..config.newcomer_count {
        peers.push(SimPeer {
            id: format!("peer:new:{index:04}"),
            role: PeerRole::Newcomer,
        });
    }
    peers.push(SimPeer {
        id: "peer:degrading:0000".to_string(),
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
    stored_event(
        pubkey_for_node(LOCAL_RATER),
        created_at.max(1),
        HASHTREE_ROOT_KIND,
        tags,
        "",
    )
}

fn scope_filter(kind: u32, scope: &str) -> Filter {
    Filter::new().kind(Kind::from(kind as u16)).custom_tag(
        SingleLetterTag::lowercase(Alphabet::I),
        scope.to_lowercase(),
    )
}

fn stored_event(
    pubkey: String,
    created_at: u64,
    kind: u32,
    tags: Vec<Vec<String>>,
    content: impl Into<String>,
) -> Result<StoredNostrEvent, NostrEventStoreError> {
    let content = content.into();
    let id = nostr_event_id(&pubkey, created_at, kind, &tags, &content)?;
    Ok(StoredNostrEvent {
        id,
        pubkey,
        created_at,
        kind,
        tags,
        content,
        sig: "0".repeat(128),
    })
}

fn nostr_event_id(
    pubkey: &str,
    created_at: u64,
    kind: u32,
    tags: &[Vec<String>],
    content: &str,
) -> Result<String, NostrEventStoreError> {
    let payload = serde_json::to_string(&(0_u8, pubkey, created_at, kind, tags, content))?;
    Ok(to_hex(&sha256(payload.as_bytes())))
}

fn pubkey_for_node(id: &str) -> String {
    to_hex(&sha256(id.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wot_prioritizes_good_limits_bad_and_recovers_newcomers() {
        let report = run_wot_pubsub_simulation(WotPubsubSimConfig::default())
            .await
            .unwrap();

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
    async fn wot_history_lookup_uses_scope_i_tag_filters() {
        let mut history = WotRatingHistory::new();
        let rating = WotRatingRecord::new(
            LOCAL_RATER,
            "peer:subject",
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
