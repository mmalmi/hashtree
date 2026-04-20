use anyhow::Result;
use heed::{CompactionOption, EnvOpenOptions};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::{GcStats, HashtreeStore};

#[cfg(feature = "s3")]
use hashtree_core::from_hex;
use hashtree_core::{sha256, to_hex};

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

const COMPACT_MAX_DBS: u32 = 64;
const COMPACT_MAX_READERS: u32 = 2048;
const COMPACT_OPEN_MAP_SIZE_BYTES: usize = 10 * 1024 * 1024;
const COMPACT_PAGE_SIZE_BYTES: u64 = 4096;

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
