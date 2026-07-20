use anyhow::{Context, Result};
use heed::{CompactionOption, EnvOpenOptions};
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::managed_env::ManagedEnv;
#[cfg(feature = "s3")]
use std::sync::Arc;
#[cfg(feature = "s3")]
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::{GcStats, HashtreeStore};

#[cfg(feature = "s3")]
use futures::{stream::FuturesUnordered, StreamExt};
#[cfg(feature = "s3")]
use hashtree_core::from_hex;
use hashtree_core::{sha256, to_hex};
use serde::{Deserialize, Serialize};

/// Result of blob integrity verification
#[derive(Debug, Clone)]
pub struct VerifyResult {
    pub total: usize,
    pub valid: usize,
    pub corrupted: usize,
    pub deleted: usize,
}

#[derive(Debug, Clone)]
pub struct CompactResult {
    pub env_dir: PathBuf,
    pub before_bytes: u64,
    pub after_bytes: u64,
    pub backup_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub struct R2ImportOptions {
    pub concurrency: usize,
    pub check_only: bool,
    pub resume: bool,
    pub fast_list: bool,
    pub stream_merge: bool,
    pub keys: Vec<String>,
    pub keys_file: Option<PathBuf>,
    pub start_after: Option<String>,
    pub scan_prefix: Option<String>,
    pub state_file: Option<PathBuf>,
    pub max_objects: Option<usize>,
    pub progress_every: usize,
    pub scan_delay_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct R2ImportResult {
    pub listed: usize,
    pub skipped: usize,
    pub missing: usize,
    pub imported: usize,
    pub corrupted: usize,
    pub failed: usize,
    pub bytes_imported: u64,
    pub last_key: Option<String>,
    pub completed: bool,
}

#[cfg(feature = "s3")]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct R2ImportState {
    #[serde(flatten)]
    result: R2ImportResult,
    updated_at_unix: u64,
}

#[cfg(feature = "s3")]
#[derive(Debug, Clone)]
struct R2ObjectCandidate {
    key: String,
    hash: hashtree_core::types::Hash,
}

#[cfg(feature = "s3")]
#[derive(Debug, Clone, Default)]
struct R2ObjectImportOutcome {
    skipped: bool,
    missing: bool,
    imported: bool,
    corrupted: bool,
    failed: bool,
    bytes_imported: u64,
    message: Option<String>,
}

#[cfg(feature = "s3")]
const R2_IMPORT_OBJECT_READ_ATTEMPTS: usize = 4;
#[cfg(feature = "s3")]
const R2_IMPORT_OBJECT_RETRY_BASE_DELAY_MS: u64 = 250;

const COMPACT_MAX_DBS: u32 = 64;
const COMPACT_MAX_READERS: u32 = 2048;
const COMPACT_OPEN_MAP_SIZE_BYTES: usize = 10 * 1024 * 1024;
const COMPACT_PAGE_SIZE_BYTES: u64 = 4096;

#[cfg(feature = "s3")]
fn unix_timestamp_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(feature = "s3")]
fn r2_import_key_hash(prefix: &str, key: &str) -> Option<hashtree_core::types::Hash> {
    let filename = key.strip_prefix(prefix).unwrap_or(key);
    let hash_hex = filename.strip_suffix(".bin")?;
    if hash_hex.contains('/') {
        return None;
    }
    if hash_hex.len() != 64 {
        return None;
    }
    from_hex(hash_hex).ok()
}

#[cfg(feature = "s3")]
fn r2_import_key_candidate(prefix: &str, input: &str) -> Option<R2ObjectCandidate> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }

    let key = if input.len() == 64 && input.chars().all(|ch| ch.is_ascii_hexdigit()) {
        format!("{prefix}{input}.bin")
    } else if !prefix.is_empty() && !input.starts_with(prefix) && !input.contains('/') {
        format!("{prefix}{input}")
    } else {
        input.to_string()
    };

    let hash = r2_import_key_hash(prefix, &key)?;
    Some(R2ObjectCandidate { key, hash })
}

#[cfg(feature = "s3")]
fn existing_r2_candidates(
    local: &super::LocalStore,
    candidates: &[R2ObjectCandidate],
) -> Result<Vec<bool>> {
    let mut indexed_hashes: Vec<(usize, hashtree_core::types::Hash)> = candidates
        .iter()
        .enumerate()
        .map(|(index, candidate)| (index, candidate.hash))
        .collect();
    indexed_hashes.sort_unstable_by(|left, right| left.1.cmp(&right.1).then(left.0.cmp(&right.0)));

    let sorted_hashes: Vec<hashtree_core::types::Hash> =
        indexed_hashes.iter().map(|(_, hash)| *hash).collect();
    let sorted_existing = local
        .existing_hashes_in_sorted_candidates(&sorted_hashes)
        .map_err(|err| anyhow::anyhow!("Failed to compare local hashes: {err}"))?;

    let mut existing = vec![false; candidates.len()];
    for ((candidate_index, _), exists) in indexed_hashes.into_iter().zip(sorted_existing) {
        existing[candidate_index] = exists;
    }
    Ok(existing)
}

#[cfg(feature = "s3")]
fn read_r2_import_keys_file(path: &Path) -> Result<Vec<String>> {
    let raw = std::fs::read_to_string(path)?;
    Ok(raw
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(ToOwned::to_owned)
        .collect())
}

#[cfg(feature = "s3")]
fn read_r2_import_state(path: &Path) -> Option<R2ImportState> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

#[cfg(feature = "s3")]
async fn fetch_r2_object_body_with_retries(
    client: &aws_sdk_s3::Client,
    bucket: &str,
    key: &str,
) -> Result<Vec<u8>, String> {
    let mut last_error = None;
    for attempt in 1..=R2_IMPORT_OBJECT_READ_ATTEMPTS {
        let output = match client.get_object().bucket(bucket).key(key).send().await {
            Ok(output) => output,
            Err(err) => {
                last_error = Some(format!("fetch failed for {key}: {err}"));
                if attempt < R2_IMPORT_OBJECT_READ_ATTEMPTS {
                    let delay_ms = R2_IMPORT_OBJECT_RETRY_BASE_DELAY_MS << (attempt - 1);
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
                continue;
            }
        };

        match output.body.collect().await {
            Ok(body) => return Ok(body.into_bytes().to_vec()),
            Err(err) => {
                last_error = Some(format!("read failed for {key}: {err}"));
                if attempt < R2_IMPORT_OBJECT_READ_ATTEMPTS {
                    let delay_ms = R2_IMPORT_OBJECT_RETRY_BASE_DELAY_MS << (attempt - 1);
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
            }
        }
    }

    Err(format!(
        "{} after {} attempt(s)",
        last_error.unwrap_or_else(|| format!("fetch failed for {key}: unknown error")),
        R2_IMPORT_OBJECT_READ_ATTEMPTS
    ))
}

#[cfg(feature = "s3")]
fn write_r2_import_state(path: &Path, result: &R2ImportResult) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let state = R2ImportState {
        result: result.clone(),
        updated_at_unix: unix_timestamp_now(),
    };
    std::fs::write(path, serde_json::to_vec_pretty(&state)?)?;
    Ok(())
}

#[cfg(feature = "s3")]
async fn import_r2_object_to_local(
    client: Arc<aws_sdk_s3::Client>,
    bucket: Arc<String>,
    local: Arc<super::LocalStore>,
    candidate: R2ObjectCandidate,
    check_only: bool,
    prechecked_missing: bool,
) -> R2ObjectImportOutcome {
    if !prechecked_missing {
        match local.exists(&candidate.hash) {
            Ok(true) => {
                return R2ObjectImportOutcome {
                    skipped: true,
                    ..Default::default()
                };
            }
            Ok(false) => {}
            Err(err) => {
                return R2ObjectImportOutcome {
                    failed: true,
                    message: Some(format!("local exists failed for {}: {err}", candidate.key)),
                    ..Default::default()
                };
            }
        }
    }

    if check_only {
        return R2ObjectImportOutcome {
            missing: true,
            ..Default::default()
        };
    }

    let body =
        match fetch_r2_object_body_with_retries(client.as_ref(), bucket.as_str(), &candidate.key)
            .await
        {
            Ok(body) => body,
            Err(err) => {
                return R2ObjectImportOutcome {
                    missing: true,
                    failed: true,
                    message: Some(err),
                    ..Default::default()
                };
            }
        };
    let data = body.as_slice();
    let actual_hash = sha256(data);
    if actual_hash != candidate.hash {
        return R2ObjectImportOutcome {
            missing: true,
            corrupted: true,
            message: Some(format!(
                "hash mismatch for {}: actual {}",
                candidate.key,
                to_hex(&actual_hash)
            )),
            ..Default::default()
        };
    }

    match local.put_sync(candidate.hash, data) {
        Ok(inserted) => R2ObjectImportOutcome {
            missing: true,
            imported: inserted,
            skipped: !inserted,
            bytes_imported: if inserted { data.len() as u64 } else { 0 },
            ..Default::default()
        },
        Err(err) => R2ObjectImportOutcome {
            missing: true,
            failed: true,
            message: Some(format!("local put failed for {}: {err}", candidate.key)),
            ..Default::default()
        },
    }
}

#[cfg(feature = "s3")]
async fn settle_one_r2_import(
    pending: &mut FuturesUnordered<impl std::future::Future<Output = R2ObjectImportOutcome>>,
    result: &mut R2ImportResult,
) {
    if let Some(outcome) = pending.next().await {
        if outcome.skipped {
            result.skipped += 1;
        }
        if outcome.missing {
            result.missing += 1;
        }
        if outcome.imported {
            result.imported += 1;
            result.bytes_imported = result.bytes_imported.saturating_add(outcome.bytes_imported);
        }
        if outcome.corrupted {
            result.corrupted += 1;
        }
        if outcome.failed {
            result.failed += 1;
        }
        if let Some(message) = outcome.message {
            println!("  {message}");
        }
    }
}

impl HashtreeStore {
    /// Garbage collect unpinned content
    pub fn gc(&self) -> Result<GcStats> {
        let rtxn = self.env.read_txn()?;

        // Get all pinned hashes as raw bytes
        let pinned: HashSet<[u8; 32]> = self
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

        // Get hashes in the canonical writable store.
        let all_hashes = self
            .router
            .list_writable()
            .map_err(|e| anyhow::anyhow!("Failed to list writable hashes: {}", e))?;

        // Delete unpinned hashes
        let mut deleted = 0;
        let mut freed_bytes = 0u64;

        for hash in all_hashes {
            if !pinned.contains(&hash) {
                if let Ok(Some(size)) = self.router.blob_size_sync(&hash) {
                    freed_bytes += size;
                    // Delete locally only - keep S3 as archive
                    let _ = self.router.delete_local_only(&hash);
                    deleted += 1;
                }
            }
        }

        Ok(GcStats {
            deleted_dags: deleted,
            freed_bytes,
        })
    }

    /// Verify LMDB blob integrity - checks that stored data matches its key hash
    /// Returns verification statistics and optionally deletes corrupted entries
    pub fn verify_lmdb_integrity(&self, delete: bool) -> Result<VerifyResult> {
        let all_hashes = self
            .router
            .list()
            .map_err(|e| anyhow::anyhow!("Failed to list hashes: {}", e))?;

        let total = all_hashes.len();
        let mut valid = 0;
        let mut corrupted = 0;
        let mut deleted = 0;
        let mut corrupted_hashes = Vec::new();

        for hash in &all_hashes {
            let hash_hex = to_hex(hash);

            match self.router.get_sync(hash) {
                Ok(Some(data)) => {
                    let actual_hash = sha256(&data);

                    if actual_hash == *hash {
                        valid += 1;
                    } else {
                        corrupted += 1;
                        let actual_hex = to_hex(&actual_hash);
                        println!(
                            "  CORRUPTED: key={} actual={} size={}",
                            &hash_hex[..16],
                            &actual_hex[..16],
                            data.len()
                        );
                        corrupted_hashes.push(*hash);
                    }
                }
                Ok(None) => {
                    corrupted += 1;
                    println!("  MISSING: key={}", &hash_hex[..16]);
                    corrupted_hashes.push(*hash);
                }
                Err(e) => {
                    corrupted += 1;
                    println!("  ERROR: key={} err={}", &hash_hex[..16], e);
                    corrupted_hashes.push(*hash);
                }
            }
        }

        if delete {
            for hash in &corrupted_hashes {
                match self.router.delete_sync(hash) {
                    Ok(true) => deleted += 1,
                    Ok(false) => {}
                    Err(e) => {
                        let hash_hex = to_hex(hash);
                        println!("  Failed to delete {}: {}", &hash_hex[..16], e);
                    }
                }
            }
        }

        Ok(VerifyResult {
            total,
            valid,
            corrupted,
            deleted,
        })
    }

    /// Verify R2/S3 blob integrity - lists all objects and verifies hash matches filename
    /// Returns verification statistics and optionally deletes corrupted entries
    #[cfg(feature = "s3")]
    pub async fn verify_r2_integrity(&self, delete: bool) -> Result<VerifyResult> {
        use aws_sdk_s3::Client as S3Client;

        let config = crate::config::Config::load()?;
        let s3_config = config
            .storage
            .s3
            .ok_or_else(|| anyhow::anyhow!("S3 not configured"))?;

        let aws_config = aws_config::from_env()
            .region(aws_sdk_s3::config::Region::new(s3_config.region.clone()))
            .load()
            .await;

        let s3_client = S3Client::from_conf(
            aws_sdk_s3::config::Builder::from(&aws_config)
                .endpoint_url(&s3_config.endpoint)
                .force_path_style(true)
                .build(),
        );

        let bucket = &s3_config.bucket;
        let prefix = s3_config.prefix.as_deref().unwrap_or("");

        let mut total = 0;
        let mut valid = 0;
        let mut corrupted = 0;
        let mut deleted = 0;
        let mut corrupted_keys = Vec::new();

        let mut continuation_token: Option<String> = None;

        loop {
            let mut list_req = s3_client.list_objects_v2().bucket(bucket).prefix(prefix);

            if let Some(ref token) = continuation_token {
                list_req = list_req.continuation_token(token);
            }

            let list_resp = list_req
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("Failed to list S3 objects: {}", e))?;

            for object in list_resp.contents() {
                let key = object.key().unwrap_or("");

                if !key.ends_with(".bin") {
                    continue;
                }

                total += 1;

                let filename = key.strip_prefix(prefix).unwrap_or(key);
                let expected_hash_hex = filename.strip_suffix(".bin").unwrap_or(filename);

                if expected_hash_hex.len() != 64 {
                    corrupted += 1;
                    println!("  INVALID KEY: {}", key);
                    corrupted_keys.push(key.to_string());
                    continue;
                }

                let expected_hash = match from_hex(expected_hash_hex) {
                    Ok(h) => h,
                    Err(_) => {
                        corrupted += 1;
                        println!("  INVALID HEX: {}", key);
                        corrupted_keys.push(key.to_string());
                        continue;
                    }
                };

                match s3_client.get_object().bucket(bucket).key(key).send().await {
                    Ok(resp) => match resp.body.collect().await {
                        Ok(bytes) => {
                            let data = bytes.into_bytes();
                            let actual_hash = sha256(&data);

                            if actual_hash == expected_hash {
                                valid += 1;
                            } else {
                                corrupted += 1;
                                let actual_hex = to_hex(&actual_hash);
                                println!(
                                    "  CORRUPTED: key={} actual={} size={}",
                                    &expected_hash_hex[..16],
                                    &actual_hex[..16],
                                    data.len()
                                );
                                corrupted_keys.push(key.to_string());
                            }
                        }
                        Err(e) => {
                            corrupted += 1;
                            println!("  READ ERROR: {} - {}", key, e);
                            corrupted_keys.push(key.to_string());
                        }
                    },
                    Err(e) => {
                        corrupted += 1;
                        println!("  FETCH ERROR: {} - {}", key, e);
                        corrupted_keys.push(key.to_string());
                    }
                }

                if total % 100 == 0 {
                    println!(
                        "  Progress: {} objects checked, {} corrupted so far",
                        total, corrupted
                    );
                }
            }

            if list_resp.is_truncated() == Some(true) {
                continuation_token = list_resp.next_continuation_token().map(|s| s.to_string());
            } else {
                break;
            }
        }

        if delete {
            for key in &corrupted_keys {
                match s3_client
                    .delete_object()
                    .bucket(bucket)
                    .key(key)
                    .send()
                    .await
                {
                    Ok(_) => deleted += 1,
                    Err(e) => {
                        println!("  Failed to delete {}: {}", key, e);
                    }
                }
            }
        }

        Ok(VerifyResult {
            total,
            valid,
            corrupted,
            deleted,
        })
    }

    /// Import missing R2/S3 blobs into local storage without writing back to S3.
    ///
    /// This mirrors rclone's shape: list the source, compare each source object
    /// against the destination by cheap metadata (here the content-addressed key),
    /// and only transfer missing objects. `--check-only` runs the same comparison
    /// without downloading object bodies.
    #[cfg(feature = "s3")]
    pub async fn import_r2_to_local(&self, options: R2ImportOptions) -> Result<R2ImportResult> {
        use aws_sdk_s3::Client as S3Client;

        let config = crate::config::Config::load()?;
        let s3_config = config
            .storage
            .s3
            .ok_or_else(|| anyhow::anyhow!("S3 not configured"))?;

        let aws_config = aws_config::from_env()
            .region(aws_sdk_s3::config::Region::new(s3_config.region.clone()))
            .load()
            .await;

        let s3_client = S3Client::from_conf(
            aws_sdk_s3::config::Builder::from(&aws_config)
                .endpoint_url(&s3_config.endpoint)
                .force_path_style(true)
                .build(),
        );

        let bucket = Arc::new(s3_config.bucket);
        let prefix = s3_config.prefix.unwrap_or_default();
        let list_prefix = options
            .scan_prefix
            .as_ref()
            .map(|scan_prefix| format!("{prefix}{scan_prefix}"))
            .unwrap_or_else(|| prefix.clone());
        let mut explicit_keys = options.keys.clone();
        if let Some(keys_file) = options.keys_file.as_ref() {
            explicit_keys.extend(read_r2_import_keys_file(keys_file)?);
        }

        let local = self.router.local_store();
        let client = Arc::new(s3_client);
        let concurrency = options.concurrency.max(1);
        let mut pending = FuturesUnordered::new();

        if !explicit_keys.is_empty() {
            let mut result = R2ImportResult {
                completed: false,
                ..Default::default()
            };

            println!(
                "R2 import {} targeted: bucket={}, prefix={}, requested_keys={}, state_file={}",
                if options.check_only { "check" } else { "sync" },
                bucket.as_str(),
                prefix,
                explicit_keys.len(),
                options
                    .state_file
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "<none>".to_string()),
            );
            for key in explicit_keys {
                let Some(candidate) = r2_import_key_candidate(&prefix, &key) else {
                    result.failed += 1;
                    println!("  invalid R2 blob key/hash: {key}");
                    continue;
                };

                result.last_key = Some(candidate.key.clone());
                result.listed += 1;
                pending.push(import_r2_object_to_local(
                    client.clone(),
                    bucket.clone(),
                    local.clone(),
                    candidate,
                    options.check_only,
                    false,
                ));

                while pending.len() >= concurrency {
                    settle_one_r2_import(&mut pending, &mut result).await;
                }
            }

            while !pending.is_empty() {
                settle_one_r2_import(&mut pending, &mut result).await;
            }

            result.completed = true;
            if let Some(state_file) = options.state_file.as_ref() {
                write_r2_import_state(state_file, &result)?;
            }
            return Ok(result);
        }

        let state_file = options
            .state_file
            .unwrap_or_else(|| self.base_path().join("r2-import-state.json"));
        let saved_state = read_r2_import_state(&state_file);
        let saved_incomplete = saved_state
            .as_ref()
            .is_some_and(|state| !state.result.completed && state.result.last_key.is_some());
        let start_after = options.start_after.clone().or_else(|| {
            if options.resume && saved_incomplete {
                saved_state
                    .as_ref()
                    .and_then(|state| state.result.last_key.clone())
            } else {
                None
            }
        });
        let mut result = if options.resume && options.start_after.is_none() && saved_incomplete {
            saved_state.map(|state| state.result).unwrap_or_default()
        } else {
            R2ImportResult::default()
        };
        result.completed = false;

        println!(
            "R2 import {}: bucket={}, prefix={}, list_prefix={}, start_after={}, state_file={}",
            if options.check_only { "check" } else { "sync" },
            bucket.as_str(),
            prefix,
            list_prefix,
            start_after.as_deref().unwrap_or("<beginning>"),
            state_file.display(),
        );

        if options.stream_merge && options.fast_list {
            println!("  Stream merge enabled; skipping in-memory --fast-list index");
        }

        let local_hashes = if options.fast_list && !options.stream_merge {
            println!("  Loading local hash index...");
            let mut local_hashes = self
                .router
                .list()
                .map_err(|err| anyhow::anyhow!("Failed to list local blobs: {err}"))?;
            local_hashes.sort_unstable();
            println!("  Local hash index loaded: {} blobs", local_hashes.len());
            Some(local_hashes)
        } else {
            None
        };

        let progress_every = options.progress_every.max(1);
        let mut continuation_token: Option<String> = None;
        let mut listed_since_progress = 0usize;
        let mut listed_this_run = 0usize;
        let mut first_page = true;
        let mut hit_max_objects = false;

        loop {
            let mut list_req = client
                .list_objects_v2()
                .bucket(bucket.as_str())
                .prefix(&list_prefix);

            if let Some(ref token) = continuation_token {
                list_req = list_req.continuation_token(token);
            } else if first_page {
                if let Some(ref start_after) = start_after {
                    list_req = list_req.start_after(start_after);
                }
            }
            first_page = false;

            let list_resp = list_req
                .send()
                .await
                .map_err(|err| anyhow::anyhow!("Failed to list S3 objects: {err}"))?;

            let mut page_candidates = Vec::new();
            let mut page_last_key = None;
            for object in list_resp.contents() {
                if options
                    .max_objects
                    .is_some_and(|max_objects| listed_this_run >= max_objects)
                {
                    hit_max_objects = true;
                    break;
                }

                let key = object.key().unwrap_or("").to_string();
                page_last_key = Some(key.clone());
                if !key.ends_with(".bin") {
                    continue;
                }

                let Some(hash) = r2_import_key_hash(&prefix, &key) else {
                    continue;
                };

                result.listed += 1;
                listed_this_run += 1;
                listed_since_progress += 1;
                page_candidates.push(R2ObjectCandidate { key, hash });
            }

            let page_existing = if options.stream_merge && !page_candidates.is_empty() {
                Some(existing_r2_candidates(local.as_ref(), &page_candidates)?)
            } else {
                None
            };

            for (candidate_index, candidate) in page_candidates.into_iter().enumerate() {
                let already_exists = page_existing
                    .as_ref()
                    .is_some_and(|existing| existing[candidate_index]);

                if options.scan_delay_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(options.scan_delay_ms)).await;
                }

                if already_exists {
                    result.skipped += 1;
                    continue;
                }

                if let Some(local_hashes) = &local_hashes {
                    if local_hashes.binary_search(&candidate.hash).is_ok() {
                        result.skipped += 1;
                        continue;
                    }
                }

                pending.push(import_r2_object_to_local(
                    client.clone(),
                    bucket.clone(),
                    local.clone(),
                    candidate,
                    options.check_only,
                    page_existing.is_some(),
                ));

                while pending.len() >= concurrency {
                    settle_one_r2_import(&mut pending, &mut result).await;
                }
            }

            while !pending.is_empty() {
                settle_one_r2_import(&mut pending, &mut result).await;
            }
            if let Some(last_key) = page_last_key {
                result.last_key = Some(last_key);
            }
            if listed_since_progress >= progress_every {
                listed_since_progress = 0;
                println!(
                    "  Progress: {} listed, {} imported, {} skipped, {} missing, {} corrupted, {} failed, {:.2} GB imported",
                    result.listed,
                    result.imported,
                    result.skipped,
                    result.missing,
                    result.corrupted,
                    result.failed,
                    result.bytes_imported as f64 / 1024.0 / 1024.0 / 1024.0,
                );
            }
            write_r2_import_state(&state_file, &result)?;

            if hit_max_objects {
                break;
            }
            if list_resp.is_truncated() == Some(true) {
                continuation_token = list_resp.next_continuation_token().map(|s| s.to_string());
            } else {
                result.completed = true;
                break;
            }
        }

        write_r2_import_state(&state_file, &result)?;
        Ok(result)
    }

    /// Fallback for non-S3 builds
    #[cfg(not(feature = "s3"))]
    pub async fn verify_r2_integrity(&self, _delete: bool) -> Result<VerifyResult> {
        Err(anyhow::anyhow!("S3 feature not enabled"))
    }

    pub fn compact_lmdb_environments(
        &self,
        env_dirs: &[PathBuf],
        scratch_dir: Option<&Path>,
        keep_backup: bool,
    ) -> Result<Vec<CompactResult>> {
        compact_lmdb_environments_under(self.base_path(), env_dirs, scratch_dir, keep_backup)
    }
}

pub fn compact_lmdb_environments_under(
    base_path: &Path,
    env_dirs: &[PathBuf],
    scratch_dir: Option<&Path>,
    keep_backup: bool,
) -> Result<Vec<CompactResult>> {
    let targets = if env_dirs.is_empty() {
        discover_lmdb_environment_dirs(base_path)?
    } else {
        env_dirs
            .iter()
            .map(|path| {
                if path.is_absolute() {
                    path.clone()
                } else {
                    base_path.join(path)
                }
            })
            .collect()
    };

    let mut results = Vec::new();
    for env_dir in targets {
        results.push(compact_lmdb_environment_dir(
            &env_dir,
            scratch_dir,
            keep_backup,
        )?);
    }
    Ok(results)
}

fn discover_lmdb_environment_dirs(root: &Path) -> Result<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    collect_lmdb_environment_dirs(root, &mut dirs)?;
    dirs.sort();
    Ok(dirs)
}

fn collect_lmdb_environment_dirs(root: &Path, dirs: &mut Vec<PathBuf>) -> Result<()> {
    if root.join("data.mdb").exists() {
        dirs.push(root.to_path_buf());
    }

    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_lmdb_environment_dirs(&path, dirs)?;
        }
    }

    Ok(())
}

fn compact_lmdb_environment_dir(
    env_dir: &Path,
    scratch_dir: Option<&Path>,
    keep_backup: bool,
) -> Result<CompactResult> {
    if let Some(scratch_dir) = scratch_dir {
        return compact_lmdb_environment_dir_with_scratch(env_dir, scratch_dir, keep_backup);
    }

    let data_path = env_dir.join("data.mdb");
    if !data_path.exists() {
        anyhow::bail!("No data.mdb found in {}", env_dir.display());
    }

    let before_bytes = std::fs::metadata(&data_path)?.len();
    let compact_path = env_dir.join("data.mdb.compact");
    let backup_path = env_dir.join("data.mdb.bak");

    if compact_path.exists() {
        std::fs::remove_file(&compact_path)?;
    }
    if !keep_backup && backup_path.exists() {
        std::fs::remove_file(&backup_path)?;
    }

    let open_map_size = existing_lmdb_map_size_bytes(&data_path)?;

    {
        let mut options = EnvOpenOptions::new();
        options
            .map_size(open_map_size)
            .max_dbs(COMPACT_MAX_DBS)
            .max_readers(COMPACT_MAX_READERS);
        let env = unsafe { ManagedEnv::open(&options, env_dir) }?;
        env.force_sync()?;
        env.copy_to_file(&compact_path, CompactionOption::Enabled)?;
    }

    let after_bytes = std::fs::metadata(&compact_path)?.len();

    if backup_path.exists() {
        std::fs::remove_file(&backup_path)?;
    }

    std::fs::rename(&data_path, &backup_path)?;
    if let Err(error) = std::fs::rename(&compact_path, &data_path) {
        let _ = std::fs::rename(&backup_path, &data_path);
        return Err(error.into());
    }

    if !keep_backup {
        std::fs::remove_file(&backup_path)?;
    }

    Ok(CompactResult {
        env_dir: env_dir.to_path_buf(),
        before_bytes,
        after_bytes,
        backup_path: keep_backup.then_some(backup_path),
    })
}

fn compact_lmdb_environment_dir_with_scratch(
    env_dir: &Path,
    scratch_root: &Path,
    keep_backup: bool,
) -> Result<CompactResult> {
    let data_path = env_dir.join("data.mdb");
    if !data_path.exists() {
        anyhow::bail!("No data.mdb found in {}", env_dir.display());
    }

    std::fs::create_dir_all(scratch_root)?;
    let scratch_dir = unique_compaction_scratch_dir(scratch_root, env_dir)?;
    std::fs::create_dir(&scratch_dir)?;
    sync_directory(scratch_root)?;

    let compact_path = scratch_dir.join("data.mdb.compact");
    let backup_path = scratch_dir.join("data.mdb.bak");
    let recovery_path = env_dir.join("data.mdb.compact-recovery");
    let replacement_path = env_dir.join("data.mdb.replacement");
    let restore_path = env_dir.join("data.mdb.restore");
    for path in [&recovery_path, &replacement_path, &restore_path] {
        if path.exists() {
            anyhow::bail!(
                "refusing to compact {} while recovery artifact {} exists",
                env_dir.display(),
                path.display()
            );
        }
    }

    let before_bytes = std::fs::metadata(&data_path)?.len();
    let open_map_size = existing_lmdb_map_size_bytes(&data_path)?;
    {
        let mut options = EnvOpenOptions::new();
        options
            .map_size(open_map_size)
            .max_dbs(COMPACT_MAX_DBS)
            .max_readers(COMPACT_MAX_READERS);
        let env = unsafe { ManagedEnv::open(&options, env_dir) }?;
        env.force_sync()?;
        env.copy_to_file(&compact_path, CompactionOption::Enabled)?;
    }
    sync_file(&compact_path)?;
    let after_bytes = std::fs::metadata(&compact_path)?.len();

    let copied = std::fs::copy(&data_path, &backup_path)
        .with_context(|| format!("copying recovery backup to {}", backup_path.display()))?;
    if copied != before_bytes {
        anyhow::bail!(
            "incomplete recovery backup at {}: copied {} of {} bytes",
            backup_path.display(),
            copied,
            before_bytes
        );
    }
    sync_file(&backup_path)?;
    sync_directory(&scratch_dir)?;

    write_recovery_marker(&recovery_path, &backup_path, &compact_path)?;
    std::fs::remove_file(&data_path)?;
    sync_directory(env_dir)?;

    let replacement_result = (|| -> Result<()> {
        let copied = std::fs::copy(&compact_path, &replacement_path)
            .with_context(|| format!("copying compact LMDB to {}", replacement_path.display()))?;
        if copied != after_bytes {
            anyhow::bail!(
                "incomplete compact replacement at {}: copied {} of {} bytes",
                replacement_path.display(),
                copied,
                after_bytes
            );
        }
        sync_file(&replacement_path)?;
        std::fs::rename(&replacement_path, &data_path)?;
        sync_directory(env_dir)?;
        Ok(())
    })();

    if let Err(replacement_error) = replacement_result {
        let _ = std::fs::remove_file(&replacement_path);
        let restore_result = (|| -> Result<()> {
            let copied = std::fs::copy(&backup_path, &restore_path)?;
            if copied != before_bytes {
                anyhow::bail!("incomplete restoration: copied {copied} of {before_bytes} bytes");
            }
            sync_file(&restore_path)?;
            std::fs::rename(&restore_path, &data_path)?;
            sync_directory(env_dir)?;
            Ok(())
        })();
        return match restore_result {
            Ok(()) => Err(replacement_error.context("compact replacement failed; original restored")),
            Err(restore_error) => Err(replacement_error.context(format!(
                "compact replacement failed and automatic restoration failed: {restore_error:#}; recovery backup is {}",
                backup_path.display()
            ))),
        };
    }

    std::fs::remove_file(&recovery_path)?;
    std::fs::remove_file(&compact_path)?;
    if !keep_backup {
        std::fs::remove_file(&backup_path)?;
        std::fs::remove_dir(&scratch_dir)?;
    }
    sync_directory(env_dir)?;
    sync_directory(scratch_root)?;

    Ok(CompactResult {
        env_dir: env_dir.to_path_buf(),
        before_bytes,
        after_bytes,
        backup_path: keep_backup.then_some(backup_path),
    })
}

fn unique_compaction_scratch_dir(scratch_root: &Path, env_dir: &Path) -> Result<PathBuf> {
    let leaf = env_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("lmdb")
        .replace(|character: char| !character.is_ascii_alphanumeric(), "-");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    Ok(scratch_root.join(format!("htree-compact-{leaf}-{}-{now}", std::process::id())))
}

fn write_recovery_marker(path: &Path, backup_path: &Path, compact_path: &Path) -> Result<()> {
    let mut marker = OpenOptions::new().write(true).create_new(true).open(path)?;
    writeln!(marker, "backup={}", backup_path.display())?;
    writeln!(marker, "compact={}", compact_path.display())?;
    marker.sync_all()?;
    sync_directory(path.parent().context("recovery marker has no parent")?)
}

fn sync_file(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

fn existing_lmdb_map_size_bytes(data_path: &Path) -> Result<usize> {
    let file_bytes = std::fs::metadata(data_path)?.len();
    let aligned_bytes = if file_bytes == 0 {
        COMPACT_OPEN_MAP_SIZE_BYTES as u64
    } else {
        let remainder = file_bytes % COMPACT_PAGE_SIZE_BYTES;
        if remainder == 0 {
            file_bytes
        } else {
            file_bytes.saturating_add(COMPACT_PAGE_SIZE_BYTES - remainder)
        }
    };

    Ok(usize::try_from(aligned_bytes)
        .unwrap_or(usize::MAX)
        .max(COMPACT_OPEN_MAP_SIZE_BYTES))
}

#[cfg(all(test, feature = "s3"))]
mod tests {
    use super::{r2_import_key_candidate, r2_import_key_hash};

    const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn r2_import_key_hash_accepts_only_root_blob_keys() {
        assert!(r2_import_key_hash("", &format!("{HASH}.bin")).is_some());
        assert!(r2_import_key_hash("legacy/", &format!("legacy/{HASH}.bin")).is_some());

        assert!(r2_import_key_hash("", &format!("hot/{HASH}.bin")).is_none());
        assert!(
            r2_import_key_hash("", &format!("site-bytes/pubkey/tree/root/{HASH}.bin")).is_none()
        );
        assert!(r2_import_key_hash("", "roots/pubkey/tree.json").is_none());
        assert!(r2_import_key_hash("", &format!("{HASH}.png")).is_none());
        assert!(r2_import_key_hash("", "not-a-hash.bin").is_none());
    }

    #[test]
    fn r2_import_key_candidate_accepts_hash_or_canonical_key() {
        let bare = r2_import_key_candidate("", HASH).expect("bare hash");
        assert_eq!(bare.key, format!("{HASH}.bin"));

        let explicit = r2_import_key_candidate("", &format!("{HASH}.bin")).expect("hash key");
        assert_eq!(explicit.key, format!("{HASH}.bin"));

        assert!(r2_import_key_candidate("", &format!("hot/{HASH}.bin")).is_none());
    }

    #[test]
    fn r2_import_key_candidate_applies_configured_prefix() {
        let bare = r2_import_key_candidate("legacy/", HASH).expect("prefixed bare hash");
        assert_eq!(bare.key, format!("legacy/{HASH}.bin"));

        let explicit =
            r2_import_key_candidate("legacy/", &format!("{HASH}.bin")).expect("prefixed key");
        assert_eq!(explicit.key, format!("legacy/{HASH}.bin"));

        let already_prefixed =
            r2_import_key_candidate("legacy/", &format!("legacy/{HASH}.bin")).expect("key");
        assert_eq!(already_prefixed.key, format!("legacy/{HASH}.bin"));
    }
}
