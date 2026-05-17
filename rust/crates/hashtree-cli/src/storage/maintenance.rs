use anyhow::Result;
use heed::{CompactionOption, EnvOpenOptions};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
#[cfg(feature = "s3")]
use std::sync::Arc;
#[cfg(feature = "s3")]
use std::time::{SystemTime, UNIX_EPOCH};

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
}

#[derive(Debug, Clone, Default)]
pub struct R2ImportOptions {
    pub concurrency: usize,
    pub check_only: bool,
    pub resume: bool,
    pub fast_list: bool,
    pub start_after: Option<String>,
    pub state_file: Option<PathBuf>,
    pub max_objects: Option<usize>,
    pub progress_every: usize,
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
    if hash_hex.len() != 64 {
        return None;
    }
    from_hex(hash_hex).ok()
}

#[cfg(feature = "s3")]
fn read_r2_import_state(path: &Path) -> Option<R2ImportState> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
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
) -> R2ObjectImportOutcome {
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

    if check_only {
        return R2ObjectImportOutcome {
            missing: true,
            ..Default::default()
        };
    }

    let output = match client
        .get_object()
        .bucket(bucket.as_str())
        .key(&candidate.key)
        .send()
        .await
    {
        Ok(output) => output,
        Err(err) => {
            return R2ObjectImportOutcome {
                missing: true,
                failed: true,
                message: Some(format!("fetch failed for {}: {err}", candidate.key)),
                ..Default::default()
            };
        }
    };

    let body = match output.body.collect().await {
        Ok(body) => body.into_bytes(),
        Err(err) => {
            return R2ObjectImportOutcome {
                missing: true,
                failed: true,
                message: Some(format!("read failed for {}: {err}", candidate.key)),
                ..Default::default()
            };
        }
    };
    let data = body.as_ref();
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

        // Get all stored hashes
        let all_hashes = self
            .router
            .list()
            .map_err(|e| anyhow::anyhow!("Failed to list hashes: {}", e))?;

        // Delete unpinned hashes
        let mut deleted = 0;
        let mut freed_bytes = 0u64;

        for hash in all_hashes {
            if !pinned.contains(&hash) {
                if let Ok(Some(data)) = self.router.get_sync(&hash) {
                    freed_bytes += data.len() as u64;
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
            "R2 import {}: bucket={}, prefix={}, start_after={}, state_file={}",
            if options.check_only { "check" } else { "sync" },
            bucket.as_str(),
            prefix,
            start_after.as_deref().unwrap_or("<beginning>"),
            state_file.display(),
        );

        let local_hashes = if options.fast_list {
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

        let local = self.router.local_store();
        let client = Arc::new(s3_client);
        let concurrency = options.concurrency.max(1);
        let progress_every = options.progress_every.max(1);
        let mut continuation_token: Option<String> = None;
        let mut pending = FuturesUnordered::new();
        let mut listed_since_progress = 0usize;
        let mut first_page = true;
        let mut hit_max_objects = false;

        loop {
            let mut list_req = client
                .list_objects_v2()
                .bucket(bucket.as_str())
                .prefix(&prefix);

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

            for object in list_resp.contents() {
                if options
                    .max_objects
                    .is_some_and(|max_objects| result.listed >= max_objects)
                {
                    hit_max_objects = true;
                    break;
                }

                let key = object.key().unwrap_or("").to_string();
                if !key.ends_with(".bin") {
                    continue;
                }

                result.listed += 1;
                listed_since_progress += 1;
                result.last_key = Some(key.clone());

                let Some(hash) = r2_import_key_hash(&prefix, &key) else {
                    result.corrupted += 1;
                    println!("  INVALID KEY: {key}");
                    continue;
                };

                if let Some(local_hashes) = &local_hashes {
                    if local_hashes.binary_search(&hash).is_ok() {
                        result.skipped += 1;
                        continue;
                    }
                }

                pending.push(import_r2_object_to_local(
                    client.clone(),
                    bucket.clone(),
                    local.clone(),
                    R2ObjectCandidate { key, hash },
                    options.check_only,
                ));

                while pending.len() >= concurrency {
                    settle_one_r2_import(&mut pending, &mut result).await;
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
                    let _ = write_r2_import_state(&state_file, &result);
                }
            }

            while !pending.is_empty() {
                settle_one_r2_import(&mut pending, &mut result).await;
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
        keep_backup: bool,
    ) -> Result<Vec<CompactResult>> {
        compact_lmdb_environments_under(self.base_path(), env_dirs, keep_backup)
    }
}

pub fn compact_lmdb_environments_under(
    base_path: &Path,
    env_dirs: &[PathBuf],
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
        results.push(compact_lmdb_environment_dir(&env_dir, keep_backup)?);
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

fn compact_lmdb_environment_dir(env_dir: &Path, keep_backup: bool) -> Result<CompactResult> {
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
        let env = unsafe {
            EnvOpenOptions::new()
                .map_size(open_map_size)
                .max_dbs(COMPACT_MAX_DBS)
                .max_readers(COMPACT_MAX_READERS)
                .open(env_dir)
        }?;
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
    })
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
