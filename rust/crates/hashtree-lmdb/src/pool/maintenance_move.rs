use super::{LocationRecord, PoolMemberId, PoolMemberState, PoolStore};
use hashtree_core::store::StoreError;
use hashtree_core::types::Hash;

impl PoolStore {
    pub(super) fn move_blob(
        &self,
        source: PoolMemberId,
        target: PoolMemberId,
        hash: Hash,
    ) -> Result<Option<u64>, StoreError> {
        let Some(location) = self.read_location(&hash)? else {
            return Ok(None);
        };
        let (actual_target, size, moving) = match location {
            LocationRecord::Pending { member, size } | LocationRecord::Stored { member, size }
                if member == source =>
            {
                (target, size, false)
            }
            LocationRecord::Moving {
                source: moving_source,
                target,
                size,
            } if moving_source == source => (target, size, true),
            _ => return Ok(None),
        };
        self.move_blob_inner(source, actual_target, hash, location, size, moving)
    }

    fn move_blob_inner(
        &self,
        source: PoolMemberId,
        target: PoolMemberId,
        hash: Hash,
        location: LocationRecord,
        size: u64,
        moving: bool,
    ) -> Result<Option<u64>, StoreError> {
        let target_store = self.get_member(target)?;
        let source_store = match self.get_member(source) {
            Ok(store) => store,
            Err(_) if moving => {
                target_store.verify_blob_streaming(
                    &hash,
                    size,
                    self.temperature_config.copy_chunk_bytes,
                )?;
                self.complete_move(hash, source, target, size)?;
                return Ok(Some(size));
            }
            Err(error) => return Err(error),
        };
        let source_size = match source_store.blob_size_sync(&hash) {
            Ok(size) => size,
            Err(error) if moving => {
                target_store.verify_blob_streaming(
                    &hash,
                    size,
                    self.temperature_config.copy_chunk_bytes,
                )?;
                self.complete_move(hash, source, target, size)?;
                self.delete_completed_source(hash, source, target, size, &source_store)
                    .map_err(|cleanup_error| {
                        StoreError::Other(format!(
                            "pool source {source} could not be inspected ({error}); cleanup failed: {cleanup_error}"
                        ))
                    })?;
                return Ok(Some(size));
            }
            Err(error) => return Err(error),
        };
        if source_size.is_none() && moving {
            target_store.verify_blob_streaming(
                &hash,
                size,
                self.temperature_config.copy_chunk_bytes,
            )?;
            self.complete_move(hash, source, target, size)?;
            self.delete_completed_source(hash, source, target, size, &source_store)?;
            return Ok(Some(size));
        }
        let source_size = source_size.ok_or_else(|| {
            StoreError::Other(format!(
                "draining source {source} does not contain the blob"
            ))
        })?;
        if source_size != size {
            return Err(StoreError::Other(format!(
                "pool source {source} size mismatch: catalog={size}, member={source_size}"
            )));
        }

        let moving = LocationRecord::Moving {
            source,
            target,
            size,
        };
        if !self.begin_move_record(hash, location, moving)? {
            return Ok(None);
        }
        let source_gate = self.member_gate(source, false)?;
        let target_gate = self.member_gate(target, true)?;
        let _source_permit = source_gate.acquire()?;
        let _target_permit = target_gate.acquire()?;
        source_store.copy_blob_to_sync(
            &target_store,
            &hash,
            size,
            self.temperature_config.copy_chunk_bytes,
        )?;
        self.complete_move(hash, source, target, size)?;
        self.delete_completed_source(hash, source, target, size, &source_store)?;
        Ok(Some(size))
    }

    fn complete_move(
        &self,
        hash: Hash,
        source: PoolMemberId,
        target: PoolMemberId,
        size: u64,
    ) -> Result<(), StoreError> {
        let current = self.read_location(&hash)?;
        match current {
            Some(LocationRecord::Moving { .. }) | Some(LocationRecord::Stored { .. }) => {
                self.finish_move_record(hash, source, target, size)
            }
            other => Err(StoreError::Other(format!(
                "pool location disappeared while moving {hash:?}: {other:?}"
            ))),
        }
    }

    fn delete_completed_source(
        &self,
        hash: Hash,
        source: PoolMemberId,
        target: PoolMemberId,
        size: u64,
        source_store: &crate::LmdbBlobStore,
    ) -> Result<(), StoreError> {
        self.delete_member_blob(source, source_store, &hash)?;
        self.clear_move_cleanup_records(&[(hash, source, target, size)])
    }

    pub(super) fn rebalance_pair(
        &self,
    ) -> Result<Option<(PoolMemberId, PoolMemberId)>, StoreError> {
        let manifest = self.read_manifest()?;
        let mut members = Vec::new();
        for member in manifest
            .members
            .into_iter()
            .filter(|member| member.state == PoolMemberState::Active)
        {
            let Ok(store) = self.get_member(member.id) else {
                continue;
            };
            let stats = store.stats()?;
            members.push((member.id, stats.total_bytes, member.config.capacity_bytes));
        }
        if members.len() < 2 {
            return Ok(None);
        }
        let total_bytes = members
            .iter()
            .map(|(_, bytes, _)| *bytes)
            .fold(0u64, u64::saturating_add);
        let total_capacity = members
            .iter()
            .map(|(_, _, capacity)| *capacity)
            .fold(0u64, u64::saturating_add);
        if total_bytes == 0 || total_capacity == 0 {
            return Ok(None);
        }
        let deviation = |bytes: u64, capacity: u64| -> i128 {
            i128::from(bytes) * i128::from(total_capacity)
                - i128::from(total_bytes) * i128::from(capacity)
        };
        let source = members
            .iter()
            .max_by_key(|(_, bytes, capacity)| deviation(*bytes, *capacity))
            .copied();
        let target = members
            .iter()
            .min_by_key(|(_, bytes, capacity)| deviation(*bytes, *capacity))
            .copied();
        match (source, target) {
            (
                Some((source, source_bytes, source_capacity)),
                Some((target, target_bytes, target_capacity)),
            ) if source != target
                && deviation(source_bytes, source_capacity) > 0
                && deviation(target_bytes, target_capacity) < 0 =>
            {
                Ok(Some((source, target)))
            }
            _ => Ok(None),
        }
    }

    pub(super) fn move_improves_balance(
        &self,
        source: PoolMemberId,
        target: PoolMemberId,
        blob_bytes: u64,
    ) -> Result<bool, StoreError> {
        let members = self.members()?;
        let active = members
            .iter()
            .filter(|member| member.state == PoolMemberState::Active && member.available)
            .collect::<Vec<_>>();
        let Some(source_status) = active.iter().find(|member| member.id == source) else {
            return Ok(false);
        };
        let Some(target_status) = active.iter().find(|member| member.id == target) else {
            return Ok(false);
        };
        if blob_bytes > source_status.logical_bytes
            || target_status.logical_bytes.saturating_add(blob_bytes) > target_status.capacity_bytes
        {
            return Ok(false);
        }
        let total_bytes = active
            .iter()
            .map(|member| member.logical_bytes)
            .fold(0u64, u64::saturating_add);
        let total_capacity = active
            .iter()
            .map(|member| member.capacity_bytes)
            .fold(0u64, u64::saturating_add);
        if total_capacity == 0 {
            return Ok(false);
        }
        let deviation = |bytes: u64, capacity: u64| -> i128 {
            i128::from(bytes) * i128::from(total_capacity)
                - i128::from(total_bytes) * i128::from(capacity)
        };
        let before = deviation(source_status.logical_bytes, source_status.capacity_bytes).abs()
            + deviation(target_status.logical_bytes, target_status.capacity_bytes).abs();
        let after = deviation(
            source_status.logical_bytes - blob_bytes,
            source_status.capacity_bytes,
        )
        .abs()
            + deviation(
                target_status.logical_bytes.saturating_add(blob_bytes),
                target_status.capacity_bytes,
            )
            .abs();
        Ok(after < before)
    }
}
