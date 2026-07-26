use super::{LocationRecord, PoolMemberId, PoolStore};
use hashtree_core::store::StoreError;
use hashtree_core::types::Hash;
use std::collections::HashSet;
use std::time::Instant;

pub(super) const DEFAULT_MAINTENANCE_BATCH_ITEMS: usize = 256;
pub(super) const MAX_MAINTENANCE_BATCH_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct MovePlan {
    pub(super) hash: Hash,
    pub(super) source: PoolMemberId,
    pub(super) target: PoolMemberId,
    pub(super) size: u64,
    pub(super) expected: LocationRecord,
}

impl MovePlan {
    pub(super) fn moving(self) -> LocationRecord {
        LocationRecord::Moving {
            source: self.source,
            target: self.target,
            size: self.size,
        }
    }

    pub(super) fn catalog_tuple(self) -> (Hash, PoolMemberId, PoolMemberId, u64) {
        (self.hash, self.source, self.target, self.size)
    }
}

#[derive(Default)]
pub(super) struct MoveBatchResult {
    pub(super) moved: Vec<MovePlan>,
    pub(super) failed: Vec<(Hash, String)>,
}

impl PoolStore {
    pub(super) fn execute_move_batch(
        &self,
        plans: &[MovePlan],
    ) -> Result<MoveBatchResult, StoreError> {
        if plans.is_empty() {
            return Ok(MoveBatchResult::default());
        }
        let source = plans[0].source;
        let target = plans[0].target;
        if plans
            .iter()
            .any(|plan| plan.source != source || plan.target != target)
        {
            return Err(StoreError::Other(
                "pool move batch spans multiple source or target members".into(),
            ));
        }

        let transitions = plans
            .iter()
            .map(|plan| (plan.hash, plan.expected, plan.moving()))
            .collect::<Vec<_>>();
        let started = self
            .begin_move_records(&transitions)?
            .into_iter()
            .collect::<HashSet<_>>();
        let plans = plans
            .iter()
            .copied()
            .filter(|plan| started.contains(&plan.hash))
            .collect::<Vec<_>>();
        if plans.is_empty() {
            return Ok(MoveBatchResult::default());
        }

        let target_store = self.get_member(target)?;
        let target_gate = self.member_gate(target, true)?;
        let _target_permit = target_gate.acquire()?;
        let source_store = self.get_member(source).ok();
        let source_gate = source_store
            .as_ref()
            .map(|_| self.member_gate(source, false))
            .transpose()?;
        let _source_permit = source_gate
            .as_ref()
            .map(|gate| gate.acquire())
            .transpose()?;

        let mut result = MoveBatchResult::default();
        let mut verified = Vec::with_capacity(plans.len());
        let mut writes = Vec::<(MovePlan, Vec<u8>)>::new();
        for plan in plans {
            if target_store
                .verify_blob_streaming(
                    &plan.hash,
                    plan.size,
                    self.temperature_config.copy_chunk_bytes,
                )
                .is_ok()
            {
                verified.push(plan);
                continue;
            }
            let Some(source_store) = source_store.as_ref() else {
                result.failed.push((
                    plan.hash,
                    format!(
                        "pool source {} is unavailable and target {} is not verified",
                        plan.source, plan.target
                    ),
                ));
                continue;
            };
            match source_store.blob_size_sync(&plan.hash) {
                Ok(Some(size)) if size == plan.size => {}
                Ok(Some(size)) => {
                    result.failed.push((
                        plan.hash,
                        format!(
                            "pool source {source} size mismatch: expected {}, found {size}",
                            plan.size
                        ),
                    ));
                    continue;
                }
                Ok(None) => {
                    result.failed.push((
                        plan.hash,
                        format!("pool source {source} does not contain the blob"),
                    ));
                    continue;
                }
                Err(error) => {
                    result.failed.push((plan.hash, error.to_string()));
                    continue;
                }
            }
            let read_started = Instant::now();
            match source_store.get_sync(&plan.hash) {
                Ok(Some(data))
                    if data.len() as u64 == plan.size
                        && hashtree_core::sha256(&data) == plan.hash =>
                {
                    self.record_read(source, read_started.elapsed(), true);
                    writes.push((plan, data));
                }
                Ok(Some(data)) => {
                    self.record_read(source, read_started.elapsed(), false);
                    result.failed.push((
                        plan.hash,
                        format!(
                            "pool source {source} returned corrupt bytes: expected {} bytes, found {}",
                            plan.size,
                            data.len()
                        ),
                    ));
                }
                Ok(None) => {
                    self.record_read(source, read_started.elapsed(), true);
                    result.failed.push((
                        plan.hash,
                        format!("pool source {source} does not contain the blob"),
                    ));
                }
                Err(error) => {
                    self.record_read(source, read_started.elapsed(), false);
                    result.failed.push((plan.hash, error.to_string()));
                }
            }
        }

        if !writes.is_empty() {
            let refs = writes
                .iter()
                .map(|(plan, data)| (plan.hash, data.as_slice()))
                .collect::<Vec<_>>();
            let bytes = refs.iter().map(|(_, data)| data.len()).sum::<usize>();
            let write_started = Instant::now();
            match target_store.put_many_refs_report_sync(&refs) {
                Ok(_) => {
                    self.record_write(target, write_started.elapsed(), bytes, true);
                    for (plan, _) in writes {
                        match target_store.verify_blob_streaming(
                            &plan.hash,
                            plan.size,
                            self.temperature_config.copy_chunk_bytes,
                        ) {
                            Ok(()) => verified.push(plan),
                            Err(error) => {
                                result.failed.push((plan.hash, error.to_string()));
                            }
                        }
                    }
                }
                Err(error) => {
                    self.record_write(target, write_started.elapsed(), bytes, false);
                    let message = error.to_string();
                    result.failed.extend(
                        writes
                            .into_iter()
                            .map(|(plan, _)| (plan.hash, message.clone())),
                    );
                }
            }
        }

        if verified.is_empty() {
            return Ok(result);
        }
        let catalog = verified
            .iter()
            .copied()
            .map(MovePlan::catalog_tuple)
            .collect::<Vec<_>>();
        if let Err(error) = self.finish_move_records(&catalog) {
            let message = error.to_string();
            result.failed.extend(
                verified
                    .into_iter()
                    .map(|plan| (plan.hash, message.clone())),
            );
            return Ok(result);
        }
        result.moved.extend(verified.iter().copied());
        drop(_source_permit);
        drop(_target_permit);

        let Some(source_store) = source_store else {
            result.failed.extend(verified.into_iter().map(|plan| {
                (
                    plan.hash,
                    format!(
                        "pool source {} is unavailable; cleanup remains pending",
                        plan.source
                    ),
                )
            }));
            return Ok(result);
        };
        let hashes = verified.iter().map(|plan| plan.hash).collect::<Vec<_>>();
        let source_write_gate = self.member_gate(source, true)?;
        let _source_write_permit = source_write_gate.acquire()?;
        if let Err(error) = source_store.delete_many_sync(&hashes) {
            let message = error.to_string();
            result.failed.extend(
                verified
                    .into_iter()
                    .map(|plan| (plan.hash, message.clone())),
            );
            return Ok(result);
        }
        if let Err(error) = self.clear_move_cleanup_records(&catalog) {
            let message = error.to_string();
            result.failed.extend(
                verified
                    .into_iter()
                    .map(|plan| (plan.hash, message.clone())),
            );
        }
        Ok(result)
    }

    pub(super) fn execute_move_cleanup_batch(
        &self,
        plans: &[MovePlan],
    ) -> Result<MoveBatchResult, StoreError> {
        if plans.is_empty() {
            return Ok(MoveBatchResult::default());
        }
        let source = plans[0].source;
        let target = plans[0].target;
        if plans
            .iter()
            .any(|plan| plan.source != source || plan.target != target)
        {
            return Err(StoreError::Other(
                "pool cleanup batch spans multiple source or target members".into(),
            ));
        }
        let target_store = self.get_member(target)?;
        let target_gate = self.member_gate(target, false)?;
        let _target_permit = target_gate.acquire()?;
        let mut ready = Vec::with_capacity(plans.len());
        let mut result = MoveBatchResult::default();
        for plan in plans {
            match self.read_location(&plan.hash)? {
                Some(LocationRecord::Stored { member, size })
                    if member == target
                        && size == plan.size
                        && target_store
                            .verify_blob_streaming(
                                &plan.hash,
                                plan.size,
                                self.temperature_config.copy_chunk_bytes,
                            )
                            .is_ok() =>
                {
                    ready.push(*plan);
                }
                location => result.failed.push((
                    plan.hash,
                    format!(
                        "pool target {target} is not verified while cleanup is pending: {location:?}"
                    ),
                )),
            }
        }
        if ready.is_empty() {
            return Ok(result);
        }
        drop(_target_permit);
        let source_store = match self.get_member(source) {
            Ok(store) => store,
            Err(error) => {
                let message = error.to_string();
                result
                    .failed
                    .extend(ready.into_iter().map(|plan| (plan.hash, message.clone())));
                return Ok(result);
            }
        };
        let hashes = ready.iter().map(|plan| plan.hash).collect::<Vec<_>>();
        let source_gate = self.member_gate(source, true)?;
        let _source_permit = source_gate.acquire()?;
        if let Err(error) = source_store.delete_many_sync(&hashes) {
            let message = error.to_string();
            result
                .failed
                .extend(ready.into_iter().map(|plan| (plan.hash, message.clone())));
            return Ok(result);
        }
        let catalog = ready
            .iter()
            .copied()
            .map(MovePlan::catalog_tuple)
            .collect::<Vec<_>>();
        if let Err(error) = self.clear_move_cleanup_records(&catalog) {
            let message = error.to_string();
            result
                .failed
                .extend(ready.into_iter().map(|plan| (plan.hash, message.clone())));
        } else {
            result.moved.extend(ready);
        }
        Ok(result)
    }
}
