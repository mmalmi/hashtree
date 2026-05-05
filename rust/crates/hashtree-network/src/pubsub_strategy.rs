//! Pure peer traffic and pubsub fanout scheduling strategies.
//!
//! This module has no transport or runtime dependencies so content-hash
//! responses, production pubsub, and deterministic simulations can exercise the
//! same reciprocity ranking code.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PubsubSchedulingPolicy {
    Fair,
    Random,
    Reciprocal,
    ReciprocalDebt,
    AgingReciprocal,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PubsubSchedulerConfig {
    pub policy: PubsubSchedulingPolicy,
    pub fanout: usize,
    pub anonymous_free_credit_bytes: u64,
    pub reciprocal_credit_multiplier: f64,
    pub aging_credit_bytes: u64,
}

impl Default for PubsubSchedulerConfig {
    fn default() -> Self {
        Self {
            policy: PubsubSchedulingPolicy::Reciprocal,
            fanout: 8,
            anonymous_free_credit_bytes: 4 * 1024,
            reciprocal_credit_multiplier: 1.0,
            aging_credit_bytes: 2 * 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PeerTrafficSnapshot {
    pub bytes_sent: u64,
    pub bytes_received: u64,
    /// Bytes that matched local demand or downstream demand.
    ///
    /// Raw ingress can be spam. Reciprocity weighting should only reward bytes
    /// from content we requested, pubsub streams we subscribed to, or data we
    /// forwarded for a peer with an active downstream interest.
    pub useful_bytes_received: u64,
    pub bandwidth_debt: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PubsubCandidate {
    pub peer_id: String,
    pub traffic: PeerTrafficSnapshot,
    pub deferred_count: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PubsubSelection {
    pub selected: Vec<String>,
    pub deferred: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundJobCandidate {
    pub job_id: u64,
    pub peer_id: String,
    pub message_bytes: u64,
    pub queue_sequence: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutboundJobSelection {
    pub job_id: u64,
    pub virtual_finish: f64,
}

impl PubsubSchedulerConfig {
    pub fn select(
        &self,
        stream_id: &str,
        seq: u64,
        parent_id: &str,
        message_bytes: u64,
        candidates: &[PubsubCandidate],
    ) -> PubsubSelection {
        let mut ranked = candidates.to_vec();
        match self.policy {
            PubsubSchedulingPolicy::Fair => {
                ranked.sort_by(|left, right| {
                    left.traffic
                        .bytes_sent
                        .cmp(&right.traffic.bytes_sent)
                        .then_with(|| left.peer_id.cmp(&right.peer_id))
                });
            }
            PubsubSchedulingPolicy::Random => {
                ranked.sort_by_key(|candidate| {
                    stable_pubsub_score(stream_id, seq, parent_id, &candidate.peer_id)
                });
            }
            PubsubSchedulingPolicy::Reciprocal => {
                ranked.sort_by(|left, right| {
                    self.credit_score(right)
                        .partial_cmp(&self.credit_score(left))
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| left.traffic.bytes_sent.cmp(&right.traffic.bytes_sent))
                        .then_with(|| left.peer_id.cmp(&right.peer_id))
                });
            }
            PubsubSchedulingPolicy::ReciprocalDebt => {
                ranked.sort_by(|left, right| {
                    self.virtual_finish(left, message_bytes)
                        .partial_cmp(&self.virtual_finish(right, message_bytes))
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| left.peer_id.cmp(&right.peer_id))
                });
            }
            PubsubSchedulingPolicy::AgingReciprocal => {
                ranked.sort_by(|left, right| {
                    self.aging_score(right)
                        .partial_cmp(&self.aging_score(left))
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| left.traffic.bytes_sent.cmp(&right.traffic.bytes_sent))
                        .then_with(|| left.peer_id.cmp(&right.peer_id))
                });
            }
        }

        let capacity = self.fanout.min(ranked.len());
        let deferred = ranked
            .iter()
            .skip(capacity)
            .map(|candidate| candidate.peer_id.clone())
            .collect();
        let selected = ranked
            .into_iter()
            .take(capacity)
            .map(|candidate| candidate.peer_id)
            .collect();
        PubsubSelection { selected, deferred }
    }

    fn credit_score(&self, candidate: &PubsubCandidate) -> f64 {
        self.anonymous_free_credit_bytes as f64
            + candidate.traffic.useful_bytes_received as f64 * self.reciprocal_credit_multiplier
            - candidate.traffic.bytes_sent as f64
    }

    fn aging_score(&self, candidate: &PubsubCandidate) -> f64 {
        self.credit_score(candidate)
            + candidate.deferred_count as f64 * self.aging_credit_bytes as f64
    }

    pub fn virtual_finish(&self, candidate: &PubsubCandidate, message_bytes: u64) -> f64 {
        reciprocal_virtual_finish(candidate.traffic, message_bytes)
    }
}

pub fn reciprocal_upload_weight(traffic: PeerTrafficSnapshot) -> f64 {
    let raw_ratio = (traffic.bytes_received.saturating_add(1024) as f64)
        / (traffic.bytes_sent.saturating_add(1024) as f64);
    let useful_ratio = (traffic.useful_bytes_received.saturating_add(1024) as f64)
        / (traffic.bytes_sent.saturating_add(1024) as f64);
    let raw_ratio = raw_ratio.min(useful_ratio);
    let bounded_ratio = raw_ratio / (1.0 + raw_ratio);
    0.5 + 1.5 * bounded_ratio
}

pub fn reciprocal_virtual_finish(traffic: PeerTrafficSnapshot, message_bytes: u64) -> f64 {
    traffic.bandwidth_debt + message_bytes as f64 / reciprocal_upload_weight(traffic)
}

pub fn select_reciprocal_outbound_job(
    jobs: &[OutboundJobCandidate],
    traffic_for_peer: impl Fn(&str) -> PeerTrafficSnapshot,
) -> Option<OutboundJobSelection> {
    jobs.iter()
        .map(|job| {
            let traffic = traffic_for_peer(&job.peer_id);
            (
                OutboundJobSelection {
                    job_id: job.job_id,
                    virtual_finish: reciprocal_virtual_finish(traffic, job.message_bytes),
                },
                job.queue_sequence,
                job.peer_id.as_str(),
            )
        })
        .min_by(|left, right| {
            left.0
                .virtual_finish
                .partial_cmp(&right.0.virtual_finish)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.1.cmp(&right.1))
                .then_with(|| left.2.cmp(right.2))
        })
        .map(|choice| choice.0)
}

pub fn stable_pubsub_score(stream_id: &str, seq: u64, parent_id: &str, peer_id: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    stream_id.hash(&mut hasher);
    seq.hash(&mut hasher);
    parent_id.hash(&mut hasher);
    peer_id.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(
        peer_id: &str,
        bytes_sent: u64,
        bytes_received: u64,
        bandwidth_debt: f64,
        deferred_count: u64,
    ) -> PubsubCandidate {
        PubsubCandidate {
            peer_id: peer_id.to_string(),
            traffic: PeerTrafficSnapshot {
                bytes_sent,
                bytes_received,
                useful_bytes_received: bytes_received,
                bandwidth_debt,
            },
            deferred_count,
        }
    }

    #[test]
    fn reciprocal_policy_prioritizes_peers_that_have_served_bandwidth() {
        let config = PubsubSchedulerConfig {
            policy: PubsubSchedulingPolicy::Reciprocal,
            fanout: 1,
            anonymous_free_credit_bytes: 0,
            reciprocal_credit_multiplier: 1.0,
            aging_credit_bytes: 0,
        };
        let selection = config.select(
            "author",
            1,
            "parent",
            1024,
            &[
                candidate("leecher", 0, 0, 0.0, 0),
                candidate("provider", 0, 64 * 1024, 0.0, 0),
            ],
        );

        assert_eq!(selection.selected, vec!["provider"]);
        assert_eq!(selection.deferred, vec!["leecher"]);
    }

    #[test]
    fn aging_reciprocal_eventually_schedules_deferred_peers() {
        let config = PubsubSchedulerConfig {
            policy: PubsubSchedulingPolicy::AgingReciprocal,
            fanout: 1,
            anonymous_free_credit_bytes: 0,
            reciprocal_credit_multiplier: 1.0,
            aging_credit_bytes: 16 * 1024,
        };
        let selection = config.select(
            "author",
            1,
            "parent",
            1024,
            &[
                candidate("fresh-provider", 0, 64 * 1024, 0.0, 0),
                candidate("starved-peer", 0, 0, 0.0, 5),
            ],
        );

        assert_eq!(selection.selected, vec!["starved-peer"]);
        assert_eq!(selection.deferred, vec!["fresh-provider"]);
    }

    #[test]
    fn reciprocal_debt_matches_content_hash_response_scheduler_shape() {
        let config = PubsubSchedulerConfig {
            policy: PubsubSchedulingPolicy::ReciprocalDebt,
            fanout: 1,
            anonymous_free_credit_bytes: 0,
            reciprocal_credit_multiplier: 1.0,
            aging_credit_bytes: 0,
        };
        let selection = config.select(
            "author",
            1,
            "parent",
            1024,
            &[
                candidate("already-served", 64 * 1024, 64 * 1024, 16_000.0, 0),
                candidate("low-debt-provider", 0, 64 * 1024, 0.0, 0),
            ],
        );

        assert_eq!(selection.selected, vec!["low-debt-provider"]);
        assert_eq!(selection.deferred, vec!["already-served"]);
    }

    #[test]
    fn reciprocal_upload_weight_prefers_peers_who_sent_us_more() {
        let provider = reciprocal_upload_weight(PeerTrafficSnapshot {
            bytes_sent: 1024,
            bytes_received: 64 * 1024,
            useful_bytes_received: 64 * 1024,
            bandwidth_debt: 0.0,
        });
        let leecher = reciprocal_upload_weight(PeerTrafficSnapshot {
            bytes_sent: 64 * 1024,
            bytes_received: 1024,
            useful_bytes_received: 1024,
            bandwidth_debt: 0.0,
        });
        assert!(provider > leecher);
    }

    #[test]
    fn reciprocal_upload_weight_ignores_useless_ingress_spam() {
        let spammer = reciprocal_upload_weight(PeerTrafficSnapshot {
            bytes_sent: 64 * 1024,
            bytes_received: 512 * 1024,
            useful_bytes_received: 0,
            bandwidth_debt: 0.0,
        });
        let useful = reciprocal_upload_weight(PeerTrafficSnapshot {
            bytes_sent: 64 * 1024,
            bytes_received: 64 * 1024,
            useful_bytes_received: 64 * 1024,
            bandwidth_debt: 0.0,
        });

        assert!(useful > spammer);
    }

    #[test]
    fn reciprocal_outbound_job_scheduler_uses_shared_traffic_snapshot() {
        let jobs = vec![
            OutboundJobCandidate {
                job_id: 1,
                peer_id: "spammer".to_string(),
                message_bytes: 4096,
                queue_sequence: 1,
            },
            OutboundJobCandidate {
                job_id: 2,
                peer_id: "useful".to_string(),
                message_bytes: 4096,
                queue_sequence: 2,
            },
        ];
        let selected = select_reciprocal_outbound_job(&jobs, |peer_id| match peer_id {
            "useful" => PeerTrafficSnapshot {
                bytes_sent: 1024,
                bytes_received: 16 * 1024,
                useful_bytes_received: 16 * 1024,
                bandwidth_debt: 0.0,
            },
            _ => PeerTrafficSnapshot {
                bytes_sent: 1024,
                bytes_received: 512 * 1024,
                useful_bytes_received: 0,
                bandwidth_debt: 0.0,
            },
        })
        .expect("selected job");

        assert_eq!(selected.job_id, 2);
    }
}
