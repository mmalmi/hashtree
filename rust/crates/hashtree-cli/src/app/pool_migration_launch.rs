use anyhow::{bail, Context, Result};
use hashtree_core::from_hex;
use hashtree_lmdb::{
    LmdbSourceKeysetAudit, PinnedLmdbFileIdentity, PinnedLmdbIdentity, PoolMigrationAuditSummary,
    PoolPhysicalAudit,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::pool_migration_checkpoint::{
    ack_file_name, boottime_millis, request_file_name, timeout_millis,
    validate_checkpoint_operation, validate_root_broker_process, validate_root_broker_service,
    CheckpointBrokerAuthorityV3, MigrationCheckpointAckV3, MigrationCheckpointRequestV3,
    CHECKPOINT_ACK_SCHEMA, CHECKPOINT_REQUEST_SCHEMA, MAX_CHECKPOINT_BYTES,
};
use super::pool_migration_evidence::{
    SourceEvidenceManifestAuthorityV3, SourceEvidenceManifestWriterV3,
};
use super::pool_migration_mount::{
    require_host_execution_namespace, validate_cached_source_read_only_mount_authorities,
    validate_source_read_only_mount_authority, SourceReadOnlyMountAuthorityV3,
};
use super::pool_migration_online_audit::{
    compute_online_audit_binding, load_validated_online_target_audit, online_audit_path,
    OnlineTargetAuditExpectationV3, PoolMigrationOnlineTargetAuditReceiptV3,
    ValidatedOnlineTargetAuditV3, ONLINE_TARGET_AUDIT_FILE_NAME, ONLINE_TARGET_AUDIT_SCHEMA,
};
use super::pool_migration_pinned::{PinnedDirectory, PinnedRegularEntry};
pub(super) use super::pool_migration_protocol::{
    ControllerAuthorityV3, ControllerStateV3, CursorAuthorityV3, FileAuthorityV3, FileIdentityV3,
    LmdbIdentityV3, NamedFileAuthorityV3, PoolAuthorityV3, PoolMigrationLaunchRequestV3,
    PoolTopologyMemberV3, PoolTopologyV3, SourceAuthorityV3, WriterUnitMaskV3,
};
use super::pool_migration_receipt::{
    capture_source_generation_fingerprint, load_validated_prior_source_terminal_receipts,
    validate_frozen_source_generation, PoolMigrationSourceTerminalReceiptV3,
    PriorSourceReceiptExpectationV3, SourceContentAuditV3, SourceGenerationFingerprintV3,
    ValidatedSourceTerminalReceiptV3, MAX_FINAL_SOURCE_RECEIPTS, SOURCE_TERMINAL_FILE_NAME,
    SOURCE_TERMINAL_SCHEMA,
};

#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

pub(super) const REQUEST_SCHEMA: &str = "hashtree-pool-migration-launch-request/v3";
const START_SCHEMA: &str = "hashtree-pool-migration-launch-start/v3";
pub(super) const ACK_SCHEMA: &str = "hashtree-pool-migration-launch-ack/v3";
pub(super) const ATTEMPT_NAMESPACE_NAME: &str = "attempts-v3";
pub(super) const REQUEST_FILE_NAME: &str = "launch-request.json";
const START_FILE_NAME: &str = "launch-started.json";
const ACK_FILE_NAME: &str = "launch-ack.json";
const TERMINAL_AUDIT_FILE_NAME: &str = "terminal-audit.json";
const MAX_REQUEST_BYTES: u64 = 1024 * 1024;
const MAX_TOPOLOGY_BYTES: u64 = 1024 * 1024;
#[cfg(target_os = "linux")]
const MAX_SYSTEMD_ENVIRONMENT_BYTES: u64 = 64 * 1024;
const MAX_CONTROLLER_STATE_BYTES: u64 = 64 * 1024;
const MAX_CURSOR_BYTES: u64 = 1024;
const SYSTEMD_INVOCATION_ID_ENV: &str = "INVOCATION_ID";
pub(super) const POOL_TOPOLOGY_SCHEMA: &str = "hashtree-pool-migration-topology/v3";
pub(super) const CONTROLLER_STATE_SCHEMA: &str = "hashtree-pool-migration-controller-state/v3";
pub(super) const MAX_FINAL_REOPEN_BATCHES: usize = 256;
pub(super) const MAX_FINAL_BATCH_SIZE: usize = 4_096;
pub(super) const MAX_FINAL_SOURCE_READ_CONCURRENCY: usize = 64;
const MEMBER_MARKER_NAME: &str = ".hashtree-pool-member-v1";
const EXTERNAL_MARKER_NAME: &str = ".hashtree-pool-external-v1";

pub(super) fn validate_stopped_final_batch_size(
    final_stopped: bool,
    batch_size: usize,
) -> Result<()> {
    if final_stopped && batch_size != MAX_FINAL_BATCH_SIZE {
        bail!(
            "stopped final migration requires --batch-size {MAX_FINAL_BATCH_SIZE} to bound durable checkpoint amplification"
        );
    }
    Ok(())
}

pub(super) fn validate_pool_migration_release_phase(phase: &str) -> Result<()> {
    match phase {
        "online-bounded" | "final-stopped-source" | "final-stopped-full" => Ok(()),
        _ => bail!(
            "unsupported Pool migration controller phase {phase}; expected online-bounded, final-stopped-source, or final-stopped-full"
        ),
    }
}

pub(super) fn validate_source_read_concurrency(concurrency: usize) -> Result<()> {
    if concurrency > MAX_FINAL_SOURCE_READ_CONCURRENCY {
        bail!(
            "Pool migration source read concurrency exceeds the hard maximum of {MAX_FINAL_SOURCE_READ_CONCURRENCY}"
        );
    }
    Ok(())
}

#[derive(Debug)]
pub(super) struct PoolMigrationLaunchContext<'a> {
    pub(super) launch_request: &'a Path,
    pub(super) source: &'a Path,
    pub(super) source_external_dir: Option<&'a Path>,
    pub(super) pool: &'a Path,
    pub(super) state_file: &'a Path,
    pub(super) resume: bool,
    pub(super) max_items: Option<usize>,
    pub(super) request_wait: Duration,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PoolMigrationLaunchAckV3<'a> {
    schema: &'static str,
    status: &'static str,
    request_path: &'a Path,
    request_sha256: &'a str,
    attempt_namespace: &'a Path,
    nonce: &'a str,
    boot_id: &'a str,
    systemd_invocation_id: &'a str,
    systemd_unit: &'a str,
    systemd_manager: &'a str,
    systemd_fragment_path: &'a Path,
    systemd_fragment_sha256: &'a str,
    systemd_environment_file_path: &'a Path,
    systemd_environment_file_sha256: &'a str,
    pid: u32,
    proc_start_time_ticks: u64,
    acknowledged_at_unix_seconds: u64,
    binary_path: &'a Path,
    binary_sha256: &'a str,
    argv_sha256: String,
    controller_state_sha256: &'a str,
    checkpoint_broker_pid: u32,
    checkpoint_broker_proc_start_time_ticks: u64,
    source_writers_fenced: bool,
    target_writers_fenced: bool,
    fence_held_until_completion: bool,
    source_baseline_sha256: &'a str,
    pool_topology_sha256: &'a str,
    pool_manifest_sha256: String,
    source_lmdb_identity: LmdbIdentityV3,
    pool_lmdb_identity: LmdbIdentityV3,
    cursor_value: Option<&'a str>,
    cursor_sha256: Option<&'a str>,
    additional_cas: Vec<AcknowledgedCasV3<'a>>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PoolMigrationLaunchStartV3 {
    schema: &'static str,
    status: &'static str,
    pid: u32,
    started_at_unix_seconds: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AcknowledgedCasV3<'a> {
    label: &'a str,
    sha256: &'a str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PoolMigrationTerminalAuditReceiptV3<'a> {
    schema: &'static str,
    status: &'static str,
    controller_state_sha256: &'a str,
    source_receipt_sha256: &'a [String],
    source_count: u64,
    source_entries: u64,
    source_bytes: u64,
    source_reconciliation_sha256: String,
    target_stored_locations: u64,
    target_stored_bytes: u64,
    target_catalog_sha256: String,
    target_physical_sha256: String,
    target_manifest_sha256: String,
}

pub(super) struct PoolMigrationSourceUnionAuditV3 {
    pub(super) receipt_sha256: Vec<String>,
    pub(super) source_count: u64,
    pub(super) entries: u64,
    pub(super) bytes: u64,
    pub(super) sha256: [u8; 32],
}

struct ValidatedLaunch {
    cursor: Option<[u8; 32]>,
    boot_id: String,
    systemd_invocation_id: String,
    main_pid: u32,
    proc_start_time_ticks: u64,
    controller_state: ControllerStateV3,
    online_target_audit: Option<ValidatedOnlineTargetAuditV3>,
    paths: PinnedMigrationPaths,
}

pub(super) struct AcknowledgedPoolMigrationLaunch {
    pub(super) cursor: Option<[u8; 32]>,
    pub(super) final_stopped_pass: bool,
    pub(super) final_stopped_source_pass: bool,
    pub(super) final_stopped_full_pass: bool,
    source: PathBuf,
    source_external: Option<PathBuf>,
    pool: PathBuf,
    controller_state_authority: FileAuthorityV3,
    controller_state: ControllerStateV3,
    request: PoolMigrationLaunchRequestV3,
    request_sha256: String,
    acknowledgement_sha256: String,
    checkpoint_state: Mutex<CheckpointChainState>,
    cursor_authority: Mutex<CursorAuthorityV3>,
    attempt: PinnedDirectory,
    pins: PinnedMigrationPaths,
    online_target_audit: Option<ValidatedOnlineTargetAuditV3>,
}

struct CheckpointChainState {
    next_sequence: u64,
    previous_ack_sha256: Option<String>,
}

pub(super) struct AcknowledgedPoolMemberRuntimePaths {
    pub(super) id: String,
    pub(super) configured_path: PathBuf,
    pub(super) runtime_path: PathBuf,
    pub(super) configured_external_path: Option<PathBuf>,
    pub(super) runtime_external_path: Option<PathBuf>,
    pub(super) lmdb_identity: PinnedLmdbIdentity,
}

pub(super) struct AcknowledgedSourceRuntimePlanV3 {
    pub(super) validated: ValidatedSourceTerminalReceiptV3,
    pub(super) runtime_path: PathBuf,
    pub(super) runtime_external_path: Option<PathBuf>,
}

impl AcknowledgedPoolMigrationLaunch {
    pub(super) fn source(&self) -> &Path {
        &self.source
    }

    pub(super) fn source_external(&self) -> Option<&Path> {
        self.source_external.as_deref()
    }

    pub(super) fn pool(&self) -> &Path {
        &self.pool
    }

    pub(super) fn pool_member_runtime_paths(&self) -> Vec<AcknowledgedPoolMemberRuntimePaths> {
        self.pins.pool_member_runtime_paths()
    }

    pub(super) fn source_lmdb_identity(&self) -> PinnedLmdbIdentity {
        self.pins.source_lmdb_files.identity()
    }

    pub(super) fn pool_catalog_lmdb_identity(&self) -> PinnedLmdbIdentity {
        self.pins.pool_lmdb_files.identity()
    }

    pub(super) fn pool_manifest_sha256(&self) -> [u8; 32] {
        self.pins.pool_manifest_sha256
    }

    pub(super) fn online_audit_path(&self) -> Result<PathBuf> {
        if self.request.controller.phase != "online-bounded" {
            bail!("online audit state exists only for online-bounded migration");
        }
        online_audit_path(&self.request.cursor.path)
    }

    pub(super) fn online_audit_binding(&self) -> Result<[u8; 32]> {
        if self.request.controller.phase != "online-bounded" {
            bail!("online audit binding exists only for online-bounded migration");
        }
        compute_online_audit_binding(
            &self.request.controller.rollout_id,
            self.request.source.lmdb_identity,
            self.request.source.external_identity,
            self.request.pool.lmdb_identity,
            &self.request.pool.topology.sha256,
            self.pins.pool_manifest_sha256,
        )
    }

    pub(super) fn ensure_store_paths(&self) -> Result<()> {
        self.pins.ensure_path_identities()
    }

    pub(super) fn ensure_source_paths(&self) -> Result<()> {
        self.pins.ensure_source_path_identities()
    }

    pub(super) fn capture_source_generation(
        &self,
        lmdb: hashtree_lmdb::LmdbEnvironmentGeneration,
    ) -> Result<SourceGenerationFingerprintV3> {
        self.pins.ensure_source_path_identities()?;
        let first = capture_source_generation_fingerprint(
            &self.request.source.lmdb_path,
            self.request.source.lmdb_identity,
            self.request.source.external_path.as_deref(),
            self.request.source.external_identity,
            lmdb,
        )?;
        self.ensure_final_writer_fence()?;
        self.pins.ensure_source_path_identities()?;
        Ok(first)
    }

    pub(super) fn ensure_final_writer_fence(&self) -> Result<()> {
        validate_file_authority(&self.controller_state_authority, "controller state")?;
        validate_controller_state_ownership(&self.controller_state_authority.path)?;
        if !self.final_stopped_pass {
            return validate_legacy_worker_activation_fence(
                &self.controller_state.legacy_worker_template_mask,
                &self.controller_state.legacy_worker_instance_masks,
            );
        }
        // The root controller owns continuous start inhibition and the
        // complete /proc open-handle census. This process can only revalidate
        // the immutable attestation plus point-in-time systemd unit state.
        validate_batched_runtime_masked_final_fence(
            &self.controller_state.stopped_writer_units,
            &self.controller_state.writer_unit_masks,
            &self.controller_state.legacy_worker_template_mask,
            &self.controller_state.legacy_worker_instance_masks,
        )?;
        let mounts = self
            .request
            .source
            .read_only_mounts
            .as_ref()
            .context("stopped final launch has no source read-only mount authority")?;
        validate_source_read_only_mount_authority(
            mounts,
            &self.request.source.lmdb_path,
            self.request.source.lmdb_identity,
            self.request.source.external_path.as_deref(),
            self.request.source.external_identity,
        )?;
        self.validate_source_terminal_receipts(false)
    }

    /// Revalidate the enforceable part of the final writer fence without a
    /// systemd round trip. The migration loop calls this before and after every
    /// bounded batch so removing or replacing a runtime mask cannot go
    /// unnoticed until the next mapping epoch.
    pub(super) fn ensure_final_writer_masks(&self) -> Result<()> {
        validate_legacy_worker_mask_authorities(
            &self.controller_state.legacy_worker_template_mask,
            &self.controller_state.legacy_worker_instance_masks,
        )?;
        if !self.final_stopped_pass {
            return Ok(());
        }
        validate_runtime_writer_mask_authorities(
            &self.controller_state.stopped_writer_units,
            &self.controller_state.writer_unit_masks,
        )?;
        let current_mounts = self
            .request
            .source
            .read_only_mounts
            .as_ref()
            .context("stopped final launch has no source read-only mount authority")?;
        let mut mounts = Vec::with_capacity(self.pins.prior_sources.len().saturating_add(1));
        mounts.push(current_mounts);
        mounts.extend(
            self.pins
                .prior_sources
                .iter()
                .map(|source| &source.read_only_mounts),
        );
        validate_cached_source_read_only_mount_authorities(&mounts)
    }

    pub(super) fn authorize_checkpoint(
        &self,
        operation: &str,
        cursor: Option<[u8; 32]>,
        range_limit: Option<usize>,
    ) -> Result<()> {
        validate_checkpoint_operation(operation)?;
        self.attempt
            .ensure_path_identity("Pool migration attempt directory")?;
        validate_root_broker_process(
            self.request.checkpoint_broker.pid,
            self.request.checkpoint_broker.proc_start_time_ticks,
        )?;
        let mut chain = self
            .checkpoint_state
            .lock()
            .map_err(|_| anyhow::anyhow!("migration checkpoint chain lock poisoned"))?;
        let sequence = chain.next_sequence;
        let request_name = request_file_name(sequence);
        let ack_name = ack_file_name(sequence);
        if self
            .attempt
            .entry_exists(OsStr::new(&request_name), "migration checkpoint request")?
            || self.attempt.entry_exists(
                OsStr::new(&ack_name),
                "migration checkpoint acknowledgement",
            )?
        {
            bail!("migration checkpoint sequence {sequence} was prepublished or replayed");
        }
        let requested_at = boottime_millis()?;
        let timeout = Duration::from_secs(self.request.checkpoint_broker.timeout_seconds);
        let start_before = requested_at
            .checked_add(timeout_millis(timeout)?)
            .context("checkpoint authorization deadline overflow")?;
        let cursor = cursor.map(|hash| hashtree_core::to_hex(&hash));
        let range_limit = range_limit
            .map(u64::try_from)
            .transpose()
            .context("checkpoint range limit exceeds u64")?;
        let request = MigrationCheckpointRequestV3 {
            schema: CHECKPOINT_REQUEST_SCHEMA.to_string(),
            sequence,
            previous_ack_sha256: chain.previous_ack_sha256.clone(),
            operation: operation.to_string(),
            cursor: cursor.clone(),
            range_limit,
            worker_pid: self.request.main_pid,
            worker_proc_start_time_ticks: self.request.proc_start_time_ticks,
            broker_pid: self.request.checkpoint_broker.pid,
            broker_proc_start_time_ticks: self.request.checkpoint_broker.proc_start_time_ticks,
            boot_id: self.request.boot_id.clone(),
            attempt_nonce: self.request.nonce.clone(),
            launch_request_sha256: self.request_sha256.clone(),
            requested_at_boottime_millis: requested_at,
            start_before_boottime_millis: start_before,
        };
        let mut request_bytes =
            serde_json::to_vec(&request).context("serialize migration checkpoint request")?;
        request_bytes.push(b'\n');
        let request_sha256 = sha256_bytes(&request_bytes);
        self.attempt.create_durable_exclusive_with_mode(
            OsStr::new(&request_name),
            &request_bytes,
            "migration checkpoint request",
            0o640,
        )?;

        loop {
            validate_root_broker_process(
                self.request.checkpoint_broker.pid,
                self.request.checkpoint_broker.proc_start_time_ticks,
            )
            .context("checkpoint broker died before authorizing the next operation")?;
            if boottime_millis()? > start_before {
                bail!(
                    "checkpoint broker timed out before authorizing sequence {sequence} operation {operation}"
                );
            }
            if let Some(mut ack_file) = self.attempt.open_regular_optional(
                OsStr::new(&ack_name),
                "migration checkpoint acknowledgement",
            )? {
                let metadata = ack_file
                    .metadata()
                    .context("inspect migration checkpoint acknowledgement")?;
                validate_checkpoint_ack_ownership(&metadata)?;
                let ack_bytes = read_bounded_open_file(
                    &mut ack_file,
                    MAX_CHECKPOINT_BYTES,
                    "migration checkpoint acknowledgement",
                    &self.attempt.path.join(&ack_name),
                )?;
                let ack: MigrationCheckpointAckV3 = serde_json::from_slice(&ack_bytes)
                    .context("parse strict migration checkpoint acknowledgement")?;
                if ack.schema != CHECKPOINT_ACK_SCHEMA
                    || ack.status != "authorized"
                    || ack.sequence != sequence
                    || ack.previous_ack_sha256 != chain.previous_ack_sha256
                    || ack.request_sha256 != request_sha256
                    || ack.operation != operation
                    || ack.cursor != cursor
                    || ack.range_limit != range_limit
                    || ack.worker_pid != self.request.main_pid
                    || ack.worker_proc_start_time_ticks != self.request.proc_start_time_ticks
                    || ack.broker_pid != self.request.checkpoint_broker.pid
                    || ack.broker_proc_start_time_ticks
                        != self.request.checkpoint_broker.proc_start_time_ticks
                    || ack.boot_id != self.request.boot_id
                    || ack.attempt_nonce != self.request.nonce
                    || ack.launch_request_sha256 != self.request_sha256
                    || ack.start_before_boottime_millis != start_before
                    || ack.authorized_at_boottime_millis < requested_at
                    || ack.authorized_at_boottime_millis > start_before
                {
                    bail!(
                        "migration checkpoint acknowledgement does not exactly authorize sequence {sequence}"
                    );
                }
                if boottime_millis()? > ack.start_before_boottime_millis {
                    bail!("migration checkpoint acknowledgement expired before operation start");
                }
                validate_root_broker_process(
                    self.request.checkpoint_broker.pid,
                    self.request.checkpoint_broker.proc_start_time_ticks,
                )
                .context("checkpoint broker died while publishing its authorization")?;
                chain.previous_ack_sha256 = Some(sha256_bytes(&ack_bytes));
                chain.next_sequence = chain
                    .next_sequence
                    .checked_add(1)
                    .context("migration checkpoint sequence overflow")?;
                return Ok(());
            }
            thread::sleep(Duration::from_millis(10));
            self.attempt
                .ensure_path_identity("Pool migration attempt directory")?;
        }
    }

    pub(super) fn ensure_checkpoint_broker_alive(&self) -> Result<()> {
        validate_root_broker_process(
            self.request.checkpoint_broker.pid,
            self.request.checkpoint_broker.proc_start_time_ticks,
        )
        .context("checkpoint broker died during an authorized operation")
    }

    pub(super) fn validate_controller_terminal_cursor(&self, expected_value: &str) -> Result<()> {
        self.pins
            .cursor_parent
            .ensure_path_identity("migration cursor parent")?;
        let mut file = self
            .pins
            .cursor_parent
            .open_regular_optional(&self.pins.cursor_name, "controller terminal cursor")?
            .context("root checkpoint broker did not publish the terminal cursor")?;
        let metadata = file
            .metadata()
            .context("inspect controller terminal cursor")?;
        validate_controller_terminal_cursor_ownership(&metadata)?;
        let bytes = read_bounded_open_file(
            &mut file,
            MAX_CURSOR_BYTES,
            "controller terminal cursor",
            &self.request.cursor.path,
        )?;
        if bytes != format!("{expected_value}\n").as_bytes() {
            bail!("controller terminal cursor does not contain {expected_value}");
        }
        self.ensure_checkpoint_broker_alive()
    }

    fn validate_source_terminal_receipts(&self, physical: bool) -> Result<()> {
        validate_request_source_terminal_receipts(
            &self.request,
            &self.controller_state,
            &self.pins,
            physical,
        )
    }

    pub(super) fn source_terminal_receipts(
        &self,
        physical: bool,
    ) -> Result<Vec<ValidatedSourceTerminalReceiptV3>> {
        load_validated_prior_source_terminal_receipts(
            &self.request.cas,
            &PriorSourceReceiptExpectationV3 {
                boot_id: &self.request.boot_id,
                pool_path: &self.request.pool.path,
                pool_lmdb_identity: self.request.pool.lmdb_identity,
                pool_topology_sha256: &self.request.pool.topology.sha256,
                pool_manifest_sha256: &self.controller_state.pool_manifest_sha256,
                pool_topology: &self.pins.pool_topology,
                stopped_writer_units: &self.controller_state.stopped_writer_units,
                writer_unit_masks: &self.controller_state.writer_unit_masks,
                legacy_worker_template_mask: &self.controller_state.legacy_worker_template_mask,
                legacy_worker_instance_masks: &self.controller_state.legacy_worker_instance_masks,
                expected_service_gid: service_gid(),
                validate_physical_generation: physical,
            },
        )
    }

    pub(super) fn source_terminal_runtime_plans(
        &self,
    ) -> Result<Vec<AcknowledgedSourceRuntimePlanV3>> {
        let receipts = self.source_terminal_receipts(false)?;
        let plans = self.pins.prior_source_runtime_paths(receipts)?;
        for plan in &plans {
            validate_frozen_source_generation(
                &plan.validated.receipt,
                &plan.runtime_path,
                plan.runtime_external_path.as_deref(),
            )
            .with_context(|| {
                format!(
                    "validate pinned prior source generation {}",
                    plan.validated.receipt.source_path.display()
                )
            })?;
        }
        Ok(plans)
    }

    pub(super) fn online_target_audit(&self) -> Result<&ValidatedOnlineTargetAuditV3> {
        self.online_target_audit
            .as_ref()
            .context("launch has no validated online target audit")
    }

    pub(super) fn write_cursor(&self, value: &str) -> Result<()> {
        if self.final_stopped_source_pass {
            self.pins.ensure_source_path_identities()?;
        } else {
            self.pins.ensure_path_identities()?;
        }
        self.pins
            .cursor_parent
            .ensure_path_identity("migration cursor parent")?;
        let mut authority = self
            .cursor_authority
            .lock()
            .map_err(|_| anyhow::anyhow!("migration cursor authority lock poisoned"))?;
        replace_cursor_checkpoint(
            &mut authority,
            &self.pins.cursor_parent,
            &self.pins.cursor_name,
            value,
        )
    }

    pub(super) fn reset_online_cursor(&self) -> Result<()> {
        if self.request.controller.phase != "online-bounded" {
            bail!("only online-bounded migration may reset a scan cursor");
        }
        self.pins.ensure_path_identities()?;
        let mut authority = self
            .cursor_authority
            .lock()
            .map_err(|_| anyhow::anyhow!("migration cursor authority lock poisoned"))?;
        validate_cursor_checkpoint(&authority, &self.pins.cursor_parent, &self.pins.cursor_name)?;
        if authority.exists {
            self.pins
                .cursor_parent
                .durable_remove_regular(&self.pins.cursor_name, "Pool migration cursor")?;
        }
        authority.exists = false;
        authority.value = None;
        authority.sha256 = None;
        validate_cursor_checkpoint(&authority, &self.pins.cursor_parent, &self.pins.cursor_name)
    }

    pub(super) fn write_terminal_audit_receipt(
        &self,
        source: &PoolMigrationSourceUnionAuditV3,
        target: &PoolPhysicalAudit,
    ) -> Result<()> {
        if !self.final_stopped_full_pass {
            bail!("terminal Pool audit receipts are valid only for final-stopped-full");
        }
        self.attempt
            .ensure_path_identity("Pool migration attempt directory")?;
        let receipt = PoolMigrationTerminalAuditReceiptV3 {
            schema: "hashtree-pool-migration-terminal-audit/v3",
            status: "verified",
            controller_state_sha256: &self.controller_state_authority.sha256,
            source_receipt_sha256: &source.receipt_sha256,
            source_count: source.source_count,
            source_entries: source.entries,
            source_bytes: source.bytes,
            source_reconciliation_sha256: hashtree_core::to_hex(&source.sha256),
            target_stored_locations: target.stored_locations,
            target_stored_bytes: target.stored_bytes,
            target_catalog_sha256: hashtree_core::to_hex(&target.catalog_sha256),
            target_physical_sha256: hashtree_core::to_hex(&target.physical_sha256),
            target_manifest_sha256: hashtree_core::to_hex(&target.manifest_sha256),
        };
        let mut bytes =
            serde_json::to_vec(&receipt).context("serialize terminal Pool audit receipt")?;
        bytes.push(b'\n');
        self.attempt.create_durable_exclusive(
            OsStr::new(TERMINAL_AUDIT_FILE_NAME),
            &bytes,
            "terminal Pool audit receipt",
        )
    }

    pub(super) fn write_online_target_audit_receipt(
        &self,
        audit_store_path: PathBuf,
        audit_binding: [u8; 32],
        source_evidence: SourceEvidenceManifestAuthorityV3,
        summary: &PoolMigrationAuditSummary,
    ) -> Result<()> {
        if self.request.controller.phase != "online-bounded" {
            bail!("online target audit receipts are valid only for online-bounded");
        }
        if source_evidence.entries != summary.entries {
            bail!("online target evidence count differs from its durable audit summary");
        }
        let terminal_cursor = self
            .cursor_authority
            .lock()
            .map_err(|_| anyhow::anyhow!("migration cursor authority lock poisoned"))?
            .clone();
        let cursor_shape_valid = if terminal_cursor.exists {
            terminal_cursor.value.is_some() && terminal_cursor.sha256.is_some()
        } else {
            terminal_cursor.value.is_none() && terminal_cursor.sha256.is_none()
        };
        if !cursor_shape_valid {
            bail!("completed online target audit has an inconsistent terminal cursor");
        }
        let receipt = PoolMigrationOnlineTargetAuditReceiptV3 {
            schema: ONLINE_TARGET_AUDIT_SCHEMA.to_string(),
            status: "verified".to_string(),
            phase: self.request.controller.phase.clone(),
            rollout_id: self.request.controller.rollout_id.clone(),
            boot_id: self.request.boot_id.clone(),
            attempt_namespace: self.request.attempt_namespace.clone(),
            attempt_namespace_identity: self.request.attempt_namespace_identity,
            attempt_identity: self.request.attempt_identity,
            attempt_nonce: self.request.nonce.clone(),
            request_path: self.attempt.path.join(REQUEST_FILE_NAME),
            request_sha256: self.request_sha256.clone(),
            acknowledgement_path: self.attempt.path.join(ACK_FILE_NAME),
            acknowledgement_sha256: self.acknowledgement_sha256.clone(),
            terminal_cursor,
            worker_binary: self.request.binary.clone(),
            worker_argv_sha256: argv_sha256(&self.request.argv),
            systemd_unit: self.request.systemd_unit.clone(),
            systemd_invocation_id: self.request.systemd_invocation_id.clone(),
            systemd_fragment: self.request.systemd_fragment.clone(),
            systemd_environment_file: self.request.systemd_environment_file.clone(),
            main_pid: self.request.main_pid,
            proc_start_time_ticks: self.request.proc_start_time_ticks,
            controller_state_sha256: self.controller_state_authority.sha256.clone(),
            source_path: self.request.source.lmdb_path.clone(),
            source_lmdb_identity: self.request.source.lmdb_identity,
            source_external_path: self.request.source.external_path.clone(),
            source_external_identity: self.request.source.external_identity,
            source_baseline_sha256: self.request.source.baseline.sha256.clone(),
            pool_path: self.request.pool.path.clone(),
            pool_lmdb_identity: self.request.pool.lmdb_identity,
            pool_topology_sha256: self.request.pool.topology.sha256.clone(),
            pool_manifest_sha256: hex::encode(self.pins.pool_manifest_sha256),
            audit_store_path,
            audit_binding_sha256: hex::encode(audit_binding),
            verified_entries: summary.entries,
            verified_bytes: summary.bytes,
            content_sha256: hashtree_core::to_hex(&summary.content_sha256),
            source_evidence,
        };
        let mut bytes =
            serde_json::to_vec(&receipt).context("serialize online target audit receipt")?;
        bytes.push(b'\n');
        self.attempt.create_durable_exclusive_with_mode(
            OsStr::new(ONLINE_TARGET_AUDIT_FILE_NAME),
            &bytes,
            "online target audit receipt",
            0o640,
        )
    }

    pub(super) fn write_source_terminal_receipt(
        &self,
        source: &LmdbSourceKeysetAudit,
        content: &SourceContentAuditV3,
        source_evidence: SourceEvidenceManifestAuthorityV3,
        source_generation: SourceGenerationFingerprintV3,
    ) -> Result<()> {
        if self.request.controller.phase != "final-stopped-source" {
            bail!("source-terminal receipts are valid only for final-stopped-source");
        }
        if content.verified_entries != source.blob_entries {
            bail!(
                "source content verification count {} differs from terminal source key count {}",
                content.verified_entries,
                source.blob_entries
            );
        }
        self.attempt
            .ensure_path_identity("Pool migration attempt directory")?;
        self.pins.ensure_source_path_identities()?;
        self.ensure_final_writer_fence()?;
        let terminal_cursor_bytes = b"source-complete\n";
        let terminal_cursor = CursorAuthorityV3 {
            path: self.request.cursor.path.clone(),
            parent_identity: self.request.cursor.parent_identity,
            exists: true,
            value: Some("source-complete".to_string()),
            sha256: Some(sha256_bytes(terminal_cursor_bytes)),
        };
        let receipt = PoolMigrationSourceTerminalReceiptV3 {
            schema: SOURCE_TERMINAL_SCHEMA.to_string(),
            status: "verified".to_string(),
            phase: self.request.controller.phase.clone(),
            boot_id: self.request.boot_id.clone(),
            attempt_namespace: self.request.attempt_namespace.clone(),
            attempt_namespace_identity: self.request.attempt_namespace_identity,
            attempt_identity: self.request.attempt_identity,
            attempt_nonce: self.request.nonce.clone(),
            request_path: self.attempt.path.join(REQUEST_FILE_NAME),
            request_sha256: self.request_sha256.clone(),
            acknowledgement_path: self.attempt.path.join(ACK_FILE_NAME),
            acknowledgement_sha256: self.acknowledgement_sha256.clone(),
            terminal_cursor,
            worker_binary: self.request.binary.clone(),
            worker_argv_sha256: argv_sha256(&self.request.argv),
            systemd_unit: self.request.systemd_unit.clone(),
            systemd_invocation_id: self.request.systemd_invocation_id.clone(),
            systemd_fragment: self.request.systemd_fragment.clone(),
            systemd_environment_file: self.request.systemd_environment_file.clone(),
            main_pid: self.request.main_pid,
            proc_start_time_ticks: self.request.proc_start_time_ticks,
            controller_state_sha256: self.controller_state_authority.sha256.clone(),
            source_path: self.request.source.lmdb_path.clone(),
            source_lmdb_identity: self.request.source.lmdb_identity,
            source_external_path: self.request.source.external_path.clone(),
            source_external_identity: self.request.source.external_identity,
            source_read_only_mounts: self
                .request
                .source
                .read_only_mounts
                .clone()
                .context("source-terminal launch has no read-only mount authority")?,
            source_baseline_sha256: self.request.source.baseline.sha256.clone(),
            source_blob_entries: source.blob_entries,
            source_metadata_entries: source.metadata_entries,
            source_blob_only_entries: source.blob_only_entries,
            source_legacy_blob_only: source.legacy_blob_only,
            source_inline_entries: source.inline_entries,
            source_loose_external_entries: source.loose_external_entries,
            source_packed_external_entries: source.packed_external_entries,
            source_keyset_sha256: hashtree_core::to_hex(&source.sha256),
            source_catalog_location_sha256: hashtree_core::to_hex(&source.catalog_location_sha256),
            source_verified_entries: content.verified_entries,
            source_verified_bytes: content.verified_bytes,
            source_content_sha256: hashtree_core::to_hex(&content.sha256),
            online_target_audit_certification_sha256: self
                .online_target_audit()?
                .certification_sha256
                .clone(),
            source_evidence,
            source_generation,
            pool_path: self.request.pool.path.clone(),
            pool_lmdb_identity: self.request.pool.lmdb_identity,
            pool_topology_sha256: self.request.pool.topology.sha256.clone(),
            pool_manifest_sha256: hex::encode(self.pins.pool_manifest_sha256),
            pool_topology: self.pins.pool_topology.clone(),
            stopped_writer_units: self.controller_state.stopped_writer_units.clone(),
            writer_unit_masks: self.controller_state.writer_unit_masks.clone(),
            legacy_worker_template_mask: self.controller_state.legacy_worker_template_mask.clone(),
            legacy_worker_instance_masks: self
                .controller_state
                .legacy_worker_instance_masks
                .clone(),
            source_read_only: true,
            target_audit_deferred: true,
        };
        let mut bytes =
            serde_json::to_vec(&receipt).context("serialize source-terminal receipt")?;
        bytes.push(b'\n');
        self.attempt.create_durable_exclusive_with_mode(
            OsStr::new(SOURCE_TERMINAL_FILE_NAME),
            &bytes,
            "source-terminal receipt",
            0o640,
        )
    }

    pub(super) fn create_source_evidence_writer(&self) -> Result<SourceEvidenceManifestWriterV3> {
        if !matches!(
            self.request.controller.phase.as_str(),
            "online-bounded" | "final-stopped-source"
        ) {
            bail!(
                "source evidence manifests are valid only for online-bounded or final-stopped-source"
            );
        }
        self.attempt
            .ensure_path_identity("Pool migration attempt directory")?;
        SourceEvidenceManifestWriterV3::create(&self.attempt.path)
    }
}

fn validate_request_source_terminal_receipts(
    request: &PoolMigrationLaunchRequestV3,
    state: &ControllerStateV3,
    paths: &PinnedMigrationPaths,
    physical: bool,
) -> Result<()> {
    load_request_source_terminal_receipts(request, state, paths, physical).map(|_| ())
}

fn load_request_source_terminal_receipts(
    request: &PoolMigrationLaunchRequestV3,
    state: &ControllerStateV3,
    paths: &PinnedMigrationPaths,
    physical: bool,
) -> Result<Vec<ValidatedSourceTerminalReceiptV3>> {
    let receipts = load_validated_prior_source_terminal_receipts(
        &request.cas,
        &PriorSourceReceiptExpectationV3 {
            boot_id: &request.boot_id,
            pool_path: &request.pool.path,
            pool_lmdb_identity: request.pool.lmdb_identity,
            pool_topology_sha256: &request.pool.topology.sha256,
            pool_manifest_sha256: &state.pool_manifest_sha256,
            pool_topology: &paths.pool_topology,
            stopped_writer_units: &state.stopped_writer_units,
            writer_unit_masks: &state.writer_unit_masks,
            legacy_worker_template_mask: &state.legacy_worker_template_mask,
            legacy_worker_instance_masks: &state.legacy_worker_instance_masks,
            expected_service_gid: service_gid(),
            validate_physical_generation: physical,
        },
    )?;
    let receipt_sha256 = receipts
        .iter()
        .map(|validated| validated.authority_sha256.clone())
        .collect::<Vec<_>>();
    if receipt_sha256 != state.source_terminal_receipt_sha256 {
        bail!(
            "source-terminal receipt CAS set differs from the exact controller-state receipt set"
        );
    }
    Ok(receipts)
}

#[cfg(target_os = "linux")]
fn service_gid() -> Option<u32> {
    Some(unsafe { libc::getegid() })
}

#[cfg(not(target_os = "linux"))]
fn service_gid() -> Option<u32> {
    None
}

fn pin_prior_source_paths(
    receipts: &[ValidatedSourceTerminalReceiptV3],
) -> Result<Vec<PinnedPriorSourcePaths>> {
    receipts
        .iter()
        .map(|validated| {
            let receipt = &validated.receipt;
            let directory =
                PinnedDirectory::open_exact(&receipt.source_path, "prior source LMDB directory")?;
            let lmdb_files = PinnedLmdbFiles::pin(&directory, "prior source LMDB")?;
            lmdb_files.require_authority_identity(
                &directory,
                receipt.source_lmdb_identity,
                "prior source LMDB",
            )?;
            let external_directory = receipt
                .source_external_path
                .as_deref()
                .map(|path| {
                    let directory =
                        PinnedDirectory::open_exact(path, "prior source external directory")?;
                    let identity = receipt
                        .source_external_identity
                        .context("prior source receipt has an external path without identity")?;
                    directory
                        .require_authority_identity(identity, "prior source external directory")?;
                    Ok::<PinnedDirectory, anyhow::Error>(directory)
                })
                .transpose()?;
            if receipt.source_external_path.is_none() != receipt.source_external_identity.is_none()
            {
                bail!("prior source external path/identity authority is incomplete");
            }
            validate_frozen_source_generation(
                receipt,
                &directory.runtime_path(),
                external_directory
                    .as_ref()
                    .map(PinnedDirectory::runtime_path)
                    .as_deref(),
            )
            .with_context(|| {
                format!(
                    "validate retained prior source generation {}",
                    receipt.source_path.display()
                )
            })?;
            Ok(PinnedPriorSourcePaths {
                authority_sha256: validated.authority_sha256.clone(),
                configured_path: receipt.source_path.clone(),
                directory,
                lmdb_files,
                configured_external_path: receipt.source_external_path.clone(),
                external_directory,
                read_only_mounts: receipt.source_read_only_mounts.clone(),
            })
        })
        .collect()
}

struct PinnedMigrationPaths {
    source: PinnedDirectory,
    source_lmdb_files: PinnedLmdbFiles,
    source_external: Option<PinnedDirectory>,
    pool: PinnedDirectory,
    pool_lmdb_files: PinnedLmdbFiles,
    pool_manifest_sha256: [u8; 32],
    pool_topology: PoolTopologyV3,
    pool_members: Vec<PinnedPoolMemberPaths>,
    prior_sources: Vec<PinnedPriorSourcePaths>,
    cursor_parent: PinnedDirectory,
    cursor_name: std::ffi::OsString,
}

struct PinnedPriorSourcePaths {
    authority_sha256: String,
    configured_path: PathBuf,
    directory: PinnedDirectory,
    lmdb_files: PinnedLmdbFiles,
    configured_external_path: Option<PathBuf>,
    external_directory: Option<PinnedDirectory>,
    read_only_mounts: SourceReadOnlyMountAuthorityV3,
}

struct PinnedPoolMemberPaths {
    id: String,
    configured_path: PathBuf,
    directory: PinnedDirectory,
    lmdb_files: PinnedLmdbFiles,
    marker_sha256: String,
    configured_external_path: Option<PathBuf>,
    external_directory: Option<PinnedDirectory>,
    external_marker_sha256: Option<String>,
}

struct PinnedPoolTopology {
    manifest_sha256: [u8; 32],
    topology: PoolTopologyV3,
    members: Vec<PinnedPoolMemberPaths>,
}

struct PinnedLmdbFiles {
    data: PinnedRegularEntry,
    lock: PinnedRegularEntry,
}

struct LaunchRendezvous {
    attempt: PinnedDirectory,
    request: File,
    request_snapshot: std::fs::Metadata,
    request_path: PathBuf,
}

impl PinnedLmdbFiles {
    fn pin(directory: &PinnedDirectory, label: &str) -> Result<Self> {
        Ok(Self {
            data: directory.pin_regular(OsStr::new("data.mdb"), &format!("{label} data.mdb"))?,
            lock: directory.pin_regular(OsStr::new("lock.mdb"), &format!("{label} lock.mdb"))?,
        })
    }

    fn same_objects(&self, other: &Self) -> bool {
        self.data.same_object(&other.data) && self.lock.same_object(&other.lock)
    }

    fn ensure_identities(&self, directory: &PinnedDirectory, label: &str) -> Result<()> {
        self.data
            .ensure_identity(directory, &format!("{label} data.mdb"))?;
        self.lock
            .ensure_identity(directory, &format!("{label} lock.mdb"))?;
        Ok(())
    }

    fn identity(&self) -> PinnedLmdbIdentity {
        #[cfg(unix)]
        {
            PinnedLmdbIdentity {
                data: PinnedLmdbFileIdentity {
                    device: self.data.device,
                    inode: self.data.inode,
                },
                lock: PinnedLmdbFileIdentity {
                    device: self.lock.device,
                    inode: self.lock.inode,
                },
            }
        }
        #[cfg(not(unix))]
        {
            PinnedLmdbIdentity {
                data: PinnedLmdbFileIdentity {
                    device: 0,
                    inode: 0,
                },
                lock: PinnedLmdbFileIdentity {
                    device: 0,
                    inode: 0,
                },
            }
        }
    }

    fn leaf_authority_identities(&self) -> [(&'static str, FileIdentityV3); 2] {
        #[cfg(unix)]
        {
            [
                (
                    "data.mdb",
                    FileIdentityV3 {
                        device: self.data.device,
                        inode: self.data.inode,
                    },
                ),
                (
                    "lock.mdb",
                    FileIdentityV3 {
                        device: self.lock.device,
                        inode: self.lock.inode,
                    },
                ),
            ]
        }
        #[cfg(not(unix))]
        {
            [
                (
                    "data.mdb",
                    FileIdentityV3 {
                        device: 0,
                        inode: 0,
                    },
                ),
                (
                    "lock.mdb",
                    FileIdentityV3 {
                        device: 0,
                        inode: 0,
                    },
                ),
            ]
        }
    }

    fn authority_identity(&self, directory: &PinnedDirectory) -> LmdbIdentityV3 {
        #[cfg(unix)]
        {
            LmdbIdentityV3 {
                directory: directory.authority_identity(),
                data: FileIdentityV3 {
                    device: self.data.device,
                    inode: self.data.inode,
                },
                lock: FileIdentityV3 {
                    device: self.lock.device,
                    inode: self.lock.inode,
                },
            }
        }
        #[cfg(not(unix))]
        {
            LmdbIdentityV3 {
                directory: directory.authority_identity(),
                data: FileIdentityV3 {
                    device: 0,
                    inode: 0,
                },
                lock: FileIdentityV3 {
                    device: 0,
                    inode: 0,
                },
            }
        }
    }

    fn require_authority_identity(
        &self,
        directory: &PinnedDirectory,
        expected: LmdbIdentityV3,
        label: &str,
    ) -> Result<()> {
        if self.authority_identity(directory) != expected {
            bail!("{label} directory/data/lock identity differs from controller authority");
        }
        Ok(())
    }
}

impl PinnedMigrationPaths {
    fn same_objects(&self, other: &Self) -> bool {
        self.source.same_object(&other.source)
            && self
                .source_lmdb_files
                .same_objects(&other.source_lmdb_files)
            && match (&self.source_external, &other.source_external) {
                (Some(left), Some(right)) => left.same_object(right),
                (None, None) => true,
                _ => false,
            }
            && self.pool.same_object(&other.pool)
            && self.pool_lmdb_files.same_objects(&other.pool_lmdb_files)
            && self.pool_manifest_sha256 == other.pool_manifest_sha256
            && self.pool_topology == other.pool_topology
            && self.pool_members.len() == other.pool_members.len()
            && self
                .pool_members
                .iter()
                .zip(&other.pool_members)
                .all(|(left, right)| left.same_objects(right))
            && self.prior_sources.len() == other.prior_sources.len()
            && self
                .prior_sources
                .iter()
                .zip(&other.prior_sources)
                .all(|(left, right)| left.same_objects(right))
            && self.cursor_parent.same_object(&other.cursor_parent)
            && self.cursor_name == other.cursor_name
    }

    fn ensure_path_identities(&self) -> Result<()> {
        self.ensure_source_path_identities()?;
        self.pool.ensure_path_identity("target Pool")?;
        self.pool_lmdb_files
            .ensure_identities(&self.pool, "target Pool catalog")?;
        for member in &self.pool_members {
            member.ensure_path_identities_and_markers()?;
        }
        for source in &self.prior_sources {
            source.ensure_path_identities()?;
        }
        Ok(())
    }

    fn ensure_source_path_identities(&self) -> Result<()> {
        self.source.ensure_path_identity("source LMDB")?;
        self.source_lmdb_files
            .ensure_identities(&self.source, "source LMDB")?;
        if let Some(external) = &self.source_external {
            external.ensure_path_identity("source external directory")?;
        }
        self.cursor_parent
            .ensure_path_identity("migration cursor parent")?;
        Ok(())
    }

    fn acquire_cursor_parent_lease(&self) -> Result<()> {
        self.cursor_parent.acquire_exclusive_migration_lease()
    }

    fn source_runtime_path(&self) -> PathBuf {
        self.source.runtime_path()
    }

    fn source_external_runtime_path(&self) -> Option<PathBuf> {
        self.source_external
            .as_ref()
            .map(PinnedDirectory::runtime_path)
    }

    fn pool_runtime_path(&self) -> PathBuf {
        self.pool.runtime_path()
    }

    fn pool_member_runtime_paths(&self) -> Vec<AcknowledgedPoolMemberRuntimePaths> {
        self.pool_members
            .iter()
            .map(|member| AcknowledgedPoolMemberRuntimePaths {
                id: member.id.clone(),
                configured_path: member.configured_path.clone(),
                runtime_path: member.directory.runtime_path(),
                configured_external_path: member.configured_external_path.clone(),
                runtime_external_path: member
                    .external_directory
                    .as_ref()
                    .map(PinnedDirectory::runtime_path),
                lmdb_identity: member.lmdb_files.identity(),
            })
            .collect()
    }

    fn prior_source_runtime_paths(
        &self,
        receipts: Vec<ValidatedSourceTerminalReceiptV3>,
    ) -> Result<Vec<AcknowledgedSourceRuntimePlanV3>> {
        if receipts.len() != self.prior_sources.len() {
            bail!("validated source receipt count differs from retained source pin count");
        }
        receipts
            .into_iter()
            .zip(&self.prior_sources)
            .map(|(validated, pinned)| {
                if validated.authority_sha256 != pinned.authority_sha256
                    || validated.receipt.source_path != pinned.configured_path
                    || validated.receipt.source_external_path != pinned.configured_external_path
                {
                    bail!("validated source receipt order/path differs from retained source pins");
                }
                pinned.ensure_path_identities()?;
                Ok(AcknowledgedSourceRuntimePlanV3 {
                    validated,
                    runtime_path: pinned.directory.runtime_path(),
                    runtime_external_path: pinned
                        .external_directory
                        .as_ref()
                        .map(PinnedDirectory::runtime_path),
                })
            })
            .collect()
    }

    fn ensure_isolated_authority_roots(
        &self,
        request: &PoolMigrationLaunchRequestV3,
        request_path: &Path,
    ) -> Result<()> {
        let mut lmdbs: Vec<(String, &PinnedLmdbFiles)> = vec![
            ("source LMDB".into(), &self.source_lmdb_files),
            ("target Pool catalog".into(), &self.pool_lmdb_files),
        ];
        for member in &self.pool_members {
            lmdbs.push((format!("Pool member {}", member.id), &member.lmdb_files));
        }
        for source in &self.prior_sources {
            if !source.lmdb_files.same_objects(&self.source_lmdb_files) {
                lmdbs.push((
                    format!("prior source {} LMDB", source.authority_sha256),
                    &source.lmdb_files,
                ));
            }
        }
        let mut leaf_owners: HashMap<FileIdentityV3, String> = HashMap::new();
        for (role, files) in lmdbs {
            for (leaf, identity) in files.leaf_authority_identities() {
                let owner = format!("{role} {leaf}");
                if let Some(previous) = leaf_owners.insert(identity, owner.clone()) {
                    bail!(
                        "LMDB leaf identity alias is forbidden: {previous} and {owner} are the same inode"
                    );
                }
            }
        }

        let mut roots: Vec<(String, &PinnedDirectory)> = vec![
            ("source LMDB".into(), &self.source),
            ("target Pool catalog".into(), &self.pool),
            ("migration cursor parent".into(), &self.cursor_parent),
        ];
        if let Some(directory) = &self.source_external {
            roots.push(("source external directory".into(), directory));
        }
        for member in &self.pool_members {
            roots.push((
                format!("Pool member {} directory", member.id),
                &member.directory,
            ));
            if let Some(directory) = &member.external_directory {
                roots.push((
                    format!("Pool member {} external directory", member.id),
                    directory,
                ));
            }
        }
        for source in &self.prior_sources {
            if !source.directory.same_object(&self.source) {
                roots.push((
                    format!("prior source {} LMDB", source.authority_sha256),
                    &source.directory,
                ));
            }
            if let Some(directory) = &source.external_directory {
                let aliases_current = self
                    .source_external
                    .as_ref()
                    .is_some_and(|current| directory.same_object(current));
                if !aliases_current {
                    roots.push((
                        format!(
                            "prior source {} external directory",
                            source.authority_sha256
                        ),
                        directory,
                    ));
                }
            }
        }
        for left in 0..roots.len() {
            for right in left + 1..roots.len() {
                let (left_label, left_root) = &roots[left];
                let (right_label, right_root) = &roots[right];
                if left_root.same_object(right_root)
                    || paths_overlap(&left_root.path, &right_root.path)
                {
                    bail!("{left_label} overlaps {right_label}");
                }
            }
        }

        let attempt_path = request_path
            .parent()
            .context("launch request has no attempt directory")?;
        let attempt =
            PinnedDirectory::open_exact(attempt_path, "Pool migration attempt directory")?;
        let namespace = PinnedDirectory::open_exact(
            &request.attempt_namespace,
            "Pool migration v3 attempt namespace",
        )?;
        for (label, root) in &roots {
            for (control_label, control) in [
                ("Pool migration attempt directory", &attempt),
                ("Pool migration v3 attempt namespace", &namespace),
            ] {
                if root.same_object(control) || paths_overlap(&root.path, &control.path) {
                    bail!("{label} overlaps {control_label}");
                }
            }
        }

        let mut evidence = vec![
            ("migration binary", request.binary.path.as_path()),
            (
                "systemd unit fragment",
                request.systemd_fragment.path.as_path(),
            ),
            (
                "systemd environment file",
                request.systemd_environment_file.path.as_path(),
            ),
            (
                "controller executable",
                request.controller.executable.path.as_path(),
            ),
            ("controller state", request.controller.state.path.as_path()),
            ("source baseline", request.source.baseline.path.as_path()),
            ("Pool topology", request.pool.topology.path.as_path()),
        ];
        evidence.extend(
            request
                .cas
                .iter()
                .map(|authority| (authority.label.as_str(), authority.path.as_path())),
        );
        for (evidence_label, evidence_path) in evidence {
            for (root_label, root) in &roots {
                if evidence_path.starts_with(&root.path) {
                    bail!("{evidence_label} authority is stored inside {root_label}");
                }
            }
        }
        Ok(())
    }
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

impl PinnedPoolMemberPaths {
    fn same_objects(&self, other: &Self) -> bool {
        self.id == other.id
            && self.configured_path == other.configured_path
            && self.directory.same_object(&other.directory)
            && self.lmdb_files.same_objects(&other.lmdb_files)
            && self.marker_sha256 == other.marker_sha256
            && self.configured_external_path == other.configured_external_path
            && match (&self.external_directory, &other.external_directory) {
                (Some(left), Some(right)) => left.same_object(right),
                (None, None) => true,
                _ => false,
            }
            && self.external_marker_sha256 == other.external_marker_sha256
    }

    fn ensure_path_identities_and_markers(&self) -> Result<()> {
        self.directory
            .ensure_path_identity(&format!("Pool member {} directory", self.id))?;
        self.lmdb_files
            .ensure_identities(&self.directory, &format!("Pool member {}", self.id))?;
        validate_marker_in_directory(
            &self.directory,
            OsStr::new(MEMBER_MARKER_NAME),
            &self.marker_sha256,
            &format!("Pool member {} marker", self.id),
        )?;
        match (
            &self.external_directory,
            self.external_marker_sha256.as_deref(),
        ) {
            (Some(directory), Some(expected_sha256)) => {
                directory
                    .ensure_path_identity(&format!("Pool member {} external directory", self.id))?;
                validate_marker_in_directory(
                    directory,
                    OsStr::new(EXTERNAL_MARKER_NAME),
                    expected_sha256,
                    &format!("Pool member {} external marker", self.id),
                )?;
            }
            (None, None) => {}
            _ => bail!(
                "Pool member {} has incomplete pinned external paths",
                self.id
            ),
        }
        Ok(())
    }
}

impl PinnedPriorSourcePaths {
    fn same_objects(&self, other: &Self) -> bool {
        self.authority_sha256 == other.authority_sha256
            && self.configured_path == other.configured_path
            && self.directory.same_object(&other.directory)
            && self.lmdb_files.same_objects(&other.lmdb_files)
            && self.configured_external_path == other.configured_external_path
            && self.read_only_mounts == other.read_only_mounts
            && match (&self.external_directory, &other.external_directory) {
                (Some(left), Some(right)) => left.same_object(right),
                (None, None) => true,
                _ => false,
            }
    }

    fn ensure_path_identities(&self) -> Result<()> {
        self.directory
            .ensure_path_identity("prior source LMDB directory")?;
        self.lmdb_files
            .ensure_identities(&self.directory, "prior source LMDB")?;
        if let Some(external) = &self.external_directory {
            external.ensure_path_identity("prior source external directory")?;
        }
        Ok(())
    }
}

impl LaunchRendezvous {
    fn read_request(&mut self) -> Result<Vec<u8>> {
        self.attempt
            .ensure_path_identity("Pool migration attempt directory")?;
        let before = self
            .request
            .metadata()
            .context("inspect open Pool migration launch request")?;
        validate_launch_request_ownership(&before)?;
        ensure_same_file_snapshot(
            &self.request_snapshot,
            &before,
            "Pool migration launch request",
        )?;
        if before.len() > MAX_REQUEST_BYTES {
            bail!(
                "launch request {} is larger than the {} byte limit",
                self.request_path.display(),
                MAX_REQUEST_BYTES
            );
        }
        self.request
            .seek(SeekFrom::Start(0))
            .context("rewind Pool migration launch request")?;
        let mut bytes = Vec::with_capacity(before.len() as usize);
        Read::by_ref(&mut self.request)
            .take(MAX_REQUEST_BYTES + 1)
            .read_to_end(&mut bytes)
            .context("read Pool migration launch request")?;
        if bytes.len() as u64 > MAX_REQUEST_BYTES {
            bail!(
                "launch request {} grew beyond the {} byte limit",
                self.request_path.display(),
                MAX_REQUEST_BYTES
            );
        }
        let after = self
            .request
            .metadata()
            .context("reinspect open Pool migration launch request")?;
        validate_launch_request_ownership(&after)?;
        ensure_same_file_snapshot(&before, &after, "Pool migration launch request")?;
        let reopened = self
            .attempt
            .open_regular_optional(OsStr::new(REQUEST_FILE_NAME), "launch request")?
            .context("Pool migration launch request disappeared during validation")?;
        let entry = reopened
            .metadata()
            .context("reinspect Pool migration launch request directory entry")?;
        ensure_same_file_snapshot(&after, &entry, "Pool migration launch request path")?;
        Ok(bytes)
    }

    fn acknowledge(&mut self, expected_request: &[u8], bytes: &[u8]) -> Result<()> {
        self.attempt
            .ensure_path_identity("Pool migration attempt directory")?;
        if self.read_request()? != expected_request {
            bail!("Pool migration launch request changed immediately before acknowledgement");
        }
        if self
            .attempt
            .entry_exists(OsStr::new(ACK_FILE_NAME), "launch acknowledgement")?
        {
            bail!(
                "Pool migration launch acknowledgement already exists; create a fresh {ATTEMPT_NAMESPACE_NAME} nonce"
            );
        }
        self.attempt.create_durable_exclusive(
            OsStr::new(ACK_FILE_NAME),
            bytes,
            "Pool migration launch acknowledgement",
        )
    }

    fn into_attempt(self) -> PinnedDirectory {
        self.attempt
    }
}

pub(super) fn acknowledge_pool_migration_launch(
    context: PoolMigrationLaunchContext<'_>,
) -> Result<AcknowledgedPoolMigrationLaunch> {
    validate_durable_lmdb_environment()?;
    if !context.resume {
        bail!("Pool migration v3 launch requests require --resume");
    }

    let mut rendezvous = wait_for_launch_request(context.launch_request, context.request_wait)?;
    let request_path = rendezvous.request_path.clone();
    validate_request_location(&request_path)?;
    let request_bytes = rendezvous.read_request()?;
    let request_sha256 = sha256_bytes(&request_bytes);
    let request: PoolMigrationLaunchRequestV3 =
        serde_json::from_slice(&request_bytes).context("parse Pool migration launch request v3")?;

    validate_request_shape(&request, &request_path)?;
    let first = validate_launch_authority(&request, &context)?;

    // Re-read every external authority immediately before the durable
    // acknowledgement. The controller owns exclusion, while this second CAS
    // pass makes a changed request, cursor, binary, or evidence leaf a
    // fail-closed pre-open error.
    let reloaded_request = rendezvous.read_request()?;
    if sha256_bytes(&reloaded_request) != request_sha256 || reloaded_request != request_bytes {
        bail!("Pool migration launch request changed during authority validation");
    }
    let second = validate_launch_authority(&request, &context)?;
    if first.cursor != second.cursor
        || first.boot_id != second.boot_id
        || first.systemd_invocation_id != second.systemd_invocation_id
        || first.main_pid != second.main_pid
        || first.proc_start_time_ticks != second.proc_start_time_ticks
        || first.controller_state != second.controller_state
        || first
            .online_target_audit
            .as_ref()
            .map(|audit| audit.certification_sha256.as_str())
            != second
                .online_target_audit
                .as_ref()
                .map(|audit| audit.certification_sha256.as_str())
        || !first.paths.same_objects(&second.paths)
    {
        bail!("Pool migration launch authority changed during validation");
    }
    second.paths.ensure_path_identities()?;
    second.paths.acquire_cursor_parent_lease()?;

    let attempt_dir = request_path
        .parent()
        .context("launch request has no attempt directory")?;
    let ack_path = attempt_dir.join(ACK_FILE_NAME);
    let ack = PoolMigrationLaunchAckV3 {
        schema: ACK_SCHEMA,
        status: "acknowledged",
        request_path: &request_path,
        request_sha256: &request_sha256,
        attempt_namespace: &request.attempt_namespace,
        nonce: &request.nonce,
        boot_id: &second.boot_id,
        systemd_invocation_id: &second.systemd_invocation_id,
        systemd_unit: &request.systemd_unit,
        systemd_manager: &request.systemd_manager,
        systemd_fragment_path: &request.systemd_fragment.path,
        systemd_fragment_sha256: &request.systemd_fragment.sha256,
        systemd_environment_file_path: &request.systemd_environment_file.path,
        systemd_environment_file_sha256: &request.systemd_environment_file.sha256,
        pid: second.main_pid,
        proc_start_time_ticks: second.proc_start_time_ticks,
        acknowledged_at_unix_seconds: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock predates Unix epoch")?
            .as_secs(),
        binary_path: &request.binary.path,
        binary_sha256: &request.binary.sha256,
        argv_sha256: argv_sha256(&request.argv),
        controller_state_sha256: &request.controller.state.sha256,
        checkpoint_broker_pid: request.checkpoint_broker.pid,
        checkpoint_broker_proc_start_time_ticks: request.checkpoint_broker.proc_start_time_ticks,
        source_writers_fenced: second.controller_state.source_writers_fenced,
        target_writers_fenced: second.controller_state.target_writers_fenced,
        fence_held_until_completion: second.controller_state.fence_held_until_completion,
        source_baseline_sha256: &request.source.baseline.sha256,
        pool_topology_sha256: &request.pool.topology.sha256,
        pool_manifest_sha256: hex::encode(second.paths.pool_manifest_sha256),
        source_lmdb_identity: second
            .paths
            .source_lmdb_files
            .authority_identity(&second.paths.source),
        pool_lmdb_identity: second
            .paths
            .pool_lmdb_files
            .authority_identity(&second.paths.pool),
        cursor_value: request.cursor.value.as_deref(),
        cursor_sha256: request.cursor.sha256.as_deref(),
        additional_cas: request
            .cas
            .iter()
            .map(|authority| AcknowledgedCasV3 {
                label: &authority.label,
                sha256: &authority.sha256,
            })
            .collect(),
    };
    let mut ack_bytes =
        serde_json::to_vec(&ack).context("serialize Pool migration launch acknowledgement")?;
    ack_bytes.push(b'\n');
    second.paths.ensure_path_identities()?;
    let final_cursor = validate_cursor_authority(
        &request.cursor,
        &second.paths.cursor_parent,
        &second.paths.cursor_name,
    )?;
    if final_cursor != second.cursor {
        bail!("Pool migration cursor changed immediately before acknowledgement");
    }
    rendezvous
        .acknowledge(&request_bytes, &ack_bytes)
        .with_context(|| {
            format!(
                "durably acknowledge Pool migration launch at {}",
                ack_path.display()
            )
        })?;
    let attempt = rendezvous.into_attempt();
    let acknowledgement_sha256 = sha256_bytes(&ack_bytes);

    println!("Pool migration launch acknowledged: {}", ack_path.display());
    let source = second.paths.source_runtime_path();
    let source_external = second.paths.source_external_runtime_path();
    let pool = second.paths.pool_runtime_path();
    Ok(AcknowledgedPoolMigrationLaunch {
        cursor: second.cursor,
        final_stopped_pass: matches!(
            request.controller.phase.as_str(),
            "final-stopped-source" | "final-stopped-full"
        ),
        final_stopped_source_pass: request.controller.phase == "final-stopped-source",
        final_stopped_full_pass: request.controller.phase == "final-stopped-full",
        source,
        source_external,
        pool,
        controller_state_authority: request.controller.state.clone(),
        controller_state: second.controller_state,
        request: request.clone(),
        request_sha256,
        acknowledgement_sha256,
        checkpoint_state: Mutex::new(CheckpointChainState {
            next_sequence: 0,
            previous_ack_sha256: None,
        }),
        cursor_authority: Mutex::new(request.cursor.clone()),
        attempt,
        pins: second.paths,
        online_target_audit: second.online_target_audit,
    })
}

#[cfg(test)]
pub(super) fn write_durable_pool_migration_cursor(path: &Path, value: &str) -> Result<()> {
    require_absolute(path, "Pool migration cursor")?;
    let parent = path
        .parent()
        .filter(|value| !value.as_os_str().is_empty())
        .context("Pool migration cursor has no parent directory")?;
    let name = path
        .file_name()
        .context("Pool migration cursor has no file name")?;
    let parent = PinnedDirectory::open_exact(parent, "Pool migration cursor parent")?;
    let mut bytes = value.as_bytes().to_vec();
    bytes.push(b'\n');
    parent.durable_replace(name, &bytes, "Pool migration cursor")
}

fn validate_request_shape(
    request: &PoolMigrationLaunchRequestV3,
    request_path: &Path,
) -> Result<()> {
    if request.schema != REQUEST_SCHEMA {
        bail!(
            "unsupported Pool migration launch request schema {}; expected {REQUEST_SCHEMA}",
            request.schema
        );
    }
    require_lower_hex("launch nonce", &request.nonce, 64)?;
    validate_file_identity("v3 attempt namespace", request.attempt_namespace_identity)?;
    validate_file_identity("v3 attempt directory", request.attempt_identity)?;
    require_boot_id("request boot ID", &request.boot_id)?;
    require_lower_hex(
        "request systemd invocation ID",
        &request.systemd_invocation_id,
        32,
    )?;
    require_systemd_service_name(&request.systemd_unit)?;
    if request.systemd_manager != "system" {
        bail!("request systemd manager must be exactly system");
    }
    validate_sha256("systemd unit fragment", &request.systemd_fragment.sha256)?;
    validate_sha256(
        "systemd environment file",
        &request.systemd_environment_file.sha256,
    )?;
    if request.main_pid == 0 {
        bail!("request main PID must be positive");
    }
    if request.proc_start_time_ticks == 0 {
        bail!("request /proc starttime must be positive");
    }
    require_safe_component("controller rollout ID", &request.controller.rollout_id, 128)?;
    require_safe_component("controller phase", &request.controller.phase, 64)?;
    validate_sha256("migration binary", &request.binary.sha256)?;
    validate_sha256(
        "controller executable",
        &request.controller.executable.sha256,
    )?;
    validate_sha256("controller state", &request.controller.state.sha256)?;
    if request.checkpoint_broker.pid == 0
        || request.checkpoint_broker.proc_start_time_ticks == 0
        || request.checkpoint_broker.timeout_seconds == 0
        || request.checkpoint_broker.timeout_seconds > 300
    {
        bail!("checkpoint broker PID/starttime/timeout authority is invalid");
    }
    require_controller_systemd_service_name(&request.checkpoint_broker.systemd_unit)?;
    require_lower_hex(
        "checkpoint broker systemd invocation ID",
        &request.checkpoint_broker.systemd_invocation_id,
        32,
    )?;
    validate_sha256(
        "checkpoint broker systemd fragment",
        &request.checkpoint_broker.systemd_fragment_sha256,
    )?;
    validate_sha256(
        "checkpoint broker systemd environment file",
        &request.checkpoint_broker.systemd_environment_file_sha256,
    )?;
    validate_sha256("source baseline", &request.source.baseline.sha256)?;
    validate_sha256("Pool topology", &request.pool.topology.sha256)?;

    if request.argv.is_empty() {
        bail!("Pool migration launch request argv must not be empty");
    }
    if request.cas.is_empty() {
        bail!("Pool migration launch request requires at least one additional CAS authority");
    }

    let namespace = canonical_directory_path(&request.attempt_namespace, "v3 attempt namespace")?;
    if namespace.file_name().and_then(|value| value.to_str()) != Some(ATTEMPT_NAMESPACE_NAME) {
        bail!("Pool migration attempt namespace must end in {ATTEMPT_NAMESPACE_NAME}");
    }
    let attempt_dir = request_path
        .parent()
        .context("launch request has no attempt directory")?;
    if attempt_dir.parent() != Some(namespace.as_path()) {
        bail!("launch request is not directly beneath its pinned v3 attempt namespace");
    }
    PinnedDirectory::open_exact(&namespace, "v3 attempt namespace")?
        .require_authority_identity(request.attempt_namespace_identity, "v3 attempt namespace")?;
    PinnedDirectory::open_exact(attempt_dir, "Pool migration attempt directory")?
        .require_authority_identity(request.attempt_identity, "v3 attempt directory")?;
    if attempt_dir.file_name().and_then(|value| value.to_str()) != Some(request.nonce.as_str()) {
        bail!("launch request attempt directory does not equal its nonce");
    }
    let rollout_dir = namespace
        .parent()
        .context("v3 attempt namespace has no rollout directory")?;
    if rollout_dir.file_name().and_then(|value| value.to_str())
        != Some(request.controller.rollout_id.as_str())
    {
        bail!("v3 attempt namespace does not belong to the pinned controller rollout");
    }
    let controller_state =
        canonical_regular_path(&request.controller.state.path, "controller state")?;
    if controller_state.parent() != Some(rollout_dir) {
        bail!("controller state is not directly beneath the pinned rollout directory");
    }

    let mut labels = HashSet::new();
    let mut paths = HashSet::new();
    for authority in &request.cas {
        require_safe_component("additional CAS label", &authority.label, 128)?;
        validate_sha256(
            &format!("additional CAS {}", authority.label),
            &authority.sha256,
        )?;
        if !labels.insert(authority.label.as_str()) {
            bail!("duplicate additional CAS label {}", authority.label);
        }
        let canonical = canonical_regular_path(
            &authority.path,
            &format!("additional CAS {}", authority.label),
        )?;
        if !paths.insert(canonical) {
            bail!(
                "multiple additional CAS authorities reference the same path ({})",
                authority.path.display()
            );
        }
    }

    match (
        request.cursor.exists,
        request.cursor.value.as_deref(),
        request.cursor.sha256.as_deref(),
    ) {
        (false, None, None) => {}
        (true, Some(value), Some(sha256)) => {
            validate_cursor_value(value)?;
            validate_sha256("migration cursor", sha256)?;
        }
        _ => bail!(
            "cursor authority must be either absent (exists=false, null value/hash) or a complete present value/hash tuple"
        ),
    }
    validate_lmdb_identity("source LMDB", request.source.lmdb_identity)?;
    validate_lmdb_identity("target Pool catalog", request.pool.lmdb_identity)?;
    match (
        request.source.external_path.as_ref(),
        request.source.external_identity,
    ) {
        (Some(_), Some(identity)) => {
            validate_file_identity("source external directory", identity)?;
        }
        (None, None) => {}
        _ => bail!("source external path and identity must be present or absent together"),
    }
    let final_stopped = matches!(
        request.controller.phase.as_str(),
        "final-stopped-source" | "final-stopped-full"
    );
    if final_stopped != request.source.read_only_mounts.is_some() {
        bail!("stopped-final phase and source read-only mount authority must be present together");
    }
    validate_file_identity("migration cursor parent", request.cursor.parent_identity)?;
    Ok(())
}

fn validate_file_identity(label: &str, identity: FileIdentityV3) -> Result<()> {
    if identity.device == 0 || identity.inode == 0 {
        bail!("{label} device/inode identity must be non-zero");
    }
    Ok(())
}

fn validate_lmdb_identity(label: &str, identity: LmdbIdentityV3) -> Result<()> {
    validate_file_identity(&format!("{label} directory"), identity.directory)?;
    validate_file_identity(&format!("{label} data.mdb"), identity.data)?;
    validate_file_identity(&format!("{label} lock.mdb"), identity.lock)
}

fn validate_launch_authority(
    request: &PoolMigrationLaunchRequestV3,
    context: &PoolMigrationLaunchContext<'_>,
) -> Result<ValidatedLaunch> {
    let mut host_paths = vec![
        (context.launch_request, "Pool migration launch request"),
        (context.source, "source LMDB"),
        (context.pool, "target Pool catalog"),
    ];
    if let Some(path) = context.source_external_dir {
        host_paths.push((path, "source external corpus"));
    }
    require_host_execution_namespace(&host_paths)?;
    validate_pool_migration_release_phase(&request.controller.phase)?;

    match request.controller.phase.as_str() {
        "online-bounded" => {
            if context.max_items.is_none() {
                bail!("online-bounded Pool migration launch requires --max-items");
            }
        }
        "final-stopped-source" | "final-stopped-full" => {
            if context.max_items.is_some() {
                bail!("stopped final Pool migration launch forbids --max-items");
            }
            if request.cursor.exists {
                bail!(
                    "stopped final Pool migration launch requires a fresh absent cursor and full rescan"
                );
            }
        }
        phase => bail!(
            "unsupported Pool migration controller phase {phase}; expected online-bounded, final-stopped-source, or final-stopped-full"
        ),
    }

    let boot_id = current_boot_id()?;
    if request.boot_id != boot_id {
        bail!(
            "Pool migration launch request boot ID {} does not match current boot {}",
            request.boot_id,
            boot_id
        );
    }
    validate_root_broker_service(&request.checkpoint_broker)?;

    let invocation_id = std::env::var(SYSTEMD_INVOCATION_ID_ENV)
        .context("Pool migration launch requires systemd INVOCATION_ID")?;
    require_lower_hex("systemd INVOCATION_ID", &invocation_id, 32)?;
    if request.systemd_invocation_id != invocation_id {
        bail!("Pool migration launch request systemd invocation ID does not match this process");
    }
    let main_pid = std::process::id();
    if request.main_pid != main_pid {
        bail!(
            "Pool migration launch request MainPID {} does not match this process {}",
            request.main_pid,
            main_pid
        );
    }
    let proc_start_time_ticks = current_process_start_time_ticks()?;
    if request.proc_start_time_ticks != proc_start_time_ticks {
        bail!("Pool migration launch request /proc starttime does not match this process");
    }
    validate_systemd_membership(
        &request.systemd_unit,
        &request.systemd_invocation_id,
        request.main_pid,
        &request.systemd_fragment,
        &request.systemd_environment_file,
        &request.binary.path,
    )?;

    let current_exe = std::env::current_exe()
        .context("resolve current Pool migration executable")?
        .canonicalize()
        .context("canonicalize current Pool migration executable")?;
    let requested_exe = canonical_regular_path(&request.binary.path, "migration binary")?;
    if current_exe != requested_exe {
        bail!(
            "Pool migration request binary {} does not match running executable {}",
            requested_exe.display(),
            current_exe.display()
        );
    }
    validate_migration_binary_ownership(&requested_exe)?;
    validate_file_authority(&request.binary, "migration binary")?;
    let running_executable_sha256 = running_executable_sha256(&current_exe)?;
    if running_executable_sha256 != request.binary.sha256 {
        bail!("running /proc/self/exe SHA-256 differs from launch request binary authority");
    }

    let actual_argv = std::env::args_os()
        .map(|argument| {
            argument
                .into_string()
                .map_err(|_| anyhow::anyhow!("Pool migration argv contains non-UTF-8 bytes"))
        })
        .collect::<Result<Vec<_>>>()?;
    if request.argv != actual_argv {
        bail!("Pool migration launch request argv does not match this process exactly");
    }
    if request.argv.first().map(String::as_str) != request.binary.path.to_str() {
        bail!("Pool migration argv[0] does not equal the exact pinned binary path");
    }

    validate_file_authority(&request.controller.executable, "controller executable")?;
    validate_file_authority(
        &FileAuthorityV3 {
            path: request.checkpoint_broker.systemd_fragment_path.clone(),
            sha256: request.checkpoint_broker.systemd_fragment_sha256.clone(),
        },
        "checkpoint broker systemd fragment",
    )?;
    validate_file_authority(
        &FileAuthorityV3 {
            path: request
                .checkpoint_broker
                .systemd_environment_file_path
                .clone(),
            sha256: request
                .checkpoint_broker
                .systemd_environment_file_sha256
                .clone(),
        },
        "checkpoint broker systemd environment file",
    )?;
    validate_file_authority(&request.systemd_fragment, "systemd unit fragment")?;
    validate_file_authority(
        &request.systemd_environment_file,
        "systemd environment file",
    )?;
    validate_file_authority(&request.controller.state, "controller state")?;
    validate_file_authority(&request.source.baseline, "source baseline")?;
    validate_file_authority(&request.pool.topology, "Pool topology")?;
    for authority in &request.cas {
        validate_named_file_authority(authority)?;
    }
    let pool_topology = pin_pool_topology(&request.pool.topology, &request.pool.path)?;
    let mut topology_paths = Vec::with_capacity(pool_topology.topology.members.len() * 2);
    for member in &pool_topology.topology.members {
        topology_paths.push((member.path.as_path(), "target Pool member LMDB"));
        if let Some(path) = member.external_path.as_deref() {
            topology_paths.push((path, "target Pool member external corpus"));
        }
    }
    require_host_execution_namespace(&topology_paths)?;

    let requested_source =
        canonical_directory_path(&request.source.lmdb_path, "requested source LMDB")?;
    let actual_source = canonical_directory_path(context.source, "source LMDB")?;
    if requested_source != actual_source {
        bail!("Pool migration source LMDB differs from launch request authority");
    }
    let source = PinnedDirectory::open_exact(&actual_source, "source LMDB")?;
    let source_lmdb_files = PinnedLmdbFiles::pin(&source, "source LMDB")?;
    source_lmdb_files.require_authority_identity(
        &source,
        request.source.lmdb_identity,
        "source LMDB",
    )?;

    let requested_external = request
        .source
        .external_path
        .as_deref()
        .map(|path| canonical_directory_path(path, "requested source external directory"))
        .transpose()?;
    let actual_external = context
        .source_external_dir
        .map(|path| canonical_directory_path(path, "source external directory"))
        .transpose()?;
    if requested_external != actual_external {
        bail!("Pool migration source external directory differs from launch request authority");
    }
    let source_external = actual_external
        .as_deref()
        .map(|path| PinnedDirectory::open_exact(path, "source external directory"))
        .transpose()?;
    match (&source_external, request.source.external_identity) {
        (Some(directory), Some(identity)) => {
            directory.require_authority_identity(identity, "source external directory")?;
        }
        (None, None) => {}
        _ => bail!("source external directory authority is incomplete"),
    }
    if let Some(mounts) = &request.source.read_only_mounts {
        validate_source_read_only_mount_authority(
            mounts,
            &request.source.lmdb_path,
            request.source.lmdb_identity,
            request.source.external_path.as_deref(),
            request.source.external_identity,
        )
        .context("validate launch source read-only mount authority")?;
    }

    let requested_pool = canonical_directory_path(&request.pool.path, "requested Pool")?;
    let actual_pool = canonical_directory_path(context.pool, "Pool")?;
    if requested_pool != actual_pool {
        bail!("Pool migration target Pool differs from launch request authority");
    }
    let pool = PinnedDirectory::open_exact(&actual_pool, "target Pool")?;
    let pool_lmdb_files = PinnedLmdbFiles::pin(&pool, "target Pool catalog")?;
    pool_lmdb_files.require_authority_identity(
        &pool,
        request.pool.lmdb_identity,
        "target Pool catalog",
    )?;

    let actual_cursor_path = canonical_or_absent_path(context.state_file, "migration cursor")?;
    let requested_cursor_path =
        canonical_or_absent_path(&request.cursor.path, "requested migration cursor")?;
    if requested_cursor_path != actual_cursor_path {
        bail!("Pool migration cursor path differs from launch request authority");
    }
    let cursor_parent_path = actual_cursor_path
        .parent()
        .filter(|value| !value.as_os_str().is_empty())
        .context("migration cursor has no parent directory")?;
    let cursor_name = actual_cursor_path
        .file_name()
        .context("migration cursor has no file name")?
        .to_os_string();
    let cursor_parent = PinnedDirectory::open_exact(cursor_parent_path, "migration cursor parent")?;
    cursor_parent
        .require_authority_identity(request.cursor.parent_identity, "migration cursor parent")?;

    let cursor = validate_cursor_authority(&request.cursor, &cursor_parent, &cursor_name)?;
    let PinnedPoolTopology {
        manifest_sha256,
        topology,
        members,
    } = pool_topology;
    let mut paths = PinnedMigrationPaths {
        source,
        source_lmdb_files,
        source_external,
        pool,
        pool_lmdb_files,
        pool_manifest_sha256: manifest_sha256,
        pool_topology: topology,
        pool_members: members,
        prior_sources: Vec::new(),
        cursor_parent,
        cursor_name,
    };
    let controller_state = validate_controller_state(request, &paths, &boot_id)?;
    let online_target_audit = load_validated_online_target_audit(
        &request.cas,
        &OnlineTargetAuditExpectationV3 {
            rollout_id: &request.controller.rollout_id,
            source_path: &request.source.lmdb_path,
            source_lmdb_identity: request.source.lmdb_identity,
            source_external_path: request.source.external_path.as_deref(),
            source_external_identity: request.source.external_identity,
            pool_path: &request.pool.path,
            pool_lmdb_identity: request.pool.lmdb_identity,
            pool_topology_sha256: &request.pool.topology.sha256,
            pool_manifest_sha256: &controller_state.pool_manifest_sha256,
            expected_service_gid: service_gid()
                .context("online target audit validation requires a service GID")?,
            validate_evidence_content: false,
        },
    )?;
    match request.controller.phase.as_str() {
        "final-stopped-source" if online_target_audit.is_none() => {
            bail!("final-stopped-source requires one root-certified online target audit CAS")
        }
        "online-bounded" | "final-stopped-full" if online_target_audit.is_some() => {
            bail!("online target audit CAS is accepted only by final-stopped-source")
        }
        _ => {}
    }
    let prior_receipts =
        load_request_source_terminal_receipts(request, &controller_state, &paths, false)?;
    paths.prior_sources = pin_prior_source_paths(&prior_receipts)?;
    if request.controller.phase == "final-stopped-full" {
        let matching_current = paths
            .prior_sources
            .iter()
            .filter(|prior| {
                prior.directory.same_object(&paths.source)
                    && prior.lmdb_files.same_objects(&paths.source_lmdb_files)
                    && match (&prior.external_directory, &paths.source_external) {
                        (Some(left), Some(right)) => left.same_object(right),
                        (None, None) => true,
                        _ => false,
                    }
            })
            .count();
        if matching_current != 1 {
            bail!(
                "final-stopped-full current source must be exactly one validated receipt-owned source"
            );
        }
    }
    paths.ensure_isolated_authority_roots(request, context.launch_request)?;
    Ok(ValidatedLaunch {
        cursor,
        boot_id,
        systemd_invocation_id: invocation_id,
        main_pid,
        proc_start_time_ticks,
        controller_state,
        online_target_audit,
        paths,
    })
}

fn validate_controller_state(
    request: &PoolMigrationLaunchRequestV3,
    paths: &PinnedMigrationPaths,
    boot_id: &str,
) -> Result<ControllerStateV3> {
    let state_path = canonical_regular_path(&request.controller.state.path, "controller state")?;
    validate_controller_state_ownership(&state_path)?;
    let mut state_file = open_regular_file(&state_path, "controller state")?;
    let state_bytes = read_bounded_open_file(
        &mut state_file,
        MAX_CONTROLLER_STATE_BYTES,
        "controller state",
        &state_path,
    )?;
    if sha256_bytes(&state_bytes) != request.controller.state.sha256 {
        bail!("controller state bytes changed after launch-request CAS validation");
    }
    let state: ControllerStateV3 = serde_json::from_slice(&state_bytes)
        .context("parse strict Pool migration controller state")?;
    if state.schema != CONTROLLER_STATE_SCHEMA {
        bail!(
            "unsupported Pool migration controller state schema {}; expected {CONTROLLER_STATE_SCHEMA}",
            state.schema
        );
    }
    if state.rollout_id != request.controller.rollout_id
        || state.phase != request.controller.phase
        || state.boot_id != boot_id
    {
        bail!("Pool migration controller state does not bind this rollout, phase, and boot");
    }
    if state.source_lmdb_identity != request.source.lmdb_identity
        || state.source_external_identity != request.source.external_identity
        || state.pool_lmdb_identity != request.pool.lmdb_identity
        || state.source_lmdb_identity != paths.source_lmdb_files.authority_identity(&paths.source)
        || state.pool_lmdb_identity != paths.pool_lmdb_files.authority_identity(&paths.pool)
    {
        bail!("Pool migration controller state does not bind the exact source and target LMDB identities");
    }
    let expected_manifest_sha256 = hex::encode(paths.pool_manifest_sha256);
    if state.pool_manifest_sha256 != expected_manifest_sha256 {
        bail!("Pool migration controller state does not bind the exact Pool manifest");
    }
    if state.pool_topology_sha256 != request.pool.topology.sha256 {
        bail!("Pool migration controller state does not bind the exact Pool topology CAS");
    }
    let mut previous_unit: Option<&str> = None;
    for unit in &state.stopped_writer_units {
        require_writer_systemd_service_name(unit)?;
        if previous_unit.is_some_and(|previous| previous >= unit.as_str()) {
            bail!("controller stopped writer units must be uniquely sorted");
        }
        previous_unit = Some(unit);
    }
    let mut previous_receipt: Option<&str> = None;
    if state.source_terminal_receipt_sha256.len() > MAX_FINAL_SOURCE_RECEIPTS {
        bail!(
            "controller source-terminal receipt set exceeds the hard maximum of {MAX_FINAL_SOURCE_RECEIPTS}"
        );
    }
    for sha256 in &state.source_terminal_receipt_sha256 {
        validate_sha256("source-terminal receipt", sha256)?;
        if previous_receipt.is_some_and(|previous| previous >= sha256.as_str()) {
            bail!("controller source-terminal receipt SHA-256 set must be uniquely sorted");
        }
        previous_receipt = Some(sha256);
    }
    if request.controller.phase != "final-stopped-full"
        && !state.source_terminal_receipt_sha256.is_empty()
    {
        bail!("only final-stopped-full may consume source-terminal receipts");
    }
    let final_stopped = matches!(
        request.controller.phase.as_str(),
        "final-stopped-source" | "final-stopped-full"
    );
    if final_stopped
        && (!state.source_writers_fenced
            || !state.fence_held_until_completion
            || state.source_writer_processes_with_open_handles != 0
            || state.stopped_writer_units.is_empty()
            || state.writer_unit_masks.is_empty())
    {
        bail!(
            "stopped final controller state must attest its source writer fence held through completion, zero source writers holding store handles, and the exact stopped systemd writer units"
        );
    }
    if request.controller.phase == "final-stopped-full"
        && (!state.target_writers_fenced || state.target_writer_processes_with_open_handles != 0)
    {
        bail!(
            "final-stopped-full controller state must additionally attest the target writer fence and zero target writers holding store handles"
        );
    }
    if final_stopped {
        validate_runtime_masked_writer_units(
            &state.stopped_writer_units,
            &state.writer_unit_masks,
        )?;
    } else if !state.writer_unit_masks.is_empty() {
        bail!("online-bounded controller state must not claim final runtime writer masks");
    }
    validate_legacy_worker_activation_fence(
        &state.legacy_worker_template_mask,
        &state.legacy_worker_instance_masks,
    )?;
    Ok(state)
}

#[cfg(target_os = "linux")]
fn validate_controller_state_ownership(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspect controller state {}", path.display()))?;
    if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
        bail!("controller state must be root-owned and not group/world writable");
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn validate_controller_state_ownership(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn trusted_systemctl_path() -> Result<&'static Path> {
    let systemctl = Path::new("/usr/bin/systemctl");
    let metadata = std::fs::symlink_metadata(systemctl).context("inspect /usr/bin/systemctl")?;
    if !metadata.file_type().is_file() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
        bail!("/usr/bin/systemctl is not a trusted root-owned non-writable regular file");
    }
    Ok(systemctl)
}

#[cfg(any(target_os = "linux", test))]
fn parse_systemd_properties<'a>(output: &'a str, label: &str) -> Result<HashMap<&'a str, &'a str>> {
    let mut properties = HashMap::new();
    for line in output.lines() {
        let (name, value) = line
            .split_once('=')
            .with_context(|| format!("{label} contains a malformed property line"))?;
        if name.is_empty() || properties.insert(name, value).is_some() {
            bail!("{label} contains an empty or duplicate property name");
        }
    }
    Ok(properties)
}

#[cfg(any(target_os = "linux", test))]
fn parse_systemd_unit_property_blocks<'a>(
    output: &'a str,
    label: &str,
) -> Result<HashMap<&'a str, HashMap<&'a str, &'a str>>> {
    let mut units = HashMap::new();
    for block in output
        .split("\n\n")
        .filter(|block| !block.trim().is_empty())
    {
        let properties = parse_systemd_properties(block, label)?;
        let unit = properties
            .get("Id")
            .copied()
            .with_context(|| format!("{label} block omits its exact Id property"))?;
        if unit.is_empty() || units.insert(unit, properties).is_some() {
            bail!("{label} contains an empty or duplicate unit Id");
        }
    }
    if units.is_empty() {
        bail!("{label} contains no unit property blocks");
    }
    Ok(units)
}

#[cfg(any(target_os = "linux", test))]
fn require_empty_systemd_properties(
    properties: &HashMap<&str, &str>,
    names: &[&str],
    label: &str,
) -> Result<()> {
    for name in names {
        if properties.get(name).copied() != Some("") {
            bail!("{label} must have an explicit empty {name} property");
        }
    }
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
fn reject_nonempty_systemd_properties(
    properties: &HashMap<&str, &str>,
    names: &[&str],
    label: &str,
) -> Result<()> {
    for name in names {
        if properties.get(name).is_some_and(|value| !value.is_empty()) {
            bail!("{label} must not have a nonempty {name} property");
        }
    }
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
fn validate_runtime_masked_writer_property_map(
    unit: &str,
    mask: &WriterUnitMaskV3,
    properties: &HashMap<&str, &str>,
) -> Result<()> {
    if properties.get("LoadState").copied() != Some("masked")
        || properties.get("UnitFileState").copied() != Some("masked-runtime")
        || properties.get("ActiveState").copied() != Some("inactive")
        || properties.get("SubState").copied() != Some("dead")
        || properties.get("MainPID").copied() != Some("0")
        || properties.get("ControlPID").copied() != Some("0")
        || properties.get("Job").copied() != Some("")
        || properties.get("NeedDaemonReload").copied() != Some("no")
        || properties.get("FragmentPath").copied() != mask.path.to_str()
    {
        bail!(
            "writer unit {unit} is not runtime-masked by its exact authority, inactive/dead, process-free, job-free, and daemon-reloaded"
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(super) fn validate_runtime_masked_writer_owned_properties(
    unit: &str,
    mask: &WriterUnitMaskV3,
    properties: &HashMap<String, String>,
) -> Result<()> {
    let borrowed = properties
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect::<HashMap<_, _>>();
    validate_runtime_masked_writer_property_map(unit, mask, &borrowed)
}

#[cfg(target_os = "linux")]
pub(super) fn validate_runtime_writer_mask_authorities(
    units: &[String],
    masks: &[WriterUnitMaskV3],
) -> Result<()> {
    if units.is_empty() || units.len() != masks.len() {
        bail!("final writer units and runtime-mask authorities must be nonempty and one-to-one");
    }
    validate_runtime_mask_directory()?;
    for (unit, mask) in units.iter().zip(masks) {
        require_writer_systemd_service_name(unit)?;
        validate_runtime_mask_authority(unit, mask, "writer")?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_runtime_mask_directory() -> Result<()> {
    let runtime_dir = Path::new("/run/systemd/system");
    let runtime_metadata =
        std::fs::symlink_metadata(runtime_dir).context("inspect systemd runtime unit directory")?;
    if !runtime_metadata.file_type().is_dir()
        || runtime_metadata.uid() != 0
        || runtime_metadata.mode() & 0o022 != 0
    {
        bail!("/run/systemd/system must be a root-owned non-writable directory");
    }
    let canonical_runtime = runtime_dir
        .canonicalize()
        .context("canonicalize systemd runtime unit directory")?;
    if canonical_runtime != runtime_dir {
        bail!("/run/systemd/system must be an exact canonical directory");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_runtime_mask_authority(unit: &str, mask: &WriterUnitMaskV3, class: &str) -> Result<()> {
    if mask.unit != unit {
        bail!("runtime {class}-mask authority does not bind the expected unit {unit}");
    }
    let expected_path = Path::new("/run/systemd/system").join(unit);
    if mask.path != expected_path {
        bail!(
            "runtime {class} mask for {unit} must be exactly {}",
            expected_path.display()
        );
    }
    if mask.target != Path::new("/dev/null") {
        bail!("runtime {class} mask for {unit} must target exactly /dev/null");
    }
    validate_file_identity(&format!("runtime {class} mask {unit}"), mask.identity)?;
    let metadata = std::fs::symlink_metadata(&mask.path)
        .with_context(|| format!("inspect runtime {class} mask {}", mask.path.display()))?;
    if !metadata.file_type().is_symlink()
        || metadata.uid() != 0
        || metadata.gid() != 0
        || metadata.nlink() != 1
    {
        bail!("runtime {class} mask for {unit} must be a root:root single-link symbolic link");
    }
    let identity = FileIdentityV3 {
        device: metadata.dev(),
        inode: metadata.ino(),
    };
    if identity != mask.identity {
        bail!("runtime {class} mask identity changed for {unit}");
    }
    let target = std::fs::read_link(&mask.path)
        .with_context(|| format!("read runtime {class} mask {}", mask.path.display()))?;
    if target != mask.target {
        bail!("runtime {class} mask target changed for {unit}");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(super) fn validate_legacy_worker_mask_authorities(
    template: &WriterUnitMaskV3,
    instances: &[WriterUnitMaskV3],
) -> Result<()> {
    const TEMPLATE: &str = "hashtree-pool-migrate@.service";
    validate_runtime_mask_directory()?;
    validate_runtime_mask_authority(TEMPLATE, template, "legacy migration-worker template")?;
    let mut previous: Option<&str> = None;
    for mask in instances {
        require_legacy_worker_instance_name(&mask.unit)?;
        if previous.is_some_and(|unit| unit >= mask.unit.as_str()) {
            bail!("legacy migration-worker instance masks must be uniquely sorted by unit");
        }
        previous = Some(&mask.unit);
        validate_runtime_mask_authority(&mask.unit, mask, "legacy migration-worker instance")?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn require_legacy_worker_instance_name(unit: &str) -> Result<()> {
    if unit.len() > 255
        || !unit.starts_with("hashtree-pool-migrate@")
        || !unit.ends_with(".service")
        || unit == "hashtree-pool-migrate@.service"
        || unit.contains('/')
        || !unit.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'_' | b'.' | b'@' | b'\\' | b'-')
        })
    {
        bail!("legacy migration worker must be an exact hashtree-pool-migrate@*.service instance");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn list_loaded_legacy_worker_instances(systemctl: &Path) -> Result<Vec<String>> {
    let output = std::process::Command::new(systemctl)
        .env_clear()
        .env("LANG", "C")
        .args([
            "--system",
            "--no-pager",
            "--plain",
            "--no-legend",
            "list-units",
            "--all",
            "--type=service",
            "hashtree-pool-migrate@*.service",
        ])
        .output()
        .context("enumerate loaded legacy Pool migration worker instances")?;
    if !output.status.success() {
        bail!(
            "systemctl could not enumerate loaded legacy Pool migration workers: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let stdout =
        String::from_utf8(output.stdout).context("legacy worker unit list is not UTF-8")?;
    let mut units = Vec::new();
    for line in stdout.lines() {
        let unit = line
            .split_ascii_whitespace()
            .next()
            .context("systemctl emitted an empty legacy worker unit row")?;
        require_legacy_worker_instance_name(unit)?;
        units.push(unit.to_string());
    }
    units.sort();
    if units.windows(2).any(|pair| pair[0] == pair[1]) {
        bail!("systemctl returned a duplicate loaded legacy migration-worker instance");
    }
    Ok(units)
}

#[cfg(target_os = "linux")]
fn list_runtime_legacy_worker_instance_masks() -> Result<Vec<String>> {
    let runtime_dir = Path::new("/run/systemd/system");
    let mut units = Vec::new();
    for entry in std::fs::read_dir(runtime_dir)
        .context("enumerate runtime systemd legacy migration-worker masks")?
    {
        let entry = entry.context("read runtime systemd directory entry")?;
        let name = entry.file_name();
        let bytes = name.as_bytes();
        if !bytes.starts_with(b"hashtree-pool-migrate@")
            || !bytes.ends_with(b".service")
            || bytes == b"hashtree-pool-migrate@.service"
        {
            continue;
        }
        let unit = name
            .into_string()
            .map_err(|_| anyhow::anyhow!("legacy migration-worker mask name is not UTF-8"))?;
        require_legacy_worker_instance_name(&unit)?;
        units.push(unit);
    }
    units.sort();
    if units.windows(2).any(|pair| pair[0] == pair[1]) {
        bail!("runtime systemd directory contains duplicate legacy worker mask names");
    }
    Ok(units)
}

#[cfg(target_os = "linux")]
pub(super) fn validate_legacy_worker_activation_fence_with_systemctl(
    systemctl: &Path,
    template: &WriterUnitMaskV3,
    instances: &[WriterUnitMaskV3],
) -> Result<()> {
    validate_legacy_worker_mask_authorities(template, instances)?;
    let expected = instances
        .iter()
        .map(|mask| mask.unit.clone())
        .collect::<Vec<_>>();
    if list_runtime_legacy_worker_instance_masks()? != expected {
        bail!(
            "legacy migration-worker authority does not exactly cover every runtime instance mask"
        );
    }
    if !list_loaded_legacy_worker_instances(systemctl)?.is_empty() {
        bail!("a legacy migration-worker instance remains loaded despite the exact runtime masks");
    }
    for mask in instances {
        let output = std::process::Command::new(systemctl)
            .env_clear()
            .env("LANG", "C")
            .args([
                "--system",
                "--no-pager",
                "show",
                &mask.unit,
                "--property",
                "LoadState",
                "--property",
                "UnitFileState",
                "--property",
                "ActiveState",
                "--property",
                "SubState",
                "--property",
                "MainPID",
                "--property",
                "ControlPID",
                "--property",
                "Job",
                "--property",
                "NeedDaemonReload",
                "--property",
                "FragmentPath",
            ])
            .output()
            .with_context(|| {
                format!(
                    "inspect runtime-masked legacy migration worker {}",
                    mask.unit
                )
            })?;
        if !output.status.success() {
            bail!(
                "systemctl could not verify runtime-masked legacy migration worker {}: {}",
                mask.unit,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let stdout = String::from_utf8(output.stdout).with_context(|| {
            format!(
                "decode runtime-masked legacy migration worker {} properties",
                mask.unit
            )
        })?;
        let properties = parse_systemd_properties(
            &stdout,
            &format!("runtime-masked legacy migration worker {}", mask.unit),
        )?;
        validate_runtime_masked_writer_property_map(&mask.unit, mask, &properties)?;
    }
    if !list_loaded_legacy_worker_instances(systemctl)?.is_empty() {
        bail!("a legacy migration-worker instance became loaded during fence validation");
    }
    if list_runtime_legacy_worker_instance_masks()? != expected {
        bail!("legacy migration-worker runtime instance-mask set changed during validation");
    }
    validate_legacy_worker_mask_authorities(template, instances)
}

#[cfg(target_os = "linux")]
fn validate_legacy_worker_activation_fence(
    template: &WriterUnitMaskV3,
    instances: &[WriterUnitMaskV3],
) -> Result<()> {
    validate_legacy_worker_activation_fence_with_systemctl(
        trusted_systemctl_path()?,
        template,
        instances,
    )
}

#[cfg(target_os = "linux")]
pub(super) fn validate_runtime_masked_writer_units_with_systemctl(
    systemctl: &Path,
    units: &[String],
    masks: &[WriterUnitMaskV3],
) -> Result<()> {
    validate_runtime_writer_mask_authorities(units, masks)?;
    let mut command = std::process::Command::new(systemctl);
    command
        .env_clear()
        .env("LANG", "C")
        .args(["--system", "--no-pager", "show"])
        .args(units)
        .args([
            "--property",
            "Id",
            "--property",
            "LoadState",
            "--property",
            "UnitFileState",
            "--property",
            "ActiveState",
            "--property",
            "SubState",
            "--property",
            "MainPID",
            "--property",
            "ControlPID",
            "--property",
            "Job",
            "--property",
            "NeedDaemonReload",
            "--property",
            "FragmentPath",
        ]);
    let output = command
        .output()
        .context("inspect all runtime-masked writer units in one systemctl query")?;
    if !output.status.success() {
        bail!(
            "systemctl could not verify the runtime-masked writer-unit set: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let stdout = String::from_utf8(output.stdout)
        .context("decode batched runtime-masked writer-unit properties")?;
    let mut unit_properties =
        parse_systemd_unit_property_blocks(&stdout, "batched runtime-masked writer-unit output")?;
    for (unit, mask) in units.iter().zip(masks) {
        let properties = unit_properties
            .remove(unit.as_str())
            .with_context(|| format!("systemctl omitted runtime-masked writer unit {unit}"))?;
        validate_runtime_masked_writer_property_map(unit, mask, &properties)?;
    }
    if !unit_properties.is_empty() {
        bail!("systemctl returned an unrequested runtime-masked writer unit");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(super) fn validate_batched_runtime_masked_final_fence_with_systemctl(
    systemctl: &Path,
    writer_units: &[String],
    writer_masks: &[WriterUnitMaskV3],
    legacy_template: &WriterUnitMaskV3,
    legacy_instances: &[WriterUnitMaskV3],
) -> Result<()> {
    validate_runtime_writer_mask_authorities(writer_units, writer_masks)?;
    validate_legacy_worker_mask_authorities(legacy_template, legacy_instances)?;
    let expected_legacy_instances = legacy_instances
        .iter()
        .map(|mask| mask.unit.clone())
        .collect::<Vec<_>>();
    if list_runtime_legacy_worker_instance_masks()? != expected_legacy_instances {
        bail!(
            "legacy migration-worker authority does not exactly cover every runtime instance mask"
        );
    }

    let mut units = writer_units.to_vec();
    units.push(legacy_template.unit.clone());
    units.extend(legacy_instances.iter().map(|mask| mask.unit.clone()));
    let mut command = std::process::Command::new(systemctl);
    command
        .env_clear()
        .env("LANG", "C")
        .args(["--system", "--no-pager", "show"])
        .args(&units)
        .args([
            "--property",
            "Id",
            "--property",
            "LoadState",
            "--property",
            "UnitFileState",
            "--property",
            "ActiveState",
            "--property",
            "SubState",
            "--property",
            "MainPID",
            "--property",
            "ControlPID",
            "--property",
            "Job",
            "--property",
            "NeedDaemonReload",
            "--property",
            "FragmentPath",
        ]);
    let output = command
        .output()
        .context("inspect the complete runtime-masked final fence in one systemctl query")?;
    if !output.status.success() {
        bail!(
            "systemctl could not verify the complete runtime-masked final fence: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let stdout = String::from_utf8(output.stdout)
        .context("decode batched runtime-masked final-fence properties")?;
    let mut unit_properties =
        parse_systemd_unit_property_blocks(&stdout, "batched runtime-masked final-fence output")?;
    for (unit, mask) in writer_units.iter().zip(writer_masks) {
        let properties = unit_properties
            .remove(unit.as_str())
            .with_context(|| format!("systemctl omitted runtime-masked writer unit {unit}"))?;
        validate_runtime_masked_writer_property_map(unit, mask, &properties)?;
    }
    for mask in std::iter::once(legacy_template).chain(legacy_instances) {
        let properties = unit_properties
            .remove(mask.unit.as_str())
            .with_context(|| {
                format!(
                    "systemctl omitted runtime-masked legacy migration worker {}",
                    mask.unit
                )
            })?;
        validate_runtime_masked_writer_property_map(&mask.unit, mask, &properties)?;
    }
    if !unit_properties.is_empty() {
        bail!("systemctl returned an unrequested runtime-masked final-fence unit");
    }

    validate_runtime_writer_mask_authorities(writer_units, writer_masks)?;
    validate_legacy_worker_mask_authorities(legacy_template, legacy_instances)?;
    if list_runtime_legacy_worker_instance_masks()? != expected_legacy_instances {
        bail!("legacy migration-worker runtime instance-mask set changed during validation");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_batched_runtime_masked_final_fence(
    writer_units: &[String],
    writer_masks: &[WriterUnitMaskV3],
    legacy_template: &WriterUnitMaskV3,
    legacy_instances: &[WriterUnitMaskV3],
) -> Result<()> {
    validate_batched_runtime_masked_final_fence_with_systemctl(
        trusted_systemctl_path()?,
        writer_units,
        writer_masks,
        legacy_template,
        legacy_instances,
    )
}

#[cfg(not(target_os = "linux"))]
fn validate_batched_runtime_masked_final_fence(
    _writer_units: &[String],
    _writer_masks: &[WriterUnitMaskV3],
    _legacy_template: &WriterUnitMaskV3,
    _legacy_instances: &[WriterUnitMaskV3],
) -> Result<()> {
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub(super) fn validate_batched_runtime_masked_final_fence_with_systemctl(
    _systemctl: &Path,
    _writer_units: &[String],
    _writer_masks: &[WriterUnitMaskV3],
    _legacy_template: &WriterUnitMaskV3,
    _legacy_instances: &[WriterUnitMaskV3],
) -> Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_runtime_masked_writer_units(
    units: &[String],
    masks: &[WriterUnitMaskV3],
) -> Result<()> {
    let systemctl = trusted_systemctl_path()?;
    validate_runtime_masked_writer_units_with_systemctl(systemctl, units, masks)
}

#[cfg(not(target_os = "linux"))]
fn validate_runtime_masked_writer_units(
    _units: &[String],
    _masks: &[WriterUnitMaskV3],
) -> Result<()> {
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn validate_runtime_writer_mask_authorities(
    _units: &[String],
    _masks: &[WriterUnitMaskV3],
) -> Result<()> {
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn validate_legacy_worker_mask_authorities(
    _template: &WriterUnitMaskV3,
    _instances: &[WriterUnitMaskV3],
) -> Result<()> {
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn validate_legacy_worker_activation_fence(
    _template: &WriterUnitMaskV3,
    _instances: &[WriterUnitMaskV3],
) -> Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_migration_binary_ownership(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspect migration binary {}", path.display()))?;
    if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
        bail!("migration binary must be root-owned and not group/world writable");
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn validate_migration_binary_ownership(_path: &Path) -> Result<()> {
    Ok(())
}

fn wait_for_launch_request(path: &Path, wait: Duration) -> Result<LaunchRendezvous> {
    if wait.is_zero() || wait > Duration::from_secs(300) {
        bail!("Pool migration launch request wait must be between 1 and 300 seconds");
    }
    let attempt = validate_pending_request_location(path)?;
    let mut start_bytes = serde_json::to_vec(&PoolMigrationLaunchStartV3 {
        schema: START_SCHEMA,
        status: "started",
        pid: std::process::id(),
        started_at_unix_seconds: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock predates Unix epoch")?
            .as_secs(),
    })
    .context("serialize Pool migration launch start claim")?;
    start_bytes.push(b'\n');
    attempt.create_durable_exclusive(
        OsStr::new(START_FILE_NAME),
        &start_bytes,
        "Pool migration launch start claim",
    )?;
    let started = Instant::now();
    loop {
        if let Some(request) =
            attempt.open_regular_optional(OsStr::new(REQUEST_FILE_NAME), "launch request")?
        {
            let request_snapshot = request
                .metadata()
                .context("inspect Pool migration launch request")?;
            validate_launch_request_ownership(&request_snapshot)?;
            return Ok(LaunchRendezvous {
                attempt,
                request,
                request_snapshot,
                request_path: path.to_path_buf(),
            });
        }
        if attempt.entry_exists(
            OsStr::new(ACK_FILE_NAME),
            "Pool migration launch acknowledgement",
        )? {
            bail!(
                "Pool migration launch acknowledgement exists before its request; create a fresh {ATTEMPT_NAMESPACE_NAME} nonce"
            );
        }
        if started.elapsed() >= wait {
            bail!(
                "timed out after {} seconds waiting for Pool migration launch request {}",
                wait.as_secs(),
                path.display()
            );
        }
        thread::sleep(Duration::from_millis(25));
        attempt.ensure_path_identity("Pool migration attempt directory")?;
    }
}

fn validate_pending_request_location(path: &Path) -> Result<PinnedDirectory> {
    require_absolute(path, "Pool migration launch request")?;
    if path.file_name().and_then(|value| value.to_str()) != Some(REQUEST_FILE_NAME) {
        bail!("Pool migration launch request must be named {REQUEST_FILE_NAME}");
    }
    let attempt_dir = path
        .parent()
        .context("launch request has no attempt directory")?;
    let attempt = PinnedDirectory::open_exact(attempt_dir, "Pool migration attempt directory")?;
    let namespace = attempt_dir
        .parent()
        .context("launch request has no v3 attempt namespace")?;
    if namespace.file_name().and_then(|value| value.to_str()) != Some(ATTEMPT_NAMESPACE_NAME) {
        bail!(
            "Pool migration launch request must live beneath an {ATTEMPT_NAMESPACE_NAME} namespace"
        );
    }
    let nonce = attempt_dir
        .file_name()
        .and_then(|value| value.to_str())
        .context("Pool migration attempt directory nonce is not UTF-8")?;
    require_lower_hex("Pool migration attempt directory nonce", nonce, 64)?;
    validate_attempt_namespace_ownership(namespace, &attempt)?;
    if attempt.entry_exists(
        OsStr::new(ACK_FILE_NAME),
        "Pool migration launch acknowledgement",
    )? {
        bail!(
            "Pool migration launch acknowledgement already exists; create a fresh {ATTEMPT_NAMESPACE_NAME} nonce"
        );
    }
    if attempt.entry_exists(
        OsStr::new(START_FILE_NAME),
        "Pool migration launch start claim",
    )? {
        bail!(
            "Pool migration launch start claim already exists; create a fresh {ATTEMPT_NAMESPACE_NAME} nonce"
        );
    }
    if attempt.entry_exists(
        OsStr::new(TERMINAL_AUDIT_FILE_NAME),
        "terminal Pool audit receipt",
    )? {
        bail!(
            "terminal Pool audit receipt already exists; create a fresh {ATTEMPT_NAMESPACE_NAME} nonce"
        );
    }
    if attempt.entry_exists(
        OsStr::new(SOURCE_TERMINAL_FILE_NAME),
        "source-terminal receipt",
    )? {
        bail!(
            "source-terminal receipt already exists; create a fresh {ATTEMPT_NAMESPACE_NAME} nonce"
        );
    }
    Ok(attempt)
}

#[cfg(target_os = "linux")]
fn validate_attempt_namespace_ownership(namespace: &Path, attempt: &PinnedDirectory) -> Result<()> {
    let namespace = std::fs::symlink_metadata(namespace)
        .context("inspect Pool migration v3 attempt namespace")?;
    if !namespace.file_type().is_dir() || namespace.uid() != 0 || namespace.mode() & 0o022 != 0 {
        bail!("Pool migration attempts-v3 namespace must be a root-owned non-writable directory");
    }
    let metadata = attempt.metadata("Pool migration attempt directory ownership")?;
    if metadata.uid() != 0
        || metadata.gid() != unsafe { libc::getegid() }
        || metadata.mode() & libc::S_ISVTX == 0
        || metadata.mode() & 0o030 != 0o030
        || metadata.mode() & 0o007 != 0
    {
        bail!(
            "Pool migration attempt directory must be root-owned, owned by the service group, sticky, group-writable/searchable, and inaccessible to others"
        );
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn validate_attempt_namespace_ownership(
    _namespace: &Path,
    _attempt: &PinnedDirectory,
) -> Result<()> {
    Ok(())
}

fn validate_durable_lmdb_environment() -> Result<()> {
    for variable in [
        "LD_PRELOAD",
        "LD_AUDIT",
        "LD_LIBRARY_PATH",
        "DYLD_INSERT_LIBRARIES",
        "DYLD_LIBRARY_PATH",
        "HTREE_LMDB_NO_SYNC",
        "HTREE_LMDB_NO_META_SYNC",
    ] {
        if std::env::var_os(variable).is_some() {
            bail!("{variable} must be absent from the Pool migration process environment");
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_launch_request_ownership(metadata: &std::fs::Metadata) -> Result<()> {
    if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
        bail!("Pool migration launch request must be root-owned and not group/world writable");
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn validate_launch_request_ownership(_metadata: &std::fs::Metadata) -> Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_checkpoint_ack_ownership(metadata: &std::fs::Metadata) -> Result<()> {
    if !metadata.file_type().is_file()
        || metadata.uid() != 0
        || metadata.gid() != unsafe { libc::getegid() }
        || metadata.mode() & 0o022 != 0
        || metadata.mode() & 0o040 == 0
    {
        bail!(
            "migration checkpoint acknowledgement must be root-owned, service-group-readable, and non-writable by group/others"
        );
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn validate_checkpoint_ack_ownership(_metadata: &std::fs::Metadata) -> Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_controller_terminal_cursor_ownership(metadata: &std::fs::Metadata) -> Result<()> {
    if !metadata.file_type().is_file()
        || metadata.uid() != 0
        || metadata.gid() != unsafe { libc::getegid() }
        || metadata.mode() & 0o7777 != 0o440
        || metadata.nlink() != 1
    {
        bail!(
            "controller terminal cursor must be root-owned, service-group-readable, mode 0440, and single-link"
        );
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn validate_controller_terminal_cursor_ownership(_metadata: &std::fs::Metadata) -> Result<()> {
    Ok(())
}

fn validate_request_location(request_path: &Path) -> Result<()> {
    if request_path.file_name().and_then(|value| value.to_str()) != Some(REQUEST_FILE_NAME) {
        bail!("Pool migration launch request must be named {REQUEST_FILE_NAME}");
    }
    let attempt_dir = request_path
        .parent()
        .context("launch request has no attempt directory")?;
    let namespace = attempt_dir
        .parent()
        .context("launch request has no v3 attempt namespace")?;
    if namespace.file_name().and_then(|value| value.to_str()) != Some(ATTEMPT_NAMESPACE_NAME) {
        bail!(
            "Pool migration launch request must live beneath an {ATTEMPT_NAMESPACE_NAME} namespace"
        );
    }
    if attempt_dir.join(ACK_FILE_NAME).exists() {
        bail!(
            "Pool migration launch acknowledgement already exists; create a fresh {ATTEMPT_NAMESPACE_NAME} nonce"
        );
    }
    Ok(())
}

fn validate_cursor_authority(
    authority: &CursorAuthorityV3,
    parent: &PinnedDirectory,
    name: &OsStr,
) -> Result<Option<[u8; 32]>> {
    validate_cursor_checkpoint(authority, parent, name)?;
    if !authority.exists {
        return Ok(None);
    }

    let value = authority
        .value
        .as_deref()
        .context("present migration cursor has no value")?;
    let decoded = from_hex(value).context("decode pinned migration cursor")?;
    Ok(Some(decoded))
}

fn validate_cursor_checkpoint(
    authority: &CursorAuthorityV3,
    parent: &PinnedDirectory,
    name: &OsStr,
) -> Result<()> {
    if !authority.exists {
        if authority.value.is_some() || authority.sha256.is_some() {
            bail!("absent migration cursor authority contains a value or SHA-256");
        }
        if parent.entry_exists(name, "migration cursor")? {
            bail!(
                "migration cursor {} exists but its authority pins it as absent",
                authority.path.display()
            );
        }
        return Ok(());
    }

    let value = authority
        .value
        .as_deref()
        .context("present migration cursor has no value")?;
    let expected_sha256 = authority
        .sha256
        .as_deref()
        .context("present migration cursor has no SHA-256")?;
    let mut file = parent
        .open_regular_optional(name, "migration cursor")?
        .context("present migration cursor disappeared during validation")?;
    let bytes = read_bounded_open_file(
        &mut file,
        MAX_CURSOR_BYTES,
        "migration cursor",
        &authority.path,
    )?;
    let expected_bytes = format!("{value}\n");
    if bytes != expected_bytes.as_bytes() {
        bail!("migration cursor bytes are not the exact canonical pinned value");
    }
    if sha256_bytes(&bytes) != expected_sha256 {
        bail!("migration cursor SHA-256 differs from its exact authority");
    }
    Ok(())
}

fn validate_cursor_value(value: &str) -> Result<()> {
    if matches!(value, "complete" | "source-complete") {
        bail!("a completed migration cursor is terminal and must never be launched");
    }
    require_lower_hex("migration cursor", value, 64)?;
    let _: [u8; 32] = from_hex(value).context("decode migration cursor")?;
    Ok(())
}

fn validate_cursor_write_value(value: &str) -> Result<()> {
    if matches!(value, "complete" | "source-complete") {
        return Ok(());
    }
    validate_cursor_value(value)
}

fn replace_cursor_checkpoint(
    authority: &mut CursorAuthorityV3,
    parent: &PinnedDirectory,
    name: &OsStr,
    value: &str,
) -> Result<()> {
    validate_cursor_write_value(value)?;
    if authority
        .value
        .as_deref()
        .is_some_and(|value| matches!(value, "complete" | "source-complete"))
    {
        bail!("a completed migration cursor is terminal and cannot be overwritten");
    }
    validate_cursor_checkpoint(authority, parent, name)?;
    let mut bytes = value.as_bytes().to_vec();
    bytes.push(b'\n');
    parent.durable_replace(name, &bytes, "Pool migration cursor")?;
    authority.exists = true;
    authority.value = Some(value.to_owned());
    authority.sha256 = Some(sha256_bytes(&bytes));
    validate_cursor_checkpoint(authority, parent, name)
}

fn validate_file_authority(authority: &FileAuthorityV3, label: &str) -> Result<()> {
    validate_sha256(label, &authority.sha256)?;
    let actual = sha256_regular_file(&authority.path, label)?;
    if actual != authority.sha256 {
        bail!(
            "{label} SHA-256 mismatch for {}: expected {}, got {}",
            authority.path.display(),
            authority.sha256,
            actual
        );
    }
    Ok(())
}

fn validate_named_file_authority(authority: &NamedFileAuthorityV3) -> Result<()> {
    let label = format!("additional CAS {}", authority.label);
    validate_sha256(&label, &authority.sha256)?;
    let actual = sha256_regular_file(&authority.path, &label)?;
    if actual != authority.sha256 {
        bail!(
            "{label} SHA-256 mismatch for {}: expected {}, got {}",
            authority.path.display(),
            authority.sha256,
            actual
        );
    }
    Ok(())
}

fn pin_pool_topology(
    authority: &FileAuthorityV3,
    expected_pool_path: &Path,
) -> Result<PinnedPoolTopology> {
    let topology_path = canonical_regular_path(&authority.path, "Pool topology")?;
    let mut topology_file = open_regular_file(&topology_path, "Pool topology")?;
    let topology_bytes = read_bounded_open_file(
        &mut topology_file,
        MAX_TOPOLOGY_BYTES,
        "Pool topology",
        &topology_path,
    )?;
    let topology_metadata = topology_file
        .metadata()
        .context("reinspect open Pool topology")?;
    ensure_path_still_matches(&topology_path, &topology_metadata, "Pool topology")?;
    if sha256_bytes(&topology_bytes) != authority.sha256 {
        bail!("Pool topology bytes changed after their launch-request CAS validation");
    }
    let topology: PoolTopologyV3 =
        serde_json::from_slice(&topology_bytes).context("parse strict Pool topology v3")?;
    if topology.schema != POOL_TOPOLOGY_SCHEMA {
        bail!(
            "unsupported Pool topology schema {}; expected {POOL_TOPOLOGY_SCHEMA}",
            topology.schema
        );
    }
    let topology_pool = canonical_directory_path(&topology.pool_path, "topology Pool")?;
    let expected_pool = canonical_directory_path(expected_pool_path, "requested Pool")?;
    if topology_pool != expected_pool {
        bail!("Pool topology belongs to a different Pool path");
    }
    validate_sha256("Pool topology manifest", &topology.manifest_sha256)?;
    let manifest_sha256: [u8; 32] =
        from_hex(&topology.manifest_sha256).context("decode Pool topology manifest SHA-256")?;
    if topology.members.is_empty() {
        bail!("Pool topology must pin at least one member");
    }

    let topology_authority = topology.clone();
    let mut last_id: Option<String> = None;
    let mut paths = HashSet::new();
    let mut pinned = Vec::with_capacity(topology.members.len());
    for member in topology.members {
        let parsed_id =
            uuid::Uuid::parse_str(&member.id).context("parse Pool topology member ID")?;
        let id = parsed_id.to_string();
        if id != member.id {
            bail!("Pool topology member ID must be a canonical lowercase UUID");
        }
        if last_id.as_ref().is_some_and(|previous| previous >= &id) {
            bail!("Pool topology members must be uniquely sorted by ID");
        }
        last_id = Some(id.clone());
        validate_file_identity(
            &format!("Pool member {id} directory"),
            member.directory_identity,
        )?;
        validate_lmdb_identity(&format!("Pool member {id}"), member.lmdb_identity)?;

        let configured_path =
            canonical_directory_path(&member.path, &format!("Pool member {id} directory"))?;
        if !paths.insert(configured_path.clone()) {
            bail!("Pool topology contains duplicate member/external paths");
        }
        let directory =
            PinnedDirectory::open_exact(&configured_path, &format!("Pool member {id} directory"))?;
        directory.require_authority_identity(
            member.directory_identity,
            &format!("Pool member {id} directory"),
        )?;
        let lmdb_files = PinnedLmdbFiles::pin(&directory, &format!("Pool member {id}"))?;
        lmdb_files.require_authority_identity(
            &directory,
            member.lmdb_identity,
            &format!("Pool member {id}"),
        )?;
        validate_marker_authority(
            &directory,
            MEMBER_MARKER_NAME,
            &member.marker,
            &id,
            &format!("Pool member {id} marker"),
        )?;

        let (configured_external_path, external_directory, external_marker_sha256) = match (
            member.external_path,
            member.external_directory_identity,
            member.external_marker,
        ) {
            (Some(path), Some(directory_identity), Some(marker)) => {
                validate_file_identity(
                    &format!("Pool member {id} external directory"),
                    directory_identity,
                )?;
                let path = canonical_directory_path(
                    &path,
                    &format!("Pool member {id} external directory"),
                )?;
                if !paths.insert(path.clone()) {
                    bail!("Pool topology contains duplicate member/external paths");
                }
                let directory = PinnedDirectory::open_exact(
                    &path,
                    &format!("Pool member {id} external directory"),
                )?;
                directory.require_authority_identity(
                    directory_identity,
                    &format!("Pool member {id} external directory"),
                )?;
                validate_marker_authority(
                    &directory,
                    EXTERNAL_MARKER_NAME,
                    &marker,
                    &id,
                    &format!("Pool member {id} external marker"),
                )?;
                (Some(path), Some(directory), Some(marker.sha256))
            }
            (None, None, None) => (None, None, None),
            _ => bail!("Pool topology member {id} has incomplete external path authority"),
        };
        pinned.push(PinnedPoolMemberPaths {
            id,
            configured_path,
            directory,
            lmdb_files,
            marker_sha256: member.marker.sha256,
            configured_external_path,
            external_directory,
            external_marker_sha256,
        });
    }
    Ok(PinnedPoolTopology {
        manifest_sha256,
        topology: topology_authority,
        members: pinned,
    })
}

#[cfg(target_os = "linux")]
pub(super) fn validate_pool_migration_topology_authority(
    authority: &FileAuthorityV3,
    expected_pool_path: &Path,
) -> Result<()> {
    pin_pool_topology(authority, expected_pool_path).map(|_| ())
}

fn validate_marker_authority(
    directory: &PinnedDirectory,
    marker_name: &str,
    authority: &FileAuthorityV3,
    expected_member_id: &str,
    label: &str,
) -> Result<()> {
    validate_sha256(label, &authority.sha256)?;
    let expected_path = directory.path.join(marker_name);
    if authority.path != expected_path {
        bail!("{label} path must be exactly {}", expected_path.display());
    }
    let bytes = read_file_in_directory(
        directory,
        OsStr::new(marker_name),
        256,
        label,
        &expected_path,
    )?;
    if sha256_bytes(&bytes) != authority.sha256 {
        bail!("{label} SHA-256 differs from Pool topology authority");
    }
    if bytes != format!("{expected_member_id}\n").as_bytes() {
        bail!("{label} does not contain the exact pinned member ID");
    }
    Ok(())
}

fn validate_marker_in_directory(
    directory: &PinnedDirectory,
    marker_name: &OsStr,
    expected_sha256: &str,
    label: &str,
) -> Result<()> {
    let display_path = directory.path.join(marker_name);
    let bytes = read_file_in_directory(directory, marker_name, 256, label, &display_path)?;
    if sha256_bytes(&bytes) != expected_sha256 {
        bail!("{label} changed after Pool topology validation");
    }
    Ok(())
}

fn read_file_in_directory(
    directory: &PinnedDirectory,
    name: &OsStr,
    max_bytes: u64,
    label: &str,
    display_path: &Path,
) -> Result<Vec<u8>> {
    let mut file = directory
        .open_regular_optional(name, label)?
        .with_context(|| format!("{label} {} is absent", display_path.display()))?;
    let bytes = read_bounded_open_file(&mut file, max_bytes, label, display_path)?;
    let opened = file
        .metadata()
        .with_context(|| format!("reinspect {label}"))?;
    let reopened = directory
        .open_regular_optional(name, label)?
        .with_context(|| format!("{label} disappeared during validation"))?;
    let entry = reopened
        .metadata()
        .with_context(|| format!("reinspect {label} directory entry"))?;
    ensure_same_file_snapshot(&opened, &entry, label)?;
    Ok(bytes)
}

fn canonical_regular_path(path: &Path, label: &str) -> Result<PathBuf> {
    require_absolute(path, label)?;
    let canonical = path
        .canonicalize()
        .with_context(|| format!("canonicalize {label} {}", path.display()))?;
    if canonical != path {
        bail!(
            "{label} must be an exact canonical path (got {}, canonical {})",
            path.display(),
            canonical.display()
        );
    }
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspect {label} {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!("{label} {} is not a regular file", path.display());
    }
    Ok(canonical)
}

fn canonical_directory_path(path: &Path, label: &str) -> Result<PathBuf> {
    require_absolute(path, label)?;
    let canonical = path
        .canonicalize()
        .with_context(|| format!("canonicalize {label} {}", path.display()))?;
    if canonical != path {
        bail!(
            "{label} must be an exact canonical path (got {}, canonical {})",
            path.display(),
            canonical.display()
        );
    }
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspect {label} {}", path.display()))?;
    if !metadata.file_type().is_dir() {
        bail!("{label} {} is not a directory", path.display());
    }
    Ok(canonical)
}

fn canonical_or_absent_path(path: &Path, label: &str) -> Result<PathBuf> {
    require_absolute(path, label)?;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                bail!("{label} {} is not a regular file", path.display());
            }
            let canonical = path
                .canonicalize()
                .with_context(|| format!("canonicalize {label} {}", path.display()))?;
            if canonical != path {
                bail!("{label} {} is not an exact canonical path", path.display());
            }
            Ok(canonical)
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            let parent = path
                .parent()
                .filter(|value| !value.as_os_str().is_empty())
                .context("absent migration cursor has no parent directory")?;
            let canonical_parent = canonical_directory_path(parent, "migration cursor parent")?;
            let file_name = path
                .file_name()
                .context("absent migration cursor has no file name")?;
            let canonical = canonical_parent.join(file_name);
            if canonical != path {
                bail!("{label} {} is not an exact canonical path", path.display());
            }
            Ok(canonical)
        }
        Err(error) => Err(error).with_context(|| format!("inspect {label} {}", path.display())),
    }
}

fn require_absolute(path: &Path, label: &str) -> Result<()> {
    if !path.is_absolute() {
        bail!("{label} path must be absolute: {}", path.display());
    }
    Ok(())
}

fn require_safe_component(label: &str, value: &str, max_len: usize) -> Result<()> {
    if value.is_empty()
        || value.len() > max_len
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        bail!("{label} is not a safe bounded path component");
    }
    Ok(())
}

fn require_systemd_service_name(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 255
        || !value.starts_with("hashtree-pool-migration-worker@")
        || !value.ends_with(".service")
        || value == "hashtree-pool-migration-worker@.service"
        || value.contains('/')
        || value == "."
        || value == ".."
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'_' | b'.' | b'@' | b'\\' | b'-')
        })
    {
        bail!(
            "request systemd unit must be an exact bounded hashtree-pool-migration-worker@*.service name"
        );
    }
    Ok(())
}

fn require_controller_systemd_service_name(value: &str) -> Result<()> {
    if value.len() > 255
        || !value.starts_with("hashtree-pool-migration-controller@")
        || !value.ends_with(".service")
        || value == "hashtree-pool-migration-controller@.service"
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'@' | b'.' | b'_' | b':' | b'\\' | b'-')
        })
    {
        bail!(
            "checkpoint broker unit must be an exact bounded hashtree-pool-migration-controller@*.service name"
        );
    }
    Ok(())
}

fn require_writer_systemd_service_name(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 255
        || !value.ends_with(".service")
        || value.contains('/')
        || value == "."
        || value == ".."
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'_' | b'.' | b'@' | b'\\' | b'-')
        })
    {
        bail!("controller writer unit must be an exact bounded .service unit name");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_systemd_membership(
    expected_unit: &str,
    expected_invocation_id: &str,
    expected_main_pid: u32,
    expected_fragment: &FileAuthorityV3,
    expected_environment_file: &FileAuthorityV3,
    expected_binary: &Path,
) -> Result<()> {
    let cgroups = std::fs::read_to_string("/proc/self/cgroup").context("read /proc/self/cgroup")?;
    let belongs_to_unit = cgroups.lines().any(|line| {
        let Some((_, path)) = line.rsplit_once(':') else {
            return false;
        };
        Path::new(path)
            .components()
            .next_back()
            .and_then(|component| component.as_os_str().to_str())
            == Some(expected_unit)
    });
    if !belongs_to_unit {
        bail!(
            "Pool migration process is not in the exact requested systemd service cgroup {expected_unit}"
        );
    }

    if cgroups
        .lines()
        .filter_map(|line| line.rsplit_once(':').map(|(_, path)| path))
        .any(|path| path.contains("/user.slice/"))
    {
        bail!("Pool migration v3 must run under the system manager, never a user manager");
    }

    validate_systemd_fragment_authority(expected_fragment)?;
    let loaded_environment =
        validate_systemd_environment_file_authority(expected_environment_file)?;
    for (key, expected) in &loaded_environment {
        let actual = std::env::var(key).with_context(|| {
            format!("systemd environment file key {key} is absent from process")
        })?;
        if &actual != expected {
            bail!("systemd environment file key {key} differs from the process environment");
        }
    }
    for (key, _) in std::env::vars() {
        if key.starts_with("HTREE_POOL_") && !loaded_environment.contains_key(&key) {
            bail!("process has unbound Pool migration environment key {key}");
        }
    }
    let systemctl = trusted_systemctl_path()?;
    let mut command = std::process::Command::new(systemctl);
    command.env_clear().env("LANG", "C");
    command.arg("--system");
    let output = command
        .args([
            "--no-pager",
            "show",
            expected_unit,
            "--property",
            "InvocationID",
            "--property",
            "MainPID",
            "--property",
            "FragmentPath",
            "--property",
            "Type",
            "--property",
            "Restart",
            "--property",
            "NeedDaemonReload",
            "--property",
            "DropInPaths",
            "--property",
            "EnvironmentFiles",
            "--property",
            "Environment",
            "--property",
            "PassEnvironment",
            "--property",
            "UnsetEnvironment",
            "--property",
            "ExecCondition",
            "--property",
            "ExecStartPre",
            "--property",
            "ExecStart",
            "--property",
            "ExecStartPost",
            "--property",
            "ExecReload",
            "--property",
            "ExecStop",
            "--property",
            "ExecStopPost",
            "--property",
            "NRestarts",
            "--property",
            "ControlPID",
            "--property",
            "UID",
            "--property",
            "GID",
            "--property",
            "PrivateNetwork",
            "--property",
            "NoNewPrivileges",
            "--property",
            "TimeoutStartUSec",
        ])
        .output()
        .context("query systemd-owned Pool migration identity")?;
    if !output.status.success() {
        bail!(
            "systemd manager rejected Pool migration identity query: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let properties =
        String::from_utf8(output.stdout).context("systemd identity output is not UTF-8")?;
    let properties_by_name =
        parse_systemd_properties(&properties, "systemd migration identity output")?;
    if properties_by_name.get("InvocationID").copied() != Some(expected_invocation_id) {
        bail!("systemd-owned InvocationID does not match the launch request");
    }
    let main_pid = properties_by_name
        .get("MainPID")
        .context("systemd-owned migration unit has no MainPID property")?
        .parse::<u32>()
        .context("parse systemd-owned MainPID")?;
    if main_pid != expected_main_pid {
        bail!("systemd-owned MainPID does not match the launch request and current process");
    }
    if properties_by_name.get("FragmentPath").copied() != expected_fragment.path.to_str() {
        bail!("systemd-owned FragmentPath does not match the launch request");
    }
    if properties_by_name.get("Type").copied() != Some("oneshot")
        || properties_by_name.get("Restart").copied() != Some("no")
    {
        bail!("systemd-owned migration unit must be Type=oneshot with Restart=no");
    }
    if properties_by_name.get("DropInPaths").copied() != Some("") {
        bail!("systemd-owned migration unit must have empty DropInPaths");
    }
    let expected_environment = expected_environment_file
        .path
        .to_str()
        .context("systemd environment file path is not UTF-8")?;
    let environment_files = properties_by_name
        .get("EnvironmentFiles")
        .copied()
        .context("systemd-owned migration unit has no EnvironmentFiles property")?;
    if environment_files != expected_environment
        && environment_files != format!("{expected_environment} (ignore_errors=no)")
    {
        bail!("systemd-owned migration unit has unexpected EnvironmentFiles");
    }
    require_empty_systemd_properties(
        &properties_by_name,
        &["Environment", "PassEnvironment"],
        "systemd-owned migration unit",
    )?;
    let unset_environment = properties_by_name
        .get("UnsetEnvironment")
        .copied()
        .context("systemd-owned migration unit has no UnsetEnvironment property")?
        .split_ascii_whitespace()
        .collect::<HashSet<_>>();
    for variable in [
        "LD_PRELOAD",
        "LD_AUDIT",
        "LD_LIBRARY_PATH",
        "DYLD_INSERT_LIBRARIES",
        "DYLD_LIBRARY_PATH",
        "HTREE_LMDB_NO_SYNC",
        "HTREE_LMDB_NO_META_SYNC",
    ] {
        if !unset_environment.contains(variable) {
            bail!("systemd-owned migration unit must unset {variable}");
        }
    }
    if properties_by_name.get("NeedDaemonReload").copied() != Some("no") {
        bail!("systemd-owned migration unit has stale loaded fragment state");
    }
    // systemd suppresses empty Exec* array properties in `systemctl show`
    // output (including when they are requested explicitly). The exact
    // root-owned fragment, empty DropInPaths, and fresh loaded state above
    // establish where hooks can come from; any hook that systemd does emit
    // must therefore remain empty.
    reject_nonempty_systemd_properties(
        &properties_by_name,
        &[
            "ExecCondition",
            "ExecStartPre",
            "ExecStartPost",
            "ExecReload",
            "ExecStop",
            "ExecStopPost",
        ],
        "systemd-owned migration unit",
    )?;
    if properties_by_name.get("NRestarts").copied() != Some("0")
        || properties_by_name.get("ControlPID").copied() != Some("0")
    {
        bail!("systemd-owned migration unit has an unexpected restart or control process");
    }
    let exec_start = properties_by_name
        .get("ExecStart")
        .copied()
        .context("systemd-owned migration unit has no ExecStart property")?;
    let exec_start_path = exec_start
        .strip_prefix("{ path=")
        .and_then(|remaining| remaining.split_once(" ;"))
        .map(|(path, _)| path);
    if exec_start.matches("{ path=").count() != 1 || exec_start_path != expected_binary.to_str() {
        bail!("systemd-owned migration unit must have one exact direct ExecStart binary");
    }
    let uid = properties_by_name
        .get("UID")
        .context("systemd-owned migration unit has no UID")?
        .parse::<u32>()
        .context("parse systemd-owned service UID")?;
    let gid = properties_by_name
        .get("GID")
        .context("systemd-owned migration unit has no GID")?
        .parse::<u32>()
        .context("parse systemd-owned service GID")?;
    if uid != unsafe { libc::geteuid() } || gid != unsafe { libc::getegid() } {
        bail!("systemd-owned service UID/GID do not match the migration process");
    }
    if properties_by_name.get("PrivateNetwork").copied() != Some("yes")
        || properties_by_name.get("NoNewPrivileges").copied() != Some("yes")
        || properties_by_name.get("TimeoutStartUSec").copied() != Some("infinity")
    {
        bail!("systemd-owned migration unit is missing required launch isolation");
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn validate_systemd_membership(
    _expected_unit: &str,
    _expected_invocation_id: &str,
    _expected_main_pid: u32,
    _expected_fragment: &FileAuthorityV3,
    _expected_environment_file: &FileAuthorityV3,
    _expected_binary: &Path,
) -> Result<()> {
    bail!("Pool migration v3 launch is supported only on Linux under systemd")
}

#[cfg(target_os = "linux")]
fn validate_systemd_fragment_authority(authority: &FileAuthorityV3) -> Result<()> {
    let fragment = canonical_regular_path(&authority.path, "systemd unit fragment")?;
    if fragment.file_name().and_then(|value| value.to_str())
        != Some("hashtree-pool-migration-worker@.service")
    {
        bail!("systemd unit fragment must be named hashtree-pool-migration-worker@.service");
    }
    let metadata = std::fs::symlink_metadata(&fragment)
        .context("inspect systemd Pool migration unit fragment")?;
    if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
        bail!("systemd unit fragment must be root-owned and not group/world writable");
    }
    validate_file_authority(authority, "systemd unit fragment")
}

#[cfg(target_os = "linux")]
fn validate_systemd_environment_file_authority(
    authority: &FileAuthorityV3,
) -> Result<HashMap<String, String>> {
    let path = canonical_regular_path(&authority.path, "systemd environment file")?;
    let mut file = open_regular_file(&path, "systemd environment file")?;
    let metadata = file
        .metadata()
        .context("inspect open systemd Pool migration environment file")?;
    if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
        bail!("systemd environment file must be root-owned and not group/world writable");
    }
    let bytes = read_bounded_open_file(
        &mut file,
        MAX_SYSTEMD_ENVIRONMENT_BYTES,
        "systemd environment file",
        &path,
    )?;
    ensure_path_still_matches(&path, &metadata, "systemd environment file")?;
    if sha256_bytes(&bytes) != authority.sha256 {
        bail!("systemd environment file SHA-256 differs from launch request authority");
    }
    let text = std::str::from_utf8(&bytes).context("systemd environment file is not UTF-8")?;
    let allowed = [
        "HTREE_POOL_TARGET_DATA_DIR",
        "HTREE_POOL_LAUNCH_REQUEST",
        "HTREE_POOL_LAUNCH_WAIT_SECONDS",
        "HTREE_POOL_SOURCE_LMDB_DIR",
        "HTREE_POOL_SOURCE_EXTERNAL_ARGS",
        "HTREE_POOL_STATE_FILE",
        "HTREE_POOL_BATCH_SIZE",
        "HTREE_POOL_MAX_BUFFER_MIB",
        "HTREE_POOL_SOURCE_READ_CONCURRENCY",
        "HTREE_POOL_REOPEN_BATCHES",
        "HTREE_POOL_LIMIT_ARGS",
    ]
    .into_iter()
    .collect::<HashSet<_>>();
    let mut loaded = HashMap::new();
    for (index, line) in text.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.trim() != line || line.contains('\0') || line.contains('\r') {
            bail!(
                "systemd environment file line {} is not a canonical KEY=VALUE assignment",
                index + 1
            );
        }
        let (key, value) = line.split_once('=').with_context(|| {
            format!(
                "systemd environment file line {} is not KEY=VALUE",
                index + 1
            )
        })?;
        if !allowed.contains(key) || loaded.contains_key(key) {
            bail!("systemd environment file has unknown or duplicate key {key}");
        }
        if key == "HTREE_POOL_LIMIT_ARGS" {
            if !value.is_empty() {
                let Some(limit) = value.strip_prefix("--max-items ") else {
                    bail!("HTREE_POOL_LIMIT_ARGS must be empty or exactly --max-items N");
                };
                if limit.is_empty()
                    || limit.starts_with('0')
                    || !limit.bytes().all(|byte| byte.is_ascii_digit())
                {
                    bail!("HTREE_POOL_LIMIT_ARGS max-items value must be a positive integer");
                }
            }
        } else if key == "HTREE_POOL_SOURCE_EXTERNAL_ARGS" {
            if !value.is_empty() {
                let Some(path) = value.strip_prefix("--source-external-dir ") else {
                    bail!(
                        "HTREE_POOL_SOURCE_EXTERNAL_ARGS must be empty or exactly --source-external-dir /absolute/path"
                    );
                };
                if !Path::new(path).is_absolute()
                    || path.bytes().any(|byte| byte.is_ascii_whitespace())
                {
                    bail!(
                        "HTREE_POOL_SOURCE_EXTERNAL_ARGS path must be absolute without whitespace"
                    );
                }
                let canonical = canonical_directory_path(
                    Path::new(path),
                    "HTREE_POOL_SOURCE_EXTERNAL_ARGS path",
                )?;
                if canonical != Path::new(path) {
                    bail!("HTREE_POOL_SOURCE_EXTERNAL_ARGS path must be canonical");
                }
            }
        } else if value.is_empty()
            || value
                .bytes()
                .any(|byte| byte.is_ascii_whitespace() || byte < 0x20 || byte == 0x7f)
        {
            bail!("systemd environment file key {key} has an unsafe or empty value");
        }
        loaded.insert(key.to_string(), value.to_string());
    }
    Ok(loaded)
}

fn validate_sha256(label: &str, value: &str) -> Result<()> {
    require_lower_hex(&format!("{label} SHA-256"), value, 64)
}

fn require_lower_hex(label: &str, value: &str, len: usize) -> Result<()> {
    if value.len() != len
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("{label} must be exactly {len} lowercase hexadecimal characters");
    }
    Ok(())
}

fn require_boot_id(label: &str, value: &str) -> Result<()> {
    let bytes = value.as_bytes();
    if bytes.len() != 36
        || ![8usize, 13, 18, 23]
            .into_iter()
            .all(|index| bytes[index] == b'-')
    {
        bail!("{label} must be a canonical lowercase UUID");
    }
    for (index, byte) in bytes.iter().copied().enumerate() {
        if [8usize, 13, 18, 23].contains(&index) {
            continue;
        }
        if !(byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)) {
            bail!("{label} must be a canonical lowercase UUID");
        }
    }
    Ok(())
}

fn read_bounded_open_file(
    file: &mut File,
    max_bytes: u64,
    label: &str,
    display_path: &Path,
) -> Result<Vec<u8>> {
    let before = file
        .metadata()
        .with_context(|| format!("inspect open {label} {}", display_path.display()))?;
    if before.len() > max_bytes {
        bail!(
            "{label} {} is larger than the {} byte limit",
            display_path.display(),
            max_bytes
        );
    }
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("rewind {label} {}", display_path.display()))?;
    let mut bytes = Vec::with_capacity(before.len() as usize);
    Read::by_ref(file)
        .take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {label} {}", display_path.display()))?;
    if bytes.len() as u64 > max_bytes {
        bail!(
            "{label} {} grew beyond the {} byte limit",
            display_path.display(),
            max_bytes
        );
    }
    let after = file
        .metadata()
        .with_context(|| format!("reinspect open {label} {}", display_path.display()))?;
    ensure_same_file_snapshot(&before, &after, label)?;
    Ok(bytes)
}

fn sha256_regular_file(path: &Path, label: &str) -> Result<String> {
    let canonical = canonical_regular_path(path, label)?;
    let mut file = open_regular_file(&canonical, label)?;
    let before = file
        .metadata()
        .with_context(|| format!("inspect open {label} {}", canonical.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("hash {label} {}", canonical.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let after = file
        .metadata()
        .with_context(|| format!("reinspect open {label} {}", canonical.display()))?;
    ensure_same_file_snapshot(&before, &after, label)?;
    ensure_path_still_matches(&canonical, &after, label)?;
    Ok(hex::encode(hasher.finalize()))
}

fn open_regular_file(path: &Path, label: &str) -> Result<File> {
    let parent = path
        .parent()
        .filter(|value| !value.as_os_str().is_empty())
        .with_context(|| format!("{label} has no parent directory"))?;
    let name = path
        .file_name()
        .with_context(|| format!("{label} has no file name"))?;
    let parent = PinnedDirectory::open_exact(parent, &format!("{label} parent"))?;
    parent
        .open_regular_optional(name, label)?
        .with_context(|| format!("{label} {} disappeared while opening", path.display()))
}

#[cfg(unix)]
fn ensure_same_file_snapshot(
    before: &std::fs::Metadata,
    after: &std::fs::Metadata,
    label: &str,
) -> Result<()> {
    if before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.uid() != after.uid()
        || before.gid() != after.gid()
        || before.mode() != after.mode()
        || before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
        || before.ctime() != after.ctime()
        || before.ctime_nsec() != after.ctime_nsec()
    {
        bail!("{label} changed while it was being validated");
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_same_file_snapshot(
    before: &std::fs::Metadata,
    after: &std::fs::Metadata,
    label: &str,
) -> Result<()> {
    if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
        bail!("{label} changed while it was being validated");
    }
    Ok(())
}

#[cfg(unix)]
fn ensure_path_still_matches(path: &Path, opened: &std::fs::Metadata, label: &str) -> Result<()> {
    let current = std::fs::symlink_metadata(path)
        .with_context(|| format!("reinspect {label} {}", path.display()))?;
    if current.dev() != opened.dev()
        || current.ino() != opened.ino()
        || current.len() != opened.len()
        || current.mtime() != opened.mtime()
        || current.mtime_nsec() != opened.mtime_nsec()
        || current.ctime() != opened.ctime()
        || current.ctime_nsec() != opened.ctime_nsec()
    {
        bail!("{label} path changed while it was being validated");
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_path_still_matches(path: &Path, opened: &std::fs::Metadata, label: &str) -> Result<()> {
    let current = std::fs::symlink_metadata(path)
        .with_context(|| format!("reinspect {label} {}", path.display()))?;
    ensure_same_file_snapshot(opened, &current, label)
}

fn sha256_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn argv_sha256(argv: &[String]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"hashtree-pool-migration-argv/v3\0");
    for argument in argv {
        hasher.update((argument.len() as u64).to_be_bytes());
        hasher.update(argument.as_bytes());
    }
    hex::encode(hasher.finalize())
}

#[cfg(target_os = "linux")]
fn current_process_start_time_ticks() -> Result<u64> {
    let stat = std::fs::read_to_string("/proc/self/stat").context("read /proc/self/stat")?;
    let command_end = stat
        .rfind(") ")
        .context("parse /proc/self/stat process name")?;
    // The first token after ") " is field 3. Linux starttime is field 22.
    let value = stat[command_end + 2..]
        .split_ascii_whitespace()
        .nth(19)
        .context("read /proc/self/stat starttime field")?
        .parse::<u64>()
        .context("parse /proc/self/stat starttime")?;
    if value == 0 {
        bail!("/proc/self/stat starttime is zero");
    }
    Ok(value)
}

#[cfg(target_os = "macos")]
fn current_process_start_time_ticks() -> Result<u64> {
    use std::mem::{size_of, MaybeUninit};

    let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = size_of::<libc::proc_bsdinfo>();
    let read = unsafe {
        libc::proc_pidinfo(
            std::process::id() as i32,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size as i32,
        )
    };
    if read != size as i32 {
        return Err(std::io::Error::last_os_error()).context("read macOS process start identity");
    }
    let info = unsafe { info.assume_init() };
    let value = info
        .pbi_start_tvsec
        .checked_mul(1_000_000)
        .and_then(|seconds| seconds.checked_add(info.pbi_start_tvusec))
        .context("macOS process start identity overflow")?;
    if value == 0 {
        bail!("macOS process start identity is zero");
    }
    Ok(value)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn current_process_start_time_ticks() -> Result<u64> {
    bail!("Pool migration v3 launch requires a supported process start identity")
}

#[cfg(target_os = "linux")]
fn running_executable_sha256(expected_path: &Path) -> Result<String> {
    let proc_exe = Path::new("/proc/self/exe");
    let proc_target = proc_exe
        .canonicalize()
        .context("canonicalize /proc/self/exe")?;
    if proc_target != expected_path {
        bail!(
            "/proc/self/exe resolves to {}, expected {}",
            proc_target.display(),
            expected_path.display()
        );
    }
    let mut file = File::open(proc_exe).context("open /proc/self/exe")?;
    let opened = file.metadata().context("inspect /proc/self/exe")?;
    let expected = std::fs::symlink_metadata(expected_path)
        .with_context(|| format!("inspect running binary {}", expected_path.display()))?;
    ensure_same_file_snapshot(&opened, &expected, "running executable")?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer).context("hash /proc/self/exe")?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let after = file.metadata().context("reinspect /proc/self/exe")?;
    ensure_same_file_snapshot(&opened, &after, "running executable")?;
    ensure_path_still_matches(expected_path, &after, "running executable")?;
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(not(target_os = "linux"))]
fn running_executable_sha256(expected_path: &Path) -> Result<String> {
    sha256_regular_file(expected_path, "running executable")
}

#[cfg(target_os = "linux")]
fn current_boot_id() -> Result<String> {
    let value = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .context("read Linux boot ID")?
        .trim()
        .to_ascii_lowercase();
    require_boot_id("current boot ID", &value)?;
    Ok(value)
}

#[cfg(target_os = "macos")]
fn current_boot_id() -> Result<String> {
    use std::ffi::CString;
    use std::ptr;

    let name = CString::new("kern.bootsessionuuid").expect("static sysctl name");
    let mut length = 0usize;
    let size_status = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            ptr::null_mut(),
            &mut length,
            ptr::null_mut(),
            0,
        )
    };
    if size_status != 0 || length == 0 {
        return Err(std::io::Error::last_os_error()).context("read macOS boot session UUID size");
    }
    let mut bytes = vec![0u8; length];
    let read_status = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            bytes.as_mut_ptr().cast(),
            &mut length,
            ptr::null_mut(),
            0,
        )
    };
    if read_status != 0 {
        return Err(std::io::Error::last_os_error()).context("read macOS boot session UUID");
    }
    bytes.truncate(length);
    let value = String::from_utf8(bytes)
        .context("macOS boot session UUID is not UTF-8")?
        .trim_matches(char::from(0))
        .trim()
        .to_ascii_lowercase();
    require_boot_id("current boot ID", &value)?;
    Ok(value)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn current_boot_id() -> Result<String> {
    bail!("Pool migration v3 launch acknowledgement requires a supported OS boot ID")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stopped_final_batch_size_is_fixed_to_bound_checkpoint_files() {
        validate_stopped_final_batch_size(false, 1)
            .expect("online bounded passes may use a smaller batch");
        validate_stopped_final_batch_size(true, MAX_FINAL_BATCH_SIZE)
            .expect("the fixed stopped-final batch is accepted");
        for batch_size in [1, MAX_FINAL_BATCH_SIZE - 1, MAX_FINAL_BATCH_SIZE + 1] {
            let error = validate_stopped_final_batch_size(true, batch_size)
                .expect_err("stopped-final batch amplification must fail closed");
            assert!(
                error
                    .to_string()
                    .contains(&MAX_FINAL_BATCH_SIZE.to_string()),
                "unexpected stopped-final batch error: {error:#}"
            );
        }
    }

    #[test]
    fn release_phase_accepts_online_exact_target_audit_path() {
        validate_pool_migration_release_phase("online-bounded")
            .expect("online bounded now publishes a root-certified target audit");
        validate_pool_migration_release_phase("final-stopped-source")
            .expect("source-final is a supported release phase");
        validate_pool_migration_release_phase("final-stopped-full")
            .expect("full-final is a supported release phase");
        validate_pool_migration_release_phase("not-a-phase")
            .expect_err("unknown migration phase must fail closed");
    }

    #[test]
    fn source_read_concurrency_has_a_hard_process_cap() {
        validate_source_read_concurrency(1).expect("one source reader is accepted");
        validate_source_read_concurrency(MAX_FINAL_SOURCE_READ_CONCURRENCY)
            .expect("the exact source reader cap is accepted");
        let error = validate_source_read_concurrency(MAX_FINAL_SOURCE_READ_CONCURRENCY + 1)
            .expect_err("one source reader above the cap must fail");
        assert!(
            error.to_string().contains("hard maximum"),
            "unexpected source concurrency error: {error:#}"
        );
    }

    fn absent_cursor(path: PathBuf, parent: &PinnedDirectory) -> CursorAuthorityV3 {
        CursorAuthorityV3 {
            path,
            parent_identity: parent.authority_identity(),
            exists: false,
            value: None,
            sha256: None,
        }
    }

    #[test]
    fn cursor_checkpoint_replace_is_cas_and_complete_is_terminal() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().canonicalize().expect("canonical tempdir");
        let parent =
            PinnedDirectory::open_exact(&root, "test cursor parent").expect("pin cursor parent");
        let name = OsStr::new("migration.cursor");
        let path = root.join(name);
        let first = "11".repeat(32);
        let unexpected = "22".repeat(32);
        let mut authority = absent_cursor(path, &parent);

        replace_cursor_checkpoint(&mut authority, &parent, name, &first)
            .expect("publish first cursor");
        parent
            .durable_replace(
                name,
                format!("{unexpected}\n").as_bytes(),
                "out-of-band cursor replacement",
            )
            .expect("replace cursor outside its authority");
        let error = replace_cursor_checkpoint(&mut authority, &parent, name, &first)
            .expect_err("changed cursor must fail CAS");
        assert!(error.to_string().contains("exact canonical pinned value"));
        assert_eq!(
            std::fs::read_to_string(root.join(name)).expect("read changed cursor"),
            format!("{unexpected}\n"),
            "failed CAS must not overwrite the changed cursor"
        );

        authority.value = Some(unexpected.clone());
        authority.sha256 = Some(sha256_bytes(format!("{unexpected}\n").as_bytes()));
        replace_cursor_checkpoint(&mut authority, &parent, name, "complete")
            .expect("publish terminal cursor");
        let error = replace_cursor_checkpoint(&mut authority, &parent, name, &first)
            .expect_err("complete cursor must be terminal");
        assert!(error.to_string().contains("terminal"));
    }

    #[test]
    fn systemd_required_property_validation_fails_closed_on_missing_properties() {
        let mask = WriterUnitMaskV3 {
            unit: "writer.service".to_string(),
            path: PathBuf::from("/run/systemd/system/writer.service"),
            identity: FileIdentityV3 {
                device: 1,
                inode: 2,
            },
            target: PathBuf::from("/dev/null"),
        };
        let stopped = parse_systemd_properties(
            "LoadState=masked\nUnitFileState=masked-runtime\nActiveState=inactive\nSubState=dead\nMainPID=0\nControlPID=0\nJob=\nNeedDaemonReload=no\nFragmentPath=/run/systemd/system/writer.service\n",
            "test stopped writer",
        )
        .expect("parse complete stopped writer properties");
        validate_runtime_masked_writer_property_map("writer.service", &mask, &stopped)
            .expect("complete stopped writer properties");

        let missing_job = parse_systemd_properties(
            "LoadState=masked\nUnitFileState=masked-runtime\nActiveState=inactive\nSubState=dead\nMainPID=0\nControlPID=0\nNeedDaemonReload=no\nFragmentPath=/run/systemd/system/writer.service\n",
            "test stopped writer",
        )
        .expect("parse missing-Job stopped writer properties");
        let error =
            validate_runtime_masked_writer_property_map("writer.service", &mask, &missing_job)
                .expect_err("missing Job must not prove a job-free writer");
        assert!(error.to_string().contains("job-free"));

        let empty_properties =
            parse_systemd_properties("Environment=\nPassEnvironment=\n", "test migration unit")
                .expect("parse complete empty properties");
        require_empty_systemd_properties(
            &empty_properties,
            &["Environment", "PassEnvironment"],
            "test migration unit",
        )
        .expect("all explicitly empty properties");

        let missing = parse_systemd_properties("Environment=\n", "test migration unit")
            .expect("parse intentionally incomplete properties");
        let error = require_empty_systemd_properties(
            &missing,
            &["Environment", "PassEnvironment"],
            "test migration unit",
        )
        .expect_err("missing empty property must fail closed");
        assert!(error.to_string().contains("PassEnvironment"));

        let duplicate =
            parse_systemd_properties("Environment=\nEnvironment=\n", "test migration unit")
                .expect_err("duplicate properties must be rejected");
        assert!(duplicate.to_string().contains("duplicate"));
    }

    #[test]
    fn batched_systemd_unit_properties_are_exactly_partitioned_by_id() {
        let output = concat!(
            "Id=writer-a.service\n",
            "LoadState=masked\n",
            "UnitFileState=masked-runtime\n",
            "\n",
            "Id=writer-b.service\n",
            "LoadState=masked\n",
            "UnitFileState=masked-runtime\n",
        );
        let mut blocks = parse_systemd_unit_property_blocks(output, "test batched writers")
            .expect("parse exact systemd unit blocks");
        assert_eq!(blocks.len(), 2);
        assert_eq!(
            blocks
                .remove("writer-a.service")
                .expect("writer A block")
                .get("UnitFileState"),
            Some(&"masked-runtime")
        );
        assert_eq!(
            blocks
                .remove("writer-b.service")
                .expect("writer B block")
                .get("LoadState"),
            Some(&"masked")
        );
        assert!(blocks.is_empty());

        let duplicate = concat!(
            "Id=writer-a.service\n",
            "LoadState=masked\n",
            "\n",
            "Id=writer-a.service\n",
            "LoadState=masked\n",
        );
        let error = parse_systemd_unit_property_blocks(duplicate, "duplicate writer blocks")
            .expect_err("duplicate unit blocks must fail closed");
        assert!(error.to_string().contains("duplicate unit Id"));

        let error = parse_systemd_unit_property_blocks(
            "LoadState=masked\nUnitFileState=masked-runtime\n",
            "missing writer Id",
        )
        .expect_err("a block without Id must fail closed");
        assert!(error.to_string().contains("omits its exact Id"));
    }

    #[test]
    fn systemd_exec_hook_validation_handles_suppressed_empty_arrays() {
        let omitted = parse_systemd_properties(
            "ExecStart={ path=/usr/bin/true ; }\n",
            "test migration unit",
        )
        .expect("parse properties with omitted empty hooks");
        reject_nonempty_systemd_properties(
            &omitted,
            &[
                "ExecCondition",
                "ExecStartPre",
                "ExecStartPost",
                "ExecReload",
                "ExecStop",
                "ExecStopPost",
            ],
            "test migration unit",
        )
        .expect("systemd may suppress empty Exec hook arrays");

        let explicit_empty = parse_systemd_properties(
            "ExecStart={ path=/usr/bin/true ; }\nExecStartPre=\n",
            "test migration unit",
        )
        .expect("parse explicitly empty hook");
        reject_nonempty_systemd_properties(
            &explicit_empty,
            &["ExecStartPre"],
            "test migration unit",
        )
        .expect("an explicitly empty hook is safe");

        let nonempty = parse_systemd_properties(
            "ExecStart={ path=/usr/bin/true ; }\nExecStartPre={ path=/usr/bin/false ; }\n",
            "test migration unit",
        )
        .expect("parse nonempty hook");
        let error =
            reject_nonempty_systemd_properties(&nonempty, &["ExecStartPre"], "test migration unit")
                .expect_err("a configured hook must be rejected");
        assert!(error.to_string().contains("nonempty ExecStartPre"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cursor_parent_lease_serializes_independent_open_descriptions() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().canonicalize().expect("canonical tempdir");
        let first =
            PinnedDirectory::open_exact(&root, "first cursor parent").expect("pin first parent");
        let second =
            PinnedDirectory::open_exact(&root, "second cursor parent").expect("pin second parent");
        let third =
            PinnedDirectory::open_exact(&root, "third cursor parent").expect("pin third parent");

        first
            .acquire_exclusive_migration_lease()
            .expect("acquire first lease");
        let error = second
            .acquire_exclusive_migration_lease()
            .expect_err("second lease must fail while first is held");
        assert!(error.to_string().contains("holds the cursor-parent lease"));
        drop(first);
        third
            .acquire_exclusive_migration_lease()
            .expect("lease becomes available after holder drops");
    }
}
