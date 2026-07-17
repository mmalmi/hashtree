use super::{LocationRecord, PoolMaintenanceReport, PoolMemberId, PoolMemberState, PoolStore};
use hashtree_core::store::StoreError;
use hashtree_core::types::Hash;

impl PoolStore {
    pub fn maintain(&self, max_items: usize) -> Result<PoolMaintenanceReport, StoreError> {
        let mut report = PoolMaintenanceReport::default();
        if max_items == 0 {
            return Ok(report);
        }
        for (hash, location) in self.active_moves(max_items)? {
            if report.examined >= max_items {
                return Ok(report);
            }
            report.examined += 1;
            let LocationRecord::Moving { source, target, .. } = location else {
                continue;
            };
            match self.move_blob(source, target, hash) {
                Ok(Some(bytes)) => {
                    report.moved += 1;
                    report.bytes_moved = report.bytes_moved.saturating_add(bytes);
                }
                Ok(None) => {}
                Err(error) => report.failed.push(format!("{hash:?}: {error}")),
            }
        }
        let draining = self
            .read_manifest()?
            .members
            .into_iter()
            .filter(|member| member.state == PoolMemberState::Draining)
            .map(|member| member.id)
            .collect::<Vec<_>>();

        for source in draining {
            let hashes = self.member_hashes(source, max_items.saturating_sub(report.examined))?;
            for hash in hashes {
                if report.examined >= max_items {
                    return Ok(report);
                }
                report.examined += 1;
                match self.move_from_draining(source, hash) {
                    Ok(Some(bytes)) => {
                        report.moved += 1;
                        report.bytes_moved = report.bytes_moved.saturating_add(bytes);
                    }
                    Ok(None) => {}
                    Err(error) => report.failed.push(format!("{hash:?}: {error}")),
                }
            }
        }
        while report.examined < max_items {
            let Some((source, target)) = self.rebalance_pair()? else {
                break;
            };
            let hashes = self.member_hashes(source, max_items - report.examined)?;
            if hashes.is_empty() {
                break;
            }
            let mut progressed = false;
            for hash in hashes {
                if report.examined >= max_items {
                    break;
                }
                report.examined += 1;
                let Some(location) = self.read_location(&hash)? else {
                    continue;
                };
                if !self.move_improves_balance(source, target, location.size())? {
                    continue;
                }
                match self.move_blob(source, target, hash) {
                    Ok(Some(bytes)) => {
                        report.moved += 1;
                        report.bytes_moved = report.bytes_moved.saturating_add(bytes);
                        progressed = true;
                    }
                    Ok(None) => {}
                    Err(error) => report.failed.push(format!("{hash:?}: {error}")),
                }
            }
            if !progressed {
                break;
            }
        }
        Ok(report)
    }

    fn move_from_draining(
        &self,
        source: PoolMemberId,
        hash: Hash,
    ) -> Result<Option<u64>, StoreError> {
        let Some(location) = self.read_location(&hash)? else {
            return Ok(None);
        };
        let (target, size, moving) = match location {
            LocationRecord::Pending { member, size } | LocationRecord::Stored { member, size }
                if member == source =>
            {
                let target = self.choose_write_member(size, Some(source))?;
                return self.move_blob(source, target, hash);
            }
            LocationRecord::Moving {
                source: moving_source,
                target,
                size,
            } if moving_source == source => (target, size, true),
            _ => return Ok(None),
        };

        self.move_blob_inner(source, target, hash, location, size, moving)
    }

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
            Err(_) if moving => {
                target_store.verify_blob_streaming(
                    &hash,
                    size,
                    self.temperature_config.copy_chunk_bytes,
                )?;
                self.complete_move(hash, source, target, size)?;
                let _ = self.delete_member_blob(source, &source_store, &hash);
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
            let _ = self.delete_member_blob(source, &source_store, &hash);
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
        let _ = self.delete_member_blob(source, &source_store, &hash);
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

    fn rebalance_pair(&self) -> Result<Option<(PoolMemberId, PoolMemberId)>, StoreError> {
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

    fn move_improves_balance(
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
