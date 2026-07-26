use super::temperature::{AccessRecord, TemperatureCandidate};
use super::{map_heed, LocationRecord, PoolMemberId, PoolStore};
use hashtree_core::store::StoreError;
use hashtree_core::types::Hash;
use std::ops::Bound;

const CURSOR_KEY: &[u8] = b"temperature-cursor-v1";
const LEASE_KEY: &[u8] = b"temperature-lease-v1";

impl PoolStore {
    pub(super) fn flush_sampled_accesses(
        &self,
        now: u64,
    ) -> Result<Vec<TemperatureCandidate>, StoreError> {
        let samples = self
            .temperature
            .lock()
            .map_err(|_| StoreError::Other("pool temperature lock poisoned".into()))?
            .samples
            .drain(self.temperature_config.access_flush_batch);
        if samples.is_empty() {
            return Ok(Vec::new());
        }

        let half_life = self.temperature_config.heat_half_life.as_secs().max(1);
        let mut wtxn = self.env.write_txn().map_err(map_heed)?;
        let mut candidates = Vec::with_capacity(samples.len());
        for sample in samples {
            let Some(location) = self
                .locations
                .get(&wtxn, &sample.hash)
                .map_err(map_heed)?
                .map(LocationRecord::decode)
                .transpose()?
            else {
                continue;
            };
            let mut access = self
                .last_accessed
                .get(&wtxn, &sample.hash)
                .map_err(map_heed)?
                .and_then(AccessRecord::decode)
                .unwrap_or_else(|| AccessRecord::new(sample.observed_at));
            access.record_samples(sample.samples, sample.observed_at, half_life);
            let encoded = access.encode();
            self.last_accessed
                .put(&mut wtxn, &sample.hash, &encoded)
                .map_err(map_heed)?;
            candidates.push(TemperatureCandidate {
                hash: sample.hash,
                member: location.preferred_member(),
                size: location.size(),
                heat: access.decayed_heat(now, half_life),
                last_accessed_at: access.last_accessed_at,
                placed_at: access.placed_at,
            });
        }
        wtxn.commit().map_err(map_heed)?;
        Ok(candidates)
    }

    pub(super) fn scan_temperature_candidates(
        &self,
        now: u64,
    ) -> Result<Vec<TemperatureCandidate>, StoreError> {
        let limit = self.temperature_config.scan_items_per_cycle;
        if limit == 0 {
            return Ok(Vec::new());
        }
        let rtxn = self.env.read_txn().map_err(map_heed)?;
        let after = self
            .temperature_state
            .get(&rtxn, CURSOR_KEY)
            .map_err(map_heed)?
            .map(|bytes| {
                bytes
                    .try_into()
                    .map_err(|_| StoreError::Other("invalid temperature cursor".into()))
            })
            .transpose()?;
        let mut entries = Vec::with_capacity(limit);
        self.collect_temperature_range(&rtxn, after, limit, &mut entries)?;
        if entries.len() < limit && after.is_some() {
            self.collect_temperature_range(&rtxn, None, limit - entries.len(), &mut entries)?;
        }
        let half_life = self.temperature_config.heat_half_life.as_secs().max(1);
        for candidate in &mut entries {
            let access = self
                .last_accessed
                .get(&rtxn, &candidate.hash)
                .map_err(map_heed)?
                .and_then(AccessRecord::decode)
                .unwrap_or_else(|| AccessRecord::new(now));
            candidate.heat = access.decayed_heat(now, half_life);
            candidate.last_accessed_at = access.last_accessed_at;
            candidate.placed_at = access.placed_at;
        }
        drop(rtxn);

        let mut wtxn = self.env.write_txn().map_err(map_heed)?;
        if let Some(last) = entries.last().map(|candidate| candidate.hash) {
            self.temperature_state
                .put(&mut wtxn, CURSOR_KEY, &last)
                .map_err(map_heed)?;
        } else {
            self.temperature_state
                .delete(&mut wtxn, CURSOR_KEY)
                .map_err(map_heed)?;
        }
        wtxn.commit().map_err(map_heed)?;
        Ok(entries)
    }

    fn collect_temperature_range(
        &self,
        rtxn: &heed::RoTxn<'_>,
        after: Option<Hash>,
        limit: usize,
        entries: &mut Vec<TemperatureCandidate>,
    ) -> Result<(), StoreError> {
        let mut push = |hash: &[u8], location: &[u8]| -> Result<bool, StoreError> {
            let hash: Hash = hash
                .try_into()
                .map_err(|_| StoreError::Other("invalid pool location hash".into()))?;
            if entries.iter().any(|candidate| candidate.hash == hash) {
                return Ok(false);
            }
            let location = LocationRecord::decode(location)?;
            entries.push(TemperatureCandidate {
                hash,
                member: location.preferred_member(),
                size: location.size(),
                heat: 0,
                last_accessed_at: 0,
                placed_at: 0,
            });
            Ok(entries.len() >= limit)
        };

        match after {
            Some(after) => {
                let range = (Bound::Excluded(after.as_slice()), Bound::<&[u8]>::Unbounded);
                for item in self.locations.range(rtxn, &range).map_err(map_heed)? {
                    let (hash, location) = item.map_err(map_heed)?;
                    if push(hash, location)? {
                        break;
                    }
                }
            }
            None => {
                for item in self.locations.iter(rtxn).map_err(map_heed)? {
                    let (hash, location) = item.map_err(map_heed)?;
                    if push(hash, location)? {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    pub(super) fn try_acquire_temperature_lease(&self, now: u64) -> Result<bool, StoreError> {
        let mut wtxn = self.env.write_txn().map_err(map_heed)?;
        if let Some(bytes) = self
            .temperature_state
            .get(&wtxn, LEASE_KEY)
            .map_err(map_heed)?
        {
            if bytes.len() != 24 {
                return Err(StoreError::Other("invalid temperature lease".into()));
            }
            let owner = PoolMemberId(bytes[..16].try_into().expect("checked lease length"));
            let expires = u64::from_be_bytes(bytes[16..].try_into().expect("checked lease length"));
            if owner != self.temperature_owner && expires > now {
                return Ok(false);
            }
        }
        let mut lease = [0u8; 24];
        lease[..16].copy_from_slice(self.temperature_owner.as_bytes());
        lease[16..].copy_from_slice(&temperature_lease_expiry(self, now).to_be_bytes());
        self.temperature_state
            .put(&mut wtxn, LEASE_KEY, &lease)
            .map_err(map_heed)?;
        wtxn.commit().map_err(map_heed)?;
        Ok(true)
    }

    pub(super) fn renew_temperature_lease(&self, now: u64) -> Result<bool, StoreError> {
        let mut wtxn = self.env.write_txn().map_err(map_heed)?;
        let Some(bytes) = self
            .temperature_state
            .get(&wtxn, LEASE_KEY)
            .map_err(map_heed)?
        else {
            return Ok(false);
        };
        if bytes.len() != 24 {
            return Err(StoreError::Other("invalid temperature lease".into()));
        }
        let owner = PoolMemberId(bytes[..16].try_into().expect("checked lease length"));
        if owner != self.temperature_owner {
            return Ok(false);
        }
        let mut lease = [0u8; 24];
        lease[..16].copy_from_slice(self.temperature_owner.as_bytes());
        lease[16..].copy_from_slice(&temperature_lease_expiry(self, now).to_be_bytes());
        self.temperature_state
            .put(&mut wtxn, LEASE_KEY, &lease)
            .map_err(map_heed)?;
        wtxn.commit().map_err(map_heed)?;
        Ok(true)
    }

    #[cfg(test)]
    pub(super) fn release_temperature_lease(&self) -> Result<(), StoreError> {
        let mut wtxn = self.env.write_txn().map_err(map_heed)?;
        let owned = self
            .temperature_state
            .get(&wtxn, LEASE_KEY)
            .map_err(map_heed)?
            .is_some_and(|bytes| bytes.starts_with(self.temperature_owner.as_bytes()));
        if owned {
            self.temperature_state
                .delete(&mut wtxn, LEASE_KEY)
                .map_err(map_heed)?;
        }
        wtxn.commit().map_err(map_heed)
    }
}

fn temperature_lease_expiry(pool: &PoolStore, now: u64) -> u64 {
    // Lease storage uses whole seconds. One clock tick of grace prevents a
    // subsecond heartbeat from becoming expired at the same encoded second.
    now.saturating_add(
        pool.temperature_config
            .lease_duration
            .as_secs()
            .max(1)
            .saturating_add(1),
    )
}
