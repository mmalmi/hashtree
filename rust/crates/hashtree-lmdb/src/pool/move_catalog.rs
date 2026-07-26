use super::temperature::AccessRecord;
use super::{map_heed, LocationRecord, PoolMemberId, PoolStore};
use hashtree_core::store::StoreError;
use hashtree_core::types::Hash;

const MOVE_CLEANUP_KEY_PREFIX: u8 = b'c';
const MOVE_KEY_PREFIX: u8 = b'm';

impl PoolStore {
    pub(super) fn begin_move_record(
        &self,
        hash: Hash,
        expected: LocationRecord,
        moving: LocationRecord,
    ) -> Result<bool, StoreError> {
        Ok(!self
            .begin_move_records(&[(hash, expected, moving)])?
            .is_empty())
    }

    pub(super) fn begin_move_records(
        &self,
        plans: &[(Hash, LocationRecord, LocationRecord)],
    ) -> Result<Vec<Hash>, StoreError> {
        if plans.is_empty() {
            return Ok(Vec::new());
        }
        let mut wtxn = self.env.write_txn().map_err(map_heed)?;
        let mut started = Vec::with_capacity(plans.len());
        for (hash, expected, moving) in plans {
            if !matches!(moving, LocationRecord::Moving { .. }) {
                return Err(StoreError::Other(
                    "pool move batch contains a non-moving target record".into(),
                ));
            }
            let current = self
                .locations
                .get(&wtxn, hash)
                .map_err(map_heed)?
                .map(LocationRecord::decode)
                .transpose()?;
            if current != Some(*expected) && current != Some(*moving) {
                continue;
            }
            if current != Some(*moving) {
                self.set_location_txn(&mut wtxn, *hash, Some(*moving))?;
            }
            self.temperature_state
                .put(&mut wtxn, &move_state_key(*hash), &moving.encode())
                .map_err(map_heed)?;
            started.push(*hash);
        }
        wtxn.commit().map_err(map_heed)?;
        Ok(started)
    }

    pub(super) fn finish_move_record(
        &self,
        hash: Hash,
        source: PoolMemberId,
        target: PoolMemberId,
        size: u64,
    ) -> Result<(), StoreError> {
        self.finish_move_records(&[(hash, source, target, size)])
    }

    pub(super) fn finish_move_records(
        &self,
        plans: &[(Hash, PoolMemberId, PoolMemberId, u64)],
    ) -> Result<(), StoreError> {
        if plans.is_empty() {
            return Ok(());
        }
        let mut wtxn = self.env.write_txn().map_err(map_heed)?;
        for (hash, source, target, size) in plans {
            let moving = LocationRecord::Moving {
                source: *source,
                target: *target,
                size: *size,
            };
            let current = self
                .locations
                .get(&wtxn, hash)
                .map_err(map_heed)?
                .map(LocationRecord::decode)
                .transpose()?;
            match current {
                Some(LocationRecord::Moving {
                    source: actual_source,
                    target: actual_target,
                    ..
                }) if actual_source == *source && actual_target == *target => {
                    self.set_location_txn(
                        &mut wtxn,
                        *hash,
                        Some(LocationRecord::Stored {
                            member: *target,
                            size: *size,
                        }),
                    )?;
                    if let Some(mut access) = self
                        .last_accessed
                        .get(&wtxn, hash)
                        .map_err(map_heed)?
                        .and_then(AccessRecord::decode)
                    {
                        access.mark_moved(super::unix_timestamp_now());
                        self.last_accessed
                            .put(&mut wtxn, hash, &access.encode())
                            .map_err(map_heed)?;
                    }
                }
                Some(LocationRecord::Stored {
                    member,
                    size: actual_size,
                }) if member == *target && actual_size == *size => {}
                other => {
                    return Err(StoreError::Other(format!(
                        "pool location changed while moving {hash:?}: {other:?}"
                    )))
                }
            }
            self.temperature_state
                .put(&mut wtxn, &move_cleanup_state_key(*hash), &moving.encode())
                .map_err(map_heed)?;
            self.temperature_state
                .delete(&mut wtxn, &move_state_key(*hash))
                .map_err(map_heed)?;
        }
        wtxn.commit().map_err(map_heed)
    }

    pub(super) fn clear_move_cleanup_records(
        &self,
        plans: &[(Hash, PoolMemberId, PoolMemberId, u64)],
    ) -> Result<(), StoreError> {
        if plans.is_empty() {
            return Ok(());
        }
        let mut wtxn = self.env.write_txn().map_err(map_heed)?;
        for (hash, source, target, size) in plans {
            let expected = LocationRecord::Moving {
                source: *source,
                target: *target,
                size: *size,
            };
            let cleanup = self
                .temperature_state
                .get(&wtxn, &move_cleanup_state_key(*hash))
                .map_err(map_heed)?
                .map(LocationRecord::decode)
                .transpose()?;
            let location = self
                .locations
                .get(&wtxn, hash)
                .map_err(map_heed)?
                .map(LocationRecord::decode)
                .transpose()?;
            let stored = matches!(
                location,
                Some(LocationRecord::Stored { member, size: actual_size })
                    if member == *target && actual_size == *size
            );
            if !stored || cleanup.is_some_and(|cleanup| cleanup != expected) {
                return Err(StoreError::Other(format!(
                    "pool cleanup state changed while deleting source for {hash:?}"
                )));
            }
            if cleanup.is_some() {
                self.temperature_state
                    .delete(&mut wtxn, &move_cleanup_state_key(*hash))
                    .map_err(map_heed)?;
            }
        }
        wtxn.commit().map_err(map_heed)
    }

    pub(super) fn active_moves(
        &self,
        limit: usize,
    ) -> Result<Vec<(Hash, LocationRecord)>, StoreError> {
        self.active_move_records(MOVE_KEY_PREFIX, limit, "move-state")
    }

    pub(super) fn active_move_cleanups(
        &self,
        limit: usize,
    ) -> Result<Vec<(Hash, LocationRecord)>, StoreError> {
        self.active_move_records(MOVE_CLEANUP_KEY_PREFIX, limit, "move-cleanup")
    }

    pub(super) fn count_move_cleanups_for_source_txn(
        &self,
        txn: &heed::RoTxn<'_>,
        source: PoolMemberId,
    ) -> Result<u64, StoreError> {
        let mut count = 0u64;
        for item in self
            .temperature_state
            .prefix_iter(txn, &[MOVE_CLEANUP_KEY_PREFIX])
            .map_err(map_heed)?
        {
            let (_, value) = item.map_err(map_heed)?;
            if matches!(
                LocationRecord::decode(value)?,
                LocationRecord::Moving {
                    source: actual_source,
                    ..
                } if actual_source == source
            ) {
                count = count.saturating_add(1);
            }
        }
        Ok(count)
    }

    fn active_move_records(
        &self,
        prefix: u8,
        limit: usize,
        label: &str,
    ) -> Result<Vec<(Hash, LocationRecord)>, StoreError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let rtxn = self.env.read_txn().map_err(map_heed)?;
        let mut moves = Vec::with_capacity(limit);
        for item in self
            .temperature_state
            .prefix_iter(&rtxn, &[prefix])
            .map_err(map_heed)?
        {
            let (key, value) = item.map_err(map_heed)?;
            if key.len() != 33 {
                return Err(StoreError::Other(format!("invalid pool {label} key")));
            }
            let hash: Hash = key[1..]
                .try_into()
                .map_err(|_| StoreError::Other(format!("invalid pool {label} hash")))?;
            moves.push((hash, LocationRecord::decode(value)?));
            if moves.len() >= limit {
                break;
            }
        }
        Ok(moves)
    }
}

pub(super) fn move_state_key(hash: Hash) -> [u8; 33] {
    move_record_key(MOVE_KEY_PREFIX, hash)
}

pub(super) fn move_cleanup_state_key(hash: Hash) -> [u8; 33] {
    move_record_key(MOVE_CLEANUP_KEY_PREFIX, hash)
}

fn move_record_key(prefix: u8, hash: Hash) -> [u8; 33] {
    let mut key = [0u8; 33];
    key[0] = prefix;
    key[1..].copy_from_slice(&hash);
    key
}
