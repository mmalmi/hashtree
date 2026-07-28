use anyhow::{bail, Context, Result};
use hashtree_core::from_hex;
use hashtree_lmdb::{
    LmdbSourceKeysetAudit, PinnedLmdbFileIdentity, PinnedLmdbIdentity, PoolTerminalAudit,
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

#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

const REQUEST_SCHEMA: &str = "hashtree-pool-migration-launch-request/v3";
const START_SCHEMA: &str = "hashtree-pool-migration-launch-start/v3";
const ACK_SCHEMA: &str = "hashtree-pool-migration-launch-ack/v3";
const ATTEMPT_NAMESPACE_NAME: &str = "attempts-v3";
const REQUEST_FILE_NAME: &str = "launch-request.json";
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
const POOL_TOPOLOGY_SCHEMA: &str = "hashtree-pool-migration-topology/v3";
const CONTROLLER_STATE_SCHEMA: &str = "hashtree-pool-migration-controller-state/v3";
const MEMBER_MARKER_NAME: &str = ".hashtree-pool-member-v1";
const EXTERNAL_MARKER_NAME: &str = ".hashtree-pool-external-v1";

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

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PoolMigrationLaunchRequestV3 {
    schema: String,
    attempt_namespace: PathBuf,
    attempt_namespace_identity: FileIdentityV3,
    attempt_identity: FileIdentityV3,
    nonce: String,
    boot_id: String,
    systemd_invocation_id: String,
    systemd_unit: String,
    systemd_manager: String,
    systemd_fragment: FileAuthorityV3,
    systemd_environment_file: FileAuthorityV3,
    main_pid: u32,
    proc_start_time_ticks: u64,
    binary: FileAuthorityV3,
    argv: Vec<String>,
    controller: ControllerAuthorityV3,
    source: SourceAuthorityV3,
    pool: PoolAuthorityV3,
    cursor: CursorAuthorityV3,
    cas: Vec<NamedFileAuthorityV3>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FileAuthorityV3 {
    path: PathBuf,
    sha256: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FileIdentityV3 {
    device: u64,
    inode: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LmdbIdentityV3 {
    directory: FileIdentityV3,
    data: FileIdentityV3,
    lock: FileIdentityV3,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NamedFileAuthorityV3 {
    label: String,
    path: PathBuf,
    sha256: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ControllerAuthorityV3 {
    rollout_id: String,
    phase: String,
    executable: FileAuthorityV3,
    state: FileAuthorityV3,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ControllerStateV3 {
    schema: String,
    rollout_id: String,
    phase: String,
    boot_id: String,
    source_lmdb_identity: LmdbIdentityV3,
    source_external_identity: Option<FileIdentityV3>,
    pool_lmdb_identity: LmdbIdentityV3,
    pool_manifest_sha256: String,
    pool_topology_sha256: String,
    source_writers_fenced: bool,
    target_writers_fenced: bool,
    fence_held_until_completion: bool,
    source_writer_processes_with_open_handles: u64,
    target_writer_processes_with_open_handles: u64,
    stopped_writer_units: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SourceAuthorityV3 {
    lmdb_path: PathBuf,
    lmdb_identity: LmdbIdentityV3,
    external_path: Option<PathBuf>,
    external_identity: Option<FileIdentityV3>,
    baseline: FileAuthorityV3,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PoolAuthorityV3 {
    path: PathBuf,
    lmdb_identity: LmdbIdentityV3,
    topology: FileAuthorityV3,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PoolTopologyV3 {
    schema: String,
    pool_path: PathBuf,
    manifest_sha256: String,
    members: Vec<PoolTopologyMemberV3>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PoolTopologyMemberV3 {
    id: String,
    path: PathBuf,
    directory_identity: FileIdentityV3,
    lmdb_identity: LmdbIdentityV3,
    marker: FileAuthorityV3,
    external_path: Option<PathBuf>,
    external_directory_identity: Option<FileIdentityV3>,
    external_marker: Option<FileAuthorityV3>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CursorAuthorityV3 {
    path: PathBuf,
    parent_identity: FileIdentityV3,
    exists: bool,
    value: Option<String>,
    sha256: Option<String>,
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
    source_blob_entries: u64,
    source_metadata_entries: u64,
    source_legacy_blob_only: bool,
    source_keyset_sha256: String,
    target_stored_locations: u64,
    target_stored_bytes: u64,
    target_catalog_sha256: String,
    target_payload_sha256: String,
    target_manifest_sha256: String,
}

struct ValidatedLaunch {
    cursor: Option<[u8; 32]>,
    boot_id: String,
    systemd_invocation_id: String,
    main_pid: u32,
    proc_start_time_ticks: u64,
    controller_state: ControllerStateV3,
    paths: PinnedMigrationPaths,
}

pub(super) struct AcknowledgedPoolMigrationLaunch {
    pub(super) cursor: Option<[u8; 32]>,
    pub(super) final_stopped_full_pass: bool,
    source: PathBuf,
    source_external: Option<PathBuf>,
    pool: PathBuf,
    controller_state_authority: FileAuthorityV3,
    controller_state: ControllerStateV3,
    cursor_authority: Mutex<CursorAuthorityV3>,
    attempt: PinnedDirectory,
    pins: PinnedMigrationPaths,
}

pub(super) struct AcknowledgedPoolMemberRuntimePaths {
    pub(super) id: String,
    pub(super) configured_path: PathBuf,
    pub(super) runtime_path: PathBuf,
    pub(super) configured_external_path: Option<PathBuf>,
    pub(super) runtime_external_path: Option<PathBuf>,
    pub(super) lmdb_identity: PinnedLmdbIdentity,
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

    pub(super) fn ensure_store_paths(&self) -> Result<()> {
        self.pins.ensure_path_identities()
    }

    pub(super) fn ensure_final_writer_fence(&self) -> Result<()> {
        if !self.final_stopped_full_pass {
            return Ok(());
        }
        // The root controller owns continuous start inhibition and the
        // complete /proc open-handle census. This process can only revalidate
        // the immutable attestation plus point-in-time systemd unit state.
        validate_file_authority(&self.controller_state_authority, "controller state")?;
        validate_controller_state_ownership(&self.controller_state_authority.path)?;
        validate_stopped_writer_units(&self.controller_state.stopped_writer_units)
    }

    pub(super) fn write_cursor(&self, value: &str) -> Result<()> {
        self.pins.ensure_path_identities()?;
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

    pub(super) fn write_terminal_audit_receipt(
        &self,
        source: &LmdbSourceKeysetAudit,
        target: &PoolTerminalAudit,
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
            source_blob_entries: source.blob_entries,
            source_metadata_entries: source.metadata_entries,
            source_legacy_blob_only: source.legacy_blob_only,
            source_keyset_sha256: hashtree_core::to_hex(&source.sha256),
            target_stored_locations: target.stored_locations,
            target_stored_bytes: target.stored_bytes,
            target_catalog_sha256: hashtree_core::to_hex(&target.catalog_sha256),
            target_payload_sha256: hashtree_core::to_hex(&target.payload_sha256),
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
}

struct PinnedMigrationPaths {
    source: PinnedDirectory,
    source_lmdb_files: PinnedLmdbFiles,
    source_external: Option<PinnedDirectory>,
    pool: PinnedDirectory,
    pool_lmdb_files: PinnedLmdbFiles,
    pool_manifest_sha256: [u8; 32],
    pool_members: Vec<PinnedPoolMemberPaths>,
    cursor_parent: PinnedDirectory,
    cursor_name: std::ffi::OsString,
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
    members: Vec<PinnedPoolMemberPaths>,
}

struct PinnedLmdbFiles {
    data: PinnedRegularEntry,
    lock: PinnedRegularEntry,
}

struct PinnedRegularEntry {
    file: File,
    name: std::ffi::OsString,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

struct LaunchRendezvous {
    attempt: PinnedDirectory,
    request: File,
    request_snapshot: std::fs::Metadata,
    request_path: PathBuf,
}

struct PinnedDirectory {
    file: File,
    path: PathBuf,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl PinnedDirectory {
    fn open_exact(path: &Path, label: &str) -> Result<Self> {
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

        #[cfg(unix)]
        let file = open_absolute_directory_without_symlinks(path, label)?;
        #[cfg(not(unix))]
        let file = File::open(path).with_context(|| format!("open {label} {}", path.display()))?;

        let metadata = file
            .metadata()
            .with_context(|| format!("inspect open {label} {}", path.display()))?;
        if !metadata.is_dir() {
            bail!("{label} {} is not a directory", path.display());
        }
        let pinned = Self {
            file,
            path: path.to_path_buf(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
        };
        pinned.ensure_path_identity(label)?;
        Ok(pinned)
    }

    fn ensure_path_identity(&self, label: &str) -> Result<()> {
        let current = std::fs::symlink_metadata(&self.path)
            .with_context(|| format!("reinspect {label} {}", self.path.display()))?;
        if !current.file_type().is_dir() {
            bail!("{label} {} is no longer a directory", self.path.display());
        }
        #[cfg(unix)]
        if current.dev() != self.device || current.ino() != self.inode {
            bail!("{label} path changed while the launch was being validated");
        }
        Ok(())
    }

    fn same_object(&self, other: &Self) -> bool {
        #[cfg(unix)]
        {
            self.device == other.device && self.inode == other.inode
        }
        #[cfg(not(unix))]
        {
            self.path == other.path
        }
    }

    fn authority_identity(&self) -> FileIdentityV3 {
        #[cfg(unix)]
        {
            FileIdentityV3 {
                device: self.device,
                inode: self.inode,
            }
        }
        #[cfg(not(unix))]
        {
            FileIdentityV3 {
                device: 0,
                inode: 0,
            }
        }
    }

    fn require_authority_identity(&self, expected: FileIdentityV3, label: &str) -> Result<()> {
        if self.authority_identity() != expected {
            bail!("{label} device/inode differs from controller authority");
        }
        Ok(())
    }

    fn runtime_path(&self) -> PathBuf {
        #[cfg(target_os = "linux")]
        {
            PathBuf::from(format!("/proc/self/fd/{}", self.file.as_raw_fd()))
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.path.clone()
        }
    }

    fn open_regular_optional(&self, name: &OsStr, label: &str) -> Result<Option<File>> {
        #[cfg(unix)]
        {
            let name = os_str_to_c_string(name, label)?;
            let raw = unsafe {
                libc::openat(
                    self.file.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
                )
            };
            if raw < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == ErrorKind::NotFound {
                    return Ok(None);
                }
                return Err(error)
                    .with_context(|| format!("open {label} beneath {}", self.path.display()));
            }
            let file = unsafe { File::from_raw_fd(raw) };
            if !file
                .metadata()
                .with_context(|| format!("inspect open {label}"))?
                .is_file()
            {
                bail!("{label} is not a regular file");
            }
            Ok(Some(file))
        }

        #[cfg(not(unix))]
        {
            let path = self.path.join(name);
            match OpenOptions::new().read(true).open(&path) {
                Ok(file) => {
                    if !file.metadata()?.is_file() {
                        bail!("{label} {} is not a regular file", path.display());
                    }
                    Ok(Some(file))
                }
                Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error.into()),
            }
        }
    }

    fn pin_regular(&self, name: &OsStr, label: &str) -> Result<PinnedRegularEntry> {
        let file = self
            .open_regular_optional(name, label)?
            .with_context(|| format!("{label} is absent beneath {}", self.path.display()))?;
        let metadata = file
            .metadata()
            .with_context(|| format!("inspect pinned {label}"))?;
        let entry = PinnedRegularEntry {
            file,
            name: name.to_os_string(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
        };
        entry.ensure_identity(self, label)?;
        Ok(entry)
    }

    fn entry_exists(&self, name: &OsStr, label: &str) -> Result<bool> {
        #[cfg(unix)]
        {
            let name = os_str_to_c_string(name, label)?;
            let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
            let status = unsafe {
                libc::fstatat(
                    self.file.as_raw_fd(),
                    name.as_ptr(),
                    stat.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            if status == 0 {
                return Ok(true);
            }
            let error = std::io::Error::last_os_error();
            if error.kind() == ErrorKind::NotFound {
                return Ok(false);
            }
            Err(error).with_context(|| format!("inspect {label} beneath {}", self.path.display()))
        }

        #[cfg(not(unix))]
        {
            match std::fs::symlink_metadata(self.path.join(name)) {
                Ok(_) => Ok(true),
                Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
                Err(error) => Err(error.into()),
            }
        }
    }

    fn acquire_exclusive_migration_lease(&self) -> Result<()> {
        self.ensure_path_identity("migration cursor parent")?;
        #[cfg(target_os = "linux")]
        {
            let status =
                unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if status != 0 {
                let error = std::io::Error::last_os_error();
                if error
                    .raw_os_error()
                    .is_some_and(|code| code == libc::EWOULDBLOCK || code == libc::EAGAIN)
                {
                    bail!(
                        "another Pool migration process holds the cursor-parent lease; every attempt sharing this cursor authority must serialize"
                    );
                }
                return Err(error).context("acquire Pool migration cursor-parent lease");
            }
        }
        Ok(())
    }

    fn create_durable_exclusive(&self, name: &OsStr, bytes: &[u8], label: &str) -> Result<()> {
        #[cfg(target_os = "linux")]
        let mut file = {
            let dot = os_str_to_c_string(OsStr::new("."), label)?;
            let raw = unsafe {
                libc::openat(
                    self.file.as_raw_fd(),
                    dot.as_ptr(),
                    libc::O_RDWR | libc::O_TMPFILE | libc::O_CLOEXEC,
                    0o600,
                )
            };
            if raw < 0 {
                return Err(std::io::Error::last_os_error()).with_context(|| {
                    format!("create unnamed {label} beneath {}", self.path.display())
                });
            }
            unsafe { File::from_raw_fd(raw) }
        };

        #[cfg(not(target_os = "linux"))]
        let mut file = {
            let path = self.path.join(name);
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    bail!("{label} already exists; create a fresh {ATTEMPT_NAMESPACE_NAME} nonce");
                }
                Err(error) => return Err(error.into()),
            }
        };

        file.write_all(bytes)?;
        file.sync_all()?;

        #[cfg(target_os = "linux")]
        {
            file.seek(SeekFrom::Start(0))?;
            let mut verified = Vec::with_capacity(bytes.len());
            Read::by_ref(&mut file)
                .take((bytes.len() as u64).saturating_add(1))
                .read_to_end(&mut verified)?;
            if verified != bytes {
                bail!("open unnamed {label} differs from intended acknowledgement bytes");
            }
            if unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } != 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("set unnamed {label} mode"));
            }
            let name = os_str_to_c_string(name, label)?;
            let proc_fd = std::ffi::CString::new(format!("/proc/self/fd/{}", file.as_raw_fd()))
                .expect("generated procfd path has no NUL");
            let status = unsafe {
                libc::linkat(
                    libc::AT_FDCWD,
                    proc_fd.as_ptr(),
                    self.file.as_raw_fd(),
                    name.as_ptr(),
                    libc::AT_SYMLINK_FOLLOW,
                )
            };
            if status != 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == ErrorKind::AlreadyExists {
                    bail!("{label} already exists; create a fresh {ATTEMPT_NAMESPACE_NAME} nonce");
                }
                return Err(error).with_context(|| format!("publish retained unnamed {label}"));
            }
        }

        self.file.sync_all()?;
        self.ensure_path_identity("Pool migration attempt directory")?;
        let created = file
            .metadata()
            .with_context(|| format!("reinspect created {label}"))?;
        let mut reopened = self
            .open_regular_optional(name, label)?
            .with_context(|| format!("{label} disappeared after durable creation"))?;
        let entry = reopened
            .metadata()
            .with_context(|| format!("reinspect created {label} directory entry"))?;
        ensure_same_file_snapshot(&created, &entry, label)?;
        let mut published = Vec::with_capacity(bytes.len());
        Read::by_ref(&mut reopened)
            .take((bytes.len() as u64).saturating_add(1))
            .read_to_end(&mut published)?;
        if published != bytes {
            bail!("published {label} differs from intended bytes");
        }
        self.ensure_path_identity("Pool migration attempt directory")?;
        Ok(())
    }

    fn durable_replace(&self, name: &OsStr, bytes: &[u8], label: &str) -> Result<()> {
        static TEMP_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let temporary = format!(
            ".htree-pool-migration-cursor.tmp.{}.{}",
            std::process::id(),
            sequence
        );

        #[cfg(target_os = "linux")]
        {
            let temporary_c = os_str_to_c_string(OsStr::new(&temporary), label)?;
            let target_c = os_str_to_c_string(name, label)?;
            let dot = os_str_to_c_string(OsStr::new("."), label)?;
            let raw = unsafe {
                libc::openat(
                    self.file.as_raw_fd(),
                    dot.as_ptr(),
                    libc::O_RDWR | libc::O_TMPFILE | libc::O_CLOEXEC,
                    0o600,
                )
            };
            if raw < 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("create unnamed temporary {label}"));
            }
            let mut file = unsafe { File::from_raw_fd(raw) };
            let result = (|| -> Result<()> {
                file.write_all(bytes)?;
                file.sync_all()?;
                file.seek(SeekFrom::Start(0))?;
                let mut verified = Vec::with_capacity(bytes.len());
                Read::by_ref(&mut file)
                    .take((bytes.len() as u64).saturating_add(1))
                    .read_to_end(&mut verified)?;
                if verified != bytes {
                    bail!("open unnamed {label} differs from intended bytes");
                }
                if unsafe { libc::fchmod(file.as_raw_fd(), 0o400) } != 0 {
                    return Err(std::io::Error::last_os_error())
                        .with_context(|| format!("make unnamed {label} read-only"));
                }
                let proc_fd = std::ffi::CString::new(format!("/proc/self/fd/{}", file.as_raw_fd()))
                    .expect("generated procfd path has no NUL");
                let linked = unsafe {
                    libc::linkat(
                        libc::AT_FDCWD,
                        proc_fd.as_ptr(),
                        self.file.as_raw_fd(),
                        temporary_c.as_ptr(),
                        libc::AT_SYMLINK_FOLLOW,
                    )
                };
                if linked != 0 {
                    return Err(std::io::Error::last_os_error())
                        .with_context(|| format!("publish retained unnamed {label}"));
                }
                let created = file
                    .metadata()
                    .with_context(|| format!("inspect retained unnamed {label}"))?;
                let mut linked_file = self
                    .open_regular_optional(OsStr::new(&temporary), label)?
                    .with_context(|| format!("linked temporary {label} disappeared"))?;
                let linked_metadata = linked_file
                    .metadata()
                    .with_context(|| format!("inspect linked temporary {label}"))?;
                ensure_same_file_snapshot(&created, &linked_metadata, label)?;
                let mut linked_bytes = Vec::with_capacity(bytes.len());
                Read::by_ref(&mut linked_file)
                    .take((bytes.len() as u64).saturating_add(1))
                    .read_to_end(&mut linked_bytes)?;
                if linked_bytes != bytes {
                    bail!("linked temporary {label} differs from intended bytes");
                }
                let status = unsafe {
                    libc::renameat(
                        self.file.as_raw_fd(),
                        temporary_c.as_ptr(),
                        self.file.as_raw_fd(),
                        target_c.as_ptr(),
                    )
                };
                if status != 0 {
                    return Err(std::io::Error::last_os_error())
                        .with_context(|| format!("atomically replace {label}"));
                }
                let mut published = self
                    .open_regular_optional(name, label)?
                    .with_context(|| format!("published {label} disappeared"))?;
                let published_metadata = published
                    .metadata()
                    .with_context(|| format!("inspect published {label}"))?;
                ensure_same_file_snapshot(&created, &published_metadata, label)?;
                let mut published_bytes = Vec::with_capacity(bytes.len());
                Read::by_ref(&mut published)
                    .take((bytes.len() as u64).saturating_add(1))
                    .read_to_end(&mut published_bytes)?;
                if published_bytes != bytes {
                    bail!("published {label} differs from intended bytes");
                }
                self.file.sync_all()?;
                Ok(())
            })();
            if result.is_err() {
                unsafe {
                    libc::unlinkat(self.file.as_raw_fd(), temporary_c.as_ptr(), 0);
                }
            }
            return result;
        }

        #[cfg(not(target_os = "linux"))]
        {
            let _ = label;
            let temporary = self.path.join(temporary);
            let target = self.path.join(name);
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            let result = (|| -> Result<()> {
                file.write_all(bytes)?;
                file.sync_all()?;
                std::fs::rename(&temporary, &target)?;
                self.file.sync_all()?;
                Ok(())
            })();
            if result.is_err() {
                let _ = std::fs::remove_file(&temporary);
            }
            result
        }
    }
}

impl PinnedRegularEntry {
    fn same_object(&self, other: &Self) -> bool {
        #[cfg(unix)]
        {
            self.device == other.device && self.inode == other.inode && self.name == other.name
        }
        #[cfg(not(unix))]
        {
            self.name == other.name
        }
    }

    fn ensure_identity(&self, directory: &PinnedDirectory, label: &str) -> Result<()> {
        let opened = self
            .file
            .metadata()
            .with_context(|| format!("reinspect pinned {label}"))?;
        #[cfg(unix)]
        if opened.dev() != self.device || opened.ino() != self.inode {
            bail!("{label} open file identity changed");
        }
        let current = directory
            .open_regular_optional(&self.name, label)?
            .with_context(|| format!("{label} disappeared after it was pinned"))?;
        let current = current
            .metadata()
            .with_context(|| format!("reinspect {label} directory entry"))?;
        #[cfg(unix)]
        if current.dev() != self.device || current.ino() != self.inode {
            bail!("{label} directory entry changed after it was pinned");
        }
        #[cfg(not(unix))]
        if !current.is_file() {
            bail!("{label} is no longer a regular file");
        }
        Ok(())
    }
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
            && self.pool_members.len() == other.pool_members.len()
            && self
                .pool_members
                .iter()
                .zip(&other.pool_members)
                .all(|(left, right)| left.same_objects(right))
            && self.cursor_parent.same_object(&other.cursor_parent)
            && self.cursor_name == other.cursor_name
    }

    fn ensure_path_identities(&self) -> Result<()> {
        self.source.ensure_path_identity("source LMDB")?;
        self.source_lmdb_files
            .ensure_identities(&self.source, "source LMDB")?;
        if let Some(external) = &self.source_external {
            external.ensure_path_identity("source external directory")?;
        }
        self.pool.ensure_path_identity("target Pool")?;
        self.pool_lmdb_files
            .ensure_identities(&self.pool, "target Pool catalog")?;
        for member in &self.pool_members {
            member.ensure_path_identities_and_markers()?;
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

#[cfg(unix)]
fn open_absolute_directory_without_symlinks(path: &Path, label: &str) -> Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW);
    let mut directory = options
        .open(Path::new("/"))
        .with_context(|| format!("open filesystem root while resolving {label}"))?;
    for component in path.components() {
        match component {
            std::path::Component::RootDir => {}
            std::path::Component::Normal(name) => {
                let name = os_str_to_c_string(name, label)?;
                let raw = unsafe {
                    libc::openat(
                        directory.as_raw_fd(),
                        name.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                    )
                };
                if raw < 0 {
                    return Err(std::io::Error::last_os_error()).with_context(|| {
                        format!("open trusted {label} component {}", path.display())
                    });
                }
                directory = unsafe { File::from_raw_fd(raw) };
            }
            std::path::Component::CurDir
            | std::path::Component::ParentDir
            | std::path::Component::Prefix(_) => {
                bail!("{label} path contains a non-canonical component");
            }
        }
    }
    Ok(directory)
}

#[cfg(unix)]
fn os_str_to_c_string(value: &OsStr, label: &str) -> Result<std::ffi::CString> {
    std::ffi::CString::new(value.as_bytes())
        .with_context(|| format!("{label} path component contains NUL"))
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

    println!("Pool migration launch acknowledged: {}", ack_path.display());
    let source = second.paths.source_runtime_path();
    let source_external = second.paths.source_external_runtime_path();
    let pool = second.paths.pool_runtime_path();
    Ok(AcknowledgedPoolMigrationLaunch {
        cursor: second.cursor,
        final_stopped_full_pass: request.controller.phase == "final-stopped-full",
        source,
        source_external,
        pool,
        controller_state_authority: request.controller.state.clone(),
        controller_state: second.controller_state,
        cursor_authority: Mutex::new(request.cursor.clone()),
        attempt,
        pins: second.paths,
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
    match request.controller.phase.as_str() {
        "online-bounded" => {
            if context.max_items.is_none() {
                bail!("online-bounded Pool migration launch requires --max-items");
            }
        }
        "final-stopped-full" => {
            if context.max_items.is_some() {
                bail!("final-stopped-full Pool migration launch forbids --max-items");
            }
            if request.cursor.exists {
                bail!(
                    "final-stopped-full Pool migration launch requires a fresh absent cursor and full rescan"
                );
            }
        }
        phase => bail!(
            "unsupported Pool migration controller phase {phase}; expected online-bounded or final-stopped-full"
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
    let paths = PinnedMigrationPaths {
        source,
        source_lmdb_files,
        source_external,
        pool,
        pool_lmdb_files,
        pool_manifest_sha256: pool_topology.manifest_sha256,
        pool_members: pool_topology.members,
        cursor_parent,
        cursor_name,
    };
    let controller_state = validate_controller_state(request, &paths, &boot_id)?;
    paths.ensure_isolated_authority_roots(request, context.launch_request)?;
    Ok(ValidatedLaunch {
        cursor,
        boot_id,
        systemd_invocation_id: invocation_id,
        main_pid,
        proc_start_time_ticks,
        controller_state,
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
    if request.controller.phase == "final-stopped-full"
        && (!state.source_writers_fenced
            || !state.target_writers_fenced
            || !state.fence_held_until_completion
            || state.source_writer_processes_with_open_handles != 0
            || state.target_writer_processes_with_open_handles != 0
            || state.stopped_writer_units.is_empty())
    {
        bail!(
            "final-stopped-full controller state must attest source and target writer fences held through completion, zero writer processes holding store handles, and the exact stopped systemd writer units"
        );
    }
    if request.controller.phase == "final-stopped-full" {
        validate_stopped_writer_units(&state.stopped_writer_units)?;
    }
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
fn validate_stopped_writer_property_map(
    unit: &str,
    properties: &HashMap<&str, &str>,
) -> Result<()> {
    if properties.get("LoadState").copied() != Some("loaded")
        || properties.get("ActiveState").copied() != Some("inactive")
        || properties.get("SubState").copied() != Some("dead")
        || properties.get("MainPID").copied() != Some("0")
        || properties.get("ControlPID").copied() != Some("0")
        || properties.get("Job").copied() != Some("")
    {
        bail!("writer unit {unit} is not loaded, inactive/dead, process-free, and job-free");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_stopped_writer_units(units: &[String]) -> Result<()> {
    let systemctl = trusted_systemctl_path()?;
    for unit in units {
        let output = std::process::Command::new(systemctl)
            .env_clear()
            .env("LANG", "C")
            .args([
                "--system",
                "--no-pager",
                "show",
                unit,
                "--property",
                "LoadState",
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
            ])
            .output()
            .with_context(|| format!("inspect stopped writer unit {unit}"))?;
        if !output.status.success() {
            bail!(
                "systemctl could not verify stopped writer unit {unit}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let stdout = String::from_utf8(output.stdout)
            .with_context(|| format!("decode stopped writer unit {unit} properties"))?;
        let properties =
            parse_systemd_properties(&stdout, &format!("stopped writer unit {unit} output"))?;
        validate_stopped_writer_property_map(unit, &properties)?;
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn validate_stopped_writer_units(_units: &[String]) -> Result<()> {
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
    Ok(attempt)
}

#[cfg(target_os = "linux")]
fn validate_attempt_namespace_ownership(namespace: &Path, attempt: &PinnedDirectory) -> Result<()> {
    let namespace = std::fs::symlink_metadata(namespace)
        .context("inspect Pool migration v3 attempt namespace")?;
    if !namespace.file_type().is_dir() || namespace.uid() != 0 || namespace.mode() & 0o022 != 0 {
        bail!("Pool migration attempts-v3 namespace must be a root-owned non-writable directory");
    }
    let metadata = attempt
        .file
        .metadata()
        .context("inspect Pool migration attempt directory ownership")?;
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
    if value == "complete" {
        bail!("a complete migration cursor is terminal and must never be launched");
    }
    require_lower_hex("migration cursor", value, 64)?;
    let _: [u8; 32] = from_hex(value).context("decode migration cursor")?;
    Ok(())
}

fn validate_cursor_write_value(value: &str) -> Result<()> {
    if value == "complete" {
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
    if authority.value.as_deref() == Some("complete") {
        bail!("a complete migration cursor is terminal and cannot be overwritten");
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
        members: pinned,
    })
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
        || !value.starts_with("hashtree-pool-migrate@")
        || !value.ends_with(".service")
        || value == "hashtree-pool-migrate@.service"
        || value.contains('/')
        || value == "."
        || value == ".."
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'_' | b'.' | b'@' | b'\\' | b'-')
        })
    {
        bail!("request systemd unit must be an exact bounded .service unit name");
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
        != Some("hashtree-pool-migrate@.service")
    {
        bail!("systemd unit fragment must be named hashtree-pool-migrate@.service");
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
        let stopped = parse_systemd_properties(
            "LoadState=loaded\nActiveState=inactive\nSubState=dead\nMainPID=0\nControlPID=0\nJob=\n",
            "test stopped writer",
        )
        .expect("parse complete stopped writer properties");
        validate_stopped_writer_property_map("writer.service", &stopped)
            .expect("complete stopped writer properties");

        let missing_job = parse_systemd_properties(
            "LoadState=loaded\nActiveState=inactive\nSubState=dead\nMainPID=0\nControlPID=0\n",
            "test stopped writer",
        )
        .expect("parse missing-Job stopped writer properties");
        let error = validate_stopped_writer_property_map("writer.service", &missing_job)
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
