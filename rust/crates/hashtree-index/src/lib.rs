//! Content-addressed B-tree indexes backed by hashtree.

mod search;

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use hashtree_core::{
    Cid, DirEntry, HashTree, HashTreeConfig, HashTreeError, LinkType, Store, TreeEntry,
};
pub use search::{
    SearchError, SearchIndex, SearchIndexOptions, SearchLinkResult, SearchOptions, SearchResult,
};

const DEFAULT_ORDER: usize = 32;
const UPDATE_CHILD_CONCURRENCY: usize = 4;
const PARALLEL_UPDATE_MIN_CHANGES: usize = 256;

#[derive(Debug, Clone, Default)]
pub struct BTreeOptions {
    pub order: Option<usize>,
}

/// Result of a copy-on-write link-index update.
///
/// `superseded_nodes` contains only B-tree directory nodes replaced by `root`.
/// Their referenced values and untouched descendant nodes are deliberately not
/// included. Callers may delete these nodes after the new root is durable.
#[derive(Debug, Clone, Default)]
pub struct BTreeLinkUpdate {
    pub root: Option<Cid>,
    pub superseded_nodes: Vec<Cid>,
}

#[derive(Debug, thiserror::Error)]
pub enum BTreeError {
    #[error("hash tree error: {0}")]
    HashTree(#[from] HashTreeError),
    #[error("value was not valid utf-8: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
    #[error("sorted B-tree input moved backwards from `{previous}` to `{next}`")]
    UnsortedInput { previous: String, next: String },
}

#[derive(Debug, Clone)]
struct SplitResult {
    left: Cid,
    right: Cid,
    left_first_key: String,
    right_first_key: String,
    left_count: u64,
    right_count: u64,
}

#[derive(Debug, Clone)]
enum InsertValue {
    String(String),
    Link(Cid),
}

type BTreeFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, BTreeError>> + 'a>>;

pub struct BTree<S: Store> {
    tree: HashTree<S>,
    max_keys: usize,
    update_child_concurrency: usize,
}

/// Incremental builder for an already sorted CID-link stream.
pub struct BTreeLinkBulkBuilder<'a, S: Store> {
    index: &'a BTree<S>,
    levels: Vec<Vec<BuiltNode>>,
    leaf: Vec<(String, Cid)>,
    pending: Option<(String, Cid)>,
}

#[derive(Debug, Clone)]
struct BuiltNode {
    first_key: String,
    cid: Cid,
    count: Option<u64>,
}

#[derive(Debug, Default)]
struct LinkNodeUpdate {
    nodes: Vec<BuiltNode>,
    superseded_nodes: Vec<Cid>,
}

impl<S: Store> BTree<S> {
    pub fn new(store: Arc<S>, options: BTreeOptions) -> Self {
        let order = options.order.unwrap_or(DEFAULT_ORDER).max(2);
        Self {
            tree: HashTree::new(HashTreeConfig::new(store)),
            max_keys: order - 1,
            update_child_concurrency: UPDATE_CHILD_CONCURRENCY,
        }
    }

    /// Bound parallel copy-on-write subtree updates.
    ///
    /// Large derived-index projections can touch many independent root
    /// children at once. Restricting that fanout lets callers operating under
    /// a tight memory cgroup trade throughput for a lower peak working set.
    pub fn with_update_concurrency(mut self, concurrency: usize) -> Self {
        self.update_child_concurrency = concurrency.max(1);
        self
    }

    pub async fn insert(
        &self,
        root: Option<&Cid>,
        key: &str,
        value: &str,
    ) -> Result<Cid, BTreeError> {
        if let Some(root) = root {
            if self.get(Some(root), key).await?.as_deref() == Some(value) {
                return Ok(root.clone());
            }

            let result = self
                .insert_recursive(
                    root.clone(),
                    key.to_string(),
                    InsertValue::String(value.to_string()),
                )
                .await?;
            return self.finish_insert(result).await;
        }

        self.create_leaf(&[(key.to_string(), value.to_string())])
            .await
    }

    pub async fn get(&self, root: Option<&Cid>, key: &str) -> Result<Option<String>, BTreeError> {
        let Some(root) = root else {
            return Ok(None);
        };
        self.get_recursive(root.clone(), key.to_string()).await
    }

    pub async fn insert_link(
        &self,
        root: Option<&Cid>,
        key: &str,
        target_cid: &Cid,
    ) -> Result<Cid, BTreeError> {
        if let Some(root) = root {
            if self
                .get_link(Some(root), key)
                .await?
                .is_some_and(|existing| cid_equals(&existing, target_cid))
            {
                return Ok(root.clone());
            }

            let result = self
                .insert_recursive(
                    root.clone(),
                    key.to_string(),
                    InsertValue::Link(target_cid.clone()),
                )
                .await?;
            return self.finish_insert(result).await;
        }

        self.create_leaf_with_links(&[(key.to_string(), target_cid.clone())])
            .await
    }

    pub async fn insert_link_unchecked(
        &self,
        root: Option<&Cid>,
        key: &str,
        target_cid: &Cid,
    ) -> Result<Cid, BTreeError> {
        if let Some(root) = root {
            let result = self
                .insert_recursive(
                    root.clone(),
                    key.to_string(),
                    InsertValue::Link(target_cid.clone()),
                )
                .await?;
            return self.finish_insert(result).await;
        }

        self.create_leaf_with_links(&[(key.to_string(), target_cid.clone())])
            .await
    }

    pub async fn get_link(&self, root: Option<&Cid>, key: &str) -> Result<Option<Cid>, BTreeError> {
        let Some(root) = root else {
            return Ok(None);
        };
        self.get_link_recursive(root.clone(), key.to_string()).await
    }

    /// Resolve many link keys in one tree walk, skipping subtrees that cannot
    /// contain any requested key.
    pub async fn get_links<I>(
        &self,
        root: Option<&Cid>,
        keys: I,
    ) -> Result<BTreeMap<String, Cid>, BTreeError>
    where
        I: IntoIterator<Item = String>,
    {
        let keys = keys.into_iter().collect::<BTreeSet<_>>();
        let Some(root) = root else {
            return Ok(BTreeMap::new());
        };
        if keys.is_empty() {
            return Ok(BTreeMap::new());
        }
        self.get_links_recursive(root.clone(), &keys.into_iter().collect::<Vec<_>>())
            .await
    }

    pub async fn entries(&self, root: Option<&Cid>) -> Result<Vec<(String, String)>, BTreeError> {
        let Some(root) = root else {
            return Ok(Vec::new());
        };
        self.traverse_in_order(root.clone()).await
    }

    pub async fn links_entries(
        &self,
        root: Option<&Cid>,
    ) -> Result<Vec<(String, Cid)>, BTreeError> {
        let Some(root) = root else {
            return Ok(Vec::new());
        };
        self.traverse_links_in_order(root.clone()).await
    }

    pub async fn links_entries_limited(
        &self,
        root: Option<&Cid>,
        limit: usize,
    ) -> Result<Vec<(String, Cid)>, BTreeError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let Some(root) = root else {
            return Ok(Vec::new());
        };
        self.range_link_traverse_limited(root.clone(), None, None, limit)
            .await
    }

    /// Count CID links by walking the tree.
    ///
    /// Uses stored subtree sizes when available, but scans descendants when
    /// older roots do not carry complete counts.
    pub async fn count_links(&self, root: Option<&Cid>) -> Result<u64, BTreeError> {
        self.scan_links(root).await
    }

    /// Count CID links by explicitly walking the tree.
    pub async fn scan_links(&self, root: Option<&Cid>) -> Result<u64, BTreeError> {
        let Some(root) = root else {
            return Ok(0);
        };
        self.count_links_recursive(root.clone()).await
    }

    /// Read the stored CID-link count from the root node without scanning.
    ///
    /// Returns `Ok(None)` when the root was built by older code that does not
    /// store complete subtree sizes.
    pub async fn count_stored_links(&self, root: Option<&Cid>) -> Result<Option<u64>, BTreeError> {
        let Some(root) = root else {
            return Ok(Some(0));
        };

        let entries = self.tree.list_directory(root).await?;
        if is_leaf_node(&entries) {
            return Ok(Some(count_link_entries(&entries)));
        }

        let mut count = 0;
        for entry in &entries {
            let Some(child_count) = stored_link_subtree_count(entry) else {
                return Ok(None);
            };
            count += child_count;
        }
        Ok(Some(count))
    }

    pub async fn range(
        &self,
        root: &Cid,
        start: Option<&str>,
        end: Option<&str>,
    ) -> Result<Vec<(String, String)>, BTreeError> {
        self.range_traverse(
            root.clone(),
            start.map(ToOwned::to_owned),
            end.map(ToOwned::to_owned),
        )
        .await
    }

    pub async fn prefix(
        &self,
        root: &Cid,
        prefix: &str,
    ) -> Result<Vec<(String, String)>, BTreeError> {
        let end = increment_prefix(prefix);
        self.range(root, Some(prefix), end.as_deref()).await
    }

    pub async fn prefix_links(
        &self,
        root: &Cid,
        prefix: &str,
    ) -> Result<Vec<(String, Cid)>, BTreeError> {
        let end = increment_prefix(prefix);
        self.range_link_traverse(root.clone(), Some(prefix.to_string()), end)
            .await
    }

    pub async fn prefix_links_limited(
        &self,
        root: &Cid,
        prefix: &str,
        limit: usize,
    ) -> Result<Vec<(String, Cid)>, BTreeError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let end = increment_prefix(prefix);
        self.range_link_traverse_limited(root.clone(), Some(prefix.to_string()), end, limit)
            .await
    }

    /// Read a bounded page of CID-link entries in key order.
    ///
    /// `start` is inclusive and `end` is exclusive. Callers paging forward can
    /// append `\0` to the final key from the previous page to exclude it.
    pub async fn range_links_limited(
        &self,
        root: &Cid,
        start: Option<&str>,
        end: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, Cid)>, BTreeError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        self.range_link_traverse_limited(
            root.clone(),
            start.map(ToOwned::to_owned),
            end.map(ToOwned::to_owned),
            limit,
        )
        .await
    }

    pub async fn delete(&self, root: &Cid, key: &str) -> Result<Option<Cid>, BTreeError> {
        self.delete_recursive(root.clone(), key.to_string()).await
    }

    pub async fn merge(
        &self,
        base: Option<&Cid>,
        other: Option<&Cid>,
        prefer_other: bool,
    ) -> Result<Option<Cid>, BTreeError> {
        let Some(other) = other else {
            return Ok(base.cloned());
        };
        let Some(mut result) = base.cloned().or_else(|| Some(other.clone())) else {
            return Ok(None);
        };
        if base.is_none() {
            return Ok(Some(result));
        }

        for (key, value) in self.entries(Some(other)).await? {
            let existing = self.get(Some(&result), &key).await?;
            if existing.is_none() || prefer_other {
                result = self.insert(Some(&result), &key, &value).await?;
            }
        }

        Ok(Some(result))
    }

    pub async fn merge_links(
        &self,
        base: Option<&Cid>,
        other: Option<&Cid>,
        prefer_other: bool,
    ) -> Result<Option<Cid>, BTreeError> {
        let Some(other) = other else {
            return Ok(base.cloned());
        };
        let Some(mut result) = base.cloned().or_else(|| Some(other.clone())) else {
            return Ok(None);
        };
        if base.is_none() {
            return Ok(Some(result));
        }

        for (key, value) in self.links_entries(Some(other)).await? {
            let existing = self.get_link(Some(&result), &key).await?;
            if existing.is_none() || prefer_other {
                result = self.insert_link(Some(&result), &key, &value).await?;
            }
        }

        Ok(Some(result))
    }

    pub async fn build<I>(&self, items: I) -> Result<Option<Cid>, BTreeError>
    where
        I: IntoIterator<Item = (String, String)>,
    {
        let mut sorted: Vec<(String, String)> = items.into_iter().collect();
        if sorted.is_empty() {
            return Ok(None);
        }

        sorted.sort_by(|left, right| left.0.cmp(&right.0));

        let mut deduped = Vec::with_capacity(sorted.len());
        for (key, value) in sorted {
            if let Some((last_key, last_value)) = deduped.last_mut() {
                if *last_key == key {
                    *last_value = value;
                    continue;
                }
            }
            deduped.push((key, value));
        }

        let mut level = Vec::with_capacity(deduped.len().div_ceil(self.max_keys));
        for chunk in deduped.chunks(self.max_keys) {
            let cid = self.create_leaf(chunk).await?;
            level.push(BuiltNode {
                first_key: chunk[0].0.clone(),
                cid,
                count: Some(chunk.len() as u64),
            });
        }

        while level.len() > 1 {
            let mut next_level = Vec::with_capacity(level.len().div_ceil(self.max_keys));
            for chunk in level.chunks(self.max_keys) {
                let cid = self.create_internal_node(chunk).await?;
                next_level.push(BuiltNode {
                    first_key: chunk[0].first_key.clone(),
                    cid,
                    count: chunk.iter().map(|child| child.count).sum(),
                });
            }
            level = next_level;
        }

        Ok(level.pop().map(|node| node.cid))
    }

    /// Apply a sorted batch of string insertions and deletions, reusing
    /// untouched subtrees. Repeated changes for one key use the last value;
    /// `None` deletes a key.
    pub async fn update<I>(&self, root: Option<&Cid>, changes: I) -> Result<Option<Cid>, BTreeError>
    where
        I: IntoIterator<Item = (String, Option<String>)>,
    {
        let changes = changes.into_iter().collect::<BTreeMap<_, _>>();
        if changes.is_empty() {
            return Ok(root.cloned());
        }

        let changes = changes.into_iter().collect::<Vec<_>>();
        let Some(root) = root else {
            return self
                .build(
                    changes
                        .into_iter()
                        .filter_map(|(key, value)| value.map(|value| (key, value))),
                )
                .await;
        };

        let nodes = self
            .update_string_node(root.clone(), &changes, true)
            .await?;
        self.finish_link_node_updates(nodes).await
    }

    fn update_string_node<'a>(
        &'a self,
        node: Cid,
        changes: &'a [(String, Option<String>)],
        parallel_children: bool,
    ) -> BTreeFuture<'a, Vec<BuiltNode>> {
        Box::pin(async move {
            let entries = sort_entries(self.tree.list_directory(&node).await?);
            if is_leaf_node(&entries) {
                return self.update_string_leaf(entries, changes).await;
            }

            let mut work = Vec::with_capacity(entries.len());
            let mut change_start = 0;
            for (child_index, entry) in entries.iter().enumerate() {
                let change_end = entries
                    .get(child_index + 1)
                    .map(|next| {
                        let next_key = unescape_key(&next.name);
                        change_start
                            + changes[change_start..].partition_point(|(key, _)| key < &next_key)
                    })
                    .unwrap_or(changes.len());
                work.push((
                    BuiltNode {
                        first_key: unescape_key(&entry.name),
                        cid: entry_cid(entry),
                        count: stored_link_subtree_count(entry),
                    },
                    change_start,
                    change_end,
                ));
                change_start = change_end;
            }

            let touched_children = work
                .iter()
                .filter(|(_, change_start, change_end)| change_start != change_end)
                .count();
            let children = if parallel_children
                && changes.len() >= PARALLEL_UPDATE_MIN_CHANGES
                && touched_children > 1
            {
                self.update_string_children_parallel(work, changes)?
            } else {
                let mut children = Vec::new();
                for (child, change_start, change_end) in work {
                    if change_start == change_end {
                        children.push(child);
                    } else {
                        children.extend(
                            self.update_string_node(
                                child.cid,
                                &changes[change_start..change_end],
                                parallel_children,
                            )
                            .await?,
                        );
                    }
                }
                children
            };

            self.create_link_node_level(children).await
        })
    }

    fn update_string_children_parallel(
        &self,
        work: Vec<(BuiltNode, usize, usize)>,
        changes: &[(String, Option<String>)],
    ) -> Result<Vec<BuiltNode>, BTreeError> {
        // Store reads are intentionally synchronous behind the async trait on
        // local LMDB. Run independent immutable root branches on a small,
        // fixed number of scoped workers so the backend can issue real
        // parallel reads without multiplying memory at every tree depth.
        let worker_count = UPDATE_CHILD_CONCURRENCY.min(work.len()).max(1);
        let work_per_worker = work.len().div_ceil(worker_count);
        std::thread::scope(|scope| {
            let handles = work
                .chunks(work_per_worker)
                .map(|work_chunk| {
                    scope.spawn(move || {
                        let mut children = Vec::new();
                        for (child, change_start, change_end) in work_chunk {
                            if change_start == change_end {
                                children.push(child.clone());
                            } else {
                                children.extend(futures::executor::block_on(
                                    self.update_string_node(
                                        child.cid.clone(),
                                        &changes[*change_start..*change_end],
                                        false,
                                    ),
                                )?);
                            }
                        }
                        Ok::<_, BTreeError>(children)
                    })
                })
                .collect::<Vec<_>>();
            let mut children = Vec::new();
            for handle in handles {
                children.extend(
                    handle
                        .join()
                        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))?,
                );
            }
            Ok(children)
        })
    }

    async fn update_string_leaf(
        &self,
        entries: Vec<TreeEntry>,
        changes: &[(String, Option<String>)],
    ) -> Result<Vec<BuiltNode>, BTreeError> {
        let mut final_entries = BTreeMap::new();
        for entry in entries {
            if entry.link_type != LinkType::Blob {
                continue;
            }
            let Some(data) = self.tree.get(&entry_cid(&entry), None).await? else {
                continue;
            };
            final_entries.insert(unescape_key(&entry.name), String::from_utf8(data)?);
        }
        for (key, value) in changes {
            match value {
                Some(value) => {
                    final_entries.insert(key.clone(), value.clone());
                }
                None => {
                    final_entries.remove(key);
                }
            }
        }

        let final_entries = final_entries.into_iter().collect::<Vec<_>>();
        let mut nodes = Vec::with_capacity(final_entries.len().div_ceil(self.max_keys));
        for chunk in final_entries.chunks(self.max_keys) {
            let cid = self.create_leaf(chunk).await?;
            nodes.push(BuiltNode {
                first_key: chunk[0].0.clone(),
                cid,
                count: Some(chunk.len() as u64),
            });
        }
        Ok(nodes)
    }

    pub async fn build_links<I>(&self, items: I) -> Result<Option<Cid>, BTreeError>
    where
        I: IntoIterator<Item = (String, Cid)>,
    {
        let mut sorted: Vec<(String, Cid)> = items.into_iter().collect();
        if sorted.is_empty() {
            return Ok(None);
        }

        sorted.sort_by(|left, right| left.0.cmp(&right.0));

        let mut deduped = Vec::with_capacity(sorted.len());
        for (key, cid) in sorted {
            if let Some((last_key, last_cid)) = deduped.last_mut() {
                if *last_key == key {
                    *last_cid = cid;
                    continue;
                }
            }
            deduped.push((key, cid));
        }

        let mut level = Vec::with_capacity(deduped.len().div_ceil(self.max_keys));
        for chunk in deduped.chunks(self.max_keys) {
            let cid = self.create_leaf_with_links(chunk).await?;
            level.push(BuiltNode {
                first_key: chunk[0].0.clone(),
                cid,
                count: Some(chunk.len() as u64),
            });
        }

        while level.len() > 1 {
            let mut next_level = Vec::with_capacity(level.len().div_ceil(self.max_keys));
            for chunk in level.chunks(self.max_keys) {
                let cid = self.create_internal_node(chunk).await?;
                next_level.push(BuiltNode {
                    first_key: chunk[0].first_key.clone(),
                    cid,
                    count: chunk.iter().map(|child| child.count).sum(),
                });
            }
            level = next_level;
        }

        Ok(level.pop().map(|node| node.cid))
    }

    /// Build a CID-link tree from entries already sorted by key.
    ///
    /// Unlike [`BTree::build_links`], this keeps only one leaf plus at most one
    /// node-sized frontier at each tree level in memory. Adjacent duplicate
    /// keys use the last value, matching the ordinary bulk builder. This is
    /// intended for disk-backed sorters whose output must not be collected into
    /// RAM again.
    pub async fn build_sorted_links<I>(&self, items: I) -> Result<Option<Cid>, BTreeError>
    where
        I: IntoIterator<Item = (String, Cid)>,
    {
        let mut builder = self.sorted_link_builder();
        for (key, cid) in items {
            builder.push(key, cid).await?;
        }
        builder.finish().await
    }

    pub fn sorted_link_builder(&self) -> BTreeLinkBulkBuilder<'_, S> {
        BTreeLinkBulkBuilder {
            index: self,
            levels: Vec::new(),
            leaf: Vec::with_capacity(self.max_keys),
            pending: None,
        }
    }
}

impl<S: Store> BTreeLinkBulkBuilder<'_, S> {
    pub async fn push(&mut self, key: String, cid: Cid) -> Result<(), BTreeError> {
        if let Some((previous_key, previous_cid)) = self.pending.as_mut() {
            match key.cmp(previous_key) {
                Ordering::Less => {
                    return Err(BTreeError::UnsortedInput {
                        previous: previous_key.clone(),
                        next: key,
                    });
                }
                Ordering::Equal => {
                    *previous_cid = cid;
                    return Ok(());
                }
                Ordering::Greater => self
                    .leaf
                    .push(self.pending.take().expect("pending sorted link")),
            }
        }
        self.pending = Some((key, cid));
        self.flush_full_leaf().await
    }

    pub async fn finish(mut self) -> Result<Option<Cid>, BTreeError> {
        if let Some(entry) = self.pending.take() {
            self.leaf.push(entry);
        }
        self.flush_leaf().await?;
        loop {
            let Some(lowest) = self.levels.iter().position(|level| !level.is_empty()) else {
                return Ok(None);
            };
            let highest = self
                .levels
                .iter()
                .rposition(|level| !level.is_empty())
                .expect("a lowest non-empty bulk-builder level exists");
            if lowest == highest && self.levels[lowest].len() == 1 {
                return Ok(self.levels[lowest].pop().map(|node| node.cid));
            }
            let children = std::mem::take(&mut self.levels[lowest]);
            let parent = self.build_parent(&children).await?;
            self.push_node(lowest + 1, parent).await?;
        }
    }

    async fn flush_full_leaf(&mut self) -> Result<(), BTreeError> {
        if self.leaf.len() == self.index.max_keys {
            self.flush_leaf().await?;
        }
        Ok(())
    }

    async fn flush_leaf(&mut self) -> Result<(), BTreeError> {
        if self.leaf.is_empty() {
            return Ok(());
        }
        let cid = self.index.create_leaf_with_links(&self.leaf).await?;
        let node = BuiltNode {
            first_key: self.leaf[0].0.clone(),
            cid,
            count: Some(self.leaf.len() as u64),
        };
        self.leaf.clear();
        self.push_node(0, node).await?;
        Ok(())
    }

    async fn push_node(
        &mut self,
        mut level_index: usize,
        mut node: BuiltNode,
    ) -> Result<(), BTreeError> {
        loop {
            if self.levels.len() <= level_index {
                self.levels.push(Vec::with_capacity(self.index.max_keys));
            }
            self.levels[level_index].push(node);
            if self.levels[level_index].len() < self.index.max_keys {
                return Ok(());
            }
            let children = std::mem::take(&mut self.levels[level_index]);
            node = self.build_parent(&children).await?;
            level_index += 1;
        }
    }

    async fn build_parent(&self, children: &[BuiltNode]) -> Result<BuiltNode, BTreeError> {
        let cid = self.index.create_internal_node(children).await?;
        Ok(BuiltNode {
            first_key: children[0].first_key.clone(),
            cid,
            count: children.iter().map(|child| child.count).sum(),
        })
    }
}

impl<S: Store> BTree<S> {
    /// Apply a sorted batch of link insertions and deletions, reusing untouched
    /// subtrees. Repeated changes for one key use the last value; `None` deletes
    /// a key.
    pub async fn update_links<I>(
        &self,
        root: Option<&Cid>,
        changes: I,
    ) -> Result<Option<Cid>, BTreeError>
    where
        I: IntoIterator<Item = (String, Option<Cid>)>,
    {
        Ok(self.update_links_with_superseded(root, changes).await?.root)
    }

    /// Apply a sorted batch of link changes and report the old copy-on-write
    /// B-tree nodes made unreachable by the resulting root.
    pub async fn update_links_with_superseded<I>(
        &self,
        root: Option<&Cid>,
        changes: I,
    ) -> Result<BTreeLinkUpdate, BTreeError>
    where
        I: IntoIterator<Item = (String, Option<Cid>)>,
    {
        let changes = changes.into_iter().collect::<BTreeMap<_, _>>();
        if changes.is_empty() {
            return Ok(BTreeLinkUpdate {
                root: root.cloned(),
                superseded_nodes: Vec::new(),
            });
        }

        let changes = changes.into_iter().collect::<Vec<_>>();
        let Some(root) = root else {
            let root = self
                .build_links(
                    changes
                        .into_iter()
                        .filter_map(|(key, cid)| cid.map(|cid| (key, cid))),
                )
                .await?;
            return Ok(BTreeLinkUpdate {
                root,
                superseded_nodes: Vec::new(),
            });
        };

        let update = self.update_link_node(root.clone(), &changes, true).await?;
        let new_root = self.finish_link_node_updates(update.nodes).await?;
        Ok(BTreeLinkUpdate {
            root: new_root,
            superseded_nodes: update.superseded_nodes,
        })
    }

    fn update_link_node<'a>(
        &'a self,
        node: Cid,
        changes: &'a [(String, Option<Cid>)],
        parallel_children: bool,
    ) -> BTreeFuture<'a, LinkNodeUpdate> {
        Box::pin(async move {
            let entries = sort_entries(self.tree.list_directory(&node).await?);
            let mut update = if is_leaf_node(&entries) {
                LinkNodeUpdate {
                    nodes: self.update_link_leaf(entries, changes).await?,
                    superseded_nodes: Vec::new(),
                }
            } else {
                let mut work = Vec::with_capacity(entries.len());
                let mut change_start = 0;
                for (child_index, entry) in entries.iter().enumerate() {
                    let change_end = entries
                        .get(child_index + 1)
                        .map(|next| {
                            let next_key = unescape_key(&next.name);
                            change_start
                                + changes[change_start..]
                                    .partition_point(|(key, _)| key < &next_key)
                        })
                        .unwrap_or(changes.len());
                    work.push((
                        BuiltNode {
                            first_key: unescape_key(&entry.name),
                            cid: entry_cid(entry),
                            count: stored_link_subtree_count(entry),
                        },
                        change_start,
                        change_end,
                    ));
                    change_start = change_end;
                }

                let touched_children = work
                    .iter()
                    .filter(|(_, change_start, change_end)| change_start != change_end)
                    .count();
                let children_update = if parallel_children
                    && self.update_child_concurrency > 1
                    && changes.len() >= PARALLEL_UPDATE_MIN_CHANGES
                    && touched_children > 1
                {
                    self.update_link_children_parallel(work, changes)?
                } else {
                    let mut update = LinkNodeUpdate::default();
                    for (child, change_start, change_end) in work {
                        if change_start == change_end {
                            update.nodes.push(child);
                        } else {
                            let child_update = self
                                .update_link_node(
                                    child.cid,
                                    &changes[change_start..change_end],
                                    parallel_children,
                                )
                                .await?;
                            update.nodes.extend(child_update.nodes);
                            update
                                .superseded_nodes
                                .extend(child_update.superseded_nodes);
                        }
                    }
                    update
                };

                LinkNodeUpdate {
                    nodes: self.create_link_node_level(children_update.nodes).await?,
                    superseded_nodes: children_update.superseded_nodes,
                }
            };

            // A split can retain the original node verbatim as one output
            // chunk while adding siblings around it. Content addressing then
            // gives that retained chunk the same CID as `node`; deleting it as
            // superseded would corrupt the new root. Reclaim the old hash only
            // when none of the replacement nodes still references it.
            if !update
                .nodes
                .iter()
                .any(|replacement| replacement.cid == node)
            {
                update.superseded_nodes.push(node);
            }
            Ok(update)
        })
    }

    fn update_link_children_parallel(
        &self,
        work: Vec<(BuiltNode, usize, usize)>,
        changes: &[(String, Option<Cid>)],
    ) -> Result<LinkNodeUpdate, BTreeError> {
        let worker_count = self.update_child_concurrency.min(work.len()).max(1);
        let work_per_worker = work.len().div_ceil(worker_count);
        std::thread::scope(|scope| {
            let handles = work
                .chunks(work_per_worker)
                .map(|work_chunk| {
                    scope.spawn(move || {
                        let mut update = LinkNodeUpdate::default();
                        for (child, change_start, change_end) in work_chunk {
                            if change_start == change_end {
                                update.nodes.push(child.clone());
                            } else {
                                let child_update =
                                    futures::executor::block_on(self.update_link_node(
                                        child.cid.clone(),
                                        &changes[*change_start..*change_end],
                                        false,
                                    ))?;
                                update.nodes.extend(child_update.nodes);
                                update
                                    .superseded_nodes
                                    .extend(child_update.superseded_nodes);
                            }
                        }
                        Ok::<_, BTreeError>(update)
                    })
                })
                .collect::<Vec<_>>();
            let mut update = LinkNodeUpdate::default();
            for handle in handles {
                let child_update = handle
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))?;
                update.nodes.extend(child_update.nodes);
                update
                    .superseded_nodes
                    .extend(child_update.superseded_nodes);
            }
            Ok(update)
        })
    }

    async fn update_link_leaf(
        &self,
        entries: Vec<TreeEntry>,
        changes: &[(String, Option<Cid>)],
    ) -> Result<Vec<BuiltNode>, BTreeError> {
        let mut final_entries = entries
            .into_iter()
            .map(|entry| (unescape_key(&entry.name), entry_cid(&entry)))
            .collect::<BTreeMap<_, _>>();
        for (key, cid) in changes {
            match cid {
                Some(cid) => {
                    final_entries.insert(key.clone(), cid.clone());
                }
                None => {
                    final_entries.remove(key);
                }
            }
        }

        let final_entries = final_entries.into_iter().collect::<Vec<_>>();
        let mut nodes = Vec::with_capacity(final_entries.len().div_ceil(self.max_keys));
        for chunk in final_entries.chunks(self.max_keys) {
            let cid = self.create_leaf_with_links(chunk).await?;
            nodes.push(BuiltNode {
                first_key: chunk[0].0.clone(),
                cid,
                count: Some(chunk.len() as u64),
            });
        }
        Ok(nodes)
    }

    async fn create_link_node_level(
        &self,
        children: Vec<BuiltNode>,
    ) -> Result<Vec<BuiltNode>, BTreeError> {
        let mut nodes = Vec::with_capacity(children.len().div_ceil(self.max_keys));
        for chunk in children.chunks(self.max_keys) {
            let cid = self.create_internal_node(chunk).await?;
            nodes.push(BuiltNode {
                first_key: chunk[0].first_key.clone(),
                cid,
                count: chunk.iter().map(|child| child.count).sum(),
            });
        }
        Ok(nodes)
    }

    async fn finish_link_node_updates(
        &self,
        mut nodes: Vec<BuiltNode>,
    ) -> Result<Option<Cid>, BTreeError> {
        while nodes.len() > 1 {
            nodes = self.create_link_node_level(nodes).await?;
        }
        Ok(nodes.pop().map(|node| node.cid))
    }

    async fn finish_insert(&self, result: InsertResult) -> Result<Cid, BTreeError> {
        if let Some(split) = result.split {
            return self
                .create_internal_root(
                    &split.left_first_key,
                    &split.left,
                    split.left_count,
                    &split.right_first_key,
                    &split.right,
                    split.right_count,
                )
                .await;
        }
        Ok(result.cid)
    }

    fn get_recursive<'a>(&'a self, root: Cid, key: String) -> BTreeFuture<'a, Option<String>> {
        Box::pin(async move {
            let entries = self.tree.list_directory(&root).await?;
            if is_leaf_node(&entries) {
                let escaped = escape_key(&key);
                let Some(entry) = entries.iter().find(|entry| entry.name == escaped) else {
                    return Ok(None);
                };
                if entry.link_type != LinkType::Blob {
                    return Ok(None);
                }

                let cid = entry_cid(entry);
                let Some(data) = self.tree.get(&cid, None).await? else {
                    return Ok(None);
                };
                return Ok(Some(String::from_utf8(data)?));
            }

            let child = find_child(&entries, &key);
            self.get_recursive(entry_cid(&child), key).await
        })
    }

    fn get_link_recursive<'a>(&'a self, root: Cid, key: String) -> BTreeFuture<'a, Option<Cid>> {
        Box::pin(async move {
            let entries = self.tree.list_directory(&root).await?;
            if is_leaf_node(&entries) {
                let escaped = escape_key(&key);
                let Some(entry) = entries.iter().find(|entry| entry.name == escaped) else {
                    return Ok(None);
                };
                if entry.link_type != LinkType::File {
                    return Ok(None);
                }
                return Ok(Some(entry_cid(entry)));
            }

            let child = find_child(&entries, &key);
            self.get_link_recursive(entry_cid(&child), key).await
        })
    }

    fn get_links_recursive<'a>(
        &'a self,
        root: Cid,
        keys: &'a [String],
    ) -> BTreeFuture<'a, BTreeMap<String, Cid>> {
        Box::pin(async move {
            let entries = sort_entries(self.tree.list_directory(&root).await?);
            if is_leaf_node(&entries) {
                let links = entries
                    .into_iter()
                    .map(|entry| (unescape_key(&entry.name), entry_cid(&entry)))
                    .collect::<BTreeMap<_, _>>();
                return Ok(keys
                    .iter()
                    .filter_map(|key| links.get(key).cloned().map(|cid| (key.clone(), cid)))
                    .collect());
            }

            let mut found = BTreeMap::new();
            let mut key_start = 0;
            for (child_index, entry) in entries.iter().enumerate() {
                let key_end = entries
                    .get(child_index + 1)
                    .map(|next| {
                        let next_key = unescape_key(&next.name);
                        key_start + keys[key_start..].partition_point(|key| key < &next_key)
                    })
                    .unwrap_or(keys.len());
                if key_start < key_end {
                    found.extend(
                        self.get_links_recursive(entry_cid(entry), &keys[key_start..key_end])
                            .await?,
                    );
                }
                key_start = key_end;
            }
            Ok(found)
        })
    }

    fn insert_recursive<'a>(
        &'a self,
        node: Cid,
        key: String,
        value: InsertValue,
    ) -> BTreeFuture<'a, InsertResult> {
        Box::pin(async move {
            let entries = self.tree.list_directory(&node).await?;
            if is_leaf_node(&entries) {
                return self.insert_into_leaf(node, entries, key, value).await;
            }
            self.insert_into_internal(node, entries, key, value).await
        })
    }

    fn insert_into_leaf<'a>(
        &'a self,
        node: Cid,
        _entries: Vec<TreeEntry>,
        key: String,
        value: InsertValue,
    ) -> BTreeFuture<'a, InsertResult> {
        Box::pin(async move {
            let escaped_key = escape_key(&key);
            let (entry_cid, size, link_type) = match value {
                InsertValue::String(value) => {
                    let (cid, size) = self.tree.put_file(value.as_bytes()).await?;
                    (cid, size, LinkType::Blob)
                }
                InsertValue::Link(cid) => (cid, 0, LinkType::File),
            };

            let new_node = self
                .tree
                .set_entry(&node, &[], &escaped_key, &entry_cid, size, link_type)
                .await?;

            let new_entries = self.tree.list_directory(&new_node).await?;
            if new_entries.len() > self.max_keys {
                return Ok(InsertResult {
                    cid: new_node,
                    count: count_link_entries_or_subtrees(self, &new_entries).await?,
                    split: Some(self.split_leaf(new_entries).await?),
                });
            }

            Ok(InsertResult {
                cid: new_node,
                count: count_link_entries_or_subtrees(self, &new_entries).await?,
                split: None,
            })
        })
    }

    fn insert_into_internal<'a>(
        &'a self,
        node: Cid,
        entries: Vec<TreeEntry>,
        key: String,
        value: InsertValue,
    ) -> BTreeFuture<'a, InsertResult> {
        Box::pin(async move {
            let child = find_child(&entries, &key);
            let child_name = child.name.clone();
            let child_cid = entry_cid(&child);
            let result = self.insert_recursive(child_cid, key, value).await?;

            let mut new_node = self
                .tree
                .set_entry(
                    &node,
                    &[],
                    &child_name,
                    &result.cid,
                    result.count,
                    LinkType::Dir,
                )
                .await?;

            if let Some(split) = result.split {
                new_node = self.tree.remove_entry(&new_node, &[], &child_name).await?;
                new_node = self
                    .tree
                    .set_entry(
                        &new_node,
                        &[],
                        &escape_key(&split.left_first_key),
                        &split.left,
                        split.left_count,
                        LinkType::Dir,
                    )
                    .await?;
                new_node = self
                    .tree
                    .set_entry(
                        &new_node,
                        &[],
                        &escape_key(&split.right_first_key),
                        &split.right,
                        split.right_count,
                        LinkType::Dir,
                    )
                    .await?;
            }

            let new_entries = self.tree.list_directory(&new_node).await?;
            if new_entries.len() > self.max_keys {
                return Ok(InsertResult {
                    cid: new_node,
                    count: count_link_entries_or_subtrees(self, &new_entries).await?,
                    split: Some(self.split_internal(new_entries).await?),
                });
            }

            Ok(InsertResult {
                cid: new_node,
                count: count_link_entries_or_subtrees(self, &new_entries).await?,
                split: None,
            })
        })
    }

    async fn split_leaf(&self, entries: Vec<TreeEntry>) -> Result<SplitResult, BTreeError> {
        let sorted = sort_entries(entries);
        let mid = sorted.len() / 2;
        let left_entries = &sorted[..mid];
        let right_entries = &sorted[mid..];

        let left = self.create_node_from_entries(left_entries).await?;
        let right = self.create_node_from_entries(right_entries).await?;

        Ok(SplitResult {
            left,
            right,
            left_first_key: unescape_key(&left_entries[0].name),
            right_first_key: unescape_key(&right_entries[0].name),
            left_count: count_link_entries(left_entries),
            right_count: count_link_entries(right_entries),
        })
    }

    async fn split_internal(&self, entries: Vec<TreeEntry>) -> Result<SplitResult, BTreeError> {
        let sorted = sort_entries(entries);
        let mid = sorted.len() / 2;
        let left_entries = &sorted[..mid];
        let right_entries = &sorted[mid..];

        let left = self.create_node_from_entries(left_entries).await?;
        let right = self.create_node_from_entries(right_entries).await?;

        Ok(SplitResult {
            left,
            right,
            left_first_key: unescape_key(&left_entries[0].name),
            right_first_key: unescape_key(&right_entries[0].name),
            left_count: count_link_entries_or_subtrees(self, left_entries).await?,
            right_count: count_link_entries_or_subtrees(self, right_entries).await?,
        })
    }

    async fn create_leaf(&self, items: &[(String, String)]) -> Result<Cid, BTreeError> {
        let mut entries = Vec::with_capacity(items.len());
        for (key, value) in items {
            let (cid, size) = self.tree.put_file(value.as_bytes()).await?;
            entries.push(
                DirEntry::from_cid(escape_key(key), &cid)
                    .with_size(size)
                    .with_link_type(LinkType::Blob),
            );
        }
        Ok(self.tree.put_directory(entries).await?)
    }

    async fn create_leaf_with_links(&self, items: &[(String, Cid)]) -> Result<Cid, BTreeError> {
        let entries: Vec<DirEntry> = items
            .iter()
            .map(|(key, cid)| {
                DirEntry::from_cid(escape_key(key), cid).with_link_type(LinkType::File)
            })
            .collect();
        Ok(self.tree.put_directory(entries).await?)
    }

    async fn create_internal_node(&self, children: &[BuiltNode]) -> Result<Cid, BTreeError> {
        let entries: Vec<DirEntry> = children
            .iter()
            .map(|child| {
                DirEntry::from_cid(escape_key(&child.first_key), &child.cid)
                    .with_size(child.count.unwrap_or(0))
                    .with_link_type(LinkType::Dir)
            })
            .collect();
        Ok(self.tree.put_directory(entries).await?)
    }

    async fn create_internal_root(
        &self,
        left_key: &str,
        left: &Cid,
        left_count: u64,
        right_key: &str,
        right: &Cid,
        right_count: u64,
    ) -> Result<Cid, BTreeError> {
        let entries = vec![
            DirEntry::from_cid(escape_key(left_key), left)
                .with_size(left_count)
                .with_link_type(LinkType::Dir),
            DirEntry::from_cid(escape_key(right_key), right)
                .with_size(right_count)
                .with_link_type(LinkType::Dir),
        ];
        Ok(self.tree.put_directory(entries).await?)
    }

    async fn create_node_from_entries(&self, entries: &[TreeEntry]) -> Result<Cid, BTreeError> {
        let dir_entries = entries
            .iter()
            .cloned()
            .map(tree_entry_to_dir_entry)
            .collect::<Vec<_>>();
        Ok(self.tree.put_directory(dir_entries).await?)
    }

    fn delete_recursive<'a>(&'a self, root: Cid, key: String) -> BTreeFuture<'a, Option<Cid>> {
        Box::pin(async move {
            let entries = self.tree.list_directory(&root).await?;
            if is_leaf_node(&entries) {
                let escaped = escape_key(&key);
                if !entries.iter().any(|entry| entry.name == escaped) {
                    return Ok(Some(root));
                }

                let new_root = self.tree.remove_entry(&root, &[], &escaped).await?;
                let new_entries = self.tree.list_directory(&new_root).await?;
                if new_entries.is_empty() {
                    return Ok(None);
                }
                return Ok(Some(new_root));
            }

            let child = find_child(&entries, &key);
            let child_name = child.name.clone();
            let new_child = self.delete_recursive(entry_cid(&child), key).await?;

            let Some(new_child) = new_child else {
                let new_root = self.tree.remove_entry(&root, &[], &child_name).await?;
                let new_entries = self.tree.list_directory(&new_root).await?;
                if new_entries.is_empty() {
                    return Ok(None);
                }
                if new_entries.len() == 1 && new_entries[0].link_type == LinkType::Dir {
                    return Ok(Some(entry_cid(&new_entries[0])));
                }
                return Ok(Some(new_root));
            };

            if cid_equals(&new_child, &entry_cid(&child)) {
                return Ok(Some(root));
            }

            let updated = self
                .tree
                .set_entry(
                    &root,
                    &[],
                    &child_name,
                    &new_child,
                    count_link_entries_or_subtrees(
                        self,
                        &self.tree.list_directory(&new_child).await?,
                    )
                    .await?,
                    LinkType::Dir,
                )
                .await?;
            Ok(Some(updated))
        })
    }

    fn traverse_in_order<'a>(&'a self, node: Cid) -> BTreeFuture<'a, Vec<(String, String)>> {
        Box::pin(async move {
            let entries = self.tree.list_directory(&node).await?;
            let sorted = sort_entries(entries);
            let mut out = Vec::new();

            if is_leaf_node(&sorted) {
                for entry in sorted {
                    if entry.link_type != LinkType::Blob {
                        continue;
                    }
                    let cid = entry_cid(&entry);
                    if let Some(data) = self.tree.get(&cid, None).await? {
                        out.push((unescape_key(&entry.name), String::from_utf8(data)?));
                    }
                }
                return Ok(out);
            }

            for child in sorted {
                out.extend(self.traverse_in_order(entry_cid(&child)).await?);
            }
            Ok(out)
        })
    }

    fn traverse_links_in_order<'a>(&'a self, node: Cid) -> BTreeFuture<'a, Vec<(String, Cid)>> {
        Box::pin(async move {
            let entries = self.tree.list_directory(&node).await?;
            let sorted = sort_entries(entries);
            let mut out = Vec::new();

            if is_leaf_node(&sorted) {
                for entry in sorted {
                    if entry.link_type == LinkType::File {
                        out.push((unescape_key(&entry.name), entry_cid(&entry)));
                    }
                }
                return Ok(out);
            }

            for child in sorted {
                out.extend(self.traverse_links_in_order(entry_cid(&child)).await?);
            }
            Ok(out)
        })
    }

    fn range_traverse<'a>(
        &'a self,
        node: Cid,
        start: Option<String>,
        end: Option<String>,
    ) -> BTreeFuture<'a, Vec<(String, String)>> {
        Box::pin(async move {
            let entries = self.tree.list_directory(&node).await?;
            let sorted = sort_entries(entries);
            let mut out = Vec::new();

            if is_leaf_node(&sorted) {
                for entry in sorted {
                    if entry.link_type != LinkType::Blob {
                        continue;
                    }
                    let key = unescape_key(&entry.name);
                    if start.as_ref().is_some_and(|start| key < *start) {
                        continue;
                    }
                    if end.as_ref().is_some_and(|end| key >= *end) {
                        return Ok(out);
                    }

                    let cid = entry_cid(&entry);
                    if let Some(data) = self.tree.get(&cid, None).await? {
                        out.push((key, String::from_utf8(data)?));
                    }
                }
                return Ok(out);
            }

            for (index, child) in sorted.iter().enumerate() {
                let child_min = unescape_key(&child.name);
                let child_max = sorted.get(index + 1).map(|entry| unescape_key(&entry.name));

                if start.as_ref().is_some_and(|start| {
                    child_max
                        .as_ref()
                        .is_some_and(|child_max| child_max <= start)
                }) {
                    continue;
                }
                if end.as_ref().is_some_and(|end| child_min >= *end) {
                    return Ok(out);
                }

                out.extend(
                    self.range_traverse(entry_cid(child), start.clone(), end.clone())
                        .await?,
                );
            }

            Ok(out)
        })
    }

    fn range_link_traverse<'a>(
        &'a self,
        node: Cid,
        start: Option<String>,
        end: Option<String>,
    ) -> BTreeFuture<'a, Vec<(String, Cid)>> {
        Box::pin(async move {
            let entries = self.tree.list_directory(&node).await?;
            let sorted = sort_entries(entries);
            let mut out = Vec::new();

            if is_leaf_node(&sorted) {
                for entry in sorted {
                    if entry.link_type != LinkType::File {
                        continue;
                    }
                    let key = unescape_key(&entry.name);
                    if start.as_ref().is_some_and(|start| key < *start) {
                        continue;
                    }
                    if end.as_ref().is_some_and(|end| key >= *end) {
                        return Ok(out);
                    }
                    out.push((key, entry_cid(&entry)));
                }
                return Ok(out);
            }

            for (index, child) in sorted.iter().enumerate() {
                let child_min = unescape_key(&child.name);
                let child_max = sorted.get(index + 1).map(|entry| unescape_key(&entry.name));

                if start.as_ref().is_some_and(|start| {
                    child_max
                        .as_ref()
                        .is_some_and(|child_max| child_max <= start)
                }) {
                    continue;
                }
                if end.as_ref().is_some_and(|end| child_min >= *end) {
                    return Ok(out);
                }

                out.extend(
                    self.range_link_traverse(entry_cid(child), start.clone(), end.clone())
                        .await?,
                );
            }

            Ok(out)
        })
    }

    fn range_link_traverse_limited<'a>(
        &'a self,
        node: Cid,
        start: Option<String>,
        end: Option<String>,
        limit: usize,
    ) -> BTreeFuture<'a, Vec<(String, Cid)>> {
        Box::pin(async move {
            let entries = self.tree.list_directory(&node).await?;
            let sorted = sort_entries(entries);
            let mut out = Vec::new();

            if is_leaf_node(&sorted) {
                for entry in sorted {
                    if entry.link_type != LinkType::File {
                        continue;
                    }
                    let key = unescape_key(&entry.name);
                    if start.as_ref().is_some_and(|start| key < *start) {
                        continue;
                    }
                    if end.as_ref().is_some_and(|end| key >= *end) {
                        return Ok(out);
                    }
                    out.push((key, entry_cid(&entry)));
                    if out.len() >= limit {
                        return Ok(out);
                    }
                }
                return Ok(out);
            }

            for (index, child) in sorted.iter().enumerate() {
                let child_min = unescape_key(&child.name);
                let child_max = sorted.get(index + 1).map(|entry| unescape_key(&entry.name));

                if start.as_ref().is_some_and(|start| {
                    child_max
                        .as_ref()
                        .is_some_and(|child_max| child_max <= start)
                }) {
                    continue;
                }
                if end.as_ref().is_some_and(|end| child_min >= *end) {
                    return Ok(out);
                }

                let remaining = limit.saturating_sub(out.len());
                if remaining == 0 {
                    return Ok(out);
                }
                out.extend(
                    self.range_link_traverse_limited(
                        entry_cid(child),
                        start.clone(),
                        end.clone(),
                        remaining,
                    )
                    .await?,
                );
                if out.len() >= limit {
                    return Ok(out);
                }
            }

            Ok(out)
        })
    }

    fn count_links_recursive<'a>(&'a self, node: Cid) -> BTreeFuture<'a, u64> {
        Box::pin(async move {
            let entries = self.tree.list_directory(&node).await?;
            count_link_entries_or_subtrees(self, &entries).await
        })
    }
}

#[derive(Debug, Clone)]
struct InsertResult {
    cid: Cid,
    count: u64,
    split: Option<SplitResult>,
}

pub fn escape_key(key: &str) -> String {
    key.replace('%', "%25")
        .replace('/', "%2F")
        .replace('\0', "%00")
}

pub fn unescape_key(name: &str) -> String {
    name.replace("%2F", "/")
        .replace("%2f", "/")
        .replace("%00", "\0")
        .replace("%25", "%")
}

fn increment_prefix(value: &str) -> Option<String> {
    if value.is_empty() {
        return Some(String::new());
    }

    let mut chars: Vec<char> = value.chars().collect();
    let last = chars.pop()?;
    let next = char::from_u32(last as u32 + 1)?;
    chars.push(next);
    Some(chars.into_iter().collect())
}

fn cid_equals(left: &Cid, right: &Cid) -> bool {
    left.hash == right.hash && left.key == right.key
}

fn is_leaf_node(entries: &[TreeEntry]) -> bool {
    entries.is_empty() || entries.iter().any(|entry| entry.link_type != LinkType::Dir)
}

fn sort_entries(mut entries: Vec<TreeEntry>) -> Vec<TreeEntry> {
    entries.sort_by(|left, right| compare_unescaped_names(&left.name, &right.name));
    entries
}

fn compare_unescaped_names(left: &str, right: &str) -> Ordering {
    unescape_key(left).cmp(&unescape_key(right))
}

fn find_child(entries: &[TreeEntry], key: &str) -> TreeEntry {
    let sorted = sort_entries(entries.to_vec());
    for window in sorted.windows(2) {
        let next_name = unescape_key(&window[1].name);
        if key < next_name.as_str() {
            return window[0].clone();
        }
    }
    sorted
        .last()
        .cloned()
        .expect("internal nodes must have children")
}

fn entry_cid(entry: &TreeEntry) -> Cid {
    Cid {
        hash: entry.hash,
        key: entry.key,
    }
}

fn tree_entry_to_dir_entry(entry: TreeEntry) -> DirEntry {
    let mut out = DirEntry::from_cid(&entry.name, &entry_cid(&entry))
        .with_size(entry.size)
        .with_link_type(entry.link_type);
    if let Some(meta) = entry.meta {
        out = out.with_meta(meta);
    }
    out
}

fn count_link_entries(entries: &[TreeEntry]) -> u64 {
    entries
        .iter()
        .filter(|entry| entry.link_type == LinkType::File)
        .count() as u64
}

fn stored_link_subtree_count(entry: &TreeEntry) -> Option<u64> {
    if entry.link_type != LinkType::Dir || entry.size == 0 {
        return None;
    }
    Some(entry.size)
}

async fn count_link_entries_or_subtrees<S: Store>(
    btree: &BTree<S>,
    entries: &[TreeEntry],
) -> Result<u64, BTreeError> {
    if is_leaf_node(entries) {
        return Ok(count_link_entries(entries));
    }

    let mut count = 0;
    for entry in entries {
        count += match stored_link_subtree_count(entry) {
            Some(child_count) => child_count,
            None => btree.count_links_recursive(entry_cid(entry)).await?,
        };
    }
    Ok(count)
}
