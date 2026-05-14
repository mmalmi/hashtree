use anyhow::Result;
use hashtree_cli::config::parse_npub;
use hashtree_cli::socialgraph::{open_social_graph_store, SocialGraphBackend, SocialGraphStore};
use hashtree_cli::storage::PinnedItem;
use hashtree_cli::{HashtreeStore, TreeMeta, PRIORITY_FOLLOWED, PRIORITY_OWN};
use hashtree_core::{from_hex, to_hex, types::Hash};
use std::collections::{HashMap, HashSet};
use std::path::Path;

use super::util::format_bytes;

const PIN_DETAIL_LIMIT: usize = 20;
const TREE_DETAIL_LIMIT: usize = 5;
const AUTHOR_DETAIL_LIMIT: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StorageBucket {
    Mine,
    Followed,
    SocialGraph,
    Other,
}

impl StorageBucket {
    fn label(self) -> &'static str {
        match self {
            StorageBucket::Mine => "My stuff",
            StorageBucket::Followed => "Followed users' stuff",
            StorageBucket::SocialGraph => "Social graph people's stuff",
            StorageBucket::Other => "Other/unknown stuff",
        }
    }

    fn all() -> [StorageBucket; 4] {
        [
            StorageBucket::Mine,
            StorageBucket::Followed,
            StorageBucket::SocialGraph,
            StorageBucket::Other,
        ]
    }

    fn index(self) -> usize {
        match self {
            StorageBucket::Mine => 0,
            StorageBucket::Followed => 1,
            StorageBucket::SocialGraph => 2,
            StorageBucket::Other => 3,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PinnedDetail {
    pub name: String,
    pub cid: String,
    pub is_directory: bool,
    pub size_bytes: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct TreeDetail {
    pub name: String,
    pub owner: String,
    pub root: String,
    pub size_bytes: u64,
    pub pinned: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct AuthorSummary {
    pub key: String,
    pub label: String,
    pub indexed_tree_count: usize,
    pub indexed_tree_bytes: u64,
    pub pinned_tree_count: usize,
    pub owned_blob_count: usize,
    pub owned_blob_bytes: u64,
}

impl AuthorSummary {
    fn new(key: String, label: String) -> Self {
        Self {
            key,
            label,
            indexed_tree_count: 0,
            indexed_tree_bytes: 0,
            pinned_tree_count: 0,
            owned_blob_count: 0,
            owned_blob_bytes: 0,
        }
    }

    fn known_bytes(&self) -> u64 {
        self.indexed_tree_bytes
            .saturating_add(self.owned_blob_bytes)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct StorageBucketSummary {
    pub bucket: StorageBucket,
    pub indexed_tree_count: usize,
    pub indexed_tree_bytes: u64,
    pub owned_blob_count: usize,
    pub owned_blob_bytes: u64,
    pub pinned_unindexed_count: usize,
    pub pinned_unindexed_bytes: u64,
    pub pinned_items: Vec<PinnedDetail>,
    pub trees: Vec<TreeDetail>,
    pub authors: Vec<AuthorSummary>,
}

impl StorageBucketSummary {
    fn new(bucket: StorageBucket) -> Self {
        Self {
            bucket,
            indexed_tree_count: 0,
            indexed_tree_bytes: 0,
            owned_blob_count: 0,
            owned_blob_bytes: 0,
            pinned_unindexed_count: 0,
            pinned_unindexed_bytes: 0,
            pinned_items: Vec::new(),
            trees: Vec::new(),
            authors: Vec::new(),
        }
    }

    fn author_mut(&mut self, key: &str, label: String) -> &mut AuthorSummary {
        if let Some(index) = self.authors.iter().position(|author| author.key == key) {
            return &mut self.authors[index];
        }

        self.authors
            .push(AuthorSummary::new(key.to_string(), label));
        self.authors.last_mut().expect("just pushed author")
    }

    fn known_bytes(&self) -> u64 {
        self.indexed_tree_bytes
            .saturating_add(self.owned_blob_bytes)
            .saturating_add(self.pinned_unindexed_bytes)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct StorageInventory {
    pub buckets: Vec<StorageBucketSummary>,
}

impl StorageInventory {
    pub(crate) fn new() -> Self {
        Self {
            buckets: StorageBucket::all()
                .into_iter()
                .map(StorageBucketSummary::new)
                .collect(),
        }
    }

    fn bucket_mut(&mut self, bucket: StorageBucket) -> &mut StorageBucketSummary {
        &mut self.buckets[bucket.index()]
    }

    fn indexed_tree_count(&self) -> usize {
        self.buckets
            .iter()
            .map(|bucket| bucket.indexed_tree_count)
            .sum()
    }

    fn indexed_tree_bytes(&self) -> u64 {
        self.buckets
            .iter()
            .map(|bucket| bucket.indexed_tree_bytes)
            .sum()
    }

    fn owned_blob_count(&self) -> usize {
        self.buckets
            .iter()
            .map(|bucket| bucket.owned_blob_count)
            .sum()
    }

    fn owned_blob_bytes(&self) -> u64 {
        self.buckets
            .iter()
            .map(|bucket| bucket.owned_blob_bytes)
            .sum()
    }
}

pub(crate) fn classify_storage_bucket(priority: u8, follow_distance: Option<u32>) -> StorageBucket {
    if let Some(distance) = follow_distance {
        return match distance {
            0 => StorageBucket::Mine,
            1 => StorageBucket::Followed,
            _ => StorageBucket::SocialGraph,
        };
    }

    if priority == PRIORITY_OWN {
        StorageBucket::Mine
    } else if priority >= PRIORITY_FOLLOWED {
        StorageBucket::Followed
    } else {
        StorageBucket::Other
    }
}

pub(crate) fn collect_storage_inventory(
    store: &HashtreeStore,
    data_dir: &Path,
) -> Result<StorageInventory> {
    let social_graph = open_stats_social_graph(data_dir);
    collect_storage_inventory_with_graph(store, social_graph.as_deref())
}

fn collect_storage_inventory_with_graph(
    store: &HashtreeStore,
    social_graph: Option<&SocialGraphStore>,
) -> Result<StorageInventory> {
    let mut inventory = StorageInventory::new();
    let trees = store.list_indexed_trees()?;
    let pins = store.list_pins_with_names()?;
    let pinned_hashes = pin_hashes(&pins);
    let mut tree_buckets = HashMap::<Hash, StorageBucket>::new();
    let mut author_labels = AuthorLabelCache::new(social_graph);

    for (root, meta) in trees {
        let bucket = classify_tree_bucket(&meta, social_graph);
        let author = author_labels.for_owner(&meta.owner);
        let is_pinned = pinned_hashes.contains(&root);
        tree_buckets.insert(root, bucket);

        let summary = inventory.bucket_mut(bucket);
        summary.indexed_tree_count += 1;
        summary.indexed_tree_bytes = summary.indexed_tree_bytes.saturating_add(meta.total_size);
        let author_summary = summary.author_mut(&author.key, author.label.clone());
        author_summary.indexed_tree_count += 1;
        author_summary.indexed_tree_bytes = author_summary
            .indexed_tree_bytes
            .saturating_add(meta.total_size);
        if is_pinned {
            author_summary.pinned_tree_count += 1;
        }
        summary.trees.push(TreeDetail {
            name: tree_display_name(&root, &meta),
            owner: author.label,
            root: to_hex(&root),
            size_bytes: meta.total_size,
            pinned: is_pinned,
        });
    }

    for pin in pins {
        let hash = from_hex(&pin.cid).ok();
        let bucket = hash
            .and_then(|hash| tree_buckets.get(&hash).copied())
            .unwrap_or(StorageBucket::Other);
        let indexed = hash
            .as_ref()
            .and_then(|hash| tree_buckets.get(hash))
            .is_some();
        let summary = inventory.bucket_mut(bucket);
        if !indexed {
            summary.pinned_unindexed_count += 1;
            summary.pinned_unindexed_bytes = summary
                .pinned_unindexed_bytes
                .saturating_add(pin.size_bytes);
        }
        summary.pinned_items.push(PinnedDetail {
            name: pin.name,
            cid: pin.cid,
            is_directory: pin.is_directory,
            size_bytes: pin.size_bytes,
        });
    }

    for owned in store.owned_blob_stats()? {
        let distance = social_graph.and_then(|graph| graph.follow_distance(&owned.owner).ok()?);
        let bucket = classify_storage_bucket(0, distance);
        let author = author_labels.for_pubkey(&owned.owner);
        let summary = inventory.bucket_mut(bucket);
        summary.owned_blob_count = summary.owned_blob_count.saturating_add(owned.count);
        summary.owned_blob_bytes = summary.owned_blob_bytes.saturating_add(owned.total_bytes);
        let author_summary = summary.author_mut(&author.key, author.label);
        author_summary.owned_blob_count =
            author_summary.owned_blob_count.saturating_add(owned.count);
        author_summary.owned_blob_bytes = author_summary
            .owned_blob_bytes
            .saturating_add(owned.total_bytes);
    }

    for bucket in &mut inventory.buckets {
        bucket
            .trees
            .sort_by(|left, right| right.size_bytes.cmp(&left.size_bytes));
        bucket
            .pinned_items
            .sort_by(|left, right| right.size_bytes.cmp(&left.size_bytes));
        bucket.authors.sort_by(|left, right| {
            right
                .known_bytes()
                .cmp(&left.known_bytes())
                .then_with(|| left.label.cmp(&right.label))
        });
    }

    Ok(inventory)
}

pub(crate) fn render_storage_inventory(inventory: &StorageInventory) -> String {
    let mut out = String::new();
    out.push_str("Known content:\n");

    for bucket in &inventory.buckets {
        out.push_str(&format!(
            "  {}: {}\n",
            bucket.bucket.label(),
            format_bytes(bucket.known_bytes())
        ));

        if bucket.indexed_tree_count > 0 {
            out.push_str(&format!(
                "    Indexed trees: {} ({})\n",
                count_label(bucket.indexed_tree_count, "tree", "trees"),
                format_bytes(bucket.indexed_tree_bytes)
            ));
        }
        if bucket.owned_blob_count > 0 {
            out.push_str(&format!(
                "    Owned Blossom blobs: {} ({})\n",
                count_label(bucket.owned_blob_count, "blob", "blobs"),
                format_bytes(bucket.owned_blob_bytes)
            ));
        }
        if bucket.pinned_unindexed_count > 0 {
            out.push_str(&format!(
                "    Pinned-only items: {} ({})\n",
                count_label(bucket.pinned_unindexed_count, "item", "items"),
                format_bytes(bucket.pinned_unindexed_bytes)
            ));
        }
        if !bucket.authors.is_empty() {
            out.push_str("    Authors:\n");
            for author in bucket.authors.iter().take(AUTHOR_DETAIL_LIMIT) {
                let mut parts = Vec::new();
                if author.indexed_tree_count > 0 {
                    let mut trees = format!(
                        "{} ({})",
                        count_label(author.indexed_tree_count, "tree", "trees"),
                        format_bytes(author.indexed_tree_bytes)
                    );
                    if author.pinned_tree_count > 0 {
                        trees.push_str(&format!(
                            ", {} pinned",
                            count_label(author.pinned_tree_count, "tree", "trees")
                        ));
                    }
                    parts.push(trees);
                }
                if author.owned_blob_count > 0 {
                    parts.push(format!(
                        "{} ({})",
                        count_label(author.owned_blob_count, "blob", "blobs"),
                        format_bytes(author.owned_blob_bytes)
                    ));
                }
                out.push_str(&format!(
                    "      - {} - {}\n",
                    author.label,
                    parts.join("; ")
                ));
            }
            append_more_line(
                &mut out,
                bucket.authors.len(),
                AUTHOR_DETAIL_LIMIT,
                "author",
                "authors",
            );
        }
        if !bucket.pinned_items.is_empty() {
            out.push_str("    Pinned items:\n");
            for pin in bucket.pinned_items.iter().take(PIN_DETAIL_LIMIT) {
                let kind = if pin.is_directory { "dir" } else { "file" };
                out.push_str(&format!(
                    "      - [{}] {} - {} - {}\n",
                    kind,
                    pin.name,
                    format_bytes(pin.size_bytes),
                    short_hash(&pin.cid)
                ));
            }
            append_more_line(
                &mut out,
                bucket.pinned_items.len(),
                PIN_DETAIL_LIMIT,
                "pinned item",
                "pinned items",
            );
        }
        if !bucket.trees.is_empty() {
            out.push_str("    Largest indexed trees:\n");
            for tree in bucket.trees.iter().take(TREE_DETAIL_LIMIT) {
                let pinned = if tree.pinned { " (pinned)" } else { "" };
                out.push_str(&format!(
                    "      - {} - {} - {}{} - {}\n",
                    tree.name,
                    format_bytes(tree.size_bytes),
                    tree.owner,
                    pinned,
                    short_hash(&tree.root)
                ));
            }
            append_more_line(
                &mut out,
                bucket.trees.len(),
                TREE_DETAIL_LIMIT,
                "indexed tree",
                "indexed trees",
            );
        }
    }

    let tree_count = inventory.indexed_tree_count();
    if tree_count > 0 {
        out.push_str(&format!(
            "  Known indexed payloads: {} across {}\n",
            format_bytes(inventory.indexed_tree_bytes()),
            count_label(tree_count, "tree", "trees")
        ));
    }

    let owned_blob_count = inventory.owned_blob_count();
    if owned_blob_count > 0 {
        out.push_str(&format!(
            "  Known Blossom ownership: {} across {}\n",
            format_bytes(inventory.owned_blob_bytes()),
            count_label(owned_blob_count, "blob", "blobs")
        ));
    }

    out
}

pub(crate) fn print_storage_inventory(store: &HashtreeStore, data_dir: &Path) -> Result<()> {
    let inventory = collect_storage_inventory(store, data_dir)?;
    println!();
    print!("{}", render_storage_inventory(&inventory));
    Ok(())
}

fn open_stats_social_graph(data_dir: &Path) -> Option<std::sync::Arc<SocialGraphStore>> {
    if !data_dir.join("socialgraph").exists() {
        return None;
    }

    match open_social_graph_store(data_dir) {
        Ok(store) => Some(store),
        Err(err) => {
            tracing::debug!("Failed to open social graph for storage stats: {}", err);
            None
        }
    }
}

fn classify_tree_bucket(meta: &TreeMeta, social_graph: Option<&SocialGraphStore>) -> StorageBucket {
    let distance = social_graph.and_then(|graph| {
        owner_pubkey_bytes(&meta.owner).and_then(|owner| graph.follow_distance(&owner).ok()?)
    });
    classify_storage_bucket(meta.priority, distance)
}

#[derive(Debug, Clone)]
struct AuthorIdentity {
    key: String,
    label: String,
}

struct AuthorLabelCache<'a> {
    graph: Option<&'a SocialGraphStore>,
    labels: HashMap<String, String>,
}

impl<'a> AuthorLabelCache<'a> {
    fn new(graph: Option<&'a SocialGraphStore>) -> Self {
        Self {
            graph,
            labels: HashMap::new(),
        }
    }

    fn for_owner(&mut self, owner: &str) -> AuthorIdentity {
        if let Some(pubkey) = owner_pubkey_bytes(owner) {
            return self.for_pubkey_with_fallback(&pubkey, owner);
        }

        let label = owner_display_name(owner);
        AuthorIdentity {
            key: label.clone(),
            label,
        }
    }

    fn for_pubkey(&mut self, pubkey: &[u8; 32]) -> AuthorIdentity {
        self.for_pubkey_with_fallback(pubkey, &to_hex(pubkey))
    }

    fn for_pubkey_with_fallback(&mut self, pubkey: &[u8; 32], fallback: &str) -> AuthorIdentity {
        let key = to_hex(pubkey);
        if let Some(label) = self.labels.get(&key) {
            return AuthorIdentity {
                key,
                label: label.clone(),
            };
        }

        let fallback_label = owner_display_name(fallback);
        let label = self
            .graph
            .and_then(|graph| graph.latest_profile_event(&key).ok().flatten())
            .and_then(|event| profile_name_from_json(&event.content))
            .map(|name| format!("{name} ({fallback_label})"))
            .unwrap_or(fallback_label);
        self.labels.insert(key.clone(), label.clone());
        AuthorIdentity { key, label }
    }
}

fn profile_name_from_json(content: &str) -> Option<String> {
    let profile = serde_json::from_str::<serde_json::Value>(content).ok()?;
    ["display_name", "displayName", "name", "username"]
        .into_iter()
        .find_map(|key| normalize_profile_name(profile.get(key)?))
}

fn normalize_profile_name(value: &serde_json::Value) -> Option<String> {
    let raw = value.as_str()?;
    let trimmed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(100).collect())
}

fn owner_pubkey_bytes(owner: &str) -> Option<[u8; 32]> {
    if owner.starts_with("npub") {
        return parse_npub(owner).ok();
    }

    if owner.len() == 64 {
        let bytes = hex::decode(owner).ok()?;
        if bytes.len() != 32 {
            return None;
        }
        let mut pubkey = [0u8; 32];
        pubkey.copy_from_slice(&bytes);
        return Some(pubkey);
    }

    None
}

fn pin_hashes(pins: &[PinnedItem]) -> HashSet<Hash> {
    pins.iter()
        .filter_map(|pin| from_hex(&pin.cid).ok())
        .collect()
}

fn tree_display_name(root: &Hash, meta: &TreeMeta) -> String {
    match (meta.owner.as_str(), meta.name.as_deref()) {
        ("", Some(name)) | ("pinned", Some(name)) => name.to_string(),
        (owner, Some(name)) if !owner.is_empty() => {
            format!("{}/{}", owner_display_name(owner), name)
        }
        (owner, None) if !owner.is_empty() => owner_display_name(owner),
        _ => short_hash(&to_hex(root)),
    }
}

fn owner_display_name(owner: &str) -> String {
    if owner.len() <= 18 {
        owner.to_string()
    } else {
        format!("{}...", &owner[..18])
    }
}

fn short_hash(hash: &str) -> String {
    if hash.len() <= 12 {
        hash.to_string()
    } else {
        format!("{}...", &hash[..12])
    }
}

fn count_label(count: usize, singular: &str, plural: &str) -> String {
    let noun = if count == 1 { singular } else { plural };
    format!("{count} {noun}")
}

fn append_more_line(out: &mut String, total: usize, limit: usize, singular: &str, plural: &str) {
    if total > limit {
        let remaining = total - limit;
        let noun = if remaining == 1 { singular } else { plural };
        out.push_str(&format!("      - ... {} more {}\n", remaining, noun));
    }
}
