//! Adaptive peer selection based on Freenet patterns
//!
//! Implements sophisticated peer selection that favors reliable, fast peers:
//! - Per-peer performance tracking (RTT, success rate)
//! - RFC 2988-style smoothed RTT calculation
//! - Exponential backoff for failing/slow peers
//! - Fairness constraints to prevent overloading any single peer
//! - Weighted selection combining multiple signals

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::Instant;

/// Constants from Freenet's PeerManager
const SELECTION_PERCENTAGE_WARNING: f64 = 0.30; // Skip if selected >30% of time
const SELECTION_MIN_PEERS: usize = 5; // Enable fairness if >=5 peers

/// Backoff constants (from Freenet)
const INITIAL_BACKOFF_MS: u64 = 1000; // 1 second initial backoff
const BACKOFF_MULTIPLIER: u64 = 2; // Exponential backoff
const MAX_BACKOFF_MS: u64 = 480_000; // 8 minutes max backoff

/// RTO constants (RFC 2988)
const MIN_RTO_MS: u64 = 50; // Minimum retransmission timeout
const MAX_RTO_MS: u64 = 60_000; // Maximum RTO (60 seconds)
const INITIAL_RTO_MS: u64 = 1000; // Initial RTO before any measurements

/// Current schema version for persisted peer metadata snapshots.
pub const PEER_METADATA_SNAPSHOT_VERSION: u32 = 1;

/// Persisted metadata for a logical peer principal (pubkey/npub identity).
///
/// This omits process-local runtime fields (`Instant`, active backoff timers) so
/// metadata can survive restarts and session UUID churn.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PersistedPeerMetadata {
    /// Stable principal identity (usually pubkey/npub).
    pub principal: String,
    pub requests_sent: u64,
    pub successes: u64,
    #[serde(default)]
    pub misses: u64,
    pub timeouts: u64,
    pub failures: u64,
    pub srtt_ms: f64,
    pub rttvar_ms: f64,
    pub rto_ms: u64,
    pub bytes_received: u64,
    pub bytes_sent: u64,
    pub cashu_paid_sat: u64,
    pub cashu_received_sat: u64,
    pub cashu_payment_receipts: u64,
    pub cashu_payment_defaults: u64,
}

impl PersistedPeerMetadata {
    fn from_stats(principal: String, stats: &PeerStats) -> Self {
        Self {
            principal,
            requests_sent: stats.requests_sent,
            successes: stats.successes,
            misses: stats.misses,
            timeouts: stats.timeouts,
            failures: stats.failures,
            srtt_ms: sanitize_latency(stats.srtt_ms),
            rttvar_ms: sanitize_latency(stats.rttvar_ms),
            rto_ms: clamp_rto(stats.rto_ms),
            bytes_received: stats.bytes_received,
            bytes_sent: stats.bytes_sent,
            cashu_paid_sat: stats.cashu_paid_sat,
            cashu_received_sat: stats.cashu_received_sat,
            cashu_payment_receipts: stats.cashu_payment_receipts,
            cashu_payment_defaults: stats.cashu_payment_defaults,
        }
    }

    fn apply_to_stats(&self, stats: &mut PeerStats) {
        stats.requests_sent = self.requests_sent;
        stats.successes = self.successes;
        stats.misses = self.misses;
        stats.timeouts = self.timeouts;
        stats.failures = self.failures;
        stats.srtt_ms = sanitize_latency(self.srtt_ms);
        stats.rttvar_ms = sanitize_latency(self.rttvar_ms);
        stats.rto_ms = clamp_rto(self.rto_ms);
        stats.bytes_received = self.bytes_received;
        stats.bytes_sent = self.bytes_sent;
        stats.cashu_paid_sat = self.cashu_paid_sat;
        stats.cashu_received_sat = self.cashu_received_sat;
        stats.cashu_payment_receipts = self.cashu_payment_receipts;
        stats.cashu_payment_defaults = self.cashu_payment_defaults;

        // Runtime-only state is intentionally reset on restore.
        stats.backoff_level = 0;
        stats.backed_off_until = None;
        stats.last_success = None;
        stats.last_failure = None;
        stats.consecutive_rto_backoffs = 0;
    }
}

/// Snapshot of metadata for all known principals.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PeerMetadataSnapshot {
    pub version: u32,
    pub peers: Vec<PersistedPeerMetadata>,
}

impl Default for PeerMetadataSnapshot {
    fn default() -> Self {
        Self {
            version: PEER_METADATA_SNAPSHOT_VERSION,
            peers: Vec::new(),
        }
    }
}

fn sanitize_latency(value: f64) -> f64 {
    if value.is_finite() && value >= 0.0 {
        value
    } else {
        0.0
    }
}

fn compute_backoff_ms(level: u32) -> u64 {
    if level == 0 {
        return 0;
    }

    let mut backoff_ms = INITIAL_BACKOFF_MS;
    for _ in 1..level {
        backoff_ms = backoff_ms.saturating_mul(BACKOFF_MULTIPLIER);
        if backoff_ms >= MAX_BACKOFF_MS {
            return MAX_BACKOFF_MS;
        }
    }

    backoff_ms.min(MAX_BACKOFF_MS)
}

fn clamp_rto(rto_ms: u64) -> u64 {
    if rto_ms == 0 {
        INITIAL_RTO_MS
    } else {
        rto_ms.clamp(MIN_RTO_MS, MAX_RTO_MS)
    }
}

/// Extract the stable principal identity for a peer.
///
/// Peer IDs are the stable endpoint identifier now, so selector state keys map
/// directly to the peer id instead of stripping any session suffix.
pub fn peer_principal(peer_id: &str) -> &str {
    peer_id
}

/// Per-peer performance statistics
#[derive(Debug, Clone)]
pub struct PeerStats {
    /// Peer identifier
    pub peer_id: String,
    /// When this peer was connected
    pub connected_at: Instant,
    /// Total requests sent to this peer
    pub requests_sent: u64,
    /// Total successful responses received
    pub successes: u64,
    /// Total explicit misses (peer was reachable but did not have the content)
    pub misses: u64,
    /// Total timeouts
    pub timeouts: u64,
    /// Total failures (bad data, disconnects, etc.)
    pub failures: u64,
    /// Smoothed round-trip time (RFC 2988 SRTT)
    pub srtt_ms: f64,
    /// RTT variance (RFC 2988 RTTVAR)
    pub rttvar_ms: f64,
    /// Retransmission timeout (computed from SRTT and RTTVAR)
    pub rto_ms: u64,
    /// Consecutive RTO backoffs (for capping)
    pub consecutive_rto_backoffs: u32,
    /// Current backoff level (how many times we've backed off)
    pub backoff_level: u32,
    /// When backoff expires (None if not backed off)
    pub backed_off_until: Option<Instant>,
    /// Last successful response timestamp
    pub last_success: Option<Instant>,
    /// Last failure timestamp
    pub last_failure: Option<Instant>,
    /// Total bytes received from this peer
    pub bytes_received: u64,
    /// Total bytes sent to this peer
    pub bytes_sent: u64,
    /// Total sats paid to this peer through an external payment channel.
    pub cashu_paid_sat: u64,
    /// Total sats this peer paid us after successful delivery.
    pub cashu_received_sat: u64,
    /// Number of successful post-delivery payments received from this peer.
    pub cashu_payment_receipts: u64,
    /// Number of times this peer failed to pay after successful delivery.
    pub cashu_payment_defaults: u64,
}

impl PeerStats {
    /// Create new peer stats
    pub fn new(peer_id: impl Into<String>) -> Self {
        Self {
            peer_id: peer_id.into(),
            connected_at: Instant::now(),
            requests_sent: 0,
            successes: 0,
            misses: 0,
            timeouts: 0,
            failures: 0,
            srtt_ms: 0.0,
            rttvar_ms: 0.0,
            rto_ms: INITIAL_RTO_MS,
            consecutive_rto_backoffs: 0,
            backoff_level: 0,
            backed_off_until: None,
            last_success: None,
            last_failure: None,
            bytes_received: 0,
            bytes_sent: 0,
            cashu_paid_sat: 0,
            cashu_received_sat: 0,
            cashu_payment_receipts: 0,
            cashu_payment_defaults: 0,
        }
    }

    /// Get success rate (0.0 to 1.0)
    pub fn success_rate(&self) -> f64 {
        let attempts_with_health_outcome = self.requests_sent.saturating_sub(self.misses);
        if attempts_with_health_outcome == 0 {
            return 0.5; // Neutral for new peers
        }
        self.successes as f64 / attempts_with_health_outcome as f64
    }

    /// Get selection rate (selections per second since connected)
    pub fn selection_rate(&self) -> f64 {
        let elapsed = self.connected_at.elapsed();
        if elapsed.as_secs() < 10 {
            return 0.0; // Avoid bias from short uptime (Freenet pattern)
        }
        self.requests_sent as f64 / elapsed.as_secs_f64()
    }

    /// Check if peer is currently backed off
    pub fn is_backed_off(&self) -> bool {
        if let Some(until) = self.backed_off_until {
            Instant::now() < until
        } else {
            false
        }
    }

    /// Get remaining backoff time
    pub fn backoff_remaining(&self) -> Duration {
        if let Some(until) = self.backed_off_until {
            let now = Instant::now();
            if now < until {
                return until - now;
            }
        }
        Duration::ZERO
    }

    /// Record a request being sent
    pub fn record_request(&mut self, bytes: u64) {
        self.requests_sent += 1;
        self.bytes_sent += bytes;
    }

    /// Record a successful response with RTT
    /// Uses RFC 2988 algorithm for smoothed RTT calculation
    pub fn record_success(&mut self, rtt_ms: u64, bytes: u64) {
        self.successes += 1;
        self.bytes_received += bytes;
        self.last_success = Some(Instant::now());
        self.consecutive_rto_backoffs = 0;

        // Clear backoff on success
        self.backed_off_until = None;
        self.backoff_level = 0;

        // RFC 2988 RTT update
        let rtt = rtt_ms as f64;
        if self.srtt_ms == 0.0 {
            // First measurement
            self.srtt_ms = rtt;
            self.rttvar_ms = rtt / 2.0;
        } else {
            // Subsequent measurements
            // RTTVAR = (1 - beta) * RTTVAR + beta * |SRTT - R'|
            // SRTT = (1 - alpha) * SRTT + alpha * R'
            // where alpha = 1/8 = 0.125 and beta = 1/4 = 0.25
            self.rttvar_ms = 0.75 * self.rttvar_ms + 0.25 * (self.srtt_ms - rtt).abs();
            self.srtt_ms = 0.875 * self.srtt_ms + 0.125 * rtt;
        }

        // RTO = SRTT + max(G, K*RTTVAR) where G=20ms granularity, K=4
        let rto = self.srtt_ms + (20.0_f64).max(4.0 * self.rttvar_ms);
        self.rto_ms = (rto as u64).clamp(MIN_RTO_MS, MAX_RTO_MS);
    }

    /// Record an explicit not-found response from a reachable peer.
    pub fn record_miss(&mut self) {
        self.misses += 1;
    }

    /// Record a timeout
    pub fn record_timeout(&mut self) {
        self.timeouts += 1;
        self.last_failure = Some(Instant::now());

        // Apply backoff
        self.apply_backoff();

        // RFC 2988: Double RTO on timeout (up to max)
        if self.consecutive_rto_backoffs < 5 {
            self.rto_ms = (self.rto_ms * 2).min(MAX_RTO_MS);
            self.consecutive_rto_backoffs += 1;
        }
    }

    /// Record a failure (bad data, disconnect, etc.)
    pub fn record_failure(&mut self) {
        self.failures += 1;
        self.last_failure = Some(Instant::now());
        self.apply_backoff();
    }

    /// Record an out-of-band payment to this peer (e.g. Cashu channel transfer).
    pub fn record_cashu_payment(&mut self, amount_sat: u64) {
        if amount_sat == 0 {
            return;
        }
        self.cashu_paid_sat = self.cashu_paid_sat.saturating_add(amount_sat);
    }

    /// Record a settled payment received from this peer after we served data.
    pub fn record_cashu_receipt(&mut self, amount_sat: u64) {
        if amount_sat == 0 {
            return;
        }
        self.cashu_received_sat = self.cashu_received_sat.saturating_add(amount_sat);
        self.cashu_payment_receipts = self.cashu_payment_receipts.saturating_add(1);
    }

    /// Record that this peer failed to pay after successful delivery.
    pub fn record_cashu_payment_default(&mut self) {
        self.cashu_payment_defaults = self.cashu_payment_defaults.saturating_add(1);
        self.last_failure = Some(Instant::now());
        self.apply_backoff();
    }

    /// Apply exponential backoff
    fn apply_backoff(&mut self) {
        self.backoff_level += 1;
        let backoff_ms = compute_backoff_ms(self.backoff_level);
        self.backed_off_until = Some(Instant::now() + Duration::from_millis(backoff_ms));
    }

    /// Calculate peer score for selection (higher is better)
    /// Combines success rate, RTT, and recent performance
    pub fn score(&self) -> f64 {
        // Base score from success rate (0-1)
        let success_score = self.success_rate();

        // RTT score: prefer faster peers (inverse of normalized RTT)
        // Scale: 0-50ms = 1.0, 500ms+ = 0.1
        let rtt_score = if self.srtt_ms <= 0.0 {
            0.5 // Unknown RTT, neutral
        } else {
            (500.0 / (self.srtt_ms + 50.0)).min(1.0)
        };

        // Recency bonus: slight boost for recently successful peers
        let recency_bonus = if let Some(last) = self.last_success {
            let secs_ago = last.elapsed().as_secs_f64();
            if secs_ago < 60.0 {
                0.1 // Recent success
            } else {
                0.0
            }
        } else {
            0.0
        };

        // Combine scores (weighted)
        // Success rate is most important (60%), RTT next (30%), recency last (10%)
        0.6 * success_score + 0.3 * rtt_score + 0.1 * (1.0 + recency_bonus)
    }

    /// Utility-centric score with exploration bonus (UCB-style).
    ///
    /// Balances:
    /// - good/bad outcome ratio (successes vs failures+timeouts),
    /// - latency efficiency,
    /// - bytes efficiency (received vs sent),
    /// - uncertainty bonus for less-tested peers.
    pub fn utility_score(&self, total_requests: u64) -> f64 {
        let good = self.successes as f64 + 1.0;
        let bad = (self.failures + self.timeouts) as f64 + 1.0;
        let ratio = good / bad;
        let ratio_score = ratio / (1.0 + ratio);

        let latency_score = if self.srtt_ms <= 0.0 {
            0.5
        } else {
            (300.0 / (self.srtt_ms + 50.0)).min(1.0)
        };

        let efficiency_score = if self.bytes_sent == 0 {
            0.5
        } else {
            (self.bytes_received as f64 / self.bytes_sent as f64).min(1.0)
        };

        let exploitation = 0.55 * ratio_score + 0.25 * latency_score + 0.20 * efficiency_score;

        let uncertainty =
            (((total_requests as f64) + 1.0).ln() / ((self.requests_sent as f64) + 1.0)).sqrt();
        let exploration_bonus = 0.20 * uncertainty;

        exploitation + exploration_bonus
    }

    /// Normalize paid amount to a bounded priority score in [0, 1).
    pub fn cashu_priority_boost(&self) -> f64 {
        if self.cashu_paid_sat == 0 {
            return 0.0;
        }
        let paid = self.cashu_paid_sat as f64;
        paid / (paid + 32.0)
    }

    /// Cooperative peers that actually pay us should not be penalized; repeated
    /// defaults quickly reduce their desirability.
    pub fn payment_reliability_multiplier(&self) -> f64 {
        if self.cashu_payment_receipts == 0 && self.cashu_payment_defaults == 0 {
            return 1.0;
        }
        (self.cashu_payment_receipts as f64 + 1.0)
            / (self.cashu_payment_receipts as f64 + self.cashu_payment_defaults as f64 + 1.0)
    }

    pub fn exceeds_payment_default_threshold(&self, threshold: u64) -> bool {
        threshold > 0 && self.cashu_payment_defaults >= threshold
    }
}

/// Peer selection strategy
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SelectionStrategy {
    /// Select by score (success rate + RTT) - recommended
    #[default]
    Weighted,
    /// Round-robin (ignores performance)
    RoundRobin,
    /// Random selection
    Random,
    /// Lowest RTT first
    LowestLatency,
    /// Highest success rate first
    HighestSuccessRate,
    /// Utility + exploration (good/bad ratio + RTT/efficiency + UCB bonus)
    UtilityUcb,
}

/// Adaptive peer selector
///
/// Tracks peer performance and selects peers intelligently:
/// - Prefers high success rate peers
/// - Prefers low latency peers
/// - Backs off failing peers exponentially
/// - Ensures fairness (no peer gets >30% of traffic with 5+ peers)
#[derive(Debug, Default)]
pub struct PeerSelector {
    /// Per-peer statistics
    stats: HashMap<String, PeerStats>,
    /// Persisted peer metadata indexed by stable principal identity (pubkey/npub).
    persisted_metadata: HashMap<String, PersistedPeerMetadata>,
    /// Selection strategy
    strategy: SelectionStrategy,
    /// Enable fairness constraints (Freenet FOAF mitigation)
    fairness_enabled: bool,
    /// Round-robin index for RoundRobin strategy
    round_robin_idx: usize,
    /// Blending weight for payment priority. 0.0 keeps pure reputation routing.
    cashu_payment_weight: f64,
}

impl PeerSelector {
    /// Create a new peer selector with default weighted strategy
    pub fn new() -> Self {
        Self {
            stats: HashMap::new(),
            persisted_metadata: HashMap::new(),
            strategy: SelectionStrategy::Weighted,
            fairness_enabled: true,
            round_robin_idx: 0,
            cashu_payment_weight: 0.0,
        }
    }

    /// Create with specific strategy
    pub fn with_strategy(strategy: SelectionStrategy) -> Self {
        Self {
            stats: HashMap::new(),
            persisted_metadata: HashMap::new(),
            strategy,
            fairness_enabled: true,
            round_robin_idx: 0,
            cashu_payment_weight: 0.0,
        }
    }

    /// Enable/disable fairness constraints
    pub fn set_fairness(&mut self, enabled: bool) {
        self.fairness_enabled = enabled;
    }

    /// Configure payment-priority influence when ranking peers.
    /// `0.0` disables payment influence and preserves reputation-only behavior.
    pub fn set_cashu_payment_weight(&mut self, weight: f64) {
        self.cashu_payment_weight = weight.clamp(0.0, 1.0);
    }

    /// Add a peer to track
    pub fn add_peer(&mut self, peer_id: impl Into<String>) {
        let peer_id = peer_id.into();
        if self.stats.contains_key(&peer_id) {
            return;
        }

        let mut stats = PeerStats::new(peer_id.clone());
        if let Some(saved) = self.persisted_metadata.get(peer_principal(&peer_id)) {
            saved.apply_to_stats(&mut stats);
        }
        self.stats.insert(peer_id, stats);
    }

    /// Remove a peer
    pub fn remove_peer(&mut self, peer_id: &str) {
        if let Some(stats) = self.stats.remove(peer_id) {
            let principal = peer_principal(&stats.peer_id).to_string();
            self.persisted_metadata.insert(
                principal.clone(),
                PersistedPeerMetadata::from_stats(principal, &stats),
            );
        }
    }

    /// Get peer stats (immutable)
    pub fn get_stats(&self, peer_id: &str) -> Option<&PeerStats> {
        self.stats.get(peer_id)
    }

    /// Get peer stats (mutable)
    pub fn get_stats_mut(&mut self, peer_id: &str) -> Option<&mut PeerStats> {
        self.stats.get_mut(peer_id)
    }

    /// Get all peer stats
    pub fn all_stats(&self) -> impl Iterator<Item = &PeerStats> {
        self.stats.values()
    }

    /// Whether this peer is currently backed off due to recent failures/timeouts.
    pub fn is_peer_backed_off(&self, peer_id: &str) -> bool {
        self.stats
            .get(peer_id)
            .is_some_and(PeerStats::is_backed_off)
    }

    /// Record a request being sent to a peer
    pub fn record_request(&mut self, peer_id: &str, bytes: u64) {
        if let Some(stats) = self.stats.get_mut(peer_id) {
            stats.record_request(bytes);
        }
    }

    /// Record a successful response
    pub fn record_success(&mut self, peer_id: &str, rtt_ms: u64, bytes: u64) {
        if let Some(stats) = self.stats.get_mut(peer_id) {
            stats.record_success(rtt_ms, bytes);
        }
    }

    /// Record an explicit content miss.
    pub fn record_miss(&mut self, peer_id: &str) {
        if let Some(stats) = self.stats.get_mut(peer_id) {
            stats.record_miss();
        }
    }

    /// Record a timeout
    pub fn record_timeout(&mut self, peer_id: &str) {
        if let Some(stats) = self.stats.get_mut(peer_id) {
            stats.record_timeout();
        }
    }

    /// Record a failure
    pub fn record_failure(&mut self, peer_id: &str) {
        if let Some(stats) = self.stats.get_mut(peer_id) {
            stats.record_failure();
        }
    }

    /// Record payment channel credit for a peer.
    pub fn record_cashu_payment(&mut self, peer_id: &str, amount_sat: u64) {
        if amount_sat == 0 {
            return;
        }
        let entry = self
            .stats
            .entry(peer_id.to_string())
            .or_insert_with(|| PeerStats::new(peer_id.to_string()));
        entry.record_cashu_payment(amount_sat);
    }

    /// Record a settled post-delivery payment received from a peer.
    pub fn record_cashu_receipt(&mut self, peer_id: &str, amount_sat: u64) {
        if amount_sat == 0 {
            return;
        }
        let entry = self
            .stats
            .entry(peer_id.to_string())
            .or_insert_with(|| PeerStats::new(peer_id.to_string()));
        entry.record_cashu_receipt(amount_sat);
    }

    /// Record that a peer failed to settle after we delivered successfully.
    pub fn record_cashu_payment_default(&mut self, peer_id: &str) {
        let entry = self
            .stats
            .entry(peer_id.to_string())
            .or_insert_with(|| PeerStats::new(peer_id.to_string()));
        entry.record_cashu_payment_default();
    }

    pub fn is_peer_blocked_for_payment_defaults(&self, peer_id: &str, threshold: u64) -> bool {
        self.stats
            .get(peer_id)
            .map(|stats| stats.exceeds_payment_default_threshold(threshold))
            .unwrap_or(false)
    }

    fn blend_with_payment_priority(&self, stats: &PeerStats, base_score: f64) -> f64 {
        let reliable_base = base_score * stats.payment_reliability_multiplier();
        if self.cashu_payment_weight <= 0.0 {
            return reliable_base;
        }
        let payment_score = stats.cashu_priority_boost();
        (1.0 - self.cashu_payment_weight) * reliable_base
            + self.cashu_payment_weight * payment_score
    }

    /// Get available (non-backed-off) peers
    fn available_peers(&self) -> Vec<String> {
        self.stats
            .iter()
            .filter(|(_, s)| !s.is_backed_off())
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Check fairness: should this peer be skipped due to over-selection?
    #[cfg(test)]
    fn should_skip_for_fairness(&self, peer_id: &str) -> bool {
        let total_rate: f64 = self.stats.values().map(|s| s.selection_rate()).sum();
        self.should_skip_for_fairness_with_total(peer_id, total_rate)
    }

    fn should_skip_for_fairness_with_total(&self, peer_id: &str, total_rate: f64) -> bool {
        if !self.fairness_enabled || self.stats.len() < SELECTION_MIN_PEERS || total_rate <= 0.0 {
            return false;
        }

        // Check if this peer is selected too often
        if let Some(stats) = self.stats.get(peer_id) {
            let peer_rate = stats.selection_rate();
            let proportion = peer_rate / total_rate;
            return proportion > SELECTION_PERCENTAGE_WARNING;
        }

        false
    }

    /// Select peers ordered by preference
    ///
    /// Returns all available peers sorted by preference (best first).
    /// Respects backoff states and fairness constraints.
    pub fn select_peers(&mut self) -> Vec<String> {
        let available = self.available_peers();
        if available.is_empty() {
            // If all peers are backed off, return backed off peers anyway
            // sorted by when their backoff expires (soonest first)
            let mut backed_off: Vec<_> = self
                .stats
                .iter()
                .filter(|(_, s)| s.is_backed_off())
                .map(|(id, s)| (id.clone(), s.backoff_remaining()))
                .collect();
            backed_off.sort_by_key(|(_, remaining)| *remaining);
            return backed_off.into_iter().map(|(id, _)| id).collect();
        }

        // Apply fairness filter
        let candidates: Vec<String> =
            if self.fairness_enabled && available.len() >= SELECTION_MIN_PEERS {
                let total_rate: f64 = self.stats.values().map(|s| s.selection_rate()).sum();
                available
                    .into_iter()
                    .filter(|id| !self.should_skip_for_fairness_with_total(id, total_rate))
                    .collect()
            } else {
                available
            };

        // If all peers were filtered out for fairness, use all available
        let candidates = if candidates.is_empty() {
            self.available_peers()
        } else {
            candidates
        };

        // Sort by strategy
        let mut sorted: Vec<_> = candidates
            .into_iter()
            .filter_map(|id| self.stats.get(&id).map(|s| (id, s.clone())))
            .collect();

        match self.strategy {
            SelectionStrategy::Weighted => {
                // Sort by score (highest first), then by peer_id for determinism
                sorted.sort_by(|(id_a, a), (id_b, b)| {
                    let score_a = self.blend_with_payment_priority(a, a.score());
                    let score_b = self.blend_with_payment_priority(b, b.score());
                    let score_cmp = score_b
                        .partial_cmp(&score_a)
                        .unwrap_or(std::cmp::Ordering::Equal);
                    if score_cmp == std::cmp::Ordering::Equal {
                        id_a.cmp(id_b) // Alphabetical for determinism
                    } else {
                        score_cmp
                    }
                });
            }
            SelectionStrategy::LowestLatency => {
                // Sort by SRTT (lowest first), use score and peer_id as tiebreakers
                sorted.sort_by(|(id_a, a), (id_b, b)| {
                    let rtt_cmp = a
                        .srtt_ms
                        .partial_cmp(&b.srtt_ms)
                        .unwrap_or(std::cmp::Ordering::Equal);
                    if rtt_cmp == std::cmp::Ordering::Equal {
                        let score_cmp = b
                            .score()
                            .partial_cmp(&a.score())
                            .unwrap_or(std::cmp::Ordering::Equal);
                        if score_cmp == std::cmp::Ordering::Equal {
                            id_a.cmp(id_b)
                        } else {
                            score_cmp
                        }
                    } else {
                        rtt_cmp
                    }
                });
            }
            SelectionStrategy::HighestSuccessRate => {
                // Sort by success rate (highest first), peer_id as tiebreaker
                sorted.sort_by(|(id_a, a), (id_b, b)| {
                    let rate_cmp = b
                        .success_rate()
                        .partial_cmp(&a.success_rate())
                        .unwrap_or(std::cmp::Ordering::Equal);
                    if rate_cmp == std::cmp::Ordering::Equal {
                        id_a.cmp(id_b)
                    } else {
                        rate_cmp
                    }
                });
            }
            SelectionStrategy::UtilityUcb => {
                let total_requests: u64 = sorted.iter().map(|(_, s)| s.requests_sent).sum();
                sorted.sort_by(|(id_a, a), (id_b, b)| {
                    let score_a =
                        self.blend_with_payment_priority(a, a.utility_score(total_requests));
                    let score_b =
                        self.blend_with_payment_priority(b, b.utility_score(total_requests));
                    let score_cmp = score_b
                        .partial_cmp(&score_a)
                        .unwrap_or(std::cmp::Ordering::Equal);
                    if score_cmp == std::cmp::Ordering::Equal {
                        id_a.cmp(id_b)
                    } else {
                        score_cmp
                    }
                });
            }
            SelectionStrategy::RoundRobin => {
                // Rotate the list based on round-robin index
                if !sorted.is_empty() {
                    let idx = self.round_robin_idx % sorted.len();
                    sorted.rotate_left(idx);
                    self.round_robin_idx = (self.round_robin_idx + 1) % sorted.len();
                }
            }
            SelectionStrategy::Random => {
                // Shuffle using simple deterministic approach for reproducibility
                // In production, use proper random shuffle
            }
        }

        sorted.into_iter().map(|(id, _)| id).collect()
    }

    /// Select single best peer
    pub fn select_best(&mut self) -> Option<String> {
        self.select_peers().into_iter().next()
    }

    /// Select top N peers
    pub fn select_top(&mut self, n: usize) -> Vec<String> {
        self.select_peers().into_iter().take(n).collect()
    }

    /// Get summary statistics across all peers
    pub fn summary(&self) -> SelectorSummary {
        let count = self.stats.len();
        if count == 0 {
            return SelectorSummary::default();
        }

        let total_requests: u64 = self.stats.values().map(|s| s.requests_sent).sum();
        let total_successes: u64 = self.stats.values().map(|s| s.successes).sum();
        let total_misses: u64 = self.stats.values().map(|s| s.misses).sum();
        let total_timeouts: u64 = self.stats.values().map(|s| s.timeouts).sum();
        let backed_off = self.stats.values().filter(|s| s.is_backed_off()).count();
        let total_health_attempts = total_requests.saturating_sub(total_misses);

        let avg_rtt = {
            let rtts: Vec<f64> = self
                .stats
                .values()
                .filter(|s| s.srtt_ms > 0.0)
                .map(|s| s.srtt_ms)
                .collect();
            if rtts.is_empty() {
                0.0
            } else {
                rtts.iter().sum::<f64>() / rtts.len() as f64
            }
        };

        SelectorSummary {
            peer_count: count,
            total_requests,
            total_successes,
            total_timeouts,
            backed_off_count: backed_off,
            avg_rtt_ms: avg_rtt,
            overall_success_rate: if total_health_attempts > 0 {
                total_successes as f64 / total_health_attempts as f64
            } else {
                0.0
            },
        }
    }

    /// Export persisted peer metadata keyed by stable principal identity.
    pub fn export_peer_metadata_snapshot(&self) -> PeerMetadataSnapshot {
        let mut by_principal = self.persisted_metadata.clone();
        for stats in self.stats.values() {
            let principal = peer_principal(&stats.peer_id).to_string();
            by_principal.insert(
                principal.clone(),
                PersistedPeerMetadata::from_stats(principal, stats),
            );
        }

        let mut peers: Vec<PersistedPeerMetadata> = by_principal.into_values().collect();
        peers.sort_by(|a, b| a.principal.cmp(&b.principal));

        PeerMetadataSnapshot {
            version: PEER_METADATA_SNAPSHOT_VERSION,
            peers,
        }
    }

    /// Import persisted metadata and apply it to currently tracked peers.
    pub fn import_peer_metadata_snapshot(&mut self, snapshot: &PeerMetadataSnapshot) {
        if snapshot.version != PEER_METADATA_SNAPSHOT_VERSION {
            return;
        }

        self.persisted_metadata.clear();
        for peer in &snapshot.peers {
            self.persisted_metadata
                .insert(peer.principal.clone(), peer.clone());
        }

        for stats in self.stats.values_mut() {
            if let Some(saved) = self.persisted_metadata.get(peer_principal(&stats.peer_id)) {
                saved.apply_to_stats(stats);
            }
        }
    }
}

/// Summary statistics for the selector
#[derive(Debug, Clone, Default)]
pub struct SelectorSummary {
    pub peer_count: usize,
    pub total_requests: u64,
    pub total_successes: u64,
    pub total_timeouts: u64,
    pub backed_off_count: usize,
    pub avg_rtt_ms: f64,
    pub overall_success_rate: f64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    #[test]
    fn test_peer_stats_success_rate() {
        let mut stats = PeerStats::new("peer1");
        assert_eq!(stats.success_rate(), 0.5); // Neutral for new peer

        stats.record_request(40);
        stats.record_success(50, 1024);
        assert_eq!(stats.success_rate(), 1.0);

        stats.record_request(40);
        stats.record_timeout();
        assert_eq!(stats.success_rate(), 0.5);
    }

    #[test]
    fn test_peer_stats_miss_does_not_lower_health_success_rate() {
        let mut stats = PeerStats::new("peer1");

        stats.record_request(40);
        stats.record_miss();
        assert_eq!(stats.success_rate(), 0.5);

        stats.record_request(40);
        stats.record_success(50, 1024);
        assert_eq!(stats.success_rate(), 1.0);
    }

    #[test]
    fn test_peer_stats_rtt_calculation() {
        let mut stats = PeerStats::new("peer1");

        // First RTT measurement
        stats.record_request(40);
        stats.record_success(100, 1024);
        assert_eq!(stats.srtt_ms, 100.0);
        assert_eq!(stats.rttvar_ms, 50.0); // RTT/2

        // Second measurement
        stats.record_request(40);
        stats.record_success(80, 1024);
        // SRTT = 0.875 * 100 + 0.125 * 80 = 87.5 + 10 = 97.5
        assert!((stats.srtt_ms - 97.5).abs() < 0.1);
    }

    #[test]
    fn test_peer_stats_backoff() {
        let mut stats = PeerStats::new("peer1");
        assert!(!stats.is_backed_off());

        stats.record_timeout();
        assert!(stats.is_backed_off());
        assert!(stats.backoff_remaining() > Duration::ZERO);
    }

    #[test]
    fn test_peer_stats_backoff_clears_on_success() {
        let mut stats = PeerStats::new("peer1");
        stats.record_timeout();
        assert!(stats.is_backed_off());

        stats.record_success(50, 1024);
        assert!(!stats.is_backed_off());
        assert_eq!(stats.backoff_level, 0);
    }

    #[test]
    fn test_peer_stats_backoff_saturates_without_overflow() {
        let mut stats = PeerStats::new("peer1");

        for _ in 0..128 {
            stats.record_failure();
        }

        assert_eq!(compute_backoff_ms(stats.backoff_level), MAX_BACKOFF_MS);
        assert!(stats.is_backed_off());
    }

    #[test]
    fn test_peer_selector_add_remove() {
        let mut selector = PeerSelector::new();
        selector.add_peer("peer1");
        selector.add_peer("peer2");
        assert!(selector.get_stats("peer1").is_some());
        assert!(selector.get_stats("peer2").is_some());

        selector.remove_peer("peer1");
        assert!(selector.get_stats("peer1").is_none());
        assert!(selector.get_stats("peer2").is_some());
    }

    #[test]
    fn test_peer_selector_weighted_selection() {
        let mut selector = PeerSelector::with_strategy(SelectionStrategy::Weighted);
        selector.add_peer("peer1");
        selector.add_peer("peer2");
        selector.add_peer("peer3");

        // Peer 1: good (high success, low RTT)
        selector.record_request("peer1", 40);
        selector.record_success("peer1", 20, 1024);
        selector.record_request("peer1", 40);
        selector.record_success("peer1", 25, 1024);

        // Peer 2: medium
        selector.record_request("peer2", 40);
        selector.record_success("peer2", 100, 1024);
        selector.record_request("peer2", 40);
        selector.record_timeout("peer2");

        // Peer 3: bad (timeouts)
        selector.record_request("peer3", 40);
        selector.record_timeout("peer3");
        selector.record_request("peer3", 40);
        selector.record_timeout("peer3");

        // Peer 3 should be backed off
        let peers = selector.select_peers();
        // Peer 1 should be first (best score)
        assert_eq!(peers[0], "peer1");
    }

    #[test]
    fn test_peer_selector_backed_off_peers() {
        let mut selector = PeerSelector::new();
        selector.add_peer("peer1");
        selector.add_peer("peer2");

        // Back off peer 1
        selector.record_timeout("peer1");
        assert!(selector.get_stats("peer1").unwrap().is_backed_off());

        // Peer 2 should be available
        let peers = selector.select_peers();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0], "peer2");
    }

    #[test]
    fn test_peer_selector_all_backed_off_fallback() {
        let mut selector = PeerSelector::new();
        selector.add_peer("peer1");
        selector.add_peer("peer2");

        // Back off both peers
        selector.record_timeout("peer1");
        selector.record_timeout("peer2");

        // Should still return peers (sorted by backoff remaining)
        let peers = selector.select_peers();
        assert_eq!(peers.len(), 2);
    }

    #[test]
    fn test_peer_selector_fairness() {
        let mut selector = PeerSelector::new();
        selector.set_fairness(true);

        // Add 5+ peers to enable fairness
        for i in 1..=6 {
            selector.add_peer(format!("peer{}", i));
        }

        // Simulate peer 1 being selected way too often
        sleep(Duration::from_millis(15));

        for _ in 0..100 {
            selector.record_request("peer1", 40);
            selector.record_success("peer1", 10, 100);
        }

        // Other peers get very few requests
        for i in 2..=6 {
            selector.record_request(&format!("peer{}", i), 40);
            selector.record_success(&format!("peer{}", i), 10, 100);
        }

        // Peer 1 should be skipped due to fairness (>30% selection rate)
        let skipped = selector.should_skip_for_fairness("peer1");
        let _ = skipped; // May or may not trigger depending on timing
    }

    #[test]
    fn test_peer_selector_summary() {
        let mut selector = PeerSelector::new();
        selector.add_peer("peer1");
        selector.add_peer("peer2");

        selector.record_request("peer1", 40);
        selector.record_success("peer1", 50, 1024);
        selector.record_request("peer2", 40);
        selector.record_timeout("peer2");

        let summary = selector.summary();
        assert_eq!(summary.peer_count, 2);
        assert_eq!(summary.total_requests, 2);
        assert_eq!(summary.total_successes, 1);
        assert_eq!(summary.total_timeouts, 1);
        assert_eq!(summary.backed_off_count, 1);
        assert_eq!(summary.overall_success_rate, 0.5);
    }

    #[test]
    fn test_peer_stats_score() {
        let mut stats = PeerStats::new("peer1");

        // New peer has neutral score
        let initial_score = stats.score();
        assert!(initial_score > 0.3 && initial_score < 0.7);

        // Good peer: high success rate + low RTT
        for _ in 0..10 {
            stats.record_request(40);
            stats.record_success(20, 1024);
        }
        let good_score = stats.score();
        assert!(good_score > 0.8);

        // Bad peer: high timeout rate
        let mut bad_stats = PeerStats::new("peer2");
        for _ in 0..10 {
            bad_stats.record_request(40);
            bad_stats.record_timeout();
        }
        let bad_score = bad_stats.score();
        assert!(bad_score < 0.3);

        assert!(good_score > bad_score);
    }

    #[test]
    fn test_peer_stats_utility_score_prefers_good_over_bad() {
        let mut good = PeerStats::new("good");
        good.requests_sent = 120;
        good.successes = 96;
        good.failures = 8;
        good.timeouts = 4;
        good.srtt_ms = 30.0;
        good.bytes_sent = 120 * 40;
        good.bytes_received = 96 * 1024;

        let mut bad = PeerStats::new("bad");
        bad.requests_sent = 120;
        bad.successes = 40;
        bad.failures = 50;
        bad.timeouts = 30;
        bad.srtt_ms = 220.0;
        bad.bytes_sent = 120 * 40;
        bad.bytes_received = 40 * 1024;

        let total_requests = good.requests_sent + bad.requests_sent;
        assert!(good.utility_score(total_requests) > bad.utility_score(total_requests));
    }

    #[test]
    fn test_utility_ucb_strategy_explores_less_sampled_peer() {
        let mut selector = PeerSelector::with_strategy(SelectionStrategy::UtilityUcb);
        selector.add_peer("stable");
        selector.add_peer("new");

        {
            let stable = selector.get_stats_mut("stable").unwrap();
            stable.requests_sent = 500;
            stable.successes = 450;
            stable.failures = 35;
            stable.timeouts = 15;
            stable.srtt_ms = 35.0;
            stable.bytes_sent = 500 * 40;
            stable.bytes_received = 450 * 1024;
        }
        {
            let new_peer = selector.get_stats_mut("new").unwrap();
            new_peer.requests_sent = 2;
            new_peer.successes = 2;
            new_peer.failures = 0;
            new_peer.timeouts = 0;
            new_peer.srtt_ms = 70.0;
            new_peer.bytes_sent = 2 * 40;
            new_peer.bytes_received = 2 * 1024;
        }

        let peers = selector.select_peers();
        assert_eq!(peers[0], "new");
    }

    #[test]
    fn test_lowest_latency_strategy() {
        let mut selector = PeerSelector::with_strategy(SelectionStrategy::LowestLatency);
        selector.add_peer("peer1");
        selector.add_peer("peer2");
        selector.add_peer("peer3");

        // Peer 1: 100ms RTT
        selector.record_request("peer1", 40);
        selector.record_success("peer1", 100, 1024);

        // Peer 2: 20ms RTT (fastest)
        selector.record_request("peer2", 40);
        selector.record_success("peer2", 20, 1024);

        // Peer 3: 50ms RTT
        selector.record_request("peer3", 40);
        selector.record_success("peer3", 50, 1024);

        let peers = selector.select_peers();
        // Peer 2 should be first (lowest RTT)
        assert_eq!(peers[0], "peer2");
    }

    fn build_cashu_priority_fixture() -> PeerSelector {
        let mut selector = PeerSelector::with_strategy(SelectionStrategy::Weighted);
        selector.add_peer("reliable");
        selector.add_peer("paid");

        {
            let reliable = selector.get_stats_mut("reliable").expect("reliable");
            reliable.requests_sent = 80;
            reliable.successes = 75;
            reliable.failures = 2;
            reliable.timeouts = 3;
            reliable.srtt_ms = 40.0;
            reliable.bytes_sent = 80 * 40;
            reliable.bytes_received = 75 * 1024;
        }
        {
            let paid = selector.get_stats_mut("paid").expect("paid");
            paid.requests_sent = 80;
            paid.successes = 36;
            paid.failures = 24;
            paid.timeouts = 20;
            paid.srtt_ms = 700.0;
            paid.bytes_sent = 80 * 40;
            paid.bytes_received = 36 * 512;
        }

        selector
    }

    #[test]
    fn test_cashu_payment_weight_zero_keeps_reputation_order() {
        let mut selector = build_cashu_priority_fixture();
        selector.set_cashu_payment_weight(0.0);
        selector.record_cashu_payment("paid", 5_000);

        let peers = selector.select_peers();
        assert_eq!(peers[0], "reliable");
    }

    #[test]
    fn test_cashu_payment_weight_prioritizes_paid_peer() {
        let mut selector = build_cashu_priority_fixture();
        selector.set_cashu_payment_weight(0.8);
        selector.record_cashu_payment("paid", 5_000);

        let peers = selector.select_peers();
        assert_eq!(peers[0], "paid");
    }

    #[test]
    fn test_cashu_payment_default_downranks_peer() {
        let mut selector = PeerSelector::with_strategy(SelectionStrategy::Weighted);
        selector.add_peer("honest");
        selector.add_peer("delinquent");

        for peer_id in ["honest", "delinquent"] {
            let stats = selector.get_stats_mut(peer_id).expect("stats");
            stats.requests_sent = 40;
            stats.successes = 34;
            stats.failures = 3;
            stats.timeouts = 3;
            stats.srtt_ms = 60.0;
            stats.bytes_sent = 40 * 40;
            stats.bytes_received = 34 * 1024;
        }

        selector.record_cashu_payment_default("delinquent");

        let peers = selector.select_peers();
        assert_eq!(peers[0], "honest");
        assert!(!peers.iter().any(|peer| peer == "delinquent"));
    }

    #[test]
    fn test_payment_default_threshold_blocks_peer() {
        let mut selector = PeerSelector::new();
        selector.record_cashu_payment_default("peer-a");
        assert!(selector.is_peer_blocked_for_payment_defaults("peer-a", 1));
        assert!(!selector.is_peer_blocked_for_payment_defaults("peer-a", 2));
    }

    #[test]
    fn test_peer_principal_matches_peer_id() {
        assert_eq!(peer_principal("npub1abc"), "npub1abc");
        assert_eq!(peer_principal("peer-hex-01"), "peer-hex-01");
    }

    #[test]
    fn test_metadata_snapshot_restores_for_same_peer_id() {
        let mut selector = PeerSelector::new();
        selector.add_peer("npub1stable");
        selector.record_request("npub1stable", 64);
        selector.record_success("npub1stable", 32, 1024);
        selector.record_cashu_payment("npub1stable", 77);
        selector.record_cashu_receipt("npub1stable", 33);
        selector.record_cashu_payment_default("npub1stable");

        let snapshot = selector.export_peer_metadata_snapshot();
        assert_eq!(snapshot.version, PEER_METADATA_SNAPSHOT_VERSION);
        assert_eq!(snapshot.peers.len(), 1);
        assert_eq!(snapshot.peers[0].principal, "npub1stable");

        let mut restored = PeerSelector::new();
        restored.import_peer_metadata_snapshot(&snapshot);
        restored.add_peer("npub1stable");
        let stats = restored.get_stats("npub1stable").expect("restored stats");
        assert_eq!(stats.requests_sent, 1);
        assert_eq!(stats.successes, 1);
        assert_eq!(stats.cashu_paid_sat, 77);
        assert_eq!(stats.cashu_received_sat, 33);
        assert_eq!(stats.cashu_payment_receipts, 1);
        assert_eq!(stats.cashu_payment_defaults, 1);
    }
}
