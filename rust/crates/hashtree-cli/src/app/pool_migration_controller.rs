#[cfg(target_os = "linux")]
use anyhow::Context;
use anyhow::{bail, Result};
#[cfg(target_os = "linux")]
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;

use super::args::PoolMigrationControllerPhase;

#[derive(Debug)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) struct PoolMigrationControllerOptions {
    pub(super) preflight: bool,
    pub(super) rollout_dir: PathBuf,
    pub(super) rollout_id: String,
    pub(super) phase: PoolMigrationControllerPhase,
    pub(super) controller_executable: PathBuf,
    pub(super) controller_systemd_unit: String,
    pub(super) controller_systemd_fragment: PathBuf,
    pub(super) controller_systemd_environment_file: PathBuf,
    pub(super) controller_state_input: PathBuf,
    pub(super) source_baseline_input: PathBuf,
    pub(super) pool_topology_input: PathBuf,
    pub(super) additional_cas: Vec<String>,
    pub(super) writer_units: Vec<String>,
    pub(super) systemd_unit: String,
    pub(super) systemctl: PathBuf,
    pub(super) systemd_fragment: PathBuf,
    pub(super) systemd_environment_file: PathBuf,
    pub(super) service_gid: u32,
    pub(super) migration_binary: PathBuf,
    pub(super) target_data_dir: PathBuf,
    pub(super) pool: PathBuf,
    pub(super) source: PathBuf,
    pub(super) source_external_dir: Option<PathBuf>,
    pub(super) state_file: PathBuf,
    pub(super) batch_size: usize,
    pub(super) max_buffer_mib: u64,
    pub(super) source_read_concurrency: usize,
    pub(super) reopen_batches: usize,
    pub(super) max_items: Option<usize>,
    pub(super) launch_request_wait: Duration,
    pub(super) acknowledgement_wait: Duration,
}

pub(super) fn run_pool_migration_controller(options: PoolMigrationControllerOptions) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        linux::run(options)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = options;
        bail!("Pool migration v3 controller is supported only on Linux with systemd")
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use hashtree_lmdb::PoolMigrationAuditStore;
    use rand::{rngs::OsRng, RngCore};
    use serde::Deserialize;
    use sha2::{Digest, Sha256};
    use std::collections::{HashMap, HashSet};
    use std::ffi::{CString, OsStr};
    use std::fs::{File, OpenOptions};
    use std::io::{ErrorKind, Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};
    use std::thread;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    use super::super::pool_migration_checkpoint::{
        ack_file_name, boottime_millis, request_file_name, timeout_millis,
        validate_checkpoint_operation, CheckpointBrokerAuthorityV3, MigrationCheckpointAckV3,
        MigrationCheckpointRequestV3, CHECKPOINT_ACK_SCHEMA, CHECKPOINT_REQUEST_SCHEMA,
        MAX_CHECKPOINT_BYTES,
    };
    use super::super::pool_migration_evidence::{
        validate_source_evidence_metadata, validate_terminal_catalog_target_evidence,
        SourceEvidenceManifestReaderV3, SourceEvidenceUnionReaderV3,
        ONLINE_TARGET_EVIDENCE_FILE_NAME, SOURCE_EVIDENCE_FILE_NAME,
    };
    use super::super::pool_migration_launch::{
        validate_batched_runtime_masked_final_fence_with_systemctl,
        validate_legacy_worker_activation_fence_with_systemctl,
        validate_legacy_worker_mask_authorities, validate_pool_migration_release_phase,
        validate_pool_migration_topology_authority,
        validate_runtime_masked_writer_owned_properties,
        validate_runtime_masked_writer_units_with_systemctl,
        validate_runtime_writer_mask_authorities, validate_source_read_concurrency,
        validate_stopped_final_batch_size, ControllerAuthorityV3, ControllerStateV3,
        CursorAuthorityV3, FileAuthorityV3, FileIdentityV3, LmdbIdentityV3, NamedFileAuthorityV3,
        PoolAuthorityV3, PoolMigrationLaunchRequestV3, PoolTopologyV3, SourceAuthorityV3,
        ACK_SCHEMA, ATTEMPT_NAMESPACE_NAME, CONTROLLER_STATE_SCHEMA, MAX_FINAL_REOPEN_BATCHES,
        POOL_TOPOLOGY_SCHEMA, REQUEST_FILE_NAME, REQUEST_SCHEMA,
    };
    use super::super::pool_migration_mount::{
        ensure_source_read_only_mount_authority_from_plan, host_execution_namespace_authority,
        plan_source_read_only_mount_authority, plan_source_read_only_mount_teardown,
        require_host_execution_namespace, require_host_mount_administrator,
        teardown_one_source_read_only_mount, validate_source_read_only_mount_authority,
        SourceReadOnlyMountAuthorityV3,
    };
    use super::super::pool_migration_mount_lifecycle::{
        create_full_mount_lifecycle, create_source_mount_lifecycle,
        record_full_mount_lifecycle_closed, record_source_mounts_created,
        record_source_mounts_retained, recover_rollout_mount_lifecycle_state,
        PreparedMountLifecycleV3,
    };
    use super::super::pool_migration_online_audit::{
        compute_online_audit_binding, compute_online_target_fence_binding,
        load_validated_online_target_audit, online_audit_path, OnlineTargetAuditExpectationV3,
        PoolMigrationOnlineTargetAuditCertificationV3, PoolMigrationOnlineTargetAuditReceiptV3,
        ONLINE_TARGET_AUDIT_CERTIFICATION_FILE_NAME, ONLINE_TARGET_AUDIT_CERTIFICATION_SCHEMA,
        ONLINE_TARGET_AUDIT_FILE_NAME, ONLINE_TARGET_AUDIT_SCHEMA,
    };
    use super::super::pool_migration_pinned::PinnedDirectory;
    use super::super::pool_migration_receipt::{
        load_validated_prior_source_terminal_receipts, validate_frozen_source_generation,
        validate_source_terminal_receipt_shape, PoolMigrationSourceTerminalReceiptV3,
        PriorSourceReceiptExpectationV3, ValidatedSourceTerminalReceiptV3,
        MAX_FINAL_SOURCE_RECEIPTS, SOURCE_TERMINAL_SCHEMA,
    };
    use super::super::pool_migration_teardown::{
        capture_bounded_worker_file_authority, read_bounded_file_authority,
        recover_rollout_teardown_state, serialize_json_line, sha256_bytes as teardown_sha256_bytes,
        step_file_name as teardown_step_file_name, validate_completed_teardown_attempt,
        validate_teardown_intent, validate_teardown_receipt, validate_teardown_step,
        BoundedFileAuthorityV3, MountTeardownIntentV3, MountTeardownReceiptV3,
        MountTeardownStepAuthorityV3, MountTeardownStepReceiptV3, MOUNT_TEARDOWN_INTENT_FILE,
        MOUNT_TEARDOWN_INTENT_SCHEMA, MOUNT_TEARDOWN_RECEIPT_FILE, MOUNT_TEARDOWN_RECEIPT_SCHEMA,
        MOUNT_TEARDOWN_STEP_SCHEMA,
    };
    use super::super::pool_migration_terminal_publication::{
        complete_terminal_publication, create_terminal_publication_intent,
        recover_rollout_terminal_publications, PreparedTerminalPublicationV3,
    };

    const MAX_CONTROLLER_STATE_BYTES: u64 = 64 * 1024;
    const MAX_TOPOLOGY_BYTES: u64 = 1024 * 1024;
    const MAX_BASELINE_BYTES: u64 = 16 * 1024 * 1024;
    const MAX_ADDITIONAL_CAS_BYTES: u64 = 16 * 1024 * 1024;
    const MAX_ACK_BYTES: u64 = 1024 * 1024;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct FileSnapshot {
        device: u64,
        inode: u64,
        links: u64,
        len: u64,
        modified_seconds: i64,
        modified_nanoseconds: i64,
        changed_seconds: i64,
        changed_nanoseconds: i64,
        mode: u32,
        uid: u32,
        gid: u32,
    }

    impl FileSnapshot {
        fn from_metadata(metadata: &std::fs::Metadata) -> Self {
            Self {
                device: metadata.dev(),
                inode: metadata.ino(),
                links: metadata.nlink(),
                len: metadata.len(),
                modified_seconds: metadata.mtime(),
                modified_nanoseconds: metadata.mtime_nsec(),
                changed_seconds: metadata.ctime(),
                changed_nanoseconds: metadata.ctime_nsec(),
                mode: metadata.mode(),
                uid: metadata.uid(),
                gid: metadata.gid(),
            }
        }
    }

    struct PinnedAuthorityFile {
        label: String,
        path: PathBuf,
        file: File,
        snapshot: FileSnapshot,
        bytes: Option<Vec<u8>>,
        sha256: String,
    }

    impl PinnedAuthorityFile {
        fn open_bytes(path: &Path, label: &str, maximum: u64) -> Result<Self> {
            let mut pinned = Self::open(path, label)?;
            if pinned.snapshot.len > maximum {
                bail!(
                    "{label} {} is {} bytes; maximum is {maximum}",
                    pinned.path.display(),
                    pinned.snapshot.len
                );
            }
            let mut bytes = Vec::with_capacity(pinned.snapshot.len as usize);
            pinned
                .file
                .read_to_end(&mut bytes)
                .with_context(|| format!("read {label} {}", pinned.path.display()))?;
            if bytes.len() as u64 != pinned.snapshot.len {
                bail!("{label} length changed while it was read");
            }
            pinned.ensure_unchanged()?;
            pinned.sha256 = sha256_bytes(&bytes);
            pinned.bytes = Some(bytes);
            Ok(pinned)
        }

        fn open_hashed(path: &Path, label: &str) -> Result<Self> {
            let mut pinned = Self::open(path, label)?;
            let mut hasher = Sha256::new();
            std::io::copy(&mut pinned.file, &mut hasher)
                .with_context(|| format!("hash {label} {}", pinned.path.display()))?;
            pinned.ensure_unchanged()?;
            pinned.sha256 = hex::encode(hasher.finalize());
            Ok(pinned)
        }

        fn open(path: &Path, label: &str) -> Result<Self> {
            let path = canonical_regular_path(path, label)?;
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&path)
                .with_context(|| format!("open {label} {}", path.display()))?;
            let metadata = file
                .metadata()
                .with_context(|| format!("inspect {label} {}", path.display()))?;
            if !metadata.file_type().is_file() {
                bail!("{label} {} is not a regular file", path.display());
            }
            if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
                bail!("{label} must be root-owned and not group/world writable");
            }
            Ok(Self {
                label: label.to_string(),
                path,
                file,
                snapshot: FileSnapshot::from_metadata(&metadata),
                bytes: None,
                sha256: String::new(),
            })
        }

        fn bytes(&self) -> &[u8] {
            self.bytes
                .as_deref()
                .expect("byte authority opened with open_bytes")
        }

        fn authority(&self) -> FileAuthorityV3 {
            FileAuthorityV3 {
                path: self.path.clone(),
                sha256: self.sha256.clone(),
            }
        }

        fn authority_at(&self, path: PathBuf) -> FileAuthorityV3 {
            FileAuthorityV3 {
                path,
                sha256: self.sha256.clone(),
            }
        }

        fn ensure_unchanged(&self) -> Result<()> {
            let open = self
                .file
                .metadata()
                .with_context(|| format!("reinspect open {}", self.label))?;
            let path = std::fs::symlink_metadata(&self.path)
                .with_context(|| format!("reinspect {}", self.label))?;
            if FileSnapshot::from_metadata(&open) != self.snapshot
                || FileSnapshot::from_metadata(&path) != self.snapshot
            {
                bail!("{} changed after controller preflight", self.label);
            }
            Ok(())
        }
    }

    fn hash_open_file_with_snapshot(
        file: &mut File,
        expected: FileSnapshot,
        label: &str,
    ) -> Result<String> {
        let opened = file
            .metadata()
            .with_context(|| format!("inspect open {label}"))?;
        if FileSnapshot::from_metadata(&opened) != expected {
            bail!("{label} inode or metadata differs from its pinned authority");
        }
        let mut hasher = Sha256::new();
        std::io::copy(file, &mut hasher).with_context(|| format!("hash open {label}"))?;
        let after = file
            .metadata()
            .with_context(|| format!("reinspect open {label}"))?;
        if FileSnapshot::from_metadata(&after) != expected {
            bail!("{label} changed while it was hashed");
        }
        Ok(hex::encode(hasher.finalize()))
    }

    fn validate_running_controller_executable(expected: &PinnedAuthorityFile) -> Result<()> {
        let mut running = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC)
            .open("/proc/self/exe")
            .context("open running controller /proc/self/exe")?;
        let sha256 = hash_open_file_with_snapshot(
            &mut running,
            expected.snapshot,
            "running controller executable",
        )?;
        if sha256 != expected.sha256 {
            bail!("running controller /proc/self/exe SHA-256 differs from pinned executable");
        }
        expected.ensure_unchanged()
    }

    struct PreparedLaunch {
        options: PoolMigrationControllerOptions,
        nonce: String,
        boot_id: String,
        attempts_dir: PathBuf,
        attempt_dir: PathBuf,
        request_path: PathBuf,
        ack_path: PathBuf,
        state_output: PathBuf,
        baseline_output: PathBuf,
        topology_output: PathBuf,
        controller_executable: PinnedAuthorityFile,
        controller_systemd_fragment: PinnedAuthorityFile,
        controller_systemd_environment_file: PinnedAuthorityFile,
        controller_systemd_invocation_id: String,
        systemctl: PinnedAuthorityFile,
        migration_binary: PinnedAuthorityFile,
        systemd_fragment: PinnedAuthorityFile,
        controller_state_input: PinnedAuthorityFile,
        source_baseline_input: PinnedAuthorityFile,
        pool_topology_input: PinnedAuthorityFile,
        additional_cas_inputs: Vec<(String, PinnedAuthorityFile)>,
        controller_state: ControllerStateV3,
        pool_topology: PoolTopologyV3,
        source_identity: LmdbIdentityV3,
        source_external_identity: Option<FileIdentityV3>,
        pool_identity: LmdbIdentityV3,
        cursor: CursorAuthorityV3,
        environment_bytes: Vec<u8>,
        expected_argv: Vec<String>,
        source_receipts: Vec<ValidatedSourceTerminalReceiptV3>,
        broker_pid: u32,
        broker_proc_start_time_ticks: u64,
    }

    enum PreparedLaunchOutcome {
        Ready(PreparedLaunch),
        RecoveredTerminal {
            rollout_id: String,
            phase: PoolMigrationControllerPhase,
        },
    }

    #[derive(Debug, PartialEq, Eq)]
    struct ProcessIdentity {
        invocation_id: String,
        main_pid: u32,
        start_time_ticks: u64,
        uid: u32,
        gid: u32,
    }

    struct ProcessIdentityExpectation<'a> {
        systemctl: &'a Path,
        unit: &'a str,
        fragment_path: &'a Path,
        environment_file: &'a Path,
        binary: &'a Path,
        service_gid: u32,
        argv: &'a [String],
        request_wait: Duration,
        controller_unit: &'a str,
    }

    fn checkpoint_requires_runtime_writer_masks(
        phase: PoolMigrationControllerPhase,
        target_writers_fenced: bool,
    ) -> bool {
        phase.is_final_stopped() || target_writers_fenced
    }

    #[derive(Clone, Default)]
    struct CheckpointProgress {
        online_evidence_published: bool,
        online_audit_published: bool,
        online_ready: bool,
        source_keyset_audited: bool,
        source_evidence_published: bool,
        source_reconciliations: u64,
        source_generation_fingerprinted: bool,
        source_receipt_published: bool,
        target_terminal_audited: bool,
        terminal_receipt_published: bool,
        terminal_ready: bool,
    }

    struct CheckpointBrokerCompletion {
        checkpoint_count: u64,
        checkpoint_systemctl_subprocess_count: u64,
        terminal_receipt_sha256: Option<String>,
        mount_teardown_receipt: Option<FileAuthorityV3>,
        terminal_publication_receipt: Option<FileAuthorityV3>,
        source_terminal_certification: Option<FileAuthorityV3>,
        online_target_audit_certification: Option<FileAuthorityV3>,
    }

    struct PreparedMountTeardown {
        intent: MountTeardownIntentV3,
        intent_path: PathBuf,
        intent_sha256: String,
    }

    pub(super) fn run(options: PoolMigrationControllerOptions) -> Result<()> {
        require_root()?;
        let mut host_paths = vec![
            (options.rollout_dir.as_path(), "Pool migration rollout"),
            (
                options.controller_executable.as_path(),
                "controller executable",
            ),
            (
                options.controller_systemd_fragment.as_path(),
                "controller systemd fragment",
            ),
            (
                options.controller_systemd_environment_file.as_path(),
                "controller systemd environment file",
            ),
            (
                options.controller_state_input.as_path(),
                "controller-state input",
            ),
            (
                options.source_baseline_input.as_path(),
                "source-baseline input",
            ),
            (options.pool_topology_input.as_path(), "Pool-topology input"),
            (options.systemctl.as_path(), "systemctl executable"),
            (
                options.systemd_fragment.as_path(),
                "worker systemd fragment",
            ),
            (options.migration_binary.as_path(), "migration binary"),
            (options.target_data_dir.as_path(), "target Hashtree data"),
            (options.pool.as_path(), "target Pool catalog"),
            (options.source.as_path(), "source LMDB"),
        ];
        if let Some(path) = options.source_external_dir.as_deref() {
            host_paths.push((path, "source external corpus"));
        }
        require_host_execution_namespace(&host_paths)?;
        require_host_mount_administrator()?;
        validate_durability_environment()?;
        let prepared = match PreparedLaunch::prepare(options)? {
            PreparedLaunchOutcome::Ready(prepared) => prepared,
            PreparedLaunchOutcome::RecoveredTerminal { rollout_id, phase } => {
                println!(
                    "{}",
                    serde_json::to_string(&json!({
                        "schema": "hashtree-pool-migration-controller-recovery/v3",
                        "status": "terminal-recovered",
                        "rolloutId": rollout_id,
                        "phase": phase.as_protocol_str(),
                    }))
                    .context("serialize Pool migration terminal recovery result")?
                );
                return Ok(());
            }
        };
        if prepared.options.preflight {
            println!(
                "{}",
                serde_json::to_string(&json!({
                    "schema": "hashtree-pool-migration-controller-preflight/v3",
                    "status": "ok",
                    "mutation": false,
                    "rolloutId": prepared.options.rollout_id,
                    "phase": prepared.options.phase.as_protocol_str(),
                    "systemdUnit": prepared.options.systemd_unit,
                    "prospectiveNonce": prepared.nonce,
                }))
                .context("serialize Pool migration controller preflight")?
            );
            return Ok(());
        }
        prepared.launch()
    }

    impl PreparedLaunch {
        fn prepare(options: PoolMigrationControllerOptions) -> Result<PreparedLaunchOutcome> {
            validate_options(&options)?;
            let broker_pid = std::process::id();
            let broker_proc_start_time_ticks = process_start_time(broker_pid)?;
            let rollout_dir =
                canonical_root_directory(&options.rollout_dir, "Pool migration rollout")?;
            require_directory_service_search(
                &rollout_dir,
                options.service_gid,
                "Pool migration rollout",
            )?;
            if rollout_dir
                .file_name()
                .and_then(OsStr::to_str)
                .is_none_or(|name| name != options.rollout_id)
            {
                bail!("Pool migration rollout directory name must equal --rollout-id");
            }
            let attempts_dir = rollout_dir.join(ATTEMPT_NAMESPACE_NAME);
            validate_planned_attempt_namespace(&attempts_dir)?;
            let boot_id = current_boot_id()?;
            if !options.preflight {
                recover_rollout_teardown_state(&attempts_dir, &options.rollout_id, &boot_id)?;
                recover_rollout_mount_lifecycle_state(
                    &attempts_dir,
                    &options.rollout_id,
                    &boot_id,
                )?;
            }

            let nonce = fresh_nonce();
            let attempt_dir = attempts_dir.join(&nonce);
            let request_path = attempt_dir.join(REQUEST_FILE_NAME);
            let ack_path = attempt_dir.join("launch-ack.json");
            let state_output = rollout_dir.join(format!("controller-state-{nonce}.json"));
            let baseline_output = rollout_dir.join(format!("source-baseline-{nonce}.manifest"));
            let topology_output = rollout_dir.join(format!("pool-topology-{nonce}.json"));
            for (path, label) in [
                (&attempt_dir, "fresh attempt directory"),
                (&request_path, "fresh launch request"),
                (&ack_path, "fresh launch acknowledgement"),
                (&state_output, "fresh controller-state CAS"),
                (&baseline_output, "fresh source-baseline CAS"),
                (&topology_output, "fresh Pool-topology CAS"),
                (
                    &options.systemd_environment_file,
                    "fresh systemd environment file",
                ),
            ] {
                require_absent(path, label)?;
            }
            canonical_root_parent_for_absent(
                &options.systemd_environment_file,
                "systemd environment file",
            )?;

            let controller_executable = PinnedAuthorityFile::open_hashed(
                &options.controller_executable,
                "controller executable",
            )?;
            let running_executable = std::env::current_exe()
                .context("resolve running Pool migration controller executable")?
                .canonicalize()
                .context("canonicalize running Pool migration controller executable")?;
            if controller_executable.path != running_executable {
                bail!(
                    "--controller-executable {} does not equal running /proc/self/exe {}",
                    controller_executable.path.display(),
                    running_executable.display()
                );
            }
            validate_running_controller_executable(&controller_executable)?;
            let controller_systemd_fragment = PinnedAuthorityFile::open_hashed(
                &options.controller_systemd_fragment,
                "controller systemd unit fragment",
            )?;
            if controller_systemd_fragment
                .path
                .file_name()
                .and_then(OsStr::to_str)
                != Some("hashtree-pool-migration-controller@.service")
            {
                bail!(
                    "controller systemd fragment must be named hashtree-pool-migration-controller@.service"
                );
            }
            let controller_systemd_environment_file = PinnedAuthorityFile::open_hashed(
                &options.controller_systemd_environment_file,
                "controller systemd environment file",
            )?;
            let controller_systemd_invocation_id = std::env::var("INVOCATION_ID")
                .context("dedicated controller service did not provide INVOCATION_ID")?;
            require_lower_hex(
                "controller systemd invocation ID",
                &controller_systemd_invocation_id,
                32,
            )?;
            let migration_binary =
                PinnedAuthorityFile::open_hashed(&options.migration_binary, "migration binary")?;
            let systemctl =
                PinnedAuthorityFile::open_hashed(&options.systemctl, "systemctl executable")?;
            if systemctl.path != Path::new("/usr/bin/systemctl")
                && systemctl.path != Path::new("/bin/systemctl")
            {
                bail!(
                    "--systemctl must be the exact canonical /usr/bin/systemctl or /bin/systemctl"
                );
            }
            validate_running_controller_service(
                &systemctl.path,
                &options.controller_systemd_unit,
                &controller_systemd_invocation_id,
                &controller_systemd_fragment,
                &controller_systemd_environment_file.path,
                &controller_executable.path,
                broker_pid,
            )?;
            let systemd_fragment = PinnedAuthorityFile::open_hashed(
                &options.systemd_fragment,
                "systemd unit fragment",
            )?;
            if systemd_fragment.path.file_name().and_then(OsStr::to_str)
                != Some("hashtree-pool-migration-worker@.service")
            {
                bail!("systemd fragment must be named hashtree-pool-migration-worker@.service");
            }
            require_service_access(
                &controller_executable.snapshot,
                options.service_gid,
                false,
                "controller executable",
            )?;
            require_service_access(
                &migration_binary.snapshot,
                options.service_gid,
                true,
                "migration binary",
            )?;
            require_service_access(
                &systemd_fragment.snapshot,
                options.service_gid,
                false,
                "systemd unit fragment",
            )?;
            let controller_state_input = PinnedAuthorityFile::open_bytes(
                &options.controller_state_input,
                "controller-state input",
                MAX_CONTROLLER_STATE_BYTES,
            )?;
            let source_baseline_input = PinnedAuthorityFile::open_bytes(
                &options.source_baseline_input,
                "source-baseline input",
                MAX_BASELINE_BYTES,
            )?;
            if source_baseline_input.bytes().is_empty() {
                bail!("source-baseline input must not be empty");
            }
            let pool_topology_input = PinnedAuthorityFile::open_bytes(
                &options.pool_topology_input,
                "Pool-topology input",
                MAX_TOPOLOGY_BYTES,
            )?;
            let controller_state: ControllerStateV3 =
                serde_json::from_slice(controller_state_input.bytes())
                    .context("parse strict controller-state input v3")?;
            let pool_topology: PoolTopologyV3 = serde_json::from_slice(pool_topology_input.bytes())
                .context("parse strict Pool-topology input v3")?;

            let source = canonical_directory_path(&options.source, "source LMDB")?;
            let source_identity = lmdb_identity(&source, "source LMDB")?;
            let source_external_identity = options
                .source_external_dir
                .as_deref()
                .map(|path| {
                    let canonical = canonical_directory_path(path, "source external directory")?;
                    file_identity(&canonical, "source external directory")
                })
                .transpose()?;
            let pool = canonical_directory_path(&options.pool, "target Pool catalog")?;
            let pool_identity = lmdb_identity(&pool, "target Pool catalog")?;
            let target_data =
                canonical_directory_path(&options.target_data_dir, "target Hashtree data")?;
            let expected_pool_alias = target_data.join(hashtree_lmdb::SHARED_BLOB_POOL_DIR_NAME);
            let expected_pool =
                resolved_directory_path(&expected_pool_alias, "target Hashtree shared Pool")?;
            if pool != expected_pool {
                bail!(
                    "--pool must resolve to the same directory as {} for --target-data-dir",
                    expected_pool_alias.display(),
                );
            }

            let topology_authority = pool_topology_input.authority();
            validate_pool_migration_topology_authority(&topology_authority, &pool)
                .context("validate live Pool topology input")?;
            validate_topology_summary(&pool_topology, &pool)?;
            validate_topology_host_filesystems(&pool_topology)?;
            validate_controller_state_input(
                &controller_state,
                &options,
                &boot_id,
                source_identity,
                source_external_identity,
                pool_identity,
                &pool_topology,
                &pool_topology_input.sha256,
                &systemctl.path,
            )?;

            if !options.preflight {
                let recovered = recover_rollout_terminal_publications(
                    &attempts_dir,
                    &options.rollout_id,
                    &boot_id,
                    |attempt_dir, publication| {
                        authorize_terminal_recovery(
                            attempt_dir,
                            publication,
                            &options,
                            &boot_id,
                            &controller_state,
                            &controller_state_input.sha256,
                            &source_baseline_input.sha256,
                            &pool_topology,
                            &pool_topology_input.sha256,
                            source_identity,
                            source_external_identity,
                            pool_identity,
                            &systemctl.path,
                        )
                    },
                )?;
                if recovered != 0 {
                    return Ok(PreparedLaunchOutcome::RecoveredTerminal {
                        rollout_id: options.rollout_id,
                        phase: options.phase,
                    });
                }
            }

            let cursor = capture_cursor_authority(&options.state_file, options.phase)?;
            let additional_cas_specs = parse_additional_cas(&options.additional_cas)?;
            let mut additional_cas_inputs = Vec::with_capacity(additional_cas_specs.len());
            for (label, path) in additional_cas_specs {
                let authority = PinnedAuthorityFile::open_bytes(
                    &path,
                    &format!("additional CAS {label}"),
                    MAX_ADDITIONAL_CAS_BYTES,
                )?;
                require_service_access(
                    &authority.snapshot,
                    options.service_gid,
                    false,
                    &format!("additional CAS {label}"),
                )?;
                additional_cas_inputs.push((label, authority));
            }
            let receipt_authorities = additional_cas_inputs
                .iter()
                .map(|(label, file)| NamedFileAuthorityV3 {
                    label: label.clone(),
                    path: file.path.clone(),
                    sha256: file.sha256.clone(),
                })
                .collect::<Vec<_>>();
            let source_receipts = load_validated_prior_source_terminal_receipts(
                &receipt_authorities,
                &PriorSourceReceiptExpectationV3 {
                    boot_id: &boot_id,
                    pool_path: &pool,
                    pool_lmdb_identity: pool_identity,
                    pool_topology_sha256: &pool_topology_input.sha256,
                    pool_manifest_sha256: &controller_state.pool_manifest_sha256,
                    pool_topology: &pool_topology,
                    stopped_writer_units: &controller_state.stopped_writer_units,
                    writer_unit_masks: &controller_state.writer_unit_masks,
                    legacy_worker_template_mask: &controller_state.legacy_worker_template_mask,
                    legacy_worker_instance_masks: &controller_state.legacy_worker_instance_masks,
                    expected_service_gid: Some(options.service_gid),
                    validate_physical_generation: false,
                },
            )?;
            let receipt_sha256 = source_receipts
                .iter()
                .map(|validated| validated.authority_sha256.clone())
                .collect::<Vec<_>>();
            if receipt_sha256 != controller_state.source_terminal_receipt_sha256 {
                bail!(
                    "source-terminal receipt CAS set differs from the exact controller-state receipt set"
                );
            }
            let online_target_audit = load_validated_online_target_audit(
                &receipt_authorities,
                &OnlineTargetAuditExpectationV3 {
                    rollout_id: &options.rollout_id,
                    worker_binary_sha256: &migration_binary.sha256,
                    source_baseline_sha256: &source_baseline_input.sha256,
                    source_path: &source,
                    source_lmdb_identity: source_identity,
                    source_external_path: options.source_external_dir.as_deref(),
                    source_external_identity,
                    pool_path: &pool,
                    pool_lmdb_identity: pool_identity,
                    pool_topology_sha256: &pool_topology_input.sha256,
                    pool_manifest_sha256: &controller_state.pool_manifest_sha256,
                    target_writer_units: &controller_state.stopped_writer_units,
                    target_writer_unit_masks: &controller_state.writer_unit_masks,
                    legacy_worker_template_mask: &controller_state.legacy_worker_template_mask,
                    legacy_worker_instance_masks: &controller_state.legacy_worker_instance_masks,
                    expected_service_gid: options.service_gid,
                    validate_evidence_content: false,
                },
            )?;
            match options.phase {
                PoolMigrationControllerPhase::FinalStoppedSource
                    if online_target_audit.is_none() =>
                {
                    bail!(
                        "final-stopped-source requires one root-certified online target audit CAS"
                    )
                }
                PoolMigrationControllerPhase::OnlineBounded
                | PoolMigrationControllerPhase::FinalStoppedFull
                    if online_target_audit.is_some() =>
                {
                    bail!("online target audit CAS is accepted only by final-stopped-source")
                }
                _ => {}
            }

            validate_authority_isolation(
                &rollout_dir,
                &attempts_dir,
                &source,
                options.source_external_dir.as_deref(),
                &pool,
                &pool_topology,
                &cursor.path,
                &[
                    &controller_executable.path,
                    &controller_systemd_fragment.path,
                    &controller_systemd_environment_file.path,
                    &systemctl.path,
                    &migration_binary.path,
                    &systemd_fragment.path,
                    &controller_state_input.path,
                    &source_baseline_input.path,
                    &pool_topology_input.path,
                    &options.systemd_environment_file,
                    &state_output,
                    &baseline_output,
                    &topology_output,
                ],
                &additional_cas_inputs,
            )?;

            let environment_bytes = build_environment(
                &options,
                &request_path,
                &source,
                options.source_external_dir.as_deref(),
                &cursor.path,
            )?;
            let expected_argv = build_worker_argv(
                &options,
                &migration_binary.path,
                &request_path,
                &source,
                options.source_external_dir.as_deref(),
                &cursor.path,
            )?;
            validate_pristine_systemd_unit(
                &systemctl.path,
                &options.systemd_unit,
                &systemd_fragment,
                &options.systemd_environment_file,
                &migration_binary.path,
                &options.controller_systemd_unit,
            )?;

            Ok(PreparedLaunchOutcome::Ready(Self {
                options,
                nonce,
                boot_id,
                attempts_dir,
                attempt_dir,
                request_path,
                ack_path,
                state_output,
                baseline_output,
                topology_output,
                controller_executable,
                controller_systemd_fragment,
                controller_systemd_environment_file,
                controller_systemd_invocation_id,
                systemctl,
                migration_binary,
                systemd_fragment,
                controller_state_input,
                source_baseline_input,
                pool_topology_input,
                additional_cas_inputs,
                controller_state,
                pool_topology,
                source_identity,
                source_external_identity,
                pool_identity,
                cursor,
                environment_bytes,
                expected_argv,
                source_receipts,
                broker_pid,
                broker_proc_start_time_ticks,
            }))
        }

        fn launch(self) -> Result<()> {
            self.revalidate_inputs()?;
            create_attempt_namespace(&self.attempts_dir)?;
            create_attempt_directory(&self.attempt_dir, self.options.service_gid)?;
            durable_create_atomic(
                &self.state_output,
                self.controller_state_input.bytes(),
                0o440,
                0,
                self.options.service_gid,
                &self.nonce,
            )?;
            durable_create_atomic(
                &self.baseline_output,
                self.source_baseline_input.bytes(),
                0o440,
                0,
                self.options.service_gid,
                &self.nonce,
            )?;
            durable_create_atomic(
                &self.topology_output,
                self.pool_topology_input.bytes(),
                0o440,
                0,
                self.options.service_gid,
                &self.nonce,
            )?;
            durable_create_atomic(
                &self.options.systemd_environment_file,
                &self.environment_bytes,
                0o644,
                0,
                0,
                &self.nonce,
            )?;
            self.revalidate_inputs()?;
            validate_created_authority(
                &self.state_output,
                &self.controller_state_input.sha256,
                "controller-state CAS",
            )?;
            validate_created_authority(
                &self.baseline_output,
                &self.source_baseline_input.sha256,
                "source-baseline CAS",
            )?;
            validate_created_authority(
                &self.topology_output,
                &self.pool_topology_input.sha256,
                "Pool-topology CAS",
            )?;
            validate_created_authority(
                &self.options.systemd_environment_file,
                &sha256_bytes(&self.environment_bytes),
                "systemd environment file",
            )?;
            validate_pristine_systemd_unit(
                &self.options.systemctl,
                &self.options.systemd_unit,
                &self.systemd_fragment,
                &self.options.systemd_environment_file,
                &self.migration_binary.path,
                &self.options.controller_systemd_unit,
            )?;
            let online_audit = self.prepare_online_audit_store()?;

            let mount_lifecycle = self.prepare_mount_lifecycle()?;
            if let Err(start_error) = systemctl_success(
                &self.options.systemctl,
                &["start", "--no-block", &self.options.systemd_unit],
                "start Pool migration systemd unit",
            ) {
                let _ = systemctl_output(
                    &self.options.systemctl,
                    &["stop", &self.options.systemd_unit],
                );
                return match self.recover_failed_mount_state() {
                    Ok(()) => Err(start_error),
                    Err(recovery_error) => Err(anyhow::anyhow!(
                        "Pool migration worker start failed: {start_error:#}; mount lifecycle recovery also failed: {recovery_error:#}"
                    )),
                };
            }
            let mut guard = StartedUnitGuard {
                systemctl: self.options.systemctl.clone(),
                unit: self.options.systemd_unit.clone(),
                armed: true,
            };
            let result = self.complete_launch(mount_lifecycle.as_ref(), online_audit.as_ref());
            if result.is_ok() {
                guard.armed = false;
                return result;
            }
            let launch_error = result.expect_err("checked failed launch result");
            if let Err(stop_error) = guard.stop() {
                return Err(anyhow::anyhow!(
                    "Pool migration launch failed: {launch_error:#}; stopping its worker also failed: {stop_error:#}"
                ));
            }
            match self.recover_failed_mount_state() {
                Ok(()) => Err(launch_error),
                Err(recovery_error) => Err(anyhow::anyhow!(
                    "Pool migration launch failed: {launch_error:#}; mount lifecycle recovery also failed: {recovery_error:#}"
                )),
            }
        }

        fn prepare_mount_lifecycle(&self) -> Result<Option<PreparedMountLifecycleV3>> {
            match self.options.phase {
                PoolMigrationControllerPhase::OnlineBounded => Ok(None),
                PoolMigrationControllerPhase::FinalStoppedSource => {
                    let plan = plan_source_read_only_mount_authority(
                        &self.options.source,
                        self.source_identity,
                        self.options.source_external_dir.as_deref(),
                        self.source_external_identity,
                    )
                    .context("durably plan source read-only self-bind mounts")?;
                    create_source_mount_lifecycle(
                        &self.attempt_dir,
                        &self.boot_id,
                        &self.options.rollout_id,
                        &self.nonce,
                        &self.controller_state_input.sha256,
                        &self.options.source,
                        self.source_identity,
                        self.options.source_external_dir.as_deref(),
                        self.source_external_identity,
                        plan,
                    )
                    .map(Some)
                }
                PoolMigrationControllerPhase::FinalStoppedFull => {
                    let authorities = self
                        .source_receipts
                        .iter()
                        .map(|source| source.receipt.source_read_only_mounts.clone())
                        .collect();
                    create_full_mount_lifecycle(
                        &self.attempt_dir,
                        &self.boot_id,
                        &self.options.rollout_id,
                        &self.nonce,
                        &self.controller_state_input.sha256,
                        authorities,
                    )
                    .map(Some)
                }
            }
        }

        fn prepare_online_audit_store(&self) -> Result<Option<PoolMigrationAuditStore>> {
            if self.options.phase != PoolMigrationControllerPhase::OnlineBounded {
                return Ok(None);
            }
            let path = online_audit_path(&self.options.state_file)?;
            let created = match std::fs::create_dir(&path) {
                Ok(()) => true,
                Err(error) if error.kind() == ErrorKind::AlreadyExists => false,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("create root-owned online audit {}", path.display())
                    })
                }
            };
            if created {
                set_root_service_path_authority(
                    &path,
                    true,
                    self.options.service_gid,
                    0o750,
                    "online audit directory",
                )?;
                File::open(
                    path.parent()
                        .context("online audit directory has no parent")?,
                )?
                .sync_all()
                .context("fsync online audit parent")?;
            } else {
                validate_root_service_path_authority(
                    &path,
                    true,
                    self.options.service_gid,
                    0o750,
                    "online audit directory",
                )?;
                for (name, mode) in [("data.mdb", 0o640), ("lock.mdb", 0o660)] {
                    validate_root_service_path_authority(
                        &path.join(name),
                        false,
                        self.options.service_gid,
                        mode,
                        &format!("online audit {name}"),
                    )?;
                }
            }
            let manifest_sha256 =
                hashtree_core::from_hex(&self.controller_state.pool_manifest_sha256)
                    .context("decode online audit Pool manifest authority")?;
            let binding = compute_online_audit_binding(
                &self.options.rollout_id,
                &self.migration_binary.sha256,
                &self.source_baseline_input.sha256,
                self.source_identity,
                self.source_external_identity,
                self.pool_identity,
                &self.pool_topology_input.sha256,
                manifest_sha256,
            )?;
            let store = PoolMigrationAuditStore::open(&path, binding)
                .context("open root-owned online migration audit")?;
            if self.controller_state.target_writers_fenced {
                let target_fence_binding = compute_online_target_fence_binding(
                    &self.options.rollout_id,
                    &self.controller_state.stopped_writer_units,
                    &self.controller_state.writer_unit_masks,
                    &self.controller_state.legacy_worker_template_mask,
                    &self.controller_state.legacy_worker_instance_masks,
                )?;
                store
                    .begin_target_fenced_epoch(target_fence_binding)
                    .context("start or resume root-owned target-fenced proof epoch")?;
            } else if store.target_fence_binding()?.is_some() {
                bail!(
                    "online migration target-fence epoch already began; keep the exact target fence held"
                );
            }
            if created {
                set_root_service_path_authority(
                    &path.join("data.mdb"),
                    false,
                    self.options.service_gid,
                    0o640,
                    "online audit data.mdb",
                )?;
                set_root_service_path_authority(
                    &path.join("lock.mdb"),
                    false,
                    self.options.service_gid,
                    0o660,
                    "online audit lock.mdb",
                )?;
                File::open(&path)?
                    .sync_all()
                    .context("fsync online audit directory")?;
            }
            store.validate_binding()?;
            Ok(Some(store))
        }

        fn recover_failed_mount_state(&self) -> Result<()> {
            recover_rollout_teardown_state(
                &self.attempts_dir,
                &self.options.rollout_id,
                &self.boot_id,
            )?;
            recover_rollout_mount_lifecycle_state(
                &self.attempts_dir,
                &self.options.rollout_id,
                &self.boot_id,
            )?;
            recover_rollout_terminal_publications(
                &self.attempts_dir,
                &self.options.rollout_id,
                &self.boot_id,
                |attempt_dir, publication| {
                    authorize_terminal_recovery(
                        attempt_dir,
                        publication,
                        &self.options,
                        &self.boot_id,
                        &self.controller_state,
                        &self.controller_state_input.sha256,
                        &self.source_baseline_input.sha256,
                        &self.pool_topology,
                        &self.pool_topology_input.sha256,
                        self.source_identity,
                        self.source_external_identity,
                        self.pool_identity,
                        &self.systemctl.path,
                    )
                },
            )
            .map(|_| ())
        }

        fn complete_launch(
            &self,
            mount_lifecycle: Option<&PreparedMountLifecycleV3>,
            online_audit: Option<&PoolMigrationAuditStore>,
        ) -> Result<()> {
            let process = wait_for_process_identity(&ProcessIdentityExpectation {
                systemctl: &self.options.systemctl,
                unit: &self.options.systemd_unit,
                fragment_path: &self.systemd_fragment.path,
                environment_file: &self.options.systemd_environment_file,
                binary: &self.migration_binary.path,
                service_gid: self.options.service_gid,
                argv: &self.expected_argv,
                request_wait: self.options.launch_request_wait,
                controller_unit: &self.options.controller_systemd_unit,
            })?;
            if process.invocation_id.is_empty()
                || process.main_pid == 0
                || process.start_time_ticks == 0
            {
                bail!("systemd returned an incomplete Pool migration process identity");
            }
            self.revalidate_inputs()?;
            let mut mounted_lifecycle_authority = None;
            let source_read_only_mounts = match self.options.phase {
                PoolMigrationControllerPhase::OnlineBounded => {
                    if mount_lifecycle.is_some() {
                        bail!("online-bounded launch unexpectedly has a mount lifecycle");
                    }
                    if self.controller_state.target_writers_fenced {
                        self.revalidate_masks_and_census(&process, true)
                            .context("pre-launch target-fenced online writer-handle census")?;
                    }
                    None
                }
                PoolMigrationControllerPhase::FinalStoppedSource => {
                    let lifecycle =
                        mount_lifecycle.context("source-final launch has no mount lifecycle")?;
                    if lifecycle.intent.phase != "final-stopped-source" {
                        bail!("source-final launch has a mismatched mount lifecycle");
                    }
                    self.revalidate_masks_and_census(&process, true)
                        .context("pre-mount final writer-handle census")?;
                    let authority = ensure_source_read_only_mount_authority_from_plan(
                        lifecycle
                            .intent
                            .source_plan
                            .as_ref()
                            .context("source-final mount lifecycle has no pre-mount plan")?,
                        &self.options.source,
                        self.source_identity,
                        self.options.source_external_dir.as_deref(),
                        self.source_external_identity,
                    )
                    .context("establish source read-only self-bind mounts")?;
                    mounted_lifecycle_authority = Some(record_source_mounts_created(
                        &self.attempt_dir,
                        lifecycle,
                        authority.clone(),
                    )?);
                    self.revalidate_checkpoint_fence(&process, &authority, false)
                        .context("post-mount final writer-handle census")?;
                    Some(authority)
                }
                PoolMigrationControllerPhase::FinalStoppedFull => {
                    let lifecycle =
                        mount_lifecycle.context("full-final launch has no mount lifecycle")?;
                    if lifecycle.intent.phase != "final-stopped-full" {
                        bail!("full-final launch has a mismatched mount lifecycle");
                    }
                    self.revalidate_masks_and_census(&process, true)
                        .context("pre-launch full-final writer-handle census")?;
                    let authority = self
                        .matching_receipt_source()?
                        .receipt
                        .source_read_only_mounts
                        .clone();
                    self.revalidate_checkpoint_fence(&process, &authority, true)
                        .context("full-final receipt-owned source fence")?;
                    Some(authority)
                }
            };
            let request = self.build_request(&process, source_read_only_mounts)?;
            let mut request_bytes =
                serde_json::to_vec(&request).context("serialize Pool migration launch request")?;
            request_bytes.push(b'\n');
            let request_sha256 = sha256_bytes(&request_bytes);
            durable_create_atomic(
                &self.request_path,
                &request_bytes,
                0o640,
                0,
                self.options.service_gid,
                &self.nonce,
            )?;
            let ack_bytes = wait_for_ack(
                &self.ack_path,
                &self.options.systemctl,
                &self.options.systemd_unit,
                self.options.acknowledgement_wait,
                process.uid,
                process.gid,
            )?;
            validate_ack(
                &ack_bytes,
                &request,
                &request_sha256,
                &self.controller_state,
                &self.pool_topology,
            )?;
            let launch_ack_sha256 = sha256_bytes(&ack_bytes);
            let completion = self.run_checkpoint_broker(
                &process,
                &request,
                &request_sha256,
                &launch_ack_sha256,
                mount_lifecycle,
                mounted_lifecycle_authority.as_ref(),
                online_audit,
            )?;
            println!(
                "{}",
                serde_json::to_string(&json!({
                    "schema": "hashtree-pool-migration-controller-result/v3",
                    "status": "completed",
                    "rolloutId": self.options.rollout_id,
                    "phase": self.options.phase.as_protocol_str(),
                    "nonce": self.nonce,
                    "systemdUnit": self.options.systemd_unit,
                    "systemdInvocationId": process.invocation_id,
                    "mainPid": process.main_pid,
                    "procStartTimeTicks": process.start_time_ticks,
                    "requestPath": self.request_path,
                    "requestSha256": request_sha256,
                    "ackPath": self.ack_path,
                    "ackSha256": launch_ack_sha256,
                    "authorizedCheckpoints": completion.checkpoint_count,
                    "checkpointSystemctlSubprocesses": completion.checkpoint_systemctl_subprocess_count,
                    "terminalReceiptSha256": completion.terminal_receipt_sha256,
                    "mountTeardownReceipt": completion.mount_teardown_receipt,
                    "terminalPublicationReceipt": completion.terminal_publication_receipt,
                    "sourceTerminalCertification": completion.source_terminal_certification,
                    "onlineTargetAuditCertification": completion.online_target_audit_certification,
                }))
                .context("serialize Pool migration controller result")?
            );
            Ok(())
        }

        fn matching_receipt_source(&self) -> Result<&ValidatedSourceTerminalReceiptV3> {
            let mut matching = self.source_receipts.iter().filter(|validated| {
                let receipt = &validated.receipt;
                receipt.source_path == self.options.source
                    && receipt.source_lmdb_identity == self.source_identity
                    && receipt.source_external_path == self.options.source_external_dir
                    && receipt.source_external_identity == self.source_external_identity
            });
            let source = matching.next().context(
                "full-final current source is not owned by a validated source-terminal receipt",
            )?;
            if matching.next().is_some() {
                bail!("full-final current source matches more than one source-terminal receipt");
            }
            Ok(source)
        }

        fn run_checkpoint_broker(
            &self,
            process: &ProcessIdentity,
            request: &PoolMigrationLaunchRequestV3,
            launch_request_sha256: &str,
            launch_ack_sha256: &str,
            mount_lifecycle: Option<&PreparedMountLifecycleV3>,
            mounted_lifecycle_authority: Option<&FileAuthorityV3>,
            online_audit: Option<&PoolMigrationAuditStore>,
        ) -> Result<CheckpointBrokerCompletion> {
            let mut sequence = 0u64;
            let mut previous_ack_sha256 = None;
            let mut progress = CheckpointProgress::default();
            let mut prepared_teardown = None;
            let mut prepared_terminal_authority = None;
            let mut prepared_terminal_publication = None;
            let mut prepared_source_retention = None;
            let mut last_lifecycle_check = Instant::now()
                .checked_sub(Duration::from_secs(1))
                .unwrap_or_else(Instant::now);
            loop {
                validate_checkpoint_frontier(&self.attempt_dir, sequence)?;
                let checkpoint_path = self.attempt_dir.join(request_file_name(sequence));
                if let Some((checkpoint, checkpoint_bytes)) =
                    read_checkpoint_request(&checkpoint_path, process.uid, process.gid)?
                {
                    self.revalidate_static_inputs_after_launch_without_systemd()?;
                    let mut next_progress = progress.clone();
                    validate_checkpoint_request(
                        &checkpoint,
                        sequence,
                        previous_ack_sha256.as_deref(),
                        process,
                        request,
                        launch_request_sha256,
                        self.options.phase,
                        self.options.batch_size,
                        self.controller_state.source_terminal_receipt_sha256.len() as u64,
                        &mut next_progress,
                    )?;
                    self.validate_checkpoint_systemd_fence(process)?;
                    let online_target_fenced = self.options.phase
                        == PoolMigrationControllerPhase::OnlineBounded
                        && self.controller_state.target_writers_fenced;
                    if online_target_fenced {
                        let deep_external_census = matches!(
                            checkpoint.operation.as_str(),
                            "online-source-audit-batch"
                                | "online-target-audit-batch"
                                | "online-evidence-publication"
                                | "online-audit-publication"
                                | "online-readiness"
                        );
                        self.revalidate_process_census(process, deep_external_census)
                            .context("pre-checkpoint target-fenced online handle census")?;
                    }
                    if checkpoint.operation.starts_with("online-")
                        && matches!(
                            checkpoint.operation.as_str(),
                            "online-source-audit-batch"
                                | "online-target-audit-batch"
                                | "online-target-audit-reset"
                        )
                    {
                        self.apply_online_audit_checkpoint(
                            &checkpoint,
                            online_audit
                                .context("online audit checkpoint has no root-owned audit store")?,
                        )?;
                    }
                    if self.options.phase.is_final_stopped() {
                        let mount_authority =
                            request.source.read_only_mounts.as_ref().context(
                                "stopped checkpoint launch has no source mount authority",
                            )?;
                        let deep_external_census = matches!(
                            checkpoint.operation.as_str(),
                            "target-terminal-audit"
                                | "terminal-receipt-publication"
                                | "terminal-readiness"
                        );
                        self.revalidate_checkpoint_storage_fence(
                            process,
                            mount_authority,
                            deep_external_census,
                        )?;
                    } else if online_target_fenced {
                        let deep_external_census = matches!(
                            checkpoint.operation.as_str(),
                            "online-target-audit-batch"
                                | "online-evidence-publication"
                                | "online-audit-publication"
                                | "online-readiness"
                        );
                        self.revalidate_process_census(process, deep_external_census)
                            .context("revalidate target-fenced online handle census")?;
                    }
                    let authorized_at = boottime_millis()?;
                    if authorized_at > checkpoint.start_before_boottime_millis {
                        bail!("checkpoint sequence {sequence} expired before root authorization");
                    }
                    let checkpoint_sha256 = sha256_bytes(&checkpoint_bytes);
                    if checkpoint.operation == "terminal-readiness" {
                        let terminal_authority = self.capture_phase_terminal_authority(process)?;
                        prepared_terminal_publication = Some(create_terminal_publication_intent(
                            &self.attempt_dir,
                            &self.boot_id,
                            &self.options.rollout_id,
                            &self.nonce,
                            launch_request_sha256,
                            mount_lifecycle
                                .context("stopped terminal readiness has no mount lifecycle")?,
                            self.cursor.clone(),
                            self.options.service_gid,
                            terminal_authority.clone(),
                        )?);
                        match self.options.phase {
                            PoolMigrationControllerPhase::FinalStoppedSource => {
                                prepared_source_retention = Some(record_source_mounts_retained(
                                    &self.attempt_dir,
                                    mount_lifecycle.context(
                                        "source terminal readiness has no mount lifecycle",
                                    )?,
                                    mounted_lifecycle_authority.context(
                                        "source terminal readiness has no mounted receipt",
                                    )?,
                                    terminal_authority.clone(),
                                )?);
                            }
                            PoolMigrationControllerPhase::FinalStoppedFull => {
                                prepared_teardown =
                                    Some(self.prepare_completed_source_mount_teardown_intent(
                                        request,
                                        launch_request_sha256,
                                        &terminal_authority,
                                    )?);
                            }
                            PoolMigrationControllerPhase::OnlineBounded => {
                                bail!("online-bounded checkpoint cannot become terminal-ready")
                            }
                        }
                        prepared_terminal_authority = Some(terminal_authority);
                    }
                    let acknowledgement = MigrationCheckpointAckV3 {
                        schema: CHECKPOINT_ACK_SCHEMA.to_string(),
                        status: "authorized".to_string(),
                        sequence,
                        previous_ack_sha256: previous_ack_sha256.clone(),
                        request_sha256: checkpoint_sha256,
                        operation: checkpoint.operation.clone(),
                        cursor: checkpoint.cursor.clone(),
                        range_limit: checkpoint.range_limit,
                        worker_pid: checkpoint.worker_pid,
                        worker_proc_start_time_ticks: checkpoint.worker_proc_start_time_ticks,
                        broker_pid: checkpoint.broker_pid,
                        broker_proc_start_time_ticks: checkpoint.broker_proc_start_time_ticks,
                        boot_id: checkpoint.boot_id.clone(),
                        attempt_nonce: checkpoint.attempt_nonce.clone(),
                        launch_request_sha256: checkpoint.launch_request_sha256.clone(),
                        authorized_at_boottime_millis: authorized_at,
                        start_before_boottime_millis: checkpoint.start_before_boottime_millis,
                    };
                    let mut acknowledgement_bytes = serde_json::to_vec(&acknowledgement)
                        .context("serialize root checkpoint acknowledgement")?;
                    acknowledgement_bytes.push(b'\n');
                    let acknowledgement_path = self.attempt_dir.join(ack_file_name(sequence));
                    durable_create_atomic(
                        &acknowledgement_path,
                        &acknowledgement_bytes,
                        0o440,
                        0,
                        self.options.service_gid,
                        &self.nonce,
                    )?;
                    progress = next_progress;
                    previous_ack_sha256 = Some(sha256_bytes(&acknowledgement_bytes));
                    sequence = sequence
                        .checked_add(1)
                        .context("checkpoint sequence overflow")?;
                    continue;
                }

                if last_lifecycle_check.elapsed() < Duration::from_secs(1) {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
                last_lifecycle_check = Instant::now();
                self.revalidate_static_inputs_after_launch_without_systemd()?;
                match validate_process_identity(
                    process.main_pid,
                    &process.invocation_id,
                    &self.options.systemd_unit,
                    &self.migration_binary.path,
                    &self.expected_argv,
                    process.gid,
                ) {
                    Ok(current) => {
                        if &current != process {
                            bail!(
                                "Pool migration worker identity changed during lifecycle polling"
                            );
                        }
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(error) if io_error_kind(&error) == Some(ErrorKind::NotFound) => {}
                    Err(error) => {
                        return Err(error)
                            .context("revalidate Pool migration worker during lifecycle polling")
                    }
                }
                let properties = self.query_batched_systemd_fence()?;
                match property(&properties, "ActiveState")? {
                    "inactive" => {
                        if property(&properties, "SubState")? != "dead"
                            || property(&properties, "Result")? != "success"
                            || property(&properties, "NRestarts")? != "0"
                        {
                            bail!(
                                "Pool migration unit stopped without a clean success result ({}/{})",
                                property(&properties, "SubState")?,
                                property(&properties, "Result")?
                            );
                        }
                        self.revalidate_static_inputs_after_launch_without_systemd()?;
                        if self.options.phase.is_final_stopped() {
                            let mounts =
                                request.source.read_only_mounts.as_ref().context(
                                    "stopped terminal launch has no source mount authority",
                                )?;
                            validate_source_read_only_mount_authority(
                                mounts,
                                &self.options.source,
                                self.source_identity,
                                self.options.source_external_dir.as_deref(),
                                self.source_external_identity,
                            )
                            .context("terminal source mount revalidation")?;
                            for source in &self.source_receipts {
                                let receipt = &source.receipt;
                                validate_source_read_only_mount_authority(
                                    &receipt.source_read_only_mounts,
                                    &receipt.source_path,
                                    receipt.source_lmdb_identity,
                                    receipt.source_external_path.as_deref(),
                                    receipt.source_external_identity,
                                )
                                .with_context(|| {
                                    format!(
                                        "terminal receipt-source mount revalidation {}",
                                        receipt.source_path.display()
                                    )
                                })?;
                            }
                        }
                        validate_checkpoint_namespace(&self.attempt_dir, sequence)?;
                        let terminal_receipt_authority =
                            self.validate_terminal_worker_outputs(process, &progress, sequence)?;
                        if terminal_receipt_authority.as_ref()
                            != prepared_terminal_authority.as_ref()
                            && self.options.phase.is_final_stopped()
                        {
                            bail!(
                                "worker terminal receipt changed after root authorized terminal readiness"
                            );
                        }
                        if self.options.phase == PoolMigrationControllerPhase::OnlineBounded {
                            let certification = terminal_receipt_authority
                                .as_ref()
                                .map(|authority| {
                                    self.certify_online_target_audit(
                                        process,
                                        request,
                                        launch_request_sha256,
                                        launch_ack_sha256,
                                        authority,
                                        online_audit.context(
                                            "online certification has no root-owned audit store",
                                        )?,
                                    )
                                })
                                .transpose()?;
                            return Ok(CheckpointBrokerCompletion {
                                checkpoint_count: sequence,
                                checkpoint_systemctl_subprocess_count: sequence,
                                terminal_receipt_sha256: terminal_receipt_authority
                                    .map(|authority| authority.sha256),
                                mount_teardown_receipt: None,
                                terminal_publication_receipt: None,
                                source_terminal_certification: None,
                                online_target_audit_certification: certification,
                            });
                        }
                        let terminal_publication_intent = prepared_terminal_publication
                            .as_ref()
                            .context("stopped completion has no terminal publication intent")?;
                        authorize_terminal_recovery(
                            &self.attempt_dir,
                            terminal_publication_intent,
                            &self.options,
                            &self.boot_id,
                            &self.controller_state,
                            &self.controller_state_input.sha256,
                            &self.source_baseline_input.sha256,
                            &self.pool_topology,
                            &self.pool_topology_input.sha256,
                            self.source_identity,
                            self.source_external_identity,
                            self.pool_identity,
                            &self.systemctl.path,
                        )
                        .context("root terminal audit replay before live lifecycle completion")?;
                        let mut lifecycle_completion = None;
                        if self.options.phase == PoolMigrationControllerPhase::FinalStoppedSource {
                            lifecycle_completion =
                                Some(prepared_source_retention.clone().context(
                                    "source-final completion has no prepublished mount retention",
                                )?);
                        }
                        let mount_teardown_receipt = if self.options.phase
                            == PoolMigrationControllerPhase::FinalStoppedFull
                        {
                            Some(self.teardown_completed_source_mounts(
                                request,
                                terminal_receipt_authority.as_ref().context(
                                    "full-final completion has no terminal audit authority",
                                )?,
                                prepared_teardown.as_ref().context(
                                    "full-final completion has no prepublished teardown intent",
                                )?,
                            )?)
                        } else {
                            None
                        };
                        if self.options.phase == PoolMigrationControllerPhase::FinalStoppedFull {
                            lifecycle_completion = Some(record_full_mount_lifecycle_closed(
                                &self.attempt_dir,
                                mount_lifecycle
                                    .context("full-final completion has no mount lifecycle")?,
                                mount_teardown_receipt.clone().context(
                                    "full-final completion has no mount teardown receipt",
                                )?,
                            )?);
                        }
                        let terminal_publication = if self.options.phase.is_final_stopped() {
                            Some(complete_terminal_publication(
                                &self.attempt_dir,
                                terminal_publication_intent,
                                lifecycle_completion.context(
                                    "stopped completion has no lifecycle completion authority",
                                )?,
                                &self.boot_id,
                            )?)
                        } else {
                            None
                        };
                        return Ok(CheckpointBrokerCompletion {
                            checkpoint_count: sequence,
                            checkpoint_systemctl_subprocess_count: sequence,
                            terminal_receipt_sha256: terminal_receipt_authority
                                .map(|authority| authority.sha256),
                            mount_teardown_receipt,
                            terminal_publication_receipt: terminal_publication
                                .as_ref()
                                .map(|completion| completion.receipt.clone()),
                            source_terminal_certification: terminal_publication
                                .and_then(|completion| completion.source_certification),
                            online_target_audit_certification: None,
                        });
                    }
                    "failed" => {
                        bail!(
                            "Pool migration unit failed while checkpoint broker was active ({}/{})",
                            property(&properties, "SubState")?,
                            property(&properties, "Result")?
                        )
                    }
                    _ => {
                        validate_running_worker_properties(&properties, process)?;
                    }
                }
                thread::sleep(Duration::from_millis(10));
            }
        }

        fn revalidate_static_inputs_after_launch(&self) -> Result<()> {
            self.revalidate_static_inputs()?;
            self.revalidate_created_launch_authorities()
        }

        fn revalidate_static_inputs_after_launch_without_systemd(&self) -> Result<()> {
            self.revalidate_static_inputs_without_systemd()?;
            self.revalidate_created_launch_authorities()
        }

        fn revalidate_created_launch_authorities(&self) -> Result<()> {
            validate_created_authority(
                &self.state_output,
                &self.controller_state_input.sha256,
                "controller-state CAS",
            )?;
            validate_created_authority(
                &self.baseline_output,
                &self.source_baseline_input.sha256,
                "source-baseline CAS",
            )?;
            validate_created_authority(
                &self.topology_output,
                &self.pool_topology_input.sha256,
                "Pool-topology CAS",
            )?;
            validate_created_authority(
                &self.options.systemd_environment_file,
                &sha256_bytes(&self.environment_bytes),
                "systemd environment file",
            )
        }

        fn validate_checkpoint_systemd_fence(&self, process: &ProcessIdentity) -> Result<()> {
            let worker = self.query_batched_systemd_fence()?;
            validate_running_worker_with_properties(
                &worker,
                &self.options.systemd_unit,
                &self.systemd_fragment,
                &self.options.systemd_environment_file,
                &self.migration_binary.path,
                &self.options.controller_systemd_unit,
                &self.expected_argv,
                process,
            )
        }

        fn query_batched_systemd_fence(&self) -> Result<HashMap<String, String>> {
            let runtime_writer_masks_required = checkpoint_requires_runtime_writer_masks(
                self.options.phase,
                self.controller_state.target_writers_fenced,
            );
            if runtime_writer_masks_required {
                validate_runtime_writer_mask_authorities(
                    &self.controller_state.stopped_writer_units,
                    &self.controller_state.writer_unit_masks,
                )?;
            }
            validate_legacy_worker_mask_authorities(
                &self.controller_state.legacy_worker_template_mask,
                &self.controller_state.legacy_worker_instance_masks,
            )?;
            let mut units = vec![
                self.options.controller_systemd_unit.clone(),
                self.options.systemd_unit.clone(),
            ];
            if runtime_writer_masks_required {
                units.extend(self.controller_state.stopped_writer_units.iter().cloned());
            }
            // An uninstantiated `name@.service` is not a valid `systemctl
            // show` target. Its exact root-owned runtime symlink is validated
            // directly; concrete instance masks also bind systemd's view.
            units.extend(
                self.controller_state
                    .legacy_worker_instance_masks
                    .iter()
                    .map(|mask| mask.unit.clone()),
            );
            let mut properties = query_systemd_property_sets(&self.systemctl.path, &units)?;
            let controller = properties
                .remove(&self.options.controller_systemd_unit)
                .context("batched checkpoint query omitted the root controller")?;
            validate_running_controller_properties(
                &controller,
                &self.controller_systemd_invocation_id,
                &self.controller_systemd_fragment,
                &self.controller_systemd_environment_file.path,
                &self.controller_executable.path,
                self.broker_pid,
            )?;
            let worker = properties
                .remove(&self.options.systemd_unit)
                .context("batched checkpoint query omitted the migration worker")?;
            validate_loaded_unit_common(
                &worker,
                &self.systemd_fragment,
                &self.options.systemd_environment_file,
                &self.migration_binary.path,
                "oneshot",
            )?;
            if !property(&worker, "BindsTo")?
                .split_ascii_whitespace()
                .any(|bound| bound == self.options.controller_systemd_unit)
            {
                bail!("Pool migration worker lost its exact root-controller BindsTo authority");
            }
            if runtime_writer_masks_required {
                for (unit, mask) in self
                    .controller_state
                    .stopped_writer_units
                    .iter()
                    .zip(&self.controller_state.writer_unit_masks)
                {
                    let unit_properties = properties.remove(unit).with_context(|| {
                        format!("batched checkpoint query omitted writer {unit}")
                    })?;
                    validate_runtime_masked_writer_owned_properties(unit, mask, &unit_properties)?;
                }
            }
            for mask in &self.controller_state.legacy_worker_instance_masks {
                let unit_properties = properties.remove(&mask.unit).with_context(|| {
                    format!(
                        "batched checkpoint query omitted legacy worker mask {}",
                        mask.unit
                    )
                })?;
                validate_runtime_masked_writer_owned_properties(
                    &mask.unit,
                    mask,
                    &unit_properties,
                )?;
            }
            if !properties.is_empty() {
                bail!("batched checkpoint systemd query retained an unvalidated unit");
            }
            validate_legacy_worker_mask_authorities(
                &self.controller_state.legacy_worker_template_mask,
                &self.controller_state.legacy_worker_instance_masks,
            )?;
            Ok(worker)
        }

        fn revalidate_checkpoint_fence(
            &self,
            process: &ProcessIdentity,
            source_mounts: &SourceReadOnlyMountAuthorityV3,
            deep_external_census: bool,
        ) -> Result<()> {
            if !self.options.phase.is_final_stopped() {
                return Ok(());
            }
            validate_source_read_only_mount_authority(
                source_mounts,
                &self.options.source,
                self.source_identity,
                self.options.source_external_dir.as_deref(),
                self.source_external_identity,
            )
            .context("revalidate source read-only mount authority")?;
            for source in &self.source_receipts {
                let receipt = &source.receipt;
                validate_source_read_only_mount_authority(
                    &receipt.source_read_only_mounts,
                    &receipt.source_path,
                    receipt.source_lmdb_identity,
                    receipt.source_external_path.as_deref(),
                    receipt.source_external_identity,
                )
                .with_context(|| {
                    format!(
                        "revalidate receipt source read-only mount {}",
                        receipt.source_path.display()
                    )
                })?;
            }
            self.revalidate_masks_and_census(process, deep_external_census)
        }

        fn revalidate_checkpoint_storage_fence(
            &self,
            process: &ProcessIdentity,
            source_mounts: &SourceReadOnlyMountAuthorityV3,
            deep_external_census: bool,
        ) -> Result<()> {
            if !self.options.phase.is_final_stopped() {
                return Ok(());
            }
            validate_source_read_only_mount_authority(
                source_mounts,
                &self.options.source,
                self.source_identity,
                self.options.source_external_dir.as_deref(),
                self.source_external_identity,
            )
            .context("revalidate source read-only mount authority")?;
            for source in &self.source_receipts {
                let receipt = &source.receipt;
                validate_source_read_only_mount_authority(
                    &receipt.source_read_only_mounts,
                    &receipt.source_path,
                    receipt.source_lmdb_identity,
                    receipt.source_external_path.as_deref(),
                    receipt.source_external_identity,
                )
                .with_context(|| {
                    format!(
                        "revalidate receipt source read-only mount {}",
                        receipt.source_path.display()
                    )
                })?;
            }
            self.revalidate_process_census(process, deep_external_census)
        }

        fn revalidate_masks_and_census(
            &self,
            process: &ProcessIdentity,
            deep_external_census: bool,
        ) -> Result<()> {
            validate_legacy_worker_activation_fence_with_systemctl(
                &self.systemctl.path,
                &self.controller_state.legacy_worker_template_mask,
                &self.controller_state.legacy_worker_instance_masks,
            )
            .context("revalidate legacy migration-worker activation fence")?;
            if !checkpoint_requires_runtime_writer_masks(
                self.options.phase,
                self.controller_state.target_writers_fenced,
            ) {
                return Ok(());
            }
            validate_runtime_masked_writer_units_with_systemctl(
                &self.systemctl.path,
                &self.controller_state.stopped_writer_units,
                &self.controller_state.writer_unit_masks,
            )
            .context("revalidate checkpoint writer-unit masks")?;
            self.revalidate_process_census(process, deep_external_census)
        }

        fn revalidate_process_census(
            &self,
            process: &ProcessIdentity,
            deep_external_census: bool,
        ) -> Result<()> {
            match self.options.phase {
                PoolMigrationControllerPhase::FinalStoppedFull => census_store_process_handles(
                    &self.controller_state,
                    &self.pool_topology,
                    &self.source_receipts,
                    self.options.source_external_dir.as_deref(),
                    self.source_external_identity,
                    process.main_pid,
                    process.start_time_ticks,
                    deep_external_census,
                )
                .context("checkpoint source/target handle census"),
                PoolMigrationControllerPhase::FinalStoppedSource => census_store_process_handles(
                    &self.controller_state,
                    &self.pool_topology,
                    &self.source_receipts,
                    self.options.source_external_dir.as_deref(),
                    self.source_external_identity,
                    process.main_pid,
                    process.start_time_ticks,
                    deep_external_census,
                )
                .context("checkpoint source/target handle census"),
                PoolMigrationControllerPhase::OnlineBounded
                    if self.controller_state.target_writers_fenced
                        && self.controller_state.source_writers_fenced =>
                {
                    census_store_process_handles(
                        &self.controller_state,
                        &self.pool_topology,
                        &[],
                        self.options.source_external_dir.as_deref(),
                        self.source_external_identity,
                        process.main_pid,
                        process.start_time_ticks,
                        deep_external_census,
                    )
                    .context("checkpoint source/target writer-handle census")
                }
                PoolMigrationControllerPhase::OnlineBounded
                    if self.controller_state.target_writers_fenced =>
                {
                    census_target_writer_handles(
                        &self.controller_state,
                        &self.pool_topology,
                        process.main_pid,
                        process.start_time_ticks,
                        deep_external_census,
                    )
                    .context("checkpoint target writer-handle census")
                }
                PoolMigrationControllerPhase::OnlineBounded => Ok(()),
            }
        }

        fn validate_terminal_worker_outputs(
            &self,
            process: &ProcessIdentity,
            progress: &CheckpointProgress,
            checkpoint_count: u64,
        ) -> Result<Option<BoundedFileAuthorityV3>> {
            match self.options.phase {
                PoolMigrationControllerPhase::OnlineBounded => {
                    if !progress.online_evidence_published
                        && !progress.online_audit_published
                        && !progress.online_ready
                    {
                        return Ok(None);
                    }
                    if !progress.online_evidence_published
                        || !progress.online_audit_published
                        || !progress.online_ready
                    {
                        bail!("online worker exited during its terminal audit checkpoint sequence");
                    }
                    self.capture_phase_terminal_authority(process).map(Some)
                }
                PoolMigrationControllerPhase::FinalStoppedSource => {
                    if checkpoint_count == 0
                        || !progress.source_keyset_audited
                        || !progress.source_generation_fingerprinted
                        || !progress.source_receipt_published
                        || !progress.terminal_ready
                    {
                        bail!("source-final worker exited before its complete checkpoint sequence");
                    }
                    let authority = self.capture_phase_terminal_authority(process)?;
                    Ok(Some(authority))
                }
                PoolMigrationControllerPhase::FinalStoppedFull => {
                    if checkpoint_count == 0
                        || progress.source_reconciliations
                            != self.controller_state.source_terminal_receipt_sha256.len() as u64
                        || !progress.target_terminal_audited
                        || !progress.terminal_receipt_published
                        || !progress.terminal_ready
                    {
                        bail!("full-final worker exited before its complete checkpoint sequence");
                    }
                    let authority = self.capture_phase_terminal_authority(process)?;
                    Ok(Some(authority))
                }
            }
        }

        fn capture_phase_terminal_authority(
            &self,
            process: &ProcessIdentity,
        ) -> Result<BoundedFileAuthorityV3> {
            match self.options.phase {
                PoolMigrationControllerPhase::FinalStoppedSource => validate_worker_terminal_file(
                    &self.attempt_dir.join("source-terminal.json"),
                    process.uid,
                    process.gid,
                    0o640,
                    "source-terminal receipt",
                ),
                PoolMigrationControllerPhase::FinalStoppedFull => validate_worker_terminal_file(
                    &self.attempt_dir.join("terminal-audit.json"),
                    process.uid,
                    process.gid,
                    0o600,
                    "terminal Pool audit receipt",
                ),
                PoolMigrationControllerPhase::OnlineBounded => validate_worker_terminal_file(
                    &self.attempt_dir.join(ONLINE_TARGET_AUDIT_FILE_NAME),
                    process.uid,
                    process.gid,
                    0o640,
                    "online target audit receipt",
                ),
            }
        }

        fn apply_online_audit_checkpoint(
            &self,
            checkpoint: &MigrationCheckpointRequestV3,
            audit: &PoolMigrationAuditStore,
        ) -> Result<()> {
            audit.validate_binding()?;
            match checkpoint.operation.as_str() {
                "online-target-audit-reset" => {
                    audit.reset_target_cursor()?;
                    return Ok(());
                }
                "online-source-audit-batch" | "online-target-audit-batch" => {}
                _ => bail!("unsupported root online audit checkpoint operation"),
            }
            let entries = checkpoint
                .audit_entries
                .iter()
                .map(|entry| {
                    let hash: [u8; 32] = hashtree_core::from_hex(&entry.hash)
                        .context("decode online audit checkpoint hash")?;
                    Ok((hash, entry.size))
                })
                .collect::<Result<Vec<_>>>()?;
            if checkpoint.operation == "online-source-audit-batch" {
                self.verify_online_source_entries(&entries)?;
            }
            self.verify_online_target_entries(&entries)?;
            if checkpoint.operation == "online-source-audit-batch" {
                audit.record_verified_source(&entries)?;
            } else {
                let cursor: [u8; 32] = hashtree_core::from_hex(
                    checkpoint
                        .audit_target_cursor
                        .as_deref()
                        .context("online target audit checkpoint has no cursor")?,
                )
                .context("decode online target audit cursor")?;
                audit.record_verified_target_page(&entries, cursor)?;
            }
            Ok(())
        }

        fn verify_online_source_entries(&self, entries: &[([u8; 32], u64)]) -> Result<()> {
            if entries.is_empty() {
                bail!("root source audit requires a nonempty exact entry set");
            }
            let reader = self.open_online_source_audit_reader()?;
            let byte_limit = self
                .options
                .max_buffer_mib
                .checked_mul(1024 * 1024)
                .context("root source audit buffer limit overflow")?;
            let hashes = entries.iter().map(|(hash, _)| *hash).collect::<Vec<_>>();
            let mut offset = 0usize;
            while offset < hashes.len() {
                let bodies = reader
                    .read_hashes_bounded(&hashes[offset..], byte_limit)
                    .context("root-read online source audit bodies")?;
                if bodies.is_empty() {
                    bail!("root source audit body reader made no progress");
                }
                for ((actual_hash, data), (expected_hash, expected_size)) in
                    bodies.iter().zip(&entries[offset..])
                {
                    if actual_hash != expected_hash
                        || data.len() as u64 != *expected_size
                        || hashtree_core::sha256(data) != *expected_hash
                    {
                        bail!("root source audit body differs from checkpoint hash/size authority");
                    }
                }
                offset = offset
                    .checked_add(bodies.len())
                    .context("root source audit offset overflow")?;
            }
            Ok(())
        }

        fn open_online_source_audit_reader(&self) -> Result<hashtree_lmdb::LmdbBlobReader> {
            let external = self.options.source_external_dir.as_ref().map(|path| {
                hashtree_lmdb::ExternalBlobOptions {
                    base_path: path.clone(),
                    min_bytes: 1,
                    sync: true,
                    pack_target_bytes: None,
                }
            });
            hashtree_lmdb::LmdbBlobReader::open_with_external_read_concurrency_and_pinned_identity(
                &self.options.source,
                external,
                self.options.source_read_concurrency,
                hashtree_lmdb::PinnedLmdbIdentity {
                    data: hashtree_lmdb::PinnedLmdbFileIdentity {
                        device: self.source_identity.data.device,
                        inode: self.source_identity.data.inode,
                    },
                    lock: hashtree_lmdb::PinnedLmdbFileIdentity {
                        device: self.source_identity.lock.device,
                        inode: self.source_identity.lock.inode,
                    },
                },
            )
            .context("root-open exact online source audit reader")
        }

        fn verify_online_source_coverage(&self, audit: &PoolMigrationAuditStore) -> Result<()> {
            let reader = self.open_online_source_audit_reader()?;
            if let Some((hash, size)) =
                audit.first_unverified_source(&reader, self.options.batch_size)?
            {
                bail!(
                    "root source coverage found unverified entry {} / {} bytes",
                    hashtree_core::to_hex(&hash),
                    size
                );
            }
            Ok(())
        }

        fn verify_online_target_entries(&self, entries: &[([u8; 32], u64)]) -> Result<()> {
            if entries.is_empty() {
                return Ok(());
            }
            let catalog =
                PinnedDirectory::open_exact(&self.options.pool, "online audit target Pool")?;
            catalog.require_authority_identity(
                self.pool_identity.directory,
                "online audit target Pool",
            )?;
            let mut retained_members = Vec::with_capacity(self.pool_topology.members.len());
            for member in &self.pool_topology.members {
                let directory = PinnedDirectory::open_exact(
                    &member.path,
                    &format!("online audit Pool member {} directory", member.id),
                )?;
                directory.require_authority_identity(
                    member.directory_identity,
                    &format!("online audit Pool member {} directory", member.id),
                )?;
                let external = match (
                    member.external_path.as_deref(),
                    member.external_directory_identity,
                ) {
                    (Some(path), Some(identity)) => {
                        let external = PinnedDirectory::open_exact(
                            path,
                            &format!("online audit Pool member {} external", member.id),
                        )?;
                        external.require_authority_identity(
                            identity,
                            &format!("online audit Pool member {} external", member.id),
                        )?;
                        Some(external)
                    }
                    (None, None) => None,
                    _ => bail!(
                        "online audit Pool member {} external authority is incomplete",
                        member.id
                    ),
                };
                retained_members.push((member, directory, external));
            }
            let mut config = hashtree_lmdb::PoolStoreConfig::default();
            config.temperature.enabled = false;
            config.catalog_lmdb_identity = Some(hashtree_lmdb::PinnedLmdbIdentity {
                data: hashtree_lmdb::PinnedLmdbFileIdentity {
                    device: self.pool_identity.data.device,
                    inode: self.pool_identity.data.inode,
                },
                lock: hashtree_lmdb::PinnedLmdbFileIdentity {
                    device: self.pool_identity.lock.device,
                    inode: self.pool_identity.lock.inode,
                },
            });
            config.expected_manifest_sha256 = Some(
                hashtree_core::from_hex(&self.pool_topology.manifest_sha256)
                    .context("decode online audit Pool manifest")?,
            );
            config.member_runtime_paths = retained_members
                .iter()
                .map(|(member, directory, external)| {
                    Ok(hashtree_lmdb::PoolMemberRuntimePaths {
                        id: member.id.parse()?,
                        configured_path: member.path.clone(),
                        runtime_path: directory.runtime_path(),
                        configured_external_path: member.external_path.clone(),
                        runtime_external_path: external.as_ref().map(PinnedDirectory::runtime_path),
                        lmdb_identity: hashtree_lmdb::PinnedLmdbIdentity {
                            data: hashtree_lmdb::PinnedLmdbFileIdentity {
                                device: member.lmdb_identity.data.device,
                                inode: member.lmdb_identity.data.inode,
                            },
                            lock: hashtree_lmdb::PinnedLmdbFileIdentity {
                                device: member.lmdb_identity.lock.device,
                                inode: member.lmdb_identity.lock.inode,
                            },
                        },
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let reader = hashtree_lmdb::PoolStoreReader::open(catalog.runtime_path(), config)
                .context("root-open exact online target audit reader")?;
            let hashes = entries.iter().map(|(hash, _)| *hash).collect::<Vec<_>>();
            let locations = reader
                .blob_catalog_locations(&hashes)
                .context("root-read online target audit catalog")?;
            for ((hash, expected_size), location) in entries.iter().zip(&locations) {
                if !matches!(
                    location,
                    hashtree_lmdb::PoolCatalogLocation::Stored { size, .. }
                        if size == expected_size
                ) {
                    bail!(
                        "root target audit requires exact Stored location for {} / {} bytes",
                        hashtree_core::to_hex(hash),
                        expected_size
                    );
                }
            }
            let byte_limit = self
                .options
                .max_buffer_mib
                .checked_mul(1024 * 1024)
                .context("root target audit buffer limit overflow")?;
            let mut offset = 0usize;
            while offset < hashes.len() {
                let bodies = reader
                    .read_hashes_bounded(&hashes[offset..], byte_limit)
                    .context("root-read online target audit bodies")?;
                if bodies.is_empty() {
                    bail!("root target audit body reader made no progress");
                }
                for (body, (expected_hash, expected_size)) in bodies.iter().zip(&entries[offset..])
                {
                    if body.hash != *expected_hash
                        || body.declared_size != Some(*expected_size)
                        || body.data.as_ref().map(|data| data.len() as u64) != Some(*expected_size)
                        || body.error.is_some()
                    {
                        bail!("root target audit body differs from checkpoint hash/size authority");
                    }
                }
                offset = offset
                    .checked_add(bodies.len())
                    .context("root target audit offset overflow")?;
            }
            let final_locations = reader
                .blob_catalog_locations(&hashes)
                .context("root-revalidate online target audit catalog")?;
            if final_locations != locations {
                bail!("online target catalog changed during root content verification");
            }
            Ok(())
        }

        fn certify_online_target_audit(
            &self,
            process: &ProcessIdentity,
            request: &PoolMigrationLaunchRequestV3,
            launch_request_sha256: &str,
            launch_ack_sha256: &str,
            authority: &BoundedFileAuthorityV3,
            audit: &PoolMigrationAuditStore,
        ) -> Result<FileAuthorityV3> {
            let expected_path = self.attempt_dir.join(ONLINE_TARGET_AUDIT_FILE_NAME);
            if authority.path != expected_path {
                bail!("online target audit receipt has an unexpected attempt path");
            }
            let bytes = read_bounded_file_authority(authority, "online target audit receipt")?;
            let receipt: PoolMigrationOnlineTargetAuditReceiptV3 =
                serde_json::from_slice(&bytes)
                    .context("parse online target audit receipt for root certification")?;
            let terminal_cursor =
                capture_cursor_authority(&self.options.state_file, self.options.phase)?;
            let pool_manifest_sha256 =
                hashtree_core::from_hex(&self.controller_state.pool_manifest_sha256)
                    .context("decode online audit Pool manifest authority")?;
            let audit_binding = compute_online_audit_binding(
                &request.controller.rollout_id,
                &request.binary.sha256,
                &request.source.baseline.sha256,
                request.source.lmdb_identity,
                request.source.external_identity,
                request.pool.lmdb_identity,
                &request.pool.topology.sha256,
                pool_manifest_sha256,
            )?;
            let target_fence_binding = compute_online_target_fence_binding(
                &request.controller.rollout_id,
                &self.controller_state.stopped_writer_units,
                &self.controller_state.writer_unit_masks,
                &self.controller_state.legacy_worker_template_mask,
                &self.controller_state.legacy_worker_instance_masks,
            )?;
            if receipt.schema != ONLINE_TARGET_AUDIT_SCHEMA
                || receipt.status != "verified"
                || receipt.phase != "online-bounded"
                || receipt.rollout_id != self.options.rollout_id
                || receipt.boot_id != request.boot_id
                || receipt.attempt_namespace != request.attempt_namespace
                || receipt.attempt_namespace_identity != request.attempt_namespace_identity
                || receipt.attempt_identity != request.attempt_identity
                || receipt.attempt_nonce != self.nonce
                || receipt.request_path != self.request_path
                || receipt.request_sha256 != launch_request_sha256
                || receipt.acknowledgement_path != self.ack_path
                || receipt.acknowledgement_sha256 != launch_ack_sha256
                || receipt.terminal_cursor != terminal_cursor
                || receipt.worker_binary != request.binary
                || receipt.worker_argv_sha256 != argv_sha256(&request.argv)
                || receipt.systemd_unit != request.systemd_unit
                || receipt.systemd_invocation_id != process.invocation_id
                || receipt.systemd_fragment != request.systemd_fragment
                || receipt.systemd_environment_file != request.systemd_environment_file
                || receipt.main_pid != process.main_pid
                || receipt.proc_start_time_ticks != process.start_time_ticks
                || receipt.controller_state_sha256 != request.controller.state.sha256
                || receipt.source_path != request.source.lmdb_path
                || receipt.source_lmdb_identity != request.source.lmdb_identity
                || receipt.source_external_path != request.source.external_path
                || receipt.source_external_identity != request.source.external_identity
                || receipt.source_baseline_sha256 != request.source.baseline.sha256
                || receipt.pool_path != request.pool.path
                || receipt.pool_lmdb_identity != request.pool.lmdb_identity
                || receipt.pool_topology_sha256 != request.pool.topology.sha256
                || receipt.pool_manifest_sha256 != self.controller_state.pool_manifest_sha256
                || receipt.audit_store_path != online_audit_path(&request.cursor.path)?
                || receipt.audit_binding_sha256 != hashtree_core::to_hex(&audit_binding)
                || receipt.source_evidence.path != self.attempt_dir.join(SOURCE_EVIDENCE_FILE_NAME)
                || receipt.target_evidence.path
                    != self.attempt_dir.join(ONLINE_TARGET_EVIDENCE_FILE_NAME)
                || receipt.source_evidence.entries != receipt.source_verified_entries
                || receipt.target_evidence.entries != receipt.target_verified_entries
                || receipt.target_fence_binding_sha256
                    != hashtree_core::to_hex(&target_fence_binding)
                || receipt.target_writer_units != self.controller_state.stopped_writer_units
                || receipt.target_writer_unit_masks != self.controller_state.writer_unit_masks
                || receipt.legacy_worker_template_mask
                    != self.controller_state.legacy_worker_template_mask
                || receipt.legacy_worker_instance_masks
                    != self.controller_state.legacy_worker_instance_masks
            {
                bail!("online target audit receipt differs from the exact live launch authority");
            }
            validate_source_evidence_metadata(
                &receipt.source_evidence,
                Some(self.options.service_gid),
                false,
            )?;
            validate_source_evidence_metadata(
                &receipt.target_evidence,
                Some(self.options.service_gid),
                false,
            )?;
            let mut source_evidence =
                SourceEvidenceManifestReaderV3::open(&receipt.source_evidence)?;
            while source_evidence.next_entry()?.is_some() {}
            let source_summary = source_evidence.validated_summary()?;
            let root_source_summary =
                audit.for_each_source_verified_batch(self.options.batch_size, |_| Ok(()))?;
            if source_summary.entries != receipt.source_verified_entries
                || source_summary.bytes != receipt.source_verified_bytes
                || hashtree_core::to_hex(&source_summary.content_sha256)
                    != receipt.source_content_sha256
                || root_source_summary.entries != source_summary.entries
                || root_source_summary.bytes != source_summary.bytes
                || root_source_summary.content_sha256 != source_summary.content_sha256
            {
                bail!("online source evidence differs from its receipt or root-owned ledger");
            }
            self.verify_online_source_coverage(audit)?;
            let mut target_evidence =
                SourceEvidenceManifestReaderV3::open(&receipt.target_evidence)?;
            while target_evidence.next_entry()?.is_some() {}
            let target_summary = target_evidence.validated_summary()?;
            let root_target_summary =
                audit.for_each_target_verified_batch(self.options.batch_size, |_| Ok(()))?;
            if target_summary.entries != receipt.target_verified_entries
                || target_summary.bytes != receipt.target_verified_bytes
                || hashtree_core::to_hex(&target_summary.content_sha256)
                    != receipt.target_content_sha256
                || root_target_summary.entries != target_summary.entries
                || root_target_summary.bytes != target_summary.bytes
                || root_target_summary.content_sha256 != target_summary.content_sha256
            {
                bail!("online target evidence differs from its receipt or root-owned ledger");
            }
            if audit.target_fence_binding()? != Some(target_fence_binding) {
                bail!("root-owned target proof ledger is not bound to this exact writer fence");
            }
            if !self.controller_state.target_writers_fenced {
                bail!("online target audit certification requires the held target-writer fence");
            }
            if !self.controller_state.source_writers_fenced {
                bail!("online target audit certification requires the held source-writer fence");
            }
            validate_runtime_masked_writer_units_with_systemctl(
                &self.systemctl.path,
                &self.controller_state.stopped_writer_units,
                &self.controller_state.writer_unit_masks,
            )
            .context("revalidate target writer-unit masks before online certification")?;
            census_recovery_target_handles(&self.controller_state, &self.pool_topology)
                .context("revalidate target writer-handle census before online certification")?;
            census_recovery_source_handles(
                &self.controller_state,
                &self.options.source,
                self.options.source_external_dir.as_deref(),
                self.source_external_identity,
            )
            .context("revalidate source writer-handle census before online certification")?;
            let (_, target_content) = audit_recoverable_target_pool(
                &self.options.pool,
                self.pool_identity,
                &self.pool_topology,
                &self.pool_topology_input.sha256,
                std::slice::from_ref(&receipt.target_evidence),
                self.options.batch_size,
            )
            .context("root replay target catalog coverage before online certification")?;
            if target_content.evidence.as_slice() != std::slice::from_ref(&target_summary) {
                bail!("root target catalog replay used unexpected target evidence");
            }
            validate_runtime_masked_writer_units_with_systemctl(
                &self.systemctl.path,
                &self.controller_state.stopped_writer_units,
                &self.controller_state.writer_unit_masks,
            )
            .context("revalidate target writer-unit masks after online certification replay")?;
            census_recovery_target_handles(&self.controller_state, &self.pool_topology).context(
                "revalidate target writer-handle census after online certification replay",
            )?;
            census_recovery_source_handles(
                &self.controller_state,
                &self.options.source,
                self.options.source_external_dir.as_deref(),
                self.source_external_identity,
            )
            .context("revalidate source writer-handle census after online certification replay")?;
            let certification = PoolMigrationOnlineTargetAuditCertificationV3 {
                schema: ONLINE_TARGET_AUDIT_CERTIFICATION_SCHEMA.to_string(),
                status: "certified".to_string(),
                rollout_id: self.options.rollout_id.clone(),
                controller_state_sha256: self.controller_state_input.sha256.clone(),
                receipt: FileAuthorityV3 {
                    path: authority.path.clone(),
                    sha256: authority.sha256.clone(),
                },
                source_evidence: receipt.source_evidence,
                target_evidence: receipt.target_evidence,
                certified_at_unix_seconds: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .context("system clock precedes Unix epoch")?
                    .as_secs(),
            };
            let mut certification_bytes = serde_json::to_vec(&certification)
                .context("serialize online target audit certification")?;
            certification_bytes.push(b'\n');
            let path = self
                .attempt_dir
                .join(ONLINE_TARGET_AUDIT_CERTIFICATION_FILE_NAME);
            durable_create_atomic(
                &path,
                &certification_bytes,
                0o440,
                0,
                self.options.service_gid,
                &self.nonce,
            )?;
            Ok(FileAuthorityV3 {
                path,
                sha256: sha256_bytes(&certification_bytes),
            })
        }

        fn prepare_completed_source_mount_teardown_intent(
            &self,
            request: &PoolMigrationLaunchRequestV3,
            launch_request_sha256: &str,
            terminal_authority: &BoundedFileAuthorityV3,
        ) -> Result<PreparedMountTeardown> {
            let terminal_path = self.attempt_dir.join("terminal-audit.json");
            if terminal_authority.path != terminal_path {
                bail!("terminal Pool audit authority has an unexpected attempt path");
            }
            let authorities = self
                .source_receipts
                .iter()
                .map(|source| source.receipt.source_read_only_mounts.clone())
                .collect::<Vec<_>>();
            let request_mounts = request
                .source
                .read_only_mounts
                .as_ref()
                .context("full-final request has no receipt-owned source mount authority")?;
            if request_mounts
                != &self
                    .matching_receipt_source()?
                    .receipt
                    .source_read_only_mounts
            {
                bail!("full-final request source mount authority is not receipt-owned");
            }
            let mounts = plan_source_read_only_mount_teardown(&authorities)
                .context("build exact source mount teardown plan")?;
            let mount_namespace_identity = authorities
                .first()
                .context("full-final mount teardown has no source authorities")?
                .mount_namespace_identity;
            if authorities
                .iter()
                .any(|authority| authority.mount_namespace_identity != mount_namespace_identity)
            {
                bail!("full-final source mount authorities span multiple mount namespaces");
            }
            let intent = MountTeardownIntentV3 {
                schema: MOUNT_TEARDOWN_INTENT_SCHEMA.to_string(),
                status: "authorized".to_string(),
                boot_id: self.boot_id.clone(),
                rollout_id: self.options.rollout_id.clone(),
                attempt_nonce: self.nonce.clone(),
                launch_request_sha256: launch_request_sha256.to_string(),
                terminal_audit_path: terminal_path,
                terminal_audit_sha256: terminal_authority.sha256.clone(),
                terminal_audit_authority: terminal_authority.clone(),
                non_lazy: true,
                mount_namespace_identity,
                mounts,
            };
            validate_teardown_intent(&intent, &self.attempt_dir, &self.options.rollout_id)?;
            let intent_bytes = serialize_json_line(&intent, "exact mount-teardown intent")?;
            let intent_path = self.attempt_dir.join(MOUNT_TEARDOWN_INTENT_FILE);
            durable_create_atomic(&intent_path, &intent_bytes, 0o400, 0, 0, &self.nonce)?;
            let intent_sha256 = teardown_sha256_bytes(&intent_bytes);
            Ok(PreparedMountTeardown {
                intent,
                intent_path,
                intent_sha256,
            })
        }

        fn teardown_completed_source_mounts(
            &self,
            request: &PoolMigrationLaunchRequestV3,
            terminal_authority: &BoundedFileAuthorityV3,
            prepared: &PreparedMountTeardown,
        ) -> Result<FileAuthorityV3> {
            let request_mounts = request
                .source
                .read_only_mounts
                .as_ref()
                .context("full-final request has no receipt-owned source mount authority")?;
            if request_mounts
                != &self
                    .matching_receipt_source()?
                    .receipt
                    .source_read_only_mounts
            {
                bail!("full-final request source mount authority is not receipt-owned");
            }
            if terminal_authority != &prepared.intent.terminal_audit_authority {
                bail!("terminal Pool audit authority changed after teardown intent publication");
            }
            validate_teardown_intent(
                &prepared.intent,
                &self.attempt_dir,
                &self.options.rollout_id,
            )?;
            let intent = &prepared.intent;
            let intent_path = &prepared.intent_path;
            let intent_sha256 = &prepared.intent_sha256;
            let mut previous_step_sha256 = None;
            let mut steps = Vec::with_capacity(intent.mounts.len());
            for (index, mount) in intent.mounts.iter().enumerate() {
                teardown_one_source_read_only_mount(mount).with_context(|| {
                    format!("execute exact non-lazy mount teardown step {index}")
                })?;
                let step = MountTeardownStepReceiptV3 {
                    schema: MOUNT_TEARDOWN_STEP_SCHEMA.to_string(),
                    status: "removed".to_string(),
                    boot_id: intent.boot_id.clone(),
                    rollout_id: intent.rollout_id.clone(),
                    attempt_nonce: intent.attempt_nonce.clone(),
                    intent_path: intent_path.clone(),
                    intent_sha256: intent_sha256.clone(),
                    step_index: index as u64,
                    step_count: intent.mounts.len() as u64,
                    previous_step_sha256: previous_step_sha256.clone(),
                    non_lazy: true,
                    outcome: "unmounted".to_string(),
                    removed_mount: mount.clone(),
                };
                validate_teardown_step(
                    &step,
                    &intent,
                    intent_path,
                    intent_sha256,
                    index,
                    previous_step_sha256.as_deref(),
                )?;
                let step_bytes = serialize_json_line(&step, "exact mount-teardown step receipt")?;
                let step_path = self.attempt_dir.join(teardown_step_file_name(index));
                durable_create_atomic(&step_path, &step_bytes, 0o400, 0, 0, &self.nonce)?;
                let step_sha256 = teardown_sha256_bytes(&step_bytes);
                steps.push(MountTeardownStepAuthorityV3 {
                    step_index: index as u64,
                    path: step_path,
                    sha256: step_sha256.clone(),
                });
                previous_step_sha256 = Some(step_sha256);
            }
            let receipt = MountTeardownReceiptV3 {
                schema: MOUNT_TEARDOWN_RECEIPT_SCHEMA.to_string(),
                status: "verified".to_string(),
                boot_id: intent.boot_id.clone(),
                rollout_id: intent.rollout_id.clone(),
                attempt_nonce: intent.attempt_nonce.clone(),
                launch_request_sha256: intent.launch_request_sha256.clone(),
                terminal_audit_path: intent.terminal_audit_path.clone(),
                terminal_audit_sha256: intent.terminal_audit_sha256.clone(),
                terminal_audit_authority: intent.terminal_audit_authority.clone(),
                intent_path: intent_path.clone(),
                intent_sha256: intent_sha256.clone(),
                non_lazy: true,
                steps: steps.clone(),
                removed_mounts: intent.mounts.clone(),
            };
            validate_teardown_receipt(&receipt, intent, intent_path, intent_sha256, &steps)?;
            let receipt_bytes =
                serialize_json_line(&receipt, "exact terminal mount-teardown receipt")?;
            let receipt_path = self.attempt_dir.join(MOUNT_TEARDOWN_RECEIPT_FILE);
            durable_create_atomic(&receipt_path, &receipt_bytes, 0o400, 0, 0, &self.nonce)?;
            let validated = validate_completed_teardown_attempt(
                &self.attempt_dir,
                &self.options.rollout_id,
                &self.boot_id,
            )?;
            if validated != receipt {
                bail!("published mount-teardown receipt differs from its validated disk chain");
            }
            Ok(FileAuthorityV3 {
                path: receipt_path,
                sha256: teardown_sha256_bytes(&receipt_bytes),
            })
        }

        fn publish_terminal_cursor(
            &self,
            process: &ProcessIdentity,
            request: &PoolMigrationLaunchRequestV3,
            expected_terminal_authority: &BoundedFileAuthorityV3,
        ) -> Result<()> {
            let (receipt_name, receipt_mode, cursor_value) = match self.options.phase {
                PoolMigrationControllerPhase::FinalStoppedSource => {
                    ("source-terminal.json", 0o640, "source-complete")
                }
                PoolMigrationControllerPhase::FinalStoppedFull => {
                    ("terminal-audit.json", 0o600, "complete")
                }
                PoolMigrationControllerPhase::OnlineBounded => {
                    bail!("online-bounded migration cannot publish a terminal cursor")
                }
            };
            let current_terminal_authority = validate_worker_terminal_file(
                &self.attempt_dir.join(receipt_name),
                process.uid,
                process.gid,
                receipt_mode,
                if self.options.phase == PoolMigrationControllerPhase::FinalStoppedSource {
                    "source-terminal receipt"
                } else {
                    "terminal Pool audit receipt"
                },
            )?;
            if &current_terminal_authority != expected_terminal_authority {
                bail!("worker terminal receipt changed after root prepared terminal publication");
            }
            if request.cursor.exists {
                bail!("stopped terminal cursor authority was not initially absent");
            }
            durable_create_atomic(
                &request.cursor.path,
                format!("{cursor_value}\n").as_bytes(),
                0o440,
                0,
                self.options.service_gid,
                &self.nonce,
            )?;
            validate_terminal_cursor(&request.cursor.path, self.options.service_gid, cursor_value)
        }

        fn build_request(
            &self,
            process: &ProcessIdentity,
            source_read_only_mounts: Option<SourceReadOnlyMountAuthorityV3>,
        ) -> Result<PoolMigrationLaunchRequestV3> {
            let attempts_identity = file_identity(&self.attempts_dir, "v3 attempt namespace")?;
            let attempt_identity =
                file_identity(&self.attempt_dir, "Pool migration attempt directory")?;
            let additional_cas = self
                .additional_cas_inputs
                .iter()
                .map(|(label, file)| NamedFileAuthorityV3 {
                    label: label.clone(),
                    path: file.path.clone(),
                    sha256: file.sha256.clone(),
                })
                .collect();
            Ok(PoolMigrationLaunchRequestV3 {
                schema: REQUEST_SCHEMA.to_string(),
                attempt_namespace: self.attempts_dir.clone(),
                attempt_namespace_identity: attempts_identity,
                attempt_identity,
                nonce: self.nonce.clone(),
                boot_id: self.boot_id.clone(),
                execution_namespaces: host_execution_namespace_authority(&[])?,
                systemd_invocation_id: process.invocation_id.clone(),
                systemd_unit: self.options.systemd_unit.clone(),
                systemd_manager: "system".to_string(),
                systemd_fragment: self.systemd_fragment.authority(),
                systemd_environment_file: FileAuthorityV3 {
                    path: self.options.systemd_environment_file.clone(),
                    sha256: sha256_bytes(&self.environment_bytes),
                },
                main_pid: process.main_pid,
                proc_start_time_ticks: process.start_time_ticks,
                binary: self.migration_binary.authority(),
                argv: self.expected_argv.clone(),
                controller: ControllerAuthorityV3 {
                    rollout_id: self.options.rollout_id.clone(),
                    phase: self.options.phase.as_protocol_str().to_string(),
                    executable: self.controller_executable.authority(),
                    state: self
                        .controller_state_input
                        .authority_at(self.state_output.clone()),
                },
                checkpoint_broker: CheckpointBrokerAuthorityV3 {
                    pid: self.broker_pid,
                    proc_start_time_ticks: self.broker_proc_start_time_ticks,
                    timeout_seconds: self.options.launch_request_wait.as_secs(),
                    systemd_unit: self.options.controller_systemd_unit.clone(),
                    systemd_invocation_id: self.controller_systemd_invocation_id.clone(),
                    systemd_fragment_path: self.controller_systemd_fragment.path.clone(),
                    systemd_fragment_sha256: self.controller_systemd_fragment.sha256.clone(),
                    systemd_environment_file_path: self
                        .controller_systemd_environment_file
                        .path
                        .clone(),
                    systemd_environment_file_sha256: self
                        .controller_systemd_environment_file
                        .sha256
                        .clone(),
                },
                source: SourceAuthorityV3 {
                    lmdb_path: self.options.source.clone(),
                    lmdb_identity: self.source_identity,
                    external_path: self.options.source_external_dir.clone(),
                    external_identity: self.source_external_identity,
                    read_only_mounts: source_read_only_mounts,
                    baseline: self
                        .source_baseline_input
                        .authority_at(self.baseline_output.clone()),
                },
                pool: PoolAuthorityV3 {
                    path: self.options.pool.clone(),
                    lmdb_identity: self.pool_identity,
                    topology: self
                        .pool_topology_input
                        .authority_at(self.topology_output.clone()),
                },
                cursor: self.cursor.clone(),
                cas: additional_cas,
            })
        }

        fn revalidate_inputs(&self) -> Result<()> {
            self.revalidate_static_inputs()?;
            validate_cursor_matches(&self.cursor)
        }

        fn revalidate_static_inputs(&self) -> Result<()> {
            self.revalidate_static_inputs_without_systemd()?;
            validate_running_controller_service(
                &self.systemctl.path,
                &self.options.controller_systemd_unit,
                &self.controller_systemd_invocation_id,
                &self.controller_systemd_fragment,
                &self.controller_systemd_environment_file.path,
                &self.controller_executable.path,
                self.broker_pid,
            )?;
            if self.options.phase.is_final_stopped() {
                validate_runtime_masked_writer_units_with_systemctl(
                    &self.systemctl.path,
                    &self.controller_state.stopped_writer_units,
                    &self.controller_state.writer_unit_masks,
                )
                .context("revalidate enforceable final writer-unit masks")?;
            }
            validate_legacy_worker_activation_fence_with_systemctl(
                &self.systemctl.path,
                &self.controller_state.legacy_worker_template_mask,
                &self.controller_state.legacy_worker_instance_masks,
            )
            .context("revalidate legacy migration-worker activation fence")
        }

        fn revalidate_static_inputs_without_systemd(&self) -> Result<()> {
            for file in [
                &self.controller_executable,
                &self.controller_systemd_fragment,
                &self.controller_systemd_environment_file,
                &self.systemctl,
                &self.migration_binary,
                &self.systemd_fragment,
                &self.controller_state_input,
                &self.source_baseline_input,
                &self.pool_topology_input,
            ] {
                file.ensure_unchanged()?;
            }
            for (_, file) in &self.additional_cas_inputs {
                file.ensure_unchanged()?;
            }
            if current_boot_id()? != self.boot_id {
                bail!("system boot ID changed after controller preflight");
            }
            if process_start_time(self.broker_pid)? != self.broker_proc_start_time_ticks {
                bail!("checkpoint broker PID/starttime identity changed");
            }
            if lmdb_identity(&self.options.source, "source LMDB")? != self.source_identity {
                bail!("source LMDB identity changed after controller preflight");
            }
            if lmdb_identity(&self.options.pool, "target Pool catalog")? != self.pool_identity {
                bail!("target Pool catalog identity changed after controller preflight");
            }
            let external = self
                .options
                .source_external_dir
                .as_deref()
                .map(|path| file_identity(path, "source external directory"))
                .transpose()?;
            if external != self.source_external_identity {
                bail!("source external directory identity changed after controller preflight");
            }
            Ok(())
        }
    }

    struct StartedUnitGuard {
        systemctl: PathBuf,
        unit: String,
        armed: bool,
    }

    impl StartedUnitGuard {
        fn stop(&mut self) -> Result<()> {
            systemctl_success(
                &self.systemctl,
                &["stop", &self.unit],
                "stop failed Pool migration systemd unit",
            )?;
            self.armed = false;
            Ok(())
        }
    }

    impl Drop for StartedUnitGuard {
        fn drop(&mut self) {
            if !self.armed {
                return;
            }
            let _ = systemctl_output(&self.systemctl, &["stop", &self.unit]);
        }
    }

    fn validate_options(options: &PoolMigrationControllerOptions) -> Result<()> {
        require_safe_component("rollout ID", &options.rollout_id, 128)?;
        validate_pool_migration_release_phase(options.phase.as_protocol_str())?;
        require_systemd_service_name(&options.systemd_unit)?;
        require_controller_systemd_service_name(&options.controller_systemd_unit)?;
        if options.launch_request_wait.is_zero()
            || options.launch_request_wait > Duration::from_secs(300)
            || options.acknowledgement_wait.is_zero()
            || options.acknowledgement_wait > Duration::from_secs(300)
        {
            bail!("controller launch-request and acknowledgement waits must be 1..=300 seconds");
        }
        if options.batch_size == 0
            || options.max_buffer_mib == 0
            || options.source_read_concurrency == 0
            || options.reopen_batches == 0
            || options.max_items == Some(0)
            || options.service_gid == 0
        {
            bail!("Pool migration controller numeric limits must be positive");
        }
        validate_source_read_concurrency(options.source_read_concurrency)?;
        match (options.phase, options.max_items) {
            (PoolMigrationControllerPhase::OnlineBounded, None) => {
                bail!("online-bounded controller launch requires --max-items")
            }
            (
                PoolMigrationControllerPhase::FinalStoppedSource
                | PoolMigrationControllerPhase::FinalStoppedFull,
                Some(_),
            ) => {
                bail!("stopped final controller launch forbids --max-items")
            }
            _ => {}
        }
        let mut previous_writer: Option<&str> = None;
        for unit in &options.writer_units {
            require_writer_service_name(unit)?;
            if previous_writer.is_some_and(|previous| previous >= unit.as_str()) {
                bail!("--writer-unit values must be unique and strictly sorted");
            }
            previous_writer = Some(unit);
        }
        if options.phase.is_final_stopped() && options.writer_units.is_empty() {
            bail!("stopped final migration requires a nonempty complete --writer-unit set");
        }
        if options.phase.is_final_stopped() && options.reopen_batches > MAX_FINAL_REOPEN_BATCHES {
            bail!(
                "stopped final migration requires --reopen-batches <= {MAX_FINAL_REOPEN_BATCHES} for bounded fence revalidation"
            );
        }
        validate_stopped_final_batch_size(options.phase.is_final_stopped(), options.batch_size)?;
        if options.additional_cas.is_empty() {
            bail!("Pool migration controller requires at least one explicit --cas authority");
        }
        for (path, label) in [
            (&options.rollout_dir, "rollout directory"),
            (&options.controller_executable, "controller executable"),
            (
                &options.controller_systemd_fragment,
                "controller systemd fragment",
            ),
            (
                &options.controller_systemd_environment_file,
                "controller systemd environment file",
            ),
            (&options.controller_state_input, "controller-state input"),
            (&options.source_baseline_input, "source-baseline input"),
            (&options.pool_topology_input, "Pool-topology input"),
            (&options.systemctl, "systemctl executable"),
            (&options.systemd_fragment, "systemd fragment"),
            (
                &options.systemd_environment_file,
                "systemd environment file",
            ),
            (&options.migration_binary, "migration binary"),
            (&options.target_data_dir, "target data directory"),
            (&options.pool, "target Pool catalog"),
            (&options.source, "source LMDB"),
            (&options.state_file, "migration cursor"),
        ] {
            require_absolute(path, label)?;
        }
        if let Some(path) = &options.source_external_dir {
            require_absolute(path, "source external directory")?;
        }
        Ok(())
    }

    fn validate_durability_environment() -> Result<()> {
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
                bail!("{variable} must be absent from the Pool migration controller environment");
            }
        }
        Ok(())
    }

    fn require_root() -> Result<()> {
        if unsafe { libc::geteuid() } != 0 {
            bail!("Pool migration v3 controller must run as root");
        }
        Ok(())
    }

    fn require_service_access(
        snapshot: &FileSnapshot,
        service_gid: u32,
        execute: bool,
        label: &str,
    ) -> Result<()> {
        let group_bits = if execute { 0o050 } else { 0o040 };
        let other_bits = if execute { 0o005 } else { 0o004 };
        let accessible = (snapshot.gid == service_gid && snapshot.mode & group_bits == group_bits)
            || snapshot.mode & other_bits == other_bits;
        if !accessible {
            bail!(
                "{label} is not readable{} by the migration service identity",
                if execute { "/executable" } else { "" }
            );
        }
        Ok(())
    }

    fn require_directory_service_search(path: &Path, service_gid: u32, label: &str) -> Result<()> {
        let metadata =
            std::fs::symlink_metadata(path).with_context(|| format!("inspect {label} access"))?;
        let accessible = (metadata.gid() == service_gid && metadata.mode() & 0o050 == 0o050)
            || metadata.mode() & 0o005 == 0o005;
        if !accessible {
            bail!("{label} is not readable/searchable by the migration service identity");
        }
        Ok(())
    }

    fn fresh_nonce() -> String {
        let mut bytes = [0u8; 32];
        OsRng.fill_bytes(&mut bytes);
        hex::encode(bytes)
    }

    fn require_absolute(path: &Path, label: &str) -> Result<()> {
        if !path.is_absolute() {
            bail!("{label} path must be absolute: {}", path.display());
        }
        Ok(())
    }

    fn require_safe_component(label: &str, value: &str, maximum: usize) -> Result<()> {
        if value.is_empty()
            || value.len() > maximum
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            bail!("{label} is not a safe bounded path component");
        }
        Ok(())
    }

    fn require_lower_hex(label: &str, value: &str, length: usize) -> Result<()> {
        if value.len() != length
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            bail!("{label} must be exactly {length} lowercase hexadecimal characters");
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
                byte.is_ascii_alphanumeric()
                    || matches!(byte, b':' | b'_' | b'.' | b'@' | b'\\' | b'-')
            })
        {
            bail!(
                "systemd unit must be an exact bounded hashtree-pool-migration-worker@*.service name"
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
                byte.is_ascii_alphanumeric()
                    || matches!(byte, b'@' | b'.' | b'_' | b':' | b'\\' | b'-')
            })
        {
            bail!(
                "controller systemd unit must be an exact bounded hashtree-pool-migration-controller@*.service name"
            );
        }
        Ok(())
    }

    fn canonical_regular_path(path: &Path, label: &str) -> Result<PathBuf> {
        require_absolute(path, label)?;
        let metadata = std::fs::symlink_metadata(path)
            .with_context(|| format!("inspect {label} {}", path.display()))?;
        if !metadata.file_type().is_file() {
            bail!("{label} {} is not a regular file", path.display());
        }
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
        Ok(canonical)
    }

    fn canonical_directory_path(path: &Path, label: &str) -> Result<PathBuf> {
        require_absolute(path, label)?;
        let metadata = std::fs::symlink_metadata(path)
            .with_context(|| format!("inspect {label} {}", path.display()))?;
        if !metadata.file_type().is_dir() {
            bail!("{label} {} is not a directory", path.display());
        }
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
        Ok(canonical)
    }

    fn resolved_directory_path(path: &Path, label: &str) -> Result<PathBuf> {
        require_absolute(path, label)?;
        let metadata = std::fs::metadata(path)
            .with_context(|| format!("inspect {label} {}", path.display()))?;
        if !metadata.is_dir() {
            bail!("{label} {} does not resolve to a directory", path.display());
        }
        path.canonicalize()
            .with_context(|| format!("canonicalize {label} {}", path.display()))
    }

    fn canonical_root_directory(path: &Path, label: &str) -> Result<PathBuf> {
        let canonical = canonical_directory_path(path, label)?;
        let metadata = std::fs::symlink_metadata(&canonical)
            .with_context(|| format!("inspect {label} ownership"))?;
        if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            bail!("{label} must be root-owned and not group/world writable");
        }
        Ok(canonical)
    }

    fn canonical_root_parent_for_absent(path: &Path, label: &str) -> Result<PathBuf> {
        require_absolute(path, label)?;
        let parent = path
            .parent()
            .filter(|value| !value.as_os_str().is_empty())
            .with_context(|| format!("{label} has no parent directory"))?;
        let canonical = canonical_root_directory(parent, &format!("{label} parent"))?;
        let name = path
            .file_name()
            .with_context(|| format!("{label} has no file name"))?;
        if canonical.join(name) != path {
            bail!("{label} must be an exact canonical absent path");
        }
        Ok(canonical)
    }

    fn require_absent(path: &Path, label: &str) -> Result<()> {
        match std::fs::symlink_metadata(path) {
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| format!("inspect {label} {}", path.display())),
            Ok(_) => bail!("{label} already exists at {}", path.display()),
        }
    }

    fn validate_planned_attempt_namespace(path: &Path) -> Result<()> {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) => {
                if !metadata.file_type().is_dir()
                    || metadata.uid() != 0
                    || metadata.gid() != 0
                    || metadata.mode() & 0o7777 != 0o755
                {
                    bail!("existing attempts-v3 namespace must be exactly root:root mode 0755");
                }
                let canonical = path
                    .canonicalize()
                    .context("canonicalize existing attempts-v3 namespace")?;
                if canonical != path {
                    bail!("existing attempts-v3 namespace is not an exact canonical path");
                }
                Ok(())
            }
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("inspect planned attempts-v3 namespace"),
        }
    }

    fn file_identity(path: &Path, label: &str) -> Result<FileIdentityV3> {
        let metadata = std::fs::symlink_metadata(path)
            .with_context(|| format!("inspect {label} {}", path.display()))?;
        if metadata.file_type().is_symlink() {
            bail!("{label} {} must not be a symlink", path.display());
        }
        if metadata.dev() == 0 || metadata.ino() == 0 {
            bail!("{label} has an invalid zero device/inode identity");
        }
        Ok(FileIdentityV3 {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    fn lmdb_identity(path: &Path, label: &str) -> Result<LmdbIdentityV3> {
        let directory = canonical_directory_path(path, label)?;
        let data = directory.join("data.mdb");
        let lock = directory.join("lock.mdb");
        canonical_regular_path(&data, &format!("{label} data.mdb"))?;
        canonical_regular_path(&lock, &format!("{label} lock.mdb"))?;
        Ok(LmdbIdentityV3 {
            directory: file_identity(&directory, label)?,
            data: file_identity(&data, &format!("{label} data.mdb"))?,
            lock: file_identity(&lock, &format!("{label} lock.mdb"))?,
        })
    }

    fn sha256_bytes(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    fn current_boot_id() -> Result<String> {
        let value = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .context("read Linux boot ID")?
            .trim()
            .to_ascii_lowercase();
        let compact = value.replace('-', "");
        require_lower_hex("current boot ID", &compact, 32)?;
        Ok(value)
    }

    fn validate_topology_summary(topology: &PoolTopologyV3, pool: &Path) -> Result<()> {
        if topology.schema != POOL_TOPOLOGY_SCHEMA {
            bail!(
                "unsupported Pool topology schema {}; expected {POOL_TOPOLOGY_SCHEMA}",
                topology.schema
            );
        }
        if topology.pool_path != pool {
            bail!("Pool-topology input belongs to a different target Pool");
        }
        require_lower_hex(
            "Pool topology manifest SHA-256",
            &topology.manifest_sha256,
            64,
        )?;
        if topology.members.is_empty() {
            bail!("Pool-topology input must pin at least one member");
        }
        let mut previous: Option<&str> = None;
        for member in &topology.members {
            let canonical_id = uuid::Uuid::parse_str(&member.id)
                .context("parse Pool topology member ID")?
                .to_string();
            if canonical_id != member.id {
                bail!("Pool topology member ID must be a canonical lowercase UUID");
            }
            if previous.is_some_and(|value| value >= member.id.as_str()) {
                bail!("Pool topology members must be uniquely sorted by ID");
            }
            previous = Some(&member.id);
        }
        Ok(())
    }

    fn validate_topology_host_filesystems(topology: &PoolTopologyV3) -> Result<()> {
        let mut paths = Vec::with_capacity(topology.members.len().saturating_mul(2));
        for member in &topology.members {
            paths.push((member.path.as_path(), "target Pool member LMDB"));
            if let Some(external) = member.external_path.as_deref() {
                paths.push((external, "target Pool member external corpus"));
            }
        }
        require_host_execution_namespace(&paths)
    }

    #[allow(clippy::too_many_arguments)]
    fn validate_controller_state_input(
        state: &ControllerStateV3,
        options: &PoolMigrationControllerOptions,
        boot_id: &str,
        source_identity: LmdbIdentityV3,
        source_external_identity: Option<FileIdentityV3>,
        pool_identity: LmdbIdentityV3,
        topology: &PoolTopologyV3,
        topology_sha256: &str,
        systemctl: &Path,
    ) -> Result<()> {
        if state.schema != CONTROLLER_STATE_SCHEMA {
            bail!(
                "unsupported controller-state schema {}; expected {CONTROLLER_STATE_SCHEMA}",
                state.schema
            );
        }
        if state.rollout_id != options.rollout_id
            || state.phase != options.phase.as_protocol_str()
            || state.boot_id != boot_id
        {
            bail!("controller-state input does not bind the explicit rollout, phase, and boot");
        }
        if state.source_lmdb_identity != source_identity
            || state.source_external_identity != source_external_identity
            || state.pool_lmdb_identity != pool_identity
        {
            bail!("controller-state input does not bind the exact source and target identities");
        }
        if state.pool_manifest_sha256 != topology.manifest_sha256
            || state.pool_topology_sha256 != topology_sha256
        {
            bail!("controller-state input does not bind the exact Pool topology and manifest");
        }
        let mut previous: Option<&str> = None;
        for unit in &state.stopped_writer_units {
            require_writer_service_name(unit)?;
            if previous.is_some_and(|value| value >= unit.as_str()) {
                bail!("controller stopped-writer units must be uniquely sorted");
            }
            previous = Some(unit);
        }
        let mut previous_receipt: Option<&str> = None;
        if state.source_terminal_receipt_sha256.len() > MAX_FINAL_SOURCE_RECEIPTS {
            bail!(
                "controller source-terminal receipt set exceeds the hard maximum of {MAX_FINAL_SOURCE_RECEIPTS}"
            );
        }
        for sha256 in &state.source_terminal_receipt_sha256 {
            require_lower_hex("source-terminal receipt SHA-256", sha256, 64)?;
            if previous_receipt.is_some_and(|previous| previous >= sha256.as_str()) {
                bail!("controller source-terminal receipt SHA-256 set must be uniquely sorted");
            }
            previous_receipt = Some(sha256);
        }
        if options.phase != PoolMigrationControllerPhase::FinalStoppedFull
            && !state.source_terminal_receipt_sha256.is_empty()
        {
            bail!("only final-stopped-full may consume source-terminal receipts");
        }
        if options.phase == PoolMigrationControllerPhase::FinalStoppedFull
            && state.source_terminal_receipt_sha256.is_empty()
        {
            bail!("final-stopped-full requires a nonempty exact source-terminal receipt set");
        }
        if state.stopped_writer_units != options.writer_units {
            bail!(
                "controller-state stopped writer units differ from the explicit complete --writer-unit set"
            );
        }
        let online_target_fenced = options.phase == PoolMigrationControllerPhase::OnlineBounded
            && !options.writer_units.is_empty();
        if options.phase.is_final_stopped()
            && (!state.source_writers_fenced
                || !state.target_writers_fenced
                || !state.fence_held_until_completion
                || state.source_writer_processes_with_open_handles != 0
                || state.target_writer_processes_with_open_handles != 0
                || state.stopped_writer_units.is_empty()
                || state.writer_unit_masks.is_empty())
        {
            bail!(
                "stopped final controller state must attest held source and target writer fences, zero source and target writer handles, and stopped writer units"
            );
        }
        if online_target_fenced
            && (!state.target_writers_fenced
                || !state.fence_held_until_completion
                || state.target_writer_processes_with_open_handles != 0
                || (state.source_writers_fenced
                    && state.source_writer_processes_with_open_handles != 0)
                || state.writer_unit_masks.is_empty())
        {
            bail!(
                "target-fenced online-bounded state must attest its held target fence, zero target writer handles, and exact writer masks"
            );
        }
        if options.phase == PoolMigrationControllerPhase::OnlineBounded
            && !online_target_fenced
            && (state.source_writers_fenced
                || state.target_writers_fenced
                || state.fence_held_until_completion
                || !state.writer_unit_masks.is_empty())
        {
            bail!("ordinary online-bounded state must not claim writer fences or masks");
        }
        if options.phase.is_final_stopped() || online_target_fenced {
            validate_runtime_masked_writer_units_with_systemctl(
                systemctl,
                &state.stopped_writer_units,
                &state.writer_unit_masks,
            )
            .context("validate enforceable final writer-unit masks")?;
        } else if !state.writer_unit_masks.is_empty() {
            bail!("online-bounded controller state must not claim final runtime writer masks");
        }
        validate_legacy_worker_activation_fence_with_systemctl(
            systemctl,
            &state.legacy_worker_template_mask,
            &state.legacy_worker_instance_masks,
        )
        .context("validate legacy migration-worker activation fence")?;
        Ok(())
    }

    fn require_writer_service_name(value: &str) -> Result<()> {
        if value.is_empty()
            || value.len() > 255
            || !value.ends_with(".service")
            || value.contains('/')
            || value == "."
            || value == ".."
            || !value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric()
                    || matches!(byte, b':' | b'_' | b'.' | b'@' | b'\\' | b'-')
            })
        {
            bail!("stopped writer unit must be an exact bounded .service name");
        }
        Ok(())
    }

    fn capture_cursor_authority(
        path: &Path,
        phase: PoolMigrationControllerPhase,
    ) -> Result<CursorAuthorityV3> {
        require_absolute(path, "migration cursor")?;
        let parent = path
            .parent()
            .filter(|value| !value.as_os_str().is_empty())
            .context("migration cursor has no parent directory")?;
        let parent = canonical_directory_path(parent, "migration cursor parent")?;
        let name = path
            .file_name()
            .context("migration cursor has no file name")?;
        if parent.join(name) != path {
            bail!("migration cursor must be an exact canonical path");
        }
        let parent_identity = file_identity(&parent, "migration cursor parent")?;
        match std::fs::symlink_metadata(path) {
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(CursorAuthorityV3 {
                path: path.to_path_buf(),
                parent_identity,
                exists: false,
                value: None,
                sha256: None,
            }),
            Err(error) => Err(error).context("inspect migration cursor"),
            Ok(metadata) => {
                if !metadata.file_type().is_file() {
                    bail!("migration cursor is not a regular file");
                }
                if phase.is_final_stopped() {
                    bail!("stopped final controller launch requires an absent fresh cursor");
                }
                let bytes = std::fs::read(path).context("read migration cursor")?;
                if bytes.len() > 1024 {
                    bail!("migration cursor is larger than 1024 bytes");
                }
                let value = bytes
                    .strip_suffix(b"\n")
                    .context("migration cursor must end in exactly one newline")?;
                if value.contains(&b'\n') || value.contains(&b'\r') {
                    bail!("migration cursor contains more than one line");
                }
                let value = std::str::from_utf8(value).context("migration cursor is not UTF-8")?;
                if value == "complete" {
                    bail!("complete migration cursor is terminal and cannot be launched");
                }
                require_lower_hex("migration cursor", value, 64)?;
                Ok(CursorAuthorityV3 {
                    path: path.to_path_buf(),
                    parent_identity,
                    exists: true,
                    value: Some(value.to_string()),
                    sha256: Some(sha256_bytes(&bytes)),
                })
            }
        }
    }

    fn validate_cursor_matches(cursor: &CursorAuthorityV3) -> Result<()> {
        let current = capture_cursor_authority(
            &cursor.path,
            if cursor.exists {
                PoolMigrationControllerPhase::OnlineBounded
            } else {
                PoolMigrationControllerPhase::FinalStoppedFull
            },
        )?;
        if &current != cursor {
            bail!("migration cursor changed after controller preflight");
        }
        Ok(())
    }

    fn parse_additional_cas(values: &[String]) -> Result<Vec<(String, PathBuf)>> {
        let mut labels = HashSet::new();
        let mut paths = HashSet::new();
        let mut parsed = Vec::with_capacity(values.len());
        for value in values {
            let (label, path) = value
                .split_once('=')
                .context("--cas must be exactly LABEL=/absolute/path")?;
            require_safe_component("additional CAS label", label, 128)?;
            let path = PathBuf::from(path);
            require_absolute(&path, &format!("additional CAS {label}"))?;
            if !labels.insert(label.to_string()) {
                bail!("duplicate additional CAS label {label}");
            }
            let canonical = canonical_regular_path(&path, &format!("additional CAS {label}"))?;
            if !paths.insert(canonical.clone()) {
                bail!(
                    "multiple additional CAS labels reference {}",
                    canonical.display()
                );
            }
            parsed.push((label.to_string(), canonical));
        }
        parsed.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(parsed)
    }

    #[allow(clippy::too_many_arguments)]
    fn validate_authority_isolation(
        rollout: &Path,
        attempts: &Path,
        source: &Path,
        source_external: Option<&Path>,
        pool: &Path,
        topology: &PoolTopologyV3,
        cursor: &Path,
        evidence: &[&PathBuf],
        additional_cas: &[(String, PinnedAuthorityFile)],
    ) -> Result<()> {
        let cursor_parent = cursor
            .parent()
            .context("migration cursor has no parent directory")?;
        let mut roots = vec![
            ("source LMDB".to_string(), source.to_path_buf()),
            ("target Pool catalog".to_string(), pool.to_path_buf()),
            (
                "migration cursor parent".to_string(),
                cursor_parent.to_path_buf(),
            ),
        ];
        if let Some(path) = source_external {
            roots.push(("source external directory".to_string(), path.to_path_buf()));
        }
        for member in &topology.members {
            roots.push((
                format!("Pool member {} directory", member.id),
                member.path.clone(),
            ));
            if let Some(path) = &member.external_path {
                roots.push((
                    format!("Pool member {} external directory", member.id),
                    path.clone(),
                ));
            }
        }
        for left in 0..roots.len() {
            for right in left + 1..roots.len() {
                if paths_overlap(&roots[left].1, &roots[right].1) {
                    bail!("{} overlaps {}", roots[left].0, roots[right].0);
                }
            }
        }
        for (label, root) in &roots {
            if paths_overlap(root, rollout) || paths_overlap(root, attempts) {
                bail!("{label} overlaps the Pool migration rollout authority");
            }
        }
        for path in evidence {
            for (label, root) in &roots {
                if path.starts_with(root) {
                    bail!("evidence {} is stored inside {label}", path.display());
                }
            }
        }
        for (cas_label, cas) in additional_cas {
            for (root_label, root) in &roots {
                if cas.path.starts_with(root) {
                    bail!("additional CAS {cas_label} is stored inside {root_label}");
                }
            }
        }
        Ok(())
    }

    fn paths_overlap(left: &Path, right: &Path) -> bool {
        left.starts_with(right) || right.starts_with(left)
    }

    fn require_environment_value(value: &str, label: &str) -> Result<()> {
        if value.is_empty()
            || value
                .bytes()
                .any(|byte| byte.is_ascii_whitespace() || byte < 0x20 || byte == 0x7f)
        {
            bail!("{label} must be nonempty and contain no whitespace/control bytes");
        }
        Ok(())
    }

    fn build_environment(
        options: &PoolMigrationControllerOptions,
        request_path: &Path,
        source: &Path,
        source_external: Option<&Path>,
        cursor: &Path,
    ) -> Result<Vec<u8>> {
        let target_data = path_utf8(&options.target_data_dir, "target data directory")?;
        let request = path_utf8(request_path, "launch request")?;
        let source = path_utf8(source, "source LMDB")?;
        let cursor = path_utf8(cursor, "migration cursor")?;
        for (value, label) in [
            (target_data, "target data directory"),
            (request, "launch request"),
            (source, "source LMDB"),
            (cursor, "migration cursor"),
        ] {
            require_environment_value(value, label)?;
        }
        let external = source_external
            .map(|path| {
                let path = path_utf8(path, "source external directory")?;
                require_environment_value(path, "source external directory")?;
                Ok::<_, anyhow::Error>(format!("--source-external-dir {path}"))
            })
            .transpose()?
            .unwrap_or_default();
        let limit = options
            .max_items
            .map(|value| format!("--max-items {value}"))
            .unwrap_or_default();
        let text = format!(
            "HTREE_POOL_TARGET_DATA_DIR={target_data}\n\
HTREE_POOL_LAUNCH_REQUEST={request}\n\
HTREE_POOL_LAUNCH_WAIT_SECONDS={}\n\
HTREE_POOL_SOURCE_LMDB_DIR={source}\n\
HTREE_POOL_SOURCE_EXTERNAL_ARGS={external}\n\
HTREE_POOL_STATE_FILE={cursor}\n\
HTREE_POOL_BATCH_SIZE={}\n\
HTREE_POOL_MAX_BUFFER_MIB={}\n\
HTREE_POOL_SOURCE_READ_CONCURRENCY={}\n\
HTREE_POOL_REOPEN_BATCHES={}\n\
HTREE_POOL_LIMIT_ARGS={limit}\n",
            options.launch_request_wait.as_secs(),
            options.batch_size,
            options.max_buffer_mib,
            options.source_read_concurrency,
            options.reopen_batches,
        );
        Ok(text.into_bytes())
    }

    fn build_worker_argv(
        options: &PoolMigrationControllerOptions,
        binary: &Path,
        request_path: &Path,
        source: &Path,
        source_external: Option<&Path>,
        cursor: &Path,
    ) -> Result<Vec<String>> {
        let mut argv = vec![
            path_utf8(binary, "migration binary")?.to_string(),
            "--data-dir".to_string(),
            path_utf8(&options.target_data_dir, "target data directory")?.to_string(),
            "storage".to_string(),
            "pool".to_string(),
            "migrate-lmdb".to_string(),
            "--launch-request".to_string(),
            path_utf8(request_path, "launch request")?.to_string(),
            "--launch-request-wait-seconds".to_string(),
            options.launch_request_wait.as_secs().to_string(),
            "--source".to_string(),
            path_utf8(source, "source LMDB")?.to_string(),
        ];
        if let Some(path) = source_external {
            argv.push("--source-external-dir".to_string());
            argv.push(path_utf8(path, "source external directory")?.to_string());
        }
        argv.extend([
            "--state-file".to_string(),
            path_utf8(cursor, "migration cursor")?.to_string(),
            "--batch-size".to_string(),
            options.batch_size.to_string(),
            "--max-buffer-mib".to_string(),
            options.max_buffer_mib.to_string(),
            "--source-read-concurrency".to_string(),
            options.source_read_concurrency.to_string(),
            "--reopen-batches".to_string(),
            options.reopen_batches.to_string(),
        ]);
        if let Some(max_items) = options.max_items {
            argv.push("--max-items".to_string());
            argv.push(max_items.to_string());
        }
        argv.push("--resume".to_string());
        Ok(argv)
    }

    fn path_utf8<'a>(path: &'a Path, label: &str) -> Result<&'a str> {
        path.to_str()
            .with_context(|| format!("{label} path is not UTF-8"))
    }

    fn systemctl_output(systemctl: &Path, arguments: &[&str]) -> Result<Output> {
        let mut command = Command::new(systemctl);
        command
            .env_clear()
            .env("LANG", "C")
            .arg("--system")
            .arg("--no-pager")
            .args(arguments);
        command
            .output()
            .with_context(|| format!("run trusted systemctl {}", arguments.join(" ")))
    }

    fn systemctl_success(systemctl: &Path, arguments: &[&str], label: &str) -> Result<()> {
        let output = systemctl_output(systemctl, arguments)?;
        if !output.status.success() {
            bail!(
                "{label} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }

    fn query_systemd_properties(systemctl: &Path, unit: &str) -> Result<HashMap<String, String>> {
        let mut sets = query_systemd_property_sets(systemctl, &[unit.to_string()])?;
        sets.remove(unit)
            .with_context(|| format!("systemd omitted requested unit {unit}"))
    }

    fn query_systemd_property_sets(
        systemctl: &Path,
        units: &[String],
    ) -> Result<HashMap<String, HashMap<String, String>>> {
        if units.is_empty() {
            bail!("systemd property query requires a nonempty exact unit set");
        }
        let expected = units.iter().cloned().collect::<HashSet<_>>();
        if expected.len() != units.len() {
            bail!("systemd property query contains duplicate unit names");
        }
        let properties = [
            "Id",
            "LoadState",
            "UnitFileState",
            "ActiveState",
            "SubState",
            "Result",
            "InvocationID",
            "MainPID",
            "ControlPID",
            "NRestarts",
            "Job",
            "FragmentPath",
            "DropInPaths",
            "NeedDaemonReload",
            "Type",
            "Restart",
            "EnvironmentFiles",
            "Environment",
            "PassEnvironment",
            "UnsetEnvironment",
            "ExecCondition",
            "ExecStartPre",
            "ExecStart",
            "ExecStartPost",
            "ExecReload",
            "ExecStop",
            "ExecStopPost",
            "PrivateNetwork",
            "NoNewPrivileges",
            "TimeoutStartUSec",
            "UID",
            "GID",
            "BindsTo",
        ];
        let mut arguments = vec!["show"];
        arguments.extend(units.iter().map(String::as_str));
        let property_arguments = properties
            .iter()
            .map(|property| format!("--property={property}"))
            .collect::<Vec<_>>();
        for property in &property_arguments {
            arguments.push(property);
        }
        let output = systemctl_output(systemctl, &arguments)?;
        if !output.status.success() {
            bail!(
                "systemd rejected batched Pool migration unit query: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let text =
            String::from_utf8(output.stdout).context("batched systemd properties are not UTF-8")?;
        let mut parsed_units = HashMap::new();
        for block in text.split("\n\n").filter(|block| !block.trim().is_empty()) {
            let mut parsed = HashMap::new();
            for line in block.lines() {
                let (name, value) = line
                    .split_once('=')
                    .context("systemd returned a malformed batched property line")?;
                if name.is_empty() || parsed.insert(name.to_string(), value.to_string()).is_some() {
                    bail!("systemd returned an empty or duplicate batched property {name}");
                }
            }
            let id = parsed
                .get("Id")
                .context("systemd batched property block omitted Id")?
                .clone();
            if !expected.contains(&id) {
                bail!("systemd returned unrequested batched unit {id}");
            }
            if parsed_units.insert(id.clone(), parsed).is_some() {
                bail!("systemd returned duplicate batched unit {id}");
            }
        }
        if parsed_units.len() != expected.len() {
            bail!("systemd omitted one or more requested batched units");
        }
        Ok(parsed_units)
    }

    fn property<'a>(properties: &'a HashMap<String, String>, name: &str) -> Result<&'a str> {
        properties
            .get(name)
            .map(String::as_str)
            .with_context(|| format!("systemd omitted required {name} property"))
    }

    fn validate_loaded_unit_common(
        properties: &HashMap<String, String>,
        fragment: &PinnedAuthorityFile,
        environment_file: &Path,
        binary: &Path,
        expected_type: &str,
    ) -> Result<()> {
        if property(properties, "LoadState")? != "loaded" {
            bail!("systemd migration unit is not loaded");
        }
        if property(properties, "FragmentPath")? != path_utf8(&fragment.path, "fragment")? {
            bail!("systemd FragmentPath differs from the explicit fragment authority");
        }
        if !property(properties, "DropInPaths")?.is_empty() {
            bail!("systemd migration unit must have no drop-ins");
        }
        if property(properties, "NeedDaemonReload")? != "no" {
            bail!("systemd migration unit has stale loaded fragment state");
        }
        if property(properties, "Type")? != expected_type
            || property(properties, "Restart")? != "no"
        {
            bail!("systemd migration unit must be Type={expected_type} with Restart=no");
        }
        if property(properties, "PrivateNetwork")? != "yes"
            || property(properties, "NoNewPrivileges")? != "yes"
            || property(properties, "TimeoutStartUSec")? != "infinity"
        {
            bail!("systemd migration unit is missing required launch isolation");
        }
        let expected_environment = path_utf8(environment_file, "systemd environment file")?;
        let environment_files = property(properties, "EnvironmentFiles")?;
        if environment_files != expected_environment
            && environment_files != format!("{expected_environment} (ignore_errors=no)")
        {
            bail!("systemd migration unit EnvironmentFiles differs from the explicit authority");
        }
        for name in ["Environment", "PassEnvironment"] {
            if !property(properties, name)?.is_empty() {
                bail!("systemd migration unit must have empty {name}");
            }
        }
        let unset = property(properties, "UnsetEnvironment")?
            .split_ascii_whitespace()
            .collect::<HashSet<_>>();
        for name in [
            "LD_PRELOAD",
            "LD_AUDIT",
            "LD_LIBRARY_PATH",
            "DYLD_INSERT_LIBRARIES",
            "DYLD_LIBRARY_PATH",
            "HTREE_LMDB_NO_SYNC",
            "HTREE_LMDB_NO_META_SYNC",
        ] {
            if !unset.contains(name) {
                bail!("systemd migration unit must unset {name}");
            }
        }
        for name in [
            "ExecCondition",
            "ExecStartPre",
            "ExecStartPost",
            "ExecReload",
            "ExecStop",
            "ExecStopPost",
        ] {
            if properties.get(name).is_some_and(|value| !value.is_empty()) {
                bail!("systemd migration unit has forbidden nonempty {name}");
            }
        }
        let exec_start = property(properties, "ExecStart")?;
        let exec_path = exec_start
            .strip_prefix("{ path=")
            .and_then(|remaining| remaining.split_once(" ;"))
            .map(|(path, _)| path);
        if exec_start.matches("{ path=").count() != 1 || exec_path != binary.to_str() {
            bail!("systemd migration unit must have one exact direct ExecStart binary");
        }
        Ok(())
    }

    fn validate_pristine_systemd_unit(
        systemctl: &Path,
        unit: &str,
        fragment: &PinnedAuthorityFile,
        environment_file: &Path,
        binary: &Path,
        controller_unit: &str,
    ) -> Result<()> {
        let properties = query_systemd_properties(systemctl, unit)?;
        validate_loaded_unit_common(&properties, fragment, environment_file, binary, "oneshot")?;
        if !property(&properties, "BindsTo")?
            .split_ascii_whitespace()
            .any(|bound| bound == controller_unit)
        {
            bail!("systemd migration worker is not BindsTo its exact root controller instance");
        }
        if property(&properties, "ActiveState")? != "inactive"
            || property(&properties, "SubState")? != "dead"
            || property(&properties, "Result")? != "success"
            || !property(&properties, "InvocationID")?.is_empty()
            || property(&properties, "MainPID")? != "0"
            || property(&properties, "ControlPID")? != "0"
            || property(&properties, "NRestarts")? != "0"
            || !property(&properties, "Job")?.is_empty()
        {
            bail!(
                "systemd migration unit is not a pristine never-started inactive service instance"
            );
        }
        Ok(())
    }

    fn validate_running_controller_service(
        systemctl: &Path,
        unit: &str,
        invocation_id: &str,
        fragment: &PinnedAuthorityFile,
        environment_file: &Path,
        binary: &Path,
        expected_pid: u32,
    ) -> Result<()> {
        require_controller_systemd_service_name(unit)?;
        let properties = query_systemd_properties(systemctl, unit)?;
        validate_running_controller_properties(
            &properties,
            invocation_id,
            fragment,
            environment_file,
            binary,
            expected_pid,
        )
    }

    fn validate_running_controller_properties(
        properties: &HashMap<String, String>,
        invocation_id: &str,
        fragment: &PinnedAuthorityFile,
        environment_file: &Path,
        binary: &Path,
        expected_pid: u32,
    ) -> Result<()> {
        validate_loaded_unit_common(&properties, fragment, environment_file, binary, "exec")?;
        let active = property(&properties, "ActiveState")?;
        let substate = property(&properties, "SubState")?;
        if active != "active"
            || substate != "running"
            || property(&properties, "InvocationID")? != invocation_id
            || property(&properties, "MainPID")? != expected_pid.to_string()
            || property(&properties, "ControlPID")? != "0"
            || property(&properties, "NRestarts")? != "0"
            || !property(&properties, "Job")?.is_empty()
            || property(&properties, "GID")? != "0"
        {
            bail!(
                "root checkpoint broker is not the exact active, restart-free dedicated controller systemd invocation"
            );
        }
        Ok(())
    }

    fn create_attempt_namespace(path: &Path) -> Result<()> {
        match std::fs::create_dir(path) {
            Ok(()) => {
                set_owner_mode(path, 0, 0, 0o755, "attempts-v3 namespace")?;
                sync_parent(path, "attempts-v3 namespace")?;
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("create attempts-v3 namespace {}", path.display()))
            }
        }
        let metadata =
            std::fs::symlink_metadata(path).context("inspect created attempts-v3 namespace")?;
        if !metadata.file_type().is_dir()
            || metadata.uid() != 0
            || metadata.gid() != 0
            || metadata.mode() & 0o7777 != 0o755
        {
            bail!("attempts-v3 namespace must be exactly root:root mode 0755");
        }
        let canonical = path
            .canonicalize()
            .context("canonicalize created attempts-v3 namespace")?;
        if canonical != path {
            bail!("attempts-v3 namespace changed to a non-canonical path");
        }
        Ok(())
    }

    fn create_attempt_directory(path: &Path, service_gid: u32) -> Result<()> {
        require_absent(path, "fresh attempt directory")?;
        std::fs::create_dir(path)
            .with_context(|| format!("create fresh attempt directory {}", path.display()))?;
        set_owner_mode(path, 0, service_gid, 0o1770, "fresh attempt directory")?;
        sync_parent(path, "fresh attempt directory")?;
        let metadata =
            std::fs::symlink_metadata(path).context("inspect fresh attempt directory")?;
        if !metadata.file_type().is_dir()
            || metadata.uid() != 0
            || metadata.gid() != service_gid
            || metadata.mode() & 0o7777 != 0o1770
        {
            bail!("fresh attempt directory does not have exact root:service mode 1770 authority");
        }
        Ok(())
    }

    fn set_owner_mode(path: &Path, uid: u32, gid: u32, mode: u32, label: &str) -> Result<()> {
        let c_path = c_string(path.as_os_str(), label)?;
        if unsafe { libc::chown(c_path.as_ptr(), uid, gid) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("chown {label} {}", path.display()));
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .with_context(|| format!("chmod {label} {}", path.display()))
    }

    fn set_root_service_path_authority(
        path: &Path,
        directory: bool,
        gid: u32,
        mode: u32,
        label: &str,
    ) -> Result<()> {
        let mut options = OpenOptions::new();
        options.read(true).custom_flags(
            libc::O_CLOEXEC | libc::O_NOFOLLOW | if directory { libc::O_DIRECTORY } else { 0 },
        );
        let file = options
            .open(path)
            .with_context(|| format!("open {label} {}", path.display()))?;
        if unsafe { libc::fchown(file.as_raw_fd(), 0, gid) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("set {label} ownership"));
        }
        if unsafe { libc::fchmod(file.as_raw_fd(), mode) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("set {label} mode"));
        }
        file.sync_all()
            .with_context(|| format!("fsync {label} after authority update"))?;
        validate_root_service_path_authority(path, directory, gid, mode, label)
    }

    fn validate_root_service_path_authority(
        path: &Path,
        directory: bool,
        gid: u32,
        mode: u32,
        label: &str,
    ) -> Result<()> {
        let metadata =
            std::fs::symlink_metadata(path).with_context(|| format!("inspect {label}"))?;
        let type_valid = if directory {
            metadata.file_type().is_dir()
        } else {
            metadata.file_type().is_file() && metadata.nlink() == 1
        };
        if !type_valid
            || metadata.uid() != 0
            || metadata.gid() != gid
            || metadata.mode() & 0o7777 != mode
        {
            bail!("{label} ownership/mode differs from root audit authority");
        }
        Ok(())
    }

    fn durable_create_atomic(
        path: &Path,
        bytes: &[u8],
        mode: u32,
        uid: u32,
        gid: u32,
        nonce: &str,
    ) -> Result<()> {
        require_absolute(path, "durable controller output")?;
        require_absent(path, "durable controller output")?;
        let parent = path
            .parent()
            .filter(|value| !value.as_os_str().is_empty())
            .context("durable controller output has no parent directory")?;
        let parent_canonical = parent
            .canonicalize()
            .context("canonicalize durable controller output parent")?;
        if parent_canonical != parent {
            bail!("durable controller output parent is not an exact canonical path");
        }
        let name = path
            .file_name()
            .context("durable controller output has no file name")?
            .to_string_lossy();
        let temporary = parent.join(format!(
            ".{name}.controller-{}-{}.tmp",
            std::process::id(),
            &nonce[..16]
        ));
        require_absent(&temporary, "durable controller staging file")?;
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&temporary)
                .with_context(|| {
                    format!(
                        "create durable controller staging file {}",
                        temporary.display()
                    )
                })?;
            if unsafe { libc::fchown(file.as_raw_fd(), uid, gid) } != 0 {
                return Err(std::io::Error::last_os_error())
                    .context("set durable controller staging ownership");
            }
            if unsafe { libc::fchmod(file.as_raw_fd(), mode as libc::mode_t) } != 0 {
                return Err(std::io::Error::last_os_error())
                    .context("set durable controller staging mode");
            }
            file.write_all(bytes)
                .context("write durable controller staging bytes")?;
            file.sync_all()
                .context("fsync durable controller staging file")?;
            drop(file);
            rename_noreplace(&temporary, path)?;
            sync_directory(parent, "durable controller output parent")?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result
    }

    fn rename_noreplace(source: &Path, target: &Path) -> Result<()> {
        let source = c_string(source.as_os_str(), "controller staging path")?;
        let target = c_string(target.as_os_str(), "controller final path")?;
        let result = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                source.as_ptr(),
                libc::AT_FDCWD,
                target.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error())
                .context("atomically publish controller output without replacement");
        }
        Ok(())
    }

    fn c_string(value: &OsStr, label: &str) -> Result<CString> {
        CString::new(value.as_bytes()).with_context(|| format!("{label} contains a NUL byte"))
    }

    fn sync_parent(path: &Path, label: &str) -> Result<()> {
        let parent = path
            .parent()
            .filter(|value| !value.as_os_str().is_empty())
            .with_context(|| format!("{label} has no parent"))?;
        sync_directory(parent, &format!("{label} parent"))
    }

    fn sync_directory(path: &Path, label: &str) -> Result<()> {
        File::open(path)
            .with_context(|| format!("open {label} {}", path.display()))?
            .sync_all()
            .with_context(|| format!("fsync {label} {}", path.display()))
    }

    fn validate_created_authority(path: &Path, expected_sha256: &str, label: &str) -> Result<()> {
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .with_context(|| format!("open created {label} {}", path.display()))?;
        let metadata = file
            .metadata()
            .with_context(|| format!("inspect created {label}"))?;
        if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            bail!("created {label} is not root-owned immutable authority");
        }
        let mut hasher = Sha256::new();
        std::io::copy(&mut file, &mut hasher).with_context(|| format!("hash created {label}"))?;
        let actual = hex::encode(hasher.finalize());
        if actual != expected_sha256 {
            bail!("created {label} SHA-256 differs from its pinned input");
        }
        let path_metadata = std::fs::symlink_metadata(path)
            .with_context(|| format!("reinspect created {label}"))?;
        if FileSnapshot::from_metadata(&path_metadata) != FileSnapshot::from_metadata(&metadata) {
            bail!("created {label} changed during verification");
        }
        Ok(())
    }

    fn wait_for_process_identity(
        expectation: &ProcessIdentityExpectation<'_>,
    ) -> Result<ProcessIdentity> {
        let systemctl = expectation.systemctl;
        let unit = expectation.unit;
        let fragment_path = expectation.fragment_path;
        let environment_file = expectation.environment_file;
        let binary = expectation.binary;
        let service_gid = expectation.service_gid;
        let expected_argv = expectation.argv;
        let request_wait = expectation.request_wait;
        let fragment = PinnedAuthorityFile::open_hashed(fragment_path, "systemd unit fragment")?;
        let reserve = Duration::from_millis(250);
        let maximum = request_wait
            .saturating_sub(reserve)
            .min(Duration::from_secs(30));
        if maximum.is_zero() {
            bail!("launch-request wait leaves no time to capture the systemd process");
        }
        let deadline = Instant::now() + maximum;
        let mut last_observed_executable = None;
        let mut argv_mismatch_since = None;
        let mut last_observed_argv = None;
        loop {
            let properties = query_systemd_properties(systemctl, unit)?;
            validate_loaded_unit_common(
                &properties,
                &fragment,
                environment_file,
                binary,
                "oneshot",
            )?;
            if !property(&properties, "BindsTo")?
                .split_ascii_whitespace()
                .any(|bound| bound == expectation.controller_unit)
            {
                bail!("Pool migration worker lost its exact root-controller BindsTo authority");
            }
            let active = property(&properties, "ActiveState")?;
            let sub = property(&properties, "SubState")?;
            if matches!(active, "failed" | "inactive") {
                bail!("Pool migration unit terminated before request publication ({active}/{sub})");
            }
            let invocation_id = property(&properties, "InvocationID")?;
            let main_pid = property(&properties, "MainPID")?
                .parse::<u32>()
                .context("parse systemd MainPID")?;
            if !invocation_id.is_empty() && main_pid != 0 {
                require_lower_hex("systemd InvocationID", invocation_id, 32)?;
                if property(&properties, "ControlPID")? != "0"
                    || property(&properties, "NRestarts")? != "0"
                {
                    bail!("Pool migration unit has a control process or restart");
                }
                let gid = property(&properties, "GID")?
                    .parse::<u32>()
                    .context("parse systemd service GID")?;
                if gid != service_gid {
                    bail!("systemd service GID differs from --service-gid");
                }
                let observed_executable = match std::fs::read_link(format!("/proc/{main_pid}/exe"))
                {
                    Ok(path) => Some(path),
                    Err(error) if error.kind() == ErrorKind::NotFound => None,
                    Err(error) => {
                        return Err(error)
                            .context("read Pool migration /proc executable during systemd exec")
                    }
                };
                if observed_executable.as_deref() == Some(binary) {
                    let observed_argv = match read_process_argv(main_pid) {
                        Ok(argv) => Some(argv),
                        Err(error) if io_error_kind(&error) == Some(ErrorKind::NotFound) => None,
                        Err(error) => {
                            return Err(error)
                                .context("read Pool migration argv during systemd exec")
                        }
                    };
                    if observed_argv.as_deref() != Some(expected_argv) {
                        last_observed_argv = observed_argv;
                        let mismatch_since = argv_mismatch_since.get_or_insert_with(Instant::now);
                        if mismatch_since.elapsed() >= Duration::from_millis(250) {
                            bail!(
                                "Pool migration /proc argv did not stabilize to the controller-generated argv; last argv was {:?}",
                                last_observed_argv
                            );
                        }
                        thread::sleep(Duration::from_millis(25));
                        continue;
                    }
                    let identity = validate_process_identity(
                        main_pid,
                        invocation_id,
                        unit,
                        binary,
                        expected_argv,
                        gid,
                    )?;
                    let second = query_systemd_properties(systemctl, unit)?;
                    if property(&second, "InvocationID")? != identity.invocation_id
                        || property(&second, "MainPID")? != identity.main_pid.to_string()
                        || process_start_time(identity.main_pid)? != identity.start_time_ticks
                    {
                        bail!("systemd Pool migration process identity changed during capture");
                    }
                    return Ok(identity);
                }
                last_observed_executable = observed_executable;
            }
            if Instant::now() >= deadline {
                if let Some(argv) = last_observed_argv {
                    bail!(
                        "timed out after {} ms waiting for Pool migration argv to stabilize; last argv was {:?}",
                        maximum.as_millis(),
                        argv
                    );
                }
                if let Some(executable) = last_observed_executable {
                    bail!(
                        "timed out after {} ms waiting for Pool migration MainPID to exec {}; last executable was {}",
                        maximum.as_millis(),
                        binary.display(),
                        executable.display()
                    );
                }
                bail!(
                    "timed out after {} ms waiting for Pool migration MainPID/InvocationID",
                    maximum.as_millis()
                );
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn validate_process_identity(
        pid: u32,
        invocation_id: &str,
        unit: &str,
        binary: &Path,
        expected_argv: &[String],
        expected_gid: u32,
    ) -> Result<ProcessIdentity> {
        let proc = PathBuf::from(format!("/proc/{pid}"));
        let proc_metadata =
            std::fs::metadata(&proc).context("inspect Pool migration /proc identity")?;
        let uid = proc_metadata.uid();
        let gid = proc_metadata.gid();
        if uid == 0 {
            bail!("Pool migration service must not run as root");
        }
        if gid != expected_gid {
            bail!("Pool migration MainPID GID differs from the systemd service identity");
        }
        let start_time_ticks = process_start_time(pid)?;
        let executable =
            std::fs::read_link(proc.join("exe")).context("read Pool migration /proc executable")?;
        if executable != binary {
            bail!(
                "Pool migration MainPID executable differs from the explicit binary: expected {}, got {}",
                binary.display(),
                executable.display()
            );
        }
        let cmdline = read_process_argv(pid)?;
        if cmdline != expected_argv {
            bail!("Pool migration /proc argv differs from the controller-generated argv");
        }
        let environment =
            std::fs::read(proc.join("environ")).context("read Pool migration /proc environment")?;
        let actual_invocation = environment
            .split(|byte| *byte == 0)
            .find_map(|entry| entry.strip_prefix(b"INVOCATION_ID="))
            .context("Pool migration process environment has no INVOCATION_ID")?;
        if actual_invocation != invocation_id.as_bytes() {
            bail!("Pool migration process INVOCATION_ID differs from systemd");
        }
        let cgroup = std::fs::read_to_string(proc.join("cgroup"))
            .context("read Pool migration /proc cgroup")?;
        if !cgroup.lines().any(|line| {
            line.rsplit_once(':')
                .is_some_and(|(_, path)| Path::new(path).file_name() == Some(OsStr::new(unit)))
        }) {
            bail!("Pool migration MainPID is not in the exact requested systemd unit cgroup");
        }
        if process_start_time(pid)? != start_time_ticks {
            bail!("Pool migration MainPID was reused during identity capture");
        }
        let final_proc_metadata =
            std::fs::metadata(&proc).context("reinspect Pool migration /proc identity")?;
        if final_proc_metadata.uid() != uid || final_proc_metadata.gid() != gid {
            bail!("Pool migration MainPID UID/GID changed during identity capture");
        }
        Ok(ProcessIdentity {
            invocation_id: invocation_id.to_string(),
            main_pid: pid,
            start_time_ticks,
            uid,
            gid,
        })
    }

    fn read_process_argv(pid: u32) -> Result<Vec<String>> {
        std::fs::read(format!("/proc/{pid}/cmdline"))
            .context("read Pool migration /proc cmdline")?
            .split(|byte| *byte == 0)
            .filter(|argument| !argument.is_empty())
            .map(|argument| {
                String::from_utf8(argument.to_vec())
                    .context("Pool migration /proc argv is not UTF-8")
            })
            .collect()
    }

    fn process_start_time(pid: u32) -> Result<u64> {
        let path = format!("/proc/{pid}/stat");
        let stat =
            std::fs::read_to_string(&path).with_context(|| format!("read process stat {path}"))?;
        let command_end = stat
            .rfind(") ")
            .with_context(|| format!("parse process stat {path}"))?;
        stat[command_end + 2..]
            .split_ascii_whitespace()
            .nth(19)
            .context("process stat has no starttime field")?
            .parse::<u64>()
            .context("parse process starttime")
    }

    fn io_error_kind(error: &anyhow::Error) -> Option<ErrorKind> {
        error
            .chain()
            .find_map(|cause| cause.downcast_ref::<std::io::Error>())
            .map(std::io::Error::kind)
    }

    fn validate_running_worker(
        systemctl: &Path,
        unit: &str,
        fragment: &PinnedAuthorityFile,
        environment_file: &Path,
        binary: &Path,
        controller_unit: &str,
        expected_argv: &[String],
        process: &ProcessIdentity,
    ) -> Result<()> {
        let properties = query_systemd_properties(systemctl, unit)?;
        validate_running_worker_with_properties(
            &properties,
            unit,
            fragment,
            environment_file,
            binary,
            controller_unit,
            expected_argv,
            process,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn validate_running_worker_with_properties(
        properties: &HashMap<String, String>,
        unit: &str,
        fragment: &PinnedAuthorityFile,
        environment_file: &Path,
        binary: &Path,
        controller_unit: &str,
        expected_argv: &[String],
        process: &ProcessIdentity,
    ) -> Result<()> {
        validate_loaded_unit_common(&properties, fragment, environment_file, binary, "oneshot")?;
        if !property(&properties, "BindsTo")?
            .split_ascii_whitespace()
            .any(|bound| bound == controller_unit)
        {
            bail!("Pool migration worker lost its exact root-controller BindsTo authority");
        }
        validate_running_worker_properties(&properties, process)?;
        let revalidated = validate_process_identity(
            process.main_pid,
            &process.invocation_id,
            unit,
            binary,
            expected_argv,
            process.gid,
        )
        .context("revalidate live Pool migration worker process provenance")?;
        if revalidated.main_pid != process.main_pid
            || revalidated.start_time_ticks != process.start_time_ticks
            || revalidated.uid != process.uid
            || revalidated.gid != process.gid
            || revalidated.invocation_id != process.invocation_id
        {
            bail!("Pool migration worker process provenance changed while brokered");
        }
        if process_start_time(process.main_pid)? != process.start_time_ticks {
            bail!("Pool migration worker process changed after batched systemd validation");
        }
        Ok(())
    }

    fn validate_running_worker_properties(
        properties: &HashMap<String, String>,
        process: &ProcessIdentity,
    ) -> Result<()> {
        if matches!(property(properties, "ActiveState")?, "inactive" | "failed")
            || property(properties, "InvocationID")? != process.invocation_id
            || property(properties, "MainPID")? != process.main_pid.to_string()
            || property(properties, "ControlPID")? != "0"
            || property(properties, "NRestarts")? != "0"
            || property(properties, "UID")? != process.uid.to_string()
            || property(properties, "GID")? != process.gid.to_string()
        {
            bail!("Pool migration systemd worker identity changed while brokered");
        }
        Ok(())
    }

    fn validate_checkpoint_namespace(attempt: &Path, next_sequence: u64) -> Result<()> {
        let mut requests = HashSet::new();
        let mut acknowledgements = HashSet::new();
        for entry in std::fs::read_dir(attempt).context("enumerate checkpoint namespace")? {
            let entry = entry.context("enumerate checkpoint namespace entry")?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                bail!("checkpoint namespace contains a non-UTF-8 entry");
            };
            let parsed = if let Some(value) = name
                .strip_prefix("checkpoint-request-")
                .and_then(|value| value.strip_suffix(".json"))
            {
                Some((true, value))
            } else if let Some(value) = name
                .strip_prefix("checkpoint-ack-")
                .and_then(|value| value.strip_suffix(".json"))
            {
                Some((false, value))
            } else if name.starts_with("checkpoint-") {
                bail!("checkpoint namespace contains malformed checkpoint entry {name}");
            } else {
                None
            };
            let Some((is_request, value)) = parsed else {
                continue;
            };
            if value.len() != 20 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                bail!("checkpoint namespace contains malformed sequence entry {name}");
            }
            let sequence = value
                .parse::<u64>()
                .with_context(|| format!("parse checkpoint sequence in {name}"))?;
            if sequence > next_sequence {
                bail!("checkpoint sequence {sequence} was published out of order");
            }
            let inserted = if is_request {
                requests.insert(sequence)
            } else {
                acknowledgements.insert(sequence)
            };
            if !inserted {
                bail!("checkpoint namespace contains a duplicate sequence entry");
            }
        }
        for sequence in 0..next_sequence {
            if !requests.contains(&sequence) || !acknowledgements.contains(&sequence) {
                bail!("checkpoint chain is missing completed sequence {sequence}");
            }
        }
        if acknowledgements.contains(&next_sequence) {
            bail!("checkpoint acknowledgement {next_sequence} was prepublished");
        }
        Ok(())
    }

    fn validate_checkpoint_frontier(attempt: &Path, next_sequence: u64) -> Result<()> {
        let acknowledgement = attempt.join(ack_file_name(next_sequence));
        match std::fs::symlink_metadata(&acknowledgement) {
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| {
                format!(
                    "inspect next checkpoint acknowledgement {}",
                    acknowledgement.display()
                )
            }),
            Ok(_) => bail!(
                "checkpoint acknowledgement {} was prepublished before root authorization",
                acknowledgement.display()
            ),
        }
    }

    fn read_checkpoint_request(
        path: &Path,
        expected_uid: u32,
        expected_gid: u32,
    ) -> Result<Option<(MigrationCheckpointRequestV3, Vec<u8>)>> {
        let mut file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("open checkpoint request {}", path.display()))
            }
        };
        let before = file.metadata().context("inspect checkpoint request")?;
        if !before.file_type().is_file()
            || before.uid() != expected_uid
            || before.gid() != expected_gid
            || before.mode() & 0o7777 != 0o640
            || before.nlink() != 1
            || before.len() > MAX_CHECKPOINT_BYTES
        {
            bail!(
                "checkpoint request is not an exact worker-owned mode 0640 bounded single-link file"
            );
        }
        let mut bytes = Vec::with_capacity(before.len() as usize);
        file.read_to_end(&mut bytes)
            .context("read checkpoint request")?;
        let after = file.metadata().context("reinspect checkpoint request")?;
        let path_metadata =
            std::fs::symlink_metadata(path).context("reinspect checkpoint request path")?;
        if FileSnapshot::from_metadata(&before) != FileSnapshot::from_metadata(&after)
            || FileSnapshot::from_metadata(&before) != FileSnapshot::from_metadata(&path_metadata)
        {
            bail!("checkpoint request changed while the controller read it");
        }
        let request = serde_json::from_slice(&bytes)
            .context("parse strict Pool migration checkpoint request")?;
        Ok(Some((request, bytes)))
    }

    #[allow(clippy::too_many_arguments)]
    fn validate_checkpoint_request(
        checkpoint: &MigrationCheckpointRequestV3,
        sequence: u64,
        previous_ack_sha256: Option<&str>,
        process: &ProcessIdentity,
        launch: &PoolMigrationLaunchRequestV3,
        launch_request_sha256: &str,
        phase: PoolMigrationControllerPhase,
        batch_size: usize,
        expected_source_reconciliations: u64,
        progress: &mut CheckpointProgress,
    ) -> Result<()> {
        validate_checkpoint_operation(&checkpoint.operation)?;
        if checkpoint.schema != CHECKPOINT_REQUEST_SCHEMA
            || checkpoint.sequence != sequence
            || checkpoint.previous_ack_sha256.as_deref() != previous_ack_sha256
            || checkpoint.worker_pid != process.main_pid
            || checkpoint.worker_proc_start_time_ticks != process.start_time_ticks
            || checkpoint.broker_pid != launch.checkpoint_broker.pid
            || checkpoint.broker_proc_start_time_ticks
                != launch.checkpoint_broker.proc_start_time_ticks
            || checkpoint.boot_id != launch.boot_id
            || checkpoint.attempt_nonce != launch.nonce
            || checkpoint.launch_request_sha256 != launch_request_sha256
        {
            bail!("checkpoint request does not exactly extend the authorized launch chain");
        }
        if let Some(cursor) = &checkpoint.cursor {
            require_lower_hex("checkpoint cursor", cursor, 64)?;
        }
        if let Some(cursor) = &checkpoint.audit_target_cursor {
            require_lower_hex("checkpoint audit target cursor", cursor, 64)?;
        }
        let mut previous_audit_hash: Option<&str> = None;
        for entry in &checkpoint.audit_entries {
            require_lower_hex("checkpoint audit hash", &entry.hash, 64)?;
            if previous_audit_hash.is_some_and(|previous| previous >= entry.hash.as_str()) {
                bail!("checkpoint audit entries must be unique and strictly sorted");
            }
            previous_audit_hash = Some(&entry.hash);
        }
        let timeout = timeout_millis(Duration::from_secs(
            launch.checkpoint_broker.timeout_seconds,
        ))?;
        if checkpoint.requested_at_boottime_millis == 0
            || checkpoint.requested_at_boottime_millis.checked_add(timeout)
                != Some(checkpoint.start_before_boottime_millis)
        {
            bail!("checkpoint request has an invalid authorization interval");
        }
        let now = boottime_millis()?;
        if checkpoint.requested_at_boottime_millis > now
            || checkpoint.start_before_boottime_millis < now
        {
            bail!("checkpoint request is future-dated or already expired");
        }

        let require_batch_range = || -> Result<()> {
            if !checkpoint
                .range_limit
                .is_some_and(|limit| limit > 0 && limit <= batch_size as u64)
            {
                bail!("bounded checkpoint range must be within the configured batch size");
            }
            Ok(())
        };
        let require_no_range = || -> Result<()> {
            if checkpoint.range_limit.is_some() {
                bail!("non-batch checkpoint must not claim a range limit");
            }
            Ok(())
        };
        let require_no_audit = || -> Result<()> {
            if !checkpoint.audit_entries.is_empty() || checkpoint.audit_target_cursor.is_some() {
                bail!("non-audit checkpoint must not carry audit state");
            }
            Ok(())
        };

        match phase {
            PoolMigrationControllerPhase::OnlineBounded => match checkpoint.operation.as_str() {
                "migration-batch" if !progress.online_evidence_published => {
                    require_batch_range()?;
                    require_no_audit()?;
                }
                "online-source-audit-batch" if !progress.online_evidence_published => {
                    require_batch_range()?;
                    if checkpoint.audit_entries.is_empty()
                        || checkpoint.audit_entries.len() as u64
                            > checkpoint.range_limit.unwrap_or(0)
                        || checkpoint.audit_target_cursor.is_some()
                    {
                        bail!("online source audit checkpoint has invalid proof entries");
                    }
                }
                "online-target-audit-batch" if !progress.online_evidence_published => {
                    require_batch_range()?;
                    if checkpoint.audit_entries.len() as u64 > checkpoint.range_limit.unwrap_or(0)
                        || checkpoint.audit_target_cursor != checkpoint.cursor
                        || checkpoint.audit_target_cursor.is_none()
                    {
                        bail!("online target audit checkpoint has invalid proof/cursor state");
                    }
                }
                "online-target-audit-reset" if !progress.online_evidence_published => {
                    require_no_range()?;
                    require_no_audit()?;
                }
                "online-evidence-publication" if !progress.online_evidence_published => {
                    require_no_range()?;
                    require_no_audit()?;
                    progress.online_evidence_published = true;
                }
                "online-audit-publication"
                    if progress.online_evidence_published && !progress.online_audit_published =>
                {
                    require_no_range()?;
                    require_no_audit()?;
                    progress.online_audit_published = true;
                }
                "online-readiness" if progress.online_audit_published && !progress.online_ready => {
                    require_no_range()?;
                    require_no_audit()?;
                    progress.online_ready = true;
                }
                _ => bail!(
                    "online checkpoint operation {} is out of order",
                    checkpoint.operation
                ),
            },
            PoolMigrationControllerPhase::FinalStoppedSource => match checkpoint.operation.as_str()
            {
                "source-keyset-audit" if !progress.source_keyset_audited => {
                    require_no_range()?;
                    progress.source_keyset_audited = true;
                }
                "source-evidence-publication"
                    if progress.source_keyset_audited && !progress.source_evidence_published =>
                {
                    require_no_range()?;
                    progress.source_evidence_published = true;
                }
                "source-generation-fingerprint"
                    if progress.source_keyset_audited
                        && progress.source_evidence_published
                        && !progress.source_generation_fingerprinted =>
                {
                    require_no_range()?;
                    progress.source_generation_fingerprinted = true;
                }
                "source-terminal-publication"
                    if progress.source_generation_fingerprinted
                        && !progress.source_receipt_published =>
                {
                    require_no_range()?;
                    progress.source_receipt_published = true;
                }
                "terminal-readiness"
                    if progress.source_receipt_published && !progress.terminal_ready =>
                {
                    require_no_range()?;
                    progress.terminal_ready = true;
                }
                _ => bail!(
                    "source-final checkpoint operation {} is out of order",
                    checkpoint.operation
                ),
            },
            PoolMigrationControllerPhase::FinalStoppedFull => match checkpoint.operation.as_str() {
                "migration-batch" if !progress.target_terminal_audited => {
                    require_batch_range()?;
                }
                "source-evidence-consumed"
                    if progress.source_reconciliations < expected_source_reconciliations
                        && !progress.target_terminal_audited =>
                {
                    require_no_range()?;
                    progress.source_reconciliations = progress
                        .source_reconciliations
                        .checked_add(1)
                        .context("source reconciliation checkpoint count overflow")?;
                }
                "target-terminal-audit"
                    if progress.source_reconciliations == expected_source_reconciliations
                        && !progress.target_terminal_audited =>
                {
                    require_no_range()?;
                    progress.target_terminal_audited = true;
                }
                "terminal-receipt-publication"
                    if progress.target_terminal_audited && !progress.terminal_receipt_published =>
                {
                    require_no_range()?;
                    progress.terminal_receipt_published = true;
                }
                "terminal-readiness"
                    if progress.terminal_receipt_published && !progress.terminal_ready =>
                {
                    require_no_range()?;
                    progress.terminal_ready = true;
                }
                _ => bail!(
                    "full-final checkpoint operation {} is out of order",
                    checkpoint.operation
                ),
            },
        }
        if phase != PoolMigrationControllerPhase::OnlineBounded {
            require_no_audit()?;
        }
        Ok(())
    }

    fn validate_worker_terminal_file(
        path: &Path,
        expected_uid: u32,
        expected_gid: u32,
        expected_mode: u32,
        label: &str,
    ) -> Result<BoundedFileAuthorityV3> {
        let (authority, bytes) = capture_bounded_worker_file_authority(
            path,
            expected_uid,
            expected_gid,
            expected_mode,
            label,
        )?;
        let value: Value =
            serde_json::from_slice(&bytes).with_context(|| format!("parse strict {label} JSON"))?;
        let expected_schema = match label {
            "source-terminal receipt" => "hashtree-pool-migration-source-terminal/v3",
            "online target audit receipt" => ONLINE_TARGET_AUDIT_SCHEMA,
            _ => "hashtree-pool-migration-terminal-audit/v3",
        };
        if value.get("schema").and_then(Value::as_str) != Some(expected_schema)
            || value.get("status").and_then(Value::as_str) != Some("verified")
        {
            bail!("{label} has an invalid schema or status");
        }
        Ok(authority)
    }

    fn validate_terminal_cursor(
        path: &Path,
        expected_gid: u32,
        expected_value: &str,
    ) -> Result<()> {
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .context("open terminal migration cursor")?;
        let metadata = file
            .metadata()
            .context("inspect terminal migration cursor")?;
        if !metadata.file_type().is_file()
            || metadata.uid() != 0
            || metadata.gid() != expected_gid
            || metadata.mode() & 0o7777 != 0o440
            || metadata.nlink() != 1
            || metadata.len() > 1024
        {
            bail!("terminal migration cursor has invalid root-controller authority");
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.read_to_end(&mut bytes)
            .context("read terminal migration cursor")?;
        if bytes != format!("{expected_value}\n").as_bytes() {
            bail!("terminal migration cursor does not contain {expected_value}");
        }
        Ok(())
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct RecoverableTerminalAuditReceiptV3 {
        schema: String,
        status: String,
        controller_state_sha256: String,
        source_receipt_sha256: Vec<String>,
        source_count: u64,
        source_entries: u64,
        source_bytes: u64,
        source_reconciliation_sha256: String,
        target_content_proof_entries: u64,
        target_content_proof_bytes: u64,
        target_content_proof_sha256: String,
        target_stored_locations: u64,
        target_stored_bytes: u64,
        target_catalog_sha256: String,
        target_physical_sha256: String,
        target_manifest_sha256: String,
    }

    #[allow(clippy::too_many_arguments)]
    fn authorize_terminal_recovery(
        attempt_dir: &Path,
        publication: &PreparedTerminalPublicationV3,
        options: &PoolMigrationControllerOptions,
        current_boot_id: &str,
        state: &ControllerStateV3,
        controller_state_sha256: &str,
        source_baseline_sha256: &str,
        topology: &PoolTopologyV3,
        topology_sha256: &str,
        source_identity: LmdbIdentityV3,
        source_external_identity: Option<FileIdentityV3>,
        pool_identity: LmdbIdentityV3,
        systemctl: &Path,
    ) -> Result<()> {
        if publication.intent.boot_id != current_boot_id {
            bail!("terminal recovery authorization belongs to a different boot");
        }
        if publication.intent.phase != options.phase.as_protocol_str() {
            bail!(
                "pending terminal publication phase {} differs from requested recovery phase {}",
                publication.intent.phase,
                options.phase.as_protocol_str()
            );
        }
        validate_batched_runtime_masked_final_fence_with_systemctl(
            systemctl,
            &state.stopped_writer_units,
            &state.writer_unit_masks,
            &state.legacy_worker_template_mask,
            &state.legacy_worker_instance_masks,
        )
        .context("revalidate the batched final writer fence before terminal recovery audit")?;
        match options.phase {
            PoolMigrationControllerPhase::FinalStoppedSource => {
                authorize_source_terminal_recovery(
                    attempt_dir,
                    publication,
                    options,
                    current_boot_id,
                    state,
                    controller_state_sha256,
                    source_baseline_sha256,
                    topology,
                    topology_sha256,
                    source_identity,
                    source_external_identity,
                    pool_identity,
                )?;
            }
            PoolMigrationControllerPhase::FinalStoppedFull => {
                authorize_full_terminal_recovery(
                    publication,
                    options,
                    current_boot_id,
                    state,
                    controller_state_sha256,
                    topology,
                    topology_sha256,
                    pool_identity,
                )?;
            }
            PoolMigrationControllerPhase::OnlineBounded => {
                bail!("online-bounded migration cannot recover a terminal publication")
            }
        }
        validate_batched_runtime_masked_final_fence_with_systemctl(
            systemctl,
            &state.stopped_writer_units,
            &state.writer_unit_masks,
            &state.legacy_worker_template_mask,
            &state.legacy_worker_instance_masks,
        )
        .context("revalidate the batched final writer fence after terminal recovery audit")
    }

    #[allow(clippy::too_many_arguments)]
    fn authorize_source_terminal_recovery(
        attempt_dir: &Path,
        publication: &PreparedTerminalPublicationV3,
        options: &PoolMigrationControllerOptions,
        current_boot_id: &str,
        state: &ControllerStateV3,
        controller_state_sha256: &str,
        source_baseline_sha256: &str,
        topology: &PoolTopologyV3,
        topology_sha256: &str,
        source_identity: LmdbIdentityV3,
        source_external_identity: Option<FileIdentityV3>,
        pool_identity: LmdbIdentityV3,
    ) -> Result<()> {
        let bytes = read_bounded_file_authority(
            &publication.intent.terminal_authority,
            "recoverable source-terminal receipt",
        )?;
        let receipt: PoolMigrationSourceTerminalReceiptV3 =
            serde_json::from_slice(&bytes).context("parse recoverable source-terminal receipt")?;
        validate_source_terminal_receipt_shape(&receipt)?;
        let online_authorities = parse_additional_cas(&options.additional_cas)?
            .into_iter()
            .filter(|(label, _)| label.starts_with("online-target-audit-"))
            .map(|(label, path)| {
                let file = PinnedAuthorityFile::open_bytes(
                    &path,
                    "recoverable online target audit certification",
                    MAX_ADDITIONAL_CAS_BYTES,
                )?;
                Ok(NamedFileAuthorityV3 {
                    label,
                    path: file.path,
                    sha256: file.sha256,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let migration_binary =
            PinnedAuthorityFile::open_hashed(&options.migration_binary, "migration binary")?;
        let online_target = load_validated_online_target_audit(
            &online_authorities,
            &OnlineTargetAuditExpectationV3 {
                rollout_id: &options.rollout_id,
                worker_binary_sha256: &migration_binary.sha256,
                source_baseline_sha256,
                source_path: &options.source,
                source_lmdb_identity: source_identity,
                source_external_path: options.source_external_dir.as_deref(),
                source_external_identity,
                pool_path: &options.pool,
                pool_lmdb_identity: pool_identity,
                pool_topology_sha256: topology_sha256,
                pool_manifest_sha256: &state.pool_manifest_sha256,
                target_writer_units: &state.stopped_writer_units,
                target_writer_unit_masks: &state.writer_unit_masks,
                legacy_worker_template_mask: &state.legacy_worker_template_mask,
                legacy_worker_instance_masks: &state.legacy_worker_instance_masks,
                expected_service_gid: options.service_gid,
                validate_evidence_content: true,
            },
        )?
        .context("recoverable source-terminal receipt has no online target audit")?;
        if receipt.online_target_audit_certification_sha256 != online_target.certification_sha256
            || receipt.online_target_verified_entries
                != online_target.receipt.target_verified_entries
            || receipt.online_target_verified_bytes != online_target.receipt.target_verified_bytes
            || receipt.online_target_content_sha256 != online_target.receipt.target_content_sha256
            || receipt.online_target_evidence != online_target.receipt.target_evidence
        {
            bail!(
                "recoverable source-terminal receipt does not propagate the exact certified online target proof"
            );
        }
        if receipt.schema != SOURCE_TERMINAL_SCHEMA
            || receipt.status != "verified"
            || receipt.phase != "final-stopped-source"
            || receipt.boot_id != current_boot_id
            || receipt.attempt_nonce != publication.intent.attempt_nonce
            || receipt.controller_state_sha256 != controller_state_sha256
            || receipt.source_baseline_sha256 != source_baseline_sha256
            || receipt.source_path != options.source
            || receipt.source_lmdb_identity != source_identity
            || receipt.source_external_path != options.source_external_dir
            || receipt.source_external_identity != source_external_identity
            || receipt.pool_path != options.pool
            || receipt.pool_lmdb_identity != pool_identity
            || receipt.pool_topology_sha256 != topology_sha256
            || receipt.pool_manifest_sha256 != state.pool_manifest_sha256
            || receipt.pool_topology != *topology
            || receipt.stopped_writer_units != state.stopped_writer_units
            || receipt.writer_unit_masks != state.writer_unit_masks
            || receipt.legacy_worker_template_mask != state.legacy_worker_template_mask
            || receipt.legacy_worker_instance_masks != state.legacy_worker_instance_masks
            || !receipt.source_read_only
            || !receipt.target_audit_deferred
        {
            bail!("recoverable source-terminal receipt differs from current root authority");
        }
        if publication.intent.terminal_authority.path != attempt_dir.join("source-terminal.json")
            || receipt.terminal_cursor.path != options.state_file
            || !receipt.terminal_cursor.exists
            || receipt.terminal_cursor.value.as_deref() != Some("source-complete")
            || receipt.terminal_cursor.sha256.as_deref()
                != Some(&sha256_bytes(b"source-complete\n"))
        {
            bail!("recoverable source-terminal receipt has an invalid terminal cursor authority");
        }
        validate_source_read_only_mount_authority(
            &receipt.source_read_only_mounts,
            &options.source,
            source_identity,
            options.source_external_dir.as_deref(),
            source_external_identity,
        )
        .context("revalidate retained source mounts before terminal recovery")?;
        validate_source_evidence_metadata(
            &receipt.source_evidence,
            Some(options.service_gid),
            true,
        )
        .context("revalidate frozen source evidence authority before terminal recovery")?;
        census_recovery_source_handles(
            state,
            &options.source,
            options.source_external_dir.as_deref(),
            source_external_identity,
        )?;
        census_recovery_target_handles(state, topology)?;
        let source_directory =
            PinnedDirectory::open_exact(&options.source, "recoverable source LMDB directory")?;
        source_directory.require_authority_identity(
            source_identity.directory,
            "recoverable source LMDB directory",
        )?;
        let source_external_directory = match (
            options.source_external_dir.as_deref(),
            source_external_identity,
        ) {
            (Some(path), Some(identity)) => {
                let directory =
                    PinnedDirectory::open_exact(path, "recoverable source external directory")?;
                directory.require_authority_identity(
                    identity,
                    "recoverable source external directory",
                )?;
                Some(directory)
            }
            (None, None) => None,
            _ => bail!("recoverable source external authority is incomplete"),
        };
        validate_frozen_source_generation(
            &receipt,
            &source_directory.runtime_path(),
            source_external_directory
                .as_ref()
                .map(PinnedDirectory::runtime_path)
                .as_deref(),
        )
        .context("revalidate exact frozen source generation before terminal recovery")?;
        validate_frozen_source_receipt_evidence(
            &receipt,
            &online_target.receipt,
            &source_directory.runtime_path(),
            source_external_directory
                .as_ref()
                .map(PinnedDirectory::runtime_path)
                .as_deref(),
            options.batch_size,
            options.source_read_concurrency,
        )
        .context("root replay frozen source catalog against terminal and online evidence")?;
        drop(source_external_directory);
        drop(source_directory);
        census_recovery_source_handles(
            state,
            &options.source,
            options.source_external_dir.as_deref(),
            source_external_identity,
        )?;
        census_recovery_target_handles(state, topology)
    }

    fn validate_frozen_source_receipt_evidence(
        receipt: &PoolMigrationSourceTerminalReceiptV3,
        online: &PoolMigrationOnlineTargetAuditReceiptV3,
        source_runtime_path: &Path,
        source_external_runtime_path: Option<&Path>,
        page_size: usize,
        source_read_concurrency: usize,
    ) -> Result<()> {
        if page_size == 0 {
            bail!("frozen source evidence replay page size must be non-zero");
        }
        let external =
            source_external_runtime_path.map(|path| hashtree_lmdb::ExternalBlobOptions {
                base_path: path.to_path_buf(),
                min_bytes: 1,
                sync: true,
                pack_target_bytes: None,
            });
        let reader =
            hashtree_lmdb::LmdbBlobReader::open_with_external_read_concurrency_and_pinned_identity(
                source_runtime_path,
                external,
                source_read_concurrency,
                hashtree_lmdb::PinnedLmdbIdentity {
                    data: hashtree_lmdb::PinnedLmdbFileIdentity {
                        device: receipt.source_lmdb_identity.data.device,
                        inode: receipt.source_lmdb_identity.data.inode,
                    },
                    lock: hashtree_lmdb::PinnedLmdbFileIdentity {
                        device: receipt.source_lmdb_identity.lock.device,
                        inode: receipt.source_lmdb_identity.lock.inode,
                    },
                },
            )
            .context("root-open frozen source for evidence replay")?;
        let generation = reader.environment_generation();
        let keyset = reader
            .validate_terminal_migration_keyset()
            .context("root-audit frozen source keyset")?;
        if keyset.blob_entries != receipt.source_blob_entries
            || keyset.metadata_entries != receipt.source_metadata_entries
            || keyset.blob_only_entries != receipt.source_blob_only_entries
            || keyset.legacy_blob_only != receipt.source_legacy_blob_only
            || keyset.inline_entries != receipt.source_inline_entries
            || keyset.loose_external_entries != receipt.source_loose_external_entries
            || keyset.packed_external_entries != receipt.source_packed_external_entries
            || hashtree_core::to_hex(&keyset.sha256) != receipt.source_keyset_sha256
            || hashtree_core::to_hex(&keyset.catalog_location_sha256)
                != receipt.source_catalog_location_sha256
        {
            bail!("root frozen source keyset differs from its terminal receipt");
        }

        let mut terminal = SourceEvidenceManifestReaderV3::open(&receipt.source_evidence)?;
        let mut next_terminal = terminal.next_entry()?;
        let mut online_evidence = SourceEvidenceManifestReaderV3::open(&online.source_evidence)?;
        let mut next_online = online_evidence.next_entry()?;
        let mut cursor = None;
        let mut entries = 0u64;
        let mut bytes = 0u64;
        let mut hasher = Sha256::new();
        hasher.update(b"hashtree-pool-migration-source-content/v3\0");
        loop {
            let hashes = reader.scan_hashes_after(cursor, page_size)?;
            if hashes.is_empty() {
                break;
            }
            let sizes = reader.sizes_for_sorted_hashes(&hashes)?;
            for (hash, size) in hashes.iter().copied().zip(sizes) {
                match next_terminal {
                    Some((evidence_hash, evidence_size))
                        if evidence_hash == hash && evidence_size == size =>
                    {
                        next_terminal = terminal.next_entry()?;
                    }
                    Some((evidence_hash, evidence_size)) => bail!(
                        "terminal source evidence {} / {} bytes differs from frozen source {} / {} bytes",
                        hashtree_core::to_hex(&evidence_hash),
                        evidence_size,
                        hashtree_core::to_hex(&hash),
                        size
                    ),
                    None => bail!(
                        "frozen source {} / {} bytes is absent from terminal source evidence",
                        hashtree_core::to_hex(&hash),
                        size
                    ),
                }
                while next_online.is_some_and(|(online_hash, _)| online_hash < hash) {
                    next_online = online_evidence.next_entry()?;
                }
                match next_online {
                    Some((online_hash, online_size))
                        if online_hash == hash && online_size == size =>
                    {
                        next_online = online_evidence.next_entry()?;
                    }
                    Some((online_hash, online_size)) if online_hash == hash => bail!(
                        "frozen source size {} for {} differs from certified online size {}",
                        size,
                        hashtree_core::to_hex(&hash),
                        online_size
                    ),
                    _ => bail!(
                        "frozen source {} / {} bytes is absent from certified online source evidence",
                        hashtree_core::to_hex(&hash),
                        size
                    ),
                }
                entries = entries
                    .checked_add(1)
                    .context("frozen source evidence entry count overflow")?;
                bytes = bytes
                    .checked_add(size)
                    .context("frozen source evidence byte count overflow")?;
                hasher.update(hash);
                hasher.update(size.to_be_bytes());
            }
            cursor = hashes.last().copied();
        }
        if let Some((hash, size)) = next_terminal {
            bail!(
                "terminal source evidence has extra entry {} / {} bytes",
                hashtree_core::to_hex(&hash),
                size
            );
        }
        while next_online.is_some() {
            next_online = online_evidence.next_entry()?;
        }
        let terminal_summary = terminal.validated_summary()?;
        let online_summary = online_evidence.validated_summary()?;
        let content_sha256: [u8; 32] = hasher.finalize().into();
        if entries != receipt.source_verified_entries
            || bytes != receipt.source_verified_bytes
            || hashtree_core::to_hex(&content_sha256) != receipt.source_content_sha256
            || terminal_summary.entries != entries
            || terminal_summary.bytes != bytes
            || terminal_summary.content_sha256 != content_sha256
        {
            bail!("root frozen source content differs from its terminal evidence or receipt");
        }
        if online_summary.entries != online.source_verified_entries
            || online_summary.bytes != online.source_verified_bytes
            || hashtree_core::to_hex(&online_summary.content_sha256) != online.source_content_sha256
        {
            bail!("certified online source evidence changed during root frozen-source replay");
        }
        if reader.environment_generation() != generation {
            bail!("frozen source LMDB generation changed during root evidence replay");
        }
        Ok(())
    }

    fn authorize_full_terminal_recovery(
        publication: &PreparedTerminalPublicationV3,
        options: &PoolMigrationControllerOptions,
        current_boot_id: &str,
        state: &ControllerStateV3,
        controller_state_sha256: &str,
        topology: &PoolTopologyV3,
        topology_sha256: &str,
        pool_identity: LmdbIdentityV3,
    ) -> Result<()> {
        let bytes = read_bounded_file_authority(
            &publication.intent.terminal_authority,
            "recoverable terminal Pool audit receipt",
        )?;
        let receipt: RecoverableTerminalAuditReceiptV3 = serde_json::from_slice(&bytes)
            .context("parse recoverable terminal Pool audit receipt")?;
        if receipt.schema != "hashtree-pool-migration-terminal-audit/v3"
            || receipt.status != "verified"
            || receipt.controller_state_sha256 != controller_state_sha256
            || receipt.source_receipt_sha256 != state.source_terminal_receipt_sha256
            || receipt.source_count != state.source_terminal_receipt_sha256.len() as u64
            || receipt.source_entries > receipt.target_stored_locations
            || receipt.source_bytes > receipt.target_stored_bytes
            || receipt.target_manifest_sha256 != state.pool_manifest_sha256
            || topology.manifest_sha256 != state.pool_manifest_sha256
        {
            bail!("recoverable terminal Pool audit differs from current root authority");
        }
        for (label, value) in [
            (
                "source reconciliation",
                receipt.source_reconciliation_sha256.as_str(),
            ),
            (
                "target content proof",
                receipt.target_content_proof_sha256.as_str(),
            ),
            ("target catalog", receipt.target_catalog_sha256.as_str()),
            ("target physical", receipt.target_physical_sha256.as_str()),
            ("target manifest", receipt.target_manifest_sha256.as_str()),
        ] {
            require_lower_hex(label, value, 64)?;
        }
        let source_receipts = load_recovery_source_receipts(
            options,
            current_boot_id,
            state,
            topology,
            topology_sha256,
            pool_identity,
        )?;
        let source_evidence = source_receipts
            .iter()
            .map(|source| source.receipt.source_evidence.clone())
            .collect::<Vec<_>>();
        let mut source_union = SourceEvidenceUnionReaderV3::open(&source_evidence)?;
        while source_union.next_entry()?.is_some() {}
        let source_summaries = source_union.validated_source_summaries()?;
        for (source, evidence) in source_receipts.iter().zip(source_summaries) {
            if evidence.entries != source.receipt.source_verified_entries
                || evidence.bytes != source.receipt.source_verified_bytes
                || hashtree_core::to_hex(&evidence.content_sha256)
                    != source.receipt.source_content_sha256
            {
                bail!("recoverable source evidence differs from its certified source receipt");
            }
        }
        let source_union_summary = source_union.validated_union_summary()?;
        if source_union_summary.entries != receipt.source_entries
            || source_union_summary.bytes != receipt.source_bytes
            || hashtree_core::to_hex(&source_union_summary.content_sha256)
                != receipt.source_reconciliation_sha256
        {
            bail!("recoverable source evidence union differs from terminal receipt");
        }
        let target_evidence = source_receipts
            .iter()
            .map(|source| source.receipt.online_target_evidence.clone())
            .collect::<Vec<_>>();
        census_recovery_target_handles(state, topology)?;
        let (actual, target_content) = audit_recoverable_target_pool(
            &options.pool,
            pool_identity,
            topology,
            topology_sha256,
            &target_evidence,
            options.batch_size,
        )?;
        for (source, evidence) in source_receipts.iter().zip(&target_content.evidence) {
            if evidence.entries != source.receipt.online_target_verified_entries
                || evidence.bytes != source.receipt.online_target_verified_bytes
                || hashtree_core::to_hex(&evidence.content_sha256)
                    != source.receipt.online_target_content_sha256
            {
                bail!("recoverable target evidence differs from its certified source receipt");
            }
        }
        if actual.stored_locations != receipt.target_stored_locations
            || actual.stored_bytes != receipt.target_stored_bytes
            || target_content.catalog.entries != receipt.target_content_proof_entries
            || target_content.catalog.bytes != receipt.target_content_proof_bytes
            || hashtree_core::to_hex(&target_content.catalog.content_sha256)
                != receipt.target_content_proof_sha256
            || target_content.catalog.entries != actual.stored_locations
            || target_content.catalog.bytes != actual.stored_bytes
            || hashtree_core::to_hex(&actual.catalog_sha256) != receipt.target_catalog_sha256
            || hashtree_core::to_hex(&actual.physical_sha256) != receipt.target_physical_sha256
            || hashtree_core::to_hex(&actual.manifest_sha256) != receipt.target_manifest_sha256
        {
            bail!("target Pool changed after its terminal audit and before cursor publication");
        }
        census_recovery_target_handles(state, topology)
    }

    fn load_recovery_source_receipts(
        options: &PoolMigrationControllerOptions,
        current_boot_id: &str,
        state: &ControllerStateV3,
        topology: &PoolTopologyV3,
        topology_sha256: &str,
        pool_identity: LmdbIdentityV3,
    ) -> Result<Vec<ValidatedSourceTerminalReceiptV3>> {
        let authorities = parse_additional_cas(&options.additional_cas)?
            .into_iter()
            .map(|(label, path)| {
                let file = PinnedAuthorityFile::open_bytes(
                    &path,
                    &format!("recoverable CAS {label}"),
                    MAX_ADDITIONAL_CAS_BYTES,
                )?;
                Ok(NamedFileAuthorityV3 {
                    label,
                    path: file.path,
                    sha256: file.sha256,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let receipts = load_validated_prior_source_terminal_receipts(
            &authorities,
            &PriorSourceReceiptExpectationV3 {
                boot_id: current_boot_id,
                pool_path: &options.pool,
                pool_lmdb_identity: pool_identity,
                pool_topology_sha256: topology_sha256,
                pool_manifest_sha256: &state.pool_manifest_sha256,
                pool_topology: topology,
                stopped_writer_units: &state.stopped_writer_units,
                writer_unit_masks: &state.writer_unit_masks,
                legacy_worker_template_mask: &state.legacy_worker_template_mask,
                legacy_worker_instance_masks: &state.legacy_worker_instance_masks,
                expected_service_gid: Some(options.service_gid),
                validate_physical_generation: false,
            },
        )?;
        let receipt_sha256 = receipts
            .iter()
            .map(|source| source.authority_sha256.clone())
            .collect::<Vec<_>>();
        if receipt_sha256 != state.source_terminal_receipt_sha256 {
            bail!("recoverable source receipt set differs from controller-state authority");
        }
        Ok(receipts)
    }

    fn audit_recoverable_target_pool(
        pool: &Path,
        pool_identity: LmdbIdentityV3,
        topology: &PoolTopologyV3,
        topology_sha256: &str,
        target_evidence: &[super::super::pool_migration_evidence::SourceEvidenceManifestAuthorityV3],
        page_size: usize,
    ) -> Result<(
        hashtree_lmdb::PoolPhysicalAudit,
        super::super::pool_migration_evidence::TargetEvidenceReplayV3,
    )> {
        require_lower_hex("recoverable Pool topology", topology_sha256, 64)?;
        let catalog =
            PinnedDirectory::open_exact(pool, "recoverable target Pool catalog directory")?;
        catalog.require_authority_identity(
            pool_identity.directory,
            "recoverable target Pool catalog directory",
        )?;
        let mut retained_members = Vec::with_capacity(topology.members.len());
        for member in &topology.members {
            let directory = PinnedDirectory::open_exact(
                &member.path,
                &format!("recoverable Pool member {} directory", member.id),
            )?;
            directory.require_authority_identity(
                member.directory_identity,
                &format!("recoverable Pool member {} directory", member.id),
            )?;
            let external_directory = match (
                member.external_path.as_deref(),
                member.external_directory_identity,
            ) {
                (Some(path), Some(identity)) => {
                    let external = PinnedDirectory::open_exact(
                        path,
                        &format!("recoverable Pool member {} external directory", member.id),
                    )?;
                    external.require_authority_identity(
                        identity,
                        &format!("recoverable Pool member {} external directory", member.id),
                    )?;
                    Some(external)
                }
                (None, None) => None,
                _ => bail!(
                    "recoverable Pool member {} external authority is incomplete",
                    member.id
                ),
            };
            retained_members.push((member, directory, external_directory));
        }
        let mut config = hashtree_lmdb::PoolStoreConfig::default();
        config.temperature.enabled = false;
        config.catalog_lmdb_identity = Some(hashtree_lmdb::PinnedLmdbIdentity {
            data: hashtree_lmdb::PinnedLmdbFileIdentity {
                device: pool_identity.data.device,
                inode: pool_identity.data.inode,
            },
            lock: hashtree_lmdb::PinnedLmdbFileIdentity {
                device: pool_identity.lock.device,
                inode: pool_identity.lock.inode,
            },
        });
        config.expected_manifest_sha256 = Some(
            hashtree_core::from_hex(&topology.manifest_sha256)
                .context("decode recoverable Pool manifest SHA-256")?,
        );
        config.member_runtime_paths = retained_members
            .iter()
            .map(|(member, directory, external_directory)| {
                Ok(hashtree_lmdb::PoolMemberRuntimePaths {
                    id: member.id.parse()?,
                    configured_path: member.path.clone(),
                    runtime_path: directory.runtime_path(),
                    configured_external_path: member.external_path.clone(),
                    runtime_external_path: external_directory
                        .as_ref()
                        .map(PinnedDirectory::runtime_path),
                    lmdb_identity: hashtree_lmdb::PinnedLmdbIdentity {
                        data: hashtree_lmdb::PinnedLmdbFileIdentity {
                            device: member.lmdb_identity.data.device,
                            inode: member.lmdb_identity.data.inode,
                        },
                        lock: hashtree_lmdb::PinnedLmdbFileIdentity {
                            device: member.lmdb_identity.lock.device,
                            inode: member.lmdb_identity.lock.inode,
                        },
                    },
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let reader = hashtree_lmdb::PoolStoreReader::open(catalog.runtime_path(), config)
            .context("open exact target Pool for terminal recovery audit")?;
        let target_content =
            validate_terminal_catalog_target_evidence(&reader, target_evidence, page_size, || {
                Ok(())
            })
            .context("replay certified target content evidence during recovery")?;
        let physical = reader
            .validate_terminal_catalog_and_physical_state()
            .context("replay exact target Pool catalog/physical audit during recovery")?;
        Ok((physical, target_content))
    }

    fn census_recovery_source_handles(
        state: &ControllerStateV3,
        source_path: &Path,
        source_external_path: Option<&Path>,
        source_external_identity: Option<FileIdentityV3>,
    ) -> Result<()> {
        for _ in 0..2 {
            let live = capture_live_process_authorities(None)?;
            let mut claimed = HashMap::new();
            validate_lmdb_census_against_live(
                &live,
                &mut claimed,
                "recoverable source LMDB",
                state.source_lmdb_identity,
            )?;
            match (source_external_path, source_external_identity) {
                (Some(path), Some(identity)) => validate_external_census_against_live(
                    &live,
                    "recoverable source external corpus",
                    path,
                    identity,
                    true,
                )?,
                (None, None) => {}
                _ => bail!("recoverable source external census authority is incomplete"),
            }
            if lmdb_identity(source_path, "recoverable source LMDB")? != state.source_lmdb_identity
            {
                bail!("recoverable source LMDB identity changed during handle census");
            }
        }
        Ok(())
    }

    fn census_recovery_target_handles(
        state: &ControllerStateV3,
        topology: &PoolTopologyV3,
    ) -> Result<()> {
        for _ in 0..2 {
            let live = capture_live_process_authorities(None)?;
            let mut claimed = HashMap::new();
            validate_lmdb_census_against_live(
                &live,
                &mut claimed,
                "recoverable target Pool catalog",
                state.pool_lmdb_identity,
            )?;
            for member in &topology.members {
                validate_lmdb_census_against_live(
                    &live,
                    &mut claimed,
                    &format!("recoverable Pool member {}", member.id),
                    member.lmdb_identity,
                )?;
                match (
                    member.external_path.as_deref(),
                    member.external_directory_identity,
                ) {
                    (Some(path), Some(identity)) => validate_external_census_against_live(
                        &live,
                        &format!("recoverable Pool member {} external corpus", member.id),
                        path,
                        identity,
                        true,
                    )?,
                    (None, None) => {}
                    _ => bail!("recoverable Pool member external census authority is incomplete"),
                }
            }
        }
        Ok(())
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct LiveProcessAuthority {
        holder: String,
    }

    fn census_store_process_handles(
        state: &ControllerStateV3,
        topology: &PoolTopologyV3,
        source_receipts: &[ValidatedSourceTerminalReceiptV3],
        current_source_external_path: Option<&Path>,
        current_source_external_identity: Option<FileIdentityV3>,
        waiting_worker_pid: u32,
        waiting_worker_start_time: u64,
        deep_external_census: bool,
    ) -> Result<()> {
        validate_runtime_writer_mask_authorities(
            &state.stopped_writer_units,
            &state.writer_unit_masks,
        )?;
        let live = capture_live_process_authorities(Some((
            waiting_worker_pid,
            waiting_worker_start_time,
        )))?;
        validate_store_census_against_live(
            state,
            topology,
            source_receipts,
            current_source_external_path,
            current_source_external_identity,
            &live,
            deep_external_census,
        )?;
        if deep_external_census {
            let live_after = capture_live_process_authorities(Some((
                waiting_worker_pid,
                waiting_worker_start_time,
            )))?;
            validate_store_census_against_live(
                state,
                topology,
                source_receipts,
                current_source_external_path,
                current_source_external_identity,
                &live_after,
                true,
            )?;
        }
        Ok(())
    }

    fn validate_store_census_against_live(
        state: &ControllerStateV3,
        topology: &PoolTopologyV3,
        source_receipts: &[ValidatedSourceTerminalReceiptV3],
        current_source_external_path: Option<&Path>,
        current_source_external_identity: Option<FileIdentityV3>,
        live: &HashMap<(u64, u64, u64), LiveProcessAuthority>,
        deep_external_census: bool,
    ) -> Result<()> {
        let mut lmdb_identities = HashMap::<(u64, u64, u64), String>::new();
        let mut source_identities = HashSet::new();
        source_identities.insert(state.source_lmdb_identity);
        for receipt in source_receipts {
            source_identities.insert(receipt.receipt.source_lmdb_identity);
        }
        for (index, identity) in source_identities.into_iter().enumerate() {
            validate_lmdb_census_against_live(
                live,
                &mut lmdb_identities,
                &format!("source LMDB {}", index + 1),
                identity,
            )?;
        }
        match (
            current_source_external_path,
            current_source_external_identity,
        ) {
            (Some(path), Some(identity)) => validate_external_census_against_live(
                live,
                "current source external corpus",
                path,
                identity,
                deep_external_census,
            )?,
            (None, None) => {}
            _ => bail!("current source external census authority is incomplete"),
        }
        validate_lmdb_census_against_live(
            live,
            &mut lmdb_identities,
            "target Pool catalog",
            state.pool_lmdb_identity,
        )?;
        for member in &topology.members {
            validate_lmdb_census_against_live(
                live,
                &mut lmdb_identities,
                &format!("Pool member {}", member.id),
                member.lmdb_identity,
            )?;
            match (
                member.external_path.as_deref(),
                member.external_directory_identity,
            ) {
                (Some(path), Some(identity)) => validate_external_census_against_live(
                    live,
                    &format!("Pool member {} external corpus", member.id),
                    path,
                    identity,
                    deep_external_census,
                )?,
                (None, None) => {}
                _ => bail!("Pool member external census authority is incomplete"),
            }
        }
        for source in source_receipts {
            if source.receipt.source_lmdb_identity == state.source_lmdb_identity {
                continue;
            }
            match (
                source.receipt.source_external_path.as_deref(),
                source.receipt.source_external_identity,
            ) {
                (Some(path), Some(identity)) => validate_external_census_against_live(
                    live,
                    &format!("source {} external corpus", source.authority_sha256),
                    path,
                    identity,
                    deep_external_census,
                )?,
                (None, None) => {}
                _ => bail!("source receipt external census authority is incomplete"),
            }
        }
        Ok(())
    }

    fn capture_live_process_authorities(
        waiting_worker: Option<(u32, u64)>,
    ) -> Result<HashMap<(u64, u64, u64), LiveProcessAuthority>> {
        let mut live = HashMap::new();
        let mut pids = std::fs::read_dir("/proc")
            .context("enumerate /proc for final writer-handle census")?
            .filter_map(|entry| {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => return Some(Err(error).context("enumerate /proc entry")),
                };
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                    return None;
                };
                name.parse::<u32>().ok().map(Ok)
            })
            .collect::<Result<Vec<_>>>()?;
        pids.sort_unstable();
        let mut saw_waiting_worker = waiting_worker.is_none();
        for pid in pids {
            let Some(before) = process_start_time_optional(pid)? else {
                continue;
            };
            if waiting_worker.is_some_and(|(worker_pid, _)| pid == worker_pid) {
                if waiting_worker.is_none_or(|(_, start_time)| before != start_time) {
                    bail!("Pool migration worker PID was reused during final handle census");
                }
                saw_waiting_worker = true;
                continue;
            }
            collect_process_fd_authorities(pid, &mut live)?;
            collect_process_anchor_authorities(pid, &mut live)?;
            collect_process_map_authorities(pid, &mut live)?;
            match process_start_time_optional(pid)? {
                Some(after) if after == before => {}
                Some(_) => bail!("process {pid} identity changed during final handle census"),
                None => {}
            }
        }
        if !saw_waiting_worker {
            bail!("Pool migration waiting worker disappeared during final handle census");
        }
        if let Some((waiting_worker_pid, waiting_worker_start_time)) = waiting_worker {
            if process_start_time_optional(waiting_worker_pid)? != Some(waiting_worker_start_time) {
                bail!("Pool migration waiting worker disappeared during final handle census");
            }
        }
        Ok(live)
    }

    fn census_target_writer_handles(
        state: &ControllerStateV3,
        topology: &PoolTopologyV3,
        waiting_worker_pid: u32,
        waiting_worker_start_time: u64,
        deep_external_census: bool,
    ) -> Result<()> {
        validate_runtime_writer_mask_authorities(
            &state.stopped_writer_units,
            &state.writer_unit_masks,
        )?;
        let waiting_worker = Some((waiting_worker_pid, waiting_worker_start_time));
        let live = capture_live_process_authorities(waiting_worker)?;
        validate_target_census_against_live(state, topology, &live, deep_external_census)?;
        if deep_external_census {
            let live_after = capture_live_process_authorities(waiting_worker)?;
            validate_target_census_against_live(state, topology, &live_after, true)?;
        }
        Ok(())
    }

    fn validate_target_census_against_live(
        state: &ControllerStateV3,
        topology: &PoolTopologyV3,
        live: &HashMap<(u64, u64, u64), LiveProcessAuthority>,
        deep_external_census: bool,
    ) -> Result<()> {
        let mut claimed = HashMap::new();
        validate_lmdb_census_against_live(
            live,
            &mut claimed,
            "target Pool catalog",
            state.pool_lmdb_identity,
        )?;
        for member in &topology.members {
            validate_lmdb_census_against_live(
                live,
                &mut claimed,
                &format!("Pool member {}", member.id),
                member.lmdb_identity,
            )?;
            match (
                member.external_path.as_deref(),
                member.external_directory_identity,
            ) {
                (Some(path), Some(identity)) => validate_external_census_against_live(
                    live,
                    &format!("Pool member {} external corpus", member.id),
                    path,
                    identity,
                    deep_external_census,
                )?,
                (None, None) => {}
                _ => bail!("Pool member external census authority is incomplete"),
            }
        }
        Ok(())
    }

    fn validate_lmdb_census_against_live(
        live: &HashMap<(u64, u64, u64), LiveProcessAuthority>,
        claimed: &mut HashMap<(u64, u64, u64), String>,
        label: &str,
        identity: LmdbIdentityV3,
    ) -> Result<()> {
        for (leaf, file) in [
            ("directory", identity.directory),
            ("data.mdb", identity.data),
            ("lock.mdb", identity.lock),
        ] {
            let key = (
                linux_device_major(file.device),
                linux_device_minor(file.device),
                file.inode,
            );
            let authority = format!("{label} {leaf}");
            if let Some(previous) = claimed.insert(key, authority.clone()) {
                bail!("final handle census identity aliases {previous} and {authority}");
            }
            reject_live_process_authority(live, key, &authority)?;
        }
        Ok(())
    }

    fn validate_external_census_against_live(
        live: &HashMap<(u64, u64, u64), LiveProcessAuthority>,
        label: &str,
        root: &Path,
        expected_root: FileIdentityV3,
        deep: bool,
    ) -> Result<()> {
        validate_external_ancestors_against_live(live, label, root)?;

        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(root)
            .with_context(|| format!("pin {label} root {}", root.display()))?;
        let before = directory
            .metadata()
            .with_context(|| format!("inspect pinned {label} root {}", root.display()))?;
        if !before.file_type().is_dir()
            || before.dev() != expected_root.device
            || before.ino() != expected_root.inode
            || before.dev() == 0
            || before.ino() == 0
        {
            bail!("{label} root no longer matches its exact directory authority");
        }
        reject_live_process_authority(
            live,
            census_identity_key(&before),
            &format!("{label} root"),
        )?;
        if deep {
            walk_external_census_against_live(live, label, &directory, Path::new(""))?;
        }
        let after = directory
            .metadata()
            .with_context(|| format!("reinspect pinned {label} root {}", root.display()))?;
        let path_after = std::fs::symlink_metadata(root)
            .with_context(|| format!("reinspect {label} root path {}", root.display()))?;
        if FileSnapshot::from_metadata(&before) != FileSnapshot::from_metadata(&after)
            || FileSnapshot::from_metadata(&before) != FileSnapshot::from_metadata(&path_after)
        {
            bail!("{label} root changed during final handle-census capture");
        }
        Ok(())
    }

    fn validate_external_ancestors_against_live(
        live: &HashMap<(u64, u64, u64), LiveProcessAuthority>,
        label: &str,
        root: &Path,
    ) -> Result<()> {
        let mut ancestors = root
            .ancestors()
            .skip(1)
            .take_while(|path| path.components().count() > 1)
            .collect::<Vec<_>>();
        ancestors.reverse();
        for ancestor in ancestors {
            let directory = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(ancestor)
                .with_context(|| {
                    format!(
                        "pin ancestor {} of {label} for final handle census",
                        ancestor.display()
                    )
                })?;
            let before = directory.metadata().with_context(|| {
                format!(
                    "inspect ancestor {} of {label} for final handle census",
                    ancestor.display()
                )
            })?;
            if !before.file_type().is_dir() || before.dev() == 0 || before.ino() == 0 {
                bail!(
                    "ancestor {} of {label} is not an exact directory authority",
                    ancestor.display()
                );
            }
            reject_live_process_authority(
                live,
                census_identity_key(&before),
                &format!("ancestor {} of {label}", ancestor.display()),
            )?;
            let after = directory.metadata().with_context(|| {
                format!(
                    "reinspect ancestor {} of {label} for final handle census",
                    ancestor.display()
                )
            })?;
            let path_after = std::fs::symlink_metadata(ancestor).with_context(|| {
                format!(
                    "reinspect ancestor path {} of {label} for final handle census",
                    ancestor.display()
                )
            })?;
            if FileSnapshot::from_metadata(&before) != FileSnapshot::from_metadata(&after)
                || FileSnapshot::from_metadata(&before) != FileSnapshot::from_metadata(&path_after)
            {
                bail!(
                    "ancestor {} of {label} changed during final handle-census capture",
                    ancestor.display()
                );
            }
        }
        Ok(())
    }

    fn walk_external_census_against_live(
        live: &HashMap<(u64, u64, u64), LiveProcessAuthority>,
        label: &str,
        directory: &File,
        relative_directory: &Path,
    ) -> Result<()> {
        let before = directory
            .metadata()
            .with_context(|| format!("inspect pinned {label} external directory"))?;
        if !before.file_type().is_dir() {
            bail!("{label} census traversal reached a non-directory");
        }
        let proc_directory = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
        let entries = std::fs::read_dir(&proc_directory).with_context(|| {
            format!(
                "enumerate pinned {label} external directory {}",
                relative_directory.display()
            )
        })?;
        for entry in entries {
            let entry = entry.with_context(|| {
                format!(
                    "enumerate pinned {label} external directory entry {}",
                    relative_directory.display()
                )
            })?;
            let name = entry.file_name();
            let relative = relative_directory.join(&name);
            let entry_path = proc_directory.join(&name);
            let path_before = std::fs::symlink_metadata(&entry_path).with_context(|| {
                format!(
                    "inspect pinned {label} external entry {}",
                    relative.display()
                )
            })?;
            if path_before.dev() == 0 || path_before.ino() == 0 {
                bail!(
                    "{label} external entry {} has a zero device/inode identity",
                    relative.display()
                );
            }
            let entry_label = format!("{label} external entry {}", relative.display());
            if path_before.file_type().is_dir() {
                let child = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(&entry_path)
                    .with_context(|| format!("pin {entry_label}"))?;
                let open_before = child
                    .metadata()
                    .with_context(|| format!("inspect pinned {entry_label}"))?;
                if FileSnapshot::from_metadata(&path_before)
                    != FileSnapshot::from_metadata(&open_before)
                {
                    bail!("{entry_label} changed while it was pinned");
                }
                reject_live_process_authority(
                    live,
                    census_identity_key(&open_before),
                    &entry_label,
                )?;
                walk_external_census_against_live(live, label, &child, &relative)?;
                let open_after = child
                    .metadata()
                    .with_context(|| format!("reinspect pinned {entry_label}"))?;
                let path_after = std::fs::symlink_metadata(&entry_path)
                    .with_context(|| format!("reinspect {entry_label} path"))?;
                if FileSnapshot::from_metadata(&open_before)
                    != FileSnapshot::from_metadata(&open_after)
                    || FileSnapshot::from_metadata(&open_before)
                        != FileSnapshot::from_metadata(&path_after)
                {
                    bail!("{entry_label} changed during final handle-census capture");
                }
            } else if path_before.file_type().is_file() {
                if path_before.nlink() != 1 {
                    bail!("{entry_label} must be single-link during final handle-census capture");
                }
                let file = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(&entry_path)
                    .with_context(|| format!("pin {entry_label}"))?;
                let open_before = file
                    .metadata()
                    .with_context(|| format!("inspect pinned {entry_label}"))?;
                if !open_before.file_type().is_file()
                    || open_before.nlink() != 1
                    || FileSnapshot::from_metadata(&path_before)
                        != FileSnapshot::from_metadata(&open_before)
                {
                    bail!("{entry_label} changed while it was pinned");
                }
                reject_live_process_authority(
                    live,
                    census_identity_key(&open_before),
                    &entry_label,
                )?;
                let open_after = file
                    .metadata()
                    .with_context(|| format!("reinspect pinned {entry_label}"))?;
                let path_after = std::fs::symlink_metadata(&entry_path)
                    .with_context(|| format!("reinspect {entry_label} path"))?;
                if FileSnapshot::from_metadata(&open_before)
                    != FileSnapshot::from_metadata(&open_after)
                    || FileSnapshot::from_metadata(&open_before)
                        != FileSnapshot::from_metadata(&path_after)
                {
                    bail!("{entry_label} changed during final handle-census capture");
                }
            } else {
                bail!(
                    "{label} external corpus contains a symlink or special entry {}",
                    relative.display()
                );
            }
        }
        let after = directory
            .metadata()
            .with_context(|| format!("reinspect pinned {label} external directory"))?;
        if FileSnapshot::from_metadata(&before) != FileSnapshot::from_metadata(&after) {
            bail!(
                "{label} external directory {} changed during final handle-census capture",
                relative_directory.display()
            );
        }
        Ok(())
    }

    fn reject_live_process_authority(
        live: &HashMap<(u64, u64, u64), LiveProcessAuthority>,
        key: (u64, u64, u64),
        authority: &str,
    ) -> Result<()> {
        if let Some(process) = live.get(&key) {
            bail!(
                "final migration authority {authority} remains accessible through {}",
                process.holder
            );
        }
        Ok(())
    }

    fn census_identity_key(metadata: &std::fs::Metadata) -> (u64, u64, u64) {
        (
            linux_device_major(metadata.dev()),
            linux_device_minor(metadata.dev()),
            metadata.ino(),
        )
    }

    fn linux_device_major(device: u64) -> u64 {
        ((device >> 8) & 0xfff) | ((device >> 32) & !0xfff)
    }

    fn linux_device_minor(device: u64) -> u64 {
        (device & 0xff) | ((device >> 12) & !0xff)
    }

    fn process_start_time_optional(pid: u32) -> Result<Option<u64>> {
        let path = format!("/proc/{pid}/stat");
        let stat = match std::fs::read_to_string(&path) {
            Ok(stat) => stat,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("read process stat {path}"));
            }
        };
        let command_end = stat
            .rfind(") ")
            .with_context(|| format!("parse process stat {path}"))?;
        let start = stat[command_end + 2..]
            .split_ascii_whitespace()
            .nth(19)
            .context("process stat has no starttime field")?
            .parse::<u64>()
            .context("parse process starttime")?;
        Ok(Some(start))
    }

    fn collect_process_fd_authorities(
        pid: u32,
        live: &mut HashMap<(u64, u64, u64), LiveProcessAuthority>,
    ) -> Result<()> {
        let directory = format!("/proc/{pid}/fd");
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error).with_context(|| format!("enumerate process fds {directory}"));
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("enumerate process fd beneath {directory}"));
                }
            };
            let metadata = match std::fs::metadata(entry.path()) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("inspect process fd {}", entry.path().display()));
                }
            };
            if !metadata.file_type().is_file() && !metadata.file_type().is_dir() {
                continue;
            }
            let after = match std::fs::metadata(entry.path()) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("reinspect process fd {}", entry.path().display())
                    });
                }
            };
            if census_identity_key(&metadata) != census_identity_key(&after) {
                bail!(
                    "process {pid} fd {} changed identity during final handle census",
                    entry.file_name().to_string_lossy()
                );
            }
            record_live_process_authority(
                live,
                census_identity_key(&metadata),
                format!("process {pid} fd {}", entry.file_name().to_string_lossy()),
            );
        }
        Ok(())
    }

    fn collect_process_anchor_authorities(
        pid: u32,
        live: &mut HashMap<(u64, u64, u64), LiveProcessAuthority>,
    ) -> Result<()> {
        for anchor in ["cwd", "root", "exe"] {
            let path = format!("/proc/{pid}/{anchor}");
            let before = match std::fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("inspect process {pid} {anchor} authority"));
                }
            };
            if !before.file_type().is_file() && !before.file_type().is_dir() {
                continue;
            }
            let after = match std::fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("reinspect process {pid} {anchor} authority"));
                }
            };
            if census_identity_key(&before) != census_identity_key(&after) {
                bail!("process {pid} {anchor} changed identity during final handle census");
            }
            record_live_process_authority(
                live,
                census_identity_key(&before),
                format!("process {pid} /proc/{pid}/{anchor}"),
            );
        }
        Ok(())
    }

    fn collect_process_map_authorities(
        pid: u32,
        live: &mut HashMap<(u64, u64, u64), LiveProcessAuthority>,
    ) -> Result<()> {
        let path = format!("/proc/{pid}/maps");
        let maps = match std::fs::read_to_string(&path) {
            Ok(maps) => maps,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).with_context(|| format!("read process maps {path}")),
        };
        for (index, line) in maps.lines().enumerate() {
            let fields = line.split_ascii_whitespace().collect::<Vec<_>>();
            if fields.len() < 5 {
                bail!("process maps {path} line {} is malformed", index + 1);
            }
            let (major, minor) = fields[3]
                .split_once(':')
                .with_context(|| format!("process maps {path} line {} has no device", index + 1))?;
            let major = u64::from_str_radix(major, 16)
                .with_context(|| format!("parse process maps {path} major device"))?;
            let minor = u64::from_str_radix(minor, 16)
                .with_context(|| format!("parse process maps {path} minor device"))?;
            let inode = fields[4]
                .parse::<u64>()
                .with_context(|| format!("parse process maps {path} inode"))?;
            if inode == 0 {
                continue;
            }
            record_live_process_authority(
                live,
                (major, minor, inode),
                format!("process {pid} mapping on line {}", index + 1),
            );
        }
        Ok(())
    }

    fn record_live_process_authority(
        live: &mut HashMap<(u64, u64, u64), LiveProcessAuthority>,
        key: (u64, u64, u64),
        holder: String,
    ) {
        live.entry(key)
            .or_insert_with(|| LiveProcessAuthority { holder });
    }

    fn wait_for_ack(
        path: &Path,
        systemctl: &Path,
        unit: &str,
        wait: Duration,
        expected_uid: u32,
        expected_gid: u32,
    ) -> Result<Vec<u8>> {
        let started = Instant::now();
        loop {
            if let Some(bytes) = read_stable_ack(path, expected_uid, expected_gid)? {
                return Ok(bytes);
            }
            let properties = query_systemd_properties(systemctl, unit)?;
            let active = property(&properties, "ActiveState")?;
            if matches!(active, "inactive" | "failed") {
                bail!(
                    "Pool migration unit terminated before durable acknowledgement ({active}/{})",
                    property(&properties, "SubState")?
                );
            }
            if started.elapsed() >= wait {
                bail!(
                    "timed out after {} seconds waiting for durable Pool migration acknowledgement {}",
                    wait.as_secs(),
                    path.display()
                );
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn read_stable_ack(
        path: &Path,
        expected_uid: u32,
        expected_gid: u32,
    ) -> Result<Option<Vec<u8>>> {
        let mut file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("open Pool migration acknowledgement {}", path.display())
                })
            }
        };
        let before = file
            .metadata()
            .context("inspect open Pool migration acknowledgement")?;
        if !before.file_type().is_file() || before.len() > MAX_ACK_BYTES {
            bail!("Pool migration acknowledgement is not a bounded regular file");
        }
        if before.uid() != expected_uid
            || before.gid() != expected_gid
            || before.mode() & 0o7777 != 0o600
            || before.nlink() != 1
        {
            bail!(
                "Pool migration acknowledgement is not an exact service-owned mode 0600 single-link authority"
            );
        }
        let mut bytes = Vec::with_capacity(before.len() as usize);
        file.read_to_end(&mut bytes)
            .context("read Pool migration acknowledgement")?;
        let after = file
            .metadata()
            .context("reinspect open Pool migration acknowledgement")?;
        let path_metadata = std::fs::symlink_metadata(path)
            .context("reinspect Pool migration acknowledgement path")?;
        if FileSnapshot::from_metadata(&before) != FileSnapshot::from_metadata(&after)
            || FileSnapshot::from_metadata(&before) != FileSnapshot::from_metadata(&path_metadata)
        {
            bail!("Pool migration acknowledgement changed while the controller read it");
        }
        let mut reopened = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .context("reopen Pool migration acknowledgement")?;
        let reopened_metadata = reopened
            .metadata()
            .context("inspect reopened Pool migration acknowledgement")?;
        let mut second = Vec::with_capacity(reopened_metadata.len() as usize);
        reopened
            .read_to_end(&mut second)
            .context("reread Pool migration acknowledgement")?;
        if FileSnapshot::from_metadata(&reopened_metadata) != FileSnapshot::from_metadata(&before)
            || second != bytes
        {
            bail!("Pool migration acknowledgement changed during controller validation");
        }
        Ok(Some(bytes))
    }

    fn validate_ack(
        bytes: &[u8],
        request: &PoolMigrationLaunchRequestV3,
        request_sha256: &str,
        state: &ControllerStateV3,
        topology: &PoolTopologyV3,
    ) -> Result<()> {
        let mut actual: Value =
            serde_json::from_slice(bytes).context("parse Pool migration acknowledgement")?;
        let object = actual
            .as_object_mut()
            .context("Pool migration acknowledgement is not a JSON object")?;
        let acknowledged_at = object
            .remove("acknowledgedAtUnixSeconds")
            .context("Pool migration acknowledgement has no timestamp")?
            .as_u64()
            .context("Pool migration acknowledgement timestamp is not an unsigned integer")?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock predates Unix epoch")?
            .as_secs();
        if acknowledged_at == 0 || acknowledged_at > now.saturating_add(5) {
            bail!("Pool migration acknowledgement timestamp is invalid");
        }
        let expected = json!({
            "schema": ACK_SCHEMA,
            "status": "acknowledged",
            "requestPath": request
                .attempt_namespace
                .join(&request.nonce)
                .join(REQUEST_FILE_NAME),
            "requestSha256": request_sha256,
            "attemptNamespace": request.attempt_namespace,
            "nonce": request.nonce,
            "bootId": request.boot_id,
            "systemdInvocationId": request.systemd_invocation_id,
            "systemdUnit": request.systemd_unit,
            "systemdManager": "system",
            "systemdFragmentPath": request.systemd_fragment.path,
            "systemdFragmentSha256": request.systemd_fragment.sha256,
            "systemdEnvironmentFilePath": request.systemd_environment_file.path,
            "systemdEnvironmentFileSha256": request.systemd_environment_file.sha256,
            "pid": request.main_pid,
            "procStartTimeTicks": request.proc_start_time_ticks,
            "binaryPath": request.binary.path,
            "binarySha256": request.binary.sha256,
            "argvSha256": argv_sha256(&request.argv),
            "controllerStateSha256": request.controller.state.sha256,
            "checkpointBrokerPid": request.checkpoint_broker.pid,
            "checkpointBrokerProcStartTimeTicks": request.checkpoint_broker.proc_start_time_ticks,
            "sourceWritersFenced": state.source_writers_fenced,
            "targetWritersFenced": state.target_writers_fenced,
            "fenceHeldUntilCompletion": state.fence_held_until_completion,
            "sourceBaselineSha256": request.source.baseline.sha256,
            "poolTopologySha256": request.pool.topology.sha256,
            "poolManifestSha256": topology.manifest_sha256,
            "sourceLmdbIdentity": request.source.lmdb_identity,
            "poolLmdbIdentity": request.pool.lmdb_identity,
            "cursorValue": request.cursor.value,
            "cursorSha256": request.cursor.sha256,
            "additionalCas": request.cas.iter().map(|authority| {
                json!({
                    "label": authority.label,
                    "sha256": authority.sha256,
                })
            }).collect::<Vec<_>>(),
        });
        if actual != expected {
            bail!("durable Pool migration acknowledgement does not exactly match the request");
        }
        Ok(())
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

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn ordinary_online_checkpoint_does_not_require_final_writer_masks() {
            assert!(!checkpoint_requires_runtime_writer_masks(
                PoolMigrationControllerPhase::OnlineBounded,
                false,
            ));
            assert!(checkpoint_requires_runtime_writer_masks(
                PoolMigrationControllerPhase::OnlineBounded,
                true,
            ));
            assert!(checkpoint_requires_runtime_writer_masks(
                PoolMigrationControllerPhase::FinalStoppedSource,
                false,
            ));
            assert!(checkpoint_requires_runtime_writer_masks(
                PoolMigrationControllerPhase::FinalStoppedFull,
                false,
            ));
        }

        #[test]
        fn running_controller_hash_rejects_same_path_swap_inode() {
            let temp = tempfile::tempdir().expect("create generated controller executable root");
            let pinned_path = temp.path().join("pinned-controller");
            let swapped_path = temp.path().join("swapped-controller");
            std::fs::write(&pinned_path, b"generated controller bytes")
                .expect("write generated pinned controller");
            std::fs::write(&swapped_path, b"generated controller bytes")
                .expect("write generated swapped controller");
            let pinned_metadata =
                std::fs::metadata(&pinned_path).expect("inspect generated pinned controller");
            let expected = FileSnapshot::from_metadata(&pinned_metadata);
            let mut pinned = File::open(&pinned_path).expect("open generated pinned controller");
            let hash = hash_open_file_with_snapshot(&mut pinned, expected, "generated controller")
                .expect("exact generated controller inode hashes");
            assert_eq!(hash, sha256_bytes(b"generated controller bytes"));

            let mut swapped = File::open(&swapped_path).expect("open generated swapped controller");
            let error =
                hash_open_file_with_snapshot(&mut swapped, expected, "generated controller")
                    .expect_err("same bytes on a swapped inode must fail");
            assert!(
                error.to_string().contains("pinned authority"),
                "unexpected running controller swap error: {error:#}"
            );
        }

        fn generated_external_root() -> (tempfile::TempDir, PathBuf, FileIdentityV3) {
            let temp = tempfile::tempdir().expect("create generated census root");
            let root = temp.path().join("external");
            std::fs::create_dir(&root).expect("create generated external root");
            let root = root.canonicalize().expect("canonicalize external root");
            let metadata = std::fs::symlink_metadata(&root).expect("inspect external root");
            let identity = FileIdentityV3 {
                device: metadata.dev(),
                inode: metadata.ino(),
            };
            (temp, root, identity)
        }

        #[test]
        fn generated_external_census_is_bounded_by_live_authorities() {
            let (_temp, root, identity) = generated_external_root();
            let shard = root.join("shard");
            std::fs::create_dir(&shard).expect("create generated shard");
            for index in 0..2048_u32 {
                std::fs::write(shard.join(format!("{index:08x}")), index.to_be_bytes())
                    .expect("write generated corpus entry");
            }

            let live = HashMap::new();
            let original_capacity = live.capacity();
            validate_external_census_against_live(
                &live,
                "generated scale corpus",
                &root,
                identity,
                true,
            )
            .expect("stream generated corpus");
            assert_eq!(live.len(), 0);
            assert_eq!(live.capacity(), original_capacity);
        }

        #[test]
        fn generated_external_census_rejects_read_only_procfd_alias() {
            let (_temp, root, identity) = generated_external_root();
            let shard = root.join("shard");
            std::fs::create_dir(&shard).expect("create generated shard");
            let blob = shard.join("blob");
            std::fs::write(&blob, b"generated").expect("write generated blob");

            let root_fd = File::open(&root).expect("open generated external root");
            let alias = PathBuf::from(format!("/proc/self/fd/{}", root_fd.as_raw_fd()))
                .join("shard")
                .join("blob");
            let retained = OpenOptions::new()
                .read(true)
                .open(&alias)
                .expect("open read-only procfd alias");
            drop(root_fd);

            let reopened = OpenOptions::new()
                .write(true)
                .open(format!("/proc/self/fd/{}", retained.as_raw_fd()))
                .expect("reopen retained read-only fd as writable");
            drop(reopened);

            let mut live = HashMap::new();
            collect_process_fd_authorities(std::process::id(), &mut live)
                .expect("collect generated live fd authority");
            let retained_key =
                census_identity_key(&retained.metadata().expect("inspect retained fd"));
            assert!(live.contains_key(&retained_key));
            let error = validate_external_census_against_live(
                &live,
                "generated alias corpus",
                &root,
                identity,
                true,
            )
            .expect_err("retained read-only alias must block final census");
            assert!(error.to_string().contains("generated alias corpus"));
        }

        #[test]
        fn generated_source_census_rejects_read_only_data_and_lock_authorities() {
            let temp = tempfile::tempdir().expect("create generated source census");
            let source = temp.path().join("source");
            std::fs::create_dir(&source).expect("create generated source directory");
            let data_path = source.join("data.mdb");
            let lock_path = source.join("lock.mdb");
            std::fs::write(&data_path, b"generated data").expect("write generated data");
            std::fs::write(&lock_path, b"generated lock").expect("write generated lock");
            let identity_of = |path: &Path| {
                let metadata = std::fs::metadata(path).expect("inspect generated source authority");
                FileIdentityV3 {
                    device: metadata.dev(),
                    inode: metadata.ino(),
                }
            };
            let source_identity = LmdbIdentityV3 {
                directory: identity_of(&source),
                data: identity_of(&data_path),
                lock: identity_of(&lock_path),
            };
            let retained_data = File::open(&data_path).expect("open generated data read-only");
            let retained_lock = File::open(&lock_path).expect("open generated lock read-only");
            let mut live = HashMap::new();
            collect_process_fd_authorities(std::process::id(), &mut live)
                .expect("capture read-only generated source authorities");

            let mut claimed = HashMap::new();
            let data_error = validate_lmdb_census_against_live(
                &live,
                &mut claimed,
                "generated source LMDB",
                source_identity,
            )
            .expect_err("read-only data.mdb authority must block source-final");
            assert!(format!("{data_error:#}").contains("data.mdb"));

            live.remove(&census_identity_key(
                &retained_data.metadata().expect("inspect retained data"),
            ));
            claimed.clear();
            let lock_error = validate_lmdb_census_against_live(
                &live,
                &mut claimed,
                "generated source LMDB",
                source_identity,
            )
            .expect_err("read-only lock.mdb authority must block source-final");
            assert!(format!("{lock_error:#}").contains("lock.mdb"));
            drop(retained_lock);
        }

        #[test]
        fn generated_recovery_census_requires_controller_pins_to_be_dropped() {
            let (_temp, root, identity) = generated_external_root();
            let pin = PinnedDirectory::open_exact(&root, "generated recovery self-pin")
                .expect("pin root");
            let mut live_with_pin = HashMap::new();
            collect_process_fd_authorities(std::process::id(), &mut live_with_pin)
                .expect("capture controller self-pin");
            validate_external_census_against_live(
                &live_with_pin,
                "generated recovery self-pin corpus",
                &root,
                identity,
                false,
            )
            .expect_err("controller-retained recovery pin must appear in its own census");

            drop(pin);
            let mut live_after_drop = HashMap::new();
            collect_process_fd_authorities(std::process::id(), &mut live_after_drop)
                .expect("recapture after dropping self-pin");
            validate_external_census_against_live(
                &live_after_drop,
                "generated recovery self-pin corpus",
                &root,
                identity,
                false,
            )
            .expect("dropped controller recovery pin must not poison trailing census");
        }

        #[test]
        fn generated_external_census_rejects_ancestor_directory_authority() {
            let (temp, root, identity) = generated_external_root();
            let ancestor = File::open(temp.path()).expect("open generated corpus ancestor");
            let mut live = HashMap::new();
            record_live_process_authority(
                &mut live,
                census_identity_key(&ancestor.metadata().expect("inspect ancestor fd")),
                "generated ancestor fd".to_string(),
            );
            let error = validate_external_census_against_live(
                &live,
                "generated ancestor corpus",
                &root,
                identity,
                false,
            )
            .expect_err("ancestor directory authority must block final census");
            assert!(error.to_string().contains("generated ancestor fd"));
        }

        #[test]
        fn generated_recovery_pins_survive_namespace_replacement_only_while_retained() {
            let temp = tempfile::tempdir().expect("create generated recovery pin root");
            let labels = [
                "source-lmdb",
                "pool-catalog",
                "pool-member",
                "pool-member-external",
            ];
            let mut paths = Vec::new();
            let mut identities = Vec::new();
            let mut pins = Vec::new();
            for label in labels {
                let path = temp.path().join(label);
                std::fs::create_dir(&path).expect("create generated recovery directory");
                std::fs::write(path.join("authority"), label.as_bytes())
                    .expect("write generated recovery authority");
                let metadata =
                    std::fs::metadata(&path).expect("inspect generated recovery directory");
                let identity = FileIdentityV3 {
                    device: metadata.dev(),
                    inode: metadata.ino(),
                };
                let pin =
                    PinnedDirectory::open_exact(&path, &format!("generated {label} authority"))
                        .expect("retain generated recovery directory");
                pin.require_authority_identity(identity, &format!("generated {label} authority"))
                    .expect("match generated recovery identity");
                paths.push(path);
                identities.push(identity);
                pins.push(pin);
            }

            let runtime_paths = pins
                .iter()
                .map(PinnedDirectory::runtime_path)
                .collect::<Vec<_>>();
            for (((label, path), identity), runtime_path) in labels
                .iter()
                .zip(&paths)
                .zip(&identities)
                .zip(&runtime_paths)
            {
                let retained = temp.path().join(format!("{label}.retained"));
                std::fs::rename(path, &retained).expect("rename generated recovery authority away");
                std::fs::create_dir(path).expect("replace generated recovery namespace path");

                let replacement =
                    std::fs::metadata(path).expect("inspect replacement recovery directory");
                assert_ne!(
                    FileIdentityV3 {
                        device: replacement.dev(),
                        inode: replacement.ino(),
                    },
                    *identity
                );
                let through_procfd = std::fs::metadata(runtime_path)
                    .expect("retained procfd recovery authority remains open");
                assert_eq!(
                    FileIdentityV3 {
                        device: through_procfd.dev(),
                        inode: through_procfd.ino(),
                    },
                    *identity
                );
                assert_eq!(
                    std::fs::read(runtime_path.join("authority"))
                        .expect("read original recovery authority through procfd"),
                    label.as_bytes()
                );
            }

            drop(pins);
            for (runtime_path, identity) in runtime_paths.into_iter().zip(identities) {
                if let Ok(metadata) = std::fs::metadata(runtime_path) {
                    assert_ne!(
                        FileIdentityV3 {
                            device: metadata.dev(),
                            inode: metadata.ino(),
                        },
                        identity,
                        "recovery procfd authority survived after its retained file was dropped"
                    );
                }
            }
        }
    }
}
