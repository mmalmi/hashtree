use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use hashtree_cli::storage::{
    acquire_existing_profile_repair_retention_guard, ProfileRepairRetentionLease,
    PROFILE_REPAIR_RETENTION_LEASE_RELATIVE_PATH,
};
use hashtree_core::{from_hex, to_hex, types::Hash, Cid, MemoryStore};
use hashtree_lmdb::{
    PinnedLmdbFileIdentity, PinnedLmdbIdentity, PoolCatalogLocation, PoolMemberId,
    PoolMemberRuntimePaths, PoolStore, PoolStoreConfig, ReadOnlyPoolStore,
    SHARED_BLOB_POOL_DIR_NAME,
};
use hashtree_nostr::{
    NostrEventStore, StoredNostrEvent, ValidatedNostrEventBlob, VerifiedStoredNostrEvent,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::super::{
    cid_to_nhash, parse_root_text, persist_immutable_bytes, stage_bytes_sha256,
    StagedNostrCrawlState, STAGE_DIR, STAGE_FORMAT_VERSION, STAGE_STATE_FILE,
};
use super::audit::{
    audit_event_index_layout_and_collect_missing_blobs, audit_exact_event_index_parity,
    validate_exact_event_index_parity_evidence, BulkProjectionExactIndexParityEvidence,
};
use super::repair::{
    canonical_json_bytes, hash_file, pin_committed_pool_catalog, require_sha256,
    validate_pool_catalog_pin, PoolCatalogPin,
};
use super::{
    bulk_paths, validate_terminal_stage_state, BulkProjectionSpool, BulkProjectionState,
    SpoolEventRecord, BULK_PROJECTION_VERSION,
};

const EVENT_BLOB_REPAIR_FORMAT: &str = "nostr-index/bulk-projection-v2/event-blob-repair-v1";
const EVENT_BLOB_REPAIR_RECEIPT_FORMAT: &str =
    "nostr-index/bulk-projection-v2/event-blob-repair-v1/receipt";
const EVENT_BLOB_REPAIR_DIR: &str = "event-blob-repair-v1";
const EVENT_BLOB_REPAIR_INTENT_FILE: &str = "intent.json";
const EVENT_BLOB_REPAIR_RECEIPT_FILE: &str = "receipt.json";
const MAX_SCAN_PAGE_SIZE: usize = 4_096;
const MAX_RECOVERY_EVENTS: usize = 65_536;

#[derive(Debug, Clone)]
pub(crate) struct BulkEventBlobRepairOptions {
    pub(crate) staging_data_dir: PathBuf,
    pub(crate) expected_state_sha256: String,
    pub(crate) expected_stage_state_sha256: String,
    pub(crate) expected_policy_sha256: String,
    pub(crate) expected_spool_data_sha256: String,
    pub(crate) expected_profile_repair_retention_lease_sha256: String,
    pub(crate) expected_replayed_author_count: usize,
    pub(crate) expected_full_author_count: usize,
    pub(crate) btree_order: usize,
    pub(crate) page_size: usize,
    pub(crate) apply: bool,
    pub(crate) out: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
struct EventBlobPin {
    event_id: String,
    cid: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
struct StoredBlockPin {
    hash: String,
    size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct EventBlobRepairIntent {
    format: String,
    data_dir: String,
    staging_data_dir: String,
    state_sha256: String,
    stage_state_sha256: String,
    policy_sha256: String,
    spool_data_sha256: String,
    profile_repair_retention_lease_sha256: String,
    replayed_author_count: usize,
    full_author_count: usize,
    btree_order: usize,
    built_roots: BTreeMap<u8, String>,
    published_profile_roots: PublishedProfileRootsPin,
    scanned_records: u64,
    missing_records: u64,
    missing_set_sha256: String,
    missing: Vec<EventBlobPin>,
    stored_blocks: Vec<StoredBlockPin>,
    pre_repair_pool_catalog: PoolCatalogPin,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct PublishedProfileRootsPin {
    profiles_by_pubkey: String,
    profiles_by_pubkey_file_sha256: String,
    profile_search: String,
    profile_search_file_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct EventBlobRepairReceipt {
    format: String,
    intent_sha256: String,
    recovered_records: u64,
    missing_set_sha256: String,
    completion_pool_catalog: PoolCatalogPin,
    event_index_parity: BulkProjectionExactIndexParityEvidence,
}

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
struct EventBlobRepairPlan {
    format: String,
    applied: bool,
    scanned_records: u64,
    missing_records: u64,
    missing_set_sha256: String,
    missing_event_ids: Vec<String>,
    pool_catalog: PoolCatalogPin,
}

struct LoadedAuthority {
    state_path: PathBuf,
    state_bytes: Vec<u8>,
    state: BulkProjectionState,
    stage_state_path: PathBuf,
    stage_state_bytes: Vec<u8>,
    spool_path: PathBuf,
    retention_lease_path: PathBuf,
    retention_lease_bytes: Vec<u8>,
    published_profile_roots: PublishedProfileRootsPin,
}

#[derive(Clone)]
struct RecoveryRecord {
    pin: EventBlobPin,
    event: StoredNostrEvent,
}

struct PreparedRecovery {
    blobs: Vec<ValidatedNostrEventBlob>,
    stored_blocks: Vec<StoredBlockPin>,
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
        if !path.is_absolute() {
            anyhow::bail!("{label} path must be absolute: {}", path.display());
        }
        let canonical = path
            .canonicalize()
            .with_context(|| format!("canonicalize {label} {}", path.display()))?;
        if canonical != path {
            anyhow::bail!(
                "{label} must be an exact canonical path: got {}, canonical {}",
                path.display(),
                canonical.display()
            );
        }
        let before = std::fs::symlink_metadata(path)
            .with_context(|| format!("inspect {label} {}", path.display()))?;
        if before.file_type().is_symlink() || !before.file_type().is_dir() {
            anyhow::bail!("{label} is not a direct directory: {}", path.display());
        }
        #[cfg(unix)]
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .with_context(|| format!("open exact {label} {}", path.display()))?;
        #[cfg(not(unix))]
        let file =
            File::open(path).with_context(|| format!("open exact {label} {}", path.display()))?;
        let opened = file
            .metadata()
            .with_context(|| format!("inspect opened {label} {}", path.display()))?;
        if !opened.is_dir() {
            anyhow::bail!(
                "{label} opened object is not a directory: {}",
                path.display()
            );
        }
        #[cfg(unix)]
        if opened.dev() != before.dev() || opened.ino() != before.ino() {
            anyhow::bail!("{label} identity changed while it was opened");
        }
        let pinned = Self {
            file,
            path: path.to_path_buf(),
            #[cfg(unix)]
            device: opened.dev(),
            #[cfg(unix)]
            inode: opened.ino(),
        };
        pinned.ensure_identity(label)?;
        Ok(pinned)
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

    fn ensure_identity(&self, label: &str) -> Result<()> {
        let opened = self
            .file
            .metadata()
            .with_context(|| format!("reinspect opened {label}"))?;
        let current = std::fs::symlink_metadata(&self.path)
            .with_context(|| format!("reinspect {label} {}", self.path.display()))?;
        if current.file_type().is_symlink() || !current.file_type().is_dir() || !opened.is_dir() {
            anyhow::bail!("{label} is no longer the pinned direct directory");
        }
        #[cfg(unix)]
        if opened.dev() != self.device
            || opened.ino() != self.inode
            || current.dev() != self.device
            || current.ino() != self.inode
        {
            anyhow::bail!("{label} directory identity changed");
        }
        Ok(())
    }

    #[cfg(unix)]
    fn identity(&self) -> (u64, u64) {
        (self.device, self.inode)
    }
}

struct PinnedLmdbFiles {
    data: File,
    lock: File,
    identity: PinnedLmdbIdentity,
}

impl PinnedLmdbFiles {
    fn pin(directory: &PinnedDirectory, label: &str) -> Result<Self> {
        let (data, data_identity) =
            Self::pin_file(directory, "data.mdb", &format!("{label} data.mdb"))?;
        let (lock, lock_identity) =
            Self::pin_file(directory, "lock.mdb", &format!("{label} lock.mdb"))?;
        Ok(Self {
            data,
            lock,
            identity: PinnedLmdbIdentity {
                data: data_identity,
                lock: lock_identity,
            },
        })
    }

    fn pin_file(
        directory: &PinnedDirectory,
        name: &str,
        label: &str,
    ) -> Result<(File, PinnedLmdbFileIdentity)> {
        let path = directory.runtime_path().join(name);
        let before = std::fs::symlink_metadata(&path)
            .with_context(|| format!("inspect {label} {}", path.display()))?;
        if before.file_type().is_symlink() || !before.file_type().is_file() {
            anyhow::bail!("{label} is not a direct regular file");
        }
        #[cfg(unix)]
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
            .with_context(|| format!("open {label} {}", path.display()))?;
        #[cfg(not(unix))]
        let file = File::open(&path).with_context(|| format!("open {label} {}", path.display()))?;
        let opened = file
            .metadata()
            .with_context(|| format!("inspect opened {label}"))?;
        if !opened.is_file() {
            anyhow::bail!("{label} opened object is not a regular file");
        }
        #[cfg(unix)]
        {
            if opened.dev() != before.dev() || opened.ino() != before.ino() {
                anyhow::bail!("{label} identity changed while it was opened");
            }
            Ok((
                file,
                PinnedLmdbFileIdentity {
                    device: opened.dev(),
                    inode: opened.ino(),
                },
            ))
        }
        #[cfg(not(unix))]
        {
            Ok((
                file,
                PinnedLmdbFileIdentity {
                    device: 0,
                    inode: 0,
                },
            ))
        }
    }

    fn ensure_identity(&self, directory: &PinnedDirectory, label: &str) -> Result<()> {
        for (name, file, expected) in [
            ("data.mdb", &self.data, self.identity.data),
            ("lock.mdb", &self.lock, self.identity.lock),
        ] {
            let opened = file
                .metadata()
                .with_context(|| format!("reinspect {label} {name}"))?;
            let current = std::fs::symlink_metadata(directory.runtime_path().join(name))
                .with_context(|| format!("reinspect {label} {name} directory entry"))?;
            if current.file_type().is_symlink()
                || !current.file_type().is_file()
                || !opened.is_file()
            {
                anyhow::bail!("{label} {name} is no longer a direct regular file");
            }
            #[cfg(unix)]
            if opened.dev() != expected.device
                || opened.ino() != expected.inode
                || current.dev() != expected.device
                || current.ino() != expected.inode
            {
                anyhow::bail!("{label} {name} identity changed");
            }
        }
        Ok(())
    }

    #[cfg(unix)]
    fn identities(&self) -> [(u64, u64); 2] {
        [
            (self.identity.data.device, self.identity.data.inode),
            (self.identity.lock.device, self.identity.lock.inode),
        ]
    }
}

struct PinnedPoolMember {
    id: PoolMemberId,
    configured_path: PathBuf,
    configured_external_path: Option<PathBuf>,
    directory: PinnedDirectory,
    lmdb: PinnedLmdbFiles,
    external_directory: Option<PinnedDirectory>,
}

struct PinnedPoolAuthority {
    catalog: PinnedDirectory,
    catalog_lmdb: PinnedLmdbFiles,
    manifest_sha256: Hash,
    members: Vec<PinnedPoolMember>,
}

impl PinnedPoolAuthority {
    fn capture(data_dir: &Path) -> Result<Self> {
        if !cfg!(target_os = "linux") {
            anyhow::bail!(
                "exact controlled event-blob repair requires Linux pinned LMDB runtime paths"
            );
        }
        let pool_path = data_dir.join(SHARED_BLOB_POOL_DIR_NAME);
        let discovery = ReadOnlyPoolStore::open(&pool_path)
            .with_context(|| format!("discover exact Pool authority {}", pool_path.display()))?;
        discovery
            .require_durable_external_blob_writes()
            .context("require durable external writes for discovered Pool members")?;
        let snapshot = discovery
            .manifest_snapshot()
            .context("snapshot exact discovered Pool manifest")?;
        if snapshot.members.is_empty() {
            anyhow::bail!("event-blob repair requires at least one Pool member");
        }
        drop(discovery);

        let catalog = PinnedDirectory::open_exact(&pool_path, "Pool catalog directory")?;
        let catalog_lmdb = PinnedLmdbFiles::pin(&catalog, "Pool catalog")?;
        let mut members = Vec::with_capacity(snapshot.members.len());
        #[cfg(unix)]
        let mut directory_identities = BTreeSet::new();
        #[cfg(unix)]
        let mut lmdb_file_identities = BTreeSet::new();
        #[cfg(unix)]
        {
            directory_identities.insert(catalog.identity());
            for identity in catalog_lmdb.identities() {
                if !lmdb_file_identities.insert(identity) {
                    anyhow::bail!("Pool catalog data.mdb and lock.mdb alias the same file");
                }
            }
        }
        for member in snapshot.members {
            if member.config.external_blob_dir.is_some() && !member.config.external_blob_sync {
                anyhow::bail!("Pool member {} has external_blob_sync disabled", member.id);
            }
            let directory = PinnedDirectory::open_exact(
                &member.config.path,
                &format!("Pool member {} directory", member.id),
            )?;
            #[cfg(unix)]
            if !directory_identities.insert(directory.identity()) {
                anyhow::bail!("Pool member {} aliases another pinned directory", member.id);
            }
            let lmdb = PinnedLmdbFiles::pin(&directory, &format!("Pool member {}", member.id))?;
            #[cfg(unix)]
            for identity in lmdb.identities() {
                if !lmdb_file_identities.insert(identity) {
                    anyhow::bail!(
                        "Pool member {} LMDB file aliases another pinned Pool LMDB file",
                        member.id
                    );
                }
            }
            let external_directory = member
                .config
                .external_blob_dir
                .as_deref()
                .map(|path| {
                    PinnedDirectory::open_exact(
                        path,
                        &format!("Pool member {} external directory", member.id),
                    )
                })
                .transpose()?;
            #[cfg(unix)]
            if let Some(external) = &external_directory {
                if !directory_identities.insert(external.identity()) {
                    anyhow::bail!(
                        "Pool member {} external directory aliases another Pool directory",
                        member.id
                    );
                }
            }
            members.push(PinnedPoolMember {
                id: member.id,
                configured_path: member.config.path,
                configured_external_path: member.config.external_blob_dir,
                directory,
                lmdb,
                external_directory,
            });
        }
        let authority = Self {
            catalog,
            catalog_lmdb,
            manifest_sha256: snapshot.sha256,
            members,
        };
        authority.ensure_identities()?;
        let controlled = authority.open_read_only()?;
        let controlled_snapshot = controlled
            .manifest_snapshot()
            .context("snapshot controlled Pool manifest")?;
        if controlled_snapshot.sha256 != authority.manifest_sha256 {
            anyhow::bail!("controlled Pool manifest differs from captured authority");
        }
        controlled
            .require_durable_external_blob_writes()
            .context("require durable writes for controlled Pool members")?;
        Ok(authority)
    }

    fn config(&self) -> PoolStoreConfig {
        let mut config = PoolStoreConfig::default();
        config.temperature.enabled = false;
        config.catalog_lmdb_identity = Some(self.catalog_lmdb.identity);
        config.expected_manifest_sha256 = Some(self.manifest_sha256);
        config.member_runtime_paths = self
            .members
            .iter()
            .map(|member| PoolMemberRuntimePaths {
                id: member.id,
                configured_path: member.configured_path.clone(),
                runtime_path: member.directory.runtime_path(),
                configured_external_path: member.configured_external_path.clone(),
                runtime_external_path: member
                    .external_directory
                    .as_ref()
                    .map(PinnedDirectory::runtime_path),
                lmdb_identity: member.lmdb.identity,
            })
            .collect();
        config
    }

    fn ensure_identities(&self) -> Result<()> {
        self.catalog.ensure_identity("Pool catalog directory")?;
        self.catalog_lmdb
            .ensure_identity(&self.catalog, "Pool catalog")?;
        for member in &self.members {
            member
                .directory
                .ensure_identity(&format!("Pool member {} directory", member.id))?;
            member
                .lmdb
                .ensure_identity(&member.directory, &format!("Pool member {}", member.id))?;
            if let Some(external) = &member.external_directory {
                external
                    .ensure_identity(&format!("Pool member {} external directory", member.id))?;
            }
        }
        Ok(())
    }

    fn open_read_only(&self) -> Result<Arc<ReadOnlyPoolStore>> {
        self.ensure_identities()?;
        let store = Arc::new(
            ReadOnlyPoolStore::open_controlled(self.catalog.runtime_path(), self.config())
                .context("open controlled exact read-only Pool")?,
        );
        self.ensure_identities()?;
        Ok(store)
    }

    fn open_writer(&self) -> Result<Arc<PoolStore>> {
        self.ensure_identities()?;
        let store = Arc::new(
            PoolStore::open(self.catalog.runtime_path(), self.config())
                .context("open controlled exact writable Pool")?,
        );
        self.ensure_identities()?;
        Ok(store)
    }
}

fn repair_paths(data_dir: &Path) -> Result<(PathBuf, PathBuf)> {
    let (state_path, _) = bulk_paths(data_dir);
    let root = state_path
        .parent()
        .context("bulk projection state has no parent directory")?
        .join(EVENT_BLOB_REPAIR_DIR);
    Ok((
        root.join(EVENT_BLOB_REPAIR_INTENT_FILE),
        root.join(EVENT_BLOB_REPAIR_RECEIPT_FILE),
    ))
}

fn load_canonical<T: DeserializeOwned + Serialize>(path: &Path, label: &str) -> Result<Option<T>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("read {label} {}", path.display()))
        }
    };
    let value: T = serde_json::from_slice(&bytes)
        .with_context(|| format!("decode {label} {}", path.display()))?;
    if canonical_json_bytes(&value, label)? != bytes {
        anyhow::bail!("{label} is not canonical: {}", path.display());
    }
    Ok(Some(value))
}

fn missing_set_sha256(pins: &[EventBlobPin]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"hashtree-nostr-bulk-event-blob-repair-set-v1\0");
    digest.update((pins.len() as u64).to_be_bytes());
    for pin in pins {
        digest.update((pin.event_id.len() as u64).to_be_bytes());
        digest.update(pin.event_id.as_bytes());
        digest.update((pin.cid.len() as u64).to_be_bytes());
        digest.update(pin.cid.as_bytes());
    }
    hex::encode(digest.finalize())
}

fn validate_event_id(event_id: &str) -> Result<()> {
    if event_id.len() != 64
        || !event_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        anyhow::bail!("bulk spool event id is not canonical lowercase hex: {event_id}");
    }
    Ok(())
}

fn record_pin(event_id: &str, record: &SpoolEventRecord) -> Result<EventBlobPin> {
    validate_event_id(event_id)?;
    if record.event.id != event_id {
        anyhow::bail!(
            "bulk spool event key `{event_id}` differs from record id `{}`",
            record.event.id
        );
    }
    let cid = Cid {
        hash: record.cid_hash,
        key: record.cid_key,
    };
    Ok(EventBlobPin {
        event_id: event_id.to_string(),
        cid: cid_to_nhash(&cid)?,
    })
}

fn pin_published_profile_roots(data_dir: &Path) -> Result<PublishedProfileRootsPin> {
    let roots = hashtree_cli::socialgraph::read_profile_index_roots(data_dir)
        .context("read exact published profile root pair")?;
    let profiles_by_pubkey = roots
        .by_pubkey
        .as_ref()
        .context("published profiles-by-pubkey root is missing")?;
    let profile_search = roots
        .search
        .as_ref()
        .context("published profile-search root is missing")?;
    let profiles_by_pubkey_file_sha256 = roots
        .by_pubkey_file_sha256
        .clone()
        .context("published profiles-by-pubkey root-file digest is missing")?;
    let profile_search_file_sha256 = roots
        .search_file_sha256
        .clone()
        .context("published profile-search root-file digest is missing")?;
    require_sha256(
        "published profiles-by-pubkey root-file SHA-256",
        &profiles_by_pubkey_file_sha256,
    )?;
    require_sha256(
        "published profile-search root-file SHA-256",
        &profile_search_file_sha256,
    )?;
    Ok(PublishedProfileRootsPin {
        profiles_by_pubkey: cid_to_nhash(profiles_by_pubkey)?,
        profiles_by_pubkey_file_sha256,
        profile_search: cid_to_nhash(profile_search)?,
        profile_search_file_sha256,
    })
}

fn load_retention_lease(
    data_dir: &Path,
    expected_sha256: &str,
    built_roots: &BTreeMap<u8, String>,
) -> Result<(PathBuf, Vec<u8>)> {
    require_sha256("profile repair retention lease SHA-256", expected_sha256)?;
    let path = data_dir.join(PROFILE_REPAIR_RETENTION_LEASE_RELATIVE_PATH);
    let metadata = std::fs::symlink_metadata(&path)
        .with_context(|| format!("inspect profile repair retention lease {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        anyhow::bail!(
            "profile repair retention lease is not a direct regular file: {}",
            path.display()
        );
    }
    if metadata.len() > 64 * 1024 {
        anyhow::bail!(
            "profile repair retention lease exceeds 65536 bytes: {}",
            path.display()
        );
    }
    let bytes = std::fs::read(&path)
        .with_context(|| format!("read profile repair retention lease {}", path.display()))?;
    let actual_sha256 = stage_bytes_sha256(&bytes);
    if actual_sha256 != expected_sha256 {
        anyhow::bail!(
            "profile repair retention lease SHA-256 mismatch: expected {expected_sha256}, found {actual_sha256}"
        );
    }
    let lease: ProfileRepairRetentionLease = serde_json::from_slice(&bytes)
        .with_context(|| format!("decode profile repair retention lease {}", path.display()))?;
    if lease.canonical_bytes()? != bytes {
        anyhow::bail!(
            "profile repair retention lease is not canonical: {}",
            path.display()
        );
    }
    for (stable_id, encoded) in built_roots {
        let label = format!("event-index-{stable_id}");
        let canonical = parse_root_text(encoded)
            .with_context(|| format!("parse retained event-index root {stable_id}"))?
            .to_string();
        if lease.roots.get(&label) != Some(&canonical) {
            anyhow::bail!(
                "profile repair retention lease does not cover exact event root {stable_id}"
            );
        }
    }
    Ok((path, bytes))
}

async fn prepare_recovery_records(
    records: &[RecoveryRecord],
    page_size: usize,
) -> Result<PreparedRecovery> {
    let mut blobs = Vec::with_capacity(records.len());
    for batch in records.chunks(page_size) {
        for record in batch {
            VerifiedStoredNostrEvent::try_from(record.event.clone())
                .with_context(|| format!("verify signed recovery event {}", record.pin.event_id))?;
        }
        let memory = Arc::new(MemoryStore::new());
        let target = NostrEventStore::new(memory);
        let computed = target
            .store_event_blobs(batch.iter().map(|record| record.event.clone()))
            .await
            .context("precompute recovery event CIDs in memory")?;
        if computed.len() != batch.len() {
            anyhow::bail!(
                "in-memory recovery CID count mismatch: expected {}, found {}",
                batch.len(),
                computed.len()
            );
        }
        for (record, cid) in batch.iter().zip(&computed) {
            let actual = cid_to_nhash(cid)?;
            if actual != record.pin.cid {
                anyhow::bail!(
                    "retained event CID differs from pinned spool CID for {}",
                    record.pin.event_id
                );
            }
        }
        let loaded = target
            .load_validated_event_blobs(computed)
            .await
            .context("load exact precomputed recovery event blocks")?;
        if loaded.len() != batch.len() {
            anyhow::bail!(
                "in-memory validated recovery count mismatch: expected {}, found {}",
                batch.len(),
                loaded.len()
            );
        }
        for (record, blob) in batch.iter().zip(&loaded) {
            if blob.event() != &record.event || cid_to_nhash(blob.cid())? != record.pin.cid {
                anyhow::bail!(
                    "validated recovery blob differs from retained event {}",
                    record.pin.event_id
                );
            }
        }
        blobs.extend(loaded);
    }
    let mut blocks = BTreeMap::<Hash, u64>::new();
    for blob in &blobs {
        for (hash, size) in blob.stored_block_metadata() {
            if let Some(previous) = blocks.insert(hash, size) {
                if previous != size {
                    anyhow::bail!(
                        "recovery block {} has conflicting sizes {previous} and {size}",
                        to_hex(&hash)
                    );
                }
            }
        }
    }
    let stored_blocks = blocks
        .into_iter()
        .map(|(hash, size)| StoredBlockPin {
            hash: to_hex(&hash),
            size,
        })
        .collect();
    Ok(PreparedRecovery {
        blobs,
        stored_blocks,
    })
}

fn load_recovery_records(
    spool: &BulkProjectionSpool,
    pins: &[EventBlobPin],
) -> Result<Vec<RecoveryRecord>> {
    pins.iter()
        .map(|expected_pin| {
            let record = spool
                .event_record(&expected_pin.event_id)?
                .with_context(|| {
                    format!(
                        "intended event `{}` is absent from retained spool",
                        expected_pin.event_id
                    )
                })?;
            let pin = record_pin(&expected_pin.event_id, &record)?;
            if &pin != expected_pin {
                anyhow::bail!(
                    "retained spool CID changed for intended event {}",
                    expected_pin.event_id
                );
            }
            Ok(RecoveryRecord {
                pin,
                event: record.event,
            })
        })
        .collect()
}

async fn precompute_intended_blocks(
    spool: &BulkProjectionSpool,
    pins: &[EventBlobPin],
    page_size: usize,
) -> Result<Vec<StoredBlockPin>> {
    let mut blocks = BTreeMap::<String, u64>::new();
    for batch in pins.chunks(page_size) {
        let records = load_recovery_records(spool, batch)?;
        let prepared = prepare_recovery_records(&records, page_size).await?;
        for block in prepared.stored_blocks {
            if let Some(previous) = blocks.insert(block.hash.clone(), block.size) {
                if previous != block.size {
                    anyhow::bail!(
                        "intended recovery block {} has conflicting sizes {previous} and {}",
                        block.hash,
                        block.size
                    );
                }
            }
        }
    }
    Ok(blocks
        .into_iter()
        .map(|(hash, size)| StoredBlockPin { hash, size })
        .collect())
}

fn load_authority(
    data_dir: &Path,
    options: &BulkEventBlobRepairOptions,
) -> Result<LoadedAuthority> {
    for (label, pin) in [
        (
            "bulk projection state SHA-256",
            options.expected_state_sha256.as_str(),
        ),
        (
            "staging state SHA-256",
            options.expected_stage_state_sha256.as_str(),
        ),
        (
            "crawl policy SHA-256",
            options.expected_policy_sha256.as_str(),
        ),
        (
            "bulk spool data SHA-256",
            options.expected_spool_data_sha256.as_str(),
        ),
        (
            "profile repair retention lease SHA-256",
            options
                .expected_profile_repair_retention_lease_sha256
                .as_str(),
        ),
    ] {
        require_sha256(label, pin)?;
    }
    if options.expected_replayed_author_count == 0
        || options.expected_replayed_author_count > options.expected_full_author_count
        || options.btree_order < 2
        || options.page_size == 0
        || options.page_size > MAX_SCAN_PAGE_SIZE
    {
        anyhow::bail!(
            "event-blob repair requires valid author counts, B-tree order, and a page size in 1..={MAX_SCAN_PAGE_SIZE}"
        );
    }
    if options
        .out
        .as_deref()
        .is_some_and(|path| path != Path::new("-") && !path.is_absolute())
    {
        anyhow::bail!("event-blob repair output path must be absolute or `-`");
    }

    let (state_path, spool_path) = bulk_paths(data_dir);
    let state_bytes = std::fs::read(&state_path)
        .with_context(|| format!("read bulk state {}", state_path.display()))?;
    let state_sha256 = stage_bytes_sha256(&state_bytes);
    if state_sha256 != options.expected_state_sha256 {
        anyhow::bail!(
            "bulk state SHA-256 mismatch: expected {}, found {state_sha256}",
            options.expected_state_sha256
        );
    }
    let state: BulkProjectionState =
        serde_json::from_slice(&state_bytes).context("decode bulk projection state")?;
    if state.version != BULK_PROJECTION_VERSION
        || state.complete_root.is_some()
        || state.segment_event_offset != 0
        || state.next_author != options.expected_replayed_author_count
        || state.policy.author_count != options.expected_full_author_count
        || state.policy.max_authors != options.expected_full_author_count
        || state.built_roots.len() != 9
        || state.built_roots.keys().copied().collect::<Vec<_>>() != (0u8..9).collect::<Vec<_>>()
        || state.built_roots.values().any(String::is_empty)
    {
        anyhow::bail!(
            "event-blob repair requires terminal v2 replay, exactly nine roots, and no complete root"
        );
    }
    for (stable_id, encoded) in &state.built_roots {
        let root = parse_root_text(encoded)
            .with_context(|| format!("parse pinned event-index root {stable_id}"))?;
        if cid_to_nhash(&root)? != *encoded {
            anyhow::bail!("event-index root {stable_id} is not canonical nhash text");
        }
    }
    let policy_sha256 = stage_bytes_sha256(
        &serde_json::to_vec(&state.policy).context("encode pinned crawl policy")?,
    );
    if policy_sha256 != options.expected_policy_sha256 {
        anyhow::bail!(
            "crawl policy SHA-256 mismatch: expected {}, found {policy_sha256}",
            options.expected_policy_sha256
        );
    }

    let stage_state_path = options
        .staging_data_dir
        .join(STAGE_DIR)
        .join(STAGE_STATE_FILE);
    let stage_state_bytes = std::fs::read(&stage_state_path)
        .with_context(|| format!("read staging state {}", stage_state_path.display()))?;
    let stage_state_sha256 = stage_bytes_sha256(&stage_state_bytes);
    if stage_state_sha256 != options.expected_stage_state_sha256 {
        anyhow::bail!(
            "staging state SHA-256 mismatch: expected {}, found {stage_state_sha256}",
            options.expected_stage_state_sha256
        );
    }
    let stage: StagedNostrCrawlState =
        serde_json::from_slice(&stage_state_bytes).context("decode staging state")?;
    if stage.version != STAGE_FORMAT_VERSION {
        anyhow::bail!(
            "staging state version {} differs from required version {STAGE_FORMAT_VERSION}",
            stage.version
        );
    }
    validate_terminal_stage_state(&state, &stage)?;

    let (retention_lease_path, retention_lease_bytes) = load_retention_lease(
        data_dir,
        &options.expected_profile_repair_retention_lease_sha256,
        &state.built_roots,
    )?;
    let spool_data_path = spool_path.join("data.mdb");
    let spool_data_sha256 = hash_file(&spool_data_path)?;
    if spool_data_sha256 != options.expected_spool_data_sha256 {
        anyhow::bail!(
            "bulk spool data SHA-256 mismatch: expected {}, found {spool_data_sha256}",
            options.expected_spool_data_sha256
        );
    }
    let published_profile_roots = pin_published_profile_roots(data_dir)?;
    Ok(LoadedAuthority {
        state_path,
        state_bytes,
        state,
        stage_state_path,
        stage_state_bytes,
        spool_path,
        retention_lease_path,
        retention_lease_bytes,
        published_profile_roots,
    })
}

fn require_exact_bytes(label: &str, path: &Path, expected: &[u8]) -> Result<()> {
    let actual =
        std::fs::read(path).with_context(|| format!("re-read {label} {}", path.display()))?;
    if actual != expected {
        anyhow::bail!("{label} changed during event-blob repair");
    }
    Ok(())
}

fn validate_intent(
    intent: &EventBlobRepairIntent,
    data_dir: &Path,
    options: &BulkEventBlobRepairOptions,
    authority: &LoadedAuthority,
) -> Result<()> {
    let data_dir = data_dir
        .canonicalize()
        .context("canonicalize event-blob repair data directory")?;
    let staging_data_dir = options
        .staging_data_dir
        .canonicalize()
        .context("canonicalize event-blob repair staging directory")?;
    if intent.format != EVENT_BLOB_REPAIR_FORMAT
        || Path::new(&intent.data_dir) != data_dir
        || Path::new(&intent.staging_data_dir) != staging_data_dir
        || intent.state_sha256 != options.expected_state_sha256
        || intent.stage_state_sha256 != options.expected_stage_state_sha256
        || intent.policy_sha256 != options.expected_policy_sha256
        || intent.spool_data_sha256 != options.expected_spool_data_sha256
        || intent.profile_repair_retention_lease_sha256
            != options.expected_profile_repair_retention_lease_sha256
        || intent.replayed_author_count != options.expected_replayed_author_count
        || intent.full_author_count != options.expected_full_author_count
        || intent.btree_order != options.btree_order
        || intent.built_roots != authority.state.built_roots
        || intent.published_profile_roots != authority.published_profile_roots
        || intent.missing_records != intent.missing.len() as u64
        || intent.missing.len() > MAX_RECOVERY_EVENTS
        || intent.missing_set_sha256 != missing_set_sha256(&intent.missing)
    {
        anyhow::bail!("event-blob repair intent differs from exact pinned authority");
    }
    let mut previous = None::<&EventBlobPin>;
    for pin in &intent.missing {
        validate_event_id(&pin.event_id)?;
        parse_root_text(&pin.cid).context("parse intended event-blob CID")?;
        if previous.is_some_and(|previous| previous >= pin || previous.event_id == pin.event_id) {
            anyhow::bail!("event-blob repair intent is not strictly ordered");
        }
        previous = Some(pin);
    }
    let mut previous = None::<&StoredBlockPin>;
    for pin in &intent.stored_blocks {
        let hash = from_hex(&pin.hash).context("parse intended stored-block hash")?;
        if to_hex(&hash) != pin.hash || pin.size == 0 {
            anyhow::bail!("event-blob repair intent has an invalid stored-block pin");
        }
        if previous.is_some_and(|previous| previous >= pin) {
            anyhow::bail!("event-blob repair stored-block intent is not strictly ordered");
        }
        previous = Some(pin);
    }
    if intent.missing.is_empty() != intent.stored_blocks.is_empty() {
        anyhow::bail!(
            "event-blob repair intent event and physical-block sets disagree on emptiness"
        );
    }
    validate_pool_catalog_pin(
        "event-blob repair prestate",
        &intent.pre_repair_pool_catalog,
    )
}

fn validate_receipt(
    receipt: &EventBlobRepairReceipt,
    intent: &EventBlobRepairIntent,
    intent_bytes: &[u8],
) -> Result<()> {
    validate_pool_catalog_pin(
        "event-blob repair completion",
        &receipt.completion_pool_catalog,
    )?;
    validate_exact_event_index_parity_evidence(
        &receipt.event_index_parity,
        &intent.built_roots,
        intent.btree_order,
    )?;
    if receipt.format != EVENT_BLOB_REPAIR_RECEIPT_FORMAT
        || receipt.intent_sha256 != stage_bytes_sha256(intent_bytes)
        || receipt.recovered_records != intent.missing_records
        || receipt.missing_set_sha256 != intent.missing_set_sha256
        || receipt.event_index_parity.spool_event_records != intent.scanned_records
    {
        anyhow::bail!("event-blob repair receipt does not complete its exact durable intent");
    }
    Ok(())
}

fn publish_output(out: Option<&Path>, bytes: &[u8], label: &str) -> Result<()> {
    match out {
        Some(path) if path != Path::new("-") => persist_immutable_bytes(path, bytes, label),
        _ => {
            print!("{}", String::from_utf8_lossy(bytes));
            Ok(())
        }
    }
}

fn intended_block_hashes(pins: &[StoredBlockPin]) -> Result<Vec<Hash>> {
    pins.iter()
        .map(|pin| from_hex(&pin.hash).context("parse intended stored-block hash"))
        .collect()
}

fn validate_intended_block_locations(
    store: &ReadOnlyPoolStore,
    pins: &[StoredBlockPin],
    require_stored: bool,
) -> Result<()> {
    let hashes = intended_block_hashes(pins)?;
    let locations = store
        .blob_catalog_locations(&hashes)
        .context("inspect exact intended Pool catalog locations")?;
    if locations.len() != pins.len() {
        anyhow::bail!(
            "intended Pool location count mismatch: expected {}, found {}",
            pins.len(),
            locations.len()
        );
    }
    let members = store
        .manifest_snapshot()
        .context("recheck Pool member authority for intended blocks")?
        .members
        .into_iter()
        .map(|member| member.id)
        .collect::<BTreeSet<_>>();
    for ((pin, hash), location) in pins.iter().zip(hashes).zip(locations) {
        match location {
            PoolCatalogLocation::Missing if !require_stored => {}
            PoolCatalogLocation::Pending { member, size } if !require_stored => {
                if size != pin.size || !members.contains(&member) {
                    anyhow::bail!(
                        "intended Pending block {} differs from pinned size/member authority",
                        pin.hash
                    );
                }
            }
            PoolCatalogLocation::Stored { member, size } => {
                if size != pin.size || !members.contains(&member) {
                    anyhow::bail!(
                        "intended Stored block {} differs from pinned size/member authority",
                        pin.hash
                    );
                }
                let bytes = store
                    .get_sync(&hash)
                    .with_context(|| format!("read exact intended Stored block {}", pin.hash))?
                    .with_context(|| format!("intended Stored block {} is absent", pin.hash))?;
                if bytes.len() as u64 != pin.size {
                    anyhow::bail!(
                        "intended Stored block {} readback size differs from intent",
                        pin.hash
                    );
                }
            }
            PoolCatalogLocation::Missing => {
                anyhow::bail!("intended block {} is still missing after repair", pin.hash);
            }
            PoolCatalogLocation::Pending { .. } => {
                anyhow::bail!("intended block {} is still Pending after repair", pin.hash);
            }
            PoolCatalogLocation::Moving { .. } => {
                anyhow::bail!(
                    "intended block {} is Moving; repair never adopts relocation state",
                    pin.hash
                );
            }
        }
    }
    Ok(())
}

async fn replay_intended_event_blobs(
    spool: &BulkProjectionSpool,
    missing: &[EventBlobPin],
    stored_blocks: &[StoredBlockPin],
    page_size: usize,
    pool_authority: &PinnedPoolAuthority,
) -> Result<()> {
    if missing.is_empty() {
        return Ok(());
    }
    let reconstructed = precompute_intended_blocks(spool, missing, page_size).await?;
    if reconstructed != stored_blocks {
        anyhow::bail!("reconstructed physical event-block set differs from durable repair intent");
    }
    let prewrite = pool_authority.open_read_only()?;
    validate_intended_block_locations(&prewrite, stored_blocks, false)?;
    drop(prewrite);

    for pin_batch in missing.chunks(page_size) {
        pool_authority.ensure_identities()?;
        let records = load_recovery_records(spool, pin_batch)?;
        let prepared = prepare_recovery_records(&records, page_size).await?;
        {
            let writer = pool_authority.open_writer()?;
            writer
                .validate_controlled_authority_and_sync()
                .context("validate controlled Pool authority before repair batch")?;
            let target = NostrEventStore::new(Arc::clone(&writer));
            let written = target
                .store_validated_event_blobs(&prepared.blobs)
                .await
                .context("store exact validated retained event-blob repair batch")?;
            if written.len() != records.len() {
                anyhow::bail!(
                    "event-blob repair write count mismatch: expected {}, found {}",
                    records.len(),
                    written.len()
                );
            }
            for (record, cid) in records.iter().zip(&written) {
                let actual = cid_to_nhash(cid)?;
                if actual != record.pin.cid {
                    anyhow::bail!(
                        "repaired CID differs from durable intent for event {}",
                        record.pin.event_id
                    );
                }
            }
            writer
                .validate_controlled_authority_and_sync()
                .context("sync and revalidate controlled Pool after repair batch")?;
            pool_authority.ensure_identities()?;
            let readback = target
                .load_event_blobs(written)
                .await
                .context("read back exact force-synced event-blob repair batch")?;
            if readback.len() != records.len() {
                anyhow::bail!(
                    "event-blob repair readback count mismatch: expected {}, found {}",
                    records.len(),
                    readback.len()
                );
            }
            for (record, durable) in records.iter().zip(readback) {
                if durable != record.event {
                    anyhow::bail!(
                        "event-blob repair readback differs for event {}",
                        record.pin.event_id
                    );
                }
            }
            drop(target);
            writer
                .validate_controlled_authority_and_sync()
                .context("finalize exact controlled Pool repair batch")?;
        }
        // heed cannot safely reopen the same LMDB environment read-only while
        // the controlled writer is alive.  Scope the writer above so this
        // independent catalog and physical readback uses a closed writer.
        let batch_reader = pool_authority.open_read_only()?;
        validate_intended_block_locations(&batch_reader, &prepared.stored_blocks, true)?;
    }
    let completed = pool_authority.open_read_only()?;
    validate_intended_block_locations(&completed, stored_blocks, true)?;
    completed
        .validate_committed_catalog()
        .context("require globally committed Pool catalog after intended Pending replay")?;
    Ok(())
}

fn recheck_immutable_authority(
    data_dir: &Path,
    options: &BulkEventBlobRepairOptions,
    authority: &LoadedAuthority,
    pool_authority: &PinnedPoolAuthority,
) -> Result<()> {
    require_exact_bytes(
        "bulk projection state",
        &authority.state_path,
        &authority.state_bytes,
    )?;
    require_exact_bytes(
        "staging state",
        &authority.stage_state_path,
        &authority.stage_state_bytes,
    )?;
    require_exact_bytes(
        "profile repair retention lease",
        &authority.retention_lease_path,
        &authority.retention_lease_bytes,
    )?;
    let spool_sha256 = hash_file(&authority.spool_path.join("data.mdb"))?;
    if spool_sha256 != options.expected_spool_data_sha256 {
        anyhow::bail!(
            "bulk spool changed during event-blob repair: expected {}, found {spool_sha256}",
            options.expected_spool_data_sha256
        );
    }
    if pin_published_profile_roots(data_dir)? != authority.published_profile_roots {
        anyhow::bail!("published profile roots changed during event-blob repair");
    }
    pool_authority.ensure_identities()
}

pub(crate) async fn repair_bulk_projection_event_blobs(
    data_dir: &Path,
    options: BulkEventBlobRepairOptions,
) -> Result<()> {
    let _retention_guard = acquire_existing_profile_repair_retention_guard(data_dir)
        .context("freeze existing profile-repair retention authority")?;
    let _profile_roots = hashtree_cli::socialgraph::acquire_profile_root_snapshot_guard(data_dir)?
        .context("event-blob repair requires an existing socialgraph root namespace")?;
    let authority = load_authority(data_dir, &options)?;
    let spool = BulkProjectionSpool::open_read_only(&authority.spool_path)?;
    let (intent_path, receipt_path) = repair_paths(data_dir)?;
    let existing_intent: Option<EventBlobRepairIntent> =
        load_canonical(&intent_path, "event-blob repair intent")?;
    let existing_receipt: Option<EventBlobRepairReceipt> =
        load_canonical(&receipt_path, "event-blob repair receipt")?;

    if existing_receipt.is_some() && existing_intent.is_none() {
        anyhow::bail!("event-blob repair receipt exists without its durable intent");
    }
    if let Some(intent) = existing_intent.as_ref() {
        validate_intent(intent, data_dir, &options, &authority)?;
    }
    let pool_authority = PinnedPoolAuthority::capture(data_dir)
        .context("capture exact controlled Pool authority")?;
    if let Some(receipt) = existing_receipt.as_ref() {
        let intent = existing_intent.as_ref().expect("checked above");
        let intent_bytes = canonical_json_bytes(intent, "event-blob repair intent")?;
        validate_receipt(receipt, intent, &intent_bytes)?;
        let current_store = pool_authority.open_read_only()?;
        let current_catalog = pin_committed_pool_catalog(&current_store)
            .context("require committed Pool before accepting completed repair receipt")?;
        let (scanned_records, missing) = audit_event_index_layout_and_collect_missing_blobs(
            &spool,
            Arc::clone(&current_store),
            &authority.state.built_roots,
            options.btree_order,
            options.page_size,
            MAX_RECOVERY_EVENTS,
        )
        .await
        .context("re-audit current event bodies before accepting completed repair receipt")?;
        if scanned_records != intent.scanned_records {
            anyhow::bail!("current spool count differs from completed event-blob repair intent");
        }
        if pin_committed_pool_catalog(&current_store)? != current_catalog {
            anyhow::bail!("PoolStore catalog changed during completed-receipt revalidation");
        }
        if let Some(missing) = missing.first() {
            anyhow::bail!(
                "completed event-blob repair has degraded at event {}; a fresh recovery generation is required",
                missing.event_id
            );
        }
        drop(current_store);
        recheck_immutable_authority(data_dir, &options, &authority, &pool_authority)?;
        let receipt_bytes = canonical_json_bytes(receipt, "event-blob repair receipt")?;
        publish_output(
            options.out.as_deref(),
            &receipt_bytes,
            "event-blob repair output",
        )?;
        return Ok(());
    }

    if existing_intent.is_some() && !options.apply {
        anyhow::bail!("event-blob repair has a durable intent and requires --apply to resume");
    }
    let intent = if let Some(intent) = existing_intent {
        if to_hex(&pool_authority.manifest_sha256) != intent.pre_repair_pool_catalog.manifest_sha256
        {
            anyhow::bail!(
                "Pool manifest changed after the incomplete event-blob repair intent; start a fresh controlled generation"
            );
        }
        replay_intended_event_blobs(
            &spool,
            &intent.missing,
            &intent.stored_blocks,
            options.page_size,
            &pool_authority,
        )
        .await
        .context("resume exact durable event-blob repair intent")?;
        intent
    } else {
        let pre_store = pool_authority.open_read_only()?;
        let current_catalog = pin_committed_pool_catalog(&pre_store)?;
        validate_pool_catalog_pin("event-blob repair current prestate", &current_catalog)?;
        let (scanned_records, layout_missing) = audit_event_index_layout_and_collect_missing_blobs(
            &spool,
            Arc::clone(&pre_store),
            &authority.state.built_roots,
            options.btree_order,
            options.page_size,
            MAX_RECOVERY_EVENTS,
        )
        .await
        .context("audit all nine event-root layouts and collect every missing durable body")?;
        if pin_committed_pool_catalog(&pre_store)? != current_catalog {
            anyhow::bail!("PoolStore catalog changed during event-blob repair planning audit");
        }
        drop(pre_store);

        let missing = layout_missing
            .into_iter()
            .map(|missing| {
                Ok(EventBlobPin {
                    event_id: missing.event_id,
                    cid: cid_to_nhash(&missing.cid)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        if !options.apply {
            let plan = EventBlobRepairPlan {
                format: EVENT_BLOB_REPAIR_FORMAT.to_string(),
                applied: false,
                scanned_records,
                missing_records: missing.len() as u64,
                missing_set_sha256: missing_set_sha256(&missing),
                missing_event_ids: missing.iter().map(|pin| pin.event_id.clone()).collect(),
                pool_catalog: current_catalog,
            };
            let bytes = canonical_json_bytes(&plan, "event-blob repair plan")?;
            print!("{}", String::from_utf8_lossy(&bytes));
            return Ok(());
        }

        let stored_blocks = precompute_intended_blocks(&spool, &missing, options.page_size).await?;
        let intent = EventBlobRepairIntent {
            format: EVENT_BLOB_REPAIR_FORMAT.to_string(),
            data_dir: data_dir
                .canonicalize()
                .context("canonicalize event-blob repair data directory")?
                .to_string_lossy()
                .into_owned(),
            staging_data_dir: options
                .staging_data_dir
                .canonicalize()
                .context("canonicalize event-blob repair staging directory")?
                .to_string_lossy()
                .into_owned(),
            state_sha256: options.expected_state_sha256.clone(),
            stage_state_sha256: options.expected_stage_state_sha256.clone(),
            policy_sha256: options.expected_policy_sha256.clone(),
            spool_data_sha256: options.expected_spool_data_sha256.clone(),
            profile_repair_retention_lease_sha256: options
                .expected_profile_repair_retention_lease_sha256
                .clone(),
            replayed_author_count: options.expected_replayed_author_count,
            full_author_count: options.expected_full_author_count,
            btree_order: options.btree_order,
            built_roots: authority.state.built_roots.clone(),
            published_profile_roots: authority.published_profile_roots.clone(),
            scanned_records,
            missing_records: missing.len() as u64,
            missing_set_sha256: missing_set_sha256(&missing),
            missing,
            stored_blocks,
            pre_repair_pool_catalog: current_catalog,
        };
        validate_intent(&intent, data_dir, &options, &authority)?;
        let intent_bytes = canonical_json_bytes(&intent, "event-blob repair intent")?;
        persist_immutable_bytes(&intent_path, &intent_bytes, "event-blob repair intent")?;
        if intent.missing_records > 0 {
            replay_intended_event_blobs(
                &spool,
                &intent.missing,
                &intent.stored_blocks,
                options.page_size,
                &pool_authority,
            )
            .await
            .context("apply exact durable event-blob repair intent")?;
        }
        intent
    };
    let intent_bytes = canonical_json_bytes(&intent, "event-blob repair intent")?;
    recheck_immutable_authority(data_dir, &options, &authority, &pool_authority)?;

    let completion_store = pool_authority.open_read_only()?;
    let completion_catalog = pin_committed_pool_catalog(&completion_store)?;
    validate_pool_catalog_pin("event-blob repair completion", &completion_catalog)?;
    let event_index_parity = audit_exact_event_index_parity(
        &spool,
        Arc::clone(&completion_store),
        &authority.state.built_roots,
        options.btree_order,
        options.page_size,
    )
    .await
    .context("exhaustively audit all nine event roots after event-blob repair")?;
    if event_index_parity.spool_event_records != intent.scanned_records {
        anyhow::bail!(
            "event-root completion count {} differs from intended spool count {}",
            event_index_parity.spool_event_records,
            intent.scanned_records
        );
    }
    let final_catalog = pin_committed_pool_catalog(&completion_store)?;
    if final_catalog != completion_catalog {
        anyhow::bail!("PoolStore catalog changed during final event-root audit");
    }
    drop(completion_store);
    recheck_immutable_authority(data_dir, &options, &authority, &pool_authority)?;

    let receipt = EventBlobRepairReceipt {
        format: EVENT_BLOB_REPAIR_RECEIPT_FORMAT.to_string(),
        intent_sha256: stage_bytes_sha256(&intent_bytes),
        recovered_records: intent.missing_records,
        missing_set_sha256: intent.missing_set_sha256.clone(),
        completion_pool_catalog: final_catalog,
        event_index_parity,
    };
    let receipt_bytes = canonical_json_bytes(&receipt, "event-blob repair receipt")?;
    persist_immutable_bytes(&receipt_path, &receipt_bytes, "event-blob repair receipt")?;
    publish_output(
        options.out.as_deref(),
        &receipt_bytes,
        "event-blob repair output",
    )
}

#[cfg(test)]
mod retention_representation_tests {
    use super::*;

    #[tokio::test]
    async fn completion_audit_rejects_missing_body_outside_original_repair_set() {
        use hashtree_core::Store;
        use hashtree_lmdb::PoolMemberConfig;
        use hashtree_nostr::{stored_event_from_nostr_sdk_event, NostrEventIndex};
        use nostr::{EventBuilder, Keys, Kind, Tag};

        let temp = tempfile::tempdir().unwrap();
        let pool_path = temp.path().join(SHARED_BLOB_POOL_DIR_NAME);
        let pool = Arc::new(PoolStore::open(&pool_path, PoolStoreConfig::default()).unwrap());
        pool.add_member(PoolMemberConfig::new(
            temp.path().join("member"),
            32 * 1024 * 1024,
        ))
        .unwrap();
        let keys = Keys::generate();
        let events = [
            EventBuilder::new(Kind::Metadata, "{}"),
            EventBuilder::new(Kind::Custom(30_000), "originally intact")
                .tags([Tag::identifier("repair-audit")]),
        ]
        .into_iter()
        .map(|builder| stored_event_from_nostr_sdk_event(&builder.sign_with_keys(&keys).unwrap()))
        .collect::<Vec<_>>();
        let target = NostrEventStore::new(Arc::clone(&pool));
        let cids = target.store_event_blobs(events.clone()).await.unwrap();
        let spool = BulkProjectionSpool::open(&temp.path().join("spool")).unwrap();
        spool
            .apply(events.clone().into_iter().zip(cids.clone()).collect())
            .unwrap();
        let mut roots = BTreeMap::new();
        for index in NostrEventIndex::ALL {
            let root = spool
                .build_index_root(index, Arc::clone(&pool), 8)
                .await
                .unwrap()
                .unwrap();
            roots.insert(index.stable_id(), cid_to_nhash(&root).unwrap());
        }
        pool.delete(&cids[0].hash).await.unwrap();
        pool.force_sync().unwrap();
        drop(target);
        drop(pool);

        let reader = Arc::new(ReadOnlyPoolStore::open(&pool_path).unwrap());
        let (count, missing) = audit_event_index_layout_and_collect_missing_blobs(
            &spool,
            Arc::clone(&reader),
            &roots,
            8,
            1,
            MAX_RECOVERY_EVENTS,
        )
        .await
        .unwrap();
        assert_eq!(count, 2);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].event_id, events[0].id);
        drop(reader);

        // Complete the original repair, then lose a different body before the
        // completion audit. The final gate must examine more than the intent.
        let pool = Arc::new(PoolStore::open(&pool_path, PoolStoreConfig::default()).unwrap());
        let target = NostrEventStore::new(Arc::clone(&pool));
        assert_eq!(
            target.store_event_blobs([events[0].clone()]).await.unwrap(),
            [cids[0].clone()]
        );
        pool.delete(&cids[1].hash).await.unwrap();
        pool.force_sync().unwrap();
        drop(target);
        drop(pool);

        let reader = Arc::new(ReadOnlyPoolStore::open(&pool_path).unwrap());
        let error = audit_exact_event_index_parity(&spool, reader, &roots, 8, 1)
            .await
            .expect_err("a newly missing body must prevent repair completion");
        assert!(format!("{error:#}").contains(&format!(
            "exhaustively load durable by-id event `{}`",
            events[1].id
        )));
    }

    fn write_retention_lease(data_dir: &Path, cid: &Cid) -> Vec<u8> {
        let path = data_dir.join(PROFILE_REPAIR_RETENTION_LEASE_RELATIVE_PATH);
        std::fs::create_dir_all(path.parent().expect("retention parent"))
            .expect("create retention parent");
        let lease = ProfileRepairRetentionLease {
            format: hashtree_cli::storage::PROFILE_REPAIR_RETENTION_LEASE_FORMAT.to_string(),
            authority_sha256: "11".repeat(32),
            roots: BTreeMap::from([("event-index-0".to_string(), cid.to_string())]),
        };
        let lease_bytes = lease.canonical_bytes().expect("canonical lease");
        std::fs::write(&path, &lease_bytes).expect("write retention lease");
        lease_bytes
    }

    #[test]
    fn retention_lease_accepts_the_same_event_root_in_nhash_state_encoding() {
        let temp = tempfile::tempdir().expect("temp dir");
        let cid = Cid::encrypted([0x2a; 32], [0x7b; 32]);
        let lease_bytes = write_retention_lease(temp.path(), &cid);
        let built_roots = BTreeMap::from([(0, cid_to_nhash(&cid).expect("encode state nhash"))]);
        load_retention_lease(temp.path(), &stage_bytes_sha256(&lease_bytes), &built_roots)
            .expect("equivalent CID encodings must identify the same retained event root");
    }

    #[test]
    fn retention_lease_rejects_an_event_root_with_a_different_decryption_key() {
        let temp = tempfile::tempdir().expect("temp dir");
        let lease_cid = Cid::encrypted([0x2a; 32], [0x7b; 32]);
        let state_cid = Cid::encrypted(lease_cid.hash, [0x8c; 32]);
        let lease_bytes = write_retention_lease(temp.path(), &lease_cid);
        let built_roots = BTreeMap::from([(
            0,
            cid_to_nhash(&state_cid).expect("encode different state nhash"),
        )]);

        let error =
            load_retention_lease(temp.path(), &stage_bytes_sha256(&lease_bytes), &built_roots)
                .expect_err("a different decrypt key must not satisfy exact root retention");
        assert!(
            error
                .to_string()
                .contains("does not cover exact event root 0"),
            "unexpected error: {error:#}"
        );
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use hashtree_lmdb::PoolMemberConfig;
    use hashtree_nostr::stored_event_from_nostr_sdk_event;
    use nostr::{EventBuilder, Keys, Kind, Timestamp};

    #[tokio::test]
    async fn controlled_replay_restores_exact_real_event_blocks_and_detects_repurge() {
        let temp = tempfile::tempdir().expect("temp dir");
        let data_dir = temp.path().canonicalize().expect("canonical temp dir");
        let pool_path = data_dir.join(SHARED_BLOB_POOL_DIR_NAME);
        let member_path = data_dir.join("event-repair-member");
        let mut pool_config = PoolStoreConfig::default();
        pool_config.temperature.enabled = false;
        let pool = PoolStore::open(&pool_path, pool_config).expect("open real Pool");
        pool.add_member(PoolMemberConfig::new(member_path, 32 * 1024 * 1024))
            .expect("add real Pool member");
        pool.force_sync().expect("sync empty Pool");
        drop(pool);

        let keys = Keys::generate();
        let signed = EventBuilder::new(Kind::TextNote, "exact controlled repair")
            .custom_created_at(Timestamp::from_secs(1_723_000_000))
            .sign_with_keys(&keys)
            .expect("sign real event");
        let event = stored_event_from_nostr_sdk_event(&signed);
        let memory = Arc::new(MemoryStore::new());
        let encoder = NostrEventStore::new(memory);
        let cid = encoder
            .store_event_blobs([event.clone()])
            .await
            .expect("encode real event")
            .pop()
            .expect("event CID");
        let pin = EventBlobPin {
            event_id: event.id.clone(),
            cid: cid_to_nhash(&cid).expect("encode CID"),
        };
        let spool = BulkProjectionSpool::open(&data_dir.join("event-repair-spool"))
            .expect("open real recovery spool");
        spool
            .apply(vec![(event.clone(), cid.clone())])
            .expect("persist real spool event");
        let stored_blocks = precompute_intended_blocks(&spool, std::slice::from_ref(&pin), 1)
            .await
            .expect("precompute exact physical intent");

        let authority =
            PinnedPoolAuthority::capture(&data_dir).expect("capture exact Pool authority");
        let before = authority.open_read_only().expect("open controlled reader");
        validate_intended_block_locations(&before, &stored_blocks, false)
            .expect("accept exact Missing prestate");
        drop(before);

        replay_intended_event_blobs(
            &spool,
            std::slice::from_ref(&pin),
            &stored_blocks,
            1,
            &authority,
        )
        .await
        .expect("replay exact signed event");
        let after = authority.open_read_only().expect("open completed reader");
        validate_intended_block_locations(&after, &stored_blocks, true)
            .expect("all physical blocks committed");
        after
            .validate_committed_catalog()
            .expect("completed catalog is globally committed");
        let loaded = NostrEventStore::new(after)
            .load_event_blob(&cid)
            .await
            .expect("load restored real event");
        assert_eq!(loaded, event);

        // A crash after physical replay but before receipt publication must
        // allow the same durable intent to resume without changing the catalog.
        let before_resume = authority.open_read_only().expect("read pre-resume catalog");
        let completed_catalog = pin_committed_pool_catalog(&before_resume).unwrap();
        drop(before_resume);
        replay_intended_event_blobs(
            &spool,
            std::slice::from_ref(&pin),
            &stored_blocks,
            1,
            &authority,
        )
        .await
        .expect("resume already completed physical replay");
        let after_resume = authority.open_read_only().expect("read resumed catalog");
        assert_eq!(
            pin_committed_pool_catalog(&after_resume).unwrap(),
            completed_catalog
        );
        drop(after_resume);

        let writer = authority
            .open_writer()
            .expect("open controlled repurge writer");
        for hash in intended_block_hashes(&stored_blocks).expect("parse block intent") {
            writer.delete_sync(&hash).expect("repurge intended block");
        }
        writer
            .validate_controlled_authority_and_sync()
            .expect("sync controlled repurge");
        drop(writer);
        let repurged = authority
            .open_read_only()
            .expect("open repurged controlled reader");
        let error = validate_intended_block_locations(&repurged, &stored_blocks, true)
            .expect_err("completed-state validation must detect repurge");
        assert!(error.to_string().contains("still missing"));
    }
}
