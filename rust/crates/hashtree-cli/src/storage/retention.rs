use anyhow::Result;
use futures::executor::block_on as sync_block_on;
use hashtree_core::store::Store;
use hashtree_core::{to_hex, types::Hash, HashTree, HashTreeConfig};
use serde::de::{self, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::{
    BlobMetadata, HashtreeStore, ACCESS_UPDATE_INTERVAL_SECS, PRIORITY_FOLLOWED, PRIORITY_OWN,
};

/// Metadata for a synced tree (for eviction tracking)
#[derive(Debug, Clone, Serialize)]
pub struct TreeMeta {
    /// Pubkey of tree owner
    pub owner: String,
    /// Tree name if known (from nostr key like "npub.../name")
    pub name: Option<String>,
    /// Unix timestamp when this tree was synced
    pub synced_at: u64,
    /// Unix timestamp when this tree root was last read through a user-facing path
    pub last_accessed_at: u64,
    /// Total size of all blobs in this tree
    pub total_size: u64,
    /// Eviction priority: 255=own/pinned, 128=followed, 64=other
    pub priority: u8,
}

impl TreeMeta {
    pub fn effective_last_accessed_at(&self) -> u64 {
        self.last_accessed_at.max(self.synced_at)
    }
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
                let legacy = matches!(seq.size_hint(), Some(5));
                let owner = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(0, &self))?;
                let name = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(1, &self))?;
                let synced_at = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(2, &self))?;

                if legacy {
                    let total_size = seq
                        .next_element()?
                        .ok_or_else(|| de::Error::invalid_length(3, &self))?;
                    let priority = seq
                        .next_element()?
                        .ok_or_else(|| de::Error::invalid_length(4, &self))?;
                    return Ok(TreeMeta {
                        owner,
                        name,
                        synced_at,
                        last_accessed_at: 0,
                        total_size,
                        priority,
                    });
                }

                let last_accessed_at = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(3, &self))?;
                let total_size = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(4, &self))?;
                let priority = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(5, &self))?;

                Ok(TreeMeta {
                    owner,
                    name,
                    synced_at,
                    last_accessed_at,
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
                let mut last_accessed_at = None;
                let mut total_size = None;
                let mut priority = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "owner" => owner = Some(map.next_value()?),
                        "name" => name = Some(map.next_value()?),
                        "synced_at" => synced_at = Some(map.next_value()?),
                        "last_accessed_at" => last_accessed_at = Some(map.next_value()?),
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
                    last_accessed_at: last_accessed_at.unwrap_or(0),
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
    fn socialgraph_root_files(&self) -> [PathBuf; 4] {
        let socialgraph = self.base_path().join("socialgraph");
        [
            socialgraph.join("events-root.msgpack"),
            socialgraph.join("events-root-ambient.msgpack"),
            socialgraph.join("profile-search-root.msgpack"),
            socialgraph.join("profiles-by-pubkey-root.msgpack"),
        ]
    }

    fn read_stored_cid(path: &Path) -> Result<Option<Hash>> {
        #[derive(Deserialize)]
        struct StoredCid {
            hash: [u8; 32],
            #[allow(dead_code)]
            key: Option<[u8; 32]>,
        }

        let Ok(bytes) = std::fs::read(path) else {
            return Ok(None);
        };
        let stored: StoredCid = rmp_serde::from_slice(&bytes)
            .map_err(|e| anyhow::anyhow!("Failed to decode root file {}: {}", path.display(), e))?;
        Ok(Some(stored.hash))
    }

    async fn collect_tree_hashes<S: Store>(
        &self,
        tree: &HashTree<S>,
        root: &Hash,
    ) -> Result<HashSet<Hash>> {
        let mut hashes = HashSet::new();
        let mut stack = vec![*root];

        while let Some(hash) = stack.pop() {
            if !hashes.insert(hash) {
                continue;
            }

            let is_tree = tree
                .is_tree(&hash)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to check tree: {}", e))?;

            if !is_tree {
                continue;
            }

            if let Some(node) = tree
                .get_tree_node(&hash)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to get tree node: {}", e))?
            {
                for link in &node.links {
                    stack.push(link.hash);
                }
            }
        }

        Ok(hashes)
    }

    fn protected_hashes(&self) -> Result<HashSet<Hash>> {
        let mut protected = HashSet::new();

        let rtxn = self.env.read_txn()?;
        for (key_bytes, _) in self.blob_trees.iter(&rtxn)?.flatten() {
            if key_bytes.len() >= 32 {
                let hash: Hash = key_bytes[..32].try_into().unwrap();
                protected.insert(hash);
            }
        }
        drop(rtxn);

        let tree = HashTree::new(HashTreeConfig::new(self.store_arc()).public());
        for path in self.socialgraph_root_files() {
            let Some(root_hash) = Self::read_stored_cid(&path)? else {
                continue;
            };
            protected.extend(sync_block_on(self.collect_tree_hashes(&tree, &root_hash))?);
        }

        Ok(protected)
    }

    fn evict_disposable_orphans_to_target(&self, target_bytes: u64) -> Result<u64> {
        let stats = self
            .router
            .stats()
            .map_err(|e| anyhow::anyhow!("Failed to get stats: {}", e))?;
        let mut current_size = stats.total_bytes;
        if current_size <= target_bytes {
            return Ok(0);
        }

        let rtxn = self.env.read_txn()?;
        let pinned: HashSet<Hash> = self
            .pins
            .iter(&rtxn)?
            .filter_map(|item| item.ok())
            .filter_map(|(hash_bytes, _)| {
                if hash_bytes.len() == 32 {
                    let mut hash = [0u8; 32];
                    hash.copy_from_slice(hash_bytes);
                    Some(hash)
                } else {
                    None
                }
            })
            .collect();
        drop(rtxn);

        let protected_hashes = self.protected_hashes()?;
        let all_hashes = self
            .router
            .list()
            .map_err(|e| anyhow::anyhow!("Failed to list hashes: {}", e))?;

        let mut freed = 0u64;
        for hash in all_hashes {
            if current_size <= target_bytes {
                break;
            }

            if pinned.contains(&hash) || protected_hashes.contains(&hash) {
                continue;
            }

            if self.blob_has_owners(&hash)? {
                continue;
            }

            let Some(data) = self
                .router
                .get_sync(&hash)
                .map_err(|e| anyhow::anyhow!("Failed to get blob: {}", e))?
            else {
                continue;
            };

            let size = data.len() as u64;
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

        Ok(freed)
    }

    pub fn make_room_for_cached_blob(&self, incoming_bytes: u64) -> Result<u64> {
        if self.max_size_bytes == 0 {
            return Ok(0);
        }

        let stats = self
            .router
            .stats()
            .map_err(|e| anyhow::anyhow!("Failed to get stats: {}", e))?;
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
            .stats()
            .map_err(|e| anyhow::anyhow!("Failed to get stats: {}", e))?;
        if stats.total_bytes.saturating_add(incoming_bytes) <= self.max_size_bytes {
            return Ok(0);
        }

        let target = (self.max_size_bytes.saturating_mul(9) / 10)
            .min(self.max_size_bytes.saturating_sub(incoming_bytes));
        let freed = self.evict_with_policy_to_target(stats.total_bytes, target)?;

        let next_stats = self
            .router
            .stats()
            .map_err(|e| anyhow::anyhow!("Failed to get stats after eviction: {}", e))?;
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

    pub fn relieve_cached_blob_write_pressure(&self, incoming_bytes: u64) -> Result<u64> {
        let stats = self
            .router
            .stats()
            .map_err(|e| anyhow::anyhow!("Failed to get stats: {}", e))?;
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
                    .get_sync(&hash)
                    .map_err(|e| anyhow::anyhow!("Failed to get pinned blob: {}", e))?
                    .map(|data| data.len() as u64)
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

        owners.sort_by(|a, b| a.owner.cmp(&b.owner));
        Ok(owners)
    }

    // === Tree indexing for eviction ===

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

        // Walk tree and collect all blob hashes + compute total size
        let (_blob_hashes, total_size) =
            sync_block_on(async { self.collect_tree_blobs(&tree, root_hash).await })?;
        let tracked_hashes = sync_block_on(self.collect_tree_hashes(&tree, root_hash))?;

        let mut wtxn = self.env.write_txn()?;

        // Store blob-tree relationships (64-byte key: blob_hash ++ tree_hash)
        for tracked_hash in &tracked_hashes {
            let mut key = [0u8; 64];
            key[..32].copy_from_slice(tracked_hash);
            key[32..].copy_from_slice(root_hash);
            self.blob_trees.put(&mut wtxn, &key[..], &())?;
        }

        // Store tree metadata
        let now = unix_timestamp_now();
        let meta = TreeMeta {
            owner: owner.to_string(),
            name: name.map(|s| s.to_string()),
            synced_at: now,
            last_accessed_at: now,
            total_size,
            priority,
        };
        let meta_bytes = rmp_serde::to_vec(&meta)
            .map_err(|e| anyhow::anyhow!("Failed to serialize TreeMeta: {}", e))?;
        self.tree_meta
            .put(&mut wtxn, root_hash.as_slice(), &meta_bytes)?;

        // Store ref -> hash mapping if ref_key provided
        if let Some(key) = ref_key {
            self.tree_refs.put(&mut wtxn, key, root_hash.as_slice())?;
        }

        wtxn.commit()?;

        tracing::debug!(
            "Indexed tree {} ({} blobs, {} bytes, priority {})",
            &root_hex[..8],
            tracked_hashes.len(),
            total_size,
            priority
        );

        Ok(())
    }

    /// Collect all blob hashes in a tree and compute total size
    async fn collect_tree_blobs<S: Store>(
        &self,
        tree: &HashTree<S>,
        root: &Hash,
    ) -> Result<(Vec<Hash>, u64)> {
        let mut blobs = Vec::new();
        let mut total_size = 0u64;
        let mut stack = vec![*root];

        while let Some(hash) = stack.pop() {
            // Check if it's a tree node
            let is_tree = tree
                .is_tree(&hash)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to check tree: {}", e))?;

            if is_tree {
                // Get tree node and add children to stack
                if let Some(node) = tree
                    .get_tree_node(&hash)
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to get tree node: {}", e))?
                {
                    for link in &node.links {
                        stack.push(link.hash);
                    }
                }
            } else {
                // It's a blob - get its size
                if let Some(data) = self
                    .router
                    .get_sync(&hash)
                    .map_err(|e| anyhow::anyhow!("Failed to get blob: {}", e))?
                {
                    total_size += data.len() as u64;
                    blobs.push(hash);
                }
            }
        }

        Ok((blobs, total_size))
    }

    /// Unindex a tree - removes blob-tree mappings and deletes orphaned blobs
    /// Returns the number of bytes freed
    pub fn unindex_tree(&self, root_hash: &Hash) -> Result<u64> {
        let root_hex = to_hex(root_hash);

        let store = self.store_arc();
        let tree = HashTree::new(HashTreeConfig::new(store).public());

        // Walk tree and collect all blob hashes
        let tracked_hashes = sync_block_on(self.collect_tree_hashes(&tree, root_hash))?;

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

            // If orphaned, delete the blob
            if !has_other_tree {
                if let Some(data) = self
                    .router
                    .get_sync(tracked_hash)
                    .map_err(|e| anyhow::anyhow!("Failed to get blob: {}", e))?
                {
                    freed += data.len() as u64;
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

    pub(super) fn note_tree_access(&self, root_hash: &Hash) {
        if let Err(err) = self.touch_tree_accessed_at(root_hash, unix_timestamp_now()) {
            tracing::debug!(
                "Failed to update tree access metadata for {}: {}",
                &to_hex(root_hash)[..8],
                err
            );
        }
    }

    fn touch_tree_accessed_at(&self, root_hash: &Hash, now: u64) -> Result<bool> {
        if !self.access_update_gate.should_update(root_hash, now) {
            return Ok(false);
        }

        let mut wtxn = self.env.write_txn()?;
        let Some(bytes) = self.tree_meta.get(&wtxn, root_hash.as_slice())? else {
            return Ok(false);
        };
        let mut meta: TreeMeta = rmp_serde::from_slice(bytes)
            .map_err(|e| anyhow::anyhow!("Failed to deserialize TreeMeta: {}", e))?;

        if now.saturating_sub(meta.effective_last_accessed_at()) < ACCESS_UPDATE_INTERVAL_SECS {
            return Ok(false);
        }

        meta.last_accessed_at = now;
        let meta_bytes = rmp_serde::to_vec(&meta)
            .map_err(|e| anyhow::anyhow!("Failed to serialize TreeMeta: {}", e))?;
        self.tree_meta
            .put(&mut wtxn, root_hash.as_slice(), &meta_bytes)?;
        wtxn.commit()?;
        Ok(true)
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

    /// Get evictable trees sorted by (priority ASC, last_accessed_at ASC)
    fn get_evictable_trees(&self) -> Result<Vec<(Hash, TreeMeta)>> {
        let mut trees = self.list_indexed_trees()?;

        // Sort by priority (lower first), then by access time (least recently used first).
        trees.sort_by(|a, b| match a.1.priority.cmp(&b.1.priority) {
            std::cmp::Ordering::Equal => {
                a.1.effective_last_accessed_at()
                    .cmp(&b.1.effective_last_accessed_at())
                    .then_with(|| a.1.synced_at.cmp(&b.1.synced_at))
            }
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
        // Get actual storage used
        let stats = self
            .router
            .stats()
            .map_err(|e| anyhow::anyhow!("Failed to get stats: {}", e))?;
        let current = stats.total_bytes;

        if current <= self.max_size_bytes {
            return Ok(0);
        }

        // Target 90% of max to avoid constant eviction
        let target = self.max_size_bytes * 90 / 100;
        self.evict_with_policy_to_target(current, target)
    }

    fn evict_with_policy_to_target(&self, current: u64, target: u64) -> Result<u64> {
        let mut freed = 0u64;
        let mut current_size = current;

        // Phase 1: Evict orphaned blobs (not in any tree and not pinned)
        if self.evict_orphans {
            let orphan_freed = self.evict_disposable_orphans_to_target(target)?;
            freed += orphan_freed;
            current_size = current_size.saturating_sub(orphan_freed);

            if orphan_freed > 0 {
                tracing::info!("Evicted orphaned blobs: {} bytes freed", orphan_freed);
            }
        } else {
            tracing::debug!("Skipping orphan blob eviction; storage.evict_orphans=false");
        }

        // Check if we're now under target
        if current_size <= target {
            if freed > 0 {
                tracing::info!("Eviction complete: {} bytes freed", freed);
            }
            return Ok(freed);
        }

        // Phase 2: Evict trees by priority (lowest first) and access age (least recent first)
        // Own trees CAN be evicted (just last), but PINNED trees are never evicted
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

            let tree_freed = self.unindex_tree(&root_hash)?;
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
    use hashtree_core::Cid;
    use hashtree_index::{BTree, BTreeOptions};
    use tempfile::TempDir;

    use crate::storage::PRIORITY_OTHER;

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

    #[test]
    fn orphan_cleanup_keeps_indexed_tree_hashes() {
        let temp_dir = TempDir::new().expect("temp dir");
        let store = HashtreeStore::with_options(temp_dir.path(), None, 1024).expect("store");
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
    fn tree_meta_deserializes_legacy_metadata_without_access_field() {
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
        assert_eq!(meta.last_accessed_at, 0);
        assert_eq!(meta.effective_last_accessed_at(), 123);
        assert_eq!(meta.total_size, 456);
        assert_eq!(meta.priority, PRIORITY_OTHER);
    }

    #[test]
    fn tree_access_updates_are_throttled() {
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

        let original = store
            .get_tree_meta(&cid.hash)
            .expect("tree meta")
            .expect("indexed tree");
        let first_touch = original
            .effective_last_accessed_at()
            .saturating_add(super::ACCESS_UPDATE_INTERVAL_SECS + 1);
        assert!(
            store
                .touch_tree_accessed_at(&cid.hash, first_touch)
                .expect("touch tree"),
            "first due touch should update metadata"
        );
        assert_eq!(
            store
                .get_tree_meta(&cid.hash)
                .expect("tree meta")
                .expect("indexed tree")
                .last_accessed_at,
            first_touch
        );

        assert!(
            !store
                .touch_tree_accessed_at(&cid.hash, first_touch + 1)
                .expect("touch tree"),
            "immediate repeated touch should be skipped"
        );
        assert_eq!(
            store
                .get_tree_meta(&cid.hash)
                .expect("tree meta")
                .expect("indexed tree")
                .last_accessed_at,
            first_touch
        );
    }

    #[test]
    fn eviction_prefers_least_recently_accessed_tree_within_priority() {
        let temp_dir = TempDir::new().expect("temp dir");
        let store = HashtreeStore::with_options(temp_dir.path(), None, 500).expect("store");

        let hash1 = hashtree_core::sha256(&vec![1u8; 200]);
        let hash2 = hashtree_core::sha256(&vec![2u8; 200]);
        let hash3 = hashtree_core::sha256(&vec![3u8; 200]);
        store.put_blob(&vec![1u8; 200]).expect("put blob 1");
        store.put_blob(&vec![2u8; 200]).expect("put blob 2");
        store.put_blob(&vec![3u8; 200]).expect("put blob 3");
        store
            .index_tree(&hash1, "owner1", Some("tree1"), PRIORITY_OTHER, None)
            .expect("index tree 1");
        store
            .index_tree(&hash2, "owner2", Some("tree2"), PRIORITY_OTHER, None)
            .expect("index tree 2");
        store
            .index_tree(&hash3, "owner3", Some("tree3"), PRIORITY_OTHER, None)
            .expect("index tree 3");

        let meta1 = store
            .get_tree_meta(&hash1)
            .expect("tree meta")
            .expect("indexed tree");
        store
            .touch_tree_accessed_at(
                &hash1,
                meta1
                    .effective_last_accessed_at()
                    .saturating_add(super::ACCESS_UPDATE_INTERVAL_SECS + 1),
            )
            .expect("touch tree");

        let freed = store.evict_if_needed().expect("evict");

        assert!(freed > 0);
        assert!(
            store.get_tree_meta(&hash1).expect("tree meta").is_some(),
            "recently accessed tree should survive before older peers at the same priority"
        );
    }

    #[test]
    fn orphan_cleanup_keeps_socialgraph_root_hashes() {
        let temp_dir = TempDir::new().expect("temp dir");
        let store = HashtreeStore::with_options(temp_dir.path(), None, 1024).expect("store");
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
}
