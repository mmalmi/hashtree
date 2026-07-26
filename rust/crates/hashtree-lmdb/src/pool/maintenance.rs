use super::maintenance_batch::{
    MoveBatchResult, MovePlan, DEFAULT_MAINTENANCE_BATCH_ITEMS, MAX_MAINTENANCE_BATCH_BYTES,
};
use super::{LocationRecord, PoolMaintenanceReport, PoolMemberId, PoolMemberState, PoolStore};
use hashtree_core::store::StoreError;
use std::collections::{BTreeMap, HashMap};

impl PoolStore {
    pub fn maintain(&self, max_items: usize) -> Result<PoolMaintenanceReport, StoreError> {
        self.maintain_with_batch_items(max_items, DEFAULT_MAINTENANCE_BATCH_ITEMS)
    }

    pub fn maintain_with_batch_items(
        &self,
        max_items: usize,
        batch_items: usize,
    ) -> Result<PoolMaintenanceReport, StoreError> {
        let mut report = PoolMaintenanceReport::default();
        if max_items == 0 {
            return Ok(report);
        }
        if batch_items == 0 {
            return Err(StoreError::Other(
                "pool maintenance batch items must be non-zero".into(),
            ));
        }

        let cleanups = self
            .active_move_cleanups(max_items)?
            .into_iter()
            .filter_map(|(hash, location)| {
                let LocationRecord::Moving {
                    source,
                    target,
                    size,
                } = location
                else {
                    return None;
                };
                Some(MovePlan {
                    hash,
                    source,
                    target,
                    size,
                    expected: location,
                })
            })
            .collect::<Vec<_>>();
        report.examined = report.examined.saturating_add(cleanups.len());
        self.execute_plans_bounded(cleanups, batch_items, true, &mut report)?;
        if report.examined >= max_items || !report.failed.is_empty() {
            return Ok(report);
        }

        let active = self
            .active_moves(max_items.saturating_sub(report.examined))?
            .into_iter()
            .filter_map(|(hash, location)| {
                let LocationRecord::Moving {
                    source,
                    target,
                    size,
                } = location
                else {
                    return None;
                };
                Some(MovePlan {
                    hash,
                    source,
                    target,
                    size,
                    expected: location,
                })
            })
            .collect::<Vec<_>>();
        report.examined = report.examined.saturating_add(active.len());
        self.execute_plans_bounded(active, batch_items, false, &mut report)?;
        if report.examined >= max_items || !report.failed.is_empty() {
            return Ok(report);
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
            let mut reserved_bytes = HashMap::new();
            let mut plans = Vec::with_capacity(hashes.len());
            for hash in hashes {
                if report.examined >= max_items {
                    break;
                }
                report.examined += 1;
                let Some(location) = self.read_location(&hash)? else {
                    continue;
                };
                let (target, size) = match location {
                    LocationRecord::Pending { member, size }
                    | LocationRecord::Stored { member, size }
                        if member == source =>
                    {
                        match self.choose_write_member_with_reserved(
                            size,
                            Some(source),
                            &reserved_bytes,
                        ) {
                            Ok(target) => {
                                let reserved = reserved_bytes.entry(target).or_insert(0u64);
                                *reserved = reserved.saturating_add(size);
                                (target, size)
                            }
                            Err(error) => {
                                report.failed.push(format!("{hash:?}: {error}"));
                                continue;
                            }
                        }
                    }
                    LocationRecord::Moving {
                        source: actual_source,
                        target,
                        size,
                    } if actual_source == source => (target, size),
                    _ => continue,
                };
                plans.push(MovePlan {
                    hash,
                    source,
                    target,
                    size,
                    expected: location,
                });
            }
            self.execute_plans_bounded(plans, batch_items, false, &mut report)?;
            if report.examined >= max_items || !report.failed.is_empty() {
                return Ok(report);
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

    fn execute_plans_bounded(
        &self,
        plans: Vec<MovePlan>,
        batch_items: usize,
        cleanup_only: bool,
        report: &mut PoolMaintenanceReport,
    ) -> Result<(), StoreError> {
        let mut groups = BTreeMap::<(PoolMemberId, PoolMemberId), Vec<MovePlan>>::new();
        for plan in plans {
            groups
                .entry((plan.source, plan.target))
                .or_default()
                .push(plan);
        }
        for (_, plans) in groups {
            let mut batch = Vec::new();
            let mut batch_bytes = 0u64;
            for plan in plans {
                let exceeds_items = batch.len() >= batch_items;
                let exceeds_bytes = !batch.is_empty()
                    && batch_bytes.saturating_add(plan.size) > MAX_MAINTENANCE_BATCH_BYTES;
                if exceeds_items || exceeds_bytes {
                    self.execute_one_bounded_batch(&batch, cleanup_only, report)?;
                    batch.clear();
                    batch_bytes = 0;
                }
                if !cleanup_only && plan.size > MAX_MAINTENANCE_BATCH_BYTES {
                    match self.move_blob(plan.source, plan.target, plan.hash) {
                        Ok(Some(bytes)) => {
                            report.moved += 1;
                            report.bytes_moved = report.bytes_moved.saturating_add(bytes);
                        }
                        Ok(None) => {}
                        Err(error) => report.failed.push(format!("{:?}: {error}", plan.hash)),
                    }
                    continue;
                }
                batch_bytes = batch_bytes.saturating_add(plan.size);
                batch.push(plan);
            }
            self.execute_one_bounded_batch(&batch, cleanup_only, report)?;
        }
        Ok(())
    }

    fn execute_one_bounded_batch(
        &self,
        plans: &[MovePlan],
        cleanup_only: bool,
        report: &mut PoolMaintenanceReport,
    ) -> Result<(), StoreError> {
        if plans.is_empty() {
            return Ok(());
        }
        let result = if cleanup_only {
            self.execute_move_cleanup_batch(plans)?
        } else {
            self.execute_move_batch(plans)?
        };
        apply_batch_result(report, result);
        Ok(())
    }
}

fn apply_batch_result(report: &mut PoolMaintenanceReport, result: MoveBatchResult) {
    for plan in result.moved {
        report.moved = report.moved.saturating_add(1);
        report.bytes_moved = report.bytes_moved.saturating_add(plan.size);
    }
    report.failed.extend(
        result
            .failed
            .into_iter()
            .map(|(hash, error)| format!("{hash:?}: {error}")),
    );
}
