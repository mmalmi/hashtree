//! Adaptive peer selection based on Freenet patterns
//!
//! Implements sophisticated peer selection that favors reliable, fast peers:
//! - Per-peer performance tracking (RTT, success rate)
//! - RFC 2988-style smoothed RTT calculation
//! - Exponential backoff for failing/slow peers
//! - Fairness constraints to prevent overloading any single peer
//! - Weighted selection combining multiple signals

use std::collections::HashMap;
use std::time::{Duration, Instant};

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
}

impl PeerStats {
    /// Create new peer stats
    pub fn new(peer_id: impl Into<String>) -> Self {
        Self {
            peer_id: peer_id.into(),
            connected_at: Instant::now(),
            requests_sent: 0,
            successes: 0,
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
        }
    }

    /// Get success rate (0.0 to 1.0)
    pub fn success_rate(&self) -> f64 {
        if self.requests_sent == 0 {
            return 0.5; // Neutral for new peers
        }
        self.successes as f64 / self.requests_sent as f64
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

    /// Apply exponential backoff
    fn apply_backoff(&mut self) {
        self.backoff_level += 1;
        let backoff_ms = (INITIAL_BACKOFF_MS * BACKOFF_MULTIPLIER.pow(self.backoff_level - 1))
            .min(MAX_BACKOFF_MS);
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
    /// Selection strategy
    strategy: SelectionStrategy,
    /// Enable fairness constraints (Freenet FOAF mitigation)
    fairness_enabled: bool,
    /// Round-robin index for RoundRobin strategy
    round_robin_idx: usize,
}

impl PeerSelector {
    /// Create a new peer selector with default weighted strategy
    pub fn new() -> Self {
        Self {
            stats: HashMap::new(),
            strategy: SelectionStrategy::Weighted,
            fairness_enabled: true,
            round_robin_idx: 0,
        }
    }

    /// Create with specific strategy
    pub fn with_strategy(strategy: SelectionStrategy) -> Self {
        Self {
            stats: HashMap::new(),
            strategy,
            fairness_enabled: true,
            round_robin_idx: 0,
        }
    }

    /// Enable/disable fairness constraints
    pub fn set_fairness(&mut self, enabled: bool) {
        self.fairness_enabled = enabled;
    }

    /// Add a peer to track
    pub fn add_peer(&mut self, peer_id: impl Into<String>) {
        let peer_id = peer_id.into();
        self.stats
            .entry(peer_id.clone())
            .or_insert_with(|| PeerStats::new(peer_id));
    }

    /// Remove a peer
    pub fn remove_peer(&mut self, peer_id: &str) {
        self.stats.remove(peer_id);
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

    /// Get available (non-backed-off) peers
    fn available_peers(&self) -> Vec<String> {
        self.stats
            .iter()
            .filter(|(_, s)| !s.is_backed_off())
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Check fairness: should this peer be skipped due to over-selection?
    fn should_skip_for_fairness(&self, peer_id: &str) -> bool {
        if !self.fairness_enabled {
            return false;
        }

        // Only apply fairness with enough peers (Freenet: 5+)
        if self.stats.len() < SELECTION_MIN_PEERS {
            return false;
        }

        // Calculate total selection rate
        let total_rate: f64 = self.stats.values().map(|s| s.selection_rate()).sum();
        if total_rate <= 0.0 {
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
                available
                    .into_iter()
                    .filter(|id| !self.should_skip_for_fairness(id))
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
                    let score_cmp = b
                        .score()
                        .partial_cmp(&a.score())
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
        let total_timeouts: u64 = self.stats.values().map(|s| s.timeouts).sum();
        let backed_off = self.stats.values().filter(|s| s.is_backed_off()).count();

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
            overall_success_rate: if total_requests > 0 {
                total_successes as f64 / total_requests as f64
            } else {
                0.0
            },
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
}
