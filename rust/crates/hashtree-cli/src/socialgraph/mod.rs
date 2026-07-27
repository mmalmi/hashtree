pub mod access;
pub mod crawler;
pub mod local_lists;
pub mod snapshot;

pub use access::SocialGraphAccessControl;
pub use crawler::SocialGraphCrawler;
pub use local_lists::{
    read_local_list_file_state, sync_local_list_files_force, sync_local_list_files_if_changed,
    LocalListFileState, LocalListSyncOutcome,
};

mod index_buckets;

use index_buckets::{
    dedupe_events, latest_metadata_events_by_pubkey, EventIndexBucket, ProfileIndexBucket,
};

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock, Weak};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::fd::AsRawFd;

use anyhow::{Context, Result};
use bytes::Bytes;
use futures::executor::block_on;
use hashtree_core::{
    nhash_decode, nhash_encode_full, sha256, to_hex, BufferedStore, Cid, HashTree, HashTreeConfig,
    NHashData, Store,
};
use hashtree_index::BTree;
use hashtree_nostr::{
    is_parameterized_replaceable_kind, is_replaceable_kind, stored_event_from_nostr_sdk_event,
    ListEventsOptions, NostrEventStore, NostrEventStoreError, ProfileGuard as NostrProfileGuard,
    StoredNostrEvent,
};
#[cfg(test)]
use hashtree_nostr::{
    reset_profile as reset_nostr_profile, set_profile_enabled as set_nostr_profile_enabled,
    take_profile as take_nostr_profile,
};
use heed::EnvFlags;
use nostr::{Event, Filter, JsonUtil, Kind, SingleLetterTag};
use nostr_social_graph::{
    BinaryBudget, GraphStats, NostrEvent as GraphEvent, SocialGraph,
    SocialGraphBackend as NostrSocialGraphBackend,
};
use nostr_social_graph_heed::HeedSocialGraph;
use sha2::{Digest, Sha256};

use crate::managed_env::ManagedEnv;
use crate::storage::{LocalStore, StorageRouter};

pub type UserSet = BTreeSet<[u8; 32]>;

const PROFILE_PUBLICATION_FENCE_RELATIVE_PATH: &str =
    "nostr-index/bulk-projection-v3/profile-publication.fenced";
const DEFAULT_ROOT_HEX: &str = "0000000000000000000000000000000000000000000000000000000000000000";
const EVENTS_ROOT_FILE: &str = "events-root.msgpack";
const AMBIENT_EVENTS_ROOT_FILE: &str = "events-root-ambient.msgpack";
const AMBIENT_EVENTS_BLOB_DIR: &str = "ambient-blobs";
const PROFILE_SEARCH_ROOT_FILE: &str = "profile-search-root.msgpack";
const PROFILES_BY_PUBKEY_ROOT_FILE: &str = "profiles-by-pubkey-root.msgpack";
const PROFILE_ROOT_PAIR_COMMIT_FILE: &str = "profile-root-pair.commit.json";
const PROFILE_PROJECTION_PENDING_FILE: &str = "profile-projection.pending.json";
const PROFILE_ROOT_PAIR_LOCK_FILE: &str = "profile-root-pair.lock";
const PROFILE_PUBLICATION_LOCK_FILE: &str = "profile-publication.lock";
const PROFILE_REPAIR_EVIDENCE_RELATIVE_DIR: &str =
    "nostr-index/bulk-projection-v2/profile-repair-v1";
const PROFILE_REPAIR_COMPLETION_FILE: &str = "completion.json";
pub const PROFILE_REPAIR_FORMAT: &str = "iris-social/bulk-profile-index-repair@1";
pub const PROFILE_REPAIR_RECEIPT_FORMAT: &str = "iris-social/bulk-profile-index-repair-receipt@1";
pub const PROFILE_REPAIR_COMPLETION_FORMAT: &str =
    "iris-social/bulk-profile-index-repair-completion@1";
const PROFILE_ROOT_PAIR_COMMIT_VERSION: u32 = 1;
const PROFILE_PROJECTION_PENDING_VERSION: u32 = 1;
const PROFILE_ROOT_PAIR_LOCK_TIMEOUT: Duration = Duration::from_secs(30);
const PROFILE_ROOT_PAIR_LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(10);
const UNKNOWN_FOLLOW_DISTANCE: u32 = 1000;
const DEFAULT_SOCIALGRAPH_MAP_SIZE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MIN_SOCIALGRAPH_MAP_SIZE_BYTES: u64 = 64 * 1024 * 1024;
const SOCIALGRAPH_MAX_DBS: u32 = 16;
const PROFILE_SEARCH_INDEX_ORDER: usize = 64;
const PROFILE_SEARCH_PREFIX: &str = "p:";
const PROFILE_NAME_MAX_LENGTH: usize = 100;

pub fn profile_publication_fence_path(data_dir: &Path) -> PathBuf {
    data_dir.join(PROFILE_PUBLICATION_FENCE_RELATIVE_PATH)
}

pub fn profile_repair_evidence_paths(data_dir: &Path) -> (PathBuf, PathBuf) {
    let directory = data_dir.join(PROFILE_REPAIR_EVIDENCE_RELATIVE_DIR);
    (
        directory.join("intent.json"),
        directory.join("receipt.json"),
    )
}

pub fn profile_repair_completion_path(data_dir: &Path) -> PathBuf {
    data_dir
        .join(PROFILE_REPAIR_EVIDENCE_RELATIVE_DIR)
        .join(PROFILE_REPAIR_COMPLETION_FILE)
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileRepairCompletionWitness {
    format: String,
    intent_sha256: String,
    receipt_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileRepairRootPairPin {
    by_pubkey: String,
    by_pubkey_file_sha256: String,
    search: String,
    search_file_sha256: String,
}

#[derive(Debug, serde::Deserialize)]
struct ProfileRepairAuthorizationIntent {
    format: String,
    data_dir: String,
    old_roots: ProfileRepairRootPairPin,
    new_roots: ProfileRepairRootPairPin,
}

#[derive(Debug, serde::Deserialize)]
struct ProfileRepairAuthorizationReceipt {
    format: String,
    intent_sha256: String,
    installed_roots: ProfileRepairRootPairPin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProfileIndexRepairAuthorityPhase {
    Commit,
    Completion,
}

/// Opaque proof that the exact prepared root pair is bound to the durable
/// high-level repair evidence in this store.
///
/// The fields intentionally remain private: privileged low-level recovery and
/// publication APIs consume this value and revalidate its evidence while
/// holding the root-pair transaction.
pub struct ProfileIndexRepairAuthority {
    root_pair_lock_path: PathBuf,
    intent_sha256: String,
    receipt_sha256: Option<String>,
    old_roots: ProfileIndexRoots,
    new_roots: ProfileIndexRoots,
    phase: ProfileIndexRepairAuthorityPhase,
}

fn profile_repair_sha256(bytes: &[u8]) -> String {
    to_hex(&sha256(bytes))
}

pub fn profile_repair_completion_witness_bytes(
    intent_bytes: &[u8],
    receipt_bytes: &[u8],
) -> Result<Vec<u8>> {
    let witness = ProfileRepairCompletionWitness {
        format: PROFILE_REPAIR_COMPLETION_FORMAT.to_string(),
        intent_sha256: profile_repair_sha256(intent_bytes),
        receipt_sha256: profile_repair_sha256(receipt_bytes),
    };
    let mut bytes =
        serde_json::to_vec(&witness).context("encode canonical profile repair completion")?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn profile_publication_is_fenced(data_dir: &Path) -> Result<bool> {
    let path = profile_publication_fence_path(data_dir);
    match std::fs::symlink_metadata(&path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error)
            .with_context(|| format!("inspect profile publication fence {}", path.display())),
    }
}

pub fn require_profile_publication_unfenced(data_dir: &Path) -> Result<()> {
    if profile_publication_is_fenced(data_dir)? {
        let path = profile_publication_fence_path(data_dir);
        anyhow::bail!(
            "profile-root publication is fenced by active v3 tranche marker {}",
            path.display()
        );
    }
    Ok(())
}

pub struct ProfilePublicationGuard {
    _transaction: ProfileRootPairTransactionGuard,
}

pub struct ProfilePublicationFenceGuard {
    _transaction: ProfileRootPairTransactionGuard,
}

pub struct ProfileRootSnapshotGuard {
    db_dir: PathBuf,
    _transaction: ProfileRootPairTransactionGuard,
}

impl ProfileRootSnapshotGuard {
    /// Return every root whose DAG can still be needed after recovery while
    /// this guard freezes profile/event-root publication. A durable root-pair
    /// commit is a roll-forward obligation, so its not-yet-installed roots are
    /// retention roots just as much as the currently installed files.
    pub(crate) fn retention_roots(&self) -> Result<Vec<Cid>> {
        let mut roots = Vec::new();
        for file_name in [
            EVENTS_ROOT_FILE,
            AMBIENT_EVENTS_ROOT_FILE,
            PROFILE_SEARCH_ROOT_FILE,
            PROFILES_BY_PUBKEY_ROOT_FILE,
        ] {
            if let Some(root) = read_root_file(&self.db_dir.join(file_name))? {
                roots.push(root);
            }
        }
        if let Some(commit) =
            load_profile_root_pair_commit(&self.db_dir.join(PROFILE_ROOT_PAIR_COMMIT_FILE))?
        {
            roots.extend(
                [
                    commit.old_search,
                    commit.old_by_pubkey,
                    commit.new_search,
                    commit.new_by_pubkey,
                ]
                .into_iter()
                .flatten()
                .map(cid_from_stored),
            );
        }
        roots.sort_by_key(Cid::to_string);
        roots.dedup();
        Ok(roots)
    }
}

fn profile_publication_lock_path(data_dir: &Path) -> PathBuf {
    data_dir
        .join("socialgraph")
        .join(PROFILE_PUBLICATION_LOCK_FILE)
}

/// Hold the shared side of the profile/event-root transaction while a storage
/// retention pass snapshots and traverses every currently published root.
/// Returning `None` is valid only when the data directory has no socialgraph
/// database yet.
pub fn acquire_profile_root_snapshot_guard(
    data_dir: &Path,
) -> Result<Option<ProfileRootSnapshotGuard>> {
    let db_dir = data_dir.join("socialgraph");
    match std::fs::symlink_metadata(&db_dir) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => anyhow::bail!(
            "socialgraph database is not a direct directory: {}",
            db_dir.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect socialgraph database {}", db_dir.display()));
        }
    }
    let transaction = acquire_profile_root_pair_lock(
        &db_dir.join(PROFILE_ROOT_PAIR_LOCK_FILE),
        ProfileRootPairLockMode::Shared,
        true,
    )?;
    Ok(Some(ProfileRootSnapshotGuard {
        db_dir,
        _transaction: transaction,
    }))
}

/// Acquire a shared transaction before checking the durable profile
/// publication fence. Keep the returned guard alive through the complete
/// external upload/sign/publish operation so fence installation drains every
/// attempt that already passed the check.
pub async fn acquire_profile_publication_guard(data_dir: &Path) -> Result<ProfilePublicationGuard> {
    let transaction = acquire_profile_root_pair_lock_async(
        &profile_publication_lock_path(data_dir),
        ProfileRootPairLockMode::Shared,
        true,
    )
    .await?;
    require_profile_publication_unfenced(data_dir)?;
    Ok(ProfilePublicationGuard {
        _transaction: transaction,
    })
}

/// Acquire the exclusive side of the external profile-publication
/// transaction. Persist the durable fence while this guard is held; after it
/// is released, later publishers acquire the shared side and observe the
/// fence, while every publisher that observed the unfenced state has already
/// drained.
pub async fn acquire_profile_publication_fence_guard(
    data_dir: &Path,
) -> Result<ProfilePublicationFenceGuard> {
    let transaction = acquire_profile_root_pair_lock_async(
        &profile_publication_lock_path(data_dir),
        ProfileRootPairLockMode::Exclusive,
        true,
    )
    .await?;
    Ok(ProfilePublicationFenceGuard {
        _transaction: transaction,
    })
}

fn direct_regular_file_exists(path: &Path, label: &str) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
                anyhow::bail!("{label} is not a direct regular file: {}", path.display());
            }
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("inspect {label} {}", path.display())),
    }
}

fn profile_repair_data_dir_from_lock_path(root_pair_lock_path: &Path) -> Result<&Path> {
    let profile_db_dir = root_pair_lock_path.parent().with_context(|| {
        format!(
            "{} has no profile database parent",
            root_pair_lock_path.display()
        )
    })?;
    profile_db_dir
        .parent()
        .with_context(|| format!("{} has no data-directory parent", profile_db_dir.display()))
}

fn profile_repair_root_pair_pin(roots: &ProfileIndexRoots) -> Result<ProfileRepairRootPairPin> {
    let by_pubkey = roots
        .by_pubkey
        .as_ref()
        .context("profile-by-pubkey repair root is missing")?;
    let search = roots
        .search
        .as_ref()
        .context("profile-search repair root is missing")?;
    let by_pubkey_file_sha256 = roots
        .by_pubkey_file_sha256
        .clone()
        .context("profile-by-pubkey repair root-file SHA-256 is missing")?;
    let search_file_sha256 = roots
        .search_file_sha256
        .clone()
        .context("profile-search repair root-file SHA-256 is missing")?;
    if by_pubkey_file_sha256 != profile_index_root_file_sha256(by_pubkey)?
        || search_file_sha256 != profile_index_root_file_sha256(search)?
    {
        anyhow::bail!("profile repair root-file digest does not match its CID");
    }
    Ok(ProfileRepairRootPairPin {
        by_pubkey: nhash_encode_full(&NHashData {
            hash: by_pubkey.hash,
            decrypt_key: by_pubkey.key,
        })
        .context("encode profile-by-pubkey repair root")?,
        by_pubkey_file_sha256,
        search: nhash_encode_full(&NHashData {
            hash: search.hash,
            decrypt_key: search.key,
        })
        .context("encode profile-search repair root")?,
        search_file_sha256,
    })
}

fn profile_repair_root_pair_from_pin(pin: &ProfileRepairRootPairPin) -> Result<ProfileIndexRoots> {
    fn parse_root(value: &str, label: &str) -> Result<Cid> {
        let decoded =
            nhash_decode(value).with_context(|| format!("decode pinned {label} repair root"))?;
        if nhash_encode_full(&decoded).context("re-encode pinned repair root")? != value {
            anyhow::bail!("pinned {label} repair root is not canonical");
        }
        Ok(Cid {
            hash: decoded.hash,
            key: decoded.decrypt_key,
        })
    }

    let by_pubkey = parse_root(&pin.by_pubkey, "profile-by-pubkey")?;
    let search = parse_root(&pin.search, "profile-search")?;
    let roots = ProfileIndexRoots {
        by_pubkey: Some(by_pubkey),
        search: Some(search),
        by_pubkey_file_sha256: Some(pin.by_pubkey_file_sha256.clone()),
        search_file_sha256: Some(pin.search_file_sha256.clone()),
    };
    profile_repair_root_pair_pin(&roots)?;
    Ok(roots)
}

fn load_profile_index_repair_authority(
    root_pair_lock_path: &Path,
    prepared: &PreparedProfileIndexRepair,
    phase: ProfileIndexRepairAuthorityPhase,
    expected_intent_bytes: Option<&[u8]>,
    expected_receipt_bytes: Option<&[u8]>,
) -> Result<ProfileIndexRepairAuthority> {
    let data_dir = profile_repair_data_dir_from_lock_path(root_pair_lock_path)?;
    let (intent_path, receipt_path) = profile_repair_evidence_paths(data_dir);
    let completion_path = profile_repair_completion_path(data_dir);
    let intent_exists = direct_regular_file_exists(&intent_path, "profile repair intent")?;
    let receipt_exists = direct_regular_file_exists(&receipt_path, "profile repair receipt")?;
    let completion_exists =
        direct_regular_file_exists(&completion_path, "profile repair completion")?;
    let expected_state = match phase {
        ProfileIndexRepairAuthorityPhase::Commit => (true, false, false),
        ProfileIndexRepairAuthorityPhase::Completion => (true, true, false),
    };
    if (intent_exists, receipt_exists, completion_exists) != expected_state {
        anyhow::bail!(
            "profile-index repair authority requires evidence state {:?}, found intent={} receipt={} completion={}",
            phase,
            intent_exists,
            receipt_exists,
            completion_exists
        );
    }

    let intent_bytes = std::fs::read(&intent_path)
        .with_context(|| format!("read profile repair intent {}", intent_path.display()))?;
    if expected_intent_bytes.is_some_and(|expected| expected != intent_bytes.as_slice()) {
        anyhow::bail!("profile repair intent differs from the fully validated canonical bytes");
    }
    let intent: ProfileRepairAuthorizationIntent =
        serde_json::from_slice(&intent_bytes).context("decode profile repair authority intent")?;
    if intent.format != PROFILE_REPAIR_FORMAT {
        anyhow::bail!("profile repair intent has an unsupported format");
    }
    let canonical_data_dir = data_dir
        .canonicalize()
        .context("canonicalize profile repair authority data directory")?
        .to_string_lossy()
        .into_owned();
    if intent.data_dir != canonical_data_dir {
        anyhow::bail!("profile repair intent is bound to a different data directory");
    }
    if intent.old_roots != profile_repair_root_pair_pin(&prepared.old_roots)?
        || intent.new_roots != profile_repair_root_pair_pin(&prepared.new_roots)?
    {
        anyhow::bail!("profile repair intent does not bind the exact prepared root pair");
    }
    let intent_sha256 = profile_repair_sha256(&intent_bytes);

    let receipt_sha256 = if phase == ProfileIndexRepairAuthorityPhase::Completion {
        let receipt_bytes = std::fs::read(&receipt_path)
            .with_context(|| format!("read profile repair receipt {}", receipt_path.display()))?;
        if expected_receipt_bytes.is_some_and(|expected| expected != receipt_bytes.as_slice()) {
            anyhow::bail!(
                "profile repair receipt differs from the fully validated canonical bytes"
            );
        }
        let receipt: ProfileRepairAuthorizationReceipt = serde_json::from_slice(&receipt_bytes)
            .context("decode profile repair authority receipt")?;
        if receipt.format != PROFILE_REPAIR_RECEIPT_FORMAT
            || receipt.intent_sha256 != intent_sha256
            || receipt.installed_roots != profile_repair_root_pair_pin(&prepared.new_roots)?
        {
            anyhow::bail!(
                "profile repair receipt does not bind the exact intent and installed root pair"
            );
        }
        Some(profile_repair_sha256(&receipt_bytes))
    } else {
        if expected_receipt_bytes.is_some() {
            anyhow::bail!("commit-phase repair authority cannot bind receipt bytes");
        }
        None
    };

    Ok(ProfileIndexRepairAuthority {
        root_pair_lock_path: root_pair_lock_path.to_path_buf(),
        intent_sha256,
        receipt_sha256,
        old_roots: prepared.old_roots.clone(),
        new_roots: prepared.new_roots.clone(),
        phase,
    })
}

fn revalidate_profile_index_repair_authority(
    authority: &ProfileIndexRepairAuthority,
    root_pair_lock_path: &Path,
    prepared: &PreparedProfileIndexRepair,
    phase: ProfileIndexRepairAuthorityPhase,
) -> Result<()> {
    if authority.root_pair_lock_path != root_pair_lock_path
        || authority.old_roots != prepared.old_roots
        || authority.new_roots != prepared.new_roots
        || authority.phase != phase
    {
        anyhow::bail!("profile-index repair authority belongs to a different transaction");
    }
    let current =
        load_profile_index_repair_authority(root_pair_lock_path, prepared, phase, None, None)?;
    if current.intent_sha256 != authority.intent_sha256
        || current.receipt_sha256 != authority.receipt_sha256
    {
        anyhow::bail!("profile-index repair authority evidence changed before publication");
    }
    Ok(())
}

fn validate_profile_repair_completion(
    data_dir: &Path,
    intent_path: &Path,
    receipt_path: &Path,
    completion_path: &Path,
) -> Result<()> {
    let intent_bytes = std::fs::read(intent_path)
        .with_context(|| format!("read profile repair intent {}", intent_path.display()))?;
    let receipt_bytes = std::fs::read(receipt_path)
        .with_context(|| format!("read profile repair receipt {}", receipt_path.display()))?;
    let completion_bytes = std::fs::read(completion_path).with_context(|| {
        format!(
            "read profile repair completion {}",
            completion_path.display()
        )
    })?;

    let intent: ProfileRepairAuthorizationIntent =
        serde_json::from_slice(&intent_bytes).context("decode profile repair intent")?;
    let receipt: ProfileRepairAuthorizationReceipt =
        serde_json::from_slice(&receipt_bytes).context("decode profile repair receipt")?;
    if intent.format != PROFILE_REPAIR_FORMAT {
        anyhow::bail!("profile repair intent has an unsupported format");
    }
    if receipt.format != PROFILE_REPAIR_RECEIPT_FORMAT {
        anyhow::bail!("profile repair receipt has an unsupported format");
    }
    let canonical_data_dir = data_dir
        .canonicalize()
        .context("canonicalize completed profile repair data directory")?
        .to_string_lossy()
        .into_owned();
    if intent.data_dir != canonical_data_dir {
        anyhow::bail!("profile repair intent is bound to a different data directory");
    }
    profile_repair_root_pair_from_pin(&intent.old_roots)?;
    let new_roots = profile_repair_root_pair_from_pin(&intent.new_roots)?;
    let installed_roots = profile_repair_root_pair_from_pin(&receipt.installed_roots)?;
    if installed_roots != new_roots {
        anyhow::bail!("profile repair receipt does not bind the intended installed root pair");
    }
    let intent_sha256 = profile_repair_sha256(&intent_bytes);
    if receipt.intent_sha256 != intent_sha256 {
        anyhow::bail!("profile repair receipt does not bind the durable intent");
    }

    let completion: ProfileRepairCompletionWitness =
        serde_json::from_slice(&completion_bytes).context("decode profile repair completion")?;
    let canonical = profile_repair_completion_witness_bytes(&intent_bytes, &receipt_bytes)?;
    if completion_bytes != canonical
        || completion.format != PROFILE_REPAIR_COMPLETION_FORMAT
        || completion.intent_sha256 != intent_sha256
        || completion.receipt_sha256 != profile_repair_sha256(&receipt_bytes)
    {
        anyhow::bail!("profile repair completion does not bind the exact intent and receipt");
    }
    Ok(())
}

fn incomplete_profile_repair_intent_path(root_pair_lock_path: &Path) -> Result<Option<PathBuf>> {
    let data_dir = profile_repair_data_dir_from_lock_path(root_pair_lock_path)?;
    let (intent_path, receipt_path) = profile_repair_evidence_paths(data_dir);
    let completion_path = profile_repair_completion_path(data_dir);
    let intent_exists = direct_regular_file_exists(&intent_path, "profile repair intent")?;
    let receipt_exists = direct_regular_file_exists(&receipt_path, "profile repair receipt")?;
    let completion_exists =
        direct_regular_file_exists(&completion_path, "profile repair completion")?;
    match (intent_exists, receipt_exists, completion_exists) {
        (false, false, false) => Ok(None),
        (true, false, false) | (true, true, false) => Ok(Some(intent_path)),
        (true, true, true) => {
            validate_profile_repair_completion(
                data_dir,
                &intent_path,
                &receipt_path,
                &completion_path,
            )
            .context("profile root write is blocked by invalid repair completion")?;
            Ok(None)
        }
        (false, true, _) => anyhow::bail!(
            "profile root write is blocked by receipt without repair intent: {}",
            receipt_path.display()
        ),
        (false, false, true) => anyhow::bail!(
            "profile root write is blocked by completion without repair intent: {}",
            completion_path.display()
        ),
        (true, false, true) => anyhow::bail!(
            "profile root write is blocked by completion without repair receipt: {}",
            completion_path.display()
        ),
    }
}

fn require_no_incomplete_profile_repair_for_root_write(root_pair_lock_path: &Path) -> Result<()> {
    if let Some(intent_path) = incomplete_profile_repair_intent_path(root_pair_lock_path)? {
        anyhow::bail!(
            "profile root write is blocked by incomplete durable repair intent {}",
            intent_path.display()
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventStorageClass {
    Public,
    Ambient,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EventQueryScope {
    PublicOnly,
    AmbientOnly,
    All,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PublicEventsRootApplyOutcome {
    Applied,
    Conflict { current_root: Option<Cid> },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredCid {
    hash: [u8; 32],
    key: Option<[u8; 32]>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileRootPairCommit {
    version: u32,
    old_search: Option<StoredCid>,
    old_by_pubkey: Option<StoredCid>,
    new_search: Option<StoredCid>,
    new_by_pubkey: Option<StoredCid>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
enum StoredEventStorageClass {
    Public,
    Ambient,
}

impl From<EventStorageClass> for StoredEventStorageClass {
    fn from(value: EventStorageClass) -> Self {
        match value {
            EventStorageClass::Public => Self::Public,
            EventStorageClass::Ambient => Self::Ambient,
        }
    }
}

impl From<StoredEventStorageClass> for EventStorageClass {
    fn from(value: StoredEventStorageClass) -> Self {
        match value {
            StoredEventStorageClass::Public => Self::Public,
            StoredEventStorageClass::Ambient => Self::Ambient,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "mode", rename_all = "kebab-case", deny_unknown_fields)]
enum PendingProfileProjectionMode {
    Incremental {
        old_root: Option<StoredCid>,
        new_root: StoredCid,
        events: Vec<String>,
    },
    RebuildPublicRoot {
        old_root: Option<StoredCid>,
        new_root: StoredCid,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingProfileProjection {
    version: u32,
    storage_class: StoredEventStorageClass,
    projection: PendingProfileProjectionMode,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StoredProfileSearchEntry {
    pub pubkey: String,
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub nip05: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_distance: Option<u32>,
    pub created_at: u64,
    pub event_nhash: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProfileIndexRoots {
    pub by_pubkey: Option<Cid>,
    pub search: Option<Cid>,
    pub by_pubkey_file_sha256: Option<String>,
    pub search_file_sha256: Option<String>,
}

fn profile_index_roots_from_cids(
    by_pubkey: Option<Cid>,
    search: Option<Cid>,
) -> Result<ProfileIndexRoots> {
    Ok(ProfileIndexRoots {
        by_pubkey_file_sha256: by_pubkey
            .as_ref()
            .map(profile_index_root_file_sha256)
            .transpose()?,
        search_file_sha256: search
            .as_ref()
            .map(profile_index_root_file_sha256)
            .transpose()?,
        by_pubkey,
        search,
    })
}

fn read_profile_index_root_pair_snapshot(
    by_pubkey_path: &Path,
    search_path: &Path,
) -> Result<ProfileIndexRoots> {
    let (by_pubkey, by_pubkey_file_sha256) = read_root_file_snapshot(by_pubkey_path)?;
    let (search, search_file_sha256) = read_root_file_snapshot(search_path)?;
    Ok(ProfileIndexRoots {
        by_pubkey,
        search,
        by_pubkey_file_sha256,
        search_file_sha256,
    })
}

impl ProfileIndexRepairAuthority {
    fn require_pending_commit(
        &self,
        commit: &ProfileRootPairCommit,
        by_pubkey_path: &Path,
        search_path: &Path,
    ) -> Result<()> {
        let old_roots = profile_index_roots_from_cids(
            commit.old_by_pubkey.clone().map(cid_from_stored),
            commit.old_search.clone().map(cid_from_stored),
        )?;
        let new_roots = profile_index_roots_from_cids(
            commit.new_by_pubkey.clone().map(cid_from_stored),
            commit.new_search.clone().map(cid_from_stored),
        )?;
        if old_roots != self.old_roots || new_roots != self.new_roots {
            anyhow::bail!(
                "pending profile root-pair commit is not bound to the authorized repair roots"
            );
        }

        let current = read_profile_index_root_pair_snapshot(by_pubkey_path, search_path)?;
        let search_first = profile_index_roots_from_cids(
            self.old_roots.by_pubkey.clone(),
            self.new_roots.search.clone(),
        )?;
        if current != self.old_roots && current != search_first && current != self.new_roots {
            anyhow::bail!("profile root-pair files are not an authorized repair forward state");
        }
        Ok(())
    }

    fn require_write_target(
        &self,
        by_pubkey_root: Option<&Cid>,
        search_root: Option<&Cid>,
        current: &ProfileIndexRoots,
    ) -> Result<()> {
        let target = profile_index_roots_from_cids(by_pubkey_root.cloned(), search_root.cloned())?;
        if self.phase != ProfileIndexRepairAuthorityPhase::Commit
            || target != self.new_roots
            || *current != self.old_roots
        {
            anyhow::bail!("profile root-pair write is not the exact authorized repair transition");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PreparedProfileIndexRepair {
    old_roots: ProfileIndexRoots,
    new_roots: ProfileIndexRoots,
}

impl PreparedProfileIndexRepair {
    /// Reconstitute an unpublished repair pair from already validated durable
    /// intent pins. Publication still requires an opaque authority minted from
    /// the exact on-disk evidence.
    pub fn from_roots(old_roots: ProfileIndexRoots, new_roots: ProfileIndexRoots) -> Self {
        Self {
            old_roots,
            new_roots,
        }
    }

    pub fn old_roots(&self) -> &ProfileIndexRoots {
        &self.old_roots
    }

    pub fn new_roots(&self) -> &ProfileIndexRoots {
        &self.new_roots
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileIndexRepairCommitOutcome {
    Applied,
    AlreadyApplied,
}

pub struct ProfileIndexRepairPublicationGuard {
    by_pubkey_root_path: PathBuf,
    search_root_path: PathBuf,
    installed_roots: ProfileIndexRoots,
    outcome: ProfileIndexRepairCommitOutcome,
    _transaction: ProfileRootPairTransactionGuard,
}

impl ProfileIndexRepairPublicationGuard {
    pub fn outcome(&self) -> ProfileIndexRepairCommitOutcome {
        self.outcome
    }

    pub fn installed_roots(&self) -> &ProfileIndexRoots {
        &self.installed_roots
    }

    pub fn require_unchanged(&self) -> Result<()> {
        let (by_pubkey, by_pubkey_file_sha256) =
            read_root_file_snapshot(&self.by_pubkey_root_path)?;
        let (search, search_file_sha256) = read_root_file_snapshot(&self.search_root_path)?;
        let current = ProfileIndexRoots {
            by_pubkey,
            search,
            by_pubkey_file_sha256,
            search_file_sha256,
        };
        if current != self.installed_roots {
            anyhow::bail!("published profile roots changed while repair publication was locked");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProfileRootPairLockMode {
    Shared,
    Exclusive,
}

struct ProfileRootPairTransactionGuard {
    _process_read: Option<tokio::sync::OwnedRwLockReadGuard<()>>,
    _process_write: Option<tokio::sync::OwnedRwLockWriteGuard<()>>,
    file: File,
}

impl Drop for ProfileRootPairTransactionGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

fn profile_root_pair_process_locks(
) -> &'static StdMutex<HashMap<PathBuf, Weak<tokio::sync::RwLock<()>>>> {
    static LOCKS: OnceLock<StdMutex<HashMap<PathBuf, Weak<tokio::sync::RwLock<()>>>>> =
        OnceLock::new();
    LOCKS.get_or_init(|| StdMutex::new(HashMap::new()))
}

fn profile_root_pair_process_lock(
    root_pair_lock_path: &Path,
) -> Result<Arc<tokio::sync::RwLock<()>>> {
    let db_dir = root_pair_lock_path
        .parent()
        .with_context(|| format!("{} has no parent directory", root_pair_lock_path.display()))?;
    let canonical_db_dir = std::fs::canonicalize(db_dir)
        .with_context(|| format!("canonicalize profile index directory {}", db_dir.display()))?;
    let lock_file_name = root_pair_lock_path.file_name().with_context(|| {
        format!(
            "{} has no profile transaction lock file name",
            root_pair_lock_path.display()
        )
    })?;
    let canonical_lock_path = canonical_db_dir.join(lock_file_name);
    let mut locks = profile_root_pair_process_locks()
        .lock()
        .map_err(|_| anyhow::anyhow!("profile root-pair process lock registry was poisoned"))?;
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(&canonical_lock_path).and_then(Weak::upgrade) {
        return Ok(lock);
    }
    let lock = Arc::new(tokio::sync::RwLock::new(()));
    locks.insert(canonical_lock_path, Arc::downgrade(&lock));
    Ok(lock)
}

#[cfg(test)]
type ProfileRootPairTransactionProbe =
    Arc<dyn Fn(&Path, ProfileRootPairLockMode) + Send + Sync + 'static>;

#[cfg(test)]
fn profile_root_pair_transaction_probe(
) -> &'static StdMutex<Option<ProfileRootPairTransactionProbe>> {
    static PROBE: OnceLock<StdMutex<Option<ProfileRootPairTransactionProbe>>> = OnceLock::new();
    PROBE.get_or_init(|| StdMutex::new(None))
}

#[cfg(test)]
struct ProfileRootPairTransactionProbeGuard;

#[cfg(test)]
impl Drop for ProfileRootPairTransactionProbeGuard {
    fn drop(&mut self) {
        if let Ok(mut probe) = profile_root_pair_transaction_probe().lock() {
            *probe = None;
        }
    }
}

#[cfg(test)]
fn install_profile_root_pair_transaction_probe(
    probe: ProfileRootPairTransactionProbe,
) -> ProfileRootPairTransactionProbeGuard {
    *profile_root_pair_transaction_probe()
        .lock()
        .expect("profile root-pair transaction probe lock poisoned") = Some(probe);
    ProfileRootPairTransactionProbeGuard
}

#[cfg(test)]
fn run_profile_root_pair_transaction_probe(path: &Path, mode: ProfileRootPairLockMode) {
    let probe = profile_root_pair_transaction_probe()
        .lock()
        .expect("profile root-pair transaction probe lock poisoned")
        .clone();
    if let Some(probe) = probe {
        probe(path, mode);
    }
}

#[cfg(test)]
type PendingProfileProjectionPersistedProbe =
    Arc<dyn Fn(&Path) -> Result<()> + Send + Sync + 'static>;

#[cfg(test)]
fn pending_profile_projection_persisted_probe(
) -> &'static StdMutex<Option<PendingProfileProjectionPersistedProbe>> {
    static PROBE: OnceLock<StdMutex<Option<PendingProfileProjectionPersistedProbe>>> =
        OnceLock::new();
    PROBE.get_or_init(|| StdMutex::new(None))
}

#[cfg(test)]
struct PendingProfileProjectionPersistedProbeGuard;

#[cfg(test)]
impl Drop for PendingProfileProjectionPersistedProbeGuard {
    fn drop(&mut self) {
        if let Ok(mut probe) = pending_profile_projection_persisted_probe().lock() {
            *probe = None;
        }
    }
}

#[cfg(test)]
fn install_pending_profile_projection_persisted_probe(
    probe: PendingProfileProjectionPersistedProbe,
) -> PendingProfileProjectionPersistedProbeGuard {
    *pending_profile_projection_persisted_probe()
        .lock()
        .expect("pending profile projection probe lock poisoned") = Some(probe);
    PendingProfileProjectionPersistedProbeGuard
}

#[cfg(test)]
fn run_pending_profile_projection_persisted_probe(path: &Path) -> Result<()> {
    let probe = pending_profile_projection_persisted_probe()
        .lock()
        .map_err(|_| anyhow::anyhow!("pending profile projection probe lock poisoned"))?
        .clone();
    if let Some(probe) = probe {
        probe(path)?;
    }
    Ok(())
}

fn try_open_and_lock_profile_root_pair_file(
    root_pair_lock_path: &Path,
    mode: ProfileRootPairLockMode,
    create: bool,
) -> Result<Option<File>> {
    let mut options = OpenOptions::new();
    options.read(true);
    if create {
        options.write(true).create(true).truncate(false);
    }
    let file = options.open(root_pair_lock_path).with_context(|| {
        format!(
            "open {} profile root-pair transaction lock {}",
            if create { "writable" } else { "existing" },
            root_pair_lock_path.display()
        )
    })?;

    #[cfg(unix)]
    {
        let operation = match mode {
            ProfileRootPairLockMode::Shared => libc::LOCK_SH | libc::LOCK_NB,
            ProfileRootPairLockMode::Exclusive => libc::LOCK_EX | libc::LOCK_NB,
        };
        let result = unsafe { libc::flock(file.as_raw_fd(), operation) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(error).with_context(|| {
                format!(
                    "lock profile root-pair transaction ({:?}) at {}",
                    mode,
                    root_pair_lock_path.display()
                )
            });
        }
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
        anyhow::bail!(
            "profile root-pair transactions require an operating-system advisory file lock"
        );
    }

    Ok(Some(file))
}

fn profile_root_pair_lock_timeout_error(
    root_pair_lock_path: &Path,
    mode: ProfileRootPairLockMode,
    timeout: Duration,
) -> anyhow::Error {
    anyhow::anyhow!(
        "timed out after {} ms waiting for profile root-pair transaction ({:?}) at {}",
        timeout.as_millis(),
        mode,
        root_pair_lock_path.display()
    )
}

fn try_acquire_profile_root_pair_lock_once(
    process_lock: &Arc<tokio::sync::RwLock<()>>,
    root_pair_lock_path: &Path,
    mode: ProfileRootPairLockMode,
    create: bool,
) -> Result<Option<ProfileRootPairTransactionGuard>> {
    let (process_read, process_write) = match mode {
        ProfileRootPairLockMode::Shared => {
            let Ok(guard) = Arc::clone(process_lock).try_read_owned() else {
                return Ok(None);
            };
            (Some(guard), None)
        }
        ProfileRootPairLockMode::Exclusive => {
            let Ok(guard) = Arc::clone(process_lock).try_write_owned() else {
                return Ok(None);
            };
            (None, Some(guard))
        }
    };
    let Some(file) = try_open_and_lock_profile_root_pair_file(root_pair_lock_path, mode, create)?
    else {
        return Ok(None);
    };
    let guard = ProfileRootPairTransactionGuard {
        _process_read: process_read,
        _process_write: process_write,
        file,
    };
    #[cfg(test)]
    run_profile_root_pair_transaction_probe(root_pair_lock_path, mode);
    Ok(Some(guard))
}

fn acquire_profile_root_pair_lock_with_timeout(
    root_pair_lock_path: &Path,
    mode: ProfileRootPairLockMode,
    create: bool,
    timeout: Duration,
) -> Result<ProfileRootPairTransactionGuard> {
    let process_lock = profile_root_pair_process_lock(root_pair_lock_path)?;
    let started = Instant::now();
    loop {
        if let Some(guard) = try_acquire_profile_root_pair_lock_once(
            &process_lock,
            root_pair_lock_path,
            mode,
            create,
        )? {
            return Ok(guard);
        }
        let elapsed = started.elapsed();
        if elapsed >= timeout {
            return Err(profile_root_pair_lock_timeout_error(
                root_pair_lock_path,
                mode,
                timeout,
            ));
        }
        std::thread::sleep(
            PROFILE_ROOT_PAIR_LOCK_RETRY_INTERVAL.min(timeout.saturating_sub(elapsed)),
        );
    }
}

fn acquire_profile_root_pair_lock(
    root_pair_lock_path: &Path,
    mode: ProfileRootPairLockMode,
    create: bool,
) -> Result<ProfileRootPairTransactionGuard> {
    acquire_profile_root_pair_lock_with_timeout(
        root_pair_lock_path,
        mode,
        create,
        PROFILE_ROOT_PAIR_LOCK_TIMEOUT,
    )
}

async fn acquire_profile_root_pair_lock_async_with_timeout(
    root_pair_lock_path: &Path,
    mode: ProfileRootPairLockMode,
    create: bool,
    timeout: Duration,
) -> Result<ProfileRootPairTransactionGuard> {
    let process_lock = profile_root_pair_process_lock(root_pair_lock_path)?;
    let started = Instant::now();
    loop {
        if let Some(guard) = try_acquire_profile_root_pair_lock_once(
            &process_lock,
            root_pair_lock_path,
            mode,
            create,
        )? {
            return Ok(guard);
        }
        let elapsed = started.elapsed();
        if elapsed >= timeout {
            return Err(profile_root_pair_lock_timeout_error(
                root_pair_lock_path,
                mode,
                timeout,
            ));
        }
        tokio::time::sleep(
            PROFILE_ROOT_PAIR_LOCK_RETRY_INTERVAL.min(timeout.saturating_sub(elapsed)),
        )
        .await;
    }
}

async fn acquire_profile_root_pair_lock_async(
    root_pair_lock_path: &Path,
    mode: ProfileRootPairLockMode,
    create: bool,
) -> Result<ProfileRootPairTransactionGuard> {
    acquire_profile_root_pair_lock_async_with_timeout(
        root_pair_lock_path,
        mode,
        create,
        PROFILE_ROOT_PAIR_LOCK_TIMEOUT,
    )
    .await
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SocialGraphStats {
    pub total_users: usize,
    pub root: Option<String>,
    pub total_follows: usize,
    pub max_depth: u32,
    pub size_by_distance: BTreeMap<u32, usize>,
    pub enabled: bool,
}

#[derive(Debug, Clone)]
struct DistanceCache {
    stats: SocialGraphStats,
    users_by_distance: BTreeMap<u32, Vec<[u8; 32]>>,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct UpstreamGraphBackendError(String);

pub struct SocialGraphStore {
    graph: StdMutex<HeedSocialGraph>,
    // `HeedSocialGraph` owns a raw Heed handle. Declaring it before this
    // managed clone makes Rust drop the graph first, then close Heed's cache.
    _graph_env_lifecycle: ManagedEnv,
    ambient_store: Arc<StorageRouter>,
    distance_cache: StdMutex<Option<DistanceCache>>,
    public_events: EventIndexBucket,
    ambient_events: EventIndexBucket,
    profile_index: ProfileIndexBucket,
    profile_index_overmute_threshold: StdMutex<f64>,
}

pub trait SocialGraphBackend: Send + Sync {
    fn stats(&self) -> Result<SocialGraphStats>;
    fn users_by_follow_distance(&self, distance: u32) -> Result<Vec<[u8; 32]>>;
    fn follow_distance(&self, pk_bytes: &[u8; 32]) -> Result<Option<u32>>;
    fn follow_list_created_at(&self, owner: &[u8; 32]) -> Result<Option<u64>>;
    fn followed_targets(&self, owner: &[u8; 32]) -> Result<UserSet>;
    fn is_overmuted_user(&self, user_pk: &[u8; 32], threshold: f64) -> Result<bool>;
    fn profile_search_root(&self) -> Result<Option<Cid>> {
        Ok(None)
    }
    fn snapshot_chunks(&self, root: &[u8; 32], options: &BinaryBudget) -> Result<Vec<Bytes>>;
    fn ingest_event(&self, event: &Event) -> Result<()>;
    fn ingest_event_with_storage_class(
        &self,
        event: &Event,
        storage_class: EventStorageClass,
    ) -> Result<()> {
        let _ = storage_class;
        self.ingest_event(event)
    }
    fn ingest_events(&self, events: &[Event]) -> Result<()> {
        for event in events {
            self.ingest_event(event)?;
        }
        Ok(())
    }
    fn ingest_events_with_storage_class(
        &self,
        events: &[Event],
        storage_class: EventStorageClass,
    ) -> Result<()> {
        for event in events {
            self.ingest_event_with_storage_class(event, storage_class)?;
        }
        Ok(())
    }
    fn ingest_graph_events(&self, events: &[Event]) -> Result<()> {
        self.ingest_events(events)
    }
    fn query_events(&self, filter: &Filter, limit: usize) -> Result<Vec<Event>>;
}

#[cfg(test)]
pub type TestLockGuard = tokio::sync::MutexGuard<'static, ()>;

#[cfg(test)]
static NDB_TEST_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

#[cfg(test)]
fn test_mutex() -> &'static tokio::sync::Mutex<()> {
    NDB_TEST_LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

#[cfg(test)]
pub async fn test_lock() -> TestLockGuard {
    test_mutex().lock().await
}

#[cfg(test)]
pub fn test_lock_blocking() -> TestLockGuard {
    test_mutex().blocking_lock()
}

pub fn open_social_graph_store(data_dir: &Path) -> Result<Arc<SocialGraphStore>> {
    open_social_graph_store_with_mapsize(data_dir, None)
}

/// Read the two published profile roots without opening the writable social
/// graph LMDB environment.
pub fn read_profile_index_roots(data_dir: &Path) -> Result<ProfileIndexRoots> {
    read_profile_index_roots_with_timeout(data_dir, PROFILE_ROOT_PAIR_LOCK_TIMEOUT)
}

pub fn profile_index_root_file_sha256(root: &Cid) -> Result<String> {
    Ok(to_hex(&sha256(&encode_cid(root)?)))
}

fn read_profile_index_roots_with_timeout(
    data_dir: &Path,
    timeout: Duration,
) -> Result<ProfileIndexRoots> {
    let db_dir = data_dir.join("socialgraph");
    match std::fs::symlink_metadata(&db_dir) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ProfileIndexRoots {
                by_pubkey: None,
                search: None,
                by_pubkey_file_sha256: None,
                search_file_sha256: None,
            });
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect profile index directory {}", db_dir.display()));
        }
    }
    let _transaction = acquire_profile_root_pair_lock_with_timeout(
        &db_dir.join(PROFILE_ROOT_PAIR_LOCK_FILE),
        ProfileRootPairLockMode::Shared,
        false,
        timeout,
    )?;
    require_no_pending_profile_root_pair_commit(&db_dir)?;
    require_no_pending_profile_projection(&db_dir)?;
    let (by_pubkey, by_pubkey_file_sha256) =
        read_root_file_snapshot(&db_dir.join(PROFILES_BY_PUBKEY_ROOT_FILE))?;
    let (search, search_file_sha256) =
        read_root_file_snapshot(&db_dir.join(PROFILE_SEARCH_ROOT_FILE))?;
    Ok(ProfileIndexRoots {
        by_pubkey,
        search,
        by_pubkey_file_sha256,
        search_file_sha256,
    })
}

/// Validate both profile indexes against one real metadata event using only a
/// caller-provided blob store. This does not open or mutate the social graph.
pub async fn validate_profile_indexes_read_only<S: Store>(
    data_dir: &Path,
    store: Arc<S>,
    event: &Event,
) -> Result<StoredProfileSearchEntry> {
    let roots = read_profile_index_roots(data_dir)?;
    validate_profile_indexes_at_roots(store, &roots, event).await
}

/// Validate both profile indexes against one metadata event at an explicitly
/// pinned root pair. This variant never reads the mutable root files, so it is
/// safe to use while a repair publication guard holds their exclusive lock.
pub async fn validate_profile_indexes_at_roots<S: Store>(
    store: Arc<S>,
    roots: &ProfileIndexRoots,
    event: &Event,
) -> Result<StoredProfileSearchEntry> {
    if event.kind != Kind::Metadata {
        anyhow::bail!("profile index validation requires a kind-0 metadata event");
    }
    let by_pubkey_root = roots
        .by_pubkey
        .clone()
        .context("profile-by-pubkey root is missing")?;
    let search_root = roots
        .search
        .clone()
        .context("profile-search root is missing")?;
    let index = BTree::new(
        Arc::clone(&store),
        hashtree_index::BTreeOptions {
            order: Some(PROFILE_SEARCH_INDEX_ORDER),
        },
    );
    let pubkey = event.pubkey.to_hex();
    let mirrored_cid = index
        .get_link(Some(&by_pubkey_root), &pubkey)
        .await
        .context("query profile-by-pubkey root")?
        .with_context(|| format!("profile-by-pubkey omitted {pubkey}"))?;
    let tree = HashTree::new(HashTreeConfig::new(store));
    let mirrored_bytes = tree
        .get(&mirrored_cid, None)
        .await
        .context("read mirrored profile event")?
        .with_context(|| format!("mirrored profile blob for {pubkey} is missing"))?;
    let mirrored = Event::from_json(
        String::from_utf8(mirrored_bytes).context("decode mirrored profile event as utf-8")?,
    )
    .context("decode mirrored profile event json")?;
    if mirrored != *event {
        anyhow::bail!(
            "profile-by-pubkey returned event {} with different bytes than {} for {pubkey}",
            mirrored.id,
            event.id
        );
    }

    let term = profile_search_terms_for_event(event)
        .into_iter()
        .next()
        .with_context(|| format!("profile {pubkey} did not produce a search term"))?;
    let exact_key = format!("{PROFILE_SEARCH_PREFIX}{term}:{pubkey}");
    let encoded = index
        .get(Some(&search_root), &exact_key)
        .await
        .context("query profile-search root")?
        .with_context(|| format!("profile-search omitted exact key {exact_key}"))?;
    let entry: StoredProfileSearchEntry =
        serde_json::from_str(&encoded).context("decode stored profile search entry JSON")?;
    let expected_nhash = nhash_encode_full(&NHashData {
        hash: mirrored_cid.hash,
        decrypt_key: mirrored_cid.key,
    })
    .context("encode mirrored profile event nhash")?;
    if entry.pubkey != pubkey
        || entry.created_at != event.created_at.as_secs()
        || entry.event_nhash != expected_nhash
    {
        anyhow::bail!(
            "profile-search entry for {exact_key} does not match its profile-by-pubkey event"
        );
    }
    Ok(entry)
}

pub fn open_social_graph_store_with_mapsize(
    data_dir: &Path,
    mapsize_bytes: Option<u64>,
) -> Result<Arc<SocialGraphStore>> {
    let db_dir = data_dir.join("socialgraph");
    open_social_graph_store_at_path(&db_dir, mapsize_bytes)
}

pub fn open_social_graph_store_with_storage(
    data_dir: &Path,
    store: Arc<StorageRouter>,
    mapsize_bytes: Option<u64>,
) -> Result<Arc<SocialGraphStore>> {
    let db_dir = data_dir.join("socialgraph");
    open_social_graph_store_at_path_with_storage(&db_dir, store, mapsize_bytes)
}

#[cfg(test)]
pub fn open_test_social_graph_store(data_dir: &Path) -> Result<Arc<SocialGraphStore>> {
    open_test_social_graph_store_with_mapsize(data_dir, None)
}

#[cfg(test)]
pub fn open_test_social_graph_store_with_mapsize(
    data_dir: &Path,
    mapsize_bytes: Option<u64>,
) -> Result<Arc<SocialGraphStore>> {
    open_test_social_graph_store_at_path(&data_dir.join("socialgraph"), mapsize_bytes)
}

#[cfg(test)]
pub fn open_test_social_graph_store_with_storage(
    data_dir: &Path,
    store: Arc<StorageRouter>,
    mapsize_bytes: Option<u64>,
) -> Result<Arc<SocialGraphStore>> {
    open_embedded_social_graph_store_with_storage(data_dir, store, mapsize_bytes)
}

#[cfg(test)]
pub fn open_test_social_graph_store_at_path(
    db_dir: &Path,
    mapsize_bytes: Option<u64>,
) -> Result<Arc<SocialGraphStore>> {
    open_embedded_social_graph_store_at_path(db_dir, mapsize_bytes)
}

pub fn open_embedded_social_graph_store_with_storage(
    data_dir: &Path,
    store: Arc<StorageRouter>,
    mapsize_bytes: Option<u64>,
) -> Result<Arc<SocialGraphStore>> {
    let db_dir = data_dir.join("socialgraph");
    open_social_graph_store_at_path_with_storage_and_env_flags(
        &db_dir,
        store,
        mapsize_bytes,
        EnvFlags::NO_LOCK,
    )
}

pub fn open_social_graph_store_at_path(
    db_dir: &Path,
    mapsize_bytes: Option<u64>,
) -> Result<Arc<SocialGraphStore>> {
    let config = hashtree_config::Config::load_or_default();
    let backend = &config.storage.backend;
    let local_store = Arc::new(
        LocalStore::new_with_lmdb_map_size(db_dir.join("blobs"), backend, mapsize_bytes)
            .map_err(|err| anyhow::anyhow!("Failed to create social graph blob store: {err}"))?,
    );
    let store = Arc::new(StorageRouter::new(local_store));
    open_social_graph_store_at_path_with_storage(db_dir, store, mapsize_bytes)
}

pub fn open_embedded_social_graph_store_at_path(
    db_dir: &Path,
    mapsize_bytes: Option<u64>,
) -> Result<Arc<SocialGraphStore>> {
    let local_store = Arc::new(
        LocalStore::new_with_lmdb_map_size(
            db_dir.join("blobs"),
            &hashtree_config::StorageBackend::Fs,
            mapsize_bytes,
        )
        .map_err(|err| anyhow::anyhow!("Failed to create social graph blob store: {err}"))?,
    );
    let store = Arc::new(StorageRouter::new(local_store));
    open_social_graph_store_at_path_with_storage_and_env_flags(
        db_dir,
        store,
        mapsize_bytes,
        EnvFlags::NO_LOCK,
    )
}

pub fn open_social_graph_store_at_path_with_storage(
    db_dir: &Path,
    store: Arc<StorageRouter>,
    mapsize_bytes: Option<u64>,
) -> Result<Arc<SocialGraphStore>> {
    open_social_graph_store_at_path_with_storage_and_env_flags(
        db_dir,
        store,
        mapsize_bytes,
        EnvFlags::empty(),
    )
}

fn open_social_graph_store_at_path_with_storage_and_env_flags(
    db_dir: &Path,
    store: Arc<StorageRouter>,
    mapsize_bytes: Option<u64>,
    env_flags: EnvFlags,
) -> Result<Arc<SocialGraphStore>> {
    let ambient_backend = store.local_store().backend();
    let ambient_local = Arc::new(
        LocalStore::new_with_lmdb_map_size(
            db_dir.join(AMBIENT_EVENTS_BLOB_DIR),
            &ambient_backend,
            mapsize_bytes,
        )
        .map_err(|err| {
            anyhow::anyhow!("Failed to create social graph ambient blob store: {err}")
        })?,
    );
    let ambient_store = Arc::new(StorageRouter::new(ambient_local));
    open_social_graph_store_at_path_with_storage_split_and_env_flags(
        db_dir,
        store,
        ambient_store,
        mapsize_bytes,
        env_flags,
    )
}

pub fn open_social_graph_store_at_path_with_storage_split(
    db_dir: &Path,
    public_store: Arc<StorageRouter>,
    ambient_store: Arc<StorageRouter>,
    mapsize_bytes: Option<u64>,
) -> Result<Arc<SocialGraphStore>> {
    open_social_graph_store_at_path_with_storage_split_and_env_flags(
        db_dir,
        public_store,
        ambient_store,
        mapsize_bytes,
        EnvFlags::empty(),
    )
}

fn open_social_graph_store_at_path_with_storage_split_and_env_flags(
    db_dir: &Path,
    public_store: Arc<StorageRouter>,
    ambient_store: Arc<StorageRouter>,
    mapsize_bytes: Option<u64>,
    env_flags: EnvFlags,
) -> Result<Arc<SocialGraphStore>> {
    std::fs::create_dir_all(db_dir)?;
    let _root_transaction = acquire_profile_root_pair_lock(
        &db_dir.join(PROFILE_ROOT_PAIR_LOCK_FILE),
        ProfileRootPairLockMode::Exclusive,
        true,
    )?;
    if incomplete_profile_repair_intent_path(&db_dir.join(PROFILE_ROOT_PAIR_LOCK_FILE))?.is_none() {
        recover_profile_root_pair_commit_locked(db_dir)?;
    }
    if let Some(size) = mapsize_bytes {
        ensure_social_graph_mapsize_with_env_flags(db_dir, size, env_flags)?;
    }
    let graph_map_size = social_graph_map_size(mapsize_bytes)?;
    let graph = unsafe {
        HeedSocialGraph::open_with_env_flags_and_map_size(
            db_dir,
            DEFAULT_ROOT_HEX,
            env_flags,
            graph_map_size,
        )
    }
    .context("open nostr-social-graph heed backend")?;
    let mut lifecycle_options = heed::EnvOpenOptions::new();
    lifecycle_options
        .map_size(graph_map_size)
        .max_dbs(SOCIALGRAPH_MAX_DBS);
    unsafe {
        lifecycle_options.flags(env_flags);
    }
    let graph_env_lifecycle = unsafe { ManagedEnv::open(&lifecycle_options, db_dir) }
        .context("manage nostr-social-graph heed backend lifecycle")?;

    let graph_store = Arc::new(SocialGraphStore {
        graph: StdMutex::new(graph),
        _graph_env_lifecycle: graph_env_lifecycle,
        ambient_store: Arc::clone(&ambient_store),
        distance_cache: StdMutex::new(None),
        public_events: EventIndexBucket {
            event_store: NostrEventStore::new(Arc::clone(&public_store)),
            root_path: db_dir.join(EVENTS_ROOT_FILE),
        },
        ambient_events: EventIndexBucket {
            event_store: NostrEventStore::new(ambient_store),
            root_path: db_dir.join(AMBIENT_EVENTS_ROOT_FILE),
        },
        profile_index: ProfileIndexBucket {
            store: Arc::clone(&public_store),
            tree: HashTree::new(HashTreeConfig::new(Arc::clone(&public_store))),
            index: BTree::new(
                public_store,
                hashtree_index::BTreeOptions {
                    order: Some(PROFILE_SEARCH_INDEX_ORDER),
                },
            ),
            by_pubkey_root_path: db_dir.join(PROFILES_BY_PUBKEY_ROOT_FILE),
            search_root_path: db_dir.join(PROFILE_SEARCH_ROOT_FILE),
            root_pair_commit_path: db_dir.join(PROFILE_ROOT_PAIR_COMMIT_FILE),
            root_pair_lock_path: db_dir.join(PROFILE_ROOT_PAIR_LOCK_FILE),
        },
        profile_index_overmute_threshold: StdMutex::new(1.0),
    });
    graph_store.recover_pending_profile_projection_locked()?;
    Ok(graph_store)
}

pub fn set_social_graph_root(store: &SocialGraphStore, pk_bytes: &[u8; 32]) {
    if let Err(err) = store.set_root(pk_bytes) {
        tracing::warn!("Failed to set social graph root: {err}");
    }
}

pub fn get_follow_distance(
    backend: &(impl SocialGraphBackend + ?Sized),
    pk_bytes: &[u8; 32],
) -> Option<u32> {
    backend.follow_distance(pk_bytes).ok().flatten()
}

pub fn get_follows(
    backend: &(impl SocialGraphBackend + ?Sized),
    pk_bytes: &[u8; 32],
) -> Vec<[u8; 32]> {
    match backend.followed_targets(pk_bytes) {
        Ok(set) => set.into_iter().collect(),
        Err(_) => Vec::new(),
    }
}

pub fn is_overmuted(
    backend: &(impl SocialGraphBackend + ?Sized),
    _root_pk: &[u8; 32],
    user_pk: &[u8; 32],
    threshold: f64,
) -> bool {
    backend
        .is_overmuted_user(user_pk, threshold)
        .unwrap_or(false)
}

pub fn ingest_event(backend: &(impl SocialGraphBackend + ?Sized), _sub_id: &str, event_json: &str) {
    let event = match Event::from_json(event_json) {
        Ok(event) => event,
        Err(_) => return,
    };

    if let Err(err) = backend.ingest_event(&event) {
        tracing::warn!("Failed to ingest social graph event: {err}");
    }
}

pub fn ingest_parsed_event(
    backend: &(impl SocialGraphBackend + ?Sized),
    event: &Event,
) -> Result<()> {
    backend.ingest_event(event)
}

pub fn ingest_parsed_event_with_storage_class(
    backend: &(impl SocialGraphBackend + ?Sized),
    event: &Event,
    storage_class: EventStorageClass,
) -> Result<()> {
    backend.ingest_event_with_storage_class(event, storage_class)
}

pub fn ingest_parsed_events(
    backend: &(impl SocialGraphBackend + ?Sized),
    events: &[Event],
) -> Result<()> {
    backend.ingest_events(events)
}

pub fn ingest_parsed_events_with_storage_class(
    backend: &(impl SocialGraphBackend + ?Sized),
    events: &[Event],
    storage_class: EventStorageClass,
) -> Result<()> {
    backend.ingest_events_with_storage_class(events, storage_class)
}

pub fn ingest_graph_parsed_events(
    backend: &(impl SocialGraphBackend + ?Sized),
    events: &[Event],
) -> Result<()> {
    backend.ingest_graph_events(events)
}

pub fn query_events(
    backend: &(impl SocialGraphBackend + ?Sized),
    filter: &Filter,
    limit: usize,
) -> Vec<Event> {
    backend.query_events(filter, limit).unwrap_or_default()
}

impl SocialGraphStore {
    /// Forces graph-owned LMDB state to durable storage.
    ///
    /// The public event/profile blobs are owned by the caller's
    /// `HashtreeStore` and must be synced by that store separately.
    pub fn force_sync(&self) -> Result<()> {
        self._graph_env_lifecycle
            .force_sync()
            .context("force-sync social graph database")?;
        self.ambient_store
            .force_sync()
            .map_err(|err| anyhow::anyhow!("force-sync ambient event storage: {err}"))
    }

    pub fn set_profile_index_overmute_threshold(&self, threshold: f64) {
        *self
            .profile_index_overmute_threshold
            .lock()
            .expect("profile index overmute threshold") = threshold;
    }

    fn profile_index_overmute_threshold(&self) -> f64 {
        *self
            .profile_index_overmute_threshold
            .lock()
            .expect("profile index overmute threshold")
    }

    fn invalidate_distance_cache(&self) {
        *self.distance_cache.lock().unwrap() = None;
    }

    fn build_distance_cache(state: nostr_social_graph::SocialGraphState) -> Result<DistanceCache> {
        let unique_ids = state
            .unique_ids
            .into_iter()
            .map(|(pubkey, id)| decode_pubkey(&pubkey).map(|decoded| (id, decoded)))
            .collect::<Result<HashMap<_, _>>>()?;

        let mut users_by_distance = BTreeMap::new();
        let mut size_by_distance = BTreeMap::new();
        for (distance, users) in state.users_by_follow_distance {
            let decoded = users
                .into_iter()
                .filter_map(|id| unique_ids.get(&id).copied())
                .collect::<Vec<_>>();
            size_by_distance.insert(distance, decoded.len());
            users_by_distance.insert(distance, decoded);
        }

        let total_follows = state
            .followed_by_user
            .iter()
            .map(|(_, targets)| targets.len())
            .sum::<usize>();
        let total_users = size_by_distance.values().copied().sum();
        let max_depth = size_by_distance.keys().copied().max().unwrap_or_default();

        Ok(DistanceCache {
            stats: SocialGraphStats {
                total_users,
                root: Some(state.root),
                total_follows,
                max_depth,
                size_by_distance,
                enabled: true,
            },
            users_by_distance,
        })
    }

    fn load_distance_cache(&self) -> Result<DistanceCache> {
        if let Some(cache) = self.distance_cache.lock().unwrap().clone() {
            return Ok(cache);
        }

        let state = {
            let graph = self.graph.lock().unwrap();
            graph.export_state().context("export social graph state")?
        };
        let cache = Self::build_distance_cache(state)?;
        *self.distance_cache.lock().unwrap() = Some(cache.clone());
        Ok(cache)
    }

    fn set_root(&self, root: &[u8; 32]) -> Result<()> {
        let _transaction = self
            .profile_index
            .acquire_exclusive_root_pair_transaction()?;
        require_no_incomplete_profile_repair_for_root_write(
            &self.profile_index.root_pair_lock_path,
        )?;
        self.recover_profile_transactions_locked()?;
        let root_hex = hex::encode(root);
        {
            let mut graph = self.graph.lock().unwrap();
            if should_replace_placeholder_root(&graph)? {
                let fresh = SocialGraph::new(&root_hex);
                graph
                    .replace_state(&fresh.export_state())
                    .context("replace placeholder social graph root")?;
            } else {
                graph
                    .set_root(&root_hex)
                    .context("set nostr-social-graph root")?;
            }
        }
        self.invalidate_distance_cache();
        Ok(())
    }

    fn stats(&self) -> Result<SocialGraphStats> {
        Ok(self.load_distance_cache()?.stats)
    }

    fn follow_distance(&self, pk_bytes: &[u8; 32]) -> Result<Option<u32>> {
        let graph = self.graph.lock().unwrap();
        let distance = graph
            .get_follow_distance(&hex::encode(pk_bytes))
            .context("read social graph follow distance")?;
        Ok((distance != UNKNOWN_FOLLOW_DISTANCE).then_some(distance))
    }

    fn users_by_follow_distance(&self, distance: u32) -> Result<Vec<[u8; 32]>> {
        Ok(self
            .load_distance_cache()?
            .users_by_distance
            .get(&distance)
            .cloned()
            .unwrap_or_default())
    }

    fn follow_list_created_at(&self, owner: &[u8; 32]) -> Result<Option<u64>> {
        let graph = self.graph.lock().unwrap();
        graph
            .get_follow_list_created_at(&hex::encode(owner))
            .context("read social graph follow list timestamp")
    }

    fn followed_targets(&self, owner: &[u8; 32]) -> Result<UserSet> {
        let graph = self.graph.lock().unwrap();
        decode_pubkey_set(
            graph
                .get_followed_by_user(&hex::encode(owner))
                .context("read followed targets")?,
        )
    }

    fn is_overmuted_user(&self, user_pk: &[u8; 32], threshold: f64) -> Result<bool> {
        if threshold <= 0.0 {
            return Ok(false);
        }
        let graph = self.graph.lock().unwrap();
        graph
            .is_overmuted(&hex::encode(user_pk), threshold)
            .context("check social graph overmute")
    }

    fn recovered_profile_index_roots(&self) -> Result<(Option<Cid>, Option<Cid>)> {
        let _transaction = self
            .profile_index
            .acquire_exclusive_root_pair_transaction()?;
        self.recover_profile_transactions_locked()?;
        self.profile_index.roots_locked()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn profile_search_root(&self) -> Result<Option<Cid>> {
        Ok(self.recovered_profile_index_roots()?.1)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn profiles_by_pubkey_root(&self) -> Result<Option<Cid>> {
        Ok(self.recovered_profile_index_roots()?.0)
    }

    pub fn public_events_root(&self) -> Result<Option<Cid>> {
        self.public_events.events_root()
    }

    #[cfg_attr(test, allow(dead_code))]
    pub(crate) fn public_events_root_for_write(&self) -> Result<Option<Cid>> {
        let _transaction = self
            .profile_index
            .acquire_exclusive_root_pair_transaction()?;
        require_no_incomplete_profile_repair_for_root_write(
            &self.profile_index.root_pair_lock_path,
        )?;
        self.recover_profile_transactions_locked()?;
        self.public_events.events_root_for_write()
    }

    #[cfg(test)]
    pub(crate) fn write_public_events_root(&self, root: Option<&Cid>) -> Result<()> {
        self.public_events.write_events_root(root)
    }

    fn pending_profile_projection_path(&self) -> PathBuf {
        self.profile_index
            .root_pair_lock_path
            .with_file_name(PROFILE_PROJECTION_PENDING_FILE)
    }

    fn force_sync_event_storage(&self, storage_class: EventStorageClass) -> Result<()> {
        let store = match storage_class {
            EventStorageClass::Public => &self.profile_index.store,
            EventStorageClass::Ambient => &self.ambient_store,
        };
        store
            .force_sync()
            .map_err(|error| anyhow::anyhow!("force-sync derived event blocks: {error}"))
    }

    fn force_sync_graph_projection_for_events(&self, events: &[Event]) -> Result<()> {
        if events.iter().any(|event| is_social_graph_event(event.kind)) {
            self._graph_env_lifecycle
                .force_sync()
                .context("force-sync derived social graph projection")?;
        }
        Ok(())
    }

    fn retained_derived_events_at_root(
        &self,
        bucket: &EventIndexBucket,
        root: &Cid,
        events: &[Event],
    ) -> Result<Vec<Event>> {
        let mut retained = Vec::new();
        for event in events
            .iter()
            .filter(|event| is_derived_projection_event(event.kind))
        {
            event
                .verify()
                .with_context(|| format!("verify derived event {} before projection", event.id))?;
            match bucket.load_event_by_id(root, &event.id.to_hex())? {
                Some(stored) if same_unsigned_event(&stored, event) => retained.push(stored),
                Some(_) => {
                    anyhow::bail!(
                        "derived event {} resolved to different unsigned fields in candidate root",
                        event.id
                    )
                }
                None => {}
            }
        }
        Ok(retained)
    }

    fn load_full_derived_events_at_root(
        &self,
        bucket: &EventIndexBucket,
        root: &Cid,
    ) -> Result<Vec<Event>> {
        block_on(bucket.event_store.validate_index_root(Some(root)))
            .map_err(map_event_store_error)
            .context("validate full derived-projection event root")?;
        let mut events = Vec::new();
        for kind in [Kind::ContactList, Kind::MuteList, Kind::Metadata] {
            let stored = block_on(bucket.event_store.list_by_kind(
                Some(root),
                kind.as_u16() as u32,
                ListEventsOptions::default(),
            ))
            .map_err(map_event_store_error)?;
            events.extend(
                stored
                    .into_iter()
                    .map(stored_event_to_nostr_event)
                    .collect::<Result<Vec<_>>>()?,
            );
        }
        Ok(events)
    }

    fn persist_pending_profile_projection_locked(
        &self,
        projection: &PendingProfileProjection,
    ) -> Result<()> {
        require_no_incomplete_profile_repair_for_root_write(
            &self.profile_index.root_pair_lock_path,
        )?;
        let path = self.pending_profile_projection_path();
        replace_file_durable(
            &path,
            &pending_profile_projection_bytes(projection)?,
            "pending profile projection",
        )?;
        #[cfg(test)]
        run_pending_profile_projection_persisted_probe(&path)?;
        Ok(())
    }

    fn clear_pending_profile_projection_locked(&self) -> Result<()> {
        remove_file_durable(&self.pending_profile_projection_path())
    }

    fn recover_profile_transactions_locked(&self) -> Result<()> {
        if incomplete_profile_repair_intent_path(&self.profile_index.root_pair_lock_path)?.is_some()
        {
            return Ok(());
        }
        self.profile_index
            .recover_pending_root_pair_commit_locked()?;
        self.recover_pending_profile_projection_locked()
    }

    fn recover_profile_transactions_locked_for_repair(
        &self,
        authority: &ProfileIndexRepairAuthority,
    ) -> Result<()> {
        let db_dir = self
            .profile_index
            .root_pair_lock_path
            .parent()
            .context("profile root-pair lock has no database parent")?;
        require_no_pending_profile_projection(db_dir)?;
        self.profile_index
            .recover_pending_root_pair_commit_locked_for_repair(authority)
    }

    fn recover_pending_profile_projection_locked(&self) -> Result<()> {
        if incomplete_profile_repair_intent_path(&self.profile_index.root_pair_lock_path)?.is_some()
        {
            return Ok(());
        }
        let path = self.pending_profile_projection_path();
        let Some(projection) = load_pending_profile_projection(&path)? else {
            return Ok(());
        };
        let storage_class = EventStorageClass::from(projection.storage_class);
        let bucket = self.bucket(storage_class);
        let current_root = bucket.events_root()?;
        let (old_root, new_root) = match &projection.projection {
            PendingProfileProjectionMode::Incremental {
                old_root, new_root, ..
            }
            | PendingProfileProjectionMode::RebuildPublicRoot { old_root, new_root } => (
                old_root.clone().map(cid_from_stored),
                cid_from_stored(new_root.clone()),
            ),
        };

        if current_root.as_ref() != Some(&new_root) {
            if current_root == old_root {
                return self.clear_pending_profile_projection_locked();
            }
            anyhow::bail!(
                "event root does not match the pre- or post-publication state required by pending profile projection {}",
                path.display()
            );
        }

        match projection.projection {
            PendingProfileProjectionMode::Incremental { events, .. } => {
                block_on(bucket.event_store.validate_index_root(Some(&new_root)))
                    .map_err(map_event_store_error)
                    .with_context(|| {
                        format!(
                            "validate published event root required by pending derived projection {}",
                            path.display()
                        )
                    })?;
                if events.is_empty() {
                    anyhow::bail!(
                        "pending incremental derived projection {} contains no events",
                        path.display()
                    );
                }
                let events = events
                    .into_iter()
                    .map(|json| {
                        Event::from_json(json).context("decode pending derived projection event")
                    })
                    .collect::<Result<Vec<_>>>()?;
                let mut canonical_events = Vec::with_capacity(events.len());
                for event in events {
                    if !is_derived_projection_event(event.kind) {
                        anyhow::bail!(
                            "pending derived projection {} contains unsupported event kind {}",
                            path.display(),
                            event.kind.as_u16()
                        );
                    }
                    event.verify().with_context(|| {
                        format!(
                            "verify pending derived event {} from {}",
                            event.id,
                            path.display()
                        )
                    })?;
                    let stored = bucket
                        .load_event_by_id(&new_root, &event.id.to_hex())?
                        .with_context(|| {
                            format!(
                                "pending derived event {} is absent from its published event root",
                                event.id
                            )
                        })?;
                    if !same_unsigned_event(&stored, &event) {
                        anyhow::bail!(
                            "pending derived event {} resolved to different unsigned fields in {}",
                            event.id,
                            path.display()
                        );
                    }
                    canonical_events.push(stored);
                }
                self.apply_graph_events_only_locked(&canonical_events)?;
                self.update_profile_index_for_events_locked(&canonical_events)?;
                self.force_sync_graph_projection_for_events(&canonical_events)?;
            }
            PendingProfileProjectionMode::RebuildPublicRoot { .. } => {
                if storage_class != EventStorageClass::Public {
                    anyhow::bail!(
                        "pending full profile rebuild {} must target the public event root",
                        path.display()
                    );
                }
                let events = self.load_full_derived_events_at_root(bucket, &new_root)?;
                self.apply_graph_events_only_locked(&events)?;
                self.rebuild_profile_index_for_events_locked(&events)?;
                self.force_sync_graph_projection_for_events(&events)?;
            }
        }
        self.clear_pending_profile_projection_locked()
    }

    pub(crate) fn apply_public_events_root_and_projections(
        &self,
        expected_old_root: Option<&Cid>,
        root: Option<&Cid>,
        events: &[Event],
        rebuild_profile_index: bool,
    ) -> Result<PublicEventsRootApplyOutcome> {
        let _transaction = self
            .profile_index
            .acquire_exclusive_root_pair_transaction()?;
        require_no_incomplete_profile_repair_for_root_write(
            &self.profile_index.root_pair_lock_path,
        )?;
        self.recover_profile_transactions_locked()?;
        let old_root = self.public_events.events_root_for_write()?;
        if old_root.as_ref() != expected_old_root {
            return Ok(PublicEventsRootApplyOutcome::Conflict {
                current_root: old_root,
            });
        }
        let projection_events = match root {
            Some(root) if rebuild_profile_index => {
                self.load_full_derived_events_at_root(&self.public_events, root)?
            }
            Some(root) => {
                self.retained_derived_events_at_root(&self.public_events, root, events)?
            }
            None => Vec::new(),
        };
        let profile_projection = if rebuild_profile_index || !projection_events.is_empty() {
            let new_root = root.context("derived projection requires a public event root")?;
            self.force_sync_event_storage(EventStorageClass::Public)?;
            Some(PendingProfileProjection {
                version: PROFILE_PROJECTION_PENDING_VERSION,
                storage_class: StoredEventStorageClass::Public,
                projection: if rebuild_profile_index {
                    PendingProfileProjectionMode::RebuildPublicRoot {
                        old_root: old_root.as_ref().map(stored_cid),
                        new_root: stored_cid(new_root),
                    }
                } else {
                    PendingProfileProjectionMode::Incremental {
                        old_root: old_root.as_ref().map(stored_cid),
                        new_root: stored_cid(new_root),
                        events: projection_events.iter().map(JsonUtil::as_json).collect(),
                    }
                },
            })
        } else {
            None
        };
        if let Some(projection) = profile_projection.as_ref() {
            self.persist_pending_profile_projection_locked(projection)?;
            self.public_events.write_events_root_durable(root)?;
        } else {
            self.public_events.write_events_root(root)?;
        }
        self.apply_graph_events_only_locked(&projection_events)?;
        if profile_projection.is_some() {
            if rebuild_profile_index {
                self.rebuild_profile_index_for_events_locked(&projection_events)?;
            } else {
                self.update_profile_index_for_events_locked(&projection_events)?;
            }
            self.force_sync_graph_projection_for_events(&projection_events)?;
            self.clear_pending_profile_projection_locked()?;
        }
        Ok(PublicEventsRootApplyOutcome::Applied)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn latest_profile_event(&self, pubkey_hex: &str) -> Result<Option<Event>> {
        let (root, _) = self.recovered_profile_index_roots()?;
        self.profile_index
            .profile_event_for_pubkey_at_root(root.as_ref(), pubkey_hex)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn profile_search_entries_for_prefix(
        &self,
        prefix: &str,
    ) -> Result<Vec<(String, StoredProfileSearchEntry)>> {
        let (_, root) = self.recovered_profile_index_roots()?;
        let Some(root) = root else {
            return Ok(Vec::new());
        };
        self.profile_index
            .search_entries_for_prefix_at_root(&root, prefix)
    }

    /// Validate profile-by-pubkey and profile-search semantics against one
    /// real metadata event.
    ///
    /// This reads both persisted roots through the shared storage router,
    /// proves that the pubkey points at the expected event blob, derives a
    /// normal search term from that event, and proves the exact search entry
    /// points at the same mirrored blob.
    pub fn validate_profile_indexes_for_event(
        &self,
        event: &Event,
    ) -> Result<StoredProfileSearchEntry> {
        if event.kind != Kind::Metadata {
            anyhow::bail!("profile index validation requires a kind-0 metadata event");
        }

        let pubkey = event.pubkey.to_hex();
        let (by_pubkey_root, search_root) = self.recovered_profile_index_roots()?;
        let by_pubkey_root = by_pubkey_root.context("profile-by-pubkey root is missing")?;
        let search_root = search_root.context("profile-search root is missing")?;
        let mirrored_cid = block_on(
            self.profile_index
                .index
                .get_link(Some(&by_pubkey_root), &pubkey),
        )
        .context("query profile-by-pubkey root")?
        .with_context(|| format!("profile-by-pubkey omitted {pubkey}"))?;
        let mirrored = self
            .profile_index
            .load_profile_event(&mirrored_cid)?
            .with_context(|| format!("mirrored profile blob for {pubkey} is missing"))?;
        if mirrored.id != event.id {
            anyhow::bail!(
                "profile-by-pubkey returned event {} instead of {} for {pubkey}",
                mirrored.id,
                event.id
            );
        }

        let term = profile_search_terms_for_event(event)
            .into_iter()
            .next()
            .with_context(|| format!("profile {pubkey} did not produce a search term"))?;
        let exact_key = format!("{PROFILE_SEARCH_PREFIX}{term}:{pubkey}");
        let encoded = block_on(self.profile_index.index.get(Some(&search_root), &exact_key))
            .context("query profile-search root")?
            .with_context(|| format!("profile-search omitted exact key {exact_key}"))?;
        let entry: StoredProfileSearchEntry =
            serde_json::from_str(&encoded).context("decode stored profile search entry JSON")?;
        let expected_nhash = nhash_encode_full(&NHashData {
            hash: mirrored_cid.hash,
            decrypt_key: mirrored_cid.key,
        })
        .context("encode mirrored profile event nhash")?;
        if entry.pubkey != pubkey
            || entry.created_at != event.created_at.as_secs()
            || entry.event_nhash != expected_nhash
        {
            anyhow::bail!(
                "profile-search entry for {exact_key} does not match its profile-by-pubkey event"
            );
        }
        Ok(entry)
    }

    pub fn sync_profile_index_for_events(&self, events: &[Event]) -> Result<()> {
        self.update_profile_index_for_events(events)
    }

    /// Apply profile updates using an immutable, independently derived rank
    /// decision for every profile author. `Some(distance)` retains the profile
    /// with that exact search rank; `None` removes an excluded profile.
    pub fn sync_profile_index_for_events_with_frozen_distances(
        &self,
        events: &[Event],
        decisions: &BTreeMap<String, Option<u32>>,
    ) -> Result<()> {
        self.update_profile_index_for_events_with(events, true, |event| {
            let pubkey = event.pubkey.to_hex();
            match decisions.get(&pubkey) {
                Some(Some(distance)) => Ok((Some(*distance), false)),
                Some(None) => Ok((None, true)),
                None => {
                    anyhow::bail!("frozen profile rank decisions omitted metadata author {pubkey}")
                }
            }
        })
    }

    /// Build a complete replacement profile-index pair without publishing
    /// either root file. Every input must be the retained kind-0 winner for a
    /// distinct author and must have an independently pinned eligible rank.
    ///
    /// The returned blocks are force-synced before this function returns.
    /// Callers can therefore exhaustively validate both unpublished roots and
    /// durably record their own provenance intent before committing the pair.
    pub fn build_unpublished_profile_index_repair_with_frozen_distances(
        &self,
        events: &[Event],
        decisions: &BTreeMap<String, Option<u32>>,
    ) -> Result<PreparedProfileIndexRepair> {
        if events.is_empty() {
            anyhow::bail!("profile-index repair requires retained kind-0 winners");
        }
        let _transaction = self
            .profile_index
            .acquire_exclusive_root_pair_transaction()?;
        self.recover_profile_transactions_locked()?;
        let old_roots = self.profile_index_roots_locked()?;
        let latest_by_pubkey = latest_metadata_events_by_pubkey(events);
        if latest_by_pubkey.len() != events.len() {
            anyhow::bail!(
                "profile-index repair inputs must contain exactly one kind-0 winner per pubkey"
            );
        }
        for (pubkey, event) in &latest_by_pubkey {
            if event.pubkey.to_hex() != *pubkey || event.kind != Kind::Metadata {
                anyhow::bail!("profile-index repair input for {pubkey} is not canonical metadata");
            }
            event.verify().with_context(|| {
                format!("verify retained profile-index repair event for {pubkey}")
            })?;
            match decisions.get(pubkey) {
                Some(Some(_)) => {}
                Some(None) => {
                    anyhow::bail!("profile-index repair retained excluded metadata author {pubkey}")
                }
                None => anyhow::bail!(
                    "profile-index repair rank decisions omitted metadata author {pubkey}"
                ),
            }
        }
        let (by_pubkey, search) = self
            .profile_index
            .rebuild_profile_events_with_distances_locked(
                latest_by_pubkey.into_values(),
                |event| {
                    decisions
                        .get(&event.pubkey.to_hex())
                        .copied()
                        .flatten()
                        .map(Some)
                        .context("eligible profile-index repair rank disappeared")
                },
            )?;
        let by_pubkey = by_pubkey.context("profile-index repair built an empty by-pubkey root")?;
        let search = search.context("profile-index repair built an empty search root")?;
        self.profile_index
            .store
            .force_sync()
            .map_err(|error| anyhow::anyhow!("force-sync unpublished profile repair: {error}"))?;
        let new_roots = ProfileIndexRoots {
            by_pubkey_file_sha256: Some(profile_index_root_file_sha256(&by_pubkey)?),
            search_file_sha256: Some(profile_index_root_file_sha256(&search)?),
            by_pubkey: Some(by_pubkey),
            search: Some(search),
        };
        Ok(PreparedProfileIndexRepair {
            old_roots,
            new_roots,
        })
    }

    #[cfg(test)]
    pub(crate) fn crash_after_prepared_profile_root_pair_intent(
        &self,
        prepared: &PreparedProfileIndexRepair,
    ) -> Result<()> {
        let current = self.recovered_profile_index_roots()?;
        let current = ProfileIndexRoots {
            by_pubkey_file_sha256: current
                .0
                .as_ref()
                .map(profile_index_root_file_sha256)
                .transpose()?,
            search_file_sha256: current
                .1
                .as_ref()
                .map(profile_index_root_file_sha256)
                .transpose()?,
            by_pubkey: current.0,
            search: current.1,
        };
        if current != prepared.old_roots {
            anyhow::bail!("generated crash requires the exact prepared old root pair");
        }
        self.profile_index.write_roots_interrupted_after_intent(
            prepared.new_roots.by_pubkey.as_ref(),
            prepared.new_roots.search.as_ref(),
        )
    }

    /// Atomically publish an exhaustively validated repair pair iff the
    /// currently published pair is still the exact pair observed during
    /// preparation. Interrupted low-level commits roll forward on open; an
    /// already-installed exact replacement is therefore an idempotent success.
    pub fn commit_prepared_profile_index_repair(
        &self,
        prepared: &PreparedProfileIndexRepair,
        authority: ProfileIndexRepairAuthority,
    ) -> Result<ProfileIndexRepairCommitOutcome> {
        Ok(self
            .commit_prepared_profile_index_repair_held(prepared, authority)?
            .outcome())
    }

    /// Mint an opaque commit capability only when the durable high-level
    /// intent exists, is bound to this store, and names this exact root pair.
    pub fn authorize_prepared_profile_index_repair(
        &self,
        prepared: &PreparedProfileIndexRepair,
        validated_intent_bytes: &[u8],
    ) -> Result<ProfileIndexRepairAuthority> {
        load_profile_index_repair_authority(
            &self.profile_index.root_pair_lock_path,
            prepared,
            ProfileIndexRepairAuthorityPhase::Commit,
            Some(validated_intent_bytes),
            None,
        )
    }

    /// Mint an opaque completion-recovery capability only when an exact
    /// intent-bound receipt exists but its completion witness does not.
    pub fn authorize_completed_profile_index_repair(
        &self,
        prepared: &PreparedProfileIndexRepair,
        validated_intent_bytes: &[u8],
        validated_receipt_bytes: &[u8],
    ) -> Result<ProfileIndexRepairAuthority> {
        load_profile_index_repair_authority(
            &self.profile_index.root_pair_lock_path,
            prepared,
            ProfileIndexRepairAuthorityPhase::Completion,
            Some(validated_intent_bytes),
            Some(validated_receipt_bytes),
        )
    }

    /// Publish an exact prepared pair and retain the exclusive root-pair lock
    /// until the returned guard is dropped. Recovery callers use this form to
    /// audit the installed roots and persist their receipt without a
    /// post-commit writer race.
    pub fn commit_prepared_profile_index_repair_held(
        &self,
        prepared: &PreparedProfileIndexRepair,
        authority: ProfileIndexRepairAuthority,
    ) -> Result<ProfileIndexRepairPublicationGuard> {
        self.commit_prepared_profile_index_repair_held_with(prepared, || Ok(authority))
    }

    /// As [`Self::commit_prepared_profile_index_repair_held`], but run one
    /// durable high-level intent callback while the exclusive pair lock is
    /// held. The callback must return the opaque capability minted from that
    /// exact durable intent before privileged recovery or publication begins.
    pub fn commit_prepared_profile_index_repair_held_with<F>(
        &self,
        prepared: &PreparedProfileIndexRepair,
        before_commit: F,
    ) -> Result<ProfileIndexRepairPublicationGuard>
    where
        F: FnOnce() -> Result<ProfileIndexRepairAuthority>,
    {
        let transaction = self
            .profile_index
            .acquire_exclusive_root_pair_transaction()?;
        let intent_preexisting =
            incomplete_profile_repair_intent_path(&self.profile_index.root_pair_lock_path)?
                .is_some();
        if !intent_preexisting {
            self.recover_profile_transactions_locked()?;
            let current = self.profile_index_roots_locked()?;
            if current != prepared.old_roots {
                anyhow::bail!(
                    "published profile roots changed after repair preparation; refusing non-CAS commit"
                );
            }
        }
        let authority = before_commit()?;
        revalidate_profile_index_repair_authority(
            &authority,
            &self.profile_index.root_pair_lock_path,
            prepared,
            ProfileIndexRepairAuthorityPhase::Commit,
        )?;
        self.recover_profile_transactions_locked_for_repair(&authority)?;
        let current = self.profile_index_roots_locked()?;
        if current != prepared.old_roots && current != prepared.new_roots {
            anyhow::bail!(
                "published profile roots changed after repair preparation; refusing non-CAS commit"
            );
        }
        let outcome = if current == prepared.new_roots {
            ProfileIndexRepairCommitOutcome::AlreadyApplied
        } else {
            self.profile_index
                .write_roots_with_hooks_locked_for_repair(
                    &authority,
                    prepared.new_roots.by_pubkey.as_ref(),
                    prepared.new_roots.search.as_ref(),
                    || Ok(()),
                    || Ok(()),
                )?;
            ProfileIndexRepairCommitOutcome::Applied
        };
        let installed = self.profile_index_roots_locked()?;
        if installed != prepared.new_roots {
            anyhow::bail!("profile-index repair commit did not install the exact prepared pair");
        }
        Ok(ProfileIndexRepairPublicationGuard {
            by_pubkey_root_path: self.profile_index.by_pubkey_root_path.clone(),
            search_root_path: self.profile_index.search_root_path.clone(),
            installed_roots: installed,
            outcome,
            _transaction: transaction,
        })
    }

    /// Hold the profile-root transaction after proving a previously completed
    /// repair still has its exact installed pair. Crash recovery uses this
    /// boundary to publish a missing completion witness without racing an
    /// ordinary writer after the final verification.
    pub fn hold_completed_profile_index_repair(
        &self,
        prepared: &PreparedProfileIndexRepair,
        authority: ProfileIndexRepairAuthority,
    ) -> Result<ProfileIndexRepairPublicationGuard> {
        let transaction = self
            .profile_index
            .acquire_exclusive_root_pair_transaction()?;
        revalidate_profile_index_repair_authority(
            &authority,
            &self.profile_index.root_pair_lock_path,
            prepared,
            ProfileIndexRepairAuthorityPhase::Completion,
        )?;
        let db_dir = self
            .profile_index
            .root_pair_lock_path
            .parent()
            .context("profile root-pair lock has no database parent")?;
        require_no_pending_profile_root_pair_commit(db_dir)?;
        require_no_pending_profile_projection(db_dir)?;
        let installed = self.profile_index_roots_locked()?;
        if installed != prepared.new_roots {
            anyhow::bail!(
                "completed profile repair roots differ from the exact installed repair pair"
            );
        }
        Ok(ProfileIndexRepairPublicationGuard {
            by_pubkey_root_path: self.profile_index.by_pubkey_root_path.clone(),
            search_root_path: self.profile_index.search_root_path.clone(),
            installed_roots: installed,
            outcome: ProfileIndexRepairCommitOutcome::AlreadyApplied,
            _transaction: transaction,
        })
    }

    fn profile_index_roots_locked(&self) -> Result<ProfileIndexRoots> {
        let (by_pubkey, by_pubkey_file_sha256) =
            read_root_file_snapshot(&self.profile_index.by_pubkey_root_path)?;
        let (search, search_file_sha256) =
            read_root_file_snapshot(&self.profile_index.search_root_path)?;
        Ok(ProfileIndexRoots {
            by_pubkey,
            search,
            by_pubkey_file_sha256,
            search_file_sha256,
        })
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn rebuild_profile_index_for_events(&self, events: &[Event]) -> Result<()> {
        let _transaction = self
            .profile_index
            .acquire_exclusive_root_pair_transaction()?;
        self.recover_profile_transactions_locked()?;
        self.rebuild_profile_index_for_events_locked(events)
    }

    fn rebuild_profile_index_for_events_locked(&self, events: &[Event]) -> Result<()> {
        let latest_by_pubkey = self.filtered_latest_metadata_events_by_pubkey(events)?;
        self.profile_index
            .rebuild_profile_events_and_commit_with_distances_locked(
                latest_by_pubkey.into_values(),
                |event| self.follow_distance(&event.pubkey.to_bytes()),
            )
    }

    async fn rebuild_profile_index_for_events_async_locked(&self, events: &[Event]) -> Result<()> {
        let latest_by_pubkey = self.filtered_latest_metadata_events_by_pubkey(events)?;
        self.profile_index
            .rebuild_profile_events_async_and_commit_with_distances_locked(
                latest_by_pubkey.into_values(),
                |event| self.follow_distance(&event.pubkey.to_bytes()),
            )
            .await
    }

    pub fn rebuild_profile_index_from_stored_events(&self) -> Result<usize> {
        let _transaction = self
            .profile_index
            .acquire_exclusive_root_pair_transaction()?;
        self.rebuild_profile_index_from_stored_events_locked()
    }

    fn rebuild_profile_index_from_stored_events_locked(&self) -> Result<usize> {
        self.recover_profile_transactions_locked()?;
        let public_events_root = self.public_events.events_root()?;
        let ambient_events_root = self.ambient_events.events_root()?;
        if public_events_root.is_none() && ambient_events_root.is_none() {
            self.profile_index
                .write_roots_with_hooks_locked(None, None, || Ok(()), || Ok(()))?;
            return Ok(0);
        }

        let mut events = Vec::new();
        for (bucket, root) in [
            (&self.public_events, public_events_root),
            (&self.ambient_events, ambient_events_root),
        ] {
            let Some(root) = root else {
                continue;
            };
            let stored = block_on(bucket.event_store.list_by_kind_lossy(
                Some(&root),
                Kind::Metadata.as_u16() as u32,
                ListEventsOptions::default(),
            ))
            .map_err(map_event_store_error)?;
            events.extend(
                stored
                    .into_iter()
                    .map(stored_event_to_nostr_event)
                    .collect::<Result<Vec<_>>>()?,
            );
        }

        let latest_count = self
            .filtered_latest_metadata_events_by_pubkey(&events)?
            .len();
        self.rebuild_profile_index_for_events_locked(&events)?;
        Ok(latest_count)
    }

    pub async fn rebuild_profile_index_from_stored_events_async(&self) -> Result<usize> {
        let _transaction = self
            .profile_index
            .acquire_exclusive_root_pair_transaction_async()
            .await?;
        self.rebuild_profile_index_from_stored_events_async_locked()
            .await
    }

    async fn rebuild_profile_index_from_stored_events_async_locked(&self) -> Result<usize> {
        self.recover_profile_transactions_locked()?;
        let public_events_root = self.public_events.events_root()?;
        let ambient_events_root = self.ambient_events.events_root()?;
        if public_events_root.is_none() && ambient_events_root.is_none() {
            self.profile_index
                .write_roots_with_hooks_locked(None, None, || Ok(()), || Ok(()))?;
            return Ok(0);
        }

        let mut events = Vec::new();
        for (bucket, root) in [
            (&self.public_events, public_events_root),
            (&self.ambient_events, ambient_events_root),
        ] {
            let Some(root) = root else {
                continue;
            };
            let stored = bucket
                .event_store
                .list_by_kind_lossy(
                    Some(&root),
                    Kind::Metadata.as_u16() as u32,
                    ListEventsOptions::default(),
                )
                .await
                .map_err(map_event_store_error)?;
            events.extend(
                stored
                    .into_iter()
                    .map(stored_event_to_nostr_event)
                    .collect::<Result<Vec<_>>>()?,
            );
        }

        let latest_count = self
            .filtered_latest_metadata_events_by_pubkey(&events)?
            .len();
        self.rebuild_profile_index_for_events_async_locked(&events)
            .await?;
        Ok(latest_count)
    }

    pub fn rebuild_event_indexes_from_stored_events(&self) -> Result<(usize, usize)> {
        let _transaction = self
            .profile_index
            .acquire_exclusive_root_pair_transaction()?;
        require_no_incomplete_profile_repair_for_root_write(
            &self.profile_index.root_pair_lock_path,
        )?;
        self.recover_profile_transactions_locked()?;
        let public_count =
            self.rebuild_event_index_bucket_from_stored_events(&self.public_events)?;
        let ambient_count =
            self.rebuild_event_index_bucket_from_stored_events(&self.ambient_events)?;
        self.rebuild_profile_index_from_stored_events_locked()?;
        Ok((public_count, ambient_count))
    }

    pub async fn rebuild_event_indexes_from_stored_events_async(&self) -> Result<(usize, usize)> {
        let _transaction = self
            .profile_index
            .acquire_exclusive_root_pair_transaction_async()
            .await?;
        require_no_incomplete_profile_repair_for_root_write(
            &self.profile_index.root_pair_lock_path,
        )?;
        self.recover_profile_transactions_locked()?;
        let public_count = self
            .rebuild_event_index_bucket_from_stored_events_async(&self.public_events)
            .await?;
        let ambient_count = self
            .rebuild_event_index_bucket_from_stored_events_async(&self.ambient_events)
            .await?;
        self.rebuild_profile_index_from_stored_events_async_locked()
            .await?;
        Ok((public_count, ambient_count))
    }

    fn rebuild_event_index_bucket_from_stored_events(
        &self,
        bucket: &EventIndexBucket,
    ) -> Result<usize> {
        let Some(root) = bucket.events_root()? else {
            bucket.write_events_root(None)?;
            return Ok(0);
        };

        let manifest = match block_on(bucket.event_store.get_manifest(Some(&root))) {
            Ok(manifest) => manifest,
            Err(err) => {
                tracing::warn!(
                    "Clearing invalid social graph event index root {} before rebuild: {}",
                    hex::encode(root.hash),
                    err
                );
                bucket.write_events_root(None)?;
                return Ok(0);
            }
        };
        if manifest.by_kind_time_author.is_none() {
            let next_root = block_on(bucket.event_store.upgrade_manifest_indexes(Some(&root)))
                .map_err(map_event_store_error)?;
            if next_root.as_ref() != Some(&root) {
                bucket.write_events_root(next_root.as_ref())?;
                return Ok(0);
            }
        }

        let stored = block_on(
            bucket
                .event_store
                .list_recent_lossy(Some(&root), ListEventsOptions::default()),
        )
        .map_err(map_event_store_error)?;
        let count = stored.len();
        let next_root =
            block_on(bucket.event_store.build(None, stored)).map_err(map_event_store_error)?;
        bucket.write_events_root(next_root.as_ref())?;
        Ok(count)
    }

    async fn rebuild_event_index_bucket_from_stored_events_async(
        &self,
        bucket: &EventIndexBucket,
    ) -> Result<usize> {
        let Some(root) = bucket.events_root()? else {
            bucket.write_events_root(None)?;
            return Ok(0);
        };

        let manifest = match bucket.event_store.get_manifest(Some(&root)).await {
            Ok(manifest) => manifest,
            Err(err) => {
                tracing::warn!(
                    "Clearing invalid social graph event index root {} before rebuild: {}",
                    hex::encode(root.hash),
                    err
                );
                bucket.write_events_root(None)?;
                return Ok(0);
            }
        };
        if manifest.by_kind_time_author.is_none() {
            let next_root = bucket
                .event_store
                .upgrade_manifest_indexes(Some(&root))
                .await
                .map_err(map_event_store_error)?;
            if next_root.as_ref() != Some(&root) {
                bucket.write_events_root(next_root.as_ref())?;
                return Ok(0);
            }
        }

        let stored = bucket
            .event_store
            .list_recent_lossy(Some(&root), ListEventsOptions::default())
            .await
            .map_err(map_event_store_error)?;
        let count = stored.len();
        let next_root = bucket
            .event_store
            .build(None, stored)
            .await
            .map_err(map_event_store_error)?;
        bucket.write_events_root(next_root.as_ref())?;
        Ok(count)
    }

    fn update_profile_index_for_events(&self, events: &[Event]) -> Result<()> {
        if !events.iter().any(|event| event.kind == Kind::Metadata) {
            return Ok(());
        }
        let _transaction = self
            .profile_index
            .acquire_exclusive_root_pair_transaction()?;
        self.recover_profile_transactions_locked()?;
        self.update_profile_index_for_events_locked(events)
    }

    fn update_profile_index_for_events_locked(&self, events: &[Event]) -> Result<()> {
        let threshold = self.profile_index_overmute_threshold();
        self.update_profile_index_for_events_with_locked(events, false, |event| {
            let overmuted = self.is_overmuted_user(&event.pubkey.to_bytes(), threshold)?;
            let follow_distance = if overmuted {
                None
            } else {
                self.follow_distance(&event.pubkey.to_bytes())?
            };
            Ok((follow_distance, overmuted))
        })
    }

    fn update_profile_index_for_events_with<F>(
        &self,
        events: &[Event],
        force_existing_search_value: bool,
        classify: F,
    ) -> Result<()>
    where
        F: FnMut(&Event) -> Result<(Option<u32>, bool)>,
    {
        if !events.iter().any(|event| event.kind == Kind::Metadata) {
            return Ok(());
        }
        let _transaction = self
            .profile_index
            .acquire_exclusive_root_pair_transaction()?;
        self.recover_profile_transactions_locked()?;
        self.update_profile_index_for_events_with_locked(
            events,
            force_existing_search_value,
            classify,
        )
    }

    fn update_profile_index_for_events_with_locked<F>(
        &self,
        events: &[Event],
        force_existing_search_value: bool,
        mut classify: F,
    ) -> Result<()>
    where
        F: FnMut(&Event) -> Result<(Option<u32>, bool)>,
    {
        let latest_by_pubkey = latest_metadata_events_by_pubkey(events);
        if latest_by_pubkey.is_empty() {
            return Ok(());
        }

        let mut updates = Vec::with_capacity(latest_by_pubkey.len());
        for event in latest_by_pubkey.into_values() {
            let (follow_distance, remove) = classify(event)?;
            updates.push((event, follow_distance, remove, force_existing_search_value));
        }

        self.profile_index
            .update_profile_events_and_commit_locked(&updates)?;
        Ok(())
    }

    fn filtered_latest_metadata_events_by_pubkey<'a>(
        &self,
        events: &'a [Event],
    ) -> Result<BTreeMap<String, &'a Event>> {
        let threshold = self.profile_index_overmute_threshold();
        let mut latest_by_pubkey = BTreeMap::<String, &Event>::new();
        for event in events.iter().filter(|event| event.kind == Kind::Metadata) {
            if self.is_overmuted_user(&event.pubkey.to_bytes(), threshold)? {
                continue;
            }
            let pubkey = event.pubkey.to_hex();
            match latest_by_pubkey.get(&pubkey) {
                Some(current) if compare_nostr_events(event, current).is_le() => {}
                _ => {
                    latest_by_pubkey.insert(pubkey, event);
                }
            }
        }
        Ok(latest_by_pubkey)
    }

    fn snapshot_chunks(&self, root: &[u8; 32], options: &BinaryBudget) -> Result<Vec<Bytes>> {
        let state = {
            let graph = self.graph.lock().unwrap();
            graph.export_state().context("export social graph state")?
        };
        let mut graph = SocialGraph::from_state(state).context("rebuild social graph state")?;
        let root_hex = hex::encode(root);
        if graph.get_root() != root_hex {
            graph
                .set_root(&root_hex)
                .context("set snapshot social graph root")?;
        }
        let chunks = graph
            .to_binary_chunks_with_budget(*options)
            .context("encode social graph snapshot")?;
        Ok(chunks.into_iter().map(Bytes::from).collect())
    }

    fn ingest_event(&self, event: &Event) -> Result<()> {
        self.ingest_event_with_storage_class(event, self.default_storage_class_for(event)?)
    }

    fn ingest_events(&self, events: &[Event]) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }

        let mut public = Vec::new();
        let mut ambient = Vec::new();
        for event in events {
            match self.default_storage_class_for(event)? {
                EventStorageClass::Public => public.push(event.clone()),
                EventStorageClass::Ambient => ambient.push(event.clone()),
            }
        }

        if !public.is_empty() {
            self.ingest_events_with_storage_class(&public, EventStorageClass::Public)?;
        }
        if !ambient.is_empty() {
            self.ingest_events_with_storage_class(&ambient, EventStorageClass::Ambient)?;
        }

        Ok(())
    }

    fn apply_graph_events_only(&self, events: &[Event]) -> Result<()> {
        if !events.iter().any(|event| is_social_graph_event(event.kind)) {
            return Ok(());
        }
        let _transaction = self
            .profile_index
            .acquire_exclusive_root_pair_transaction()?;
        require_no_incomplete_profile_repair_for_root_write(
            &self.profile_index.root_pair_lock_path,
        )?;
        self.recover_profile_transactions_locked()?;
        self.apply_graph_events_only_locked(events)
    }

    fn apply_graph_events_only_locked(&self, events: &[Event]) -> Result<()> {
        let graph_events = events
            .iter()
            .filter(|event| is_social_graph_event(event.kind))
            .collect::<Vec<_>>();
        if graph_events.is_empty() {
            return Ok(());
        }

        {
            let mut graph = self.graph.lock().unwrap();
            let mut snapshot = SocialGraph::from_state(
                graph
                    .export_state()
                    .context("export social graph state for graph-only ingest")?,
            )
            .context("rebuild social graph state for graph-only ingest")?;
            for event in graph_events {
                snapshot.handle_event(&graph_event_from_nostr(event), true, 0.0);
            }
            graph
                .replace_state(&snapshot.export_state())
                .context("replace graph-only social graph state")?;
        }
        self.invalidate_distance_cache();
        Ok(())
    }

    fn query_events(&self, filter: &Filter, limit: usize) -> Result<Vec<Event>> {
        self.query_events_in_scope(filter, limit, EventQueryScope::All)
    }

    fn default_storage_class_for(&self, event: &Event) -> Result<EventStorageClass> {
        let graph = self.graph.lock().unwrap();
        let root_hex = graph.get_root().context("read social graph root")?;
        if root_hex != DEFAULT_ROOT_HEX && root_hex == event.pubkey.to_hex() {
            return Ok(EventStorageClass::Public);
        }
        Ok(EventStorageClass::Ambient)
    }

    fn bucket(&self, storage_class: EventStorageClass) -> &EventIndexBucket {
        match storage_class {
            EventStorageClass::Public => &self.public_events,
            EventStorageClass::Ambient => &self.ambient_events,
        }
    }

    fn ingest_event_with_storage_class(
        &self,
        event: &Event,
        storage_class: EventStorageClass,
    ) -> Result<()> {
        self.ingest_event_with_storage_class_and_lock_timeout(
            event,
            storage_class,
            PROFILE_ROOT_PAIR_LOCK_TIMEOUT,
        )
    }

    fn ingest_event_with_storage_class_and_lock_timeout(
        &self,
        event: &Event,
        storage_class: EventStorageClass,
        lock_timeout: Duration,
    ) -> Result<()> {
        let _transaction = self
            .profile_index
            .acquire_exclusive_root_pair_transaction_with_timeout(lock_timeout)?;
        require_no_incomplete_profile_repair_for_root_write(
            &self.profile_index.root_pair_lock_path,
        )?;
        self.recover_profile_transactions_locked()?;
        let bucket = self.bucket(storage_class);
        let current_root = bucket.events_root_for_write()?;
        let next_root = bucket.store_event(current_root.as_ref(), event)?;
        let projection_events =
            self.retained_derived_events_at_root(bucket, &next_root, std::slice::from_ref(event))?;
        let derived_projection =
            (!projection_events.is_empty()).then(|| PendingProfileProjection {
                version: PROFILE_PROJECTION_PENDING_VERSION,
                storage_class: storage_class.into(),
                projection: PendingProfileProjectionMode::Incremental {
                    old_root: current_root.as_ref().map(stored_cid),
                    new_root: stored_cid(&next_root),
                    events: projection_events.iter().map(JsonUtil::as_json).collect(),
                },
            });
        if let Some(projection) = derived_projection.as_ref() {
            self.force_sync_event_storage(storage_class)?;
            self.persist_pending_profile_projection_locked(projection)?;
            bucket.write_events_root_durable(Some(&next_root))?;
        } else {
            bucket.write_events_root(Some(&next_root))?;
        }

        if derived_projection.is_some() {
            self.apply_graph_events_only_locked(&projection_events)?;
            self.update_profile_index_for_events_locked(&projection_events)?;
            self.force_sync_graph_projection_for_events(&projection_events)?;
            self.clear_pending_profile_projection_locked()?;
        }

        Ok(())
    }

    fn ingest_events_with_storage_class(
        &self,
        events: &[Event],
        storage_class: EventStorageClass,
    ) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }

        let _transaction = self
            .profile_index
            .acquire_exclusive_root_pair_transaction()?;
        require_no_incomplete_profile_repair_for_root_write(
            &self.profile_index.root_pair_lock_path,
        )?;
        self.recover_profile_transactions_locked()?;
        let bucket = self.bucket(storage_class);
        let current_root = bucket.events_root_for_write()?;
        let stored_events = events
            .iter()
            .map(stored_event_from_nostr_sdk_event)
            .collect::<Vec<_>>();
        let next_root = block_on(
            bucket
                .event_store
                .build(current_root.as_ref(), stored_events),
        )
        .map_err(map_event_store_error)?;
        let projection_events = match next_root.as_ref() {
            Some(root) => self.retained_derived_events_at_root(bucket, root, events)?,
            None => Vec::new(),
        };
        let derived_projection = if projection_events.is_empty() {
            None
        } else {
            let next_root = next_root
                .as_ref()
                .context("derived event batch did not produce an event root")?;
            Some(PendingProfileProjection {
                version: PROFILE_PROJECTION_PENDING_VERSION,
                storage_class: storage_class.into(),
                projection: PendingProfileProjectionMode::Incremental {
                    old_root: current_root.as_ref().map(stored_cid),
                    new_root: stored_cid(next_root),
                    events: projection_events.iter().map(JsonUtil::as_json).collect(),
                },
            })
        };
        if let Some(projection) = derived_projection.as_ref() {
            self.force_sync_event_storage(storage_class)?;
            self.persist_pending_profile_projection_locked(projection)?;
            bucket.write_events_root_durable(next_root.as_ref())?;
        } else {
            bucket.write_events_root(next_root.as_ref())?;
        }

        if derived_projection.is_some() {
            self.apply_graph_events_only_locked(&projection_events)?;
            self.update_profile_index_for_events_locked(&projection_events)?;
            self.force_sync_graph_projection_for_events(&projection_events)?;
            self.clear_pending_profile_projection_locked()?;
        }

        Ok(())
    }

    pub(crate) fn query_events_in_scope(
        &self,
        filter: &Filter,
        limit: usize,
        scope: EventQueryScope,
    ) -> Result<Vec<Event>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let buckets: &[&EventIndexBucket] = match scope {
            EventQueryScope::PublicOnly => &[&self.public_events],
            EventQueryScope::AmbientOnly => &[&self.ambient_events],
            EventQueryScope::All => &[&self.public_events, &self.ambient_events],
        };

        let mut candidates = Vec::new();
        for bucket in buckets {
            candidates.extend(bucket.query_events(filter, limit)?);
        }

        let mut deduped = dedupe_events(candidates);
        deduped.retain(|event| filter.match_event(event, Default::default()));
        deduped.truncate(limit);
        Ok(deduped)
    }
}

impl SocialGraphBackend for SocialGraphStore {
    fn stats(&self) -> Result<SocialGraphStats> {
        SocialGraphStore::stats(self)
    }

    fn users_by_follow_distance(&self, distance: u32) -> Result<Vec<[u8; 32]>> {
        SocialGraphStore::users_by_follow_distance(self, distance)
    }

    fn follow_distance(&self, pk_bytes: &[u8; 32]) -> Result<Option<u32>> {
        SocialGraphStore::follow_distance(self, pk_bytes)
    }

    fn follow_list_created_at(&self, owner: &[u8; 32]) -> Result<Option<u64>> {
        SocialGraphStore::follow_list_created_at(self, owner)
    }

    fn followed_targets(&self, owner: &[u8; 32]) -> Result<UserSet> {
        SocialGraphStore::followed_targets(self, owner)
    }

    fn is_overmuted_user(&self, user_pk: &[u8; 32], threshold: f64) -> Result<bool> {
        SocialGraphStore::is_overmuted_user(self, user_pk, threshold)
    }

    fn profile_search_root(&self) -> Result<Option<Cid>> {
        SocialGraphStore::profile_search_root(self)
    }

    fn snapshot_chunks(&self, root: &[u8; 32], options: &BinaryBudget) -> Result<Vec<Bytes>> {
        SocialGraphStore::snapshot_chunks(self, root, options)
    }

    fn ingest_event(&self, event: &Event) -> Result<()> {
        SocialGraphStore::ingest_event(self, event)
    }

    fn ingest_event_with_storage_class(
        &self,
        event: &Event,
        storage_class: EventStorageClass,
    ) -> Result<()> {
        SocialGraphStore::ingest_event_with_storage_class(self, event, storage_class)
    }

    fn ingest_events(&self, events: &[Event]) -> Result<()> {
        SocialGraphStore::ingest_events(self, events)
    }

    fn ingest_events_with_storage_class(
        &self,
        events: &[Event],
        storage_class: EventStorageClass,
    ) -> Result<()> {
        SocialGraphStore::ingest_events_with_storage_class(self, events, storage_class)
    }

    fn ingest_graph_events(&self, events: &[Event]) -> Result<()> {
        SocialGraphStore::apply_graph_events_only(self, events)
    }

    fn query_events(&self, filter: &Filter, limit: usize) -> Result<Vec<Event>> {
        SocialGraphStore::query_events(self, filter, limit)
    }
}

impl NostrSocialGraphBackend for SocialGraphStore {
    type Error = UpstreamGraphBackendError;

    fn get_root(&self) -> std::result::Result<String, Self::Error> {
        let graph = self.graph.lock().unwrap();
        graph
            .get_root()
            .context("read social graph root")
            .map_err(|err| UpstreamGraphBackendError(err.to_string()))
    }

    fn set_root(&mut self, root: &str) -> std::result::Result<(), Self::Error> {
        let root_bytes =
            decode_pubkey(root).map_err(|err| UpstreamGraphBackendError(err.to_string()))?;
        SocialGraphStore::set_root(self, &root_bytes)
            .map_err(|err| UpstreamGraphBackendError(err.to_string()))
    }

    fn handle_event(
        &mut self,
        event: &GraphEvent,
        allow_unknown_authors: bool,
        overmute_threshold: f64,
    ) -> std::result::Result<(), Self::Error> {
        let _transaction = self
            .profile_index
            .acquire_exclusive_root_pair_transaction()
            .map_err(|err| UpstreamGraphBackendError(err.to_string()))?;
        require_no_incomplete_profile_repair_for_root_write(
            &self.profile_index.root_pair_lock_path,
        )
        .map_err(|err| UpstreamGraphBackendError(err.to_string()))?;
        self.recover_profile_transactions_locked()
            .map_err(|err| UpstreamGraphBackendError(err.to_string()))?;
        {
            let mut graph = self.graph.lock().unwrap();
            graph
                .handle_event(event, allow_unknown_authors, overmute_threshold)
                .context("ingest social graph event into heed backend")
                .map_err(|err| UpstreamGraphBackendError(err.to_string()))?;
        }
        self.invalidate_distance_cache();
        Ok(())
    }

    fn get_follow_distance(&self, user: &str) -> std::result::Result<u32, Self::Error> {
        let graph = self.graph.lock().unwrap();
        graph
            .get_follow_distance(user)
            .context("read social graph follow distance")
            .map_err(|err| UpstreamGraphBackendError(err.to_string()))
    }

    fn is_following(
        &self,
        follower: &str,
        followed_user: &str,
    ) -> std::result::Result<bool, Self::Error> {
        let graph = self.graph.lock().unwrap();
        graph
            .is_following(follower, followed_user)
            .context("read social graph following edge")
            .map_err(|err| UpstreamGraphBackendError(err.to_string()))
    }

    fn get_followed_by_user(&self, user: &str) -> std::result::Result<Vec<String>, Self::Error> {
        let graph = self.graph.lock().unwrap();
        graph
            .get_followed_by_user(user)
            .context("read followed-by-user list")
            .map_err(|err| UpstreamGraphBackendError(err.to_string()))
    }

    fn get_followers_by_user(&self, user: &str) -> std::result::Result<Vec<String>, Self::Error> {
        let graph = self.graph.lock().unwrap();
        graph
            .get_followers_by_user(user)
            .context("read followers-by-user list")
            .map_err(|err| UpstreamGraphBackendError(err.to_string()))
    }

    fn get_muted_by_user(&self, user: &str) -> std::result::Result<Vec<String>, Self::Error> {
        let graph = self.graph.lock().unwrap();
        graph
            .get_muted_by_user(user)
            .context("read muted-by-user list")
            .map_err(|err| UpstreamGraphBackendError(err.to_string()))
    }

    fn get_user_muted_by(&self, user: &str) -> std::result::Result<Vec<String>, Self::Error> {
        let graph = self.graph.lock().unwrap();
        graph
            .get_user_muted_by(user)
            .context("read user-muted-by list")
            .map_err(|err| UpstreamGraphBackendError(err.to_string()))
    }

    fn get_follow_list_created_at(
        &self,
        user: &str,
    ) -> std::result::Result<Option<u64>, Self::Error> {
        let graph = self.graph.lock().unwrap();
        graph
            .get_follow_list_created_at(user)
            .context("read social graph follow list timestamp")
            .map_err(|err| UpstreamGraphBackendError(err.to_string()))
    }

    fn get_mute_list_created_at(
        &self,
        user: &str,
    ) -> std::result::Result<Option<u64>, Self::Error> {
        let graph = self.graph.lock().unwrap();
        graph
            .get_mute_list_created_at(user)
            .context("read social graph mute list timestamp")
            .map_err(|err| UpstreamGraphBackendError(err.to_string()))
    }

    fn is_overmuted(&self, user: &str, threshold: f64) -> std::result::Result<bool, Self::Error> {
        let graph = self.graph.lock().unwrap();
        graph
            .is_overmuted(user, threshold)
            .context("check social graph overmute")
            .map_err(|err| UpstreamGraphBackendError(err.to_string()))
    }
}

impl<T> SocialGraphBackend for Arc<T>
where
    T: SocialGraphBackend + ?Sized,
{
    fn stats(&self) -> Result<SocialGraphStats> {
        self.as_ref().stats()
    }

    fn users_by_follow_distance(&self, distance: u32) -> Result<Vec<[u8; 32]>> {
        self.as_ref().users_by_follow_distance(distance)
    }

    fn follow_distance(&self, pk_bytes: &[u8; 32]) -> Result<Option<u32>> {
        self.as_ref().follow_distance(pk_bytes)
    }

    fn follow_list_created_at(&self, owner: &[u8; 32]) -> Result<Option<u64>> {
        self.as_ref().follow_list_created_at(owner)
    }

    fn followed_targets(&self, owner: &[u8; 32]) -> Result<UserSet> {
        self.as_ref().followed_targets(owner)
    }

    fn is_overmuted_user(&self, user_pk: &[u8; 32], threshold: f64) -> Result<bool> {
        self.as_ref().is_overmuted_user(user_pk, threshold)
    }

    fn profile_search_root(&self) -> Result<Option<Cid>> {
        self.as_ref().profile_search_root()
    }

    fn snapshot_chunks(&self, root: &[u8; 32], options: &BinaryBudget) -> Result<Vec<Bytes>> {
        self.as_ref().snapshot_chunks(root, options)
    }

    fn ingest_event(&self, event: &Event) -> Result<()> {
        self.as_ref().ingest_event(event)
    }

    fn ingest_event_with_storage_class(
        &self,
        event: &Event,
        storage_class: EventStorageClass,
    ) -> Result<()> {
        self.as_ref()
            .ingest_event_with_storage_class(event, storage_class)
    }

    fn ingest_events(&self, events: &[Event]) -> Result<()> {
        self.as_ref().ingest_events(events)
    }

    fn ingest_events_with_storage_class(
        &self,
        events: &[Event],
        storage_class: EventStorageClass,
    ) -> Result<()> {
        self.as_ref()
            .ingest_events_with_storage_class(events, storage_class)
    }

    fn ingest_graph_events(&self, events: &[Event]) -> Result<()> {
        self.as_ref().ingest_graph_events(events)
    }

    fn query_events(&self, filter: &Filter, limit: usize) -> Result<Vec<Event>> {
        self.as_ref().query_events(filter, limit)
    }
}

fn should_replace_placeholder_root(graph: &HeedSocialGraph) -> Result<bool> {
    if graph.get_root().context("read current social graph root")? != DEFAULT_ROOT_HEX {
        return Ok(false);
    }

    let GraphStats {
        users,
        follows,
        mutes,
        ..
    } = graph.size().context("size social graph")?;
    Ok(users <= 1 && follows == 0 && mutes == 0)
}

fn decode_pubkey_set(values: Vec<String>) -> Result<UserSet> {
    let mut set = UserSet::new();
    for value in values {
        set.insert(decode_pubkey(&value)?);
    }
    Ok(set)
}

fn decode_pubkey(value: &str) -> Result<[u8; 32]> {
    let mut bytes = [0u8; 32];
    hex::decode_to_slice(value, &mut bytes)
        .with_context(|| format!("decode social graph pubkey {value}"))?;
    Ok(bytes)
}

fn is_social_graph_event(kind: Kind) -> bool {
    kind == Kind::ContactList || kind == Kind::MuteList
}

fn is_derived_projection_event(kind: Kind) -> bool {
    kind == Kind::Metadata || is_social_graph_event(kind)
}

fn same_unsigned_event(left: &Event, right: &Event) -> bool {
    left.id == right.id
        && left.pubkey == right.pubkey
        && left.created_at == right.created_at
        && left.kind == right.kind
        && left.tags == right.tags
        && left.content == right.content
}

fn graph_event_from_nostr(event: &Event) -> GraphEvent {
    GraphEvent {
        created_at: event.created_at.as_secs(),
        content: event.content.clone(),
        tags: event
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect(),
        kind: event.kind.as_u16() as u32,
        pubkey: event.pubkey.to_hex(),
        id: event.id.to_hex(),
        sig: event.sig.to_string(),
    }
}

pub(crate) fn stored_event_to_nostr_event(event: StoredNostrEvent) -> Result<Event> {
    Ok(event.to_nostr_sdk_event()?)
}

fn encode_cid(cid: &Cid) -> Result<Vec<u8>> {
    rmp_serde::to_vec_named(&StoredCid {
        hash: cid.hash,
        key: cid.key,
    })
    .context("encode social graph events root")
}

fn decode_cid(bytes: &[u8]) -> Result<Option<Cid>> {
    let stored: StoredCid =
        rmp_serde::from_slice(bytes).context("decode social graph events root")?;
    Ok(Some(cid_from_stored(stored)))
}

fn cid_from_stored(stored: StoredCid) -> Cid {
    Cid {
        hash: stored.hash,
        key: stored.key,
    }
}

fn stored_cid(cid: &Cid) -> StoredCid {
    StoredCid {
        hash: cid.hash,
        key: cid.key,
    }
}

fn read_root_file(path: &Path) -> Result<Option<Cid>> {
    match std::fs::read(path) {
        Ok(bytes) => decode_cid(&bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("read profile root file {}", path.display()))
        }
    }
}

fn read_root_file_snapshot(path: &Path) -> Result<(Option<Cid>, Option<String>)> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let digest = to_hex(&sha256(&bytes));
            Ok((decode_cid(&bytes)?, Some(digest)))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok((None, None)),
        Err(error) => {
            Err(error).with_context(|| format!("read profile root file {}", path.display()))
        }
    }
}

fn write_root_file(path: &Path, root: Option<&Cid>) -> Result<()> {
    let Some(root) = root else {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        return Ok(());
    };

    let encoded = encode_cid(root)?;
    let tmp_path = path.with_extension("tmp");
    std::fs::write(&tmp_path, encoded)?;
    std::fs::rename(tmp_path, path)?;
    Ok(())
}

fn write_root_file_durable(path: &Path, root: Option<&Cid>) -> Result<()> {
    let Some(root) = root else {
        return remove_file_durable(path);
    };

    let encoded = encode_cid(root)?;
    replace_file_durable(path, &encoded, "durable social graph root")?;
    Ok(())
}

fn fsync_parent(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    File::open(parent)
        .with_context(|| format!("open {} for fsync", parent.display()))?
        .sync_all()
        .with_context(|| format!("fsync {}", parent.display()))
}

fn replace_file_durable(path: &Path, bytes: &[u8], label: &str) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create {} parent {}", label, parent.display()))?;
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .with_context(|| format!("{} path is not valid UTF-8", label))?;
    let pending = path.with_file_name(format!(".{file_name}.pending"));
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&pending)
        .with_context(|| format!("open pending {} {}", label, pending.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("write pending {} {}", label, pending.display()))?;
    file.sync_all()
        .with_context(|| format!("fsync pending {} {}", label, pending.display()))?;
    drop(file);
    std::fs::rename(&pending, path)
        .with_context(|| format!("replace {} {}", label, path.display()))?;
    fsync_parent(path)
}

fn remove_file_durable(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => fsync_parent(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

fn profile_root_pair_commit_bytes(commit: &ProfileRootPairCommit) -> Result<Vec<u8>> {
    let mut bytes =
        serde_json::to_vec(commit).context("encode canonical profile root-pair commit")?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn pending_profile_projection_bytes(projection: &PendingProfileProjection) -> Result<Vec<u8>> {
    let mut bytes =
        serde_json::to_vec(projection).context("encode canonical pending profile projection")?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn load_pending_profile_projection(path: &Path) -> Result<Option<PendingProfileProjection>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read pending profile projection {}", path.display()))
        }
    };
    let projection: PendingProfileProjection = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse pending profile projection {}", path.display()))?;
    if projection.version != PROFILE_PROJECTION_PENDING_VERSION {
        anyhow::bail!(
            "unsupported pending profile projection version {} in {}",
            projection.version,
            path.display()
        );
    }
    if pending_profile_projection_bytes(&projection)? != bytes {
        anyhow::bail!(
            "pending profile projection {} is not canonical",
            path.display()
        );
    }
    Ok(Some(projection))
}

fn load_profile_root_pair_commit(path: &Path) -> Result<Option<ProfileRootPairCommit>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read profile root-pair commit {}", path.display()))
        }
    };
    let commit: ProfileRootPairCommit = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse profile root-pair commit {}", path.display()))?;
    if commit.version != PROFILE_ROOT_PAIR_COMMIT_VERSION {
        anyhow::bail!(
            "unsupported profile root-pair commit version {} in {}",
            commit.version,
            path.display()
        );
    }
    if profile_root_pair_commit_bytes(&commit)? != bytes {
        anyhow::bail!(
            "profile root-pair commit {} is not canonical",
            path.display()
        );
    }
    Ok(Some(commit))
}

fn install_profile_root_pair_commit_with<F>(
    by_pubkey_path: &Path,
    search_path: &Path,
    commit_path: &Path,
    commit: &ProfileRootPairCommit,
    after_search: F,
) -> Result<()>
where
    F: FnOnce() -> Result<()>,
{
    let current_by_pubkey = read_root_file(by_pubkey_path)?;
    let current_search = read_root_file(search_path)?;
    let old_by_pubkey = commit.old_by_pubkey.clone().map(cid_from_stored);
    let old_search = commit.old_search.clone().map(cid_from_stored);
    let new_by_pubkey = commit.new_by_pubkey.clone().map(cid_from_stored);
    let new_search = commit.new_search.clone().map(cid_from_stored);
    let old_pair = current_by_pubkey == old_by_pubkey && current_search == old_search;
    let search_first_pair = current_by_pubkey == old_by_pubkey && current_search == new_search;
    let new_pair = current_by_pubkey == new_by_pubkey && current_search == new_search;
    if !old_pair && !search_first_pair && !new_pair {
        anyhow::bail!(
            "profile root-pair files do not match an allowed forward state for {}",
            commit_path.display()
        );
    }

    // The by-pubkey tree is the replay authority: keeping its old root until
    // the new search root is durable lets the same metadata batch reconstruct
    // removals and changed terms after any interruption.
    write_root_file_durable(search_path, new_search.as_ref())?;
    after_search()?;
    write_root_file_durable(by_pubkey_path, new_by_pubkey.as_ref())?;
    remove_file_durable(commit_path)
}

fn require_no_pending_profile_root_pair_commit(db_dir: &Path) -> Result<()> {
    let commit_path = db_dir.join(PROFILE_ROOT_PAIR_COMMIT_FILE);
    if load_profile_root_pair_commit(&commit_path)?.is_some() {
        anyhow::bail!(
            "profile root-pair commit {} is pending; open the writable social graph store to recover it before read-only audit",
            commit_path.display()
        );
    }
    Ok(())
}

fn require_no_pending_profile_projection(db_dir: &Path) -> Result<()> {
    let path = db_dir.join(PROFILE_PROJECTION_PENDING_FILE);
    if load_pending_profile_projection(&path)?.is_some() {
        anyhow::bail!(
            "profile projection {} is pending; open the writable social graph store to recover it before read-only audit",
            path.display()
        );
    }
    Ok(())
}

fn recover_profile_root_pair_commit_locked(db_dir: &Path) -> Result<()> {
    require_no_incomplete_profile_repair_for_root_write(&db_dir.join(PROFILE_ROOT_PAIR_LOCK_FILE))?;
    let commit_path = db_dir.join(PROFILE_ROOT_PAIR_COMMIT_FILE);
    let Some(commit) = load_profile_root_pair_commit(&commit_path)? else {
        return Ok(());
    };
    install_profile_root_pair_commit_with(
        &db_dir.join(PROFILES_BY_PUBKEY_ROOT_FILE),
        &db_dir.join(PROFILE_SEARCH_ROOT_FILE),
        &commit_path,
        &commit,
        || Ok(()),
    )
    .with_context(|| {
        format!(
            "recover interrupted profile root-pair commit {}",
            commit_path.display()
        )
    })
}

fn normalize_profile_name(value: &serde_json::Value) -> Option<String> {
    let raw = value.as_str()?;
    let trimmed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(PROFILE_NAME_MAX_LENGTH).collect())
}

fn extract_profile_names(profile: &serde_json::Map<String, serde_json::Value>) -> Vec<String> {
    let mut names = Vec::new();
    let mut seen = HashSet::new();

    for key in ["display_name", "displayName", "name", "username"] {
        let Some(value) = profile.get(key).and_then(normalize_profile_name) else {
            continue;
        };
        let lowered = value.to_lowercase();
        if seen.insert(lowered) {
            names.push(value);
        }
    }

    names
}

fn should_reject_profile_nip05(local_part: &str, primary_name: &str) -> bool {
    if local_part.len() == 1 || local_part.starts_with("npub1") {
        return true;
    }

    primary_name
        .to_lowercase()
        .split_whitespace()
        .collect::<String>()
        .contains(local_part)
}

fn normalize_profile_nip05(
    profile: &serde_json::Map<String, serde_json::Value>,
    primary_name: Option<&str>,
) -> Option<String> {
    let raw = profile.get("nip05")?.as_str()?;
    let local_part = raw.split('@').next()?.trim().to_lowercase();
    if local_part.is_empty() {
        return None;
    }
    let truncated: String = local_part.chars().take(PROFILE_NAME_MAX_LENGTH).collect();
    if truncated.is_empty() {
        return None;
    }
    if primary_name.is_some_and(|name| should_reject_profile_nip05(&truncated, name)) {
        return None;
    }
    Some(truncated)
}

fn is_search_stop_word(word: &str) -> bool {
    matches!(
        word,
        "a" | "an"
            | "the"
            | "and"
            | "or"
            | "but"
            | "in"
            | "on"
            | "at"
            | "to"
            | "for"
            | "of"
            | "with"
            | "by"
            | "from"
            | "is"
            | "it"
            | "as"
            | "be"
            | "was"
            | "are"
            | "this"
            | "that"
            | "these"
            | "those"
            | "i"
            | "you"
            | "he"
            | "she"
            | "we"
            | "they"
            | "my"
            | "your"
            | "his"
            | "her"
            | "its"
            | "our"
            | "their"
            | "what"
            | "which"
            | "who"
            | "whom"
            | "how"
            | "when"
            | "where"
            | "why"
            | "will"
            | "would"
            | "could"
            | "should"
            | "can"
            | "may"
            | "might"
            | "must"
            | "have"
            | "has"
            | "had"
            | "do"
            | "does"
            | "did"
            | "been"
            | "being"
            | "get"
            | "got"
            | "just"
            | "now"
            | "then"
            | "so"
            | "if"
            | "not"
            | "no"
            | "yes"
            | "all"
            | "any"
            | "some"
            | "more"
            | "most"
            | "other"
            | "into"
            | "over"
            | "after"
            | "before"
            | "about"
            | "up"
            | "down"
            | "out"
            | "off"
            | "through"
            | "during"
            | "under"
            | "again"
            | "further"
            | "once"
    )
}

fn is_pure_search_number(word: &str) -> bool {
    if !word.chars().all(|ch| ch.is_ascii_digit()) {
        return false;
    }
    !(word.len() == 4
        && word
            .parse::<u16>()
            .is_ok_and(|year| (1900..=2099).contains(&year)))
}

fn split_compound_search_word(word: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = word.chars().collect();

    for (index, ch) in chars.iter().copied().enumerate() {
        let split_before = current.chars().last().is_some_and(|prev| {
            (prev.is_lowercase() && ch.is_uppercase())
                || (prev.is_ascii_digit() && ch.is_alphabetic())
                || (prev.is_alphabetic() && ch.is_ascii_digit())
                || (prev.is_uppercase()
                    && ch.is_uppercase()
                    && chars.get(index + 1).is_some_and(|next| next.is_lowercase()))
        });

        if split_before && !current.is_empty() {
            parts.push(std::mem::take(&mut current));
        }

        current.push(ch);
    }

    if !current.is_empty() {
        parts.push(current);
    }

    parts
}

fn parse_search_keywords(text: &str) -> Vec<String> {
    let mut keywords = Vec::new();
    let mut seen = HashSet::new();

    for word in text
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| !word.is_empty())
    {
        let mut variants = Vec::with_capacity(1 + word.len() / 4);
        variants.push(word.to_lowercase());
        variants.extend(
            split_compound_search_word(word)
                .into_iter()
                .map(|part| part.to_lowercase()),
        );

        for lowered in variants {
            if lowered.chars().count() < 2
                || is_search_stop_word(&lowered)
                || is_pure_search_number(&lowered)
            {
                continue;
            }
            if seen.insert(lowered.clone()) {
                keywords.push(lowered);
            }
        }
    }

    keywords
}

#[doc(hidden)]
pub fn profile_search_terms_for_event(event: &Event) -> Vec<String> {
    let profile = match serde_json::from_str::<serde_json::Value>(&event.content) {
        Ok(serde_json::Value::Object(profile)) => profile,
        _ => serde_json::Map::new(),
    };
    let names = extract_profile_names(&profile);
    let primary_name = names.first().map(String::as_str);
    let mut parts = Vec::new();
    if let Some(name) = primary_name {
        parts.push(name.to_string());
    }
    if let Some(nip05) = normalize_profile_nip05(&profile, primary_name) {
        parts.push(nip05);
    }
    parts.push(event.pubkey.to_hex());
    if names.len() > 1 {
        parts.extend(names.into_iter().skip(1));
    }
    parse_search_keywords(&parts.join(" "))
}

#[doc(hidden)]
pub fn profile_search_keys_for_event(event: &Event) -> Vec<String> {
    let pubkey = event.pubkey.to_hex();
    profile_search_terms_for_event(event)
        .into_iter()
        .map(|term| format!("{PROFILE_SEARCH_PREFIX}{term}:{pubkey}"))
        .collect()
}

/// Reconstruct the exact value written by the profile-search index builder.
///
/// This is exposed for read-only integrity auditors. Callers must supply the
/// distance sealed into the index at the time the profile was projected; the
/// current social graph distance is not an equivalent substitute.
#[doc(hidden)]
pub fn stored_profile_search_entry_for_event(
    event: &Event,
    mirrored_cid: &Cid,
    follow_distance: Option<u32>,
) -> Result<StoredProfileSearchEntry> {
    index_buckets::build_profile_search_entry(event, mirrored_cid, follow_distance)
}

/// Seal the historic v2 profile-search distances for one complete retained
/// profile map.
///
/// The map key set must be exactly the retained profile-by-pubkey winner set.
/// `BTreeMap` supplies the required lexicographic UTF-8 pubkey ordering.
#[doc(hidden)]
pub fn profile_follow_distance_seal_v2(distances: &BTreeMap<String, Option<u32>>) -> String {
    let mut digest = Sha256::new();
    digest.update(b"hashtree-profile-follow-distance-seal-v2\0");
    digest.update((distances.len() as u64).to_be_bytes());
    for (pubkey, distance) in distances {
        digest.update((pubkey.len() as u64).to_be_bytes());
        digest.update(pubkey.as_bytes());
        match distance {
            Some(distance) => {
                digest.update([1]);
                digest.update(distance.to_be_bytes());
            }
            None => digest.update([0]),
        }
    }
    hex::encode(digest.finalize())
}

fn compare_nostr_events(left: &Event, right: &Event) -> std::cmp::Ordering {
    left.created_at
        .as_secs()
        .cmp(&right.created_at.as_secs())
        .then_with(|| left.id.to_hex().cmp(&right.id.to_hex()))
}

fn map_event_store_error(err: NostrEventStoreError) -> anyhow::Error {
    anyhow::anyhow!("nostr event store error: {err}")
}

#[cfg(test)]
fn ensure_social_graph_mapsize(db_dir: &Path, requested_bytes: u64) -> Result<()> {
    ensure_social_graph_mapsize_with_env_flags(db_dir, requested_bytes, EnvFlags::empty())
}

fn ensure_social_graph_mapsize_with_env_flags(
    db_dir: &Path,
    requested_bytes: u64,
    env_flags: EnvFlags,
) -> Result<()> {
    let map_size = social_graph_map_size(Some(requested_bytes))?;

    let mut options = heed::EnvOpenOptions::new();
    options.map_size(map_size).max_dbs(SOCIALGRAPH_MAX_DBS);
    unsafe {
        options.flags(env_flags);
    }
    let env = unsafe { ManagedEnv::open(&options, db_dir) }
        .context("open social graph LMDB env for resize")?;
    if env.info().map_size < map_size {
        unsafe { env.resize(map_size) }.context("resize social graph LMDB env")?;
    }

    Ok(())
}

fn social_graph_map_size(requested_bytes: Option<u64>) -> Result<usize> {
    let requested = match requested_bytes {
        Some(bytes) => bytes.max(MIN_SOCIALGRAPH_MAP_SIZE_BYTES),
        None => DEFAULT_SOCIALGRAPH_MAP_SIZE_BYTES,
    };
    let page_size = page_size_bytes() as u64;
    let rounded = requested
        .checked_add(page_size.saturating_sub(1))
        .map(|size| size / page_size * page_size)
        .unwrap_or(requested);
    usize::try_from(rounded).context("social graph mapsize exceeds usize")
}

fn page_size_bytes() -> usize {
    page_size::get_granularity()
}

#[cfg(test)]
mod tests;
