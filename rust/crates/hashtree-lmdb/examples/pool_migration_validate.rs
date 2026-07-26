use hashtree_core::{from_hex, sha256, to_hex, Hash};
use hashtree_lmdb::{ExternalBlobOptions, LmdbBlobReader, PoolStoreConfig, PoolStoreReader};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

const STATE_VERSION: u64 = 1;
const FAILURE_SAMPLE_LIMIT: u64 = 100;

#[derive(Clone, Default)]
struct ValidationState {
    version: u64,
    source_path: String,
    pool_path: String,
    source_device: u64,
    source_inode: u64,
    source_data_mdb_len: u64,
    source_mtime_secs: i64,
    source_mtime_nanos: i64,
    cursor: Option<Hash>,
    complete: bool,
    active_elapsed_ms: u64,
    source_keys: u64,
    source_size_known: u64,
    source_declared_bytes: u64,
    source_readable: u64,
    source_readable_bytes: u64,
    source_hash_valid: u64,
    source_failure_keys: u64,
    pool_catalog_present: u64,
    pool_size_known: u64,
    pool_declared_bytes: u64,
    pool_readable: u64,
    pool_readable_bytes: u64,
    pool_hash_valid: u64,
    pool_exact_match: u64,
    pool_failure_keys: u64,
}

impl ValidationState {
    fn new(source: &Path, pool: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let mut state = Self {
            version: STATE_VERSION,
            source_path: source.display().to_string(),
            pool_path: pool.display().to_string(),
            ..Self::default()
        };
        state.set_source_fingerprint(source)?;
        Ok(state)
    }

    fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let mut encoded = String::new();
        File::open(path)?.read_to_string(&mut encoded)?;
        let mut state = Self::default();
        for line in encoded.lines() {
            let Some((key, value)) = line.split_once('=') else {
                return Err(format!("invalid state line: {line}").into());
            };
            match key {
                "version" => state.version = value.parse()?,
                "source_path" => state.source_path = value.to_owned(),
                "pool_path" => state.pool_path = value.to_owned(),
                "source_device" => state.source_device = value.parse()?,
                "source_inode" => state.source_inode = value.parse()?,
                "source_data_mdb_len" => state.source_data_mdb_len = value.parse()?,
                "source_mtime_secs" => state.source_mtime_secs = value.parse()?,
                "source_mtime_nanos" => state.source_mtime_nanos = value.parse()?,
                "cursor" if value.is_empty() => state.cursor = None,
                "cursor" => state.cursor = Some(from_hex(value)?),
                "complete" => state.complete = value.parse()?,
                "active_elapsed_ms" => state.active_elapsed_ms = value.parse()?,
                "source_keys" => state.source_keys = value.parse()?,
                "source_size_known" => state.source_size_known = value.parse()?,
                "source_declared_bytes" => state.source_declared_bytes = value.parse()?,
                "source_readable" => state.source_readable = value.parse()?,
                "source_readable_bytes" => state.source_readable_bytes = value.parse()?,
                "source_hash_valid" => state.source_hash_valid = value.parse()?,
                "source_failure_keys" => state.source_failure_keys = value.parse()?,
                "pool_catalog_present" => state.pool_catalog_present = value.parse()?,
                "pool_size_known" => state.pool_size_known = value.parse()?,
                "pool_declared_bytes" => state.pool_declared_bytes = value.parse()?,
                "pool_readable" => state.pool_readable = value.parse()?,
                "pool_readable_bytes" => state.pool_readable_bytes = value.parse()?,
                "pool_hash_valid" => state.pool_hash_valid = value.parse()?,
                "pool_exact_match" => state.pool_exact_match = value.parse()?,
                "pool_failure_keys" => state.pool_failure_keys = value.parse()?,
                _ => return Err(format!("unknown state field: {key}").into()),
            }
        }
        if state.version != STATE_VERSION {
            return Err(format!("unsupported validation state version {}", state.version).into());
        }
        Ok(state)
    }

    fn set_source_fingerprint(&mut self, source: &Path) -> Result<(), Box<dyn std::error::Error>> {
        let metadata = fs::metadata(source.join("data.mdb"))?;
        self.source_device = metadata.dev();
        self.source_inode = metadata.ino();
        self.source_data_mdb_len = metadata.len();
        self.source_mtime_secs = metadata.mtime();
        self.source_mtime_nanos = metadata.mtime_nsec();
        Ok(())
    }

    fn verify_source_fingerprint(&self, source: &Path) -> Result<(), Box<dyn std::error::Error>> {
        let mut current = Self::default();
        current.set_source_fingerprint(source)?;
        if (
            current.source_device,
            current.source_inode,
            current.source_data_mdb_len,
            current.source_mtime_secs,
            current.source_mtime_nanos,
        ) != (
            self.source_device,
            self.source_inode,
            self.source_data_mdb_len,
            self.source_mtime_secs,
            self.source_mtime_nanos,
        ) {
            return Err(format!(
                "source data.mdb changed during validation: expected dev={} ino={} len={} mtime={}.{}, found dev={} ino={} len={} mtime={}.{}",
                self.source_device,
                self.source_inode,
                self.source_data_mdb_len,
                self.source_mtime_secs,
                self.source_mtime_nanos,
                current.source_device,
                current.source_inode,
                current.source_data_mdb_len,
                current.source_mtime_secs,
                current.source_mtime_nanos
            )
            .into());
        }
        Ok(())
    }

    fn encode(&self) -> String {
        let cursor = self.cursor.as_ref().map(to_hex).unwrap_or_default();
        format!(
            concat!(
                "version={}\n",
                "source_path={}\n",
                "pool_path={}\n",
                "source_device={}\n",
                "source_inode={}\n",
                "source_data_mdb_len={}\n",
                "source_mtime_secs={}\n",
                "source_mtime_nanos={}\n",
                "cursor={}\n",
                "complete={}\n",
                "active_elapsed_ms={}\n",
                "source_keys={}\n",
                "source_size_known={}\n",
                "source_declared_bytes={}\n",
                "source_readable={}\n",
                "source_readable_bytes={}\n",
                "source_hash_valid={}\n",
                "source_failure_keys={}\n",
                "pool_catalog_present={}\n",
                "pool_size_known={}\n",
                "pool_declared_bytes={}\n",
                "pool_readable={}\n",
                "pool_readable_bytes={}\n",
                "pool_hash_valid={}\n",
                "pool_exact_match={}\n",
                "pool_failure_keys={}\n"
            ),
            self.version,
            self.source_path,
            self.pool_path,
            self.source_device,
            self.source_inode,
            self.source_data_mdb_len,
            self.source_mtime_secs,
            self.source_mtime_nanos,
            cursor,
            self.complete,
            self.active_elapsed_ms,
            self.source_keys,
            self.source_size_known,
            self.source_declared_bytes,
            self.source_readable,
            self.source_readable_bytes,
            self.source_hash_valid,
            self.source_failure_keys,
            self.pool_catalog_present,
            self.pool_size_known,
            self.pool_declared_bytes,
            self.pool_readable,
            self.pool_readable_bytes,
            self.pool_hash_valid,
            self.pool_exact_match,
            self.pool_failure_keys
        )
    }
}

fn persist_state(path: &Path, state: &ValidationState) -> Result<(), Box<dyn std::error::Error>> {
    let parent = path
        .parent()
        .ok_or("validation state path has no parent directory")?;
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension("tmp");
    let mut file = File::create(&temporary)?;
    file.write_all(state.encode().as_bytes())?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn position_fraction(cursor: Option<Hash>) -> f64 {
    let Some(cursor) = cursor else {
        return 0.0;
    };
    let mut prefix = [0u8; 8];
    prefix.copy_from_slice(&cursor[..8]);
    u64::from_be_bytes(prefix) as f64 / u64::MAX as f64
}

fn print_progress(state: &ValidationState) {
    let seconds = state.active_elapsed_ms as f64 / 1_000.0;
    let position = position_fraction(state.cursor);
    let eta_seconds = if seconds > 0.0 && position > 0.0 {
        seconds * (1.0 - position) / position
    } else {
        f64::INFINITY
    };
    println!(
        concat!(
            "validation checkpoint: cursor={} complete={} source_keys={} ",
            "source_declared_bytes={} source_readable={} source_hash_valid={} ",
            "source_failures={} pool_present={} pool_readable={} pool_hash_valid={} ",
            "pool_exact_match={} pool_failures={} active_seconds={:.3} ",
            "items_per_second={:.3} keyspace_percent={:.6} eta_seconds={:.0}"
        ),
        state.cursor.as_ref().map(to_hex).unwrap_or_default(),
        state.complete,
        state.source_keys,
        state.source_declared_bytes,
        state.source_readable,
        state.source_hash_valid,
        state.source_failure_keys,
        state.pool_catalog_present,
        state.pool_readable,
        state.pool_hash_valid,
        state.pool_exact_match,
        state.pool_failure_keys,
        seconds,
        if seconds > 0.0 {
            state.source_keys as f64 / seconds
        } else {
            0.0
        },
        position * 100.0,
        eta_seconds
    );
}

fn log_failure(samples: &mut u64, hash: &Hash, message: impl AsRef<str>) {
    if *samples < FAILURE_SAMPLE_LIMIT {
        eprintln!(
            "validation failure: hash={} {}",
            to_hex(hash),
            message.as_ref()
        );
        *samples += 1;
        if *samples == FAILURE_SAMPLE_LIMIT {
            eprintln!("validation failure sample limit reached; counters continue");
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().collect::<Vec<_>>();
    if !(args.len() == 6 || args.len() == 7) {
        return Err(
            "usage: pool_migration_validate SOURCE SOURCE_EXTERNAL POOL STATE BATCH_SIZE [MAX_ITEMS]"
                .into(),
        );
    }
    let source_path = PathBuf::from(&args[1]);
    let source_external = PathBuf::from(&args[2]);
    let pool_path = PathBuf::from(&args[3]);
    let state_path = PathBuf::from(&args[4]);
    let batch_size = args[5].parse::<usize>()?;
    let max_items = args.get(6).map(|value| value.parse::<u64>()).transpose()?;
    if batch_size == 0 || max_items == Some(0) {
        return Err("batch size and max items must be non-zero".into());
    }
    if !source_path.join("data.mdb").is_file() {
        return Err(format!("source LMDB is missing: {}", source_path.display()).into());
    }
    if !source_external.is_dir() {
        return Err(format!(
            "source external blob directory is missing: {}",
            source_external.display()
        )
        .into());
    }
    if !pool_path.join("data.mdb").is_file() {
        return Err(format!("Pool catalog is missing: {}", pool_path.display()).into());
    }

    let mut state = if state_path.is_file() {
        ValidationState::load(&state_path)?
    } else {
        ValidationState::new(&source_path, &pool_path)?
    };
    if state.source_path != source_path.display().to_string()
        || state.pool_path != pool_path.display().to_string()
    {
        return Err("validation state belongs to different source or Pool paths".into());
    }
    state.verify_source_fingerprint(&source_path)?;
    if state.complete {
        print_progress(&state);
        return if state.source_failure_keys == 0 && state.pool_failure_keys == 0 {
            Ok(())
        } else {
            Err("completed validation contains failures".into())
        };
    }

    let external = ExternalBlobOptions {
        base_path: source_external,
        min_bytes: 1,
        sync: true,
        pack_target_bytes: None,
    };
    let source = LmdbBlobReader::open(&source_path, Some(external))?;
    let mut pool_config = PoolStoreConfig::default();
    pool_config.temperature.enabled = false;
    let pool = PoolStoreReader::open(&pool_path, pool_config)?;
    let run_started = Instant::now();
    let base_elapsed_ms = state.active_elapsed_ms;
    let invocation_start_count = state.source_keys;
    let mut failure_samples = 0u64;

    loop {
        let invocation_processed = state.source_keys.saturating_sub(invocation_start_count);
        let remaining = max_items
            .map(|limit| limit.saturating_sub(invocation_processed))
            .unwrap_or(batch_size as u64);
        if remaining == 0 {
            state.active_elapsed_ms =
                base_elapsed_ms.saturating_add(run_started.elapsed().as_millis() as u64);
            state.verify_source_fingerprint(&source_path)?;
            persist_state(&state_path, &state)?;
            print_progress(&state);
            return Ok(());
        }
        let limit = batch_size.min(usize::try_from(remaining).unwrap_or(usize::MAX));
        let hashes = source.scan_hashes_after(state.cursor, limit)?;
        if hashes.is_empty() {
            state.complete = true;
            state.active_elapsed_ms =
                base_elapsed_ms.saturating_add(run_started.elapsed().as_millis() as u64);
            state.verify_source_fingerprint(&source_path)?;
            persist_state(&state_path, &state)?;
            print_progress(&state);
            return if state.source_failure_keys == 0 && state.pool_failure_keys == 0 {
                Ok(())
            } else {
                Err("exhaustive Pool migration validation completed with failures".into())
            };
        }

        for hash in &hashes {
            state.source_keys = state.source_keys.saturating_add(1);
            let mut source_failed = false;
            let source_size = match source.blob_size_sync(hash) {
                Ok(Some(size)) => {
                    state.source_size_known = state.source_size_known.saturating_add(1);
                    state.source_declared_bytes = state.source_declared_bytes.saturating_add(size);
                    Some(size)
                }
                Ok(None) => {
                    source_failed = true;
                    log_failure(
                        &mut failure_samples,
                        hash,
                        "source size metadata is missing",
                    );
                    None
                }
                Err(error) => {
                    source_failed = true;
                    log_failure(
                        &mut failure_samples,
                        hash,
                        format!("source size read failed: {error}"),
                    );
                    None
                }
            };
            let source_data = match source.get_sync(hash) {
                Ok(Some(data)) => {
                    state.source_readable = state.source_readable.saturating_add(1);
                    state.source_readable_bytes = state
                        .source_readable_bytes
                        .saturating_add(data.len() as u64);
                    if sha256(&data) == *hash {
                        state.source_hash_valid = state.source_hash_valid.saturating_add(1);
                    } else {
                        source_failed = true;
                        log_failure(&mut failure_samples, hash, "source SHA-256 mismatch");
                    }
                    if source_size.is_some_and(|size| size != data.len() as u64) {
                        source_failed = true;
                        log_failure(
                            &mut failure_samples,
                            hash,
                            "source declared/readable size mismatch",
                        );
                    }
                    Some(data)
                }
                Ok(None) => {
                    source_failed = true;
                    log_failure(&mut failure_samples, hash, "source payload is missing");
                    None
                }
                Err(error) => {
                    source_failed = true;
                    log_failure(
                        &mut failure_samples,
                        hash,
                        format!("source payload read failed: {error}"),
                    );
                    None
                }
            };
            if source_failed {
                state.source_failure_keys = state.source_failure_keys.saturating_add(1);
            }

            let mut pool_failed = false;
            match pool.blob_location(hash) {
                Ok(Some(_)) => {
                    state.pool_catalog_present = state.pool_catalog_present.saturating_add(1)
                }
                Ok(None) => {
                    pool_failed = true;
                    log_failure(&mut failure_samples, hash, "Pool catalog entry is missing");
                }
                Err(error) => {
                    pool_failed = true;
                    log_failure(
                        &mut failure_samples,
                        hash,
                        format!("Pool catalog read failed: {error}"),
                    );
                }
            }
            let pool_size = match pool.blob_size_sync(hash) {
                Ok(Some(size)) => {
                    state.pool_size_known = state.pool_size_known.saturating_add(1);
                    state.pool_declared_bytes = state.pool_declared_bytes.saturating_add(size);
                    Some(size)
                }
                Ok(None) => {
                    pool_failed = true;
                    log_failure(&mut failure_samples, hash, "Pool size metadata is missing");
                    None
                }
                Err(error) => {
                    pool_failed = true;
                    log_failure(
                        &mut failure_samples,
                        hash,
                        format!("Pool size read failed: {error}"),
                    );
                    None
                }
            };
            match pool.get_sync(hash) {
                Ok(Some(data)) => {
                    state.pool_readable = state.pool_readable.saturating_add(1);
                    state.pool_readable_bytes =
                        state.pool_readable_bytes.saturating_add(data.len() as u64);
                    if sha256(&data) == *hash {
                        state.pool_hash_valid = state.pool_hash_valid.saturating_add(1);
                    } else {
                        pool_failed = true;
                        log_failure(&mut failure_samples, hash, "Pool SHA-256 mismatch");
                    }
                    if pool_size.is_some_and(|size| size != data.len() as u64) {
                        pool_failed = true;
                        log_failure(
                            &mut failure_samples,
                            hash,
                            "Pool declared/readable size mismatch",
                        );
                    }
                    if let Some(source_data) = source_data.as_ref() {
                        if source_data == &data {
                            state.pool_exact_match = state.pool_exact_match.saturating_add(1);
                        } else {
                            pool_failed = true;
                            log_failure(
                                &mut failure_samples,
                                hash,
                                "Pool bytes differ from source",
                            );
                        }
                    }
                }
                Ok(None) => {
                    pool_failed = true;
                    log_failure(&mut failure_samples, hash, "Pool payload is missing");
                }
                Err(error) => {
                    pool_failed = true;
                    log_failure(
                        &mut failure_samples,
                        hash,
                        format!("Pool payload read failed: {error}"),
                    );
                }
            }
            if pool_failed {
                state.pool_failure_keys = state.pool_failure_keys.saturating_add(1);
            }
        }

        state.cursor = hashes.last().copied();
        state.active_elapsed_ms =
            base_elapsed_ms.saturating_add(run_started.elapsed().as_millis() as u64);
        state.verify_source_fingerprint(&source_path)?;
        persist_state(&state_path, &state)?;
        print_progress(&state);
    }
}
