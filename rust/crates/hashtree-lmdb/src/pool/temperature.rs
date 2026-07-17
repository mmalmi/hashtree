use super::{LocationRecord, PoolMemberId};
use hashtree_core::types::Hash;
use std::collections::HashMap;

const ACCESS_RECORD_VERSION: u8 = 1;
pub(super) const HEAT_UNIT: u32 = 1_024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct AccessRecord {
    pub(super) last_accessed_at: u64,
    pub(super) heat: u32,
    pub(super) heat_updated_at: u64,
    pub(super) placed_at: u64,
}

impl AccessRecord {
    pub(super) fn new(now: u64) -> Self {
        Self {
            last_accessed_at: now,
            heat: 0,
            heat_updated_at: now,
            placed_at: now,
        }
    }

    pub(super) fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() == 8 {
            let timestamp = u64::from_be_bytes(bytes.try_into().ok()?);
            return Some(Self::new(timestamp));
        }
        if bytes.len() != 29 || bytes[0] != ACCESS_RECORD_VERSION {
            return None;
        }
        Some(Self {
            last_accessed_at: u64::from_be_bytes(bytes[1..9].try_into().ok()?),
            heat: u32::from_be_bytes(bytes[9..13].try_into().ok()?),
            heat_updated_at: u64::from_be_bytes(bytes[13..21].try_into().ok()?),
            placed_at: u64::from_be_bytes(bytes[21..29].try_into().ok()?),
        })
    }

    pub(super) fn encode(self) -> [u8; 29] {
        let mut bytes = [0u8; 29];
        bytes[0] = ACCESS_RECORD_VERSION;
        bytes[1..9].copy_from_slice(&self.last_accessed_at.to_be_bytes());
        bytes[9..13].copy_from_slice(&self.heat.to_be_bytes());
        bytes[13..21].copy_from_slice(&self.heat_updated_at.to_be_bytes());
        bytes[21..29].copy_from_slice(&self.placed_at.to_be_bytes());
        bytes
    }

    pub(super) fn decayed_heat(self, now: u64, half_life_secs: u64) -> u32 {
        let half_life_secs = half_life_secs.max(1);
        let shifts = now
            .saturating_sub(self.heat_updated_at)
            .saturating_div(half_life_secs)
            .min(31) as u32;
        self.heat >> shifts
    }

    pub(super) fn record_samples(&mut self, samples: u32, now: u64, half_life_secs: u64) {
        self.heat = self
            .decayed_heat(now, half_life_secs)
            .saturating_add(samples.saturating_mul(HEAT_UNIT));
        self.heat_updated_at = now;
        self.last_accessed_at = self.last_accessed_at.max(now);
    }

    pub(super) fn mark_moved(&mut self, now: u64) {
        self.placed_at = now;
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct TemperatureCandidate {
    pub(super) hash: Hash,
    pub(super) member: PoolMemberId,
    pub(super) size: u64,
    pub(super) heat: u32,
    pub(super) last_accessed_at: u64,
    pub(super) placed_at: u64,
}

#[derive(Debug, Clone, Copy)]
struct ClockSlot {
    candidate: TemperatureCandidate,
    referenced: bool,
}

#[derive(Debug)]
pub(super) struct CandidateClock {
    slots: Vec<Option<ClockSlot>>,
    positions: HashMap<Hash, usize>,
    hand: usize,
    capacity: usize,
}

impl CandidateClock {
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            slots: Vec::with_capacity(capacity),
            positions: HashMap::with_capacity(capacity),
            hand: 0,
            capacity,
        }
    }

    pub(super) fn len(&self) -> usize {
        self.positions.len()
    }

    pub(super) fn upsert(&mut self, candidate: TemperatureCandidate) {
        if self.capacity == 0 {
            return;
        }
        if let Some(index) = self.positions.get(&candidate.hash).copied() {
            self.slots[index] = Some(ClockSlot {
                candidate,
                referenced: true,
            });
            return;
        }
        if self.slots.len() < self.capacity {
            let index = self.slots.len();
            self.slots.push(Some(ClockSlot {
                candidate,
                referenced: true,
            }));
            self.positions.insert(candidate.hash, index);
            return;
        }
        if let Some(index) = self.slots.iter().position(Option::is_none) {
            self.slots[index] = Some(ClockSlot {
                candidate,
                referenced: true,
            });
            self.positions.insert(candidate.hash, index);
            return;
        }

        loop {
            let index = self.hand;
            self.hand = (self.hand + 1) % self.capacity;
            let slot = self.slots[index].as_mut().expect("full CLOCK slot");
            if slot.referenced {
                slot.referenced = false;
                continue;
            }
            self.positions.remove(&slot.candidate.hash);
            *slot = ClockSlot {
                candidate,
                referenced: true,
            };
            self.positions.insert(candidate.hash, index);
            break;
        }
    }

    pub(super) fn remove_best_by(
        &mut self,
        mut better: impl FnMut(&TemperatureCandidate, &TemperatureCandidate) -> bool,
    ) -> Option<TemperatureCandidate> {
        let mut best: Option<(usize, TemperatureCandidate)> = None;
        for (index, slot) in self.slots.iter().enumerate() {
            let Some(slot) = slot else {
                continue;
            };
            if best
                .as_ref()
                .is_none_or(|(_, current)| better(&slot.candidate, current))
            {
                best = Some((index, slot.candidate));
            }
        }
        let (index, candidate) = best?;
        self.slots[index] = None;
        self.positions.remove(&candidate.hash);
        Some(candidate)
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct SampledAccess {
    pub(super) hash: Hash,
    pub(super) location: LocationRecord,
    pub(super) samples: u32,
    pub(super) observed_at: u64,
}

#[derive(Debug)]
pub(super) struct SampleClock {
    slots: CandidateClock,
    samples: HashMap<Hash, SampledAccess>,
}

impl SampleClock {
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            slots: CandidateClock::new(capacity),
            samples: HashMap::with_capacity(capacity),
        }
    }

    pub(super) fn observe(&mut self, hash: Hash, location: LocationRecord, now: u64) {
        let sample = self.samples.entry(hash).or_insert(SampledAccess {
            hash,
            location,
            samples: 0,
            observed_at: now,
        });
        sample.location = location;
        sample.samples = sample.samples.saturating_add(1);
        sample.observed_at = sample.observed_at.max(now);
        self.slots.upsert(TemperatureCandidate {
            hash,
            member: location.preferred_member(),
            size: location.size(),
            heat: sample.samples,
            last_accessed_at: sample.observed_at,
            placed_at: 0,
        });
        if self.samples.len() > self.slots.len() {
            self.samples
                .retain(|hash, _| self.slots.positions.contains_key(hash));
        }
    }

    pub(super) fn drain(&mut self, limit: usize) -> Vec<SampledAccess> {
        if limit == 0 {
            return Vec::new();
        }
        let hashes = self.samples.keys().copied().take(limit).collect::<Vec<_>>();
        let mut drained = Vec::with_capacity(hashes.len());
        for hash in hashes {
            if let Some(sample) = self.samples.remove(&hash) {
                self.slots.positions.remove(&hash);
                if let Some(index) = self
                    .slots
                    .slots
                    .iter()
                    .position(|slot| slot.is_some_and(|slot| slot.candidate.hash == hash))
                {
                    self.slots.slots[index] = None;
                }
                drained.push(sample);
            }
        }
        drained
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.samples.len()
    }
}

#[derive(Debug)]
pub(super) struct TemperatureRuntime {
    pub(super) samples: SampleClock,
    pub(super) hot: CandidateClock,
    pub(super) cold: CandidateClock,
}

impl TemperatureRuntime {
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            samples: SampleClock::new(capacity),
            hot: CandidateClock::new(capacity),
            cold: CandidateClock::new(capacity),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(value: u64) -> Hash {
        let mut hash = [0u8; 32];
        hash[..8].copy_from_slice(&value.to_be_bytes());
        hash
    }

    fn member(value: u8) -> PoolMemberId {
        PoolMemberId([value; 16])
    }

    fn candidate(value: u64, heat: u32) -> TemperatureCandidate {
        TemperatureCandidate {
            hash: hash(value),
            member: member(1),
            size: 1,
            heat,
            last_accessed_at: value,
            placed_at: 0,
        }
    }

    #[test]
    fn access_heat_decays_and_legacy_timestamps_remain_readable() {
        let legacy = AccessRecord::decode(&42u64.to_be_bytes()).expect("legacy access");
        assert_eq!(legacy.last_accessed_at, 42);
        let mut access = AccessRecord::new(100);
        access.record_samples(8, 100, 10);
        assert_eq!(access.decayed_heat(110, 10), 4 * HEAT_UNIT);
        access.record_samples(1, 120, 10);
        assert_eq!(access.heat, 3 * HEAT_UNIT);
        assert_eq!(AccessRecord::decode(&access.encode()), Some(access));
    }

    #[test]
    fn clock_candidates_stay_bounded_and_refresh_hot_entries() {
        let mut clock = CandidateClock::new(3);
        clock.upsert(candidate(1, 1));
        clock.upsert(candidate(2, 2));
        clock.upsert(candidate(3, 3));
        clock.upsert(candidate(2, 20));
        clock.upsert(candidate(4, 4));
        assert_eq!(clock.len(), 3);
        let hottest = clock
            .remove_best_by(|candidate, current| candidate.heat > current.heat)
            .expect("hottest candidate");
        assert_eq!(hottest.hash, hash(2));
        assert_eq!(hottest.heat, 20);
    }

    #[test]
    fn sampled_reads_are_bounded_and_batch_drained() {
        let mut samples = SampleClock::new(4);
        for value in 0..100 {
            samples.observe(
                hash(value),
                LocationRecord::Stored {
                    member: member(1),
                    size: value,
                },
                value,
            );
        }
        assert_eq!(samples.len(), 4);
        assert_eq!(samples.drain(2).len(), 2);
        assert_eq!(samples.len(), 2);
    }

    #[test]
    fn tens_of_millions_of_candidate_ids_do_not_grow_metadata_memory() {
        let mut clock = CandidateClock::new(32);
        for value in 0..20_000_000u64 {
            clock.upsert(candidate(value, value as u32));
        }
        assert_eq!(clock.len(), 32);
        assert_eq!(clock.slots.capacity(), 32);
        assert!(clock.positions.capacity() < 128);
    }
}
