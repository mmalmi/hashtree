use hashtree_core::types::Hash;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const MIN_RETRY_DELAY: Duration = Duration::from_millis(250);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CacheQuotaDenialKind {
    CleanupInProgress,
    RetryLater,
    NoRoom,
}

#[derive(Debug, Clone)]
pub(super) struct CacheQuotaDenial {
    pub kind: CacheQuotaDenialKind,
    pub epoch: u64,
    pub retry_after: Duration,
}

impl fmt::Display for CacheQuotaDenial {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match self.kind {
            CacheQuotaDenialKind::CleanupInProgress => "quota cleanup is in progress",
            CacheQuotaDenialKind::RetryLater => "quota cleanup is temporarily unavailable",
            CacheQuotaDenialKind::NoRoom => "no disposable cache space is available",
        };
        write!(
            formatter,
            "cached blob {reason} (cleanup epoch {}); retry after {}ms",
            self.epoch,
            self.retry_after.as_millis().max(1)
        )
    }
}

#[derive(Debug)]
struct CacheQuotaState {
    running_epoch: Option<u64>,
    next_epoch: u64,
    reserved_bytes: u64,
    usage_floor: u64,
    inflight_hashes: HashMap<Hash, u32>,
    deleting_hashes: HashSet<Hash>,
    retry_not_before: Option<Instant>,
    denial_kind: CacheQuotaDenialKind,
    consecutive_failures: u32,
}

impl Default for CacheQuotaState {
    fn default() -> Self {
        Self {
            running_epoch: None,
            next_epoch: 1,
            reserved_bytes: 0,
            usage_floor: 0,
            inflight_hashes: HashMap::new(),
            deleting_hashes: HashSet::new(),
            retry_not_before: None,
            denial_kind: CacheQuotaDenialKind::RetryLater,
            consecutive_failures: 0,
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct CacheQuotaController {
    state: Mutex<CacheQuotaState>,
}

pub(super) enum CacheQuotaAdmission<'a> {
    Admitted(CacheWritePermit<'a>),
    Cleanup(CacheCleanupLease<'a>),
}

impl CacheQuotaController {
    /// Protect hashes that are about to become durable from orphan retention.
    ///
    /// Protection and per-hash deletion claims share the same mutex. A durable
    /// write and retention may proceed concurrently for unrelated hashes, but
    /// they cannot both acquire the same hash.
    pub(super) fn protect_retention_hashes(
        &self,
        mut hashes: Vec<Hash>,
    ) -> Result<RetentionWriteGuard<'_>, CacheQuotaDenial> {
        hashes.sort_unstable();
        hashes.dedup();

        let mut state = self.lock_state();
        if hashes
            .iter()
            .any(|hash| state.deleting_hashes.contains(hash))
        {
            return Err(CacheQuotaDenial {
                kind: CacheQuotaDenialKind::CleanupInProgress,
                epoch: state
                    .running_epoch
                    .unwrap_or_else(|| state.next_epoch.saturating_sub(1)),
                retry_after: MIN_RETRY_DELAY,
            });
        }
        Self::reserve(&mut state, 0, &hashes);
        drop(state);

        Ok(RetentionWriteGuard {
            controller: self,
            hashes,
        })
    }

    /// Claim a hash for retention deletion.
    ///
    /// The returned guard must remain alive through the actual store deletion.
    /// A concurrent durable writer either registered first (and this returns
    /// `None`) or observes the delete claim and fails before writing its body.
    pub(super) fn begin_retention_delete(&self, hash: Hash) -> Option<RetentionDeleteGuard<'_>> {
        let mut state = self.lock_state();
        if state.inflight_hashes.contains_key(&hash) || !state.deleting_hashes.insert(hash) {
            return None;
        }
        drop(state);
        Some(RetentionDeleteGuard {
            controller: self,
            hash,
        })
    }

    pub(super) fn quick_denial(&self) -> Option<CacheQuotaDenial> {
        let mut state = self.lock_state();
        if let Some(epoch) = state.running_epoch {
            return Some(CacheQuotaDenial {
                kind: CacheQuotaDenialKind::CleanupInProgress,
                epoch,
                retry_after: MIN_RETRY_DELAY,
            });
        }
        let retry_not_before = state.retry_not_before?;
        let now = Instant::now();
        if now >= retry_not_before {
            state.retry_not_before = None;
            return None;
        }
        Some(CacheQuotaDenial {
            kind: state.denial_kind,
            epoch: state.next_epoch.saturating_sub(1),
            retry_after: retry_not_before.saturating_duration_since(now),
        })
    }

    pub(super) fn begin_admission(
        &self,
        observed_usage: u64,
        incoming_bytes: u64,
        hashes: Vec<Hash>,
        max_size_bytes: u64,
        force_cleanup: bool,
    ) -> Result<CacheQuotaAdmission<'_>, CacheQuotaDenial> {
        let mut state = self.lock_state();
        if let Some(epoch) = state.running_epoch {
            return Err(CacheQuotaDenial {
                kind: CacheQuotaDenialKind::CleanupInProgress,
                epoch,
                retry_after: MIN_RETRY_DELAY,
            });
        }
        if let Some(retry_not_before) = state.retry_not_before {
            let now = Instant::now();
            if now < retry_not_before {
                return Err(CacheQuotaDenial {
                    kind: state.denial_kind,
                    epoch: state.next_epoch.saturating_sub(1),
                    retry_after: retry_not_before.saturating_duration_since(now),
                });
            }
            state.retry_not_before = None;
        }

        state.usage_floor = state.usage_floor.max(observed_usage);
        let projected = state
            .usage_floor
            .saturating_add(state.reserved_bytes)
            .saturating_add(incoming_bytes);
        if max_size_bytes == 0 || (!force_cleanup && projected <= max_size_bytes) {
            Self::reserve(&mut state, incoming_bytes, &hashes);
            return Ok(CacheQuotaAdmission::Admitted(CacheWritePermit {
                controller: self,
                incoming_bytes,
                hashes,
                finished: false,
            }));
        }

        let epoch = state.next_epoch;
        state.next_epoch = state.next_epoch.saturating_add(1);
        state.running_epoch = Some(epoch);
        let existing_reservations = state.reserved_bytes;
        let inflight_hashes = state.inflight_hashes.keys().copied().collect();
        let target_bytes = if force_cleanup {
            let headroom = incoming_bytes.max(observed_usage / 10).max(1);
            observed_usage.saturating_sub(headroom)
        } else if incoming_bytes.saturating_add(existing_reservations) >= max_size_bytes {
            0
        } else {
            (max_size_bytes.saturating_mul(9) / 10).min(
                max_size_bytes
                    .saturating_sub(incoming_bytes)
                    .saturating_sub(existing_reservations),
            )
        };
        drop(state);

        Ok(CacheQuotaAdmission::Cleanup(CacheCleanupLease {
            controller: self,
            epoch,
            started_at: Instant::now(),
            incoming_bytes,
            hashes,
            max_size_bytes,
            require_progress: force_cleanup,
            target_bytes,
            inflight_hashes,
            finished: false,
        }))
    }

    pub(super) fn begin_standalone_cleanup(
        &self,
    ) -> Result<StandaloneCleanupLease<'_>, CacheQuotaDenial> {
        let mut state = self.lock_state();
        if let Some(epoch) = state.running_epoch {
            return Err(CacheQuotaDenial {
                kind: CacheQuotaDenialKind::CleanupInProgress,
                epoch,
                retry_after: MIN_RETRY_DELAY,
            });
        }
        let epoch = state.next_epoch;
        state.next_epoch = state.next_epoch.saturating_add(1);
        state.running_epoch = Some(epoch);
        let inflight_hashes = state.inflight_hashes.keys().copied().collect();
        drop(state);
        Ok(StandaloneCleanupLease {
            controller: self,
            epoch,
            inflight_hashes,
            finished: false,
        })
    }

    pub(super) fn cleanup_epoch_count(&self) -> u64 {
        self.lock_state().next_epoch.saturating_sub(1)
    }

    fn reserve(state: &mut CacheQuotaState, bytes: u64, hashes: &[Hash]) {
        state.reserved_bytes = state.reserved_bytes.saturating_add(bytes);
        for hash in hashes {
            let count = state.inflight_hashes.entry(*hash).or_default();
            *count = count.saturating_add(1);
        }
    }

    fn release_reservation(state: &mut CacheQuotaState, bytes: u64, hashes: &[Hash]) {
        state.reserved_bytes = state.reserved_bytes.saturating_sub(bytes);
        for hash in hashes {
            let Some(count) = state.inflight_hashes.get_mut(hash) else {
                continue;
            };
            if *count <= 1 {
                state.inflight_hashes.remove(hash);
            } else {
                *count -= 1;
            }
        }
    }

    fn finish_write(&self, bytes: u64, hashes: &[Hash], inserted_bytes: u64) {
        let mut state = self.lock_state();
        Self::release_reservation(&mut state, bytes, hashes);
        state.usage_floor = state.usage_floor.saturating_add(inserted_bytes);
    }

    fn abort_write(&self, bytes: u64, hashes: &[Hash]) {
        let mut state = self.lock_state();
        Self::release_reservation(&mut state, bytes, hashes);
    }

    fn finish_retention_delete(&self, hash: Hash) {
        self.lock_state().deleting_hashes.remove(&hash);
    }

    fn finish_cleanup_and_reserve(
        &self,
        completion: CleanupCompletion,
    ) -> Result<CacheWritePermit<'_>, CacheQuotaDenial> {
        let CleanupCompletion {
            epoch,
            after_usage,
            freed_bytes,
            incoming_bytes,
            hashes,
            max_size_bytes,
            require_progress,
            sweep_complete,
        } = completion;
        let mut state = self.lock_state();
        if state.running_epoch != Some(epoch) {
            return Err(CacheQuotaDenial {
                kind: CacheQuotaDenialKind::RetryLater,
                epoch,
                retry_after: MIN_RETRY_DELAY,
            });
        }
        state.running_epoch = None;
        state.usage_floor = after_usage;
        let projected = after_usage
            .saturating_add(state.reserved_bytes)
            .saturating_add(incoming_bytes);
        let capacity_available = max_size_bytes == 0 || projected <= max_size_bytes;
        if capacity_available && (!require_progress || freed_bytes > 0) {
            state.retry_not_before = None;
            state.consecutive_failures = 0;
            Self::reserve(&mut state, incoming_bytes, &hashes);
            return Ok(CacheWritePermit {
                controller: self,
                incoming_bytes,
                hashes,
                finished: false,
            });
        }

        // A bounded page with no disposable entries is not evidence that the
        // store has no room. Suppress writes briefly so the next retry can
        // continue the cursor; only a complete sweep may enter the longer
        // NoRoom backoff.
        let kind = if sweep_complete {
            CacheQuotaDenialKind::NoRoom
        } else {
            CacheQuotaDenialKind::RetryLater
        };
        let retry_after = if kind == CacheQuotaDenialKind::NoRoom {
            MAX_RETRY_DELAY
        } else {
            MIN_RETRY_DELAY
        };
        state.denial_kind = kind;
        state.retry_not_before = Some(Instant::now() + retry_after);
        Err(CacheQuotaDenial {
            kind,
            epoch,
            retry_after,
        })
    }

    fn fail_cleanup(&self, epoch: u64, started_at: Instant) {
        let mut state = self.lock_state();
        if state.running_epoch != Some(epoch) {
            return;
        }
        state.running_epoch = None;
        let retry_after = retry_delay(started_at.elapsed(), state.consecutive_failures);
        state.retry_not_before = Some(Instant::now() + retry_after);
        state.denial_kind = CacheQuotaDenialKind::RetryLater;
        state.consecutive_failures = state.consecutive_failures.saturating_add(1);
    }

    fn finish_standalone_cleanup(&self, epoch: u64, after_usage: Option<u64>) {
        let mut state = self.lock_state();
        if state.running_epoch == Some(epoch) {
            state.running_epoch = None;
            if let Some(after_usage) = after_usage {
                state.usage_floor = after_usage;
            }
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, CacheQuotaState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

struct CleanupCompletion {
    epoch: u64,
    after_usage: u64,
    freed_bytes: u64,
    incoming_bytes: u64,
    hashes: Vec<Hash>,
    max_size_bytes: u64,
    require_progress: bool,
    sweep_complete: bool,
}

#[derive(Debug)]
pub(super) struct CacheWritePermit<'a> {
    controller: &'a CacheQuotaController,
    incoming_bytes: u64,
    hashes: Vec<Hash>,
    finished: bool,
}

impl CacheWritePermit<'_> {
    pub(super) fn commit(mut self, inserted_bytes: u64) {
        self.controller
            .finish_write(self.incoming_bytes, &self.hashes, inserted_bytes);
        self.finished = true;
    }
}

impl Drop for CacheWritePermit<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.controller
                .abort_write(self.incoming_bytes, &self.hashes);
        }
    }
}

#[derive(Debug)]
pub(super) struct RetentionWriteGuard<'a> {
    controller: &'a CacheQuotaController,
    hashes: Vec<Hash>,
}

impl Drop for RetentionWriteGuard<'_> {
    fn drop(&mut self) {
        self.controller.abort_write(0, &self.hashes);
    }
}

#[derive(Debug)]
pub(super) struct RetentionDeleteGuard<'a> {
    controller: &'a CacheQuotaController,
    hash: Hash,
}

impl Drop for RetentionDeleteGuard<'_> {
    fn drop(&mut self) {
        self.controller.finish_retention_delete(self.hash);
    }
}

pub(super) struct CacheCleanupLease<'a> {
    controller: &'a CacheQuotaController,
    epoch: u64,
    started_at: Instant,
    incoming_bytes: u64,
    hashes: Vec<Hash>,
    max_size_bytes: u64,
    require_progress: bool,
    target_bytes: u64,
    inflight_hashes: HashSet<Hash>,
    finished: bool,
}

impl<'a> CacheCleanupLease<'a> {
    pub(super) fn target_bytes(&self) -> u64 {
        self.target_bytes
    }

    pub(super) fn inflight_hashes(&self) -> &HashSet<Hash> {
        &self.inflight_hashes
    }

    pub(super) fn complete(
        mut self,
        after_usage: u64,
        freed_bytes: u64,
        sweep_complete: bool,
    ) -> Result<CacheWritePermit<'a>, CacheQuotaDenial> {
        let result = self
            .controller
            .finish_cleanup_and_reserve(CleanupCompletion {
                epoch: self.epoch,
                after_usage,
                freed_bytes,
                incoming_bytes: self.incoming_bytes,
                hashes: std::mem::take(&mut self.hashes),
                max_size_bytes: self.max_size_bytes,
                require_progress: self.require_progress,
                sweep_complete,
            });
        self.finished = true;
        result
    }
}

impl Drop for CacheCleanupLease<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.controller.fail_cleanup(self.epoch, self.started_at);
        }
    }
}

pub(super) struct StandaloneCleanupLease<'a> {
    controller: &'a CacheQuotaController,
    epoch: u64,
    inflight_hashes: HashSet<Hash>,
    finished: bool,
}

impl StandaloneCleanupLease<'_> {
    pub(super) fn inflight_hashes(&self) -> &HashSet<Hash> {
        &self.inflight_hashes
    }

    pub(super) fn complete(mut self, after_usage: u64) {
        self.controller
            .finish_standalone_cleanup(self.epoch, Some(after_usage));
        self.finished = true;
    }
}

impl Drop for StandaloneCleanupLease<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.controller.finish_standalone_cleanup(self.epoch, None);
        }
    }
}

fn retry_delay(elapsed: Duration, consecutive_failures: u32) -> Duration {
    let base = elapsed.clamp(MIN_RETRY_DELAY, MAX_RETRY_DELAY);
    let multiplier = 1u32 << consecutive_failures.min(6);
    base.saturating_mul(multiplier).min(MAX_RETRY_DELAY)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(value: u8) -> Hash {
        [value; 32]
    }

    #[test]
    fn concurrent_admission_fails_fast_while_cleanup_runs() {
        let controller = CacheQuotaController::default();
        let cleanup = match controller
            .begin_admission(100, 10, vec![hash(1)], 100, false)
            .expect("leader")
        {
            CacheQuotaAdmission::Cleanup(cleanup) => cleanup,
            CacheQuotaAdmission::Admitted(_) => panic!("expected cleanup"),
        };

        let denial = controller.quick_denial().expect("denied");
        assert_eq!(denial.kind, CacheQuotaDenialKind::CleanupInProgress);
        assert!(matches!(
            controller.begin_admission(100, 10, vec![hash(2)], 100, false),
            Err(CacheQuotaDenial {
                kind: CacheQuotaDenialKind::CleanupInProgress,
                ..
            })
        ));
        drop(cleanup);
        assert!(matches!(
            controller.begin_admission(80, 10, vec![hash(2)], 100, false),
            Err(CacheQuotaDenial {
                kind: CacheQuotaDenialKind::RetryLater,
                ..
            })
        ));
    }

    #[test]
    fn successful_cleanup_hands_off_to_reserved_write() {
        let controller = CacheQuotaController::default();
        let cleanup = match controller
            .begin_admission(100, 10, vec![hash(1)], 100, false)
            .expect("leader")
        {
            CacheQuotaAdmission::Cleanup(cleanup) => cleanup,
            CacheQuotaAdmission::Admitted(_) => panic!("expected cleanup"),
        };
        assert_eq!(cleanup.target_bytes(), 90);
        let permit = cleanup
            .complete(80, 20, false)
            .expect("admitted after cleanup");
        permit.commit(10);

        assert!(controller.quick_denial().is_none());
        assert!(matches!(
            controller
                .begin_admission(90, 10, vec![hash(2)], 100, false)
                .expect("under quota"),
            CacheQuotaAdmission::Admitted(_)
        ));
    }

    #[test]
    fn no_progress_is_suppressed_without_another_scan() {
        let controller = CacheQuotaController::default();
        let cleanup = match controller
            .begin_admission(100, 10, vec![hash(1)], 100, false)
            .expect("leader")
        {
            CacheQuotaAdmission::Cleanup(cleanup) => cleanup,
            CacheQuotaAdmission::Admitted(_) => panic!("expected cleanup"),
        };
        let denial = cleanup.complete(100, 0, true).expect_err("no room");
        assert_eq!(denial.kind, CacheQuotaDenialKind::NoRoom);
        assert!(matches!(
            controller.begin_admission(100, 10, vec![hash(2)], 100, false),
            Err(CacheQuotaDenial {
                kind: CacheQuotaDenialKind::NoRoom,
                ..
            })
        ));
        assert_eq!(controller.cleanup_epoch_count(), 1);
    }

    #[test]
    fn empty_bounded_page_is_retryable_until_the_sweep_completes() {
        let controller = CacheQuotaController::default();
        let cleanup = match controller
            .begin_admission(100, 10, vec![hash(1)], 100, false)
            .expect("leader")
        {
            CacheQuotaAdmission::Cleanup(cleanup) => cleanup,
            CacheQuotaAdmission::Admitted(_) => panic!("expected cleanup"),
        };
        let denial = cleanup
            .complete(100, 0, false)
            .expect_err("bounded scan must continue");
        assert_eq!(denial.kind, CacheQuotaDenialKind::RetryLater);
        assert!(denial.retry_after <= MIN_RETRY_DELAY);
    }

    #[test]
    fn forced_write_pressure_requires_real_cleanup_progress() {
        let controller = CacheQuotaController::default();
        let cleanup = match controller
            .begin_admission(80, 10, vec![hash(1)], 100, true)
            .expect("leader")
        {
            CacheQuotaAdmission::Cleanup(cleanup) => cleanup,
            CacheQuotaAdmission::Admitted(_) => panic!("forced cleanup must run"),
        };
        let denial = cleanup
            .complete(80, 0, true)
            .expect_err("map pressure was not relieved");
        assert_eq!(denial.kind, CacheQuotaDenialKind::NoRoom);
        assert_eq!(controller.cleanup_epoch_count(), 1);
    }

    #[test]
    fn inflight_hashes_are_refcounted() {
        let controller = CacheQuotaController::default();
        let first = match controller
            .begin_admission(0, 10, vec![hash(1)], 100, false)
            .expect("first")
        {
            CacheQuotaAdmission::Admitted(permit) => permit,
            CacheQuotaAdmission::Cleanup(_) => panic!("unexpected cleanup"),
        };
        let second = match controller
            .begin_admission(0, 10, vec![hash(1)], 100, false)
            .expect("second")
        {
            CacheQuotaAdmission::Admitted(permit) => permit,
            CacheQuotaAdmission::Cleanup(_) => panic!("unexpected cleanup"),
        };
        drop(first);

        let cleanup = match controller
            .begin_admission(100, 10, vec![hash(2)], 100, false)
            .expect("cleanup")
        {
            CacheQuotaAdmission::Cleanup(cleanup) => cleanup,
            CacheQuotaAdmission::Admitted(_) => panic!("expected cleanup"),
        };
        assert!(cleanup.inflight_hashes().contains(&hash(1)));
        drop(cleanup);
        drop(second);
    }

    #[test]
    fn durable_retention_protection_serializes_per_hash_with_cleanup() {
        let controller = CacheQuotaController::default();
        let protected_hash = hash(7);
        let guard = controller
            .protect_retention_hashes(vec![protected_hash, protected_hash])
            .expect("protect before cleanup");
        let cleanup = controller
            .begin_standalone_cleanup()
            .expect("cleanup after protection");
        assert_eq!(
            cleanup.inflight_hashes(),
            &HashSet::from([protected_hash]),
            "cleanup snapshot must include deduplicated durable hashes"
        );
        let concurrent_guard = controller
            .protect_retention_hashes(vec![hash(8)])
            .expect("unrelated durable write may continue during cleanup");
        drop(concurrent_guard);
        drop(guard);

        let deleting = controller
            .begin_retention_delete(hash(9))
            .expect("retention delete claim");
        let denial = controller
            .protect_retention_hashes(vec![hash(9)])
            .expect_err("durable write must not race deletion of the same hash");
        assert_eq!(denial.kind, CacheQuotaDenialKind::CleanupInProgress);
        drop(deleting);
        controller
            .protect_retention_hashes(vec![hash(9)])
            .expect("hash is writable after deletion claim releases");
        cleanup.complete(0);
    }
}
