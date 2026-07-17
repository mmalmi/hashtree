use super::PoolMemberId;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::time::{Duration, Instant};

const EWMA_ALPHA: f64 = 0.2;
const EXPLORATION_INTERVAL: u64 = 16;

#[derive(Debug, Default, Clone)]
struct MemberOutcome {
    read_latency_micros: Option<f64>,
    write_nanos_per_byte: Option<f64>,
    successes: f64,
    failures: f64,
    samples: u64,
    cooldown_until: Option<Instant>,
}

impl MemberOutcome {
    fn decay_and_record(&mut self, success: bool) {
        self.successes *= 0.9;
        self.failures *= 0.9;
        if success {
            self.successes += 1.0;
        } else {
            self.failures += 1.0;
        }
        self.samples = self.samples.saturating_add(1);
    }

    fn reliability(&self) -> f64 {
        (self.successes + 1.0) / (self.successes + self.failures + 2.0)
    }

    fn cooling(&self, now: Instant) -> bool {
        self.cooldown_until.is_some_and(|until| until > now)
    }

    fn comparable_latency(&self, other: &Self) -> Option<(f64, f64)> {
        self.read_latency_micros
            .zip(other.read_latency_micros)
            .or_else(|| self.write_nanos_per_byte.zip(other.write_nanos_per_byte))
    }
}

#[derive(Debug)]
pub(super) struct AdaptivePoolState {
    outcomes: HashMap<PoolMemberId, MemberOutcome>,
    selection_count: u64,
    failure_cooldown: Duration,
}

impl AdaptivePoolState {
    pub(super) fn new(failure_cooldown: Duration) -> Self {
        Self {
            outcomes: HashMap::new(),
            selection_count: 0,
            failure_cooldown,
        }
    }

    pub(super) fn record_read(&mut self, id: PoolMemberId, elapsed: Duration, success: bool) {
        let outcome = self.outcomes.entry(id).or_default();
        outcome.decay_and_record(success);
        if success {
            let sample = elapsed.as_secs_f64() * 1_000_000.0;
            outcome.read_latency_micros = Some(match outcome.read_latency_micros {
                Some(previous) => previous * (1.0 - EWMA_ALPHA) + sample * EWMA_ALPHA,
                None => sample,
            });
            outcome.cooldown_until = None;
        } else {
            outcome.cooldown_until = Some(Instant::now() + self.failure_cooldown);
        }
    }

    pub(super) fn record_write(
        &mut self,
        id: PoolMemberId,
        elapsed: Duration,
        bytes: usize,
        success: bool,
    ) {
        let outcome = self.outcomes.entry(id).or_default();
        outcome.decay_and_record(success);
        if success {
            let sample = elapsed.as_nanos() as f64 / bytes.max(1) as f64;
            outcome.write_nanos_per_byte = Some(match outcome.write_nanos_per_byte {
                Some(previous) => previous * (1.0 - EWMA_ALPHA) + sample * EWMA_ALPHA,
                None => sample,
            });
            outcome.cooldown_until = None;
        } else {
            outcome.cooldown_until = Some(Instant::now() + self.failure_cooldown);
        }
    }

    pub(super) fn order_reads(&mut self, ids: &mut [PoolMemberId]) {
        let now = Instant::now();
        self.selection_count = self.selection_count.saturating_add(1);
        let explore = self.selection_count.is_multiple_of(EXPLORATION_INTERVAL);
        ids.sort_by(|left, right| {
            let left = self.outcomes.get(left).cloned().unwrap_or_default();
            let right = self.outcomes.get(right).cloned().unwrap_or_default();
            if explore {
                return left.samples.cmp(&right.samples);
            }
            left.cooling(now)
                .cmp(&right.cooling(now))
                .then_with(|| {
                    right
                        .reliability()
                        .partial_cmp(&left.reliability())
                        .unwrap_or(Ordering::Equal)
                })
                .then_with(|| {
                    left.read_latency_micros
                        .unwrap_or(f64::MAX)
                        .partial_cmp(&right.read_latency_micros.unwrap_or(f64::MAX))
                        .unwrap_or(Ordering::Equal)
                })
        });
    }

    pub(super) fn choose_write(
        &mut self,
        candidates: &[(PoolMemberId, u64, u64)],
    ) -> Option<PoolMemberId> {
        if candidates.is_empty() {
            return None;
        }
        self.selection_count = self.selection_count.saturating_add(1);
        let explore = self.selection_count.is_multiple_of(EXPLORATION_INTERVAL);
        let now = Instant::now();
        candidates
            .iter()
            .min_by(
                |(left_id, left_used, left_capacity), (right_id, right_used, right_capacity)| {
                    let left = self.outcomes.get(left_id).cloned().unwrap_or_default();
                    let right = self.outcomes.get(right_id).cloned().unwrap_or_default();
                    if explore {
                        return left.samples.cmp(&right.samples);
                    }

                    // Compare utilization in coarse buckets so transient speed differences can
                    // influence scheduling without allowing the fast member to consume the pool.
                    let left_fill = fill_bucket(*left_used, *left_capacity);
                    let right_fill = fill_bucket(*right_used, *right_capacity);
                    left.cooling(now)
                        .cmp(&right.cooling(now))
                        .then_with(|| left_fill.cmp(&right_fill))
                        .then_with(|| {
                            left.write_nanos_per_byte
                                .unwrap_or(0.0)
                                .partial_cmp(&right.write_nanos_per_byte.unwrap_or(0.0))
                                .unwrap_or(Ordering::Equal)
                        })
                        .then_with(|| {
                            right
                                .reliability()
                                .partial_cmp(&left.reliability())
                                .unwrap_or(Ordering::Equal)
                        })
                        .then_with(|| left_id.cmp(right_id))
                },
            )
            .map(|(id, _, _)| *id)
    }

    pub(super) fn retain(&mut self, configured: &std::collections::HashSet<PoolMemberId>) {
        self.outcomes.retain(|id, _| configured.contains(id));
    }

    pub(super) fn meaningfully_faster(
        &self,
        target: PoolMemberId,
        source: PoolMemberId,
        hysteresis_percent: u8,
    ) -> bool {
        let now = Instant::now();
        let Some(target) = self.outcomes.get(&target) else {
            return false;
        };
        let Some(source) = self.outcomes.get(&source) else {
            return false;
        };
        if target.cooling(now)
            || (target.failures > 0.0 && target.reliability() + 0.05 < source.reliability())
        {
            return false;
        }
        let Some((target_latency, source_latency)) = target.comparable_latency(source) else {
            return false;
        };
        target_latency * (100.0 + f64::from(hysteresis_percent)) <= source_latency * 100.0
    }

    pub(super) fn order_temperature_targets(&self, ids: &mut [PoolMemberId]) {
        let now = Instant::now();
        ids.sort_by(|left, right| {
            let left = self.outcomes.get(left).cloned().unwrap_or_default();
            let right = self.outcomes.get(right).cloned().unwrap_or_default();
            left.cooling(now)
                .cmp(&right.cooling(now))
                .then_with(|| {
                    right
                        .reliability()
                        .partial_cmp(&left.reliability())
                        .unwrap_or(Ordering::Equal)
                })
                .then_with(
                    || match (left.read_latency_micros, right.read_latency_micros) {
                        (Some(left), Some(right)) => {
                            left.partial_cmp(&right).unwrap_or(Ordering::Equal)
                        }
                        (Some(_), None) => Ordering::Less,
                        (None, Some(_)) => Ordering::Greater,
                        (None, None) => left
                            .write_nanos_per_byte
                            .unwrap_or(f64::MAX)
                            .partial_cmp(&right.write_nanos_per_byte.unwrap_or(f64::MAX))
                            .unwrap_or(Ordering::Equal),
                    },
                )
                .then_with(|| left.samples.cmp(&right.samples))
        });
    }
}

fn fill_bucket(used: u64, capacity: u64) -> u16 {
    if capacity == 0 {
        return 0;
    }
    used.saturating_mul(32).saturating_div(capacity).min(32) as u16
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn id(value: u8) -> PoolMemberId {
        PoolMemberId([value; 16])
    }

    #[test]
    fn successful_write_throughput_changes_preference_without_affecting_correctness() {
        let slow = id(1);
        let fast = id(2);
        let mut state = AdaptivePoolState::new(Duration::from_secs(10));
        state.record_write(slow, Duration::from_millis(100), 1_000, true);
        state.record_write(fast, Duration::from_millis(5), 1_000, true);

        let selected = state.choose_write(&[(slow, 100, 1_000), (fast, 100, 1_000)]);
        assert_eq!(selected, Some(fast));
    }

    #[test]
    fn failure_cools_member_and_success_recovers_it() {
        let stable = id(1);
        let recovered = id(2);
        let mut state = AdaptivePoolState::new(Duration::from_secs(10));
        state.record_write(stable, Duration::from_millis(50), 1_000, true);
        state.record_write(recovered, Duration::from_millis(5), 1_000, true);
        state.record_write(recovered, Duration::ZERO, 1, false);
        assert_eq!(
            state.choose_write(&[(stable, 100, 1_000), (recovered, 100, 1_000)]),
            Some(stable)
        );

        state.record_write(recovered, Duration::from_millis(4), 1_000, true);
        assert_eq!(
            state.choose_write(&[(stable, 100, 1_000), (recovered, 100, 1_000)]),
            Some(recovered)
        );
    }

    #[test]
    fn removed_members_do_not_leave_unbounded_learning_state() {
        let retained = id(1);
        let removed = id(2);
        let mut state = AdaptivePoolState::new(Duration::from_secs(1));
        state.record_read(retained, Duration::from_millis(1), true);
        state.record_read(removed, Duration::from_millis(1), true);
        state.retain(&HashSet::from([retained]));
        assert!(state.outcomes.contains_key(&retained));
        assert!(!state.outcomes.contains_key(&removed));
    }

    #[test]
    fn temperature_moves_require_a_measured_hysteretic_speed_gain() {
        let slow = id(1);
        let slightly_faster = id(2);
        let fast = id(3);
        let mut state = AdaptivePoolState::new(Duration::from_secs(1));
        state.record_read(slow, Duration::from_millis(100), true);
        state.record_read(slightly_faster, Duration::from_millis(90), true);
        state.record_read(fast, Duration::from_millis(50), true);
        assert!(!state.meaningfully_faster(slightly_faster, slow, 20));
        assert!(state.meaningfully_faster(fast, slow, 20));
        let mut ordered = [slow, fast, slightly_faster];
        state.order_temperature_targets(&mut ordered);
        assert_eq!(ordered, [fast, slightly_faster, slow]);
    }
}
