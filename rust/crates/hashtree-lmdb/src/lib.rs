//! LMDB-backed content-addressed blob storage.

mod configured;
mod managed_env;
mod migration;
mod migration_audit;
mod pool;

pub use configured::{
    open_configured_lmdb_blob_store, open_shared_lmdb_blob_store, pool_audit_read_only_enabled,
    ConfiguredLmdbBlobStore, LOCAL_ADD_EXTERNAL_BLOB_DIR_NAME, POOL_AUDIT_READ_ONLY_ENV,
    POOL_AUDIT_READ_ONLY_ERROR, SHARED_BLOB_MIN_MAP_SIZE_BYTES, SHARED_BLOB_POOL_DIR_NAME,
};
pub use heed::{PinnedLmdbFileIdentity, PinnedLmdbIdentity};
pub use migration::{
    audit_lmdb_source_batch_with_max_buffer_bytes, migrate_lmdb_batch,
    migrate_lmdb_batch_with_max_buffer_bytes,
    migrate_lmdb_batch_with_max_buffer_bytes_and_authorizer,
    migrate_lmdb_hashes_with_max_buffer_bytes_and_authorizer,
    reconcile_lmdb_source_batch_with_max_buffer_bytes_and_authorizer,
    reconcile_lmdb_source_entries_with_max_buffer_bytes_and_authorizer,
    reconcile_lmdb_source_hashes_with_max_buffer_bytes_and_authorizer,
    reconcile_lmdb_source_union_page_with_max_buffer_bytes_and_authorizer, LmdbSourceAuditBatch,
    PoolMigrationBatch, DEFAULT_POOL_MIGRATION_MAX_BUFFER_BYTES,
};
pub use migration_audit::{PoolMigrationAuditStore, PoolMigrationAuditSummary};
pub use pool::{
    PoolCatalogLocation, PoolMaintenanceReport, PoolManifestIdentity, PoolMemberConfig,
    PoolMemberId, PoolMemberRuntimePaths, PoolMemberState, PoolMemberStatus, PoolPhysicalAudit,
    PoolReadBatchItem, PoolStalePending, PoolStalePendingCleanupReport, PoolStore, PoolStoreConfig,
    PoolStoreReader, PoolTemperatureConfig, PoolTemperatureReport, PoolTerminalAudit,
    ReadOnlyPoolCatalogAudit, ReadOnlyPoolManifestMember, ReadOnlyPoolManifestSnapshot,
    ReadOnlyPoolStore,
};

use async_trait::async_trait;
use hashtree_core::store::{PutManyReport, Store, StoreError, StoreStats};
use hashtree_core::{to_hex, types::Hash};
use heed::types::*;
use heed::{Database, EnvFlags, EnvOpenOptions, Error as HeedError, MdbError, PutFlags};
use managed_env::ManagedEnv;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
#[cfg(unix)]
use std::ffi::{CString, OsStr};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

// Re-export sha256 for convenience
pub use hashtree_core::hash::sha256 as compute_sha256;

#[cfg(target_pointer_width = "64")]
const DEFAULT_MAP_SIZE: usize = 10 * 1024 * 1024 * 1024;
#[cfg(target_pointer_width = "32")]
const DEFAULT_MAP_SIZE: usize = 1024 * 1024 * 1024;
const DEFAULT_MAX_READERS: u32 = 1024;
const DATABASE_COUNT: u32 = 5;
const REOPEN_HEADROOM_BYTES: u64 = 64 * 1024 * 1024;
const EVICTION_BATCH_TARGET_BYTES: u64 = 256 * 1024 * 1024;
const EVICTION_BATCH_MAX_ITEMS: usize = 4096;
const LEGACY_BLOB_META_BYTES: usize = 16;
const BLOB_META_BYTES: usize = 24;
const ORDER_KEY_BYTES: usize = 40;
const PIN_COUNT_BYTES: usize = 4;
const STORE_TOTALS_BYTES: usize = 32;
const STORE_TOTALS_KEY: &[u8] = b"totals";
const NEXT_ORDER_BYTES: usize = 8;
const NEXT_ORDER_KEY: &[u8] = b"next_order";
const NEXT_ORDER_MIGRATION_FLOOR: u64 = 1 << 48;
const EXTERNAL_BLOB_MARKER_PREFIX: &[u8] = b"\0hashtree-lmdb-external-blob-v1\0";
const EXTERNAL_PACK_MARKER_PREFIX: &[u8] = b"\0hashtree-lmdb-external-pack-v1\0";
const EXTERNAL_PACK_RESERVED_MARKER_PREFIX: &[u8] = b"\0hashtree-lmdb-external-pack-reserved-v1\0";
const LMDB_NO_READ_AHEAD_ENV: &str = "HTREE_LMDB_NO_READ_AHEAD";
const LMDB_NO_SYNC_ENV: &str = "HTREE_LMDB_NO_SYNC";
const LMDB_NO_META_SYNC_ENV: &str = "HTREE_LMDB_NO_META_SYNC";
const LMDB_EXTERNAL_BLOB_MIN_BYTES_ENV: &str = "HTREE_LMDB_EXTERNAL_BLOB_MIN_BYTES";
const LMDB_EXTERNAL_BLOB_DIR_ENV: &str = "HTREE_LMDB_EXTERNAL_BLOB_DIR";
const LMDB_EXTERNAL_BLOB_SYNC_ENV: &str = "HTREE_LMDB_EXTERNAL_BLOB_SYNC";
const LMDB_EXTERNAL_BLOB_PACK_TARGET_BYTES_ENV: &str = "HTREE_LMDB_EXTERNAL_BLOB_PACK_TARGET_BYTES";
static EXTERNAL_PACK_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy)]
struct BlobMeta {
    order: u64,
    size: u64,
    last_accessed_at: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct StoreTotals {
    count: u64,
    total_bytes: u64,
    pinned_count: u64,
    pinned_bytes: u64,
}

#[derive(Debug, Clone)]
struct ExternalBlobConfig {
    base_path: PathBuf,
    min_bytes: usize,
    sync: bool,
    pack_target_bytes: Option<usize>,
    pinned_root: Option<Arc<PinnedExternalRoot>>,
}

#[derive(Debug)]
struct PinnedExternalRoot {
    directory: File,
}

/// Options for storing larger LMDB blobs in normal files.
#[derive(Debug, Clone)]
pub struct ExternalBlobOptions {
    pub base_path: PathBuf,
    pub min_bytes: usize,
    pub sync: bool,
    pub pack_target_bytes: Option<usize>,
}

impl ExternalBlobOptions {
    /// Build external-blob options from the standard hashtree LMDB environment.
    pub fn from_env(env_path: &Path) -> Option<Self> {
        ExternalBlobConfig::from_env(env_path).map(Into::into)
    }

    /// Override only the base directory, keeping the remaining env-derived knobs.
    pub fn with_base_path(mut self, base_path: PathBuf) -> Self {
        self.base_path = base_path;
        self
    }
}

impl ExternalBlobConfig {
    fn from_env(env_path: &Path) -> Option<Self> {
        let min_bytes = std::env::var(LMDB_EXTERNAL_BLOB_MIN_BYTES_ENV)
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)?;
        let base_path = std::env::var(LMDB_EXTERNAL_BLOB_DIR_ENV)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| env_path.join("external-blobs"));
        let sync = env_bool(LMDB_EXTERNAL_BLOB_SYNC_ENV).unwrap_or(true);
        let pack_target_bytes = std::env::var(LMDB_EXTERNAL_BLOB_PACK_TARGET_BYTES_ENV)
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0);
        Some(Self {
            base_path,
            min_bytes,
            sync,
            pack_target_bytes,
            pinned_root: None,
        })
    }
}

impl From<ExternalBlobConfig> for ExternalBlobOptions {
    fn from(config: ExternalBlobConfig) -> Self {
        Self {
            base_path: config.base_path,
            min_bytes: config.min_bytes,
            sync: config.sync,
            pack_target_bytes: config.pack_target_bytes,
        }
    }
}

impl From<ExternalBlobOptions> for ExternalBlobConfig {
    fn from(options: ExternalBlobOptions) -> Self {
        Self {
            base_path: options.base_path,
            min_bytes: options.min_bytes,
            sync: options.sync,
            pack_target_bytes: options.pack_target_bytes,
            pinned_root: None,
        }
    }
}

impl ExternalBlobConfig {
    fn prepare_runtime(self) -> Result<Self, StoreError> {
        #[cfg(target_os = "linux")]
        {
            let mut prepared = self;
            if is_linux_proc_fd_directory(&prepared.base_path) {
                prepared.pinned_root =
                    Some(Arc::new(PinnedExternalRoot::open(&prepared.base_path)?));
            }
            return Ok(prepared);
        }
        #[cfg(not(target_os = "linux"))]
        Ok(self)
    }

    fn relative_blob_path(hash: &Hash) -> PathBuf {
        let hex = to_hex(hash);
        PathBuf::from(&hex[..2]).join(&hex[2..4]).join(&hex[4..])
    }

    fn relative_pack_path(pack_name: &str) -> PathBuf {
        PathBuf::from("packs").join(&pack_name[..2]).join(pack_name)
    }

    fn open_relative(&self, relative: &Path) -> Result<File, StoreError> {
        if let Some(root) = &self.pinned_root {
            return root.open_regular(relative);
        }
        File::open(self.base_path.join(relative)).map_err(StoreError::Io)
    }
}

#[cfg(target_os = "linux")]
fn is_linux_proc_fd_directory(path: &Path) -> bool {
    let mut components = path.components();
    components.next() == Some(std::path::Component::RootDir)
        && ["proc", "self", "fd"].into_iter().all(|expected| {
            components
                .next()
                .and_then(|component| component.as_os_str().to_str())
                == Some(expected)
        })
        && components
            .next()
            .and_then(|component| component.as_os_str().to_str())
            .is_some_and(|descriptor| {
                !descriptor.is_empty() && descriptor.bytes().all(|byte| byte.is_ascii_digit())
            })
        && components.next().is_none()
}

#[cfg(target_os = "linux")]
fn pinned_lmdb_data_len(path: &Path, identity: PinnedLmdbIdentity) -> Result<u64, StoreError> {
    if !is_linux_proc_fd_directory(path) {
        return Err(StoreError::Other(
            "pinned LMDB sizing requires an exact /proc/self/fd/N directory".into(),
        ));
    }
    let directory = File::open(path).map_err(StoreError::Io)?;
    if !directory.metadata().map_err(StoreError::Io)?.is_dir() {
        return Err(StoreError::Other(
            "pinned LMDB sizing root is not a directory".into(),
        ));
    }
    let name = CString::new("data.mdb").expect("LMDB file name has no NUL");
    let raw = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if raw < 0 {
        return Err(StoreError::Io(std::io::Error::last_os_error()));
    }
    let data = unsafe { File::from_raw_fd(raw) };
    let metadata = data.metadata().map_err(StoreError::Io)?;
    use std::os::unix::fs::MetadataExt;
    if !metadata.is_file()
        || metadata.dev() != identity.data.device
        || metadata.ino() != identity.data.inode
    {
        return Err(StoreError::Other(
            "pinned LMDB data.mdb identity changed before map sizing".into(),
        ));
    }
    Ok(metadata.len())
}

#[cfg(not(target_os = "linux"))]
fn pinned_lmdb_data_len(_path: &Path, _identity: PinnedLmdbIdentity) -> Result<u64, StoreError> {
    Err(StoreError::Other(
        "pinned LMDB sizing is supported only on Linux".into(),
    ))
}

#[cfg(unix)]
impl PinnedExternalRoot {
    #[cfg(target_os = "linux")]
    fn open(path: &Path) -> Result<Self, StoreError> {
        let directory = File::open(path).map_err(StoreError::Io)?;
        if !directory.metadata().map_err(StoreError::Io)?.is_dir() {
            return Err(StoreError::Other(
                "pinned external blob root is not a directory".into(),
            ));
        }
        Ok(Self { directory })
    }

    fn c_name(name: &OsStr) -> Result<CString, StoreError> {
        CString::new(name.as_bytes())
            .map_err(|_| StoreError::Other("external blob path component contains NUL".into()))
    }

    fn components(relative: &Path) -> Result<Vec<&OsStr>, StoreError> {
        if relative.is_absolute() {
            return Err(StoreError::Other(
                "external blob path must be relative to its pinned root".into(),
            ));
        }
        let mut names = Vec::new();
        for component in relative.components() {
            match component {
                std::path::Component::Normal(name) => names.push(name),
                _ => {
                    return Err(StoreError::Other(
                        "external blob path contains an unsafe component".into(),
                    ))
                }
            }
        }
        if names.is_empty() {
            return Err(StoreError::Other("external blob path is empty".into()));
        }
        Ok(names)
    }

    fn open_parent(
        &self,
        relative: &Path,
        create: bool,
        durable: bool,
    ) -> Result<(File, CString), StoreError> {
        let components = Self::components(relative)?;
        let (leaf, directories) = components
            .split_last()
            .expect("non-empty external path checked above");
        let mut current = self.directory.try_clone().map_err(StoreError::Io)?;
        for component in directories {
            let name = Self::c_name(component)?;
            let open = || unsafe {
                libc::openat(
                    current.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                )
            };
            let mut raw = open();
            if raw < 0
                && create
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound
            {
                let status = unsafe { libc::mkdirat(current.as_raw_fd(), name.as_ptr(), 0o700) };
                if status != 0
                    && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists
                {
                    return Err(StoreError::Io(std::io::Error::last_os_error()));
                }
                if durable {
                    current.sync_all().map_err(StoreError::Io)?;
                }
                raw = open();
            }
            if raw < 0 {
                return Err(StoreError::Io(std::io::Error::last_os_error()));
            }
            let next = unsafe { File::from_raw_fd(raw) };
            if !next.metadata().map_err(StoreError::Io)?.is_dir() {
                return Err(StoreError::Other(
                    "external blob path traverses a non-directory".into(),
                ));
            }
            if durable {
                current.sync_all().map_err(StoreError::Io)?;
            }
            current = next;
        }
        Ok((current, Self::c_name(leaf)?))
    }

    fn open_regular(&self, relative: &Path) -> Result<File, StoreError> {
        let (parent, leaf) = self.open_parent(relative, false, false)?;
        let raw = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                leaf.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            )
        };
        if raw < 0 {
            return Err(StoreError::Io(std::io::Error::last_os_error()));
        }
        let file = unsafe { File::from_raw_fd(raw) };
        if !file.metadata().map_err(StoreError::Io)?.is_file() {
            return Err(StoreError::Other(
                "external blob entry is not a regular file".into(),
            ));
        }
        Ok(file)
    }

    fn open_regular_optional(parent: &File, leaf: &CString) -> Result<Option<File>, StoreError> {
        let raw = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                leaf.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            )
        };
        if raw < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(StoreError::Io(error));
        }
        let file = unsafe { File::from_raw_fd(raw) };
        if !file.metadata().map_err(StoreError::Io)?.is_file() {
            return Err(StoreError::Other(
                "external blob entry is not a regular file".into(),
            ));
        }
        Ok(Some(file))
    }

    fn verify_existing_blob(
        parent: &File,
        leaf: &CString,
        expected_hash: &Hash,
        expected_len: usize,
        sync: bool,
    ) -> Result<bool, StoreError> {
        let Some(mut file) = Self::open_regular_optional(parent, leaf)? else {
            return Ok(false);
        };
        if file.metadata().map_err(StoreError::Io)?.len() != expected_len as u64 {
            return Err(StoreError::Other(
                "existing pinned external blob has the wrong length".into(),
            ));
        }
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; 1024 * 1024];
        let read_limit = u64::try_from(expected_len)
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        let mut limited = Read::by_ref(&mut file).take(read_limit);
        let mut total = 0usize;
        loop {
            let read = limited.read(&mut buffer).map_err(StoreError::Io)?;
            if read == 0 {
                break;
            }
            total = total.saturating_add(read);
            hasher.update(&buffer[..read]);
        }
        if total != expected_len
            || file.metadata().map_err(StoreError::Io)?.len() != expected_len as u64
            || hasher.finalize().as_slice() != expected_hash
        {
            return Err(StoreError::Other(
                "existing pinned external blob has the wrong hash".into(),
            ));
        }
        if sync {
            file.sync_all().map_err(StoreError::Io)?;
            parent.sync_all().map_err(StoreError::Io)?;
        }
        Ok(true)
    }

    fn write_blob(
        &self,
        relative: &Path,
        data: &[u8],
        expected_hash: &Hash,
        sync: bool,
    ) -> Result<(), StoreError> {
        let (parent, leaf) = self.open_parent(relative, true, sync)?;
        if Self::verify_existing_blob(&parent, &leaf, expected_hash, data.len(), sync)? {
            return Ok(());
        }
        let mut file = Self::create_unnamed_file(&parent)?;
        file.write_all(data).map_err(StoreError::Io)?;
        if sync {
            file.sync_all().map_err(StoreError::Io)?;
        }
        let linked = match Self::link_open_file(&file, &parent, &leaf) {
            Ok(()) => true,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
            Err(error) => return Err(StoreError::Io(error)),
        };
        let _ = linked;
        Self::verify_existing_blob(&parent, &leaf, expected_hash, data.len(), sync)?;
        if sync {
            parent.sync_all().map_err(StoreError::Io)?;
        }
        Ok(())
    }

    fn create_file<T>(
        &self,
        relative: &Path,
        sync: bool,
        write: impl FnOnce(&mut File) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let (parent, leaf) = self.open_parent(relative, true, sync)?;
        if Self::open_regular_optional(&parent, &leaf)?.is_some() {
            return Err(StoreError::Other(
                "generated external pack already exists".into(),
            ));
        }
        let mut file = Self::create_unnamed_file(&parent)?;
        let value = write(&mut file)?;
        if sync {
            file.sync_all().map_err(StoreError::Io)?;
        }
        Self::link_open_file(&file, &parent, &leaf).map_err(StoreError::Io)?;
        if sync {
            parent.sync_all().map_err(StoreError::Io)?;
        }
        Ok(value)
    }

    fn remove_file(&self, relative: &Path, sync: bool) -> Result<(), StoreError> {
        let (parent, leaf) = self.open_parent(relative, false, false)?;
        if unsafe { libc::unlinkat(parent.as_raw_fd(), leaf.as_ptr(), 0) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(StoreError::Io(error));
            }
        }
        if sync {
            parent.sync_all().map_err(StoreError::Io)?;
        }
        Ok(())
    }

    fn link_open_file(file: &File, parent: &File, leaf: &CString) -> Result<(), std::io::Error> {
        let proc_fd = CString::new(format!("/proc/self/fd/{}", file.as_raw_fd()))
            .expect("generated procfd path has no NUL");
        let status = unsafe {
            libc::linkat(
                libc::AT_FDCWD,
                proc_fd.as_ptr(),
                parent.as_raw_fd(),
                leaf.as_ptr(),
                libc::AT_SYMLINK_FOLLOW,
            )
        };
        if status != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let linked = Self::open_regular_optional(parent, leaf)
            .map_err(|error| std::io::Error::other(error.to_string()))?
            .ok_or_else(|| std::io::Error::other("linked external file disappeared"))?;
        let source = file.metadata()?;
        let target = linked.metadata()?;
        use std::os::unix::fs::MetadataExt;
        if source.dev() != target.dev() || source.ino() != target.ino() {
            return Err(std::io::Error::other(
                "linked external file identity differs from open writer",
            ));
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn create_unnamed_file(parent: &File) -> Result<File, StoreError> {
        let dot = CString::new(".").expect("dot has no NUL");
        let raw = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                dot.as_ptr(),
                libc::O_RDWR | libc::O_TMPFILE | libc::O_CLOEXEC,
                0o600,
            )
        };
        if raw < 0 {
            return Err(StoreError::Io(std::io::Error::last_os_error()));
        }
        Ok(unsafe { File::from_raw_fd(raw) })
    }

    #[cfg(not(target_os = "linux"))]
    fn create_unnamed_file(_parent: &File) -> Result<File, StoreError> {
        Err(StoreError::Other(
            "pinned external publication is supported only on Linux".into(),
        ))
    }
}

#[derive(Debug, Clone)]
struct ExternalPackRef {
    name: String,
    offset: u64,
    len: u64,
}

#[derive(Debug)]
enum SourceBlobRead {
    ExternalBlob {
        file: ExternalSourceFile,
        expected_size: u64,
    },
    ExternalPack {
        file: ExternalSourceFile,
        offset: u64,
        len: u64,
        expected_size: u64,
    },
}

impl SourceBlobRead {
    fn physical_path(&self) -> &Path {
        match self {
            Self::ExternalBlob { file, .. } | Self::ExternalPack { file, .. } => &file.path,
        }
    }

    fn physical_offset(&self) -> u64 {
        match self {
            Self::ExternalBlob { .. } => 0,
            Self::ExternalPack { offset, .. } => *offset,
        }
    }
}

#[derive(Debug, Clone)]
struct ExternalSourceFile {
    path: PathBuf,
    relative: PathBuf,
    pinned_root: Option<Arc<PinnedExternalRoot>>,
}

impl ExternalSourceFile {
    fn new(config: &ExternalBlobConfig, relative: PathBuf) -> Self {
        Self {
            path: config.base_path.join(&relative),
            relative,
            pinned_root: config.pinned_root.clone(),
        }
    }

    fn open(&self) -> Result<File, StoreError> {
        if let Some(root) = &self.pinned_root {
            root.open_regular(&self.relative)
        } else {
            File::open(&self.path).map_err(StoreError::Io)
        }
    }
}

#[derive(Debug)]
struct SourceBlobReadPlan {
    index: usize,
    hash: Hash,
    read: SourceBlobRead,
}

/// LMDB-backed blob store implementing hashtree's Store trait.
pub struct LmdbBlobStore {
    env: ManagedEnv,
    /// Maps SHA256 hash (32 bytes) → blob data
    blobs: Database<Bytes, Bytes>,
    /// Maps SHA256 hash (32 bytes) → [order: u64][size: u64]
    metadata: Database<Bytes, Bytes>,
    /// Maps [order: u64][hash: 32 bytes] → ()
    eviction_order: Database<Bytes, Unit>,
    /// Maps SHA256 hash (32 bytes) → pin count (u32)
    pins: Database<Bytes, Bytes>,
    /// Small aggregate counters used by quota checks and stats.
    stats: Database<Bytes, Bytes>,
    max_bytes: AtomicU64,
    next_order: AtomicU64,
    external_blobs: Option<ExternalBlobConfig>,
}

/// Read-only view of an existing LMDB blob store for online migration and verification.
///
/// It adopts the map size published in the LMDB environment instead of requesting
/// a resize, and intentionally exposes no mutation methods.
pub struct LmdbBlobReader {
    store: LmdbBlobStore,
    external_read_concurrency: usize,
}

/// Exhaustive key-set proof for a terminal LMDB migration source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LmdbSourceKeysetAudit {
    pub blob_entries: u64,
    pub metadata_entries: u64,
    pub blob_only_entries: u64,
    pub legacy_blob_only: bool,
    pub inline_entries: u64,
    pub loose_external_entries: u64,
    pub packed_external_entries: u64,
    pub sha256: Hash,
    pub catalog_location_sha256: Hash,
}

/// Exhaustive proof of the ordered raw blob keys in a frozen migration source.
///
/// This deliberately excludes blob values, metadata, and storage locations.
/// Once the same content-addressed keys have exact `Stored` Pool catalog rows
/// and the Pool bodies are independently SHA-256 audited, touching legacy
/// source values adds I/O but no preservation evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LmdbSourceKeyAudit {
    pub blob_entries: u64,
    pub sha256: Hash,
}

/// Streaming builder for [`LmdbSourceKeyAudit`].
pub struct LmdbSourceKeyAuditBuilder {
    hasher: Sha256,
    blob_entries: u64,
    previous: Option<Hash>,
}

impl LmdbSourceKeyAuditBuilder {
    pub fn new() -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"hashtree-lmdb-terminal-source-keys/v1\0");
        Self {
            hasher,
            blob_entries: 0,
            previous: None,
        }
    }

    pub fn append_sorted(&mut self, hashes: &[Hash]) -> Result<(), StoreError> {
        if hashes.windows(2).any(|pair| pair[0] >= pair[1])
            || self
                .previous
                .zip(hashes.first().copied())
                .is_some_and(|(previous, first)| previous >= first)
        {
            return Err(StoreError::Other(
                "terminal source keys must be globally unique and strictly sorted".into(),
            ));
        }
        for hash in hashes {
            self.hasher.update(hash);
        }
        self.blob_entries = self
            .blob_entries
            .checked_add(hashes.len() as u64)
            .ok_or_else(|| StoreError::Other("terminal source key count overflow".into()))?;
        self.previous = hashes.last().copied().or(self.previous);
        Ok(())
    }

    pub fn blob_entries(&self) -> u64 {
        self.blob_entries
    }

    pub fn finish(mut self) -> LmdbSourceKeyAudit {
        self.hasher.update(self.blob_entries.to_be_bytes());
        LmdbSourceKeyAudit {
            blob_entries: self.blob_entries,
            sha256: self.hasher.finalize().into(),
        }
    }
}

impl Default for LmdbSourceKeyAuditBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable LMDB generation fields used to bind a source-terminal receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LmdbEnvironmentGeneration {
    pub map_size: u64,
    pub last_page_number: u64,
    pub last_txn_id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LmdbTerminalMemberKeysetAudit {
    pub blob_entries: u64,
    pub metadata_entries: u64,
    pub total_bytes: u64,
    pub pinned_count: u64,
    pub pinned_bytes: u64,
    pub ownership_sha256: Hash,
    pub sha256: Hash,
}

impl LmdbBlobReader {
    pub fn open<P: AsRef<Path>>(
        path: P,
        external_blobs: Option<ExternalBlobOptions>,
    ) -> Result<Self, StoreError> {
        Self::open_with_external_read_concurrency(path, external_blobs, 1)
    }

    /// Open a read-only source with bounded concurrency for distinct unpacked
    /// external blob files.
    ///
    /// External pack ranges remain physically ordered and are read
    /// sequentially through one open pack at a time.
    pub fn open_with_external_read_concurrency<P: AsRef<Path>>(
        path: P,
        external_blobs: Option<ExternalBlobOptions>,
        external_read_concurrency: usize,
    ) -> Result<Self, StoreError> {
        Self::open_with_external_read_concurrency_inner(
            path.as_ref(),
            external_blobs,
            external_read_concurrency,
            None,
            false,
        )
    }

    /// Open an unpinned source for an exhaustive, key-ordered scan with
    /// kernel read-ahead enabled.
    pub fn open_sequential_with_external_read_concurrency<P: AsRef<Path>>(
        path: P,
        external_blobs: Option<ExternalBlobOptions>,
        external_read_concurrency: usize,
    ) -> Result<Self, StoreError> {
        Self::open_with_external_read_concurrency_inner(
            path.as_ref(),
            external_blobs,
            external_read_concurrency,
            None,
            true,
        )
    }

    /// Open a migration source through a retained directory descriptor and
    /// require the actual LMDB data/lock descriptors to match identities
    /// captured before the launch acknowledgement.
    pub fn open_with_external_read_concurrency_and_pinned_identity<P: AsRef<Path>>(
        path: P,
        external_blobs: Option<ExternalBlobOptions>,
        external_read_concurrency: usize,
        identity: PinnedLmdbIdentity,
    ) -> Result<Self, StoreError> {
        Self::open_with_external_read_concurrency_inner(
            path.as_ref(),
            external_blobs,
            external_read_concurrency,
            Some(identity),
            false,
        )
    }

    /// Open a pinned migration source for an exhaustive, key-ordered scan.
    ///
    /// Unlike the ordinary sparse reader, this leaves kernel read-ahead
    /// enabled. Large inline LMDB values otherwise degrade into one blocking
    /// major page fault per page during a complete migration or audit.
    pub fn open_sequential_with_external_read_concurrency_and_pinned_identity<P: AsRef<Path>>(
        path: P,
        external_blobs: Option<ExternalBlobOptions>,
        external_read_concurrency: usize,
        identity: PinnedLmdbIdentity,
    ) -> Result<Self, StoreError> {
        Self::open_with_external_read_concurrency_inner(
            path.as_ref(),
            external_blobs,
            external_read_concurrency,
            Some(identity),
            true,
        )
    }

    fn open_with_external_read_concurrency_inner(
        path: &Path,
        external_blobs: Option<ExternalBlobOptions>,
        external_read_concurrency: usize,
        pinned_identity: Option<PinnedLmdbIdentity>,
        sequential_scan: bool,
    ) -> Result<Self, StoreError> {
        if external_read_concurrency == 0 {
            return Err(StoreError::Other(
                "external read concurrency must be non-zero".into(),
            ));
        }
        let mut options = EnvOpenOptions::new();
        options
            .max_dbs(DATABASE_COUNT)
            .max_readers(DEFAULT_MAX_READERS);
        let mut flags = env_flags_from_env() | EnvFlags::READ_ONLY;
        if sequential_scan {
            flags.remove(EnvFlags::NO_READ_AHEAD);
        } else {
            // Ordinary verification performs sparse hash lookups across a
            // potentially very large source. Disable kernel read-ahead so
            // unrelated LMDB pages do not inflate reclaimable file cache.
            flags |= EnvFlags::NO_READ_AHEAD;
        }
        unsafe {
            options.flags(flags);
        }
        let env = unsafe {
            match pinned_identity {
                Some(identity) => ManagedEnv::open_pinned(&options, path, identity),
                None => ManagedEnv::open(&options, path),
            }
        }
        .map_err(map_heed_error)?;
        let rtxn = env.read_txn().map_err(map_heed_error)?;
        let open_bytes = |name| -> Result<Database<Bytes, Bytes>, StoreError> {
            env.open_database(&rtxn, Some(name))
                .map_err(map_heed_error)?
                .ok_or_else(|| StoreError::Other(format!("missing LMDB database {name}")))
        };
        let open_unit = |name| -> Result<Database<Bytes, Unit>, StoreError> {
            env.open_database(&rtxn, Some(name))
                .map_err(map_heed_error)?
                .ok_or_else(|| StoreError::Other(format!("missing LMDB database {name}")))
        };
        let blobs = open_bytes("blobs")?;
        let metadata = open_bytes("metadata")?;
        let eviction_order = open_unit("eviction_order")?;
        let pins = open_bytes("pins")?;
        let stats = open_bytes("stats")?;
        rtxn.commit().map_err(map_heed_error)?;
        let external_blobs = external_blobs
            .map(ExternalBlobConfig::from)
            .map(ExternalBlobConfig::prepare_runtime)
            .transpose()?;
        Ok(Self {
            store: LmdbBlobStore {
                env,
                blobs,
                metadata,
                eviction_order,
                pins,
                stats,
                max_bytes: AtomicU64::new(0),
                next_order: AtomicU64::new(0),
                external_blobs,
            },
            external_read_concurrency,
        })
    }

    pub fn get_sync(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        self.store.get_sync(hash)
    }

    pub fn blob_size_sync(&self, hash: &Hash) -> Result<Option<u64>, StoreError> {
        self.store.blob_size_sync(hash)
    }

    pub fn scan_hashes_after(
        &self,
        after: Option<Hash>,
        limit: usize,
    ) -> Result<Vec<Hash>, StoreError> {
        self.store.scan_hashes_after(after, limit)
    }

    /// Scan raw blob keys from `start` inclusively without requesting LMDB
    /// values.
    pub fn scan_hashes_from(&self, start: Hash, limit: usize) -> Result<Vec<Hash>, StoreError> {
        self.store.scan_hashes_from(start, limit)
    }

    /// Resolve exact logical sizes without loading blob payloads.
    ///
    /// Legacy loose external rows have no metadata size, so this stats the
    /// content-addressed external file; pack rows use their encoded interval.
    pub fn sizes_for_sorted_hashes(&self, hashes: &[Hash]) -> Result<Vec<u64>, StoreError> {
        let rtxn = self.store.env.read_txn().map_err(map_heed_error)?;
        hashes
            .iter()
            .map(|hash| {
                self.store
                    .blob_size_in_txn(&rtxn, hash)?
                    .ok_or_else(|| StoreError::Other(format!("source lost blob {hash:?}")))
            })
            .collect()
    }

    pub fn environment_generation(&self) -> LmdbEnvironmentGeneration {
        let info = self.store.env.info();
        LmdbEnvironmentGeneration {
            map_size: info.map_size as u64,
            last_page_number: info.last_page_number as u64,
            last_txn_id: info.last_txn_id as u64,
        }
    }

    /// Read a present set of hashes with one LMDB transaction and physically
    /// coalesced external-file reads, stopping before `byte_limit` after at
    /// least one result.
    ///
    /// Callers must supply only hashes that are present in this reader. The
    /// returned prefix length tells the caller where to resume.
    pub fn read_hashes_bounded(
        &self,
        hashes: &[Hash],
        byte_limit: u64,
    ) -> Result<Vec<(Hash, Vec<u8>)>, StoreError> {
        if hashes.is_empty() || byte_limit == 0 {
            return Ok(Vec::new());
        }

        // Resolve every LMDB value in one read transaction. The old migration
        // path opened one transaction per hash, amplifying cursor and mmap
        // overhead on multi-terabyte sources.
        let rtxn = self.store.env.read_txn().map_err(map_heed_error)?;
        let mut values = Vec::with_capacity(hashes.len());
        let mut reads = Vec::new();
        let mut selected_bytes = 0u64;

        for hash in hashes {
            let expected_size = self.store.blob_size_in_txn(&rtxn, hash)?.ok_or_else(|| {
                StoreError::Other(format!("source lost blob {hash:?} during scan"))
            })?;
            if !values.is_empty() && selected_bytes.saturating_add(expected_size) > byte_limit {
                break;
            }
            let value = self
                .store
                .blobs
                .get(&rtxn, hash)
                .map_err(map_heed_error)?
                .ok_or_else(|| {
                    StoreError::Other(format!("source lost blob {hash:?} during scan"))
                })?;
            let index = values.len();
            if self.store.is_external_blob_ref(hash, value) {
                let config = self.store.external_blobs.as_ref().ok_or_else(|| {
                    StoreError::Other(
                        "external LMDB blob marker found but external blobs are disabled".into(),
                    )
                })?;
                values.push((*hash, None));
                reads.push(SourceBlobReadPlan {
                    index,
                    hash: *hash,
                    read: SourceBlobRead::ExternalBlob {
                        file: ExternalSourceFile::new(
                            config,
                            ExternalBlobConfig::relative_blob_path(hash),
                        ),
                        expected_size,
                    },
                });
            } else if let Some(pack_ref) = LmdbBlobStore::decode_external_pack_ref(value)? {
                let config = self.store.external_blobs.as_ref().ok_or_else(|| {
                    StoreError::Other(
                        "external LMDB pack marker found but external blobs are disabled".into(),
                    )
                })?;
                values.push((*hash, None));
                reads.push(SourceBlobReadPlan {
                    index,
                    hash: *hash,
                    read: SourceBlobRead::ExternalPack {
                        file: ExternalSourceFile::new(
                            config,
                            ExternalBlobConfig::relative_pack_path(&pack_ref.name),
                        ),
                        offset: pack_ref.offset,
                        len: pack_ref.len,
                        expected_size,
                    },
                });
            } else {
                let data = value.to_vec();
                if data.len() as u64 != expected_size {
                    return Err(StoreError::Other(format!(
                        "source size changed for {hash:?}: metadata says {expected_size}, value has {}",
                        data.len()
                    )));
                }
                values.push((*hash, Some(data)));
            }
            selected_bytes = selected_bytes.saturating_add(expected_size);
        }
        drop(rtxn);

        let (mut external_reads, mut pack_reads): (Vec<_>, Vec<_>) = reads
            .into_iter()
            .partition(|plan| matches!(plan.read, SourceBlobRead::ExternalBlob { .. }));

        // Distinct legacy files can otherwise turn a migration into one
        // blocking disk seek per hash. Keep only a bounded number in flight so
        // RAID/device scheduling can reorder them without opening the whole
        // page at once.
        external_reads.sort_unstable_by(|left, right| {
            left.read
                .physical_path()
                .cmp(right.read.physical_path())
                .then_with(|| left.index.cmp(&right.index))
        });
        for (index, data) in
            read_external_blob_files(&external_reads, self.external_read_concurrency)?
        {
            values[index].1 = Some(data);
        }

        // Hash order is the durable cursor order, but external pack layout is
        // physical. Read pack payloads by path and offset through one open pack,
        // then restore the original hash order before verification/insertion.
        pack_reads.sort_unstable_by(|left, right| {
            left.read
                .physical_path()
                .cmp(right.read.physical_path())
                .then_with(|| {
                    left.read
                        .physical_offset()
                        .cmp(&right.read.physical_offset())
                })
                .then_with(|| left.index.cmp(&right.index))
        });
        let mut open_pack: Option<(PathBuf, File)> = None;
        for plan in pack_reads {
            let data = match plan.read {
                SourceBlobRead::ExternalPack {
                    file,
                    offset,
                    len,
                    expected_size,
                } => {
                    if len != expected_size {
                        return Err(StoreError::Other(format!(
                            "source size changed for {:?}: metadata says {expected_size}, pack marker says {len}",
                            plan.hash
                        )));
                    }
                    let must_open = open_pack
                        .as_ref()
                        .is_none_or(|(open_path, _)| open_path != &file.path);
                    if must_open {
                        open_pack = Some((file.path.clone(), file.open()?));
                    }
                    let (_, file) = open_pack.as_mut().expect("pack was opened");
                    file.seek(SeekFrom::Start(offset))?;
                    let read_len = usize::try_from(len).map_err(|_| {
                        StoreError::Other("external pack blob is too large to read".to_string())
                    })?;
                    let mut data = vec![0; read_len];
                    file.read_exact(&mut data)?;
                    data
                }
                SourceBlobRead::ExternalBlob { .. } => {
                    return Err(StoreError::Other(
                        "external blob read entered the pack read lane".to_string(),
                    ));
                }
            };
            values[plan.index].1 = Some(data);
        }

        values
            .into_iter()
            .map(|(hash, data)| {
                data.map(|data| (hash, data)).ok_or_else(|| {
                    StoreError::Other(format!("source read plan omitted blob {hash:?}"))
                })
            })
            .collect()
    }

    /// Mark candidate hashes present using one read transaction.
    pub fn existing_hashes_in_sorted_candidates(
        &self,
        sorted_hashes: &[Hash],
    ) -> Result<Vec<bool>, StoreError> {
        self.store
            .existing_hashes_in_sorted_candidates(sorted_hashes)
    }

    /// Mark exact raw `blobs` keys present without consulting metadata or
    /// requesting LMDB values.
    pub fn existing_blob_keys_in_sorted_candidates(
        &self,
        sorted_hashes: &[Hash],
    ) -> Result<Vec<bool>, StoreError> {
        self.store
            .existing_blob_keys_in_sorted_candidates(sorted_hashes)
    }

    /// Prove exact raw `blobs` keys with one bounded forward cursor scan.
    pub fn blob_keys_match_bounded_raw_range(
        &self,
        sorted_hashes: &[Hash],
        inclusive_end: Hash,
        max_scanned_keys: usize,
    ) -> Result<bool, StoreError> {
        self.store
            .blob_keys_match_bounded_raw_range(sorted_hashes, inclusive_end, max_scanned_keys)
    }

    /// Read the aggregate counters without opening the environment writable.
    pub fn stats(&self) -> Result<LmdbStats, StoreError> {
        self.store.stats()
    }

    /// Read exact LMDB entry counts from the blob and metadata database roots.
    pub fn database_entry_counts(&self) -> Result<(u64, u64), StoreError> {
        let rtxn = self.store.env.read_txn().map_err(map_heed_error)?;
        let blob_entries = self.store.blobs.len(&rtxn).map_err(map_heed_error)?;
        let metadata_entries = self.store.metadata.len(&rtxn).map_err(map_heed_error)?;
        Ok((blob_entries, metadata_entries))
    }

    /// Prove the exact terminal physical indexes and aggregate state of one
    /// Pool member.
    ///
    /// Pool members are never accepted as legacy blob-only stores. Metadata
    /// sizes must match exact inline/loose/packed body lengths, every metadata
    /// row must have its exact eviction key, every nonzero pin must reference
    /// metadata, and the recomputed totals must equal persisted counters. The
    /// digest binds all of that state, not only entry counts.
    pub(crate) fn validate_terminal_member_keyset(
        &self,
    ) -> Result<LmdbTerminalMemberKeysetAudit, StoreError> {
        self.store.validate_exact_member_state()
    }

    /// Prove the complete raw source catalog and every encoded storage
    /// location used by the final migration scan.
    ///
    /// `blobs` is the membership authority. Missing metadata is valid for
    /// legacy rows, including in a partially upgraded database; metadata with
    /// no corresponding blob row is invalid. The keyset digest binds every raw
    /// catalog key, while the location digest binds whether each value is
    /// inline, a loose external marker, or an exact external pack marker.
    /// Callers must keep every source writer fenced throughout this scan.
    pub fn validate_terminal_migration_keyset(&self) -> Result<LmdbSourceKeysetAudit, StoreError> {
        let rtxn = self.store.env.read_txn().map_err(map_heed_error)?;
        let blob_entries = self.store.blobs.len(&rtxn).map_err(map_heed_error)?;
        let metadata_entries = self.store.metadata.len(&rtxn).map_err(map_heed_error)?;
        let legacy_blob_only = metadata_entries == 0;
        let mut keyset_digest = Sha256::new();
        keyset_digest.update(b"hashtree-lmdb-terminal-source-keyset/v2\0");
        keyset_digest.update(blob_entries.to_be_bytes());
        keyset_digest.update(metadata_entries.to_be_bytes());
        let mut location_digest = Sha256::new();
        location_digest.update(b"hashtree-lmdb-terminal-source-locations/v1\0");
        location_digest.update(blob_entries.to_be_bytes());
        location_digest.update(metadata_entries.to_be_bytes());
        let mut blob_only_entries = 0u64;
        let mut inline_entries = 0u64;
        let mut loose_external_entries = 0u64;
        let mut packed_external_entries = 0u64;

        let blobs = self.store.blobs.iter(&rtxn).map_err(map_heed_error)?;
        let mut metadata = self.store.metadata.iter(&rtxn).map_err(map_heed_error)?;
        let mut next_metadata = metadata.next().transpose().map_err(map_heed_error)?;
        for item in blobs {
            let (blob_key, blob_value) = item.map_err(map_heed_error)?;
            let blob_hash: Hash = blob_key.try_into().map_err(|_| {
                StoreError::Other("terminal source has an invalid blob hash key".into())
            })?;
            while next_metadata
                .as_ref()
                .is_some_and(|(metadata_key, _)| *metadata_key < blob_key)
            {
                let metadata_key = next_metadata
                    .as_ref()
                    .expect("checked present metadata key")
                    .0;
                let metadata_hash: Hash = metadata_key.try_into().map_err(|_| {
                    StoreError::Other("terminal source has an invalid metadata hash key".into())
                })?;
                return Err(StoreError::Other(format!(
                    "terminal source metadata {} has no raw blob row",
                    to_hex(&metadata_hash)
                )));
            }
            if next_metadata
                .as_ref()
                .is_some_and(|(metadata_key, _)| *metadata_key == blob_key)
            {
                next_metadata = metadata.next().transpose().map_err(map_heed_error)?;
            } else {
                blob_only_entries = blob_only_entries
                    .checked_add(1)
                    .ok_or_else(|| StoreError::Other("source blob-only count overflow".into()))?;
            }

            keyset_digest.update(blob_hash);
            location_digest.update(blob_hash);
            if self.store.is_external_blob_ref(&blob_hash, blob_value) {
                loose_external_entries =
                    loose_external_entries.checked_add(1).ok_or_else(|| {
                        StoreError::Other("source loose-external count overflow".into())
                    })?;
                location_digest.update(b"L");
                location_digest.update((blob_value.len() as u64).to_be_bytes());
                location_digest.update(blob_value);
            } else if LmdbBlobStore::decode_external_pack_ref(blob_value)?.is_some() {
                packed_external_entries =
                    packed_external_entries.checked_add(1).ok_or_else(|| {
                        StoreError::Other("source packed-external count overflow".into())
                    })?;
                location_digest.update(b"P");
                location_digest.update((blob_value.len() as u64).to_be_bytes());
                location_digest.update(blob_value);
            } else {
                inline_entries = inline_entries
                    .checked_add(1)
                    .ok_or_else(|| StoreError::Other("source inline count overflow".into()))?;
                location_digest.update(b"I");
                location_digest.update((blob_value.len() as u64).to_be_bytes());
            }
        }
        if let Some((metadata_key, _)) = next_metadata.as_ref() {
            let metadata_hash: Hash = (*metadata_key).try_into().map_err(|_| {
                StoreError::Other("terminal source has an invalid metadata hash key".into())
            })?;
            return Err(StoreError::Other(format!(
                "terminal source metadata {} has no raw blob row",
                to_hex(&metadata_hash)
            )));
        }
        drop(metadata);
        rtxn.commit().map_err(map_heed_error)?;

        Ok(LmdbSourceKeysetAudit {
            blob_entries,
            metadata_entries,
            blob_only_entries,
            legacy_blob_only,
            inline_entries,
            loose_external_entries,
            packed_external_entries,
            sha256: keyset_digest.finalize().into(),
            catalog_location_sha256: location_digest.finalize().into(),
        })
    }

    pub fn map_size_bytes(&self) -> usize {
        self.store.map_size_bytes()
    }
}

fn read_external_blob_files(
    plans: &[SourceBlobReadPlan],
    concurrency: usize,
) -> Result<Vec<(usize, Vec<u8>)>, StoreError> {
    if plans.is_empty() {
        return Ok(Vec::new());
    }
    let worker_count = concurrency.min(plans.len());
    let next = AtomicUsize::new(0);
    let cancelled = AtomicBool::new(false);
    let (sender, receiver) = mpsc::channel();
    let mut results = (0..plans.len())
        .map(|_| None)
        .collect::<Vec<Option<Result<Vec<u8>, StoreError>>>>();

    thread::scope(|scope| -> Result<(), StoreError> {
        for _ in 0..worker_count {
            let sender = sender.clone();
            let next = &next;
            let cancelled = &cancelled;
            scope.spawn(move || loop {
                if cancelled.load(Ordering::Acquire) {
                    break;
                }
                let plan_index = next.fetch_add(1, Ordering::Relaxed);
                let Some(plan) = plans.get(plan_index) else {
                    break;
                };
                if cancelled.load(Ordering::Acquire) {
                    break;
                }
                let result = read_external_blob_file(plan);
                let failed = result.is_err();
                if failed {
                    cancelled.store(true, Ordering::Release);
                }
                if sender.send((plan_index, result)).is_err() {
                    break;
                }
                if failed {
                    break;
                }
            });
        }
        drop(sender);
        for (plan_index, result) in receiver {
            results[plan_index] = Some(result);
        }
        Ok(())
    })?;

    for result in &mut results {
        if result.as_ref().is_some_and(Result::is_err) {
            return Err(result
                .take()
                .expect("checked external source read result")
                .expect_err("checked external source read error"));
        }
    }
    results
        .into_iter()
        .enumerate()
        .map(|(plan_index, result)| {
            let data = result
                .ok_or_else(|| StoreError::Other("external source read omitted a file".into()))??;
            Ok((plans[plan_index].index, data))
        })
        .collect()
}

fn read_external_blob_file(plan: &SourceBlobReadPlan) -> Result<Vec<u8>, StoreError> {
    let SourceBlobRead::ExternalBlob {
        file,
        expected_size,
    } = &plan.read
    else {
        return Err(StoreError::Other(
            "pack range entered the external-file read lane".into(),
        ));
    };
    let mut file = file.open()?;
    let file_size = file.metadata()?.len();
    if file_size != *expected_size {
        return Err(StoreError::Other(format!(
            "source size changed for {:?}: metadata says {expected_size}, file has {}",
            plan.hash, file_size
        )));
    }
    let expected_len = usize::try_from(*expected_size)
        .map_err(|_| StoreError::Other("external source blob is too large to read".into()))?;
    let read_limit = expected_size
        .checked_add(1)
        .ok_or_else(|| StoreError::Other("external source blob is too large to read".into()))?;
    let mut data = Vec::with_capacity(expected_len.min(1024 * 1024));
    Read::by_ref(&mut file)
        .take(read_limit)
        .read_to_end(&mut data)
        .map_err(StoreError::Io)?;
    if data.len() != expected_len {
        return Err(StoreError::Other(format!(
            "source size changed for {:?}: metadata says {expected_size}, read {}",
            plan.hash,
            data.len()
        )));
    }
    Ok(data)
}

impl LmdbBlobStore {
    /// Open or create an LMDB blob store at the given path.
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self, StoreError> {
        Self::with_map_size(path, DEFAULT_MAP_SIZE)
    }

    /// Open or create with a maximum logical storage size.
    pub fn with_max_bytes<P: AsRef<Path>>(path: P, max_bytes: u64) -> Result<Self, StoreError> {
        Self::with_max_bytes_and_external_blob_options(
            path,
            max_bytes,
            ExternalBlobOptions::from_env,
        )
    }

    /// Open or create with a maximum logical storage size and custom external-blob options.
    pub fn with_max_bytes_and_external_blob_options<P, F>(
        path: P,
        max_bytes: u64,
        external_blobs: F,
    ) -> Result<Self, StoreError>
    where
        P: AsRef<Path>,
        F: FnOnce(&Path) -> Option<ExternalBlobOptions>,
    {
        let path_ref = path.as_ref();
        let existing_map_size = std::fs::metadata(path_ref.join("data.mdb"))
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let existing_headroom = if existing_map_size == 0 {
            0
        } else {
            existing_map_size
                .saturating_div(10)
                .max(REOPEN_HEADROOM_BYTES)
        };
        let requested_map_size = usize::try_from(Self::align_map_size_bytes(
            existing_map_size
                .saturating_add(existing_headroom)
                .max(max_bytes),
        ))
        .unwrap_or(usize::MAX)
        .max(DEFAULT_MAP_SIZE);
        let external_blobs = external_blobs(path_ref);
        let store = Self::with_map_size_and_external_blob_options(
            path_ref,
            requested_map_size,
            external_blobs,
        )?;
        store.max_bytes.store(max_bytes, Ordering::Relaxed);
        let current = store.total_bytes()?;
        if max_bytes > 0 && current > max_bytes {
            let target = max_bytes.saturating_mul(9) / 10;
            store.evict_to_target(current, target)?;
        }
        Ok(store)
    }

    /// Open or create with the default map size and explicit external-blob options.
    pub fn with_external_blob_options<P: AsRef<Path>>(
        path: P,
        external_blobs: Option<ExternalBlobOptions>,
    ) -> Result<Self, StoreError> {
        Self::with_map_size_and_external_blob_options(path, DEFAULT_MAP_SIZE, external_blobs)
    }

    fn align_map_size_bytes(bytes: u64) -> u64 {
        if bytes == 0 {
            return 0;
        }
        let page_size = (page_size::get() as u64).max(4096);
        let remainder = bytes % page_size;
        if remainder == 0 {
            bytes
        } else {
            bytes.saturating_add(page_size - remainder)
        }
    }

    /// Open or create with custom map size.
    pub fn with_map_size<P: AsRef<Path>>(path: P, map_size: usize) -> Result<Self, StoreError> {
        Self::with_map_size_and_settings(
            path,
            map_size,
            env_flags_from_env(),
            ExternalBlobConfig::from_env,
        )
    }

    /// Open or create with custom map size and explicit external-blob options.
    pub fn with_map_size_and_external_blob_options<P: AsRef<Path>>(
        path: P,
        map_size: usize,
        external_blobs: Option<ExternalBlobOptions>,
    ) -> Result<Self, StoreError> {
        Self::with_map_size_and_settings(path, map_size, env_flags_from_env(), |_| {
            external_blobs.map(Into::into)
        })
    }

    /// Open or create with one exact, manifest-owned map size.
    ///
    /// Dynamic pools persist this value so every same-host process opens a member
    /// with identical LMDB options. Unlike the general constructor, reopening does
    /// not opportunistically add headroom; resizing is an explicit pool operation.
    pub fn with_exact_map_size_and_external_blob_options<P: AsRef<Path>>(
        path: P,
        map_size: usize,
        external_blobs: Option<ExternalBlobOptions>,
    ) -> Result<Self, StoreError> {
        Self::with_map_size_and_settings_mode(
            path,
            map_size,
            env_flags_from_env(),
            |_| external_blobs.map(Into::into),
            false,
            None,
        )
    }

    pub(crate) fn with_exact_map_size_external_options_and_pinned_identity<P: AsRef<Path>>(
        path: P,
        map_size: usize,
        external_blobs: Option<ExternalBlobOptions>,
        identity: PinnedLmdbIdentity,
    ) -> Result<Self, StoreError> {
        Self::with_map_size_and_settings_mode(
            path,
            map_size,
            env_flags_from_env(),
            |_| external_blobs.map(Into::into),
            false,
            Some(identity),
        )
    }

    fn with_map_size_and_settings<P, F>(
        path: P,
        map_size: usize,
        flags: EnvFlags,
        external_blobs: F,
    ) -> Result<Self, StoreError>
    where
        P: AsRef<Path>,
        F: FnOnce(&Path) -> Option<ExternalBlobConfig>,
    {
        Self::with_map_size_and_settings_mode(path, map_size, flags, external_blobs, true, None)
    }

    fn with_map_size_and_settings_mode<P, F>(
        path: P,
        map_size: usize,
        flags: EnvFlags,
        external_blobs: F,
        add_reopen_headroom: bool,
        pinned_identity: Option<PinnedLmdbIdentity>,
    ) -> Result<Self, StoreError>
    where
        P: AsRef<Path>,
        F: FnOnce(&Path) -> Option<ExternalBlobConfig>,
    {
        let path_ref = path.as_ref();
        let controlled = pinned_identity.is_some();
        if !controlled {
            std::fs::create_dir_all(path_ref).map_err(StoreError::Io)?;
        }
        let existing_map_size = match pinned_identity {
            Some(identity) => pinned_lmdb_data_len(path_ref, identity)?,
            None => std::fs::metadata(path_ref.join("data.mdb"))
                .map(|metadata| metadata.len())
                .unwrap_or(0),
        };
        let existing_headroom = if controlled || existing_map_size == 0 || !add_reopen_headroom {
            0
        } else {
            existing_map_size
                .saturating_div(10)
                .max(REOPEN_HEADROOM_BYTES)
        };
        let requested_map_size = u64::try_from(map_size).unwrap_or(u64::MAX);
        let map_size = usize::try_from(Self::align_map_size_bytes(
            requested_map_size.max(existing_map_size.saturating_add(existing_headroom)),
        ))
        .unwrap_or(usize::MAX);

        let mut env_options = EnvOpenOptions::new();
        if !controlled {
            env_options.map_size(map_size);
        }
        env_options
            .max_dbs(DATABASE_COUNT)
            .max_readers(DEFAULT_MAX_READERS);
        unsafe {
            env_options.flags(flags);
        }
        let env = unsafe {
            match pinned_identity {
                Some(identity) => ManagedEnv::open_pinned(&env_options, path_ref, identity),
                None => ManagedEnv::open(&env_options, path_ref),
            }
            .map_err(map_heed_error)?
        };
        if controlled && env.info().map_size < map_size {
            return Err(StoreError::Other(format!(
                "controlled LMDB member map is {} bytes, below its manifest-owned {} bytes; pre-size it before exact migration",
                env.info().map_size,
                map_size
            )));
        }
        if !controlled {
            let _ = env.clear_stale_readers();
        }
        if !controlled && env.info().map_size < map_size {
            unsafe { env.resize(map_size) }.map_err(map_heed_error)?;
        }

        let (blobs, metadata, eviction_order, pins, stats, next_order) = if controlled {
            let rtxn = env.read_txn().map_err(map_heed_error)?;
            let open_bytes = |name| -> Result<Database<Bytes, Bytes>, StoreError> {
                env.open_database(&rtxn, Some(name))
                    .map_err(map_heed_error)?
                    .ok_or_else(|| {
                        StoreError::Other(format!(
                            "controlled LMDB member is missing required database {name}"
                        ))
                    })
            };
            let open_unit = |name| -> Result<Database<Bytes, Unit>, StoreError> {
                env.open_database(&rtxn, Some(name))
                    .map_err(map_heed_error)?
                    .ok_or_else(|| {
                        StoreError::Other(format!(
                            "controlled LMDB member is missing required database {name}"
                        ))
                    })
            };
            let blobs = open_bytes("blobs")?;
            let metadata = open_bytes("metadata")?;
            let eviction_order = open_unit("eviction_order")?;
            let pins = open_bytes("pins")?;
            let stats = open_bytes("stats")?;
            let next_order = Self::validate_controlled_member_state(
                &rtxn,
                blobs,
                metadata,
                eviction_order,
                pins,
                stats,
            )?;
            rtxn.commit().map_err(map_heed_error)?;
            (blobs, metadata, eviction_order, pins, stats, next_order)
        } else {
            let mut wtxn = env.write_txn().map_err(map_heed_error)?;
            let blobs = env
                .create_database(&mut wtxn, Some("blobs"))
                .map_err(map_heed_error)?;
            let metadata = env
                .create_database(&mut wtxn, Some("metadata"))
                .map_err(map_heed_error)?;
            let eviction_order = env
                .create_database(&mut wtxn, Some("eviction_order"))
                .map_err(map_heed_error)?;
            let pins = env
                .create_database(&mut wtxn, Some("pins"))
                .map_err(map_heed_error)?;
            let stats = env
                .create_database(&mut wtxn, Some("stats"))
                .map_err(map_heed_error)?;
            wtxn.commit().map_err(map_heed_error)?;
            let totals = Self::load_or_rebuild_totals(&env, metadata, pins, stats)?;
            let next_order = Self::load_or_initialize_next_order(&env, stats, totals.count > 0)?;
            (blobs, metadata, eviction_order, pins, stats, next_order)
        };

        let external_blobs = external_blobs(path_ref)
            .map(ExternalBlobConfig::prepare_runtime)
            .transpose()?;
        Ok(Self {
            env,
            blobs,
            metadata,
            eviction_order,
            pins,
            stats,
            max_bytes: AtomicU64::new(0),
            next_order: AtomicU64::new(next_order),
            external_blobs,
        })
    }

    fn validate_controlled_member_state(
        txn: &heed::RoTxn<'_>,
        blobs: Database<Bytes, Bytes>,
        metadata: Database<Bytes, Bytes>,
        eviction_order: Database<Bytes, Unit>,
        pins: Database<Bytes, Bytes>,
        stats: Database<Bytes, Bytes>,
    ) -> Result<u64, StoreError> {
        let totals = Self::read_store_totals(stats, txn)?.ok_or_else(|| {
            StoreError::Other("controlled LMDB member is missing persisted aggregate totals".into())
        })?;
        let next_order = Self::read_next_order(stats, txn)?.ok_or_else(|| {
            StoreError::Other(
                "controlled LMDB member is missing persisted next-order authority".into(),
            )
        })?;
        let blob_entries = blobs.len(txn).map_err(map_heed_error)?;
        let metadata_entries = metadata.len(txn).map_err(map_heed_error)?;
        let eviction_entries = eviction_order.len(txn).map_err(map_heed_error)?;
        let pin_entries = pins.len(txn).map_err(map_heed_error)?;
        if blob_entries != totals.count
            || metadata_entries != totals.count
            || eviction_entries != totals.count
            || pin_entries != totals.pinned_count
            || totals.pinned_count > totals.count
            || totals.pinned_bytes > totals.total_bytes
            || next_order < totals.count
        {
            return Err(StoreError::Other(format!(
                "controlled LMDB member secondary/stat counts are stale: blobs {blob_entries}, metadata {metadata_entries}, eviction {eviction_entries}, pins {pin_entries}, totals {totals:?}, next order {next_order}"
            )));
        }
        Ok(next_order)
    }

    fn validate_exact_member_state(&self) -> Result<LmdbTerminalMemberKeysetAudit, StoreError> {
        let rtxn = self.env.read_txn().map_err(map_heed_error)?;
        let persisted = Self::read_store_totals(self.stats, &rtxn)?.ok_or_else(|| {
            StoreError::Other("LMDB member is missing persisted aggregate totals".into())
        })?;
        let next_order = Self::read_next_order(self.stats, &rtxn)?.ok_or_else(|| {
            StoreError::Other("LMDB member is missing persisted next-order authority".into())
        })?;
        let blob_entries = self.blobs.len(&rtxn).map_err(map_heed_error)?;
        let metadata_entries = self.metadata.len(&rtxn).map_err(map_heed_error)?;
        let eviction_entries = self.eviction_order.len(&rtxn).map_err(map_heed_error)?;
        let pin_entries = self.pins.len(&rtxn).map_err(map_heed_error)?;
        if blob_entries != metadata_entries {
            return Err(StoreError::Other(format!(
                "terminal Pool member blobs/metadata key sets differ: {blob_entries} blob rows, {metadata_entries} metadata rows"
            )));
        }
        if eviction_entries != metadata_entries {
            return Err(StoreError::Other(format!(
                "terminal Pool member eviction/metadata key sets differ: {eviction_entries} eviction rows, {metadata_entries} metadata rows"
            )));
        }

        let mut computed = StoreTotals::default();
        let mut digest = Sha256::new();
        digest.update(b"hashtree-lmdb-terminal-member-state/v2\0");
        digest.update(blob_entries.to_be_bytes());
        digest.update(metadata_entries.to_be_bytes());
        digest.update(eviction_entries.to_be_bytes());
        digest.update(pin_entries.to_be_bytes());
        digest.update(next_order.to_be_bytes());
        let mut ownership_digest = Sha256::new();
        ownership_digest.update(b"hashtree-pool-member-ownership/v1\0");
        let mut external_pack_lengths = HashMap::<String, u64>::new();

        for item in self.metadata.iter(&rtxn).map_err(map_heed_error)? {
            let (metadata_key, metadata_bytes) = item.map_err(map_heed_error)?;
            let hash: Hash = metadata_key.try_into().map_err(|_| {
                StoreError::Other("terminal Pool member has an invalid metadata hash key".into())
            })?;
            let meta = Self::decode_blob_meta(metadata_bytes)?;
            let blob_value = self
                .blobs
                .get(&rtxn, &hash)
                .map_err(map_heed_error)?
                .ok_or_else(|| {
                    StoreError::Other(format!(
                        "terminal Pool member blobs/metadata key sets differ: metadata {} has no blob row",
                        to_hex(&hash)
                    ))
                })?;
            let actual_size = if self.is_external_blob_ref(&hash, blob_value) {
                let config = self.external_blobs.as_ref().ok_or_else(|| {
                    StoreError::Other(
                        "terminal Pool member has an external blob marker but no external directory"
                            .into(),
                    )
                })?;
                let size = config
                    .open_relative(&ExternalBlobConfig::relative_blob_path(&hash))?
                    .metadata()
                    .map_err(StoreError::Io)?
                    .len();
                digest.update(b"L");
                digest.update((blob_value.len() as u64).to_be_bytes());
                digest.update(blob_value);
                size
            } else if let Some(pack_ref) = Self::decode_external_pack_ref(blob_value)? {
                let config = self.external_blobs.as_ref().ok_or_else(|| {
                    StoreError::Other(
                        "terminal Pool member has an external pack marker but no external directory"
                            .into(),
                    )
                })?;
                let end = pack_ref.offset.checked_add(pack_ref.len).ok_or_else(|| {
                    StoreError::Other("terminal Pool member external pack range overflow".into())
                })?;
                let pack_size = match external_pack_lengths.get(&pack_ref.name) {
                    Some(size) => *size,
                    None => {
                        let size = config
                            .open_relative(&ExternalBlobConfig::relative_pack_path(&pack_ref.name))?
                            .metadata()
                            .map_err(StoreError::Io)?
                            .len();
                        external_pack_lengths.insert(pack_ref.name.clone(), size);
                        size
                    }
                };
                if end > pack_size {
                    return Err(StoreError::Other(format!(
                        "terminal Pool member external pack range {}..{end} exceeds physical pack size {pack_size}",
                        pack_ref.offset
                    )));
                }
                digest.update(b"P");
                digest.update((blob_value.len() as u64).to_be_bytes());
                digest.update(blob_value);
                pack_ref.len
            } else {
                digest.update(b"I");
                digest.update((blob_value.len() as u64).to_be_bytes());
                blob_value.len() as u64
            };
            if meta.size != actual_size {
                return Err(StoreError::Other(format!(
                    "terminal Pool member metadata size {} for {} differs from exact body size {actual_size}",
                    meta.size,
                    to_hex(&hash)
                )));
            }
            let order_key = Self::encode_order_key(meta.order, &hash);
            if self
                .eviction_order
                .get(&rtxn, &order_key)
                .map_err(map_heed_error)?
                .is_none()
            {
                return Err(StoreError::Other(format!(
                    "terminal Pool member eviction/metadata key sets differ: metadata {} has no exact eviction row",
                    to_hex(&hash)
                )));
            }
            if meta.order >= next_order {
                return Err(StoreError::Other(format!(
                    "terminal Pool member metadata order {} for {} is not below next order {next_order}",
                    meta.order,
                    to_hex(&hash)
                )));
            }
            computed.count = computed
                .count
                .checked_add(1)
                .ok_or_else(|| StoreError::Other("LMDB member blob count overflow".into()))?;
            computed.total_bytes = computed
                .total_bytes
                .checked_add(meta.size)
                .ok_or_else(|| StoreError::Other("LMDB member byte total overflow".into()))?;
            ownership_digest.update(hash);
            ownership_digest.update(meta.size.to_be_bytes());
            digest.update(hash);
            digest.update((metadata_bytes.len() as u64).to_be_bytes());
            digest.update(metadata_bytes);
        }

        for item in self.pins.iter(&rtxn).map_err(map_heed_error)? {
            let (pin_key, pin_bytes) = item.map_err(map_heed_error)?;
            let hash: Hash = pin_key.try_into().map_err(|_| {
                StoreError::Other("terminal Pool member has an invalid pin hash key".into())
            })?;
            let pin_count = Self::decode_pin_count(pin_bytes)?;
            if pin_count == 0 {
                return Err(StoreError::Other(format!(
                    "terminal Pool member has a zero-count pin row for {}",
                    to_hex(&hash)
                )));
            }
            let metadata_bytes = self
                .metadata
                .get(&rtxn, &hash)
                .map_err(map_heed_error)?
                .ok_or_else(|| {
                    StoreError::Other(format!(
                        "terminal Pool member pin {} has no metadata row",
                        to_hex(&hash)
                    ))
                })?;
            let meta = Self::decode_blob_meta(metadata_bytes)?;
            computed.pinned_count = computed
                .pinned_count
                .checked_add(1)
                .ok_or_else(|| StoreError::Other("LMDB member pinned count overflow".into()))?;
            computed.pinned_bytes =
                computed
                    .pinned_bytes
                    .checked_add(meta.size)
                    .ok_or_else(|| {
                        StoreError::Other("LMDB member pinned byte total overflow".into())
                    })?;
            digest.update(hash);
            digest.update(pin_count.to_be_bytes());
        }

        if computed != persisted {
            return Err(StoreError::Other(format!(
                "LMDB member persisted aggregate totals are stale: persisted {persisted:?}, exact {computed:?}"
            )));
        }
        digest.update(Self::encode_store_totals(computed));
        rtxn.commit().map_err(map_heed_error)?;

        Ok(LmdbTerminalMemberKeysetAudit {
            blob_entries,
            metadata_entries,
            total_bytes: computed.total_bytes,
            pinned_count: computed.pinned_count,
            pinned_bytes: computed.pinned_bytes,
            ownership_sha256: ownership_digest.finalize().into(),
            sha256: digest.finalize().into(),
        })
    }

    fn load_or_rebuild_totals(
        env: &heed::Env,
        metadata: Database<Bytes, Bytes>,
        pins: Database<Bytes, Bytes>,
        stats: Database<Bytes, Bytes>,
    ) -> Result<StoreTotals, StoreError> {
        {
            let rtxn = env.read_txn().map_err(map_heed_error)?;
            if let Some(totals) = Self::read_store_totals(stats, &rtxn)? {
                return Ok(totals);
            }
        }

        let mut totals = StoreTotals::default();
        {
            let rtxn = env.read_txn().map_err(map_heed_error)?;
            for item in metadata.iter(&rtxn).map_err(map_heed_error)? {
                let (_, bytes) = item.map_err(map_heed_error)?;
                let meta = Self::decode_blob_meta(bytes)?;
                totals.count = totals.count.saturating_add(1);
                totals.total_bytes = totals.total_bytes.saturating_add(meta.size);
            }
            for item in pins.iter(&rtxn).map_err(map_heed_error)? {
                let (hash, pin_bytes) = item.map_err(map_heed_error)?;
                let count = Self::decode_pin_count(pin_bytes)?;
                if count == 0 {
                    continue;
                }
                let Some(meta_bytes) = metadata.get(&rtxn, hash).map_err(map_heed_error)? else {
                    continue;
                };
                let meta = Self::decode_blob_meta(meta_bytes)?;
                totals.pinned_count = totals.pinned_count.saturating_add(1);
                totals.pinned_bytes = totals.pinned_bytes.saturating_add(meta.size);
            }
        }

        let mut wtxn = env.write_txn().map_err(map_heed_error)?;
        Self::write_store_totals(stats, &mut wtxn, totals).map_err(map_heed_error)?;
        wtxn.commit().map_err(map_heed_error)?;
        Ok(totals)
    }

    fn load_or_initialize_next_order(
        env: &heed::Env,
        stats: Database<Bytes, Bytes>,
        has_existing_blobs: bool,
    ) -> Result<u64, StoreError> {
        {
            let rtxn = env.read_txn().map_err(map_heed_error)?;
            if let Some(next_order) = Self::read_next_order(stats, &rtxn)? {
                return Ok(next_order);
            }
        }

        let next_order = if has_existing_blobs {
            Self::legacy_next_order_floor()
        } else {
            0
        };
        let mut wtxn = env.write_txn().map_err(map_heed_error)?;
        let next_order = Self::read_next_order_lossy(stats, &wtxn)
            .map_err(map_heed_error)?
            .unwrap_or(next_order);
        Self::write_next_order(stats, &mut wtxn, next_order).map_err(map_heed_error)?;
        wtxn.commit().map_err(map_heed_error)?;
        Ok(next_order)
    }

    fn legacy_next_order_floor() -> u64 {
        let micros = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|duration| u64::try_from(duration.as_micros()).ok())
            .unwrap_or(NEXT_ORDER_MIGRATION_FLOOR);
        micros.max(NEXT_ORDER_MIGRATION_FLOOR)
    }

    fn read_store_totals(
        stats: Database<Bytes, Bytes>,
        txn: &heed::RoTxn,
    ) -> Result<Option<StoreTotals>, StoreError> {
        stats
            .get(txn, STORE_TOTALS_KEY)
            .map_err(map_heed_error)?
            .map(Self::decode_store_totals)
            .transpose()
    }

    fn read_store_totals_lossy(
        stats: Database<Bytes, Bytes>,
        txn: &heed::RoTxn,
    ) -> std::result::Result<StoreTotals, HeedError> {
        Ok(stats
            .get(txn, STORE_TOTALS_KEY)?
            .and_then(Self::decode_store_totals_lossy)
            .unwrap_or_default())
    }

    fn write_store_totals(
        stats: Database<Bytes, Bytes>,
        txn: &mut heed::RwTxn,
        totals: StoreTotals,
    ) -> std::result::Result<(), HeedError> {
        let encoded = Self::encode_store_totals(totals);
        stats.put(txn, STORE_TOTALS_KEY, &encoded)
    }

    fn read_next_order(
        stats: Database<Bytes, Bytes>,
        txn: &heed::RoTxn,
    ) -> Result<Option<u64>, StoreError> {
        stats
            .get(txn, NEXT_ORDER_KEY)
            .map_err(map_heed_error)?
            .map(Self::decode_next_order)
            .transpose()
    }

    fn read_next_order_lossy(
        stats: Database<Bytes, Bytes>,
        txn: &heed::RoTxn,
    ) -> std::result::Result<Option<u64>, HeedError> {
        Ok(stats
            .get(txn, NEXT_ORDER_KEY)?
            .and_then(Self::decode_next_order_lossy))
    }

    fn write_next_order(
        stats: Database<Bytes, Bytes>,
        txn: &mut heed::RwTxn,
        next_order: u64,
    ) -> std::result::Result<(), HeedError> {
        let encoded = Self::encode_next_order(next_order);
        stats.put(txn, NEXT_ORDER_KEY, &encoded)
    }

    fn allocate_order_range_in_txn(
        &self,
        txn: &mut heed::RwTxn,
        count: usize,
    ) -> std::result::Result<u64, HeedError> {
        let start = Self::read_next_order_lossy(self.stats, txn)?
            .unwrap_or_else(|| self.next_order.load(Ordering::Relaxed));
        let end = start.saturating_add(count as u64);
        Self::write_next_order(self.stats, txn, end)?;
        self.next_order.store(end, Ordering::Relaxed);
        Ok(start)
    }

    fn increment_totals_in_txn(
        &self,
        txn: &mut heed::RwTxn,
        count: u64,
        total_bytes: u64,
        pinned_count: u64,
        pinned_bytes: u64,
    ) -> std::result::Result<(), HeedError> {
        let mut totals = Self::read_store_totals_lossy(self.stats, txn)?;
        totals.count = totals.count.saturating_add(count);
        totals.total_bytes = totals.total_bytes.saturating_add(total_bytes);
        totals.pinned_count = totals.pinned_count.saturating_add(pinned_count);
        totals.pinned_bytes = totals.pinned_bytes.saturating_add(pinned_bytes);
        Self::write_store_totals(self.stats, txn, totals)
    }

    fn decrement_totals_in_txn(
        &self,
        txn: &mut heed::RwTxn,
        count: u64,
        total_bytes: u64,
        pinned_count: u64,
        pinned_bytes: u64,
    ) -> std::result::Result<(), HeedError> {
        let mut totals = Self::read_store_totals_lossy(self.stats, txn)?;
        totals.count = totals.count.saturating_sub(count);
        totals.total_bytes = totals.total_bytes.saturating_sub(total_bytes);
        totals.pinned_count = totals.pinned_count.saturating_sub(pinned_count);
        totals.pinned_bytes = totals.pinned_bytes.saturating_sub(pinned_bytes);
        Self::write_store_totals(self.stats, txn, totals)
    }

    /// Check if a hash exists (sync version for internal use).
    pub fn exists(&self, hash: &Hash) -> Result<bool, StoreError> {
        let rtxn = self
            .env
            .read_txn()
            .map_err(|e| StoreError::Other(e.to_string()))?;

        if self
            .metadata
            .get(&rtxn, hash)
            .map_err(|e| StoreError::Other(e.to_string()))?
            .is_some()
        {
            return Ok(true);
        }

        Ok(self
            .blobs
            .get(&rtxn, hash)
            .map_err(|e| StoreError::Other(e.to_string()))?
            .is_some())
    }

    fn mark_existing_hashes_in_db(
        db: Database<Bytes, Bytes>,
        rtxn: &heed::RoTxn,
        sorted_hashes: &[Hash],
        existing: &mut [bool],
    ) -> Result<(), StoreError> {
        debug_assert_eq!(sorted_hashes.len(), existing.len());
        debug_assert!(sorted_hashes.windows(2).all(|pair| pair[0] <= pair[1]));

        if sorted_hashes.is_empty() {
            return Ok(());
        }

        for (index, hash) in sorted_hashes.iter().enumerate() {
            if existing[index] {
                continue;
            }
            if db.get(rtxn, hash).map_err(map_heed_error)?.is_some() {
                existing[index] = true;
            }
        }

        Ok(())
    }

    /// Mark which sorted hashes already exist using bounded LMDB range scans.
    pub fn existing_hashes_in_sorted_candidates(
        &self,
        sorted_hashes: &[Hash],
    ) -> Result<Vec<bool>, StoreError> {
        let mut existing = vec![false; sorted_hashes.len()];
        if sorted_hashes.is_empty() {
            return Ok(existing);
        }

        let rtxn = self.env.read_txn().map_err(map_heed_error)?;
        Self::mark_existing_hashes_in_db(self.metadata, &rtxn, sorted_hashes, &mut existing)?;
        if existing.iter().all(|exists| *exists) {
            return Ok(existing);
        }
        Self::mark_existing_hashes_in_db(self.blobs, &rtxn, sorted_hashes, &mut existing)?;
        Ok(existing)
    }

    /// Mark exact raw `blobs` keys present without consulting metadata or
    /// requesting LMDB values.
    pub fn existing_blob_keys_in_sorted_candidates(
        &self,
        sorted_hashes: &[Hash],
    ) -> Result<Vec<bool>, StoreError> {
        if sorted_hashes.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(StoreError::Other(
                "raw blob key candidates must be unique and strictly sorted".into(),
            ));
        }
        if sorted_hashes.is_empty() {
            return Ok(Vec::new());
        }
        let rtxn = self.env.read_txn().map_err(map_heed_error)?;
        let keys = sorted_hashes
            .iter()
            .map(|hash| hash.as_slice())
            .collect::<Vec<_>>();
        self.blobs
            .contains_raw_keys_without_data(&rtxn, &keys)
            .map_err(map_heed_error)
    }

    /// Prove exact raw `blobs` keys using one bounded, forward-only cursor.
    ///
    /// Keys already covered by an earlier audit may appear between candidates.
    /// The scan must still reach `inclusive_end` within `max_scanned_keys`
    /// distinct raw keys. LMDB values are never requested.
    pub fn blob_keys_match_bounded_raw_range(
        &self,
        sorted_hashes: &[Hash],
        inclusive_end: Hash,
        max_scanned_keys: usize,
    ) -> Result<bool, StoreError> {
        if sorted_hashes.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(StoreError::Other(
                "bounded raw blob key candidates must be unique and strictly sorted".into(),
            ));
        }
        let Some(last) = sorted_hashes.last() else {
            return Err(StoreError::Other(
                "bounded raw blob key proof requires at least one candidate".into(),
            ));
        };
        if max_scanned_keys == 0 || sorted_hashes.len() > max_scanned_keys {
            return Err(StoreError::Other(
                "bounded raw blob key proof has an invalid scan limit".into(),
            ));
        }
        if *last > inclusive_end {
            return Err(StoreError::Other(
                "bounded raw blob key candidate exceeds the terminal cursor".into(),
            ));
        }

        let rtxn = self.env.read_txn().map_err(map_heed_error)?;
        let keys = sorted_hashes
            .iter()
            .map(|hash| hash.as_slice())
            .collect::<Vec<_>>();
        self.blobs
            .contains_raw_keys_in_bounded_range_without_data(
                &rtxn,
                &keys,
                inclusive_end.as_slice(),
                max_scanned_keys,
            )
            .map_err(map_heed_error)
    }

    pub fn blob_size_sync(&self, hash: &Hash) -> Result<Option<u64>, StoreError> {
        let rtxn = self
            .env
            .read_txn()
            .map_err(|e| StoreError::Other(e.to_string()))?;
        self.blob_size_in_txn(&rtxn, hash)
    }

    pub fn map_size_bytes(&self) -> usize {
        self.env.info().map_size
    }

    pub fn force_sync(&self) -> Result<(), StoreError> {
        self.env.force_sync().map_err(map_heed_error)
    }

    fn total_bytes(&self) -> Result<u64, StoreError> {
        let rtxn = self.env.read_txn().map_err(map_heed_error)?;
        Ok(Self::read_store_totals(self.stats, &rtxn)?
            .unwrap_or_default()
            .total_bytes)
    }

    fn evict_for_write_pressure(&self, incoming_bytes: u64) -> Result<u64, StoreError> {
        let current = self.total_bytes()?;
        if current == 0 {
            return Ok(0);
        }

        let headroom = incoming_bytes.max(current / 10).max(1);
        let target = current.saturating_sub(headroom);
        self.evict_to_target(current, target)
    }

    fn enforce_max_bytes_after_insert(&self, inserted_bytes: u64) -> Result<u64, StoreError> {
        let max = self.max_bytes.load(Ordering::Relaxed);
        if max == 0 || inserted_bytes == 0 {
            return Ok(0);
        }

        let current = self.total_bytes()?;
        if current <= max {
            return Ok(0);
        }

        let target = if inserted_bytes >= max {
            inserted_bytes
        } else {
            max.saturating_mul(9)
                .saturating_div(10)
                .saturating_add(inserted_bytes)
                .min(max)
        };
        self.evict_to_target(current, target)
    }

    fn write_metadata_for_inserted_blobs(
        &self,
        wtxn: &mut heed::RwTxn,
        inserted_entries: &[(Hash, u64)],
    ) -> Result<(), StoreError> {
        if inserted_entries.is_empty() {
            return Ok(());
        }

        let now = unix_timestamp_now();
        let mut inserted_bytes = 0u64;
        let mut inserted_pinned = 0u64;
        let mut inserted_pinned_bytes = 0u64;
        let mut order = self
            .allocate_order_range_in_txn(wtxn, inserted_entries.len())
            .map_err(map_heed_error)?;

        for (hash, data_len) in inserted_entries {
            let meta = Self::encode_blob_meta(BlobMeta {
                order,
                size: *data_len,
                last_accessed_at: now,
            });
            let order_key = Self::encode_order_key(order, hash);
            order = order.saturating_add(1);
            self.metadata
                .put(wtxn, hash, &meta)
                .map_err(map_heed_error)?;
            self.eviction_order
                .put(wtxn, &order_key, &())
                .map_err(map_heed_error)?;
            inserted_bytes = inserted_bytes.saturating_add(*data_len);
            if self
                .read_pin_count_lossy(wtxn, hash)
                .map_err(map_heed_error)?
                > 0
            {
                inserted_pinned = inserted_pinned.saturating_add(1);
                inserted_pinned_bytes = inserted_pinned_bytes.saturating_add(*data_len);
            }
        }

        self.increment_totals_in_txn(
            wtxn,
            inserted_entries.len() as u64,
            inserted_bytes,
            inserted_pinned,
            inserted_pinned_bytes,
        )
        .map_err(map_heed_error)?;
        Ok(())
    }

    fn put_sync_attempt(&self, hash: Hash, data: &[u8]) -> Result<bool, StoreError> {
        let mut wtxn = self.env.write_txn().map_err(map_heed_error)?;
        let external_config = self
            .external_blobs
            .as_ref()
            .filter(|config| data.len() >= config.min_bytes);
        let external_marker = external_config.map(|_| Self::external_blob_ref(&hash));
        let value = external_marker.as_deref().unwrap_or(data);

        match self
            .blobs
            .put_with_flags(&mut wtxn, PutFlags::NO_OVERWRITE, &hash, value)
        {
            Ok(()) => {}
            Err(HeedError::Mdb(MdbError::KeyExist)) => return Ok(false),
            Err(err) => return Err(map_heed_error(err)),
        }

        if let Some(config) = external_config {
            self.write_external_blob(&hash, data, config)?;
        }
        self.write_metadata_for_inserted_blobs(&mut wtxn, &[(hash, data.len() as u64)])?;
        wtxn.commit().map_err(map_heed_error)?;
        Ok(true)
    }

    fn put_many_sync_attempt(
        &self,
        total: usize,
        items: &[(Hash, &[u8])],
    ) -> Result<PutManyReport, StoreError> {
        let mut wtxn = self.env.write_txn().map_err(map_heed_error)?;
        let mut report = PutManyReport {
            total,
            ..PutManyReport::default()
        };
        let mut inserted_entries: Vec<(Hash, u64)> = Vec::new();
        let mut external_blobs: Vec<(Hash, &[u8])> = Vec::new();
        let mut external_pack_entries: Vec<(usize, Hash, &[u8])> = Vec::new();
        let external_config = self.external_blobs.as_ref();

        for (hash, data) in items {
            let external = external_config.filter(|config| data.len() >= config.min_bytes);
            let pack_external = external.is_some_and(|config| config.pack_target_bytes.is_some());
            let reserved_marker;
            let external_marker;
            let value = if pack_external {
                reserved_marker = Self::external_pack_reserved_ref(hash);
                reserved_marker.as_slice()
            } else if external.is_some() {
                external_marker = Self::external_blob_ref(hash);
                external_marker.as_slice()
            } else {
                *data
            };

            match self
                .blobs
                .put_with_flags(&mut wtxn, PutFlags::NO_OVERWRITE, hash, value)
            {
                Ok(()) => {}
                Err(HeedError::Mdb(MdbError::KeyExist)) => continue,
                Err(err) => return Err(map_heed_error(err)),
            }

            let inserted_index = inserted_entries.len();
            let data_len = data.len() as u64;
            inserted_entries.push((*hash, data_len));
            report.inserted = report.inserted.saturating_add(1);
            report.inserted_bytes = report.inserted_bytes.saturating_add(data_len);
            report.inserted_hashes.push(*hash);

            if pack_external {
                external_pack_entries.push((inserted_index, *hash, *data));
            } else if external.is_some() {
                external_blobs.push((*hash, *data));
            }
        }

        if inserted_entries.is_empty() {
            return Ok(report);
        }

        if let Some(config) = external_config {
            for (hash, data) in external_blobs {
                self.write_external_blob(&hash, data, config)?;
            }
            if let Some(pack_target_bytes) = config.pack_target_bytes {
                for (inserted_index, marker) in self.write_external_blob_packs(
                    &external_pack_entries,
                    config,
                    pack_target_bytes,
                )? {
                    let hash = inserted_entries[inserted_index].0;
                    self.blobs
                        .put(&mut wtxn, &hash, &marker)
                        .map_err(map_heed_error)?;
                }
            }
        }

        self.write_metadata_for_inserted_blobs(&mut wtxn, &inserted_entries)?;

        wtxn.commit().map_err(map_heed_error)?;
        Ok(report)
    }

    /// Get storage statistics.
    pub fn stats(&self) -> Result<LmdbStats, StoreError> {
        let rtxn = self
            .env
            .read_txn()
            .map_err(|e| StoreError::Other(e.to_string()))?;
        let totals = Self::read_store_totals(self.stats, &rtxn)?.unwrap_or_default();

        Ok(LmdbStats {
            count: totals.count as usize,
            total_bytes: totals.total_bytes,
            pinned_count: totals.pinned_count as usize,
            pinned_bytes: totals.pinned_bytes,
        })
    }

    /// List all hashes in the store.
    pub fn list(&self) -> Result<Vec<Hash>, StoreError> {
        let rtxn = self
            .env
            .read_txn()
            .map_err(|e| StoreError::Other(e.to_string()))?;

        let mut hashes = Vec::new();
        for item in self.blobs.iter(&rtxn).map_err(map_heed_error)? {
            let (hash, _) = item.map_err(|e| StoreError::Other(e.to_string()))?;
            let hash_arr: Hash = hash
                .try_into()
                .map_err(|_| StoreError::Other("invalid hash length".into()))?;
            hashes.push(hash_arr);
        }

        Ok(hashes)
    }

    /// Scan every raw blob-catalog hash in lexicographic order without
    /// materializing the whole store.
    ///
    /// `after` is exclusive, so callers can persist the final returned hash as a
    /// resumable cursor. The `blobs` database is authoritative for membership:
    /// a partially upgraded legacy store may contain metadata for only a subset
    /// of its blob rows, and selecting `metadata` would silently omit the rest.
    /// LMDB receives a null data pointer, so large inline values' overflow pages
    /// are not resolved or faulted merely to enumerate keys.
    pub fn scan_hashes_after(
        &self,
        after: Option<Hash>,
        limit: usize,
    ) -> Result<Vec<Hash>, StoreError> {
        use std::ops::Bound;
        match after.as_ref() {
            Some(after) => self.scan_hashes_from_bound(Bound::Excluded(after.as_slice()), limit),
            None => self.scan_hashes_from_bound(Bound::Unbounded, limit),
        }
    }

    /// Scan raw blob keys from `start` inclusively without requesting LMDB
    /// values.
    pub fn scan_hashes_from(&self, start: Hash, limit: usize) -> Result<Vec<Hash>, StoreError> {
        self.scan_hashes_from_bound(std::ops::Bound::Included(start.as_slice()), limit)
    }

    fn scan_hashes_from_bound(
        &self,
        start: std::ops::Bound<&[u8]>,
        limit: usize,
    ) -> Result<Vec<Hash>, StoreError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let rtxn = self.env.read_txn().map_err(map_heed_error)?;
        self.blobs
            .raw_keys_from(&rtxn, start, limit)
            .map_err(map_heed_error)?
            .into_iter()
            .map(|hash| {
                hash.as_slice()
                    .try_into()
                    .map_err(|_| StoreError::Other("invalid hash length".into()))
            })
            .collect()
    }

    /// Sync put operation (for use in sync contexts).
    pub fn put_sync(&self, hash: Hash, data: &[u8]) -> Result<bool, StoreError> {
        let incoming_bytes = data.len() as u64;

        let mut retried_after_eviction = false;
        loop {
            match self.put_sync_attempt(hash, data) {
                Ok(inserted) => {
                    if inserted {
                        self.enforce_max_bytes_after_insert(incoming_bytes)?;
                    }
                    return Ok(inserted);
                }
                Err(err) if is_map_full_store_error(&err) && !retried_after_eviction => {
                    let freed = self.evict_for_write_pressure(incoming_bytes)?;
                    if freed == 0 {
                        return Err(err);
                    }
                    retried_after_eviction = true;
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// Sync batch put operation with exact insert accounting.
    pub fn put_many_report_sync(
        &self,
        items: &[(Hash, Vec<u8>)],
    ) -> Result<PutManyReport, StoreError> {
        let borrowed = items
            .iter()
            .map(|(hash, data)| (*hash, data.as_slice()))
            .collect::<Vec<_>>();
        self.put_many_refs_report_sync(&borrowed)
    }

    /// Sync batch put without requiring callers to clone owned blob buffers.
    pub fn put_many_refs_report_sync(
        &self,
        items: &[(Hash, &[u8])],
    ) -> Result<PutManyReport, StoreError> {
        let total = items.len();
        if items.is_empty() {
            return Ok(PutManyReport::default());
        }

        let mut seen_missing = HashSet::new();
        let write_items = items
            .iter()
            .filter_map(|(hash, data)| {
                if !seen_missing.insert(*hash) {
                    None
                } else {
                    Some((*hash, *data))
                }
            })
            .collect::<Vec<_>>();
        let incoming_bytes = write_items
            .iter()
            .map(|(_, data)| data.len() as u64)
            .fold(0u64, |total, size| total.saturating_add(size));

        let mut retried_after_eviction = false;
        loop {
            match self.put_many_sync_attempt(total, &write_items) {
                Ok(report) => {
                    if report.inserted_bytes > 0 {
                        self.enforce_max_bytes_after_insert(report.inserted_bytes)?;
                    }
                    return Ok(report);
                }
                Err(err) if is_map_full_store_error(&err) && !retried_after_eviction => {
                    let freed = self.evict_for_write_pressure(incoming_bytes)?;
                    if freed == 0 {
                        return Err(err);
                    }
                    retried_after_eviction = true;
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// Sync batch put operation (for use in sync contexts).
    pub fn put_many_sync(&self, items: &[(Hash, Vec<u8>)]) -> Result<usize, StoreError> {
        self.put_many_report_sync(items)
            .map(|report| report.inserted)
    }

    /// Sync get operation (for use in sync contexts).
    pub fn get_sync(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        let rtxn = self
            .env
            .read_txn()
            .map_err(|e| StoreError::Other(e.to_string()))?;

        let Some(blob) = self
            .blobs
            .get(&rtxn, hash)
            .map_err(|e| StoreError::Other(e.to_string()))?
        else {
            return Ok(None);
        };

        let expected_size = self
            .metadata
            .get(&rtxn, hash)
            .map_err(|e| StoreError::Other(e.to_string()))?
            .map(Self::decode_blob_meta)
            .transpose()?
            .map(|meta| meta.size);
        self.decode_blob_value(hash, blob, expected_size).map(Some)
    }

    pub fn get_range_sync(
        &self,
        hash: &Hash,
        start: u64,
        end_inclusive: u64,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let rtxn = self
            .env
            .read_txn()
            .map_err(|e| StoreError::Other(e.to_string()))?;
        let Some(blob) = self
            .blobs
            .get(&rtxn, hash)
            .map_err(|e| StoreError::Other(e.to_string()))?
        else {
            return Ok(None);
        };

        if self.is_external_blob_ref(hash, blob) {
            return self.read_external_blob_range(hash, start, end_inclusive);
        }
        if let Some(pack_ref) = Self::decode_external_pack_ref(blob)? {
            return self.read_external_pack_range(&pack_ref, start, end_inclusive);
        }

        if blob.is_empty() || end_inclusive < start {
            return Ok(Some(Vec::new()));
        }
        let len = blob.len() as u64;
        if start >= len {
            return Ok(Some(Vec::new()));
        }

        let actual_end = end_inclusive.min(len - 1);
        let start = usize::try_from(start)
            .map_err(|_| StoreError::Other("blob range start is too large".to_string()))?;
        let end_exclusive = usize::try_from(actual_end.saturating_add(1))
            .map_err(|_| StoreError::Other("blob range end is too large".to_string()))?;
        Ok(Some(blob[start..end_exclusive].to_vec()))
    }

    pub(crate) fn copy_blob_to_sync(
        &self,
        target: &LmdbBlobStore,
        hash: &Hash,
        expected_size: u64,
        chunk_bytes: usize,
    ) -> Result<bool, StoreError> {
        let actual_size = self
            .blob_size_sync(hash)?
            .ok_or_else(|| StoreError::Other("source blob disappeared during move".into()))?;
        if actual_size != expected_size {
            return Err(StoreError::Other(format!(
                "source blob size changed during move: expected {expected_size}, found {actual_size}"
            )));
        }
        if target.blob_size_sync(hash)?.is_some() {
            match target.verify_blob_streaming(hash, expected_size, chunk_bytes) {
                Ok(()) => return Ok(false),
                Err(_) => {
                    target.delete_sync(hash)?;
                }
            }
        }

        let external = target
            .external_blobs
            .as_ref()
            .filter(|config| expected_size >= config.min_bytes as u64);
        if let Some(config) = external {
            return self.copy_blob_to_external_target(
                target,
                hash,
                expected_size,
                chunk_bytes,
                config,
            );
        }

        let capacity = usize::try_from(expected_size)
            .map_err(|_| StoreError::Other("inline move exceeds addressable memory".into()))?;
        let mut data = Vec::new();
        data.try_reserve_exact(capacity)
            .map_err(|error| StoreError::Other(format!("reserve inline move buffer: {error}")))?;
        let actual_hash = self.stream_blob_chunks(hash, expected_size, chunk_bytes, |chunk| {
            data.extend_from_slice(chunk);
            Ok(())
        })?;
        if actual_hash != *hash {
            return Err(StoreError::Other(
                "source returned corrupt bytes during move".into(),
            ));
        }
        let inserted = target.put_sync(*hash, &data)?;
        target.verify_blob_streaming(hash, expected_size, chunk_bytes)?;
        Ok(inserted)
    }

    fn copy_blob_to_external_target(
        &self,
        target: &LmdbBlobStore,
        hash: &Hash,
        expected_size: u64,
        chunk_bytes: usize,
        config: &ExternalBlobConfig,
    ) -> Result<bool, StoreError> {
        if config.pinned_root.is_some() {
            return Err(StoreError::Other(
                "streaming member relocation is disabled while external paths are authority-pinned"
                    .into(),
            ));
        }
        let path = Self::external_blob_path_for_config(config, hash);
        let parent = path
            .parent()
            .ok_or_else(|| StoreError::Other("external blob path has no parent".into()))?;
        fs::create_dir_all(parent)?;
        let temp_path = unique_temp_path(&path);
        let mut file = File::options()
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        let actual_hash = self.stream_blob_chunks(hash, expected_size, chunk_bytes, |chunk| {
            file.write_all(chunk).map_err(StoreError::Io)
        });
        let actual_hash = match actual_hash {
            Ok(actual_hash) => actual_hash,
            Err(error) => {
                drop(file);
                let _ = fs::remove_file(&temp_path);
                return Err(error);
            }
        };
        if actual_hash != *hash {
            drop(file);
            let _ = fs::remove_file(&temp_path);
            return Err(StoreError::Other(
                "source returned corrupt bytes during move".into(),
            ));
        }
        file.flush()?;
        if config.sync {
            file.sync_all()?;
        }
        drop(file);

        let mut wtxn = target.env.write_txn().map_err(map_heed_error)?;
        if target
            .blobs
            .get(&wtxn, hash)
            .map_err(map_heed_error)?
            .is_some()
        {
            drop(wtxn);
            let _ = fs::remove_file(&temp_path);
            target.verify_blob_streaming(hash, expected_size, chunk_bytes)?;
            return Ok(false);
        }
        if path.exists() {
            fs::remove_file(&path)?;
        }
        if let Err(error) = fs::rename(&temp_path, &path) {
            let _ = fs::remove_file(&temp_path);
            return Err(error.into());
        }
        if config.sync {
            File::open(parent)?.sync_all()?;
        }
        let marker = Self::external_blob_ref(hash);
        target
            .blobs
            .put_with_flags(&mut wtxn, PutFlags::NO_OVERWRITE, hash, &marker)
            .map_err(map_heed_error)?;
        target.write_metadata_for_inserted_blobs(&mut wtxn, &[(*hash, expected_size)])?;
        wtxn.commit().map_err(map_heed_error)?;
        if let Err(error) = target.verify_blob_streaming(hash, expected_size, chunk_bytes) {
            let _ = target.delete_sync(hash);
            return Err(error);
        }
        Ok(true)
    }

    pub(crate) fn verify_blob_streaming(
        &self,
        hash: &Hash,
        expected_size: u64,
        chunk_bytes: usize,
    ) -> Result<(), StoreError> {
        let size = self
            .blob_size_sync(hash)?
            .ok_or_else(|| StoreError::Other("target blob disappeared during move".into()))?;
        if size != expected_size {
            return Err(StoreError::Other(format!(
                "target blob size mismatch: expected {expected_size}, found {size}"
            )));
        }
        if self.stream_blob_chunks(hash, size, chunk_bytes, |_| Ok(()))? != *hash {
            return Err(StoreError::Other(
                "target returned corrupt bytes during move".into(),
            ));
        }
        Ok(())
    }

    fn stream_blob_chunks(
        &self,
        hash: &Hash,
        expected_size: u64,
        chunk_bytes: usize,
        mut consume: impl FnMut(&[u8]) -> Result<(), StoreError>,
    ) -> Result<Hash, StoreError> {
        let chunk_bytes = u64::try_from(chunk_bytes.max(1)).unwrap_or(u64::MAX);
        let mut offset = 0u64;
        let mut hasher = Sha256::new();
        while offset < expected_size {
            let end = offset
                .saturating_add(chunk_bytes)
                .min(expected_size)
                .saturating_sub(1);
            let chunk = self
                .get_range_sync(hash, offset, end)?
                .ok_or_else(|| StoreError::Other("blob disappeared during streamed move".into()))?;
            let expected_chunk = end.saturating_sub(offset).saturating_add(1);
            if chunk.len() as u64 != expected_chunk {
                return Err(StoreError::Other(format!(
                    "short streamed blob read: expected {expected_chunk}, found {}",
                    chunk.len()
                )));
            }
            hasher.update(&chunk);
            consume(&chunk)?;
            offset = end.saturating_add(1);
        }
        Ok(hasher.finalize().into())
    }

    pub fn touch_accessed_sync(&self, hash: &Hash, now: u64) -> Result<bool, StoreError> {
        self.touch_many_accessed_sync(std::slice::from_ref(hash), now)
            .map(|updated| updated > 0)
    }

    pub fn touch_many_accessed_sync(&self, hashes: &[Hash], now: u64) -> Result<usize, StoreError> {
        if hashes.is_empty() {
            return Ok(0);
        }

        let mut wtxn = self
            .env
            .write_txn()
            .map_err(|e| StoreError::Other(e.to_string()))?;
        let mut to_touch = Vec::new();

        for hash in hashes {
            let meta = self
                .metadata
                .get(&wtxn, hash)
                .map_err(|e| StoreError::Other(e.to_string()))?
                .map(Self::decode_blob_meta)
                .transpose()?;
            let Some(meta) = meta else {
                continue;
            };

            if meta.last_accessed_at >= now {
                continue;
            }
            to_touch.push((*hash, meta));
        }

        let updated = to_touch.len();
        if updated == 0 {
            wtxn.commit()
                .map_err(|e| StoreError::Other(e.to_string()))?;
            return Ok(0);
        }

        let mut order = self
            .allocate_order_range_in_txn(&mut wtxn, updated)
            .map_err(|e| StoreError::Other(e.to_string()))?;

        for (hash, mut meta) in to_touch {
            let old_order_key = Self::encode_order_key(meta.order, &hash);
            let _ = self
                .eviction_order
                .delete(&mut wtxn, &old_order_key)
                .map_err(|e| StoreError::Other(e.to_string()))?;

            meta.order = order;
            order = order.saturating_add(1);
            meta.last_accessed_at = now;
            let meta_bytes = Self::encode_blob_meta(meta);
            let new_order_key = Self::encode_order_key(meta.order, &hash);
            self.metadata
                .put(&mut wtxn, &hash, &meta_bytes)
                .map_err(|e| StoreError::Other(e.to_string()))?;
            self.eviction_order
                .put(&mut wtxn, &new_order_key, &())
                .map_err(|e| StoreError::Other(e.to_string()))?;
        }

        wtxn.commit()
            .map_err(|e| StoreError::Other(e.to_string()))?;
        Ok(updated)
    }

    pub fn last_accessed_at_sync(&self, hash: &Hash) -> Result<Option<u64>, StoreError> {
        let rtxn = self
            .env
            .read_txn()
            .map_err(|e| StoreError::Other(e.to_string()))?;
        self.metadata
            .get(&rtxn, hash)
            .map_err(|e| StoreError::Other(e.to_string()))?
            .map(Self::decode_blob_meta)
            .transpose()
            .map(|meta| meta.map(|meta| meta.last_accessed_at))
    }

    pub fn many_last_accessed_at_sync(
        &self,
        hashes: &[Hash],
    ) -> Result<Vec<(Hash, u64)>, StoreError> {
        let rtxn = self
            .env
            .read_txn()
            .map_err(|e| StoreError::Other(e.to_string()))?;
        let mut results = Vec::new();
        for hash in hashes {
            let Some(meta) = self
                .metadata
                .get(&rtxn, hash)
                .map_err(|e| StoreError::Other(e.to_string()))?
                .map(Self::decode_blob_meta)
                .transpose()?
            else {
                continue;
            };
            results.push((*hash, meta.last_accessed_at));
        }
        Ok(results)
    }

    /// Sync delete operation (for use in sync contexts).
    pub fn delete_sync(&self, hash: &Hash) -> Result<bool, StoreError> {
        let mut wtxn = self
            .env
            .write_txn()
            .map_err(|e| StoreError::Other(e.to_string()))?;
        let external_path = self.external_blob_path_in_txn(&wtxn, hash)?;
        let (existed, _) = self.delete_blob_in_txn(&mut wtxn, hash)?;

        wtxn.commit()
            .map_err(|e| StoreError::Other(e.to_string()))?;
        if existed {
            self.remove_external_blob_file(external_path);
        }

        Ok(existed)
    }

    /// Delete a batch in one LMDB transaction.
    pub fn delete_many_sync(&self, hashes: &[Hash]) -> Result<usize, StoreError> {
        if hashes.is_empty() {
            return Ok(0);
        }
        let mut wtxn = self
            .env
            .write_txn()
            .map_err(|e| StoreError::Other(e.to_string()))?;
        let mut seen = HashSet::with_capacity(hashes.len());
        let mut external_paths = Vec::new();
        let mut deleted = 0usize;
        for hash in hashes {
            if !seen.insert(*hash) {
                continue;
            }
            let external_path = self.external_blob_path_in_txn(&wtxn, hash)?;
            let (existed, _) = self.delete_blob_in_txn(&mut wtxn, hash)?;
            if existed {
                deleted += 1;
                if let Some(path) = external_path {
                    external_paths.push(path);
                }
            }
        }
        wtxn.commit()
            .map_err(|e| StoreError::Other(e.to_string()))?;
        for path in external_paths {
            self.remove_external_blob_file(Some(path));
        }
        Ok(deleted)
    }

    fn pin_sync(&self, hash: &Hash) -> Result<(), StoreError> {
        let mut wtxn = self
            .env
            .write_txn()
            .map_err(|e| StoreError::Other(e.to_string()))?;
        let previous = self.read_pin_count(&wtxn, hash)?;
        let count = previous.saturating_add(1);
        let encoded = count.to_be_bytes();
        self.pins
            .put(&mut wtxn, hash, &encoded)
            .map_err(|e| StoreError::Other(e.to_string()))?;
        if previous == 0 {
            if let Some(size) = self.blob_size_in_txn(&wtxn, hash)? {
                self.increment_totals_in_txn(&mut wtxn, 0, 0, 1, size)
                    .map_err(|e| StoreError::Other(e.to_string()))?;
            }
        }
        wtxn.commit()
            .map_err(|e| StoreError::Other(e.to_string()))?;
        Ok(())
    }

    fn unpin_sync(&self, hash: &Hash) -> Result<(), StoreError> {
        let mut wtxn = self
            .env
            .write_txn()
            .map_err(|e| StoreError::Other(e.to_string()))?;
        let count = self.read_pin_count(&wtxn, hash)?;
        if count == 1 {
            if let Some(size) = self.blob_size_in_txn(&wtxn, hash)? {
                self.decrement_totals_in_txn(&mut wtxn, 0, 0, 1, size)
                    .map_err(|e| StoreError::Other(e.to_string()))?;
            }
        }
        if count <= 1 {
            let _ = self
                .pins
                .delete(&mut wtxn, hash)
                .map_err(|e| StoreError::Other(e.to_string()))?;
        } else {
            let encoded = (count - 1).to_be_bytes();
            self.pins
                .put(&mut wtxn, hash, &encoded)
                .map_err(|e| StoreError::Other(e.to_string()))?;
        }
        wtxn.commit()
            .map_err(|e| StoreError::Other(e.to_string()))?;
        Ok(())
    }

    fn evict_to_target(&self, current_bytes: u64, target_bytes: u64) -> Result<u64, StoreError> {
        if current_bytes <= target_bytes {
            return Ok(0);
        }

        let rtxn = self
            .env
            .read_txn()
            .map_err(|e| StoreError::Other(e.to_string()))?;
        let order_keys: Vec<Vec<u8>> = self
            .eviction_order
            .iter(&rtxn)
            .map_err(|e| StoreError::Other(e.to_string()))?
            .map(|item| {
                item.map(|(key, _)| key.to_vec())
                    .map_err(|e| StoreError::Other(e.to_string()))
            })
            .collect::<Result<_, _>>()?;
        drop(rtxn);

        let mut freed_total = 0u64;
        let to_free = current_bytes - target_bytes;
        let mut index = 0usize;

        while freed_total < to_free && index < order_keys.len() {
            let mut wtxn = self
                .env
                .write_txn()
                .map_err(|e| StoreError::Other(e.to_string()))?;
            let mut batch_freed = 0u64;
            let mut batch_items = 0usize;
            let mut batch_external_paths = Vec::new();

            while freed_total + batch_freed < to_free && index < order_keys.len() {
                let order_key = &order_keys[index];
                index += 1;

                let hash = Self::decode_hash_from_order_key(order_key)?;
                if self.read_pin_count(&wtxn, &hash)? > 0 {
                    continue;
                }

                let external_path = self.external_blob_path_in_txn(&wtxn, &hash)?;
                let (_, bytes_freed) = self.delete_blob_in_txn(&mut wtxn, &hash)?;
                if bytes_freed == 0 {
                    let _ = self
                        .eviction_order
                        .delete(&mut wtxn, order_key)
                        .map_err(|e| StoreError::Other(e.to_string()))?;
                    continue;
                }

                batch_freed = batch_freed.saturating_add(bytes_freed);
                batch_items += 1;
                if let Some(path) = external_path {
                    batch_external_paths.push(path);
                }

                if batch_freed >= EVICTION_BATCH_TARGET_BYTES
                    || batch_items >= EVICTION_BATCH_MAX_ITEMS
                {
                    break;
                }
            }

            wtxn.commit()
                .map_err(|e| StoreError::Other(e.to_string()))?;
            for path in batch_external_paths {
                let _ = fs::remove_file(path);
            }
            if batch_freed > 0 {
                freed_total = freed_total.saturating_add(batch_freed);
            }
        }

        Ok(freed_total)
    }

    fn delete_blob_in_txn(
        &self,
        wtxn: &mut heed::RwTxn,
        hash: &Hash,
    ) -> Result<(bool, u64), StoreError> {
        let pin_count = self.read_pin_count(wtxn, hash)?;
        let data_len = self
            .blobs
            .get(wtxn, hash)
            .map_err(|e| StoreError::Other(e.to_string()))?
            .map(|data| data.len() as u64);
        let meta = self
            .metadata
            .get(wtxn, hash)
            .map_err(|e| StoreError::Other(e.to_string()))?
            .map(Self::decode_blob_meta)
            .transpose()?;

        let existed = self
            .blobs
            .delete(wtxn, hash)
            .map_err(|e| StoreError::Other(e.to_string()))?;
        let _ = self
            .metadata
            .delete(wtxn, hash)
            .map_err(|e| StoreError::Other(e.to_string()))?;
        let _ = self
            .pins
            .delete(wtxn, hash)
            .map_err(|e| StoreError::Other(e.to_string()))?;
        if let Some(meta) = meta {
            let order_key = Self::encode_order_key(meta.order, hash);
            let _ = self
                .eviction_order
                .delete(wtxn, &order_key)
                .map_err(|e| StoreError::Other(e.to_string()))?;
        }
        let bytes_freed = meta.map(|m| m.size).or(data_len).unwrap_or(0);
        if existed || meta.is_some() {
            self.decrement_totals_in_txn(
                wtxn,
                1,
                bytes_freed,
                u64::from(pin_count > 0),
                if pin_count > 0 { bytes_freed } else { 0 },
            )
            .map_err(|e| StoreError::Other(e.to_string()))?;
        }

        Ok((existed || meta.is_some(), bytes_freed))
    }

    fn blob_size_in_txn(&self, txn: &heed::RoTxn, hash: &Hash) -> Result<Option<u64>, StoreError> {
        if let Some(meta) = self
            .metadata
            .get(txn, hash)
            .map_err(|e| StoreError::Other(e.to_string()))?
            .map(Self::decode_blob_meta)
            .transpose()?
        {
            return Ok(Some(meta.size));
        }

        let Some(value) = self
            .blobs
            .get(txn, hash)
            .map_err(|e| StoreError::Other(e.to_string()))?
        else {
            return Ok(None);
        };
        if self.is_external_blob_ref(hash, value) {
            let config = self.external_blobs.as_ref().ok_or_else(|| {
                StoreError::Other(
                    "external LMDB blob marker found but external blobs are disabled".into(),
                )
            })?;
            let path = Self::external_blob_path_for_config(config, hash);
            return path
                .metadata()
                .map(|metadata| Some(metadata.len()))
                .map_err(StoreError::Io);
        }
        if let Some(pack_ref) = Self::decode_external_pack_ref(value)? {
            return Ok(Some(pack_ref.len));
        }
        Ok(Some(value.len() as u64))
    }

    fn read_pin_count(&self, txn: &heed::RoTxn, hash: &[u8]) -> Result<u32, StoreError> {
        self.pins
            .get(txn, hash)
            .map_err(|e| StoreError::Other(e.to_string()))?
            .map(Self::decode_pin_count)
            .transpose()?
            .map_or(Ok(0), Ok)
    }

    fn read_pin_count_lossy(
        &self,
        txn: &heed::RoTxn,
        hash: &[u8],
    ) -> std::result::Result<u32, HeedError> {
        Ok(self
            .pins
            .get(txn, hash)?
            .and_then(Self::decode_pin_count_lossy)
            .unwrap_or(0))
    }

    fn encode_blob_meta(meta: BlobMeta) -> [u8; BLOB_META_BYTES] {
        let mut encoded = [0u8; BLOB_META_BYTES];
        encoded[..8].copy_from_slice(&meta.order.to_be_bytes());
        encoded[8..16].copy_from_slice(&meta.size.to_be_bytes());
        encoded[16..].copy_from_slice(&meta.last_accessed_at.to_be_bytes());
        encoded
    }

    fn decode_blob_meta(bytes: &[u8]) -> Result<BlobMeta, StoreError> {
        if bytes.len() != LEGACY_BLOB_META_BYTES && bytes.len() != BLOB_META_BYTES {
            return Err(StoreError::Other(format!(
                "invalid blob metadata length: {}",
                bytes.len()
            )));
        }
        Ok(BlobMeta {
            order: Self::decode_order(&bytes[..8])?,
            size: u64::from_be_bytes(
                bytes[8..16]
                    .try_into()
                    .map_err(|_| StoreError::Other("invalid blob size bytes".into()))?,
            ),
            last_accessed_at: if bytes.len() >= BLOB_META_BYTES {
                u64::from_be_bytes(
                    bytes[16..24]
                        .try_into()
                        .map_err(|_| StoreError::Other("invalid blob access time bytes".into()))?,
                )
            } else {
                0
            },
        })
    }

    fn encode_store_totals(totals: StoreTotals) -> [u8; STORE_TOTALS_BYTES] {
        let mut encoded = [0u8; STORE_TOTALS_BYTES];
        encoded[0..8].copy_from_slice(&totals.count.to_be_bytes());
        encoded[8..16].copy_from_slice(&totals.total_bytes.to_be_bytes());
        encoded[16..24].copy_from_slice(&totals.pinned_count.to_be_bytes());
        encoded[24..32].copy_from_slice(&totals.pinned_bytes.to_be_bytes());
        encoded
    }

    fn decode_store_totals(bytes: &[u8]) -> Result<StoreTotals, StoreError> {
        Self::decode_store_totals_lossy(bytes).ok_or_else(|| {
            StoreError::Other(format!("invalid store totals length: {}", bytes.len()))
        })
    }

    fn decode_store_totals_lossy(bytes: &[u8]) -> Option<StoreTotals> {
        if bytes.len() != STORE_TOTALS_BYTES {
            return None;
        }
        Some(StoreTotals {
            count: u64::from_be_bytes(bytes[0..8].try_into().ok()?),
            total_bytes: u64::from_be_bytes(bytes[8..16].try_into().ok()?),
            pinned_count: u64::from_be_bytes(bytes[16..24].try_into().ok()?),
            pinned_bytes: u64::from_be_bytes(bytes[24..32].try_into().ok()?),
        })
    }

    fn encode_next_order(next_order: u64) -> [u8; NEXT_ORDER_BYTES] {
        next_order.to_be_bytes()
    }

    fn decode_next_order(bytes: &[u8]) -> Result<u64, StoreError> {
        Self::decode_next_order_lossy(bytes)
            .ok_or_else(|| StoreError::Other(format!("invalid next order length: {}", bytes.len())))
    }

    fn decode_next_order_lossy(bytes: &[u8]) -> Option<u64> {
        if bytes.len() != NEXT_ORDER_BYTES {
            return None;
        }
        Some(u64::from_be_bytes(bytes.try_into().ok()?))
    }

    fn encode_order_key(order: u64, hash: &Hash) -> [u8; ORDER_KEY_BYTES] {
        let mut key = [0u8; ORDER_KEY_BYTES];
        key[..8].copy_from_slice(&order.to_be_bytes());
        key[8..].copy_from_slice(hash);
        key
    }

    fn decode_order(bytes: &[u8]) -> Result<u64, StoreError> {
        if bytes.len() != 8 {
            return Err(StoreError::Other(format!(
                "invalid order length: {}",
                bytes.len()
            )));
        }
        Ok(u64::from_be_bytes(bytes.try_into().map_err(|_| {
            StoreError::Other("invalid order bytes".into())
        })?))
    }

    fn decode_hash_from_order_key(bytes: &[u8]) -> Result<Hash, StoreError> {
        if bytes.len() != ORDER_KEY_BYTES {
            return Err(StoreError::Other(format!(
                "invalid order key length: {}",
                bytes.len()
            )));
        }
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&bytes[8..]);
        Ok(hash)
    }

    fn decode_pin_count(bytes: &[u8]) -> Result<u32, StoreError> {
        Self::decode_pin_count_lossy(bytes)
            .ok_or_else(|| StoreError::Other(format!("invalid pin count length: {}", bytes.len())))
    }

    fn decode_pin_count_lossy(bytes: &[u8]) -> Option<u32> {
        if bytes.len() != PIN_COUNT_BYTES {
            return None;
        }
        Some(u32::from_be_bytes(bytes.try_into().ok()?))
    }

    fn external_blob_ref(hash: &Hash) -> Vec<u8> {
        let mut marker = Vec::with_capacity(EXTERNAL_BLOB_MARKER_PREFIX.len() + hash.len());
        marker.extend_from_slice(EXTERNAL_BLOB_MARKER_PREFIX);
        marker.extend_from_slice(hash);
        marker
    }

    fn external_pack_reserved_ref(hash: &Hash) -> Vec<u8> {
        let mut marker =
            Vec::with_capacity(EXTERNAL_PACK_RESERVED_MARKER_PREFIX.len() + hash.len());
        marker.extend_from_slice(EXTERNAL_PACK_RESERVED_MARKER_PREFIX);
        marker.extend_from_slice(hash);
        marker
    }

    fn external_pack_blob_ref(
        pack_name: &str,
        offset: u64,
        len: u64,
    ) -> Result<Vec<u8>, StoreError> {
        let name_len = u16::try_from(pack_name.len())
            .map_err(|_| StoreError::Other("external pack file name is too long".to_string()))?;
        let mut marker =
            Vec::with_capacity(EXTERNAL_PACK_MARKER_PREFIX.len() + 2 + pack_name.len() + 16);
        marker.extend_from_slice(EXTERNAL_PACK_MARKER_PREFIX);
        marker.extend_from_slice(&name_len.to_be_bytes());
        marker.extend_from_slice(pack_name.as_bytes());
        marker.extend_from_slice(&offset.to_be_bytes());
        marker.extend_from_slice(&len.to_be_bytes());
        Ok(marker)
    }

    fn is_external_blob_ref(&self, hash: &Hash, value: &[u8]) -> bool {
        value.len() == EXTERNAL_BLOB_MARKER_PREFIX.len() + hash.len()
            && value.starts_with(EXTERNAL_BLOB_MARKER_PREFIX)
            && &value[EXTERNAL_BLOB_MARKER_PREFIX.len()..] == hash
    }

    fn decode_external_pack_ref(value: &[u8]) -> Result<Option<ExternalPackRef>, StoreError> {
        if !value.starts_with(EXTERNAL_PACK_MARKER_PREFIX) {
            return Ok(None);
        }

        let rest = &value[EXTERNAL_PACK_MARKER_PREFIX.len()..];
        if rest.len() < 2 + 8 + 8 {
            return Err(StoreError::Other(
                "invalid external LMDB pack marker length".to_string(),
            ));
        }
        let name_len = u16::from_be_bytes(
            rest[..2]
                .try_into()
                .map_err(|_| StoreError::Other("invalid external pack name length".into()))?,
        ) as usize;
        let expected_len = 2usize.saturating_add(name_len).saturating_add(16);
        if rest.len() != expected_len {
            return Err(StoreError::Other(format!(
                "invalid external LMDB pack marker length: {}",
                value.len()
            )));
        }
        let name_bytes = &rest[2..2 + name_len];
        let name = std::str::from_utf8(name_bytes)
            .map_err(|_| StoreError::Other("external pack name is not UTF-8".into()))?;
        if name.len() < 2
            || name.contains('/')
            || name.contains('\\')
            || name.starts_with('.')
            || name.contains("..")
        {
            return Err(StoreError::Other(
                "external pack name is not a safe relative file name".into(),
            ));
        }
        let offset_start = 2 + name_len;
        let offset = u64::from_be_bytes(
            rest[offset_start..offset_start + 8]
                .try_into()
                .map_err(|_| StoreError::Other("invalid external pack offset".into()))?,
        );
        let len = u64::from_be_bytes(
            rest[offset_start + 8..offset_start + 16]
                .try_into()
                .map_err(|_| StoreError::Other("invalid external pack length".into()))?,
        );
        Ok(Some(ExternalPackRef {
            name: name.to_string(),
            offset,
            len,
        }))
    }

    fn external_blob_path_for_config(config: &ExternalBlobConfig, hash: &Hash) -> PathBuf {
        let hex = to_hex(hash);
        config
            .base_path
            .join(&hex[..2])
            .join(&hex[2..4])
            .join(&hex[4..])
    }

    fn external_blob_path(&self, hash: &Hash) -> Option<PathBuf> {
        self.external_blobs
            .as_ref()
            .map(|config| Self::external_blob_path_for_config(config, hash))
    }

    fn external_pack_path_for_config(config: &ExternalBlobConfig, pack_name: &str) -> PathBuf {
        config
            .base_path
            .join("packs")
            .join(&pack_name[..2])
            .join(pack_name)
    }

    #[cfg(test)]
    fn external_pack_path(&self, pack_ref: &ExternalPackRef) -> Option<PathBuf> {
        self.external_blobs
            .as_ref()
            .map(|config| Self::external_pack_path_for_config(config, &pack_ref.name))
    }

    fn external_pack_name(first_hash: &Hash) -> String {
        let hash_hex = to_hex(first_hash);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let counter = EXTERNAL_PACK_COUNTER.fetch_add(1, Ordering::Relaxed);
        format!(
            "{}-{:032x}-{}-{:016x}.pack",
            &hash_hex[..12],
            nanos,
            std::process::id(),
            counter
        )
    }

    fn write_external_blob(
        &self,
        hash: &Hash,
        data: &[u8],
        config: &ExternalBlobConfig,
    ) -> Result<(), StoreError> {
        if let Some(root) = &config.pinned_root {
            return root.write_blob(
                &ExternalBlobConfig::relative_blob_path(hash),
                data,
                hash,
                config.sync,
            );
        }
        let path = Self::external_blob_path_for_config(config, hash);
        if path.exists() {
            return Ok(());
        }

        let parent = path
            .parent()
            .ok_or_else(|| StoreError::Other("external blob path has no parent".to_string()))?;
        fs::create_dir_all(parent)?;
        let temp_path = unique_temp_path(&path);
        {
            let mut file = File::options()
                .write(true)
                .create_new(true)
                .open(&temp_path)?;
            file.write_all(data)?;
            if config.sync {
                file.sync_all()?;
            }
        }

        if let Err(error) = fs::rename(&temp_path, &path) {
            let _ = fs::remove_file(&temp_path);
            return Err(error.into());
        }
        if config.sync {
            File::open(parent)?.sync_all()?;
        }
        Ok(())
    }

    fn write_external_blob_packs(
        &self,
        entries: &[(usize, Hash, &[u8])],
        config: &ExternalBlobConfig,
        pack_target_bytes: usize,
    ) -> Result<Vec<(usize, Vec<u8>)>, StoreError> {
        let mut markers = Vec::with_capacity(entries.len());
        let mut pack_entries = Vec::new();
        let mut pack_bytes = 0usize;

        for entry in entries {
            let data_len = entry.2.len();
            let would_exceed_target =
                !pack_entries.is_empty() && pack_bytes.saturating_add(data_len) > pack_target_bytes;
            if would_exceed_target {
                markers.extend(self.write_external_blob_pack(&pack_entries, config)?);
                pack_entries.clear();
                pack_bytes = 0;
            }

            pack_entries.push(*entry);
            pack_bytes = pack_bytes.saturating_add(data_len);
        }

        if !pack_entries.is_empty() {
            markers.extend(self.write_external_blob_pack(&pack_entries, config)?);
        }

        Ok(markers)
    }

    fn write_external_blob_pack(
        &self,
        entries: &[(usize, Hash, &[u8])],
        config: &ExternalBlobConfig,
    ) -> Result<Vec<(usize, Vec<u8>)>, StoreError> {
        if entries.is_empty() {
            return Ok(Vec::new());
        }

        let pack_name = Self::external_pack_name(&entries[0].1);
        if let Some(root) = &config.pinned_root {
            let relative = ExternalBlobConfig::relative_pack_path(&pack_name);
            return root.create_file(&relative, config.sync, |file| {
                let mut markers = Vec::with_capacity(entries.len());
                let mut offset = 0u64;
                for (index, _, data) in entries {
                    let len = data.len() as u64;
                    file.write_all(data).map_err(StoreError::Io)?;
                    markers.push((
                        *index,
                        Self::external_pack_blob_ref(&pack_name, offset, len)?,
                    ));
                    offset = offset.saturating_add(len);
                }
                Ok(markers)
            });
        }
        let path = Self::external_pack_path_for_config(config, &pack_name);
        let parent = path
            .parent()
            .ok_or_else(|| StoreError::Other("external pack path has no parent".to_string()))?;
        fs::create_dir_all(parent)?;
        let temp_path = unique_temp_path(&path);
        let mut markers = Vec::with_capacity(entries.len());
        let write_result = (|| -> Result<(), StoreError> {
            let mut file = File::options()
                .write(true)
                .create_new(true)
                .open(&temp_path)?;
            let mut offset = 0u64;
            for (index, _, data) in entries {
                let len = data.len() as u64;
                file.write_all(data)?;
                markers.push((
                    *index,
                    Self::external_pack_blob_ref(&pack_name, offset, len)?,
                ));
                offset = offset.saturating_add(len);
            }
            if config.sync {
                file.sync_all()?;
            }
            Ok(())
        })();

        if let Err(error) = write_result {
            let _ = fs::remove_file(&temp_path);
            return Err(error);
        }
        if let Err(error) = fs::rename(&temp_path, &path) {
            let _ = fs::remove_file(&temp_path);
            return Err(error.into());
        }
        if config.sync {
            File::open(parent)?.sync_all()?;
        }
        Ok(markers)
    }

    fn decode_blob_value(
        &self,
        hash: &Hash,
        value: &[u8],
        expected_size: Option<u64>,
    ) -> Result<Vec<u8>, StoreError> {
        if self.is_external_blob_ref(hash, value) {
            let config = self.external_blobs.as_ref().ok_or_else(|| {
                StoreError::Other(
                    "external LMDB blob marker found but external blobs are disabled".into(),
                )
            })?;
            let mut file = config.open_relative(&ExternalBlobConfig::relative_blob_path(hash))?;
            let expected_size = expected_size.ok_or_else(|| {
                StoreError::Other("external LMDB blob is missing size metadata".into())
            })?;
            let capacity = usize::try_from(expected_size)
                .map_err(|_| StoreError::Other("external LMDB blob size exceeds usize".into()))?;
            let mut data = Vec::with_capacity(capacity.min(1024 * 1024));
            Read::by_ref(&mut file)
                .take(expected_size.saturating_add(1))
                .read_to_end(&mut data)
                .map_err(StoreError::Io)?;
            if data.len() as u64 != expected_size
                || file.metadata().map_err(StoreError::Io)?.len() != expected_size
            {
                return Err(StoreError::Other(
                    "external LMDB blob length differs from committed metadata".into(),
                ));
            }
            return Ok(data);
        }
        if let Some(pack_ref) = Self::decode_external_pack_ref(value)? {
            return self.read_external_pack_blob(&pack_ref);
        }
        Ok(value.to_vec())
    }

    fn read_external_blob_range(
        &self,
        hash: &Hash,
        start: u64,
        end_inclusive: u64,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let config = self.external_blobs.as_ref().ok_or_else(|| {
            StoreError::Other(
                "external LMDB blob marker found but external blobs are disabled".into(),
            )
        })?;
        let mut file = config.open_relative(&ExternalBlobConfig::relative_blob_path(hash))?;
        let len = file.metadata()?.len();
        if len == 0 || start >= len || end_inclusive < start {
            return Ok(Some(Vec::new()));
        }

        let actual_end = end_inclusive.min(len - 1);
        let read_len = actual_end.saturating_sub(start).saturating_add(1);
        let read_len = usize::try_from(read_len)
            .map_err(|_| StoreError::Other("blob range is too large to read".to_string()))?;
        let mut data = vec![0; read_len];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut data)?;
        Ok(Some(data))
    }

    fn read_external_pack_blob(&self, pack_ref: &ExternalPackRef) -> Result<Vec<u8>, StoreError> {
        let read_len = usize::try_from(pack_ref.len).map_err(|_| {
            StoreError::Other("external pack blob is too large to read".to_string())
        })?;
        let config = self.external_blobs.as_ref().ok_or_else(|| {
            StoreError::Other(
                "external LMDB pack marker found but external blobs are disabled".into(),
            )
        })?;
        let mut file =
            config.open_relative(&ExternalBlobConfig::relative_pack_path(&pack_ref.name))?;
        file.seek(SeekFrom::Start(pack_ref.offset))?;
        let mut data = vec![0; read_len];
        file.read_exact(&mut data)?;
        Ok(data)
    }

    fn read_external_pack_range(
        &self,
        pack_ref: &ExternalPackRef,
        start: u64,
        end_inclusive: u64,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        if pack_ref.len == 0 || start >= pack_ref.len || end_inclusive < start {
            return Ok(Some(Vec::new()));
        }

        let actual_end = end_inclusive.min(pack_ref.len - 1);
        let read_len = actual_end.saturating_sub(start).saturating_add(1);
        let read_len = usize::try_from(read_len).map_err(|_| {
            StoreError::Other("external pack blob range is too large to read".to_string())
        })?;
        let config = self.external_blobs.as_ref().ok_or_else(|| {
            StoreError::Other(
                "external LMDB pack marker found but external blobs are disabled".into(),
            )
        })?;
        let mut file =
            config.open_relative(&ExternalBlobConfig::relative_pack_path(&pack_ref.name))?;
        file.seek(SeekFrom::Start(pack_ref.offset.saturating_add(start)))?;
        let mut data = vec![0; read_len];
        file.read_exact(&mut data)?;
        Ok(Some(data))
    }

    fn external_blob_path_for_value(
        &self,
        hash: &Hash,
        value: &[u8],
    ) -> Result<Option<PathBuf>, StoreError> {
        if self.is_external_blob_ref(hash, value) {
            return Ok(self.external_blob_path(hash));
        }
        if Self::decode_external_pack_ref(value)?.is_some() {
            return Ok(None);
        }
        Ok(None)
    }

    fn external_blob_path_in_txn(
        &self,
        txn: &heed::RoTxn,
        hash: &Hash,
    ) -> Result<Option<PathBuf>, StoreError> {
        let Some(value) = self
            .blobs
            .get(txn, hash)
            .map_err(|e| StoreError::Other(e.to_string()))?
        else {
            return Ok(None);
        };
        self.external_blob_path_for_value(hash, value)
    }

    fn remove_external_blob_file(&self, path: Option<PathBuf>) {
        if let Some(path) = path {
            if let Some(config) = &self.external_blobs {
                if let Some(root) = &config.pinned_root {
                    if let Ok(relative) = path.strip_prefix(&config.base_path) {
                        let _ = root.remove_file(relative, config.sync);
                        return;
                    }
                }
            }
            let _ = fs::remove_file(path);
        }
    }
}

fn map_heed_error(error: HeedError) -> StoreError {
    match error {
        HeedError::Io(io_error) => StoreError::Io(io_error),
        other => StoreError::Other(other.to_string()),
    }
}

fn is_map_full_store_error(err: &StoreError) -> bool {
    let message = err.to_string();
    message.contains("MDB_MAP_FULL") || message.contains("MapFull")
}

fn env_bool(name: &str) -> Option<bool> {
    std::env::var(name).ok().and_then(|value| {
        let value = value.trim();
        if value == "1" || value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("yes") {
            Some(true)
        } else if value == "0"
            || value.eq_ignore_ascii_case("false")
            || value.eq_ignore_ascii_case("no")
        {
            Some(false)
        } else {
            None
        }
    })
}

fn env_flags_from_env() -> EnvFlags {
    env_flags_from_bools(
        env_bool(LMDB_NO_READ_AHEAD_ENV).unwrap_or(false),
        env_bool(LMDB_NO_SYNC_ENV).unwrap_or(false),
        env_bool(LMDB_NO_META_SYNC_ENV).unwrap_or(false),
    )
}

fn env_flags_from_bools(no_read_ahead: bool, no_sync: bool, no_meta_sync: bool) -> EnvFlags {
    let mut flags = EnvFlags::empty();
    if no_read_ahead {
        flags |= EnvFlags::NO_READ_AHEAD;
    }
    if no_sync {
        flags |= EnvFlags::NO_SYNC;
    }
    if no_meta_sync {
        flags |= EnvFlags::NO_META_SYNC;
    }
    flags
}

fn unique_temp_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("blob");
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    path.with_file_name(format!(".{file_name}.tmp.{}.{}", std::process::id(), nanos))
}

fn unix_timestamp_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Debug, Clone)]
pub struct LmdbStats {
    pub count: usize,
    pub total_bytes: u64,
    pub pinned_count: usize,
    pub pinned_bytes: u64,
}

#[async_trait]
impl Store for LmdbBlobStore {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        self.put_sync(hash, &data)
    }

    async fn put_many(&self, items: Vec<(Hash, Vec<u8>)>) -> Result<usize, StoreError> {
        self.put_many_sync(&items)
    }

    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        self.get_sync(hash)
    }

    async fn get_range(
        &self,
        hash: &Hash,
        start: u64,
        end_inclusive: u64,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        self.get_range_sync(hash, start, end_inclusive)
    }

    async fn blob_size(&self, hash: &Hash) -> Result<Option<u64>, StoreError> {
        self.blob_size_sync(hash)
    }

    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.exists(hash)
    }

    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.delete_sync(hash)
    }

    async fn delete_many(&self, hashes: Vec<Hash>) -> Result<usize, StoreError> {
        self.delete_many_sync(&hashes)
    }

    fn set_max_bytes(&self, max: u64) {
        self.max_bytes.store(max, Ordering::Relaxed);
    }

    fn max_bytes(&self) -> Option<u64> {
        let max = self.max_bytes.load(Ordering::Relaxed);
        if max > 0 {
            Some(max)
        } else {
            None
        }
    }

    async fn stats(&self) -> StoreStats {
        match self.stats() {
            Ok(stats) => StoreStats {
                count: stats.count as u64,
                bytes: stats.total_bytes,
                pinned_count: stats.pinned_count as u64,
                pinned_bytes: stats.pinned_bytes,
            },
            Err(_) => StoreStats::default(),
        }
    }

    async fn evict_if_needed(&self) -> Result<u64, StoreError> {
        let max = self.max_bytes.load(Ordering::Relaxed);
        if max == 0 {
            return Ok(0);
        }

        let current = self.total_bytes()?;
        if current <= max {
            return Ok(0);
        }

        let target = max * 9 / 10;
        self.evict_to_target(current, target)
    }

    async fn pin(&self, hash: &Hash) -> Result<(), StoreError> {
        self.pin_sync(hash)
    }

    async fn unpin(&self, hash: &Hash) -> Result<(), StoreError> {
        self.unpin_sync(hash)
    }

    fn pin_count(&self, hash: &Hash) -> u32 {
        let Ok(rtxn) = self.env.read_txn() else {
            return 0;
        };
        self.read_pin_count(&rtxn, hash).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashtree_core::sha256;
    use heed::EnvOpenOptions;
    #[cfg(unix)]
    use std::path::{Path, PathBuf};
    #[cfg(unix)]
    use std::process::Command;
    #[cfg(target_os = "macos")]
    use std::sync::atomic::AtomicUsize;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Barrier,
    };
    use std::time::Duration;
    use tempfile::TempDir;

    #[cfg(unix)]
    const STALE_READER_HELPER_ENV: &str = "HASHTREE_LMDB_STALE_READER_HELPER";
    #[cfg(unix)]
    const STALE_READER_HELPER_MODE_ENV: &str = "HASHTREE_LMDB_STALE_READER_HELPER_MODE";
    #[cfg(unix)]
    const STALE_READER_DB_PATH_ENV: &str = "HASHTREE_LMDB_STALE_READER_DB_PATH";
    #[cfg(unix)]
    const STALE_READER_MARKER_PATH_ENV: &str = "HASHTREE_LMDB_STALE_READER_MARKER_PATH";
    #[cfg(unix)]
    const TEST_MAX_READERS: u32 = 4;

    #[cfg(target_os = "linux")]
    fn generated_pinned_external_root(
        path: &Path,
    ) -> Result<(File, Arc<PinnedExternalRoot>), StoreError> {
        std::fs::create_dir(path).map_err(StoreError::Io)?;
        let retained = File::open(path).map_err(StoreError::Io)?;
        let config = ExternalBlobConfig {
            base_path: PathBuf::from(format!("/proc/self/fd/{}", retained.as_raw_fd())),
            min_bytes: 1,
            sync: true,
            pack_target_bytes: None,
            pinned_root: None,
        }
        .prepare_runtime()?;
        Ok((
            retained,
            config
                .pinned_root
                .expect("Linux procfd external root is pinned"),
        ))
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pinned_external_publication_uses_retained_unnamed_file() -> Result<(), StoreError> {
        let temp = TempDir::new().map_err(StoreError::Io)?;
        let root_path = temp.path().join("external");
        let (_retained, root) = generated_pinned_external_root(&root_path)?;
        let data = b"generated pinned O_TMPFILE publication";
        let hash = sha256(data);
        let relative = Path::new("aa").join("bb").join("blob");

        root.write_blob(&relative, data, &hash, true)?;
        root.write_blob(&relative, data, &hash, true)?;

        assert_eq!(
            std::fs::read(root_path.join(&relative)).map_err(StoreError::Io)?,
            data
        );
        assert!(
            std::fs::read_dir(root_path.join("aa").join("bb"))
                .map_err(StoreError::Io)?
                .all(|entry| {
                    !entry
                        .expect("generated external directory entry")
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".hashtree-external.tmp.")
                }),
            "pinned publication must never expose a mutable named temporary"
        );
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pinned_external_intermediate_symlink_cannot_touch_victim() -> Result<(), StoreError> {
        let temp = TempDir::new().map_err(StoreError::Io)?;
        let root_path = temp.path().join("external");
        let victim = temp.path().join("victim");
        std::fs::create_dir(&victim).map_err(StoreError::Io)?;
        let (_retained, root) = generated_pinned_external_root(&root_path)?;
        std::os::unix::fs::symlink(&victim, root_path.join("aa")).map_err(StoreError::Io)?;
        let data = b"generated symlink rejection";
        let hash = sha256(data);

        let error = root
            .write_blob(Path::new("aa/bb/blob"), data, &hash, true)
            .expect_err("pinned external traversal followed an intermediate symlink");
        assert!(
            error.to_string().contains("Too many levels")
                || error.to_string().contains("Not a directory"),
            "unexpected pinned symlink error: {error}"
        );
        assert_eq!(
            std::fs::read_dir(&victim).map_err(StoreError::Io)?.count(),
            0,
            "failed pinned write touched the symlink victim"
        );
        Ok(())
    }

    fn count_files_under(path: &std::path::Path) -> std::io::Result<usize> {
        if !path.exists() {
            return Ok(0);
        }
        let mut count = 0usize;
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            if metadata.is_dir() {
                count += count_files_under(&entry.path())?;
            } else if metadata.is_file() {
                count += 1;
            }
        }
        Ok(count)
    }

    fn persisted_next_order(store: &LmdbBlobStore) -> Result<u64, StoreError> {
        let rtxn = store.env.read_txn().map_err(map_heed_error)?;
        LmdbBlobStore::read_next_order(store.stats, &rtxn)?
            .ok_or_else(|| StoreError::Other("missing persisted next_order".to_string()))
    }

    fn metadata_order(store: &LmdbBlobStore, hash: &Hash) -> Result<u64, StoreError> {
        let rtxn = store.env.read_txn().map_err(map_heed_error)?;
        let meta = store
            .metadata
            .get(&rtxn, hash)
            .map_err(map_heed_error)?
            .ok_or_else(|| StoreError::Other("missing blob metadata".to_string()))?;
        LmdbBlobStore::decode_blob_meta(meta).map(|meta| meta.order)
    }

    fn delete_persisted_next_order(store: &LmdbBlobStore) -> Result<(), StoreError> {
        let mut wtxn = store.env.write_txn().map_err(map_heed_error)?;
        store
            .stats
            .delete(&mut wtxn, NEXT_ORDER_KEY)
            .map_err(map_heed_error)?;
        wtxn.commit().map_err(map_heed_error)
    }

    #[cfg(unix)]
    fn run_helper(mode: &str, path: &Path, marker: &Path) {
        let output = Command::new(std::env::current_exe().expect("test binary path"))
            .arg("--ignored")
            .arg("--exact")
            .arg("tests::lmdb_stale_reader_helper")
            .env(STALE_READER_HELPER_ENV, "1")
            .env(STALE_READER_HELPER_MODE_ENV, mode)
            .env(STALE_READER_DB_PATH_ENV, path)
            .env(STALE_READER_MARKER_PATH_ENV, marker)
            .env("RUST_TEST_THREADS", "1")
            .output()
            .expect("spawn stale-reader helper");

        assert!(
            output.status.success(),
            "stale-reader helper failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            marker.exists(),
            "stale-reader helper did not run: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn env_flags_from_bools_enables_bulk_ingest_flags() {
        let flags = env_flags_from_bools(true, true, true);

        assert!(flags.contains(EnvFlags::NO_READ_AHEAD));
        assert!(flags.contains(EnvFlags::NO_SYNC));
        assert!(flags.contains(EnvFlags::NO_META_SYNC));
    }

    #[test]
    fn sequential_migration_reader_enables_kernel_read_ahead() -> Result<(), StoreError> {
        let temp = TempDir::new().map_err(StoreError::Io)?;
        let sparse_path = temp.path().join("sparse");
        let sequential_path = temp.path().join("sequential");
        drop(LmdbBlobStore::new(&sparse_path)?);
        drop(LmdbBlobStore::new(&sequential_path)?);

        let sparse = LmdbBlobReader::open(&sparse_path, None)?;
        let sparse_flags = sparse
            .store
            .env
            .flags()
            .map_err(map_heed_error)?
            .ok_or_else(|| {
                StoreError::Other("sparse reader has no LMDB environment flags".into())
            })?;
        assert!(sparse_flags.contains(EnvFlags::NO_READ_AHEAD));

        let sequential = LmdbBlobReader::open_with_external_read_concurrency_inner(
            &sequential_path,
            None,
            1,
            None,
            true,
        )?;
        let sequential_flags = sequential
            .store
            .env
            .flags()
            .map_err(map_heed_error)?
            .ok_or_else(|| {
                StoreError::Other("sequential reader has no LMDB environment flags".into())
            })?;
        assert!(!sequential_flags.contains(EnvFlags::NO_READ_AHEAD));
        Ok(())
    }

    #[tokio::test]
    async fn test_put_get() -> Result<(), StoreError> {
        let temp = TempDir::new().unwrap();
        let store = LmdbBlobStore::new(temp.path().join("blobs"))?;

        let data = b"hello lmdb";
        let hash = sha256(data);
        store.put(hash, data.to_vec()).await?;

        assert!(store.has(&hash).await?);
        assert_eq!(store.get(&hash).await?, Some(data.to_vec()));

        Ok(())
    }

    #[test]
    fn external_blob_spill_keeps_large_values_out_of_lmdb() -> Result<(), StoreError> {
        let temp = TempDir::new().unwrap();
        let external_dir = temp.path().join("external");
        let store = LmdbBlobStore::with_map_size_and_settings(
            temp.path().join("blobs"),
            16 * 1024 * 1024,
            EnvFlags::empty(),
            |_| {
                Some(ExternalBlobConfig {
                    base_path: external_dir.clone(),
                    min_bytes: 8,
                    sync: false,
                    pack_target_bytes: None,
                    pinned_root: None,
                })
            },
        )?;

        let small = b"tiny";
        let small_hash = sha256(small);
        assert!(store.put_sync(small_hash, small)?);

        let large = b"large external blob payload";
        let large_hash = sha256(large);
        assert!(store.put_sync(large_hash, large)?);

        assert_eq!(store.get_sync(&small_hash)?, Some(small.to_vec()));
        assert_eq!(store.get_sync(&large_hash)?, Some(large.to_vec()));
        assert_eq!(
            store.get_range_sync(&large_hash, 6, 13)?,
            Some(b"external".to_vec())
        );

        let rtxn = store.env.read_txn().map_err(map_heed_error)?;
        let inline_value = store
            .blobs
            .get(&rtxn, &small_hash)
            .map_err(map_heed_error)?
            .expect("small inline value");
        assert_eq!(inline_value, small);
        let external_value = store
            .blobs
            .get(&rtxn, &large_hash)
            .map_err(map_heed_error)?
            .expect("large marker value");
        assert!(store.is_external_blob_ref(&large_hash, external_value));
        drop(rtxn);

        let external_path = store.external_blob_path(&large_hash).unwrap();
        assert_eq!(std::fs::read(&external_path)?, large);
        let stats = store.stats()?;
        assert_eq!(stats.count, 2);
        assert_eq!(stats.total_bytes, (small.len() + large.len()) as u64);

        assert!(store.delete_sync(&large_hash)?);
        assert!(!external_path.exists());
        Ok(())
    }

    #[test]
    fn external_blob_pack_batches_large_values() -> Result<(), StoreError> {
        let temp = TempDir::new().unwrap();
        let external_dir = temp.path().join("external");
        let store = LmdbBlobStore::with_map_size_and_settings(
            temp.path().join("blobs"),
            16 * 1024 * 1024,
            EnvFlags::empty(),
            |_| {
                Some(ExternalBlobConfig {
                    base_path: external_dir.clone(),
                    min_bytes: 8,
                    sync: true,
                    pack_target_bytes: Some(1024 * 1024),
                    pinned_root: None,
                })
            },
        )?;

        let first = b"first packed blob payload".to_vec();
        let second = b"second packed blob payload".to_vec();
        let tiny = b"tiny".to_vec();
        let first_hash = sha256(&first);
        let second_hash = sha256(&second);
        let tiny_hash = sha256(&tiny);
        let items = vec![
            (first_hash, first.clone()),
            (tiny_hash, tiny.clone()),
            (second_hash, second.clone()),
        ];

        assert_eq!(store.put_many_sync(&items)?, 3);
        assert_eq!(store.get_sync(&first_hash)?, Some(first.clone()));
        assert_eq!(store.get_sync(&second_hash)?, Some(second.clone()));
        assert_eq!(store.get_sync(&tiny_hash)?, Some(tiny.clone()));
        assert_eq!(
            store.get_range_sync(&second_hash, 7, 12)?,
            Some(b"packed".to_vec())
        );

        let rtxn = store.env.read_txn().map_err(map_heed_error)?;
        let first_value = store
            .blobs
            .get(&rtxn, &first_hash)
            .map_err(map_heed_error)?
            .expect("first pack marker");
        let second_value = store
            .blobs
            .get(&rtxn, &second_hash)
            .map_err(map_heed_error)?
            .expect("second pack marker");
        let tiny_value = store
            .blobs
            .get(&rtxn, &tiny_hash)
            .map_err(map_heed_error)?
            .expect("tiny inline value");
        let first_pack =
            LmdbBlobStore::decode_external_pack_ref(first_value)?.expect("first external pack ref");
        let second_pack = LmdbBlobStore::decode_external_pack_ref(second_value)?
            .expect("second external pack ref");
        assert_eq!(first_pack.name, second_pack.name);
        assert_eq!(tiny_value, tiny.as_slice());
        drop(rtxn);

        let pack_path = store.external_pack_path(&first_pack).unwrap();
        assert!(pack_path.exists());
        assert_eq!(
            std::fs::read(&pack_path)?,
            [first.as_slice(), second.as_slice()].concat()
        );
        let pack_count_after_first_write = count_files_under(&external_dir.join("packs"))?;

        assert_eq!(store.put_many_sync(&items)?, 0);
        assert_eq!(
            count_files_under(&external_dir.join("packs"))?,
            pack_count_after_first_write,
            "rewriting an already-present batch must not create orphan external pack files"
        );

        assert!(store.delete_sync(&first_hash)?);
        assert!(pack_path.exists());
        assert_eq!(store.get_sync(&second_hash)?, Some(second));
        Ok(())
    }

    #[test]
    fn terminal_member_audit_rejects_truncated_external_pack() -> Result<(), StoreError> {
        let temp = TempDir::new().unwrap();
        let external_dir = temp.path().join("external");
        let store = LmdbBlobStore::with_map_size_and_settings(
            temp.path().join("blobs"),
            16 * 1024 * 1024,
            EnvFlags::empty(),
            |_| {
                Some(ExternalBlobConfig {
                    base_path: external_dir,
                    min_bytes: 8,
                    sync: true,
                    pack_target_bytes: Some(1024 * 1024),
                    pinned_root: None,
                })
            },
        )?;
        let first = b"first packed audit body".to_vec();
        let second = b"second packed audit body".to_vec();
        let items = vec![(sha256(&first), first), (sha256(&second), second)];
        assert_eq!(store.put_many_sync(&items)?, 2);
        store.validate_exact_member_state()?;

        let rtxn = store.env.read_txn().map_err(map_heed_error)?;
        let marker = store
            .blobs
            .get(&rtxn, &items[0].0)
            .map_err(map_heed_error)?
            .expect("external pack marker");
        let pack =
            LmdbBlobStore::decode_external_pack_ref(marker)?.expect("decoded external pack marker");
        drop(rtxn);
        let pack_path = store.external_pack_path(&pack).expect("external pack path");
        let pack_len = std::fs::metadata(&pack_path)?.len();
        std::fs::OpenOptions::new()
            .write(true)
            .open(pack_path)?
            .set_len(pack_len - 1)?;

        let error = store
            .validate_exact_member_state()
            .expect_err("truncated external pack must fail physical audit");
        assert!(error.to_string().contains("exceeds physical pack size"));
        Ok(())
    }

    #[tokio::test]
    async fn test_delete() -> Result<(), StoreError> {
        let temp = TempDir::new().unwrap();
        let store = LmdbBlobStore::new(temp.path().join("blobs"))?;

        let data = b"delete me";
        let hash = sha256(data);
        store.put(hash, data.to_vec()).await?;
        assert!(store.has(&hash).await?);

        assert!(store.delete(&hash).await?);
        assert!(!store.has(&hash).await?);
        assert!(!store.delete(&hash).await?);

        Ok(())
    }

    #[test]
    fn batch_delete_removes_unique_hashes_in_one_operation() -> Result<(), StoreError> {
        let temp = TempDir::new().unwrap();
        let store = LmdbBlobStore::new(temp.path().join("blobs"))?;
        let first = sha256(b"batch-delete-first");
        let second = sha256(b"batch-delete-second");
        let retained = sha256(b"batch-delete-retained");
        store.put_sync(first, b"batch-delete-first")?;
        store.put_sync(second, b"batch-delete-second")?;
        store.put_sync(retained, b"batch-delete-retained")?;

        assert_eq!(store.delete_many_sync(&[first, second, first])?, 2);
        assert!(!store.exists(&first)?);
        assert!(!store.exists(&second)?);
        assert!(store.exists(&retained)?);
        assert_eq!(store.stats()?.count, 1);
        Ok(())
    }

    #[tokio::test]
    async fn test_list() -> Result<(), StoreError> {
        let temp = TempDir::new().unwrap();
        let store = LmdbBlobStore::new(temp.path().join("blobs"))?;

        let d1 = b"one";
        let d2 = b"two";
        let d3 = b"three";
        let h1 = sha256(d1);
        let h2 = sha256(d2);
        let h3 = sha256(d3);

        store.put(h1, d1.to_vec()).await?;
        store.put(h2, d2.to_vec()).await?;
        store.put(h3, d3.to_vec()).await?;

        let hashes = store.list()?;
        assert_eq!(hashes.len(), 3);
        assert!(hashes.contains(&h1));
        assert!(hashes.contains(&h2));
        assert!(hashes.contains(&h3));

        Ok(())
    }

    #[test]
    fn scan_hashes_after_is_bounded_ordered_and_resumable() -> Result<(), StoreError> {
        let temp = TempDir::new().unwrap();
        let store = LmdbBlobStore::new(temp.path().join("blobs"))?;
        for index in 0..11 {
            let data = format!("cursor blob {index:02}").into_bytes();
            store.put_sync(sha256(&data), &data)?;
        }
        let mut expected = store.list()?;
        expected.sort_unstable();

        let mut actual = Vec::new();
        let mut cursor = None;
        loop {
            let page = store.scan_hashes_after(cursor, 3)?;
            assert!(page.len() <= 3);
            if page.is_empty() {
                break;
            }
            cursor = page.last().copied();
            actual.extend(page);
        }
        assert_eq!(actual, expected);
        assert!(store.scan_hashes_after(cursor, 0)?.is_empty());
        Ok(())
    }

    #[test]
    fn existing_hashes_in_sorted_candidates_marks_present_hashes() -> Result<(), StoreError> {
        let temp = TempDir::new().unwrap();
        let store = LmdbBlobStore::new(temp.path().join("blobs"))?;

        let h1 = sha256(b"range present one");
        let h2 = sha256(b"range present two");
        let missing = sha256(b"range missing");
        store.put_sync(h1, b"range present one")?;
        store.put_sync(h2, b"range present two")?;

        let mut candidates = vec![missing, h2, h1, h1];
        candidates.sort_unstable();
        let existing = store.existing_hashes_in_sorted_candidates(&candidates)?;

        assert_eq!(existing.len(), candidates.len());
        for (hash, exists) in candidates.iter().zip(existing) {
            assert_eq!(exists, *hash == h1 || *hash == h2);
        }

        Ok(())
    }

    #[test]
    fn exact_blob_key_candidates_allow_sparse_overlap_and_require_strict_order(
    ) -> Result<(), StoreError> {
        let temp = TempDir::new().unwrap();
        let store = LmdbBlobStore::new(temp.path().join("blobs"))?;
        let mut entries = [
            b"raw key one".as_slice(),
            b"raw key two".as_slice(),
            b"raw key three".as_slice(),
        ]
        .map(|data| (sha256(data), data));
        entries.sort_unstable_by_key(|(hash, _)| *hash);
        for (hash, data) in entries {
            store.put_sync(hash, data)?;
        }
        let hashes = entries.map(|(hash, _)| hash);

        assert_eq!(
            store.existing_blob_keys_in_sorted_candidates(&[hashes[0], hashes[2]])?,
            vec![true, true],
            "already-covered raw keys may occur between new audit candidates"
        );
        assert!(store.blob_keys_match_bounded_raw_range(&[hashes[0], hashes[2]], hashes[2], 3,)?);
        assert!(
            !store.blob_keys_match_bounded_raw_range(&[hashes[0], hashes[2]], hashes[2], 2,)?,
            "the proof must not scan beyond the checkpoint key count"
        );
        let error = store
            .existing_blob_keys_in_sorted_candidates(&[hashes[0], hashes[0]])
            .expect_err("duplicate candidates must fail closed");
        assert!(error.to_string().contains("unique and strictly sorted"));
        Ok(())
    }

    #[test]
    fn generated_partial_metadata_source_scans_and_audits_every_raw_blob_row(
    ) -> Result<(), StoreError> {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("blobs");
        let external_dir = temp.path().join("external");
        let legacy_data = b"generated legacy loose external blob without metadata";
        let legacy_hash = sha256(legacy_data);
        let new_data = b"new inline";
        let new_hash = sha256(new_data);

        let store = LmdbBlobStore::with_map_size_and_settings(
            &path,
            16 * 1024 * 1024,
            EnvFlags::empty(),
            |_| {
                Some(ExternalBlobConfig {
                    base_path: external_dir.clone(),
                    min_bytes: 16,
                    sync: true,
                    pack_target_bytes: None,
                    pinned_root: None,
                })
            },
        )?;
        store.put_sync(legacy_hash, legacy_data)?;
        let mut wtxn = store.env.write_txn().map_err(map_heed_error)?;
        store
            .metadata
            .delete(&mut wtxn, &legacy_hash)
            .map_err(map_heed_error)?;
        wtxn.commit().map_err(map_heed_error)?;
        store.put_sync(new_hash, new_data)?;
        store.force_sync()?;
        drop(store);

        let mixed_reader = LmdbBlobReader::open(
            &path,
            Some(ExternalBlobOptions {
                base_path: external_dir,
                min_bytes: 1,
                sync: true,
                pack_target_bytes: None,
            }),
        )?;
        let mut expected = vec![legacy_hash, new_hash];
        expected.sort_unstable();
        assert_eq!(mixed_reader.scan_hashes_after(None, 8)?, expected);
        let audit = mixed_reader.validate_terminal_migration_keyset()?;
        assert_eq!(audit.blob_entries, 2);
        assert_eq!(audit.metadata_entries, 1);
        assert_eq!(audit.blob_only_entries, 1);
        assert!(!audit.legacy_blob_only);
        assert_eq!(audit.inline_entries, 1);
        assert_eq!(audit.loose_external_entries, 1);
        assert_eq!(audit.packed_external_entries, 0);
        let mut cursor = None;
        let mut evidence = Vec::new();
        loop {
            let source = crate::migration::audit_lmdb_source_batch_with_max_buffer_bytes(
                &mixed_reader,
                cursor,
                1,
                1024 * 1024,
            )?;
            if source.source_exhausted {
                break;
            }
            assert_eq!(source.scanned, 1);
            assert_eq!(source.verified, 1);
            cursor = source.last_hash;
            evidence.extend(source.verified_source_entries);
        }
        assert_eq!(
            evidence.iter().map(|(hash, _)| *hash).collect::<Vec<_>>(),
            expected
        );

        let pool = PoolStore::open(temp.path().join("pool"), PoolStoreConfig::default())?;
        pool.add_member(PoolMemberConfig::new(
            temp.path().join("pool-member"),
            16 * 1024 * 1024,
        ))?;
        let union = evidence
            .iter()
            .map(|(hash, size)| (*hash, *size, 0usize))
            .collect::<Vec<_>>();
        let migrated =
            crate::migration::reconcile_lmdb_source_union_page_with_max_buffer_bytes_and_authorizer(
                &[&mixed_reader],
                &pool,
                &union,
                1024 * 1024,
                &mut |_, _| Ok(()),
            )?;
        assert_eq!(migrated.scanned, 2);
        assert_eq!(migrated.verified, 2);
        assert_eq!(migrated.inserted, 2);
        assert_eq!(pool.get_sync(&legacy_hash)?, Some(legacy_data.to_vec()));
        assert_eq!(pool.get_sync(&new_hash)?, Some(new_data.to_vec()));
        Ok(())
    }

    #[test]
    fn generated_metadata_without_raw_blob_row_is_rejected() -> Result<(), StoreError> {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("blobs");
        let data = b"generated metadata-only row";
        let hash = sha256(data);
        let store = LmdbBlobStore::new(&path)?;
        store.put_sync(hash, data)?;
        let mut wtxn = store.env.write_txn().map_err(map_heed_error)?;
        store
            .blobs
            .delete(&mut wtxn, &hash)
            .map_err(map_heed_error)?;
        wtxn.commit().map_err(map_heed_error)?;
        store.force_sync()?;
        drop(store);

        let reader = LmdbBlobReader::open(&path, None)?;
        assert_eq!(
            reader.existing_hashes_in_sorted_candidates(&[hash])?,
            vec![true],
            "the legacy metadata-or-blob helper demonstrates why it cannot prove raw keys"
        );
        assert_eq!(
            reader.existing_blob_keys_in_sorted_candidates(&[hash])?,
            vec![false],
            "metadata-only rows must not satisfy exact raw source membership"
        );
        assert!(reader.scan_hashes_from(hash, 1)?.is_empty());
        let error = reader
            .validate_terminal_migration_keyset()
            .expect_err("metadata without a raw blob row must withhold completion");
        assert!(error.to_string().contains("has no raw blob row"));
        Ok(())
    }

    #[test]
    fn key_only_reconciliation_never_reads_an_already_stored_source_value() -> Result<(), StoreError>
    {
        let temp = TempDir::new().unwrap();
        let source_path = temp.path().join("source");
        let valid = vec![0x42; 128 * 1024];
        let hash = sha256(&valid);
        let source = LmdbBlobStore::new(&source_path)?;
        source.put_sync(hash, &valid)?;

        // Keep the authoritative content-addressed source key but corrupt its
        // value at equal length. A reconciliation that touches the source
        // value must fail; the key-only Stored path must not.
        let corrupt = vec![0xa5; valid.len()];
        let mut wtxn = source.env.write_txn().map_err(map_heed_error)?;
        source
            .blobs
            .put(&mut wtxn, &hash, &corrupt)
            .map_err(map_heed_error)?;
        wtxn.commit().map_err(map_heed_error)?;
        source.force_sync()?;
        drop(source);
        let source = LmdbBlobReader::open(&source_path, None)?;

        let pool = PoolStore::open(temp.path().join("pool"), PoolStoreConfig::default())?;
        pool.add_member(PoolMemberConfig::new(
            temp.path().join("pool-member"),
            16 * 1024 * 1024,
        ))?;
        assert!(pool.put_sync(hash, &valid)?);
        pool.force_sync()?;

        let batch =
            crate::migration::reconcile_lmdb_source_batch_with_max_buffer_bytes_and_authorizer(
                &source,
                &pool,
                None,
                1,
                1024 * 1024,
                &mut |_, _| Ok(()),
            )?;
        assert_eq!(batch.scanned, 1);
        assert_eq!(batch.already_present, 1);
        assert_eq!(batch.verified, 0);
        assert_eq!(batch.source_read_groups, 0);
        assert_eq!(batch.source_entries, vec![(hash, valid.len() as u64)]);
        assert_eq!(pool.get_sync(&hash)?, Some(valid));
        Ok(())
    }

    #[test]
    fn key_only_reconciliation_still_hashes_a_missing_target_source_body() -> Result<(), StoreError>
    {
        let temp = TempDir::new().unwrap();
        let source_path = temp.path().join("source");
        let valid = b"a missing target must be recovered from verified source bytes";
        let hash = sha256(valid);
        let source = LmdbBlobStore::new(&source_path)?;
        source.put_sync(hash, valid)?;
        let corrupt = vec![0x3c; valid.len()];
        let mut wtxn = source.env.write_txn().map_err(map_heed_error)?;
        source
            .blobs
            .put(&mut wtxn, &hash, &corrupt)
            .map_err(map_heed_error)?;
        wtxn.commit().map_err(map_heed_error)?;
        source.force_sync()?;
        drop(source);
        let source = LmdbBlobReader::open(&source_path, None)?;

        let pool = PoolStore::open(temp.path().join("pool"), PoolStoreConfig::default())?;
        pool.add_member(PoolMemberConfig::new(
            temp.path().join("pool-member"),
            16 * 1024 * 1024,
        ))?;
        let error =
            crate::migration::reconcile_lmdb_source_batch_with_max_buffer_bytes_and_authorizer(
                &source,
                &pool,
                None,
                1,
                1024 * 1024,
                &mut |_, _| Ok(()),
            )
            .expect_err("missing target must not accept corrupt source bytes");
        assert!(error
            .to_string()
            .contains("differs from its content-addressed key"));
        assert_eq!(pool.get_sync(&hash)?, None);
        Ok(())
    }

    #[test]
    fn terminal_source_key_audit_is_streaming_and_order_bound() -> Result<(), StoreError> {
        let mut hashes = vec![
            sha256(b"terminal source key one"),
            sha256(b"terminal source key two"),
            sha256(b"terminal source key three"),
        ];
        hashes.sort_unstable();
        let mut paged = LmdbSourceKeyAuditBuilder::new();
        paged.append_sorted(&hashes[..1])?;
        paged.append_sorted(&hashes[1..])?;
        let paged = paged.finish();

        let mut single = LmdbSourceKeyAuditBuilder::new();
        single.append_sorted(&hashes)?;
        assert_eq!(paged, single.finish());
        assert_eq!(paged.blob_entries, 3);

        let mut invalid = LmdbSourceKeyAuditBuilder::new();
        invalid.append_sorted(&hashes[1..])?;
        assert!(invalid
            .append_sorted(&hashes[..1])
            .expect_err("global key order must be strict")
            .to_string()
            .contains("globally unique"));
        Ok(())
    }

    #[tokio::test]
    async fn test_blob_last_accessed_persists_and_updates_eviction_order() -> Result<(), StoreError>
    {
        let temp = TempDir::new().unwrap();
        let store = LmdbBlobStore::new(temp.path().join("blobs"))?;

        let first = sha256(b"first");
        let second = sha256(b"second");
        store.put(first, b"first".to_vec()).await?;
        store.put(second, b"second".to_vec()).await?;

        assert!(store.last_accessed_at_sync(&first)?.unwrap_or(0) > 0);
        let access_time = unix_timestamp_now().saturating_add(1000);
        store.touch_accessed_sync(&first, access_time)?;

        assert_eq!(store.last_accessed_at_sync(&first)?, Some(access_time));
        assert!(store.delete_sync(&first)?);
        assert!(store.exists(&second)?);

        Ok(())
    }

    #[test]
    fn duplicate_put_is_noop_and_preserves_blob_last_accessed() -> Result<(), StoreError> {
        let temp = TempDir::new().unwrap();
        let store = LmdbBlobStore::new(temp.path().join("blobs"))?;

        let data = b"already here";
        let hash = sha256(data);
        assert!(store.put_sync(hash, data)?);
        let accessed = store.last_accessed_at_sync(&hash)?;
        let next_order = persisted_next_order(&store)?;

        assert!(!store.put_sync(hash, data)?);
        assert_eq!(store.last_accessed_at_sync(&hash)?, accessed);
        assert_eq!(persisted_next_order(&store)?, next_order);

        let stats = store.stats()?;
        assert_eq!(stats.count, 1);
        assert_eq!(stats.total_bytes, data.len() as u64);

        Ok(())
    }

    #[test]
    fn next_order_persists_across_reopen_and_counts_only_new_blobs() -> Result<(), StoreError> {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("blobs");
        let first = sha256(b"order first");
        let second = sha256(b"order second");
        let third = sha256(b"order third");

        {
            let store = LmdbBlobStore::new(&path)?;
            assert!(store.put_sync(first, b"order first")?);
            assert!(store.put_sync(second, b"order second")?);
            assert_eq!(metadata_order(&store, &first)?, 0);
            assert_eq!(metadata_order(&store, &second)?, 1);
            assert_eq!(persisted_next_order(&store)?, 2);

            assert!(!store.put_sync(first, b"order first")?);
            assert_eq!(
                store.put_many_sync(&[(second, b"order second".to_vec())])?,
                0
            );
            assert_eq!(persisted_next_order(&store)?, 2);
        }

        let reopened = LmdbBlobStore::new(&path)?;
        assert_eq!(persisted_next_order(&reopened)?, 2);
        assert!(reopened.put_sync(third, b"order third")?);
        assert_eq!(metadata_order(&reopened, &third)?, 2);
        assert_eq!(persisted_next_order(&reopened)?, 3);

        Ok(())
    }

    #[test]
    fn legacy_store_without_next_order_seeds_high_counter_without_tail_scan(
    ) -> Result<(), StoreError> {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("blobs");
        let existing = sha256(b"legacy existing");
        let new = sha256(b"legacy new");

        {
            let store = LmdbBlobStore::new(&path)?;
            assert!(store.put_sync(existing, b"legacy existing")?);
            delete_persisted_next_order(&store)?;
        }

        let reopened = LmdbBlobStore::new(&path)?;
        let migrated_next_order = persisted_next_order(&reopened)?;
        assert!(migrated_next_order >= NEXT_ORDER_MIGRATION_FLOOR);
        assert!(reopened.put_sync(new, b"legacy new")?);
        assert_eq!(metadata_order(&reopened, &new)?, migrated_next_order);
        assert_eq!(
            persisted_next_order(&reopened)?,
            migrated_next_order.saturating_add(1)
        );

        Ok(())
    }

    #[test]
    fn put_many_report_counts_only_new_hashes_and_bytes() -> Result<(), StoreError> {
        let temp = TempDir::new().unwrap();
        let store = LmdbBlobStore::new(temp.path().join("blobs"))?;

        let existing = b"existing";
        let existing_hash = sha256(existing);
        assert!(store.put_sync(existing_hash, existing)?);

        let new_one = b"new one";
        let new_two = b"new two";
        let new_one_hash = sha256(new_one);
        let new_two_hash = sha256(new_two);
        let items = vec![
            (existing_hash, existing.to_vec()),
            (new_one_hash, new_one.to_vec()),
            (new_one_hash, new_one.to_vec()),
            (new_two_hash, new_two.to_vec()),
        ];

        let report = store.put_many_report_sync(&items)?;

        assert_eq!(report.total, 4);
        assert_eq!(report.inserted, 2);
        assert_eq!(
            report.inserted_bytes,
            (new_one.len() + new_two.len()) as u64
        );
        assert_eq!(report.inserted_hashes, vec![new_one_hash, new_two_hash]);
        assert_eq!(store.put_many_sync(&items)?, 0);
        assert_eq!(
            store.stats()?.total_bytes,
            (existing.len() + new_one.len() + new_two.len()) as u64
        );

        Ok(())
    }

    #[test]
    fn duplicate_heavy_batch_does_not_evict_by_candidate_bytes() -> Result<(), StoreError> {
        let temp = TempDir::new().unwrap();
        let store = LmdbBlobStore::new(temp.path().join("blobs"))?;
        store.set_max_bytes(35);

        let first = [1u8; 10];
        let second = [2u8; 10];
        let third = [3u8; 10];
        let new = [4u8; 5];
        let first_hash = sha256(&first);
        let second_hash = sha256(&second);
        let third_hash = sha256(&third);
        let new_hash = sha256(&new);
        assert!(store.put_sync(first_hash, &first)?);
        assert!(store.put_sync(second_hash, &second)?);
        assert!(store.put_sync(third_hash, &third)?);
        assert_eq!(store.stats()?.total_bytes, 30);

        let report = store.put_many_report_sync(&[
            (first_hash, first.to_vec()),
            (second_hash, second.to_vec()),
            (new_hash, new.to_vec()),
        ])?;

        assert_eq!(report.inserted, 1);
        assert_eq!(report.inserted_bytes, 5);
        assert_eq!(store.stats()?.total_bytes, 35);
        assert!(store.exists(&first_hash)?);
        assert!(store.exists(&second_hash)?);
        assert!(store.exists(&third_hash)?);
        assert!(store.exists(&new_hash)?);

        Ok(())
    }

    #[test]
    fn test_decodes_legacy_blob_metadata_without_access_time() -> Result<(), StoreError> {
        let mut encoded = [0u8; LEGACY_BLOB_META_BYTES];
        encoded[..8].copy_from_slice(&7u64.to_be_bytes());
        encoded[8..].copy_from_slice(&42u64.to_be_bytes());

        let meta = LmdbBlobStore::decode_blob_meta(&encoded)?;

        assert_eq!(meta.order, 7);
        assert_eq!(meta.size, 42);
        assert_eq!(meta.last_accessed_at, 0);
        Ok(())
    }

    #[tokio::test]
    async fn test_stats() -> Result<(), StoreError> {
        let temp = TempDir::new().unwrap();
        let store = LmdbBlobStore::new(temp.path().join("blobs"))?;

        let d1 = b"hello";
        let d2 = b"world";
        store.put(sha256(d1), d1.to_vec()).await?;
        store.put(sha256(d2), d2.to_vec()).await?;

        let stats = store.stats()?;
        assert_eq!(stats.count, 2);
        assert_eq!(stats.total_bytes, 10);

        Ok(())
    }

    #[tokio::test]
    async fn test_stats_persist_across_reopen_and_mutations() -> Result<(), StoreError> {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("blobs");
        let h1 = sha256(b"hello");
        let h2 = sha256(b"world!");
        let h3 = sha256(b"prepin");

        {
            let store = LmdbBlobStore::new(&path)?;
            store.put(h1, b"hello".to_vec()).await?;
            store.put(h2, b"world!".to_vec()).await?;
            store.pin(&h1).await?;
            store.pin(&h3).await?;
            store.put(h3, b"prepin".to_vec()).await?;

            let stats = store.stats()?;
            assert_eq!(stats.count, 3);
            assert_eq!(stats.total_bytes, 17);
            assert_eq!(stats.pinned_count, 2);
            assert_eq!(stats.pinned_bytes, 11);
        }

        {
            let reopened = LmdbBlobStore::new(&path)?;
            let stats = reopened.stats()?;
            assert_eq!(stats.count, 3);
            assert_eq!(stats.total_bytes, 17);
            assert_eq!(stats.pinned_count, 2);
            assert_eq!(stats.pinned_bytes, 11);

            assert!(reopened.delete(&h1).await?);
            let stats = reopened.stats()?;
            assert_eq!(stats.count, 2);
            assert_eq!(stats.total_bytes, 12);
            assert_eq!(stats.pinned_count, 1);
            assert_eq!(stats.pinned_bytes, 6);
        }

        let reopened = LmdbBlobStore::new(&path)?;
        let stats = reopened.stats()?;
        assert_eq!(stats.count, 2);
        assert_eq!(stats.total_bytes, 12);
        assert_eq!(stats.pinned_count, 1);
        assert_eq!(stats.pinned_bytes, 6);

        Ok(())
    }

    #[tokio::test]
    async fn test_deduplication() -> Result<(), StoreError> {
        let temp = TempDir::new().unwrap();
        let store = LmdbBlobStore::new(temp.path().join("blobs"))?;

        let data = b"same";
        let hash = sha256(data);
        assert!(store.put(hash, data.to_vec()).await?); // Returns true (newly stored)
        assert!(!store.put(hash, data.to_vec()).await?); // Returns false (already existed)

        assert_eq!(store.list()?.len(), 1);

        Ok(())
    }

    #[tokio::test]
    async fn test_max_bytes() -> Result<(), StoreError> {
        let temp = TempDir::new().unwrap();
        let store = LmdbBlobStore::new(temp.path().join("blobs"))?;

        assert!(store.max_bytes().is_none());

        store.set_max_bytes(1000);
        assert_eq!(store.max_bytes(), Some(1000));

        store.set_max_bytes(0);
        assert!(store.max_bytes().is_none());

        Ok(())
    }

    #[test]
    fn test_with_max_bytes_expands_lmdb_map_size() -> Result<(), StoreError> {
        let temp = TempDir::new().unwrap();
        let requested = (DEFAULT_MAP_SIZE as u64) + 64 * 1024 * 1024;
        let store = LmdbBlobStore::with_max_bytes(temp.path().join("blobs"), requested)?;

        assert!(
            store.map_size_bytes() as u64 >= requested,
            "expected LMDB map to grow to at least {requested} bytes, got {}",
            store.map_size_bytes()
        );
        assert_eq!(store.max_bytes(), Some(requested));

        Ok(())
    }

    #[tokio::test]
    async fn test_eviction_over_limit() -> Result<(), StoreError> {
        let temp = TempDir::new().unwrap();
        let store = LmdbBlobStore::new(temp.path().join("blobs"))?;
        store.set_max_bytes(25);

        let h1 = sha256(b"aaaaaaaaaa");
        let h2 = sha256(b"bbbbbbbbbb");
        let h3 = sha256(b"cccccccccc");

        store.put(h1, b"aaaaaaaaaa".to_vec()).await?;
        store.put(h2, b"bbbbbbbbbb".to_vec()).await?;
        store.put(h3, b"cccccccccc".to_vec()).await?;

        let freed = store.evict_if_needed().await?;
        assert_eq!(
            freed, 0,
            "write path should have already evicted stale blobs"
        );

        assert!(
            !store.has(&h1).await?,
            "oldest blob should be evicted before the third write"
        );
        assert!(store.has(&h2).await?);
        assert!(store.has(&h3).await?);

        let stats = store.stats()?;
        assert!(
            stats.total_bytes <= 22,
            "store should be reduced to 90% target"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_eviction_respects_pins() -> Result<(), StoreError> {
        let temp = TempDir::new().unwrap();
        let store = LmdbBlobStore::new(temp.path().join("blobs"))?;
        store.set_max_bytes(25);

        let h1 = sha256(b"aaaaaaaaaa");
        let h2 = sha256(b"bbbbbbbbbb");
        let h3 = sha256(b"cccccccccc");

        store.put(h1, b"aaaaaaaaaa".to_vec()).await?;
        store.put(h2, b"bbbbbbbbbb".to_vec()).await?;
        store.pin(&h1).await?;
        store.put(h3, b"cccccccccc".to_vec()).await?;

        let freed = store.evict_if_needed().await?;
        assert_eq!(
            freed, 0,
            "write path should have already evicted stale blobs"
        );

        assert!(store.has(&h1).await?, "pinned blob must not be evicted");
        assert!(
            !store.has(&h2).await?,
            "oldest unpinned blob should be evicted before the third write"
        );
        assert!(store.has(&h3).await?);

        Ok(())
    }

    #[tokio::test]
    async fn test_reopen_with_existing_eviction_order() -> Result<(), StoreError> {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("blobs");

        {
            let store = LmdbBlobStore::new(&path)?;
            let h1 = sha256(b"aaaaaaaaaa");
            let h2 = sha256(b"bbbbbbbbbb");
            store.put(h1, b"aaaaaaaaaa".to_vec()).await?;
            store.put(h2, b"bbbbbbbbbb".to_vec()).await?;
        }

        let reopened = LmdbBlobStore::new(&path)?;
        let h3 = sha256(b"cccccccccc");
        assert!(reopened.put(h3, b"cccccccccc".to_vec()).await?);
        assert!(reopened.has(&h3).await?);

        Ok(())
    }

    #[tokio::test]
    async fn test_reopen_with_lower_max_bytes_evicts_existing_blobs() -> Result<(), StoreError> {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("blobs");

        {
            let store = LmdbBlobStore::new(&path)?;
            let h1 = sha256(b"aaaaaaaaaa");
            let h2 = sha256(b"bbbbbbbbbb");
            let h3 = sha256(b"cccccccccc");
            store.put(h1, b"aaaaaaaaaa".to_vec()).await?;
            store.put(h2, b"bbbbbbbbbb".to_vec()).await?;
            store.put(h3, b"cccccccccc".to_vec()).await?;
        }

        let reopened = LmdbBlobStore::with_max_bytes(&path, 25)?;
        let h1 = sha256(b"aaaaaaaaaa");
        let h2 = sha256(b"bbbbbbbbbb");
        let h3 = sha256(b"cccccccccc");

        assert!(
            !reopened.has(&h1).await?,
            "oldest blob should be evicted when reopening over the new cap"
        );
        assert!(reopened.has(&h2).await?);
        assert!(reopened.has(&h3).await?);

        let stats = reopened.stats()?;
        assert!(
            stats.total_bytes <= 22,
            "reopened store should be reduced to the 90% target"
        );

        Ok(())
    }

    #[test]
    fn test_supports_many_concurrent_readers() -> Result<(), Box<dyn std::error::Error>> {
        const READER_THREADS: usize = 160;

        let temp = TempDir::new()?;
        let store = Arc::new(LmdbBlobStore::new(temp.path().join("blobs"))?);
        let hash = sha256(b"many readers");
        store.put_sync(hash, b"many readers")?;

        let start = Arc::new(Barrier::new(READER_THREADS + 1));
        let release = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::with_capacity(READER_THREADS);

        for _ in 0..READER_THREADS {
            let env = heed::Env::clone(&store.env);
            let start = Arc::clone(&start);
            let release = Arc::clone(&release);
            handles.push(std::thread::spawn(move || -> Result<(), String> {
                start.wait();
                let _rtxn = env.read_txn().map_err(|err| err.to_string())?;
                while !release.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Ok(())
            }));
        }

        start.wait();
        std::thread::sleep(Duration::from_millis(50));
        release.store(true, Ordering::Relaxed);

        let results: Vec<Result<(), String>> = handles
            .into_iter()
            .map(|handle| handle.join().expect("reader thread panicked"))
            .collect();

        let failures: Vec<String> = results.into_iter().filter_map(Result::err).collect();
        assert!(
            failures.is_empty(),
            "concurrent reader failures: {}",
            failures.join(" | ")
        );
        assert!(store.exists(&hash)?);

        Ok(())
    }

    #[test]
    fn managed_environment_closes_after_last_store_handle() -> Result<(), Box<dyn std::error::Error>>
    {
        let temp = TempDir::new()?;
        let path = temp.path().join("blobs");
        let first = LmdbBlobStore::new(&path)?;
        let hash = sha256(b"shared environment lifecycle");
        first.put_sync(hash, b"shared environment lifecycle")?;
        let second = LmdbBlobStore::new(&path)?;
        let canonical = std::fs::canonicalize(&path)?;

        drop(first);
        assert!(
            heed::env_closing_event(&canonical).is_some(),
            "the environment must remain open while another managed store owns it"
        );
        assert_eq!(
            second.get_sync(&hash)?,
            Some(b"shared environment lifecycle".to_vec())
        );

        drop(second);
        assert!(
            heed::env_closing_event(&canonical).is_none(),
            "the last managed store must remove Heed's cached environment"
        );
        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn managed_writers_do_not_exhaust_darwin_sem_undo_slots(
    ) -> Result<(), Box<dyn std::error::Error>> {
        const WRITERS: usize = 16;

        let temp = TempDir::new()?;
        let stores = (0..WRITERS)
            .map(|index| LmdbBlobStore::new(temp.path().join(format!("member-{index}"))))
            .collect::<Result<Vec<_>, _>>()?;
        let start = Arc::new(Barrier::new(WRITERS));
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let handles = stores
            .into_iter()
            .map(|store| {
                let start = Arc::clone(&start);
                let active = Arc::clone(&active);
                let peak = Arc::clone(&peak);
                std::thread::spawn(move || -> Result<(), String> {
                    start.wait();
                    let txn = store.env.write_txn().map_err(|error| error.to_string())?;
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(75));
                    active.fetch_sub(1, Ordering::SeqCst);
                    drop(txn);
                    Ok(())
                })
            })
            .collect::<Vec<_>>();

        let failures = handles
            .into_iter()
            .filter_map(|handle| handle.join().expect("writer thread panicked").err())
            .collect::<Vec<_>>();
        assert!(
            failures.is_empty(),
            "Darwin SEM_UNDO exhaustion rejected managed writers: {}",
            failures.join(" | ")
        );
        assert!(peak.load(Ordering::SeqCst) > 1);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_reclaims_stale_reader_slots() -> Result<(), Box<dyn std::error::Error>> {
        let temp = TempDir::new()?;
        let path = temp.path().join("blobs");
        let data = b"hello stale readers";
        let hash = sha256(data);

        run_helper("setup", &path, &temp.path().join("setup.marker"));

        for index in 0..TEST_MAX_READERS {
            let marker = temp.path().join(format!("helper-{index}.marker"));
            run_helper("stale", &path, &marker);
        }

        let store = LmdbBlobStore::with_map_size(&path, 1024 * 1024)?;
        assert!(store.exists(&hash)?);

        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_reopens_existing_env_with_larger_map_size() -> Result<(), Box<dyn std::error::Error>> {
        let temp = TempDir::new()?;
        let path = temp.path().join("blobs");
        run_helper("small-map", &path, &temp.path().join("small-map.marker"));

        let reopened = LmdbBlobStore::with_map_size(&path, 8 * 1024 * 1024)?;
        assert!(reopened.map_size_bytes() >= 8 * 1024 * 1024);

        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_reopens_existing_env_with_smaller_requested_map_size(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let temp = TempDir::new()?;
        let path = temp.path().join("blobs");
        let hash = sha256(b"large existing blob");
        run_helper("large-map", &path, &temp.path().join("large-map.marker"));

        let existing_size = std::fs::metadata(path.join("data.mdb"))?.len();
        assert!(
            existing_size > 1024 * 1024,
            "test setup should create an environment larger than the reopen request"
        );

        let reopened = LmdbBlobStore::with_map_size(&path, 1024 * 1024)?;
        assert!(
            reopened.map_size_bytes() as u64 >= existing_size,
            "expected map size to cover existing data.mdb size {existing_size}, got {}",
            reopened.map_size_bytes()
        );
        assert!(reopened.exists(&hash)?);

        Ok(())
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "used as a subprocess helper by test_reclaims_stale_reader_slots"]
    fn lmdb_stale_reader_helper() {
        let Some(db_path) = std::env::var_os(STALE_READER_DB_PATH_ENV) else {
            return;
        };
        let marker_path =
            PathBuf::from(std::env::var_os(STALE_READER_MARKER_PATH_ENV).expect("marker path"));
        std::fs::write(&marker_path, b"started").expect("write helper marker");

        let _env_flag = std::env::var_os(STALE_READER_HELPER_ENV).expect("helper mode enabled");
        let mode = std::env::var(STALE_READER_HELPER_MODE_ENV).expect("helper mode");
        let db_path = PathBuf::from(db_path);
        std::fs::create_dir_all(&db_path).expect("create helper db dir");
        let helper_map_size = if mode == "large-map" {
            8 * 1024 * 1024
        } else {
            1024 * 1024
        };
        let env = unsafe {
            EnvOpenOptions::new()
                .map_size(helper_map_size)
                .max_dbs(DATABASE_COUNT)
                .max_readers(TEST_MAX_READERS)
                .open(&db_path)
                .expect("open lmdb env")
        };
        match mode.as_str() {
            "setup" => {
                let mut wtxn = env.write_txn().expect("open write txn");
                let blobs: Database<Bytes, Bytes> = env
                    .create_database(&mut wtxn, Some("blobs"))
                    .expect("create blobs database");
                let data = b"hello stale readers";
                let hash = sha256(data);
                blobs.put(&mut wtxn, &hash, data).expect("seed blob");
                wtxn.commit().expect("commit setup txn");
                std::process::exit(0);
            }
            "stale" => {
                let _rtxn = env.read_txn().expect("open read txn");
                std::process::exit(0);
            }
            "small-map" => {
                let mut wtxn = env.write_txn().expect("open write txn");
                let _blobs: Database<Bytes, Bytes> = env
                    .create_database(&mut wtxn, Some("blobs"))
                    .expect("create blobs database");
                let _metadata: Database<Bytes, Bytes> = env
                    .create_database(&mut wtxn, Some("metadata"))
                    .expect("create metadata database");
                let _eviction_order: Database<Bytes, Unit> = env
                    .create_database(&mut wtxn, Some("eviction_order"))
                    .expect("create eviction_order database");
                let _pins: Database<Bytes, Bytes> = env
                    .create_database(&mut wtxn, Some("pins"))
                    .expect("create pins database");
                wtxn.commit().expect("commit small-map setup txn");
                std::process::exit(0);
            }
            "large-map" => {
                let mut wtxn = env.write_txn().expect("open write txn");
                let blobs: Database<Bytes, Bytes> = env
                    .create_database(&mut wtxn, Some("blobs"))
                    .expect("create blobs database");
                let _metadata: Database<Bytes, Bytes> = env
                    .create_database(&mut wtxn, Some("metadata"))
                    .expect("create metadata database");
                let _eviction_order: Database<Bytes, Unit> = env
                    .create_database(&mut wtxn, Some("eviction_order"))
                    .expect("create eviction_order database");
                let _pins: Database<Bytes, Bytes> = env
                    .create_database(&mut wtxn, Some("pins"))
                    .expect("create pins database");
                let data = vec![42u8; 3 * 1024 * 1024];
                let hash = sha256(b"large existing blob");
                blobs.put(&mut wtxn, &hash, &data).expect("seed blob");
                wtxn.commit().expect("commit large-map setup txn");
                std::process::exit(0);
            }
            other => panic!("unknown helper mode: {other}"),
        }
    }
}
