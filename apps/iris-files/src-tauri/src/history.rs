//! History storage and search using heed (LMDB)
//!
//! Stores navigation history for fuzzy search suggestions.
//! Uses heed for fast KV storage with LMDB backend.

use heed::types::{Bytes, Str};
use heed::{Database, Env, EnvOpenOptions};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use tracing::debug;

/// Maximum number of history entries to store
const MAX_HISTORY_ENTRIES: usize = 1000;

/// History entry stored in the database
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub path: String,
    pub label: String,
    pub entry_type: String, // "tree" | "file" | "video" | "user" | "app" | "hash"
    pub npub: Option<String>,
    pub tree_name: Option<String>,
    pub visit_count: u32,
    pub last_visited: u64, // Unix timestamp ms
    pub first_visited: u64,
}

/// Search result returned to frontend
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistorySearchResult {
    pub entry: HistoryEntry,
    pub score: f64,
}

/// History store using heed/LMDB
pub struct HistoryStore {
    env: Env,
    db: Database<Str, Bytes>,
    entry_count: RwLock<usize>,
}

impl HistoryStore {
    /// Open or create the history database
    pub fn new(data_dir: &Path) -> Result<Self, String> {
        let history_dir = data_dir.join("history");
        std::fs::create_dir_all(&history_dir)
            .map_err(|e| format!("Failed to create history dir: {}", e))?;

        // Open LMDB environment
        let env = unsafe {
            EnvOpenOptions::new()
                .map_size(10 * 1024 * 1024) // 10MB should be plenty for history
                .max_dbs(1)
                .open(&history_dir)
                .map_err(|e| format!("Failed to open history db: {}", e))?
        };
        if let Ok(cleared) = env.clear_stale_readers() {
            if cleared > 0 {
                debug!("Cleared {} stale LMDB readers for history store", cleared);
            }
        }

        // Open the history database
        let mut wtxn = env
            .write_txn()
            .map_err(|e| format!("Failed to start txn: {}", e))?;
        let db = env
            .create_database(&mut wtxn, Some("history"))
            .map_err(|e| format!("Failed to create db: {}", e))?;
        wtxn.commit()
            .map_err(|e| format!("Failed to commit: {}", e))?;

        // Count existing entries
        let count = {
            let rtxn = env
                .read_txn()
                .map_err(|e| format!("Failed to start read txn: {}", e))?;
            db.len(&rtxn).unwrap_or(0) as usize
        };

        Ok(Self {
            env,
            db,
            entry_count: RwLock::new(count),
        })
    }

    /// Record a history visit (insert or update)
    pub fn record_visit(&self, entry: HistoryEntry) -> Result<(), String> {
        let mut wtxn = self
            .env
            .write_txn()
            .map_err(|e| format!("Failed to start write txn: {}", e))?;

        // Check if entry exists
        let existing: Option<HistoryEntry> = self
            .db
            .get(&wtxn, &entry.path)
            .map_err(|e| format!("Failed to get: {}", e))?
            .and_then(|bytes| bincode::deserialize(bytes).ok());

        let updated_entry = if let Some(mut existing) = existing {
            // Update existing entry
            existing.label = entry.label;
            existing.visit_count += 1;
            existing.last_visited = entry.last_visited;
            existing
        } else {
            // Check if we need to evict old entries
            let count = *self.entry_count.read();
            if count >= MAX_HISTORY_ENTRIES {
                self.evict_oldest(&mut wtxn)?;
            }
            *self.entry_count.write() += 1;
            entry
        };

        let bytes =
            bincode::serialize(&updated_entry).map_err(|e| format!("Failed to serialize: {}", e))?;

        self.db
            .put(&mut wtxn, &updated_entry.path, &bytes)
            .map_err(|e| format!("Failed to put: {}", e))?;

        wtxn.commit()
            .map_err(|e| format!("Failed to commit: {}", e))?;

        debug!("Recorded history visit: {}", updated_entry.path);
        Ok(())
    }

    /// Evict oldest entries when at capacity
    fn evict_oldest(&self, wtxn: &mut heed::RwTxn) -> Result<(), String> {
        // Collect all entries with timestamps
        let rtxn = self
            .env
            .read_txn()
            .map_err(|e| format!("Failed to read: {}", e))?;

        let mut entries: Vec<(String, u64)> = Vec::new();
        let iter = self
            .db
            .iter(&rtxn)
            .map_err(|e| format!("Failed to iterate: {}", e))?;

        for item in iter {
            let (key, value) = item.map_err(|e| format!("Iter error: {}", e))?;
            if let Ok(entry) = bincode::deserialize::<HistoryEntry>(value) {
                entries.push((key.to_string(), entry.last_visited));
            }
        }
        drop(rtxn);

        // Sort by last_visited ascending (oldest first)
        entries.sort_by_key(|(_, ts)| *ts);

        // Remove oldest 10%
        let to_remove = entries.len() / 10;
        for (path, _) in entries.into_iter().take(to_remove.max(1)) {
            self.db
                .delete(wtxn, &path)
                .map_err(|e| format!("Failed to delete: {}", e))?;
            *self.entry_count.write() -= 1;
        }

        Ok(())
    }

    /// Search history with fuzzy matching
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<HistorySearchResult>, String> {
        if query.is_empty() {
            return Ok(Vec::new());
        }

        let rtxn = self
            .env
            .read_txn()
            .map_err(|e| format!("Failed to start read txn: {}", e))?;

        let query_lower = query.to_lowercase();
        let mut results: Vec<HistorySearchResult> = Vec::new();

        let iter = self
            .db
            .iter(&rtxn)
            .map_err(|e| format!("Failed to iterate: {}", e))?;

        for item in iter {
            let (_key, value) = item.map_err(|e| format!("Iter error: {}", e))?;
            if let Ok(entry) = bincode::deserialize::<HistoryEntry>(value) {
                // Calculate fuzzy score
                let score = fuzzy_score(&query_lower, &entry);
                if score > 0.0 {
                    results.push(HistorySearchResult { entry, score });
                }
            }
        }

        // Sort by score descending, then by recency
        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.entry.last_visited.cmp(&a.entry.last_visited))
        });

        results.truncate(limit);
        Ok(results)
    }

    /// Get recent history entries (no search, just recency)
    pub fn get_recent(&self, limit: usize) -> Result<Vec<HistoryEntry>, String> {
        let rtxn = self
            .env
            .read_txn()
            .map_err(|e| format!("Failed to start read txn: {}", e))?;

        let mut entries: Vec<HistoryEntry> = Vec::new();

        let iter = self
            .db
            .iter(&rtxn)
            .map_err(|e| format!("Failed to iterate: {}", e))?;

        for item in iter {
            let (_key, value) = item.map_err(|e| format!("Iter error: {}", e))?;
            if let Ok(entry) = bincode::deserialize::<HistoryEntry>(value) {
                entries.push(entry);
            }
        }

        // Sort by last_visited descending
        entries.sort_by(|a, b| b.last_visited.cmp(&a.last_visited));
        entries.truncate(limit);

        Ok(entries)
    }
}

/// Calculate fuzzy match score for a history entry
/// Returns 0.0 for no match, higher scores for better matches
fn fuzzy_score(query: &str, entry: &HistoryEntry) -> f64 {
    let mut max_score: f64 = 0.0;

    // Score against label (highest weight)
    let label_lower = entry.label.to_lowercase();
    max_score = max_score.max(fuzzy_match_string(query, &label_lower) * 1.0);

    // Score against path
    let path_lower = entry.path.to_lowercase();
    max_score = max_score.max(fuzzy_match_string(query, &path_lower) * 0.8);

    // Score against tree_name if present
    if let Some(ref tree_name) = entry.tree_name {
        let tree_lower = tree_name.to_lowercase();
        max_score = max_score.max(fuzzy_match_string(query, &tree_lower) * 0.7);
    }

    // Boost by visit frequency (log scale)
    let freq_boost = (entry.visit_count as f64).ln_1p() * 0.1;

    max_score + freq_boost
}

/// Fuzzy match a query against a target string
/// Uses subsequence matching with bonuses for consecutive/word-boundary matches
fn fuzzy_match_string(query: &str, target: &str) -> f64 {
    if query.is_empty() || target.is_empty() {
        return 0.0;
    }

    // Exact match
    if target == query {
        return 10.0;
    }

    // Prefix match
    if target.starts_with(query) {
        return 8.0 + (query.len() as f64 / target.len() as f64);
    }

    // Contains match
    if target.contains(query) {
        return 5.0 + (query.len() as f64 / target.len() as f64);
    }

    // Word prefix match (any word starts with query)
    for word in target.split(|c: char| !c.is_alphanumeric()) {
        if word.starts_with(query) {
            return 4.0 + (query.len() as f64 / word.len() as f64);
        }
    }

    // Subsequence match with scoring
    let query_chars: Vec<char> = query.chars().collect();
    let target_chars: Vec<char> = target.chars().collect();

    let mut query_idx = 0;
    let mut score = 0.0;
    let mut prev_match_idx: Option<usize> = None;

    for (target_idx, &target_char) in target_chars.iter().enumerate() {
        if query_idx < query_chars.len() && target_char == query_chars[query_idx] {
            // Bonus for consecutive matches
            if let Some(prev) = prev_match_idx {
                if target_idx == prev + 1 {
                    score += 0.5;
                }
            }

            // Bonus for word boundary
            if target_idx == 0
                || !target_chars[target_idx - 1].is_alphanumeric()
            {
                score += 0.3;
            }

            score += 0.2;
            prev_match_idx = Some(target_idx);
            query_idx += 1;
        }
    }

    // Only return score if all query chars were matched
    if query_idx == query_chars.len() {
        score
    } else {
        0.0
    }
}

// ============================================================================
// Tauri Commands
// ============================================================================

/// Record a history visit
#[tauri::command]
pub fn record_history_visit(
    path: String,
    label: String,
    entry_type: String,
    npub: Option<String>,
    tree_name: Option<String>,
    history: tauri::State<'_, Arc<HistoryStore>>,
) -> Result<(), String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let entry = HistoryEntry {
        path,
        label,
        entry_type,
        npub,
        tree_name,
        visit_count: 1,
        last_visited: now,
        first_visited: now,
    };

    history.record_visit(entry)
}

/// Search history with fuzzy matching
#[tauri::command]
pub fn search_history(
    query: String,
    limit: usize,
    history: tauri::State<'_, Arc<HistoryStore>>,
) -> Result<Vec<HistorySearchResult>, String> {
    history.search(&query, limit)
}

/// Get recent history entries
#[tauri::command]
pub fn get_recent_history(
    limit: usize,
    history: tauri::State<'_, Arc<HistoryStore>>,
) -> Result<Vec<HistoryEntry>, String> {
    history.get_recent(limit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_fuzzy_match_exact() {
        assert!(fuzzy_match_string("hello", "hello") > 9.0);
    }

    #[test]
    fn test_fuzzy_match_prefix() {
        assert!(fuzzy_match_string("hel", "hello") > 7.0);
    }

    #[test]
    fn test_fuzzy_match_contains() {
        assert!(fuzzy_match_string("ell", "hello") > 4.0);
    }

    #[test]
    fn test_fuzzy_match_subsequence() {
        let score = fuzzy_match_string("hlo", "hello");
        assert!(score > 0.0, "subsequence should match");
    }

    #[test]
    fn test_fuzzy_match_no_match() {
        assert_eq!(fuzzy_match_string("xyz", "hello"), 0.0);
    }

    #[test]
    fn test_history_store_basic() {
        let dir = tempdir().unwrap();
        let store = HistoryStore::new(dir.path()).unwrap();

        let entry = HistoryEntry {
            path: "/test/path".to_string(),
            label: "Test Entry".to_string(),
            entry_type: "tree".to_string(),
            npub: None,
            tree_name: Some("test".to_string()),
            visit_count: 1,
            last_visited: 1234567890,
            first_visited: 1234567890,
        };

        store.record_visit(entry).unwrap();

        let results = store.search("test", 10).unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].entry.path, "/test/path");
    }

    #[test]
    fn test_history_visit_count() {
        let dir = tempdir().unwrap();
        let store = HistoryStore::new(dir.path()).unwrap();

        let entry = HistoryEntry {
            path: "/test".to_string(),
            label: "Test".to_string(),
            entry_type: "tree".to_string(),
            npub: None,
            tree_name: None,
            visit_count: 1,
            last_visited: 1000,
            first_visited: 1000,
        };

        store.record_visit(entry.clone()).unwrap();
        store.record_visit(entry.clone()).unwrap();
        store.record_visit(entry).unwrap();

        let recent = store.get_recent(10).unwrap();
        assert_eq!(recent[0].visit_count, 3);
    }
}
