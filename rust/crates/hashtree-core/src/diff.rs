//! Tree diff operations for efficient incremental updates
//!
//! Computes the difference between two merkle trees, identifying which
//! hashes exist in the new tree but not the old tree. This enables
//! efficient push operations where only changed content is uploaded.
//!
//! # Key Optimization: Subtree Pruning
//!
//! When a subtree's root hash matches between old and new trees, the entire
//! subtree is skipped - no need to traverse it since identical hash means
//! identical content.

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::codec::{decode_tree_node, is_tree_node};
use crate::crypto::decrypt_chk;
use crate::hashtree::{HashTree, HashTreeError};
use crate::store::Store;
use crate::types::{Cid, Hash};

/// Result of a tree diff operation
#[derive(Debug, Clone)]
pub struct TreeDiff {
    /// Hashes present in new tree but not in old tree (need upload)
    pub added: Vec<Hash>,
    /// Statistics about the diff operation
    pub stats: DiffStats,
}

impl TreeDiff {
    /// Create an empty diff (identical trees)
    pub fn empty() -> Self {
        Self {
            added: Vec::new(),
            stats: DiffStats::default(),
        }
    }

    /// Check if there are any changes
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
    }

    /// Number of hashes to upload
    pub fn added_count(&self) -> usize {
        self.added.len()
    }
}

/// Statistics from the diff operation
#[derive(Debug, Clone, Default)]
pub struct DiffStats {
    /// Number of nodes visited in old tree
    pub old_tree_nodes: usize,
    /// Number of nodes visited in new tree
    pub new_tree_nodes: usize,
    /// Number of subtrees skipped due to hash match
    pub unchanged_subtrees: usize,
}

/// Collect all hashes in a tree using parallel traversal
///
/// This walks the tree and collects all unique hashes (both tree nodes and blobs).
/// Uses parallel fetching for efficiency.
///
/// # Arguments
/// * `tree` - The HashTree instance with store access
/// * `root` - Root CID to start from
/// * `concurrency` - Number of concurrent fetches
///
/// # Returns
/// Set of all hashes in the tree
pub async fn collect_hashes<S: Store>(
    tree: &HashTree<S>,
    root: &Cid,
    concurrency: usize,
) -> Result<HashSet<Hash>, HashTreeError> {
    collect_hashes_with_progress(tree, root, concurrency, None).await
}

/// Collect hashes with optional progress tracking
pub async fn collect_hashes_with_progress<S: Store>(
    tree: &HashTree<S>,
    root: &Cid,
    concurrency: usize,
    progress: Option<&AtomicUsize>,
) -> Result<HashSet<Hash>, HashTreeError> {
    use futures::stream::{FuturesUnordered, StreamExt};
    use std::collections::VecDeque;

    let store = tree.get_store();
    let mut hashes = HashSet::new();
    let mut pending: VecDeque<(Hash, Option<[u8; 32]>)> = VecDeque::new();
    let mut active = FuturesUnordered::new();

    // Seed with root
    pending.push_back((root.hash, root.key));

    loop {
        // Fill up to concurrency limit
        while active.len() < concurrency {
            if let Some((hash, key)) = pending.pop_front() {
                // Skip if already visited
                if hashes.contains(&hash) {
                    continue;
                }
                hashes.insert(hash);

                let store = store.clone();
                let fut = async move {
                    let data = store
                        .get(&hash)
                        .await
                        .map_err(|e| HashTreeError::Store(e.to_string()))?;
                    Ok::<_, HashTreeError>((hash, key, data))
                };
                active.push(fut);
            } else {
                break;
            }
        }

        // If nothing active, we're done
        if active.is_empty() {
            break;
        }

        // Wait for any future to complete
        if let Some(result) = active.next().await {
            let (_hash, key, data) = result?;

            if let Some(counter) = progress {
                counter.fetch_add(1, Ordering::Relaxed);
            }

            let data = match data {
                Some(d) => d,
                None => continue,
            };

            // Decrypt if key present
            let plaintext = if let Some(k) = key {
                decrypt_chk(&data, &k).unwrap_or(data)
            } else {
                data
            };

            // If it's a tree node, queue children
            if is_tree_node(&plaintext) {
                if let Ok(node) = decode_tree_node(&plaintext) {
                    for link in node.links {
                        if !hashes.contains(&link.hash) {
                            pending.push_back((link.hash, link.key));
                        }
                    }
                }
            }
        }
    }

    Ok(hashes)
}

/// Compute diff between two trees
///
/// Returns hashes present in `new_root` but not in `old_root`.
/// Uses subtree pruning: when a hash exists in old tree, skips entire subtree.
///
/// # Algorithm
/// 1. Collect all hashes from old tree into a set
/// 2. Walk new tree:
///    - If hash in old set: skip (subtree unchanged)
///    - If hash not in old set: add to result, traverse children
///
/// # Arguments
/// * `tree` - HashTree instance
/// * `old_root` - Root of the old tree (may be None for first push)
/// * `new_root` - Root of the new tree
/// * `concurrency` - Number of concurrent fetches
///
/// # Returns
/// TreeDiff with added hashes and statistics
pub async fn tree_diff<S: Store>(
    tree: &HashTree<S>,
    old_root: Option<&Cid>,
    new_root: &Cid,
    concurrency: usize,
) -> Result<TreeDiff, HashTreeError> {
    // No old tree = everything is new
    let old_hashes = if let Some(old) = old_root {
        collect_hashes(tree, old, concurrency).await?
    } else {
        HashSet::new()
    };

    tree_diff_with_old_hashes(tree, &old_hashes, new_root, concurrency).await
}

/// Compute diff given pre-computed old hashes
///
/// Use this when you already have the old tree's hash set (e.g., from a previous operation)
pub async fn tree_diff_with_old_hashes<S: Store>(
    tree: &HashTree<S>,
    old_hashes: &HashSet<Hash>,
    new_root: &Cid,
    concurrency: usize,
) -> Result<TreeDiff, HashTreeError> {
    use futures::stream::{FuturesUnordered, StreamExt};
    use std::collections::VecDeque;

    let store = tree.get_store();
    let mut added: Vec<Hash> = Vec::new();
    let mut visited: HashSet<Hash> = HashSet::new();
    let mut pending: VecDeque<(Hash, Option<[u8; 32]>)> = VecDeque::new();
    let mut active = FuturesUnordered::new();

    let mut stats = DiffStats {
        old_tree_nodes: old_hashes.len(),
        new_tree_nodes: 0,
        unchanged_subtrees: 0,
    };

    // Seed with new root
    pending.push_back((new_root.hash, new_root.key));

    loop {
        // Fill up to concurrency limit
        while active.len() < concurrency {
            if let Some((hash, key)) = pending.pop_front() {
                // Skip if already visited
                if visited.contains(&hash) {
                    continue;
                }
                visited.insert(hash);

                // KEY OPTIMIZATION: If hash exists in old tree, skip entire subtree
                if old_hashes.contains(&hash) {
                    stats.unchanged_subtrees += 1;
                    continue;
                }

                // Hash is new - will need to upload
                added.push(hash);
                stats.new_tree_nodes += 1;

                // Fetch to check for children
                let store = store.clone();
                let fut = async move {
                    let data = store
                        .get(&hash)
                        .await
                        .map_err(|e| HashTreeError::Store(e.to_string()))?;
                    Ok::<_, HashTreeError>((hash, key, data))
                };
                active.push(fut);
            } else {
                break;
            }
        }

        // If nothing active, we're done
        if active.is_empty() {
            break;
        }

        // Wait for any future to complete
        if let Some(result) = active.next().await {
            let (_hash, key, data) = result?;

            let data = match data {
                Some(d) => d,
                None => continue,
            };

            // Decrypt if key present
            let plaintext = if let Some(k) = key {
                decrypt_chk(&data, &k).unwrap_or(data)
            } else {
                data
            };

            // If it's a tree node, queue children
            if is_tree_node(&plaintext) {
                if let Ok(node) = decode_tree_node(&plaintext) {
                    for link in node.links {
                        if !visited.contains(&link.hash) {
                            pending.push_back((link.hash, link.key));
                        }
                    }
                }
            }
        }
    }

    Ok(TreeDiff { added, stats })
}

/// Streaming diff - yields hashes as they're discovered
///
/// Memory efficient for very large trees. Yields hashes that need upload
/// one at a time instead of collecting into a Vec.
pub async fn tree_diff_streaming<S, F>(
    tree: &HashTree<S>,
    old_hashes: &HashSet<Hash>,
    new_root: &Cid,
    concurrency: usize,
    mut callback: F,
) -> Result<DiffStats, HashTreeError>
where
    S: Store,
    F: FnMut(Hash) -> bool, // return false to stop early
{
    use futures::stream::{FuturesUnordered, StreamExt};
    use std::collections::VecDeque;

    let store = tree.get_store();
    let mut visited: HashSet<Hash> = HashSet::new();
    let mut pending: VecDeque<(Hash, Option<[u8; 32]>)> = VecDeque::new();
    let mut active = FuturesUnordered::new();

    let mut stats = DiffStats {
        old_tree_nodes: old_hashes.len(),
        new_tree_nodes: 0,
        unchanged_subtrees: 0,
    };

    pending.push_back((new_root.hash, new_root.key));

    loop {
        while active.len() < concurrency {
            if let Some((hash, key)) = pending.pop_front() {
                if visited.contains(&hash) {
                    continue;
                }
                visited.insert(hash);

                if old_hashes.contains(&hash) {
                    stats.unchanged_subtrees += 1;
                    continue;
                }

                stats.new_tree_nodes += 1;

                // Yield this hash via callback
                if !callback(hash) {
                    // Early termination requested
                    return Ok(stats);
                }

                let store = store.clone();
                let fut = async move {
                    let data = store
                        .get(&hash)
                        .await
                        .map_err(|e| HashTreeError::Store(e.to_string()))?;
                    Ok::<_, HashTreeError>((hash, key, data))
                };
                active.push(fut);
            } else {
                break;
            }
        }

        if active.is_empty() {
            break;
        }

        if let Some(result) = active.next().await {
            let (_hash, key, data) = result?;

            let data = match data {
                Some(d) => d,
                None => continue,
            };

            let plaintext = if let Some(k) = key {
                decrypt_chk(&data, &k).unwrap_or(data)
            } else {
                data
            };

            if is_tree_node(&plaintext) {
                if let Ok(node) = decode_tree_node(&plaintext) {
                    for link in node.links {
                        if !visited.contains(&link.hash) {
                            pending.push_back((link.hash, link.key));
                        }
                    }
                }
            }
        }
    }

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemoryStore;
    use crate::types::{DirEntry, LinkType};
    use crate::HashTreeConfig;
    use std::sync::Arc;

    fn make_tree() -> (Arc<MemoryStore>, HashTree<MemoryStore>) {
        let store = Arc::new(MemoryStore::new());
        let tree = HashTree::new(HashTreeConfig::new(store.clone()).public());
        (store, tree)
    }

    fn make_encrypted_tree() -> (Arc<MemoryStore>, HashTree<MemoryStore>) {
        let store = Arc::new(MemoryStore::new());
        let tree = HashTree::new(HashTreeConfig::new(store.clone()));
        (store, tree)
    }

    #[tokio::test]
    async fn test_diff_identical_trees() {
        let (_store, tree) = make_tree();

        // Create a simple tree
        let file1 = tree.put_blob(b"content1").await.unwrap();
        let file2 = tree.put_blob(b"content2").await.unwrap();
        let dir_cid = tree
            .put_directory(vec![
                DirEntry::new("a.txt", file1).with_size(8),
                DirEntry::new("b.txt", file2).with_size(8),
            ])
            .await
            .unwrap();

        // Diff tree against itself should be empty
        let diff = tree_diff(&tree, Some(&dir_cid), &dir_cid, 4).await.unwrap();

        assert!(diff.is_empty(), "identical trees should have empty diff");
        assert_eq!(diff.added_count(), 0);
    }

    #[tokio::test]
    async fn test_diff_single_file_change() {
        let (_store, tree) = make_tree();

        // Create old tree
        let file1 = tree.put_blob(b"content1").await.unwrap();
        let file2 = tree.put_blob(b"content2").await.unwrap();
        let old_dir = tree
            .put_directory(vec![
                DirEntry::new("a.txt", file1).with_size(8),
                DirEntry::new("b.txt", file2).with_size(8),
            ])
            .await
            .unwrap();

        // Create new tree with one file changed
        let file1_new = tree.put_blob(b"content1-modified").await.unwrap();
        let new_dir = tree
            .put_directory(vec![
                DirEntry::new("a.txt", file1_new).with_size(17),
                DirEntry::new("b.txt", file2).with_size(8), // unchanged
            ])
            .await
            .unwrap();

        let diff = tree_diff(&tree, Some(&old_dir), &new_dir, 4).await.unwrap();

        // Should have: new root dir + new file blob
        assert!(!diff.is_empty());
        assert_eq!(diff.added_count(), 2); // new dir node + new file
        assert!(diff.added.contains(&file1_new));
        assert!(diff.added.contains(&new_dir.hash));
        assert!(!diff.added.contains(&file2)); // file2 unchanged
    }

    #[tokio::test]
    async fn test_diff_subtree_unchanged() {
        let (_store, tree) = make_tree();

        // Create subdirectory
        let sub_file = tree.put_blob(b"sub content").await.unwrap();
        let subdir = tree
            .put_directory(vec![DirEntry::new("sub.txt", sub_file).with_size(11)])
            .await
            .unwrap();

        // Old tree
        let file1 = tree.put_blob(b"root file").await.unwrap();
        let old_root = tree
            .put_directory(vec![
                DirEntry::new("file.txt", file1).with_size(9),
                DirEntry::new("subdir", subdir.hash).with_size(0).with_link_type(LinkType::Dir),
            ])
            .await
            .unwrap();

        // New tree - change root file, subdir unchanged
        let file1_new = tree.put_blob(b"root file changed").await.unwrap();
        let new_root = tree
            .put_directory(vec![
                DirEntry::new("file.txt", file1_new).with_size(17),
                DirEntry::new("subdir", subdir.hash).with_size(0).with_link_type(LinkType::Dir),
            ])
            .await
            .unwrap();

        let diff = tree_diff(&tree, Some(&old_root), &new_root, 4).await.unwrap();

        // Should NOT include subdir or its contents
        assert!(diff.added.contains(&new_root.hash));
        assert!(diff.added.contains(&file1_new));
        assert!(!diff.added.contains(&subdir.hash)); // subdir unchanged
        assert!(!diff.added.contains(&sub_file)); // subdir content unchanged

        // Stats should show subtree was skipped
        assert!(diff.stats.unchanged_subtrees > 0);
    }

    #[tokio::test]
    async fn test_diff_new_directory() {
        let (_store, tree) = make_tree();

        // Old tree - simple
        let file1 = tree.put_blob(b"file1").await.unwrap();
        let old_root = tree
            .put_directory(vec![DirEntry::new("a.txt", file1).with_size(5)])
            .await
            .unwrap();

        // New tree - add directory
        let new_file = tree.put_blob(b"new file").await.unwrap();
        let new_dir = tree
            .put_directory(vec![DirEntry::new("inner.txt", new_file).with_size(8)])
            .await
            .unwrap();
        let new_root = tree
            .put_directory(vec![
                DirEntry::new("a.txt", file1).with_size(5),
                DirEntry::new("newdir", new_dir.hash).with_size(0).with_link_type(LinkType::Dir),
            ])
            .await
            .unwrap();

        let diff = tree_diff(&tree, Some(&old_root), &new_root, 4).await.unwrap();

        // Should include new root, new dir, and new file
        assert!(diff.added.contains(&new_root.hash));
        assert!(diff.added.contains(&new_dir.hash));
        assert!(diff.added.contains(&new_file));
        // Original file should NOT be in diff
        assert!(!diff.added.contains(&file1));
    }

    #[tokio::test]
    async fn test_diff_empty_old_tree() {
        let (_store, tree) = make_tree();

        // New tree
        let file1 = tree.put_blob(b"content").await.unwrap();
        let new_root = tree
            .put_directory(vec![DirEntry::new("file.txt", file1).with_size(7)])
            .await
            .unwrap();

        // No old tree (first push)
        let diff = tree_diff(&tree, None, &new_root, 4).await.unwrap();

        // Everything should be new
        assert_eq!(diff.added_count(), 2); // root + file
        assert!(diff.added.contains(&new_root.hash));
        assert!(diff.added.contains(&file1));
    }

    #[tokio::test]
    async fn test_diff_encrypted_trees() {
        let (_store, tree) = make_encrypted_tree();

        // Create old tree (encrypted)
        let (file1_cid, _) = tree.put(b"content1").await.unwrap();
        let (file2_cid, _) = tree.put(b"content2").await.unwrap();
        let old_dir = tree
            .put_directory(vec![
                DirEntry::from_cid("a.txt", &file1_cid).with_size(8),
                DirEntry::from_cid("b.txt", &file2_cid).with_size(8),
            ])
            .await
            .unwrap();

        // Create new tree with one file changed
        let (file1_new_cid, _) = tree.put(b"content1-modified").await.unwrap();
        let new_dir = tree
            .put_directory(vec![
                DirEntry::from_cid("a.txt", &file1_new_cid).with_size(17),
                DirEntry::from_cid("b.txt", &file2_cid).with_size(8),
            ])
            .await
            .unwrap();

        let diff = tree_diff(&tree, Some(&old_dir), &new_dir, 4).await.unwrap();

        // Should work with encrypted content
        assert!(!diff.is_empty());
        assert!(diff.added.contains(&file1_new_cid.hash));
        assert!(!diff.added.contains(&file2_cid.hash)); // unchanged
    }

    #[tokio::test]
    async fn test_collect_hashes() {
        let (_store, tree) = make_tree();

        let file1 = tree.put_blob(b"content1").await.unwrap();
        let file2 = tree.put_blob(b"content2").await.unwrap();
        let dir_cid = tree
            .put_directory(vec![
                DirEntry::new("a.txt", file1).with_size(8),
                DirEntry::new("b.txt", file2).with_size(8),
            ])
            .await
            .unwrap();

        let hashes = collect_hashes(&tree, &dir_cid, 4).await.unwrap();

        assert_eq!(hashes.len(), 3); // dir + 2 files
        assert!(hashes.contains(&dir_cid.hash));
        assert!(hashes.contains(&file1));
        assert!(hashes.contains(&file2));
    }

    #[tokio::test]
    async fn test_diff_streaming() {
        let (_store, tree) = make_tree();

        let file1 = tree.put_blob(b"old").await.unwrap();
        let old_root = tree
            .put_directory(vec![DirEntry::new("a.txt", file1).with_size(3)])
            .await
            .unwrap();

        let file2 = tree.put_blob(b"new").await.unwrap();
        let new_root = tree
            .put_directory(vec![DirEntry::new("a.txt", file2).with_size(3)])
            .await
            .unwrap();

        let old_hashes = collect_hashes(&tree, &old_root, 4).await.unwrap();

        let mut streamed: Vec<Hash> = Vec::new();
        let stats = tree_diff_streaming(&tree, &old_hashes, &new_root, 4, |hash| {
            streamed.push(hash);
            true // continue
        })
        .await
        .unwrap();

        assert_eq!(streamed.len(), 2); // new root + new file
        assert!(streamed.contains(&new_root.hash));
        assert!(streamed.contains(&file2));
        assert_eq!(stats.new_tree_nodes, 2);
    }

    #[tokio::test]
    async fn test_diff_streaming_early_stop() {
        let (_store, tree) = make_tree();

        let file1 = tree.put_blob(b"f1").await.unwrap();
        let file2 = tree.put_blob(b"f2").await.unwrap();
        let file3 = tree.put_blob(b"f3").await.unwrap();
        let new_root = tree
            .put_directory(vec![
                DirEntry::new("a.txt", file1).with_size(2),
                DirEntry::new("b.txt", file2).with_size(2),
                DirEntry::new("c.txt", file3).with_size(2),
            ])
            .await
            .unwrap();

        let old_hashes = HashSet::new(); // empty = all new

        let mut count = 0;
        let _stats = tree_diff_streaming(&tree, &old_hashes, &new_root, 1, |_hash| {
            count += 1;
            count < 2 // stop after 2
        })
        .await
        .unwrap();

        assert!(count <= 2, "should have stopped early");
    }

    #[tokio::test]
    async fn test_diff_large_tree_structure() {
        let (_store, tree) = make_tree();

        // Create a tree with many files
        let mut entries = Vec::new();
        let mut old_hashes_vec = Vec::new();

        for i in 0..100 {
            let data = format!("content {}", i);
            let hash = tree.put_blob(data.as_bytes()).await.unwrap();
            entries.push(DirEntry::new(format!("file{}.txt", i), hash).with_size(data.len() as u64));
            old_hashes_vec.push(hash);
        }

        let old_root = tree.put_directory(entries.clone()).await.unwrap();
        old_hashes_vec.push(old_root.hash);

        // Modify 5 files
        for i in 0..5 {
            let data = format!("modified content {}", i);
            let hash = tree.put_blob(data.as_bytes()).await.unwrap();
            entries[i] = DirEntry::new(format!("file{}.txt", i), hash).with_size(data.len() as u64);
        }

        let new_root = tree.put_directory(entries).await.unwrap();

        let diff = tree_diff(&tree, Some(&old_root), &new_root, 8).await.unwrap();

        // Should have 6 new items: new root + 5 modified files
        assert_eq!(diff.added_count(), 6);
        assert!(diff.added.contains(&new_root.hash));

        // Should have skipped ~95 unchanged files
        assert!(diff.stats.unchanged_subtrees >= 95);
    }
}
