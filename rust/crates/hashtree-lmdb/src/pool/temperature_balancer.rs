use super::temperature::{TemperatureCandidate, HEAT_UNIT};
use super::{
    LocationRecord, PoolMemberId, PoolMemberState, PoolMemberStatus, PoolStore,
    PoolTemperatureReport,
};
use hashtree_core::store::StoreError;
use std::collections::HashSet;

impl PoolStore {
    pub fn balance_temperature(&self) -> Result<PoolTemperatureReport, StoreError> {
        self.balance_temperature_at(super::unix_timestamp_now())
    }

    pub(super) fn balance_temperature_at(
        &self,
        now: u64,
    ) -> Result<PoolTemperatureReport, StoreError> {
        if !self.temperature_config.enabled {
            return Ok(PoolTemperatureReport::default());
        }
        let Ok(_cycle) = self.temperature_cycle.try_lock() else {
            return Ok(PoolTemperatureReport::default());
        };
        let sampled = self.flush_sampled_accesses(now)?;
        let mut report = PoolTemperatureReport {
            sampled_accesses_flushed: sampled.len(),
            ..PoolTemperatureReport::default()
        };
        if !self.try_acquire_temperature_lease(now)? {
            return Ok(report);
        }
        report.lease_acquired = true;
        self.balance_temperature_owned(now, sampled, report)
    }

    fn balance_temperature_owned(
        &self,
        now: u64,
        sampled: Vec<TemperatureCandidate>,
        mut report: PoolTemperatureReport,
    ) -> Result<PoolTemperatureReport, StoreError> {
        let scanned = self.scan_temperature_candidates(now)?;
        report.scanned = scanned.len();
        {
            let mut temperature = self
                .temperature
                .lock()
                .map_err(|_| StoreError::Other("pool temperature lock poisoned".into()))?;
            for candidate in sampled.into_iter().chain(scanned) {
                if candidate.heat >= HEAT_UNIT {
                    temperature.hot.upsert(candidate);
                } else {
                    temperature.cold.upsert(candidate);
                }
            }
            report.candidates = temperature.hot.len().saturating_add(temperature.cold.len());
        }

        if self.temperature_foreground_busy()? {
            report.throttled = true;
            return Ok(report);
        }

        let mut members = self.temperature_members()?;
        let pressured = members
            .iter()
            .filter(|member| {
                member.state == PoolMemberState::Active
                    && member.available
                    && fill_percent(member.logical_bytes, member.capacity_bytes)
                        >= member.temperature_high_watermark_percent
            })
            .map(|member| member.id)
            .collect::<HashSet<_>>();
        let mut remaining_bytes = self.temperature_config.max_bytes_per_cycle;
        let mut attempted_moves = 0usize;
        for (hash, location) in self.active_moves(
            self.temperature_config
                .max_moves_per_cycle
                .saturating_sub(report.moved),
        )? {
            if attempted_moves >= self.temperature_config.max_moves_per_cycle
                || remaining_bytes < location.size()
            {
                break;
            }
            let LocationRecord::Moving { source, target, .. } = location else {
                continue;
            };
            attempted_moves += 1;
            report.attempted_moves += 1;
            report.peak_concurrent_moves = report.peak_concurrent_moves.max(1);
            remaining_bytes = remaining_bytes.saturating_sub(location.size());
            match self.move_blob(source, target, hash) {
                Ok(Some(bytes)) => {
                    report.resumed += 1;
                    report.moved += 1;
                    report.bytes_moved = report.bytes_moved.saturating_add(bytes);
                    update_member_bytes(&mut members, source, target, bytes);
                }
                Ok(None) => {}
                Err(error) => report.failed.push(format!("{hash:?}: {error}")),
            }
        }
        while attempted_moves < self.temperature_config.max_moves_per_cycle && remaining_bytes > 0 {
            if self.temperature_foreground_busy()? {
                report.throttled = true;
                break;
            }
            let batch_limit = self
                .temperature_config
                .max_concurrent_moves
                .max(1)
                .min(self.temperature_config.max_moves_per_cycle - attempted_moves);
            let mut plans = Vec::with_capacity(batch_limit);
            while plans.len() < batch_limit && remaining_bytes > 0 {
                let candidate = {
                    let mut temperature = self
                        .temperature
                        .lock()
                        .map_err(|_| StoreError::Other("pool temperature lock poisoned".into()))?;
                    let mut planned = None;
                    let hot_len = temperature.hot.len();
                    for _ in 0..hot_len {
                        let Some(candidate) = temperature
                            .hot
                            .remove_best_by(|candidate, current| candidate.heat > current.heat)
                        else {
                            break;
                        };
                        if let Some(target) = self.plan_hot_move(candidate, &members, now) {
                            planned = Some((candidate, target));
                            break;
                        }
                    }
                    if planned.is_none() {
                        let cold_len = temperature.cold.len();
                        for _ in 0..cold_len {
                            let Some(candidate) =
                                temperature.cold.remove_best_by(|candidate, current| {
                                    candidate.heat < current.heat
                                        || (candidate.heat == current.heat
                                            && candidate.last_accessed_at
                                                < current.last_accessed_at)
                                })
                            else {
                                break;
                            };
                            if let Some(target) =
                                self.plan_cold_move(candidate, &members, &pressured, now)
                            {
                                planned = Some((candidate, target));
                                break;
                            }
                        }
                    }
                    planned
                };
                let Some((candidate, target)) = candidate else {
                    break;
                };
                if candidate.size > remaining_bytes {
                    continue;
                }
                remaining_bytes = remaining_bytes.saturating_sub(candidate.size);
                update_member_bytes(&mut members, candidate.member, target, candidate.size);
                plans.push((candidate, target));
            }
            if plans.is_empty() {
                break;
            }
            attempted_moves = attempted_moves.saturating_add(plans.len());
            report.attempted_moves = report.attempted_moves.saturating_add(plans.len());
            report.peak_concurrent_moves = report.peak_concurrent_moves.max(plans.len());
            let outcomes = std::thread::scope(|scope| {
                let workers = plans
                    .into_iter()
                    .map(|(candidate, target)| {
                        scope.spawn(move || {
                            (
                                candidate,
                                target,
                                self.move_blob(candidate.member, target, candidate.hash),
                            )
                        })
                    })
                    .collect::<Vec<_>>();
                workers
                    .into_iter()
                    .map(|worker| worker.join())
                    .collect::<Vec<_>>()
            });
            for outcome in outcomes {
                match outcome {
                    Ok((_, _, Ok(Some(bytes)))) => {
                        report.moved += 1;
                        report.bytes_moved = report.bytes_moved.saturating_add(bytes);
                    }
                    Ok((_, _, Ok(None))) => {}
                    Ok((candidate, _, Err(error))) => {
                        report.failed.push(format!("{:?}: {error}", candidate.hash));
                    }
                    Err(_) => report
                        .failed
                        .push("temperature move worker panicked".into()),
                }
            }
            members = self.temperature_members()?;
        }
        Ok(report)
    }

    fn temperature_members(&self) -> Result<Vec<PoolMemberStatus>, StoreError> {
        self.refresh_members()?;
        let manifest = self.read_manifest()?;
        let runtime = self
            .runtime
            .read()
            .map_err(|_| StoreError::Other("pool runtime lock poisoned".into()))?;
        manifest
            .members
            .into_iter()
            .map(|member| {
                let (logical_bytes, available, last_error) = match runtime.stores.get(&member.id) {
                    Some(store) => match store.stats() {
                        Ok(stats) => (stats.total_bytes, true, None),
                        Err(error) => (0, false, Some(error.to_string())),
                    },
                    None => (0, false, runtime.errors.get(&member.id).cloned()),
                };
                Ok(PoolMemberStatus {
                    id: member.id,
                    state: member.state,
                    path: member.config.path,
                    capacity_bytes: member.config.capacity_bytes,
                    map_size_bytes: member.config.map_size_bytes,
                    external_blob_dir: member.config.external_blob_dir,
                    external_blob_min_bytes: member.config.external_blob_min_bytes,
                    external_blob_sync: member.config.external_blob_sync,
                    external_pack_target_bytes: member.config.external_pack_target_bytes,
                    max_read_concurrency: member.config.max_read_concurrency,
                    max_write_concurrency: member.config.max_write_concurrency,
                    temperature_low_watermark_percent: member
                        .config
                        .temperature_low_watermark_percent,
                    temperature_high_watermark_percent: member
                        .config
                        .temperature_high_watermark_percent,
                    logical_bytes,
                    located_blobs: 0,
                    available,
                    last_error,
                })
            })
            .collect()
    }

    fn plan_hot_move(
        &self,
        candidate: TemperatureCandidate,
        members: &[PoolMemberStatus],
        now: u64,
    ) -> Option<PoolMemberId> {
        if now.saturating_sub(candidate.placed_at)
            < self.temperature_config.minimum_residence.as_secs()
        {
            return None;
        }
        let mut targets = members
            .iter()
            .filter(|member| {
                member.id != candidate.member
                    && member.state == PoolMemberState::Active
                    && member.available
                    && below_high_watermark(member, candidate.size)
            })
            .map(|member| member.id)
            .collect::<Vec<_>>();
        let adaptive = self.adaptive.lock().ok()?;
        adaptive.order_temperature_targets(&mut targets);
        targets.into_iter().find(|target| {
            adaptive.meaningfully_faster(
                *target,
                candidate.member,
                self.temperature_config.promotion_hysteresis_percent,
            )
        })
    }

    fn plan_cold_move(
        &self,
        candidate: TemperatureCandidate,
        members: &[PoolMemberStatus],
        pressured: &HashSet<PoolMemberId>,
        now: u64,
    ) -> Option<PoolMemberId> {
        if now.saturating_sub(candidate.placed_at)
            < self.temperature_config.minimum_residence.as_secs()
        {
            return None;
        }
        let source = members.iter().find(|member| {
            member.id == candidate.member
                && member.state == PoolMemberState::Active
                && member.available
        })?;
        if !pressured.contains(&source.id)
            || fill_percent(source.logical_bytes, source.capacity_bytes)
                <= source.temperature_low_watermark_percent
        {
            return None;
        }

        members
            .iter()
            .filter(|target| {
                target.id != source.id
                    && target.state == PoolMemberState::Active
                    && target.available
                    && target.logical_bytes.saturating_add(candidate.size) <= target.capacity_bytes
            })
            .max_by_key(|target| {
                (
                    target.capacity_bytes > source.capacity_bytes,
                    target.capacity_bytes.saturating_sub(target.logical_bytes),
                    target.capacity_bytes,
                )
            })
            .map(|target| target.id)
    }

    fn temperature_foreground_busy(&self) -> Result<bool, StoreError> {
        self.refresh_members()?;
        let runtime = self
            .runtime
            .read()
            .map_err(|_| StoreError::Other("pool runtime lock poisoned".into()))?;
        let threshold = self.temperature_config.foreground_load_percent.max(1);
        for gate in runtime
            .read_gates
            .values()
            .chain(runtime.write_gates.values())
        {
            if gate.load_percent()? >= threshold {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

fn fill_percent(bytes: u64, capacity: u64) -> u8 {
    if capacity == 0 {
        return 100;
    }
    bytes.saturating_mul(100).saturating_div(capacity).min(100) as u8
}

fn below_high_watermark(member: &PoolMemberStatus, incoming: u64) -> bool {
    fill_percent(
        member.logical_bytes.saturating_add(incoming),
        member.capacity_bytes,
    ) <= member.temperature_high_watermark_percent
}

fn update_member_bytes(
    members: &mut [PoolMemberStatus],
    source: PoolMemberId,
    target: PoolMemberId,
    bytes: u64,
) {
    for member in members {
        if member.id == source {
            member.logical_bytes = member.logical_bytes.saturating_sub(bytes);
        } else if member.id == target {
            member.logical_bytes = member.logical_bytes.saturating_add(bytes);
        }
    }
}
