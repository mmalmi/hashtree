use anyhow::{Context, Result};
use futures::executor::block_on as sync_block_on;
use hashtree_core::store::Store;
use hashtree_core::{to_hex, types::Hash, Cid, HashTree, HashTreeConfig, HashTreeError, LinkType};
use serde::de::{self, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs::{File, OpenOptions};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use super::quota::{CacheQuotaAdmission, CacheWritePermit};
use super::{BlobMetadata, HashtreeStore, PRIORITY_FOLLOWED, PRIORITY_OWN};

const MAX_PINNED_TREE_NODES: usize = 10_000_000;
const MAX_UNBOUNDED_PINNED_TREE_BYTES: u64 = 1 << 50;
const ORPHAN_SCAN_PAGE_SIZE: usize = 4_096;
const RETENTION_ROOTS_LOCK_FILE: &str = ".retention-roots.lock";
const MAX_PROFILE_REPAIR_RETENTION_LEASE_BYTES: u64 = 64 * 1024;
const MAX_PROFILE_REPAIR_RETENTION_ROOTS: usize = 64;

pub const PROFILE_REPAIR_RETENTION_LEASE_FORMAT: &str =
    "iris-social/bulk-profile-index-repair-retention@1";
pub const PROFILE_REPAIR_RETENTION_LEASE_RELATIVE_PATH: &str =
    "nostr-index/bulk-projection-v2/profile-repair-v1/retention.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProfileRepairRetentionLease {
    pub format: String,
    pub authority_sha256: String,
    pub roots: BTreeMap<String, String>,
}

impl ProfileRepairRetentionLease {
    pub fn validate(&self) -> Result<()> {
        if self.format != PROFILE_REPAIR_RETENTION_LEASE_FORMAT {
            anyhow::bail!(
                "profile repair retention lease has unsupported format {}",
                self.format
            );
        }
        if self.authority_sha256.len() != 64
            || !self
                .authority_sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            anyhow::bail!(
                "profile repair retention lease authority must be 64 lowercase hexadecimal characters"
            );
        }
        if self.roots.is_empty() || self.roots.len() > MAX_PROFILE_REPAIR_RETENTION_ROOTS {
            anyhow::bail!(
                "profile repair retention lease must contain between 1 and {} roots",
                MAX_PROFILE_REPAIR_RETENTION_ROOTS
            );
        }
        for (label, encoded) in &self.roots {
            if label.is_empty()
                || label.len() > 128
                || !label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
            {
                anyhow::bail!("profile repair retention lease has invalid root label {label:?}");
            }
            let cid = Cid::parse(encoded)
                .map_err(|error| anyhow::anyhow!("invalid retained root {label}: {error}"))?;
            if cid.to_string() != *encoded {
                anyhow::bail!(
                    "profile repair retention lease root {label} is not canonical CID text"
                );
            }
        }
        Ok(())
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let mut bytes =
            serde_json::to_vec(self).map_err(|error| anyhow::anyhow!("encode lease: {error}"))?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    pub fn sha256(&self) -> Result<String> {
        Ok(to_hex(&hashtree_core::sha256(&self.canonical_bytes()?)))
    }

    fn root_cids(&self) -> Result<Vec<Cid>> {
        self.validate()?;
        self.roots
            .iter()
            .map(|(label, encoded)| {
                Cid::parse(encoded)
                    .map_err(|error| anyhow::anyhow!("invalid retained root {label}: {error}"))
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy)]
enum RetentionRootsLockMode {
    Shared,
    Exclusive,
}

pub struct ProfileRepairRetentionPublicationGuard {
    file: File,
}

impl Drop for ProfileRepairRetentionPublicationGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// Acquire the existing retention-root publication lock without creating any
/// path or opening the application blob writer.
///
/// Recovery commands use this before they snapshot profile roots or inspect
/// retained event DAGs. Requiring an existing direct regular file keeps a
/// typo, symlink swap, or incomplete Social namespace from silently creating a
/// different lock authority.
pub fn acquire_existing_profile_repair_retention_guard(
    base_path: &Path,
) -> Result<ProfileRepairRetentionPublicationGuard> {
    let path = base_path.join(RETENTION_ROOTS_LOCK_FILE);
    let before = std::fs::symlink_metadata(&path)
        .with_context(|| format!("inspect existing retention-roots lock {}", path.display()))?;
    if before.file_type().is_symlink() || !before.file_type().is_file() {
        anyhow::bail!(
            "existing retention-roots lock is not a direct regular file: {}",
            path.display()
        );
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("open existing retention-roots lock {}", path.display()))?;
    #[cfg(unix)]
    {
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if result != 0 {
            return Err(std::io::Error::last_os_error()).with_context(|| {
                format!("acquire existing retention-roots lock {}", path.display())
            });
        }
        use std::os::unix::fs::MetadataExt;
        let opened = file
            .metadata()
            .with_context(|| format!("inspect opened retention-roots lock {}", path.display()))?;
        let current = std::fs::symlink_metadata(&path)
            .with_context(|| format!("reinspect retention-roots lock {}", path.display()))?;
        if current.file_type().is_symlink()
            || !current.file_type().is_file()
            || opened.dev() != before.dev()
            || opened.ino() != before.ino()
            || current.dev() != before.dev()
            || current.ino() != before.ino()
        {
            anyhow::bail!(
                "retention-roots lock identity changed while it was acquired: {}",
                path.display()
            );
        }
    }
    #[cfg(not(unix))]
    {
        let _ = before;
        anyhow::bail!("retention-root transactions require an operating-system advisory file lock");
    }
    Ok(ProfileRepairRetentionPublicationGuard { file })
}

pub(super) struct ActiveRetentionProtection {
    _guard: ProfileRepairRetentionPublicationGuard,
    _profile_roots: Option<crate::socialgraph::ProfileRootSnapshotGuard>,
    hashes: HashSet<Hash>,
}

impl ActiveRetentionProtection {
    pub(super) fn contains(&self, hash: &Hash) -> bool {
        self.hashes.contains(hash)
    }

    fn hashes(&self) -> &HashSet<Hash> {
        &self.hashes
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OrphanCleanupProgress {
    freed_bytes: u64,
    scanned: usize,
    sweep_complete: bool,
}

/// Resource limits for validating and indexing a complete pinned DAG.
#[derive(Debug, Clone, Copy)]
pub struct TreeIndexLimits {
    pub max_nodes: usize,
    pub max_bytes: u64,
}

/// Result of atomically indexing and pinning a complete DAG.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinTreeResult {
    pub indexed_hashes: usize,
    pub total_size: u64,
    pub already_pinned: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootRetentionReport {
    pub total_hashes: usize,
    pub reachable_hashes: usize,
    pub pinned_hashes: usize,
    pub candidate_hashes: usize,
    pub deleted_hashes: usize,
    pub logical_bytes_before: u64,
    pub logical_bytes_after: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum PinTreeError {
    #[error("root blob {hash} is missing")]
    MissingRoot { hash: String },
    #[error("descendant blob {hash} is missing")]
    MissingDescendant { hash: String },
    #[error("invalid DAG node {hash}: {message}")]
    InvalidDag { hash: String, message: String },
    #[error("DAG exceeds the {max_nodes} node limit")]
    NodeLimitExceeded { max_nodes: usize },
    #[error("DAG exceeds the {max_bytes} byte limit")]
    ByteLimitExceeded { max_bytes: u64 },
    #[error("storage error: {0}")]
    Storage(String),
}

struct TreeIndexPlan {
    tracked_hashes: HashSet<Hash>,
    total_size: u64,
}

/// Metadata for a synced tree (for eviction tracking)
#[derive(Debug, Clone, Serialize)]
pub struct TreeMeta {
    /// Pubkey of tree owner
    pub owner: String,
    /// Tree name if known (from nostr key like "npub.../name")
    pub name: Option<String>,
    /// Unix timestamp when this tree was synced
    pub synced_at: u64,
    /// Total size of all blobs in this tree
    pub total_size: u64,
    /// Eviction priority: 255=own/pinned, 128=followed, 64=other
    pub priority: u8,
}

impl<'de> Deserialize<'de> for TreeMeta {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        const FIELDS: &[&str] = &[
            "owner",
            "name",
            "synced_at",
            "last_accessed_at",
            "total_size",
            "priority",
        ];

        struct TreeMetaVisitor;

        impl<'de> Visitor<'de> for TreeMetaVisitor {
            type Value = TreeMeta;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("TreeMeta as current or legacy metadata")
            }

            fn visit_seq<A>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let has_accidental_access_field = matches!(seq.size_hint(), Some(6));
                let owner = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(0, &self))?;
                let name = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(1, &self))?;
                let synced_at = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(2, &self))?;

                if has_accidental_access_field {
                    let _: IgnoredAny = seq
                        .next_element()?
                        .ok_or_else(|| de::Error::invalid_length(3, &self))?;
                }

                let total_size = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(3, &self))?;
                let priority = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(4, &self))?;

                Ok(TreeMeta {
                    owner,
                    name,
                    synced_at,
                    total_size,
                    priority,
                })
            }

            fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut owner = None;
                let mut name = None;
                let mut synced_at = None;
                let mut total_size = None;
                let mut priority = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "owner" => owner = Some(map.next_value()?),
                        "name" => name = Some(map.next_value()?),
                        "synced_at" => synced_at = Some(map.next_value()?),
                        "last_accessed_at" => {
                            let _: IgnoredAny = map.next_value()?;
                        }
                        "total_size" => total_size = Some(map.next_value()?),
                        "priority" => priority = Some(map.next_value()?),
                        _ => {
                            let _: IgnoredAny = map.next_value()?;
                        }
                    }
                }

                Ok(TreeMeta {
                    owner: owner.ok_or_else(|| de::Error::missing_field("owner"))?,
                    name: name.unwrap_or(None),
                    synced_at: synced_at.ok_or_else(|| de::Error::missing_field("synced_at"))?,
                    total_size: total_size.ok_or_else(|| de::Error::missing_field("total_size"))?,
                    priority: priority.ok_or_else(|| de::Error::missing_field("priority"))?,
                })
            }
        }

        deserializer.deserialize_struct("TreeMeta", FIELDS, TreeMetaVisitor)
    }
}

#[derive(Debug)]
pub struct StorageStats {
    pub total_dags: usize,
    pub pinned_dags: usize,
    pub total_bytes: u64,
}

/// Storage usage broken down by priority tier
#[derive(Debug, Clone)]
pub struct StorageByPriority {
    /// Own/pinned trees (priority 255)
    pub own: u64,
    /// Followed users' trees (priority 128)
    pub followed: u64,
    /// Other trees (priority 64)
    pub other: u64,
}

#[derive(Debug, Clone)]
pub struct PinnedItem {
    pub cid: String,
    pub name: String,
    pub is_directory: bool,
    pub size_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct OwnedBlobStats {
    pub owner: [u8; 32],
    pub count: usize,
    pub total_bytes: u64,
}

fn pinned_item_name(hash: &Hash, meta: Option<&TreeMeta>) -> String {
    let Some(meta) = meta else {
        return to_hex(hash);
    };

    match (meta.owner.as_str(), meta.name.as_deref()) {
        ("pinned", Some(name)) => name.to_string(),
        ("", Some(name)) => name.to_string(),
        (owner, Some(name)) if !owner.is_empty() => format!("{owner}/{name}"),
        (owner, None) if !owner.is_empty() && owner != "pinned" => owner.to_string(),
        _ => to_hex(hash),
    }
}

fn unix_timestamp_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl HashtreeStore {
    fn acquire_retention_roots_lock(
        &self,
        mode: RetentionRootsLockMode,
    ) -> Result<ProfileRepairRetentionPublicationGuard> {
        let path = self.base_path().join(RETENTION_ROOTS_LOCK_FILE);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("open retention-roots lock {}", path.display()))?;
        #[cfg(unix)]
        {
            let operation = match mode {
                RetentionRootsLockMode::Shared => libc::LOCK_SH,
                RetentionRootsLockMode::Exclusive => libc::LOCK_EX,
            };
            let result = unsafe { libc::flock(file.as_raw_fd(), operation) };
            if result != 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("acquire retention-roots lock {}", path.display()));
            }
        }
        #[cfg(not(unix))]
        {
            let _ = mode;
            anyhow::bail!(
                "retention-root transactions require an operating-system advisory file lock"
            );
        }
        Ok(ProfileRepairRetentionPublicationGuard { file })
    }

    /// Serialize publication of the immutable Social repair retention lease
    /// against every local retention deletion pass. Persist and fsync the
    /// canonical lease while this guard is alive.
    pub fn acquire_profile_repair_retention_publication_guard(
        &self,
    ) -> Result<ProfileRepairRetentionPublicationGuard> {
        self.acquire_retention_roots_lock(RetentionRootsLockMode::Exclusive)
    }

    pub fn profile_repair_retention_lease_path(&self) -> PathBuf {
        self.base_path()
            .join(PROFILE_REPAIR_RETENTION_LEASE_RELATIVE_PATH)
    }

    pub fn validate_profile_repair_retention_lease(
        &self,
        expected_sha256: &str,
    ) -> Result<ProfileRepairRetentionLease> {
        let path = self.profile_repair_retention_lease_path();
        let metadata = std::fs::symlink_metadata(&path)
            .with_context(|| format!("inspect {}", path.display()))?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            anyhow::bail!(
                "profile repair retention lease is not a direct regular file: {}",
                path.display()
            );
        }
        if metadata.len() > MAX_PROFILE_REPAIR_RETENTION_LEASE_BYTES {
            anyhow::bail!(
                "profile repair retention lease exceeds {} bytes: {}",
                MAX_PROFILE_REPAIR_RETENTION_LEASE_BYTES,
                path.display()
            );
        }
        let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        let actual_sha256 = to_hex(&hashtree_core::sha256(&bytes));
        if actual_sha256 != expected_sha256 {
            anyhow::bail!(
                "profile repair retention lease SHA-256 mismatch: expected {expected_sha256}, found {actual_sha256}"
            );
        }
        let lease: ProfileRepairRetentionLease =
            serde_json::from_slice(&bytes).with_context(|| format!("decode {}", path.display()))?;
        if lease.canonical_bytes()? != bytes {
            anyhow::bail!(
                "profile repair retention lease is not canonical: {}",
                path.display()
            );
        }
        Ok(lease)
    }

    /// Freeze the active immutable repair lease and resolve every full-CID DAG
    /// it names while holding the shared side of the lease-publication lock.
    /// Keep this value alive through the complete deletion pass.
    pub(super) fn active_retention_protection(&self) -> Result<ActiveRetentionProtection> {
        let guard = self.acquire_retention_roots_lock(RetentionRootsLockMode::Shared)?;
        let profile_roots =
            crate::socialgraph::acquire_profile_root_snapshot_guard(self.base_path())?;
        let mut roots = match profile_roots.as_ref() {
            Some(profile_roots) => profile_roots.retention_roots()?,
            None => Vec::new(),
        };
        roots.extend(self.profile_repair_retention_roots()?);
        roots.sort_by_key(Cid::to_string);
        roots.dedup();
        let hashes = self.collect_socialgraph_protected(&roots)?;
        Ok(ActiveRetentionProtection {
            _guard: guard,
            _profile_roots: profile_roots,
            hashes,
        })
    }

    /// Retain only one Nostr event-index DAG plus explicit pins in the writable
    /// blob store. This is intended for a dedicated, closed-writer index store.
    ///
    /// Nostr B-tree links mark event values as files even though the values are
    /// direct blobs. Traversing directory/fanout links only therefore visits
    /// every index node while avoiding millions of unnecessary event reads.
    pub fn retain_nostr_root(&self, root: &Cid, apply: bool) -> Result<RootRetentionReport> {
        let retention = self.active_retention_protection()?;
        let tree = HashTree::new(HashTreeConfig::new(self.store_arc()));
        let mut reachable = sync_block_on(async {
            let mut reachable = HashSet::new();
            let mut stack = vec![(root.clone(), "root".to_string())];
            while let Some((cid, path)) = stack.pop() {
                if !reachable.insert(cid.hash) {
                    continue;
                }
                let node = tree
                    .get_tree_node_by_cid(&cid)
                    .await
                    .map_err(|error| anyhow::anyhow!("read retained root DAG: {error}"))?
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "retained directory node {} is missing at {path}",
                            to_hex(&cid.hash)
                        )
                    })?;
                for link in node.links {
                    if link.link_type.is_directory_like() {
                        let name = link.name.as_deref().unwrap_or("<unnamed>");
                        stack.push((link.to_cid(), format!("{path}/{name}")));
                    } else {
                        reachable.insert(link.hash);
                    }
                }
            }
            Ok::<_, anyhow::Error>(reachable)
        })?;
        reachable.extend(retention.hashes().iter().copied());

        let rtxn = self.env.read_txn()?;
        let pinned = self
            .pins
            .iter(&rtxn)?
            .filter_map(std::result::Result::ok)
            .filter_map(|(hash, _)| hash.try_into().ok())
            .collect::<HashSet<Hash>>();
        drop(rtxn);

        let stats_before = self
            .router
            .writable_stats()
            .map_err(|error| anyhow::anyhow!("read writable storage stats: {error}"))?;
        let all_hashes = self
            .router
            .list_writable()
            .map_err(|error| anyhow::anyhow!("list writable hashes: {error}"))?;
        let candidates = all_hashes
            .iter()
            .filter(|hash| !reachable.contains(*hash) && !pinned.contains(*hash))
            .copied()
            .collect::<Vec<_>>();

        let mut deleted = 0usize;
        if apply {
            const DELETE_BATCH_SIZE: usize = 16_384;
            for (batch_index, batch) in candidates.chunks(DELETE_BATCH_SIZE).enumerate() {
                deleted = deleted.saturating_add(
                    self.router
                        .delete_many_local_only(batch)
                        .map_err(|error| anyhow::anyhow!("delete unreachable batch: {error}"))?,
                );
                if (batch_index + 1).is_multiple_of(32) {
                    eprintln!(
                        "Retained-root cleanup: deleted {deleted}/{} unreachable hashes",
                        candidates.len()
                    );
                }
            }
        }
        let stats_after = self
            .router
            .writable_stats()
            .map_err(|error| anyhow::anyhow!("read writable storage stats: {error}"))?;

        Ok(RootRetentionReport {
            total_hashes: all_hashes.len(),
            reachable_hashes: reachable.len(),
            pinned_hashes: pinned.len(),
            candidate_hashes: candidates.len(),
            deleted_hashes: deleted,
            logical_bytes_before: stats_before.total_bytes,
            logical_bytes_after: stats_after.total_bytes,
        })
    }

    async fn collect_tree_hashes<S: Store>(
        &self,
        tree: &HashTree<S>,
        root: &Cid,
        require_tree_root: bool,
    ) -> Result<HashSet<Hash>> {
        let mut hashes = HashSet::new();
        let mut stack = vec![(root.clone(), true)];

        while let Some((cid, is_root)) = stack.pop() {
            if !hashes.insert(cid.hash) {
                continue;
            }

            let node = tree
                .get_node(&cid)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to get protected tree node: {}", e))?;
            if let Some(node) = node {
                for link in &node.links {
                    stack.push((link.to_cid(), false));
                }
            } else if is_root && require_tree_root {
                anyhow::bail!(
                    "protected retention root {} is missing or is not a tree",
                    cid
                );
            }
        }

        Ok(hashes)
    }

    fn profile_repair_retention_roots(&self) -> Result<Vec<Cid>> {
        let path = self.profile_repair_retention_lease_path();
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error).with_context(|| format!("inspect {}", path.display()));
            }
        };
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            anyhow::bail!(
                "profile repair retention lease is not a direct regular file: {}",
                path.display()
            );
        }
        if metadata.len() > MAX_PROFILE_REPAIR_RETENTION_LEASE_BYTES {
            anyhow::bail!(
                "profile repair retention lease exceeds {} bytes: {}",
                MAX_PROFILE_REPAIR_RETENTION_LEASE_BYTES,
                path.display()
            );
        }
        let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        let lease: ProfileRepairRetentionLease =
            serde_json::from_slice(&bytes).with_context(|| format!("decode {}", path.display()))?;
        if lease.canonical_bytes()? != bytes {
            anyhow::bail!(
                "profile repair retention lease is not canonical: {}",
                path.display()
            );
        }
        lease.root_cids()
    }

    fn collect_socialgraph_protected(&self, roots: &[Cid]) -> Result<HashSet<Hash>> {
        let mut protected = HashSet::new();
        let tree = HashTree::new(HashTreeConfig::new(self.store_arc()).public());
        for root in roots {
            protected.extend(sync_block_on(self.collect_tree_hashes(&tree, root, true))?);
        }
        Ok(protected)
    }

    /// Return the socialgraph protection snapshot for the active bounded sweep.
    ///
    /// Root changes during a sweep are unioned with the existing snapshot.
    /// This can temporarily over-protect an old DAG, but can never make either
    /// the old or new root disposable. The union is discarded at the sweep
    /// boundary and rebuilt from the current roots.
    fn socialgraph_protected_for_orphan_sweep(&self, roots: &[Cid]) -> Result<Arc<HashSet<Hash>>> {
        let root_ids = roots.iter().map(Cid::to_string).collect::<Vec<_>>();
        {
            let state = self
                .orphan_scan
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.socialgraph_roots.as_ref() == Some(&root_ids) {
                return Ok(Arc::clone(&state.socialgraph_protected));
            }
        }

        let newly_protected = self.collect_socialgraph_protected(roots)?;
        let mut state = self
            .orphan_scan
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.socialgraph_roots.as_ref() == Some(&root_ids) {
            return Ok(Arc::clone(&state.socialgraph_protected));
        }

        let protected = if state.sweep.is_some() && state.socialgraph_roots.is_some() {
            let mut union = (*state.socialgraph_protected).clone();
            union.extend(newly_protected);
            union
        } else {
            newly_protected
        };
        state.socialgraph_roots = Some(root_ids);
        state.socialgraph_protected = Arc::new(protected);
        Ok(Arc::clone(&state.socialgraph_protected))
    }

    fn metadata_protects_orphan(&self, hash: &Hash) -> Result<bool> {
        let rtxn = self.env.read_txn()?;
        if self.pins.get(&rtxn, hash.as_slice())?.is_some() {
            return Ok(true);
        }
        if self
            .blob_trees
            .prefix_iter(&rtxn, hash.as_slice())?
            .next()
            .transpose()?
            .is_some()
        {
            return Ok(true);
        }
        let has_owner = self
            .blob_owners
            .prefix_iter(&rtxn, hash.as_slice())?
            .next()
            .transpose()?
            .is_some();
        Ok(has_owner)
    }

    fn evict_disposable_orphans_page(
        &self,
        target_bytes: u64,
        additional_protected: &HashSet<Hash>,
        page_size: usize,
    ) -> Result<OrphanCleanupProgress> {
        if page_size == 0 {
            anyhow::bail!("orphan cleanup page size must be greater than zero");
        }

        let _retention_roots = self.acquire_retention_roots_lock(RetentionRootsLockMode::Shared)?;
        let profile_roots =
            crate::socialgraph::acquire_profile_root_snapshot_guard(self.base_path())?;
        let mut roots = match profile_roots.as_ref() {
            Some(profile_roots) => profile_roots.retention_roots()?,
            None => Vec::new(),
        };
        roots.extend(self.profile_repair_retention_roots()?);
        roots.sort_by_key(Cid::to_string);
        roots.dedup();
        let stats = self
            .router
            .writable_stats()
            .map_err(|e| anyhow::anyhow!("Failed to get writable stats: {}", e))?;
        let mut current_size = stats.total_bytes;
        if current_size <= target_bytes {
            let mut state = self
                .orphan_scan
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.sweep = None;
            state.socialgraph_roots = None;
            state.socialgraph_protected = Arc::new(HashSet::new());
            return Ok(OrphanCleanupProgress {
                freed_bytes: 0,
                scanned: 0,
                sweep_complete: false,
            });
        }

        let (after, start_after, wrapped) = {
            let mut state = self
                .orphan_scan
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.sweep.is_none() {
                state.sweep = Some(super::OrphanSweep {
                    start_after: state.cursor,
                    wrapped: false,
                });
            }
            let sweep = state.sweep.expect("orphan sweep initialized");
            (state.cursor, sweep.start_after, sweep.wrapped)
        };
        let socialgraph_protected = self.socialgraph_protected_for_orphan_sweep(&roots)?;
        let mut candidates = self
            .router
            .scan_writable_hashes_after(after, page_size)
            .map_err(|e| anyhow::anyhow!("Failed to scan writable hashes: {}", e))?;
        let backend_page_len = candidates.len();
        let mut crossed_start = false;
        if wrapped {
            let start_after = start_after.expect("only a non-zero cursor sweep wraps");
            let keep = candidates.partition_point(|hash| *hash <= start_after);
            crossed_start = keep < candidates.len();
            candidates.truncate(keep);
        }

        let mut freed = 0u64;
        let mut scanned = 0usize;
        let mut last_examined = None;
        for hash in &candidates {
            if current_size <= target_bytes {
                break;
            }
            let hash = *hash;
            scanned += 1;
            last_examined = Some(hash);

            if socialgraph_protected.contains(&hash)
                || additional_protected.contains(&hash)
                || self.metadata_protects_orphan(&hash)?
            {
                continue;
            }

            let Some(_delete_guard) = self.cache_quota.begin_retention_delete(hash) else {
                continue;
            };
            // Recheck all durable metadata after the deletion claim. Ownership
            // writers serialize with the claim; the point checks also narrow
            // the pin/index publication window without materializing either DB.
            if self.metadata_protects_orphan(&hash)? {
                continue;
            }

            let Some(size) = self
                .router
                .blob_size_sync(&hash)
                .map_err(|e| anyhow::anyhow!("Failed to get blob size: {}", e))?
            else {
                continue;
            };

            if self
                .router
                .delete_local_only(&hash)
                .map_err(|e| anyhow::anyhow!("Failed to delete orphaned blob: {}", e))?
            {
                freed = freed.saturating_add(size);
                current_size = current_size.saturating_sub(size);
                tracing::debug!(
                    "Deleted disposable orphaned blob {} ({} bytes)",
                    &to_hex(&hash)[..8],
                    size
                );
            }
        }

        let target_reached = current_size <= target_bytes;
        let processed_whole_page = scanned == candidates.len();
        let backend_exhausted = backend_page_len < page_size;
        let mut sweep_complete = false;
        let mut state = self
            .orphan_scan
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(last_examined) = last_examined {
            state.cursor = Some(last_examined);
        }

        if target_reached {
            state.sweep = None;
            state.socialgraph_roots = None;
            state.socialgraph_protected = Arc::new(HashSet::new());
        } else if processed_whole_page {
            let sweep = state.sweep.expect("active orphan sweep");
            if sweep.wrapped {
                if crossed_start || backend_exhausted {
                    state.cursor = sweep.start_after;
                    state.sweep = None;
                    sweep_complete = true;
                }
            } else if backend_exhausted {
                if sweep.start_after.is_none() {
                    state.cursor = None;
                    state.sweep = None;
                    sweep_complete = true;
                } else {
                    state.cursor = None;
                    state.sweep = Some(super::OrphanSweep {
                        wrapped: true,
                        ..sweep
                    });
                }
            }
            if sweep_complete {
                state.socialgraph_roots = None;
                state.socialgraph_protected = Arc::new(HashSet::new());
            }
        }

        Ok(OrphanCleanupProgress {
            freed_bytes: freed,
            scanned,
            sweep_complete,
        })
    }

    fn evict_disposable_orphans_to_target_raw(
        &self,
        target_bytes: u64,
        additional_protected: &HashSet<Hash>,
    ) -> Result<OrphanCleanupProgress> {
        self.evict_disposable_orphans_page(
            target_bytes,
            additional_protected,
            ORPHAN_SCAN_PAGE_SIZE,
        )
    }

    fn evict_disposable_orphans_to_target(&self, target_bytes: u64) -> Result<u64> {
        let cleanup = self
            .cache_quota
            .begin_standalone_cleanup()
            .map_err(|denial| anyhow::anyhow!(denial.to_string()))?;
        let result =
            self.evict_disposable_orphans_to_target_raw(target_bytes, cleanup.inflight_hashes());
        if result.is_ok() {
            let after_usage = self
                .router
                .writable_stats()
                .map_err(|error| {
                    anyhow::anyhow!("Failed to get writable stats after cache cleanup: {error}")
                })?
                .total_bytes;
            cleanup.complete(after_usage);
        }
        result.map(|progress| progress.freed_bytes)
    }

    pub(super) fn prepare_cached_blob_write(
        &self,
        incoming_bytes: u64,
        hashes: Vec<Hash>,
        force_cleanup: bool,
    ) -> Result<CacheWritePermit<'_>> {
        if let Some(denial) = self.cache_quota.quick_denial() {
            anyhow::bail!(denial.to_string());
        }

        let observed_usage = if self.max_size_bytes == 0 {
            0
        } else {
            self.router
                .writable_stats()
                .map_err(|error| anyhow::anyhow!("Failed to get writable stats: {error}"))?
                .total_bytes
        };
        match self
            .cache_quota
            .begin_admission(
                observed_usage,
                incoming_bytes,
                hashes,
                self.max_size_bytes,
                force_cleanup,
            )
            .map_err(|denial| anyhow::anyhow!(denial.to_string()))?
        {
            CacheQuotaAdmission::Admitted(permit) => Ok(permit),
            CacheQuotaAdmission::Cleanup(cleanup) => {
                let progress = self.evict_disposable_orphans_to_target_raw(
                    cleanup.target_bytes(),
                    cleanup.inflight_hashes(),
                )?;
                let after_usage = self
                    .router
                    .writable_stats()
                    .map_err(|error| {
                        anyhow::anyhow!("Failed to get writable stats after cache cleanup: {error}")
                    })?
                    .total_bytes;
                cleanup
                    .complete(after_usage, progress.freed_bytes, progress.sweep_complete)
                    .map_err(|denial| anyhow::anyhow!(denial.to_string()))
            }
        }
    }

    /// Number of cache/orphan cleanup leadership epochs started by this store.
    ///
    /// This is intentionally a cheap diagnostic so overload tests and operators
    /// can verify that concurrent cache pressure coalesces into one scanner.
    pub fn cache_cleanup_epoch_count(&self) -> u64 {
        self.cache_quota.cleanup_epoch_count()
    }

    pub fn make_room_for_cached_blob(&self, incoming_bytes: u64) -> Result<u64> {
        if self.max_size_bytes == 0 {
            return Ok(0);
        }

        let stats = self
            .router
            .writable_stats()
            .map_err(|e| anyhow::anyhow!("Failed to get writable stats: {}", e))?;
        if stats.total_bytes.saturating_add(incoming_bytes) <= self.max_size_bytes {
            return Ok(0);
        }

        let target = if incoming_bytes >= self.max_size_bytes {
            0
        } else {
            (self.max_size_bytes.saturating_mul(9) / 10)
                .min(self.max_size_bytes.saturating_sub(incoming_bytes))
        };
        self.evict_disposable_orphans_to_target(target)
    }

    pub fn enforce_cached_blob_budget_after_insert(&self, inserted_bytes: u64) -> Result<u64> {
        if self.max_size_bytes == 0 || inserted_bytes == 0 {
            return Ok(0);
        }

        let stats = self
            .router
            .writable_stats()
            .map_err(|e| anyhow::anyhow!("Failed to get writable stats: {}", e))?;
        if stats.total_bytes <= self.max_size_bytes {
            return Ok(0);
        }

        let target = if inserted_bytes >= self.max_size_bytes {
            inserted_bytes
        } else {
            (self.max_size_bytes.saturating_mul(9) / 10)
                .saturating_add(inserted_bytes)
                .min(self.max_size_bytes)
        };
        self.evict_disposable_orphans_to_target(target)
    }

    pub fn make_room_for_durable_blob(&self, incoming_bytes: u64) -> Result<u64> {
        if self.max_size_bytes == 0 || incoming_bytes == 0 {
            return Ok(0);
        }

        if incoming_bytes > self.max_size_bytes {
            anyhow::bail!(
                "storage limit exceeded: incoming blob is {} bytes but limit is {} bytes",
                incoming_bytes,
                self.max_size_bytes
            );
        }

        let stats = self
            .router
            .writable_stats()
            .map_err(|e| anyhow::anyhow!("Failed to get writable stats: {}", e))?;
        if stats.total_bytes.saturating_add(incoming_bytes) <= self.max_size_bytes {
            return Ok(0);
        }

        let target = (self.max_size_bytes.saturating_mul(9) / 10)
            .min(self.max_size_bytes.saturating_sub(incoming_bytes));
        let freed = self.evict_with_policy_to_target(stats.total_bytes, target)?;

        let next_stats = self
            .router
            .writable_stats()
            .map_err(|e| anyhow::anyhow!("Failed to get writable stats after eviction: {}", e))?;
        if next_stats.total_bytes.saturating_add(incoming_bytes) > self.max_size_bytes {
            anyhow::bail!(
                "storage limit exceeded: {} bytes used, {} byte incoming blob, {} byte limit",
                next_stats.total_bytes,
                incoming_bytes,
                self.max_size_bytes
            );
        }

        Ok(freed)
    }

    pub fn enforce_durable_blob_budget_after_insert(&self, inserted_bytes: u64) -> Result<u64> {
        if self.max_size_bytes == 0 || inserted_bytes == 0 {
            return Ok(0);
        }

        if inserted_bytes > self.max_size_bytes {
            anyhow::bail!(
                "storage limit exceeded: inserted blobs are {} bytes but limit is {} bytes",
                inserted_bytes,
                self.max_size_bytes
            );
        }

        let stats = self
            .router
            .writable_stats()
            .map_err(|e| anyhow::anyhow!("Failed to get writable stats: {}", e))?;
        if stats.total_bytes <= self.max_size_bytes {
            return Ok(0);
        }

        let target = (self.max_size_bytes.saturating_mul(9) / 10)
            .saturating_add(inserted_bytes)
            .min(self.max_size_bytes);
        let freed = self.evict_with_policy_to_target(stats.total_bytes, target)?;

        let next_stats = self
            .router
            .writable_stats()
            .map_err(|e| anyhow::anyhow!("Failed to get writable stats after eviction: {}", e))?;
        if next_stats.total_bytes > self.max_size_bytes {
            anyhow::bail!(
                "storage limit exceeded: {} bytes used after inserting {} bytes, {} byte limit",
                next_stats.total_bytes,
                inserted_bytes,
                self.max_size_bytes
            );
        }

        Ok(freed)
    }

    pub fn relieve_cached_blob_write_pressure(&self, incoming_bytes: u64) -> Result<u64> {
        let stats = self
            .router
            .writable_stats()
            .map_err(|e| anyhow::anyhow!("Failed to get writable stats: {}", e))?;
        if stats.total_bytes == 0 {
            return Ok(0);
        }

        let headroom = incoming_bytes.max(stats.total_bytes / 10).max(1);
        let target = stats.total_bytes.saturating_sub(headroom);
        self.evict_disposable_orphans_to_target(target)
    }

    /// Pin a hash (prevent garbage collection)
    pub fn pin(&self, hash: &[u8; 32]) -> Result<()> {
        let mut wtxn = self.env.write_txn()?;
        self.pins.put(&mut wtxn, hash.as_slice(), &())?;
        wtxn.commit()?;
        Ok(())
    }

    /// Unpin a hash (allow garbage collection)
    pub fn unpin(&self, hash: &[u8; 32]) -> Result<()> {
        let mut wtxn = self.env.write_txn()?;
        self.pins.delete(&mut wtxn, hash.as_slice())?;
        wtxn.commit()?;
        Ok(())
    }

    /// Check if hash is pinned
    pub fn is_pinned(&self, hash: &[u8; 32]) -> Result<bool> {
        let rtxn = self.env.read_txn()?;
        Ok(self.pins.get(&rtxn, hash.as_slice())?.is_some())
    }

    /// List all pinned hashes (raw bytes)
    pub fn list_pins_raw(&self) -> Result<Vec<[u8; 32]>> {
        let rtxn = self.env.read_txn()?;
        let mut pins = Vec::new();

        for item in self.pins.iter(&rtxn)? {
            let (hash_bytes, _) = item?;
            if hash_bytes.len() == 32 {
                let mut hash = [0u8; 32];
                hash.copy_from_slice(hash_bytes);
                pins.push(hash);
            }
        }

        Ok(pins)
    }

    /// List all pinned hashes with names
    pub fn list_pins_with_names(&self) -> Result<Vec<PinnedItem>> {
        let rtxn = self.env.read_txn()?;
        let store = self.store_arc();
        let tree = HashTree::new(HashTreeConfig::new(store).public());
        let mut pins = Vec::new();

        for item in self.pins.iter(&rtxn)? {
            let (hash_bytes, _) = item?;
            if hash_bytes.len() != 32 {
                continue;
            }
            let mut hash = [0u8; 32];
            hash.copy_from_slice(hash_bytes);

            // Try to determine if it's a directory
            let is_directory =
                sync_block_on(async { tree.is_directory(&hash).await.unwrap_or(false) });

            let meta = self
                .tree_meta
                .get(&rtxn, hash.as_slice())?
                .map(|bytes| {
                    rmp_serde::from_slice::<TreeMeta>(bytes)
                        .map_err(|e| anyhow::anyhow!("Failed to deserialize TreeMeta: {}", e))
                })
                .transpose()?;
            let size_bytes = if let Some(meta) = meta.as_ref() {
                meta.total_size
            } else {
                self.router
                    .blob_size_sync(&hash)
                    .map_err(|e| anyhow::anyhow!("Failed to get pinned blob size: {}", e))?
                    .unwrap_or(0)
            };

            pins.push(PinnedItem {
                cid: to_hex(&hash),
                name: pinned_item_name(&hash, meta.as_ref()),
                is_directory,
                size_bytes,
            });
        }

        Ok(pins)
    }

    pub fn owned_blob_stats(&self) -> Result<Vec<OwnedBlobStats>> {
        let rtxn = self.env.read_txn()?;
        let mut owners = Vec::new();

        for item in self.pubkey_blobs.iter(&rtxn)? {
            let (owner_bytes, blobs_bytes) = item?;
            if owner_bytes.len() != 32 {
                continue;
            }

            let blobs: Vec<BlobMetadata> = serde_json::from_slice(blobs_bytes)
                .map_err(|e| anyhow::anyhow!("Failed to deserialize blob metadata: {}", e))?;
            let mut owner = [0u8; 32];
            owner.copy_from_slice(owner_bytes);
            let total_bytes = blobs
                .iter()
                .fold(0u64, |total, blob| total.saturating_add(blob.size));
            owners.push(OwnedBlobStats {
                owner,
                count: blobs.len(),
                total_bytes,
            });
        }

        owners.sort_by_key(|stats| stats.owner);
        Ok(owners)
    }

    // === Tree indexing for eviction ===

    /// Bounds complete-DAG indexing by both the configured storage budget and
    /// an absolute traversal-node ceiling. A zero storage budget means the raw
    /// store is unbounded, but authenticated requests still retain a hard cap.
    pub fn tree_index_limits(&self) -> TreeIndexLimits {
        TreeIndexLimits {
            max_nodes: MAX_PINNED_TREE_NODES,
            max_bytes: if self.max_size_bytes == 0 {
                MAX_UNBOUNDED_PINNED_TREE_BYTES
            } else {
                self.max_size_bytes
            },
        }
    }

    /// Validate every referenced blob, then index all descendants and pin the
    /// root in one LMDB transaction. No pin or index metadata is written if
    /// traversal, decryption, decoding, or resource validation fails.
    pub fn pin_and_index_tree(
        &self,
        root: &Cid,
        owner: &str,
        name: Option<&str>,
        priority: u8,
        limits: TreeIndexLimits,
    ) -> std::result::Result<PinTreeResult, PinTreeError> {
        let store = self.store_arc();
        let tree = HashTree::new(HashTreeConfig::new(store).public());
        let plan = sync_block_on(self.collect_tree_index(&tree, root, limits))?;
        let already_pinned = self
            .write_tree_index(
                &root.hash,
                &plan.tracked_hashes,
                plan.total_size,
                owner,
                name,
                priority,
                None,
                true,
            )
            .map_err(|error| PinTreeError::Storage(error.to_string()))?;

        Ok(PinTreeResult {
            indexed_hashes: plan.tracked_hashes.len(),
            total_size: plan.total_size,
            already_pinned,
        })
    }

    /// Index a tree after sync - tracks all blobs in the tree for eviction
    ///
    /// If `ref_key` is provided (e.g. "npub.../name"), it will replace any existing
    /// tree with that ref, allowing old versions to be evicted.
    pub fn index_tree(
        &self,
        root_hash: &Hash,
        owner: &str,
        name: Option<&str>,
        priority: u8,
        ref_key: Option<&str>,
    ) -> Result<()> {
        let root_hex = to_hex(root_hash);

        // If ref_key provided, check for and unindex old version
        if let Some(key) = ref_key {
            let rtxn = self.env.read_txn()?;
            if let Some(old_hash_bytes) = self.tree_refs.get(&rtxn, key)? {
                if old_hash_bytes != root_hash.as_slice() {
                    let old_hash: Hash = old_hash_bytes
                        .try_into()
                        .map_err(|_| anyhow::anyhow!("Invalid hash in tree_refs"))?;
                    drop(rtxn);
                    let _ = self.unpin(&old_hash);
                    // Unindex old tree (will delete orphaned blobs)
                    let _ = self.unindex_tree(&old_hash);
                    tracing::debug!("Replaced old tree for ref {}", key);
                }
            }
        }

        let store = self.store_arc();
        let tree = HashTree::new(HashTreeConfig::new(store).public());

        let plan = sync_block_on(self.collect_tree_index(
            &tree,
            &Cid::public(*root_hash),
            TreeIndexLimits {
                max_nodes: MAX_PINNED_TREE_NODES,
                max_bytes: MAX_UNBOUNDED_PINNED_TREE_BYTES,
            },
        ))?;
        self.write_tree_index(
            root_hash,
            &plan.tracked_hashes,
            plan.total_size,
            owner,
            name,
            priority,
            ref_key,
            false,
        )?;

        tracing::debug!(
            "Indexed tree {} ({} blobs, {} bytes, priority {})",
            &root_hex[..8],
            plan.tracked_hashes.len(),
            plan.total_size,
            priority
        );

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn write_tree_index(
        &self,
        root_hash: &Hash,
        tracked_hashes: &HashSet<Hash>,
        total_size: u64,
        owner: &str,
        name: Option<&str>,
        priority: u8,
        ref_key: Option<&str>,
        pin: bool,
    ) -> Result<bool> {
        let mut wtxn = self.env.write_txn()?;
        let already_pinned = self.pins.get(&wtxn, root_hash.as_slice())?.is_some();

        for tracked_hash in tracked_hashes {
            let mut key = [0u8; 64];
            key[..32].copy_from_slice(tracked_hash);
            key[32..].copy_from_slice(root_hash);
            self.blob_trees.put(&mut wtxn, &key[..], &())?;
        }

        let meta = TreeMeta {
            owner: owner.to_string(),
            name: name.map(str::to_string),
            synced_at: unix_timestamp_now(),
            total_size,
            priority,
        };
        let meta_bytes = rmp_serde::to_vec(&meta)
            .map_err(|error| anyhow::anyhow!("Failed to serialize TreeMeta: {error}"))?;
        self.tree_meta
            .put(&mut wtxn, root_hash.as_slice(), &meta_bytes)?;

        if let Some(key) = ref_key {
            self.tree_refs.put(&mut wtxn, key, root_hash.as_slice())?;
        }
        if pin {
            self.pins.put(&mut wtxn, root_hash.as_slice(), &())?;
        }

        wtxn.commit()?;
        Ok(already_pinned)
    }

    async fn collect_tree_index<S: Store>(
        &self,
        tree: &HashTree<S>,
        root: &Cid,
        limits: TreeIndexLimits,
    ) -> std::result::Result<TreeIndexPlan, PinTreeError> {
        let mut hashes = HashSet::new();
        let mut visited = HashSet::new();
        let mut total_size = 0u64;
        let mut stored_size = 0u64;
        // (cid, count logical bytes, decode/follow tree nodes, require a tree node)
        let mut stack = vec![(root.clone(), true, true, false)];

        while let Some((cid, count_bytes, follow_tree, require_tree)) = stack.pop() {
            let visit_key = (cid.hash, cid.key, follow_tree);
            if !visited.insert(visit_key) {
                continue;
            }
            if visited.len() > limits.max_nodes {
                return Err(PinTreeError::NodeLimitExceeded {
                    max_nodes: limits.max_nodes,
                });
            }

            let size = self
                .router
                .blob_size_sync(&cid.hash)
                .map_err(|error| PinTreeError::Storage(error.to_string()))?
                .ok_or_else(|| {
                    if cid.hash == root.hash {
                        PinTreeError::MissingRoot {
                            hash: to_hex(&cid.hash),
                        }
                    } else {
                        PinTreeError::MissingDescendant {
                            hash: to_hex(&cid.hash),
                        }
                    }
                })?;
            if hashes.insert(cid.hash) {
                stored_size = stored_size
                    .checked_add(size)
                    .filter(|size| *size <= limits.max_bytes)
                    .ok_or(PinTreeError::ByteLimitExceeded {
                        max_bytes: limits.max_bytes,
                    })?;
            }

            if !follow_tree {
                continue;
            }

            let node = tree.get_node(&cid).await.map_err(|error| match error {
                HashTreeError::Store(message) => PinTreeError::Storage(message),
                error => PinTreeError::InvalidDag {
                    hash: to_hex(&cid.hash),
                    message: error.to_string(),
                },
            })?;
            let Some(node) = node else {
                if require_tree {
                    return Err(PinTreeError::InvalidDag {
                        hash: to_hex(&cid.hash),
                        message: "directory link does not contain a tree node".to_string(),
                    });
                }
                if count_bytes {
                    total_size = total_size
                        .checked_add(size)
                        .filter(|size| *size <= limits.max_bytes)
                        .ok_or(PinTreeError::ByteLimitExceeded {
                            max_bytes: limits.max_bytes,
                        })?;
                }
                continue;
            };

            if visited
                .len()
                .saturating_add(stack.len())
                .saturating_add(node.links.len())
                > limits.max_nodes
            {
                return Err(PinTreeError::NodeLimitExceeded {
                    max_nodes: limits.max_nodes,
                });
            }

            for link in &node.links {
                match link.link_type {
                    LinkType::Blob => {
                        if count_bytes {
                            total_size = total_size
                                .checked_add(link.size)
                                .filter(|size| *size <= limits.max_bytes)
                                .ok_or(PinTreeError::ByteLimitExceeded {
                                    max_bytes: limits.max_bytes,
                                })?;
                        }
                        stack.push((link.to_cid(), false, false, false));
                    }
                    LinkType::File => {
                        if count_bytes {
                            total_size = total_size
                                .checked_add(link.size)
                                .filter(|size| *size <= limits.max_bytes)
                                .ok_or(PinTreeError::ByteLimitExceeded {
                                    max_bytes: limits.max_bytes,
                                })?;
                        }
                        stack.push((link.to_cid(), false, true, false));
                    }
                    LinkType::Dir | LinkType::Fanout => {
                        stack.push((link.to_cid(), count_bytes, true, true));
                    }
                }
            }
        }

        Ok(TreeIndexPlan {
            tracked_hashes: hashes,
            total_size,
        })
    }

    /// Unindex a tree - removes blob-tree mappings and deletes orphaned blobs.
    /// Returns the number of bytes freed.
    pub fn unindex_tree(&self, root_hash: &Hash) -> Result<u64> {
        let cleanup = self
            .cache_quota
            .begin_standalone_cleanup()
            .map_err(|denial| anyhow::anyhow!(denial.to_string()))?;
        let retention = self.active_retention_protection()?;
        let mut protected = cleanup.inflight_hashes().clone();
        protected.extend(retention.hashes().iter().copied());
        let result = self.unindex_tree_raw(root_hash, &protected);
        if result.is_ok() {
            let after_usage = self
                .router
                .writable_stats()
                .map_err(|error| {
                    anyhow::anyhow!("Failed to get writable stats after tree unindex: {error}")
                })?
                .total_bytes;
            cleanup.complete(after_usage);
        }
        result
    }

    fn unindex_tree_raw(
        &self,
        root_hash: &Hash,
        additional_protected: &HashSet<Hash>,
    ) -> Result<u64> {
        let root_hex = to_hex(root_hash);

        let store = self.store_arc();
        let tree = HashTree::new(HashTreeConfig::new(store).public());

        // Walk tree and collect all blob hashes
        let tracked_hashes =
            sync_block_on(self.collect_tree_hashes(&tree, &Cid::public(*root_hash), false))?;

        let mut wtxn = self.env.write_txn()?;
        let mut freed = 0u64;

        // For each blob, remove the blob-tree entry and check if orphaned
        for tracked_hash in &tracked_hashes {
            // Delete blob-tree entry (64-byte key: blob_hash ++ tree_hash)
            let mut key = [0u8; 64];
            key[..32].copy_from_slice(tracked_hash);
            key[32..].copy_from_slice(root_hash);
            self.blob_trees.delete(&mut wtxn, &key[..])?;

            // Check if blob is in any other tree (prefix scan on first 32 bytes)
            let mut has_other_tree = false;
            for item in self.blob_trees.prefix_iter(&wtxn, &tracked_hash[..])? {
                if item.is_ok() {
                    has_other_tree = true;
                    break;
                }
            }

            let has_owner = self
                .blob_owners
                .prefix_iter(&wtxn, tracked_hash.as_slice())?
                .next()
                .transpose()?
                .is_some();

            // Tree retention must not delete committed Blossom data or a body
            // whose owner/index transaction is still in flight.
            if !has_other_tree && !has_owner && !additional_protected.contains(tracked_hash) {
                let Some(_delete_guard) = self.cache_quota.begin_retention_delete(*tracked_hash)
                else {
                    continue;
                };
                if let Some(size) = self
                    .router
                    .blob_size_sync(tracked_hash)
                    .map_err(|e| anyhow::anyhow!("Failed to get blob size: {}", e))?
                {
                    freed += size;
                    // Delete locally only - keep S3 as archive
                    self.router
                        .delete_local_only(tracked_hash)
                        .map_err(|e| anyhow::anyhow!("Failed to delete blob: {}", e))?;
                }
            }
        }

        // Delete tree metadata
        self.tree_meta.delete(&mut wtxn, root_hash.as_slice())?;

        wtxn.commit()?;

        tracing::debug!("Unindexed tree {} ({} bytes freed)", &root_hex[..8], freed);

        Ok(freed)
    }

    /// Get tree metadata
    pub fn get_tree_meta(&self, root_hash: &Hash) -> Result<Option<TreeMeta>> {
        let rtxn = self.env.read_txn()?;
        if let Some(bytes) = self.tree_meta.get(&rtxn, root_hash.as_slice())? {
            let meta: TreeMeta = rmp_serde::from_slice(bytes)
                .map_err(|e| anyhow::anyhow!("Failed to deserialize TreeMeta: {}", e))?;
            Ok(Some(meta))
        } else {
            Ok(None)
        }
    }

    pub fn get_tree_ref(&self, key: &str) -> Result<Option<Hash>> {
        let rtxn = self.env.read_txn()?;
        let Some(bytes) = self.tree_refs.get(&rtxn, key)? else {
            return Ok(None);
        };

        let hash: Hash = bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("Invalid hash in tree_refs"))?;
        Ok(Some(hash))
    }

    /// List all indexed trees
    pub fn list_indexed_trees(&self) -> Result<Vec<(Hash, TreeMeta)>> {
        let rtxn = self.env.read_txn()?;
        let mut trees = Vec::new();

        for item in self.tree_meta.iter(&rtxn)? {
            let (hash_bytes, meta_bytes) = item?;
            let hash: Hash = hash_bytes
                .try_into()
                .map_err(|_| anyhow::anyhow!("Invalid hash in tree_meta"))?;
            let meta: TreeMeta = rmp_serde::from_slice(meta_bytes)
                .map_err(|e| anyhow::anyhow!("Failed to deserialize TreeMeta: {}", e))?;
            trees.push((hash, meta));
        }

        Ok(trees)
    }

    /// Get total tracked storage size (sum of all tree_meta.total_size)
    pub fn tracked_size(&self) -> Result<u64> {
        let rtxn = self.env.read_txn()?;
        let mut total = 0u64;

        for item in self.tree_meta.iter(&rtxn)? {
            let (_, bytes) = item?;
            let meta: TreeMeta = rmp_serde::from_slice(bytes)
                .map_err(|e| anyhow::anyhow!("Failed to deserialize TreeMeta: {}", e))?;
            total += meta.total_size;
        }

        Ok(total)
    }

    /// Get evictable trees sorted by (priority ASC, synced_at ASC).
    ///
    /// Blob-level access and raw LRU order live in the storage adapter. Indexed
    /// tree metadata stays cheap and does not try to summarize all descendant
    /// blob access on every stats or eviction pass.
    fn get_evictable_trees(&self) -> Result<Vec<(Hash, TreeMeta)>> {
        let mut trees = self.list_indexed_trees()?;

        // Sort by priority (lower first), then by age.
        trees.sort_by(|a, b| match a.1.priority.cmp(&b.1.priority) {
            std::cmp::Ordering::Equal => a.1.synced_at.cmp(&b.1.synced_at),
            other => other,
        });

        Ok(trees)
    }

    /// Run eviction if storage is over quota
    /// Returns bytes freed
    ///
    /// Eviction order:
    /// 1. Orphaned blobs (not in any indexed tree and not pinned)
    /// 2. Trees by priority (lowest first) and access age (least recent first)
    pub fn evict_if_needed(&self) -> Result<u64> {
        // Get storage used by the canonical writable store.
        let stats = self
            .router
            .writable_stats()
            .map_err(|e| anyhow::anyhow!("Failed to get writable stats: {}", e))?;
        let current = stats.total_bytes;

        if current <= self.max_size_bytes {
            return Ok(0);
        }

        // Target 90% of max to avoid constant eviction
        let target = self.max_size_bytes * 90 / 100;
        self.evict_with_policy_to_target(current, target)
    }

    fn evict_with_policy_to_target(&self, current: u64, target: u64) -> Result<u64> {
        let cleanup = self
            .cache_quota
            .begin_standalone_cleanup()
            .map_err(|denial| anyhow::anyhow!(denial.to_string()))?;
        let mut freed = 0u64;
        let mut current_size = current;

        // Phase 1: Evict orphaned blobs (not in any tree and not pinned)
        if self.evict_orphans {
            let orphan_progress =
                self.evict_disposable_orphans_to_target_raw(target, cleanup.inflight_hashes())?;
            freed += orphan_progress.freed_bytes;
            current_size = current_size.saturating_sub(orphan_progress.freed_bytes);

            if orphan_progress.freed_bytes > 0 {
                tracing::info!(
                    "Evicted orphaned blobs: {} bytes freed",
                    orphan_progress.freed_bytes
                );
            }

            // Do not evict indexed trees merely because the current bounded
            // orphan page was protected. Finish one complete orphan sweep
            // before escalating to durable tree policy.
            if current_size > target && !orphan_progress.sweep_complete {
                let after_usage = self
                    .router
                    .writable_stats()
                    .map_err(|error| {
                        anyhow::anyhow!(
                            "Failed to get writable stats after bounded orphan cleanup: {error}"
                        )
                    })?
                    .total_bytes;
                cleanup.complete(after_usage);
                return Ok(freed);
            }
        } else {
            tracing::debug!("Skipping orphan blob eviction; storage.evict_orphans=false");
        }

        // Check if we're now under target
        if current_size <= target {
            if freed > 0 {
                tracing::info!("Eviction complete: {} bytes freed", freed);
            }
            let after_usage = self
                .router
                .writable_stats()
                .map_err(|error| {
                    anyhow::anyhow!("Failed to get writable stats after retention cleanup: {error}")
                })?
                .total_bytes;
            cleanup.complete(after_usage);
            return Ok(freed);
        }

        // Phase 2: Evict trees by priority (lowest first) and access age (least recent first)
        // Own trees CAN be evicted (just last), but PINNED trees are never evicted
        let retention = self.active_retention_protection()?;
        let retention_protected = retention.hashes();
        let mut additional_protected = cleanup.inflight_hashes().clone();
        additional_protected.extend(retention_protected.iter().copied());
        let evictable = self.get_evictable_trees()?;

        for (root_hash, meta) in evictable {
            if current_size <= target {
                break;
            }

            let root_hex = to_hex(&root_hash);

            // Never evict pinned trees
            if self.is_pinned(&root_hash)? {
                continue;
            }
            if retention_protected.contains(&root_hash) {
                continue;
            }

            let tree_freed = self.unindex_tree_raw(&root_hash, &additional_protected)?;
            freed += tree_freed;
            current_size = current_size.saturating_sub(tree_freed);

            tracing::info!(
                "Evicted tree {} (owner={}, priority={}, {} bytes)",
                &root_hex[..8],
                &meta.owner[..8.min(meta.owner.len())],
                meta.priority,
                tree_freed
            );
        }

        if freed > 0 {
            tracing::info!("Eviction complete: {} bytes freed", freed);
        }

        let after_usage = self
            .router
            .writable_stats()
            .map_err(|error| {
                anyhow::anyhow!("Failed to get writable stats after retention cleanup: {error}")
            })?
            .total_bytes;
        cleanup.complete(after_usage);
        Ok(freed)
    }

    /// Get the maximum storage size in bytes
    pub fn max_size_bytes(&self) -> u64 {
        self.max_size_bytes
    }

    /// Get storage usage by priority tier
    pub fn storage_by_priority(&self) -> Result<StorageByPriority> {
        let rtxn = self.env.read_txn()?;
        let mut own = 0u64;
        let mut followed = 0u64;
        let mut other = 0u64;

        for item in self.tree_meta.iter(&rtxn)? {
            let (_, bytes) = item?;
            let meta: TreeMeta = rmp_serde::from_slice(bytes)
                .map_err(|e| anyhow::anyhow!("Failed to deserialize TreeMeta: {}", e))?;

            if meta.priority == PRIORITY_OWN {
                own += meta.total_size;
            } else if meta.priority >= PRIORITY_FOLLOWED {
                followed += meta.total_size;
            } else {
                other += meta.total_size;
            }
        }

        Ok(StorageByPriority {
            own,
            followed,
            other,
        })
    }

    /// Get storage statistics
    pub fn get_storage_stats(&self) -> Result<StorageStats> {
        let rtxn = self.env.read_txn()?;
        let total_pins = self.pins.len(&rtxn)? as usize;

        let stats = self
            .router
            .stats()
            .map_err(|e| anyhow::anyhow!("Failed to get stats: {}", e))?;

        Ok(StorageStats {
            total_dags: stats.count,
            pinned_dags: total_pins,
            total_bytes: stats.total_bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashtree_config::StorageBackend;
    use hashtree_core::Cid;
    use hashtree_index::{BTree, BTreeOptions};
    use nostr::{EventBuilder, Keys, Kind, Timestamp};
    use std::io::Write;
    use std::path::Path;
    use tempfile::TempDir;

    use crate::storage::PRIORITY_OTHER;

    #[cfg(unix)]
    #[test]
    fn existing_profile_repair_retention_guard_never_creates_its_authority() {
        let temp_dir = TempDir::new().expect("temp dir");
        let lock_path = temp_dir.path().join(RETENTION_ROOTS_LOCK_FILE);

        let error = acquire_existing_profile_repair_retention_guard(temp_dir.path())
            .err()
            .expect("missing authority must fail");
        assert!(error
            .to_string()
            .contains("inspect existing retention-roots lock"));
        assert!(
            !lock_path.exists(),
            "recovery guard created a missing lock authority"
        );

        std::fs::write(&lock_path, b"existing-authority").expect("create existing lock authority");
        let before = std::fs::metadata(&lock_path).expect("inspect existing lock authority");
        let guard = acquire_existing_profile_repair_retention_guard(temp_dir.path())
            .expect("acquire existing lock authority");
        let after = std::fs::metadata(&lock_path).expect("reinspect existing lock authority");
        assert_eq!(before.len(), after.len());
        assert_eq!(
            std::fs::read(&lock_path).expect("read existing lock authority"),
            b"existing-authority"
        );
        drop(guard);
    }

    fn write_root_file(path: &Path, cid: &Cid) {
        #[derive(Serialize)]
        struct StoredCid {
            hash: [u8; 32],
            key: Option<[u8; 32]>,
        }

        std::fs::create_dir_all(path.parent().expect("root file parent")).expect("create dir");
        let bytes = rmp_serde::to_vec_named(&StoredCid {
            hash: cid.hash,
            key: cid.key,
        })
        .expect("encode cid");
        std::fs::write(path, bytes).expect("write root file");
    }

    fn build_test_tree(store: &HashtreeStore) -> Cid {
        let index = BTree::new(store.store_arc(), BTreeOptions { order: Some(8) });
        sync_block_on(index.build(vec![
            ("alpha".to_string(), "one".to_string()),
            ("beta".to_string(), "two".to_string()),
            ("gamma".to_string(), "three".to_string()),
        ]))
        .expect("build btree")
        .expect("non-empty root")
    }

    fn build_deep_test_tree(store: &HashtreeStore) -> Cid {
        let index = BTree::new(store.store_arc(), BTreeOptions { order: Some(4) });
        let entries: Vec<_> = (0..256)
            .map(|index| (format!("key-{index:04}"), format!("value-{index:04}")))
            .collect();
        sync_block_on(index.build(entries))
            .expect("build deep btree")
            .expect("non-empty deep root")
    }

    fn build_generated_encrypted_tree(
        store: &HashtreeStore,
        namespace: &str,
        entry_count: usize,
    ) -> Cid {
        let index = BTree::new(store.store_arc(), BTreeOptions { order: Some(4) });
        let entries: Vec<_> = (0..entry_count)
            .map(|index| {
                (
                    format!("{namespace}-key-{index:04}"),
                    format!("{namespace}-value-{index:04}"),
                )
            })
            .collect();
        let root = sync_block_on(index.build(entries))
            .expect("build generated encrypted btree")
            .expect("non-empty generated encrypted root");
        assert!(
            root.key.is_some(),
            "generated retention coverage must exercise a full encrypted CID"
        );
        root
    }

    fn generated_tree_hashes(store: &HashtreeStore, root: &Cid) -> HashSet<Hash> {
        let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());
        let hashes = sync_block_on(store.collect_tree_hashes(&tree, root, true))
            .expect("collect generated encrypted DAG");
        assert!(hashes.len() > 2, "generated DAG must contain descendants");
        hashes
    }

    fn publish_generated_profile_repair_lease(store: &HashtreeStore, label: &str, root: &Cid) {
        let lease = ProfileRepairRetentionLease {
            format: PROFILE_REPAIR_RETENTION_LEASE_FORMAT.to_string(),
            authority_sha256: to_hex(&hashtree_core::sha256(root.to_string().as_bytes())),
            roots: BTreeMap::from([(label.to_string(), root.to_string())]),
        };
        let lease_bytes = lease.canonical_bytes().expect("canonical generated lease");
        let publication = store
            .acquire_profile_repair_retention_publication_guard()
            .expect("exclusive generated retention publication");
        let lease_path = store.profile_repair_retention_lease_path();
        std::fs::create_dir_all(lease_path.parent().expect("lease parent"))
            .expect("create generated lease parent");
        let mut lease_file = File::create(&lease_path).expect("create generated lease");
        lease_file
            .write_all(&lease_bytes)
            .expect("write generated lease");
        lease_file.sync_all().expect("sync generated lease");
        File::open(lease_path.parent().expect("lease parent"))
            .expect("open generated lease parent")
            .sync_all()
            .expect("sync generated lease parent");
        drop(publication);
    }

    #[cfg(feature = "lmdb")]
    fn bounded_lmdb_store(path: &Path, max_size_bytes: u64) -> HashtreeStore {
        // Seed the legacy single-store path so the shared-layout opener does
        // not create a fresh PoolStore. Bounded orphan deletion is
        // intentionally LMDB-only until PoolStore has a remove-hot-copy API.
        drop(
            super::super::LocalStore::new_unbounded_with_lmdb_map_size(
                path.join("blobs"),
                &StorageBackend::Lmdb,
                Some(16 * 1024 * 1024),
            )
            .expect("seed single LMDB"),
        );
        HashtreeStore::with_options_and_backend(
            path,
            None,
            max_size_bytes,
            true,
            &StorageBackend::Lmdb,
        )
        .expect("LMDB store")
    }

    #[cfg(feature = "lmdb")]
    fn put_ordered_hashes(store: &HashtreeStore, count: u8) -> Vec<Hash> {
        let mut hashes = Vec::new();
        for value in 1..=count {
            let data = [value];
            let hash = hashtree_core::sha256(&data);
            store
                .router
                .put_sync(hash, &data)
                .expect("put ordered test hash");
            hashes.push(hash);
        }
        hashes.sort_unstable();
        hashes
    }

    #[cfg(feature = "lmdb")]
    #[test]
    fn bounded_orphan_sweep_progresses_and_preserves_all_durable_classes() {
        let temp_dir = TempDir::new().expect("temp dir");
        let store = bounded_lmdb_store(temp_dir.path(), 1024 * 1024);
        let hashes = put_ordered_hashes(&store, 7);

        store.pin(&hashes[0]).expect("pin first hash");
        let mut tree_key = [0u8; 64];
        tree_key[..32].copy_from_slice(&hashes[1]);
        tree_key[32..].fill(99);
        let mut wtxn = store.env.write_txn().expect("metadata write");
        store
            .blob_trees
            .put(&mut wtxn, &tree_key, &())
            .expect("index second hash");
        wtxn.commit().expect("commit tree index");
        store
            .set_blob_owner(&hashes[2], &[77; 32])
            .expect("own third hash");
        let socialgraph_root = build_test_tree(&store);
        let socialgraph_hashes = generated_tree_hashes(&store, &socialgraph_root);
        write_root_file(
            &temp_dir.path().join("socialgraph/events-root.msgpack"),
            &socialgraph_root,
        );

        let mut progress = Vec::new();
        loop {
            let page = store
                .evict_disposable_orphans_page(0, &HashSet::new(), 2)
                .expect("bounded orphan page");
            assert!(page.scanned <= 2, "page exceeded its candidate bound");
            progress.push(page);
            if page.sweep_complete {
                break;
            }
            assert!(progress.len() < 50, "bounded sweep did not converge");
        }

        assert_eq!(progress.iter().map(|page| page.freed_bytes).sum::<u64>(), 4);
        assert!(progress.iter().all(|page| page.scanned <= 2));
        for hash in &hashes[..3] {
            assert!(store.blob_exists(hash).expect("protected blob lookup"));
        }
        for hash in &hashes[3..] {
            assert!(!store.blob_exists(hash).expect("orphan blob lookup"));
        }
        for hash in socialgraph_hashes {
            assert!(
                store.blob_exists(&hash).expect("socialgraph blob lookup"),
                "bounded sweep deleted generated socialgraph DAG hash {}",
                to_hex(&hash)
            );
        }
    }

    #[cfg(feature = "lmdb")]
    #[test]
    fn socialgraph_root_change_unions_protection_until_sweep_boundary() {
        let temp_dir = TempDir::new().expect("temp dir");
        let store = bounded_lmdb_store(temp_dir.path(), 1024 * 1024);
        let old_root = build_generated_encrypted_tree(&store, "old-live-root", 24);
        let old_hashes = generated_tree_hashes(&store, &old_root);
        let root_path = temp_dir.path().join("socialgraph/events-root.msgpack");
        write_root_file(&root_path, &old_root);

        let first = store
            .evict_disposable_orphans_page(0, &HashSet::new(), 1)
            .expect("first root page");
        assert_eq!(first.scanned, 1);
        assert_eq!(first.freed_bytes, 0);
        let new_root = build_generated_encrypted_tree(&store, "new-live-root", 24);
        let new_hashes = generated_tree_hashes(&store, &new_root);
        write_root_file(&root_path, &new_root);

        let second = store
            .evict_disposable_orphans_page(0, &HashSet::new(), 1)
            .expect("changed root page");
        assert_eq!(second.scanned, 1);
        assert_eq!(second.freed_bytes, 0);
        {
            let state = store
                .orphan_scan
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(old_hashes
                .iter()
                .all(|hash| state.socialgraph_protected.contains(hash)));
            assert!(new_hashes
                .iter()
                .all(|hash| state.socialgraph_protected.contains(hash)));
        }

        let mut sweep_complete = false;
        for _ in 0..256 {
            let page = store
                .evict_disposable_orphans_page(0, &HashSet::new(), 1)
                .expect("finish unioned socialgraph sweep");
            assert_eq!(page.freed_bytes, 0);
            if page.sweep_complete {
                sweep_complete = true;
                break;
            }
        }
        assert!(sweep_complete, "unioned socialgraph sweep did not converge");
        for hash in old_hashes.iter().chain(new_hashes.iter()) {
            assert!(
                store.blob_exists(hash).expect("unioned root lookup"),
                "active sweep deleted union-protected DAG hash {}",
                to_hex(hash)
            );
        }
    }

    #[cfg(feature = "lmdb")]
    #[test]
    fn immutable_profile_repair_lease_preserves_complete_generated_dag() {
        let temp_dir = TempDir::new().expect("temp dir");
        let store = bounded_lmdb_store(temp_dir.path(), 64 * 1024 * 1024);
        let protected_root = build_deep_test_tree(&store);
        let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());
        let protected_hashes =
            sync_block_on(store.collect_tree_hashes(&tree, &protected_root, true))
                .expect("collect generated repair DAG");
        assert!(protected_hashes.len() > 2);

        let orphan_bytes = b"unleased generated orphan";
        let orphan = hashtree_core::sha256(orphan_bytes);
        store
            .router
            .put_sync(orphan, orphan_bytes)
            .expect("put generated orphan");

        let lease = ProfileRepairRetentionLease {
            format: PROFILE_REPAIR_RETENTION_LEASE_FORMAT.to_string(),
            authority_sha256: "11".repeat(32),
            roots: BTreeMap::from([("profile-search".to_string(), protected_root.to_string())]),
        };
        let lease_bytes = lease.canonical_bytes().expect("canonical lease");
        let publication = store
            .acquire_profile_repair_retention_publication_guard()
            .expect("exclusive retention publication");
        let lease_path = store.profile_repair_retention_lease_path();
        std::fs::create_dir_all(lease_path.parent().expect("lease parent"))
            .expect("create lease parent");
        let mut lease_file = File::create(&lease_path).expect("create lease");
        lease_file.write_all(&lease_bytes).expect("write lease");
        lease_file.sync_all().expect("sync lease");
        File::open(lease_path.parent().expect("lease parent"))
            .expect("open lease parent")
            .sync_all()
            .expect("sync lease parent");
        drop(publication);

        loop {
            let page = store
                .evict_disposable_orphans_page(0, &HashSet::new(), 31)
                .expect("lease-protected orphan sweep");
            if page.sweep_complete {
                break;
            }
        }

        for hash in protected_hashes {
            assert!(
                store.blob_exists(&hash).expect("protected blob lookup"),
                "lease lost generated DAG blob {}",
                to_hex(&hash)
            );
        }
        assert!(!store.blob_exists(&orphan).expect("orphan lookup"));
    }

    #[cfg(feature = "lmdb")]
    #[test]
    fn garbage_collection_preserves_leased_generated_encrypted_dag() {
        let temp_dir = TempDir::new().expect("temp dir");
        let store = bounded_lmdb_store(temp_dir.path(), 64 * 1024 * 1024);
        let leased_root = build_generated_encrypted_tree(&store, "gc-leased", 96);
        let leased_hashes = generated_tree_hashes(&store, &leased_root);
        let current_root = build_generated_encrypted_tree(&store, "gc-current", 96);
        let current_hashes = generated_tree_hashes(&store, &current_root);
        write_root_file(
            &temp_dir.path().join("socialgraph/events-root.msgpack"),
            &current_root,
        );
        let orphan_bytes = format!("gc-unleased-orphan:{}", leased_root);
        let orphan_hash = hashtree_core::sha256(orphan_bytes.as_bytes());
        store
            .router
            .put_sync(orphan_hash, orphan_bytes.as_bytes())
            .expect("put generated GC orphan");
        publish_generated_profile_repair_lease(&store, "gc-leased", &leased_root);

        let report = store.gc().expect("lease-aware garbage collection");

        assert_eq!(report.deleted_dags, 1);
        assert!(!store.blob_exists(&orphan_hash).expect("GC orphan lookup"));
        for hash in leased_hashes.into_iter().chain(current_hashes) {
            assert!(
                store.blob_exists(&hash).expect("leased GC hash lookup"),
                "GC deleted active encrypted DAG hash {}",
                to_hex(&hash)
            );
        }
    }

    #[test]
    fn garbage_collection_preserves_generated_pending_profile_root_pair_until_recovery() {
        let temp_dir = TempDir::new().expect("temp dir");
        let store = HashtreeStore::with_embedded_options(temp_dir.path(), None, 64 * 1024 * 1024)
            .expect("embedded generated store");
        let graph = crate::socialgraph::open_test_social_graph_store_with_storage(
            temp_dir.path(),
            store.store_arc(),
            None,
        )
        .expect("open generated social graph");

        let mut profiles = Vec::new();
        let mut decisions = BTreeMap::new();
        for index in 0..80 {
            let keys = Keys::generate();
            let event = EventBuilder::new(
                Kind::Metadata,
                serde_json::json!({
                    "display_name": format!("pending-profile-{index:04}")
                })
                .to_string(),
            )
            .custom_created_at(Timestamp::from_secs(index + 1))
            .sign_with_keys(&keys)
            .expect("sign generated profile");
            decisions.insert(event.pubkey.to_hex(), Some(1));
            profiles.push(event);
        }
        let expected_profile = profiles[0].clone();
        let prepared = graph
            .build_unpublished_profile_index_repair_with_frozen_distances(&profiles, &decisions)
            .expect("build generated replacement profile roots");
        let mut pending_hashes = HashSet::new();
        for root in [
            prepared
                .new_roots()
                .by_pubkey
                .as_ref()
                .expect("generated by-pubkey root"),
            prepared
                .new_roots()
                .search
                .as_ref()
                .expect("generated search root"),
        ] {
            pending_hashes.extend(generated_tree_hashes(&store, root));
        }

        let crash = graph
            .crash_after_prepared_profile_root_pair_intent(&prepared)
            .expect_err("generated commit must stop after its durable intent");
        assert!(format!("{crash:#}").contains("injected interruption after durable"));
        let commit_path = temp_dir
            .path()
            .join("socialgraph/profile-root-pair.commit.json");
        assert!(commit_path.is_file(), "generated commit intent is missing");
        assert!(
            !temp_dir
                .path()
                .join("socialgraph/profiles-by-pubkey-root.msgpack")
                .exists(),
            "crash injection unexpectedly installed by-pubkey root"
        );
        assert!(
            !temp_dir
                .path()
                .join("socialgraph/profile-search-root.msgpack")
                .exists(),
            "crash injection unexpectedly installed search root"
        );
        drop(graph);

        let orphan_bytes = b"pending-root-pair-unrelated-orphan";
        let orphan_hash = hashtree_core::sha256(orphan_bytes);
        store
            .router
            .put_sync(orphan_hash, orphan_bytes)
            .expect("put generated orphan");

        let report = store.gc().expect("pending-commit-aware garbage collection");
        assert_eq!(report.deleted_dags, 1);
        assert!(!store.blob_exists(&orphan_hash).expect("orphan lookup"));
        for hash in &pending_hashes {
            assert!(
                store.blob_exists(hash).expect("pending DAG lookup"),
                "GC deleted pending root-pair DAG hash {}",
                to_hex(hash)
            );
        }

        let recovered = crate::socialgraph::open_test_social_graph_store_with_storage(
            temp_dir.path(),
            store.store_arc(),
            None,
        )
        .expect("recover generated pending root pair");
        assert!(!commit_path.exists(), "pending commit was not recovered");
        assert_eq!(
            recovered
                .latest_profile_event(&expected_profile.pubkey.to_hex())
                .expect("read recovered profile")
                .expect("recovered profile is missing")
                .id,
            expected_profile.id
        );
    }

    #[cfg(feature = "lmdb")]
    #[test]
    fn applied_root_retention_preserves_leased_generated_encrypted_dag() {
        let temp_dir = TempDir::new().expect("temp dir");
        let store = bounded_lmdb_store(temp_dir.path(), 64 * 1024 * 1024);
        let retained_root = build_generated_encrypted_tree(&store, "retained-target", 96);
        let leased_root = build_generated_encrypted_tree(&store, "retention-leased", 96);
        let leased_hashes = generated_tree_hashes(&store, &leased_root);
        let current_root = build_generated_encrypted_tree(&store, "retention-current", 96);
        let current_hashes = generated_tree_hashes(&store, &current_root);
        write_root_file(
            &temp_dir
                .path()
                .join("socialgraph/profile-search-root.msgpack"),
            &current_root,
        );
        let orphan_bytes = format!("retention-unleased-orphan:{}", leased_root);
        let orphan_hash = hashtree_core::sha256(orphan_bytes.as_bytes());
        store
            .router
            .put_sync(orphan_hash, orphan_bytes.as_bytes())
            .expect("put generated retention orphan");
        publish_generated_profile_repair_lease(&store, "retention-leased", &leased_root);

        let report = store
            .retain_nostr_root(&retained_root, true)
            .expect("lease-aware apply retention");

        assert_eq!(report.candidate_hashes, 1);
        assert_eq!(report.deleted_hashes, 1);
        assert!(!store
            .blob_exists(&orphan_hash)
            .expect("retention orphan lookup"));
        for hash in leased_hashes.into_iter().chain(current_hashes) {
            assert!(
                store
                    .blob_exists(&hash)
                    .expect("leased retention hash lookup"),
                "apply retention deleted active encrypted DAG hash {}",
                to_hex(&hash)
            );
        }
        let leased_index = BTree::new(store.store_arc(), BTreeOptions { order: Some(4) });
        assert_eq!(
            sync_block_on(leased_index.get(Some(&leased_root), "retention-leased-key-0095"))
                .expect("read leased encrypted DAG after retention"),
            Some("retention-leased-value-0095".to_string())
        );
    }

    #[cfg(feature = "lmdb")]
    #[test]
    fn poolstore_orphan_cleanup_fails_closed_without_deleting_catalog_data() {
        let temp_dir = TempDir::new().expect("temp dir");
        let store = HashtreeStore::with_options_and_backend(
            temp_dir.path(),
            None,
            1024 * 1024,
            true,
            &StorageBackend::Lmdb,
        )
        .expect("fresh shared store");
        assert!(matches!(
            store.router.local_store().as_ref(),
            super::super::LocalStore::Pool(_)
        ));
        let data = b"durable pool catalog data";
        let hash = hashtree_core::sha256(data);
        store.router.put_sync(hash, data).expect("put pool blob");

        let error = store
            .evict_disposable_orphans_to_target(0)
            .expect_err("PoolStore orphan deletion must fail closed");
        assert!(
            error.to_string().contains("tier-aware deletion"),
            "unexpected error: {error}"
        );
        assert!(store.blob_exists(&hash).expect("pool blob lookup"));
    }

    #[cfg(feature = "lmdb")]
    #[test]
    fn hash_inserted_before_cursor_is_seen_after_wrap() {
        let temp_dir = TempDir::new().expect("temp dir");
        let store = bounded_lmdb_store(temp_dir.path(), 1024 * 1024);
        let mut entries = (1u8..=8)
            .map(|value| {
                let data = vec![value];
                (hashtree_core::sha256(&data), data)
            })
            .collect::<Vec<_>>();
        entries.sort_unstable_by_key(|(hash, _)| *hash);
        let (behind_cursor_hash, behind_cursor_data) = entries[0].clone();
        for (hash, data) in &entries[1..=3] {
            store
                .router
                .put_sync(*hash, data)
                .expect("put initial cursor fixture");
        }

        let first = store
            .evict_disposable_orphans_page(0, &HashSet::new(), 1)
            .expect("first page");
        assert_eq!(first.scanned, 1);
        assert_eq!(first.freed_bytes, 1);
        store
            .router
            .put_sync(behind_cursor_hash, &behind_cursor_data)
            .expect("insert behind active cursor");

        for _ in 0..8 {
            store
                .evict_disposable_orphans_page(0, &HashSet::new(), 1)
                .expect("continue wrapped sweep");
            if !store
                .blob_exists(&behind_cursor_hash)
                .expect("behind-cursor lookup")
            {
                return;
            }
        }
        panic!("hash inserted behind the active cursor was not seen after wrap");
    }

    #[cfg(feature = "lmdb")]
    #[test]
    fn orphan_cleanup_keeps_indexed_tree_hashes() {
        let temp_dir = TempDir::new().expect("temp dir");
        let store = bounded_lmdb_store(temp_dir.path(), 1024);
        let cid = build_test_tree(&store);

        store
            .index_tree(
                &cid.hash,
                "owner",
                Some("tree"),
                PRIORITY_OTHER,
                Some("owner/tree"),
            )
            .expect("index tree");
        let freed = store
            .evict_disposable_orphans_to_target(0)
            .expect("orphan cleanup");

        assert!(freed < 1024);
        assert!(store.blob_exists(&cid.hash).expect("root exists"));
    }

    #[test]
    fn list_pins_with_names_uses_indexed_tree_metadata() {
        let temp_dir = TempDir::new().expect("temp dir");
        let store = HashtreeStore::with_options(temp_dir.path(), None, 1024 * 1024).expect("store");
        let cid = build_test_tree(&store);

        store.pin(&cid.hash).expect("pin tree");
        store
            .index_tree(
                &cid.hash,
                "npub1example",
                Some("playlist"),
                PRIORITY_OTHER,
                Some("npub1example/playlist"),
            )
            .expect("index tree");

        let pins = store.list_pins_with_names().expect("list pins");

        assert_eq!(pins.len(), 1);
        assert_eq!(pins[0].name, "npub1example/playlist");
        assert!(pins[0].size_bytes > 0);
    }

    #[test]
    fn index_tree_records_multilevel_file_size_from_links() {
        let temp_dir = TempDir::new().expect("temp dir");
        let store = HashtreeStore::with_options(temp_dir.path(), None, 1024 * 1024).expect("store");
        let tree = HashTree::new(
            HashTreeConfig::new(store.store_arc())
                .public()
                .with_chunk_size(4)
                .with_max_links(2),
        );
        let data = (0u8..31).collect::<Vec<_>>();
        let (cid, size) = sync_block_on(tree.put(&data)).expect("put file");

        store
            .index_tree(
                &cid.hash,
                "npub1example",
                Some("large-file"),
                PRIORITY_OTHER,
                Some("npub1example/large-file"),
            )
            .expect("index tree");

        let meta = store
            .get_tree_meta(&cid.hash)
            .expect("tree meta")
            .expect("indexed meta");
        assert_eq!(size, data.len() as u64);
        assert_eq!(meta.total_size, data.len() as u64);
    }

    #[test]
    fn get_tree_ref_returns_stored_root() {
        let temp_dir = TempDir::new().expect("temp dir");
        let store = HashtreeStore::with_options(temp_dir.path(), None, 1024 * 1024).expect("store");
        let cid = build_test_tree(&store);

        store
            .index_tree(
                &cid.hash,
                "npub1example",
                Some("playlist"),
                PRIORITY_OTHER,
                Some("npub1example/playlist"),
            )
            .expect("index tree");

        assert_eq!(
            store
                .get_tree_ref("npub1example/playlist")
                .expect("tree ref lookup"),
            Some(cid.hash)
        );
    }

    #[test]
    fn tree_meta_deserializes_metadata_without_tree_access_field() {
        #[derive(Serialize)]
        struct LegacyTreeMeta {
            owner: String,
            name: Option<String>,
            synced_at: u64,
            total_size: u64,
            priority: u8,
        }

        let bytes = rmp_serde::to_vec(&LegacyTreeMeta {
            owner: "owner".to_string(),
            name: Some("tree".to_string()),
            synced_at: 123,
            total_size: 456,
            priority: PRIORITY_OTHER,
        })
        .expect("serialize legacy metadata");
        let meta: TreeMeta = rmp_serde::from_slice(&bytes).expect("deserialize tree metadata");

        assert_eq!(meta.owner, "owner");
        assert_eq!(meta.name.as_deref(), Some("tree"));
        assert_eq!(meta.synced_at, 123);
        assert_eq!(meta.total_size, 456);
        assert_eq!(meta.priority, PRIORITY_OTHER);
    }

    #[test]
    fn tree_meta_deserializes_accidental_access_field_but_drops_it_on_write() {
        #[derive(Serialize)]
        struct AccidentalTreeMeta {
            owner: String,
            name: Option<String>,
            synced_at: u64,
            last_accessed_at: u64,
            total_size: u64,
            priority: u8,
        }

        let bytes = rmp_serde::to_vec(&AccidentalTreeMeta {
            owner: "owner".to_string(),
            name: Some("tree".to_string()),
            synced_at: 123,
            last_accessed_at: 999,
            total_size: 456,
            priority: PRIORITY_OTHER,
        })
        .expect("serialize accidental metadata");
        let meta: TreeMeta = rmp_serde::from_slice(&bytes).expect("deserialize tree metadata");
        let encoded = rmp_serde::to_vec(&meta).expect("serialize current metadata");
        let reparsed: (String, Option<String>, u64, u64, u8) =
            rmp_serde::from_slice(&encoded).expect("parse current metadata shape");

        assert_eq!(meta.owner, "owner");
        assert_eq!(meta.name.as_deref(), Some("tree"));
        assert_eq!(meta.synced_at, 123);
        assert_eq!(meta.total_size, 456);
        assert_eq!(meta.priority, PRIORITY_OTHER);
        assert_eq!(reparsed.0, "owner");
        assert_eq!(reparsed.3, 456);
        assert_eq!(reparsed.4, PRIORITY_OTHER);
    }

    #[cfg(feature = "lmdb")]
    #[test]
    fn eviction_prefers_oldest_tree_within_priority() {
        let temp_dir = TempDir::new().expect("temp dir");
        let store = bounded_lmdb_store(temp_dir.path(), 500);

        let hash1 = hashtree_core::sha256(&[1u8; 200]);
        let hash2 = hashtree_core::sha256(&[2u8; 200]);
        let hash3 = hashtree_core::sha256(&[3u8; 200]);
        store.put_blob(&[1u8; 200]).expect("put blob 1");
        store.put_blob(&[2u8; 200]).expect("put blob 2");
        store.put_blob(&[3u8; 200]).expect("put blob 3");
        store
            .index_tree(&hash1, "owner1", Some("tree1"), PRIORITY_OTHER, None)
            .expect("index tree 1");
        store
            .index_tree(&hash2, "owner2", Some("tree2"), PRIORITY_OTHER, None)
            .expect("index tree 2");
        store
            .index_tree(&hash3, "owner3", Some("tree3"), PRIORITY_OTHER, None)
            .expect("index tree 3");

        let freed = store.evict_if_needed().expect("evict");

        assert!(freed > 0);
        assert!(
            store.get_tree_meta(&hash3).expect("tree meta").is_some(),
            "newest tree should survive before older peers at the same priority"
        );
    }

    #[cfg(feature = "lmdb")]
    #[test]
    fn orphan_cleanup_keeps_socialgraph_root_hashes() {
        let temp_dir = TempDir::new().expect("temp dir");
        let store = bounded_lmdb_store(temp_dir.path(), 1024);
        let cid = build_test_tree(&store);
        write_root_file(
            &temp_dir.path().join("socialgraph/events-root.msgpack"),
            &cid,
        );

        let freed = store
            .evict_disposable_orphans_to_target(0)
            .expect("orphan cleanup");

        assert!(freed < 1024);
        assert!(store.blob_exists(&cid.hash).expect("root exists"));
    }

    #[test]
    fn retained_nostr_root_cleanup_is_dry_run_first_and_keeps_the_dag() {
        let temp_dir = TempDir::new().expect("temp dir");
        let store = HashtreeStore::with_options(temp_dir.path(), None, 1024 * 1024).expect("store");
        let root = build_deep_test_tree(&store);
        let orphan_bytes = b"unreachable historical index node";
        let orphan = hashtree_core::sha256(orphan_bytes);
        store.put_blob(orphan_bytes).expect("put orphan");

        let dry_run = store
            .retain_nostr_root(&root, false)
            .expect("retention dry run");
        assert_eq!(dry_run.deleted_hashes, 0);
        assert_eq!(dry_run.candidate_hashes, 1);
        assert_eq!(dry_run.total_hashes, dry_run.reachable_hashes + 1);
        assert!(dry_run.reachable_hashes > 256);
        assert!(store.blob_exists(&orphan).expect("orphan exists"));

        let applied = store
            .retain_nostr_root(&root, true)
            .expect("apply retention");
        assert_eq!(applied.deleted_hashes, applied.candidate_hashes);
        assert!(!store.blob_exists(&orphan).expect("orphan deleted"));
        let index = BTree::new(store.store_arc(), BTreeOptions { order: Some(8) });
        assert_eq!(
            sync_block_on(index.get(Some(&root), "key-0255")).expect("read retained index"),
            Some("value-0255".to_string())
        );
    }
}
