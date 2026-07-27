use crate::config::{
    canonical_hash, inventory_identity_sha256, load_config_bytes, ValidatedConfig,
};
use crate::io::load_inventory_bytes;
use crate::model::{AuditSummary, INVENTORY_IDENTITY_SCHEMA, LEDGER_ROW_SCHEMA, MANIFEST_SCHEMA};
use crate::probe::{verify_terminal_target_residency, ProbeContext};
use hashtree_core::{sha256, to_hex, Hash};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const WITNESS_SCHEMA: &str = "iris-audio-target-pool-current-state-witness/v1";
pub const MAX_WITNESS_JSON_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub struct WitnessPaths {
    pub config: PathBuf,
    pub inventory: PathBuf,
    pub ledger: PathBuf,
    pub manifest: PathBuf,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentStateWitness {
    pub schema: &'static str,
    pub challenge: String,
    pub started_at: String,
    pub verified_at: String,
    pub input_sha256: WitnessInputSha256,
    pub inventory_identity: WitnessInventoryIdentity,
    pub ledger: WitnessLedger,
    pub pool_manifest: WitnessPoolManifest,
    pub verified_unique_block_hashes: u64,
    pub release_ready: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WitnessInputSha256 {
    pub config: String,
    pub inventory: String,
    pub ledger: String,
    pub manifest: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WitnessInventoryIdentity {
    pub schema: &'static str,
    pub sha256: String,
    pub records: usize,
    pub identity_sha256: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WitnessLedger {
    pub schema: &'static str,
    pub bytes: u64,
    pub rows: u64,
    pub unique_block_hashes: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WitnessPoolManifest {
    pub sha256: String,
    pub generation: u64,
    pub member_ids: Vec<String>,
    pub target_member_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RawFileIdentity {
    sha256: String,
    bytes: u64,
}

#[derive(Debug)]
struct LedgerSnapshot {
    identity: RawFileIdentity,
    hashes: Vec<Hash>,
    rows: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ExistingAuditManifest {
    schema: String,
    config_sha256: String,
    inventory: ExistingInventoryIdentity,
    target: ExistingTargetManifest,
    work_items: usize,
    ledger: ExistingLedgerManifest,
    summary: AuditSummary,
    release_ready: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ExistingInventoryIdentity {
    schema: String,
    sha256: String,
    records: usize,
    identity_sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ExistingTargetManifest {
    pool_manifest_sha256: String,
    pool_manifest_generation: u64,
    expected_pool_member_ids: Vec<String>,
    target_member_ids: Vec<String>,
    fallback_tier_names: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ExistingLedgerManifest {
    schema: String,
    sha256: String,
    bytes: u64,
    rows: u64,
    unique_block_hashes: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExistingLedgerRow {
    schema: String,
    block_hash: String,
}

pub fn verify_existing(
    paths: &WitnessPaths,
    challenge: &str,
) -> Result<CurrentStateWitness, Box<dyn std::error::Error>> {
    canonical_hash("challenge", challenge)
        .map_err(|error| format!("challenge must be canonical 64-lowerhex: {error}"))?;
    let started_at_millis = current_unix_millis()?;
    let started_at = canonical_utc_millis(started_at_millis)?;

    let config_bytes = read_regular_file(&paths.config, "config")?;
    let config_identity = identity_from_bytes(&config_bytes)?;
    let validated = load_config_bytes(&config_bytes)?;

    let inventory_bytes = read_regular_file(&paths.inventory, "inventory")?;
    let inventory_identity = identity_from_bytes(&inventory_bytes)?;
    let (inventory, inventory_sha256) = load_inventory_bytes(
        &inventory_bytes,
        &validated.config.expected_inventory_sha256,
        validated.config.expected_inventory_records,
    )?;
    let inventory_records = inventory.len();
    drop(inventory);
    let inventory_identity_sha256 = inventory_identity_sha256(&inventory_sha256, inventory_records);

    let ledger = read_ledger_snapshot(&paths.ledger)?;
    let manifest_bytes = read_regular_file(&paths.manifest, "manifest")?;
    let manifest_identity = identity_from_bytes(&manifest_bytes)?;
    let manifest: ExistingAuditManifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|error| format!("invalid terminal audit manifest: {error}"))?;

    validate_terminal_manifest(
        &manifest,
        &validated,
        &config_identity,
        &inventory_identity,
        &inventory_identity_sha256,
        &ledger,
    )?;

    let probe = ProbeContext::open(&validated)?;
    let live_pool_manifest = probe.pool_manifest_identity();
    let live_pool_manifest_sha256 = to_hex(&live_pool_manifest.sha256);
    let live_pool_member_ids = live_pool_manifest
        .member_ids
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let live_target_member_ids = probe.target_member_labels();
    drop(probe);

    if live_pool_manifest_sha256 != manifest.target.pool_manifest_sha256 {
        return Err(format!(
            "live Pool manifest SHA256 differs from terminal manifest: {} != {}",
            live_pool_manifest_sha256, manifest.target.pool_manifest_sha256
        )
        .into());
    }
    if live_pool_manifest.generation != manifest.target.pool_manifest_generation {
        return Err(format!(
            "live Pool manifest generation differs from terminal manifest: {} != {}",
            live_pool_manifest.generation, manifest.target.pool_manifest_generation
        )
        .into());
    }
    if live_pool_member_ids != manifest.target.expected_pool_member_ids {
        return Err("live Pool manifest member IDs differ from terminal manifest".into());
    }
    if live_target_member_ids != manifest.target.target_member_ids {
        return Err("live target member IDs differ from terminal manifest".into());
    }

    verify_terminal_target_residency(&validated, &live_pool_manifest, &ledger.hashes)?;

    verify_raw_input_unchanged("config", &paths.config, &config_identity)?;
    verify_raw_input_unchanged("inventory", &paths.inventory, &inventory_identity)?;
    verify_raw_input_unchanged("ledger", &paths.ledger, &ledger.identity)?;
    verify_raw_input_unchanged("manifest", &paths.manifest, &manifest_identity)?;

    let verified_at_millis = current_unix_millis()?.max(started_at_millis);
    let verified_at = canonical_utc_millis(verified_at_millis)?;
    let unique_block_hashes = u64::try_from(ledger.hashes.len())
        .map_err(|_| "ledger unique block-hash count exceeds u64")?;
    Ok(CurrentStateWitness {
        schema: WITNESS_SCHEMA,
        challenge: challenge.to_owned(),
        started_at,
        verified_at,
        input_sha256: WitnessInputSha256 {
            config: config_identity.sha256,
            inventory: inventory_identity.sha256.clone(),
            ledger: ledger.identity.sha256,
            manifest: manifest_identity.sha256,
        },
        inventory_identity: WitnessInventoryIdentity {
            schema: INVENTORY_IDENTITY_SCHEMA,
            sha256: inventory_identity.sha256,
            records: inventory_records,
            identity_sha256: inventory_identity_sha256,
        },
        ledger: WitnessLedger {
            schema: LEDGER_ROW_SCHEMA,
            bytes: ledger.identity.bytes,
            rows: ledger.rows,
            unique_block_hashes,
        },
        pool_manifest: WitnessPoolManifest {
            sha256: live_pool_manifest_sha256,
            generation: live_pool_manifest.generation,
            member_ids: live_pool_member_ids,
            target_member_ids: live_target_member_ids,
        },
        verified_unique_block_hashes: unique_block_hashes,
        release_ready: true,
    })
}

pub fn witness_json_line(
    witness: &CurrentStateWitness,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut line = serde_json::to_vec(witness)?;
    if line.len().saturating_add(1) > MAX_WITNESS_JSON_BYTES {
        return Err(format!(
            "witness JSON exceeds the {}-byte output bound",
            MAX_WITNESS_JSON_BYTES
        )
        .into());
    }
    line.push(b'\n');
    Ok(line)
}

fn validate_terminal_manifest(
    manifest: &ExistingAuditManifest,
    validated: &ValidatedConfig,
    config_identity: &RawFileIdentity,
    inventory_identity: &RawFileIdentity,
    inventory_identity_sha256: &str,
    ledger: &LedgerSnapshot,
) -> Result<(), Box<dyn std::error::Error>> {
    if manifest.schema != MANIFEST_SCHEMA {
        return Err(format!(
            "unsupported terminal manifest schema {}; expected {MANIFEST_SCHEMA}",
            manifest.schema
        )
        .into());
    }
    if !manifest.release_ready {
        return Err("terminal manifest releaseReady must be true".into());
    }
    canonical_hash("terminal manifest configSha256", &manifest.config_sha256)?;
    if manifest.config_sha256 != config_identity.sha256
        || manifest.config_sha256 != validated.config_sha256
    {
        return Err("terminal manifest config identity does not match raw config".into());
    }

    if manifest.inventory.schema != INVENTORY_IDENTITY_SCHEMA {
        return Err("terminal manifest inventory identity schema is unsupported".into());
    }
    canonical_hash(
        "terminal manifest inventory SHA256",
        &manifest.inventory.sha256,
    )?;
    canonical_hash(
        "terminal manifest inventory identity SHA256",
        &manifest.inventory.identity_sha256,
    )?;
    if manifest.inventory.sha256 != inventory_identity.sha256
        || manifest.inventory.sha256 != validated.config.expected_inventory_sha256
        || manifest.inventory.records != validated.config.expected_inventory_records
        || manifest.inventory.identity_sha256 != inventory_identity_sha256
    {
        return Err("terminal manifest inventory identity does not match raw inventory".into());
    }

    if manifest.ledger.schema != LEDGER_ROW_SCHEMA {
        return Err("terminal manifest ledger schema is unsupported".into());
    }
    canonical_hash("terminal manifest ledger SHA256", &manifest.ledger.sha256)?;
    let unique_block_hashes = u64::try_from(ledger.hashes.len())
        .map_err(|_| "ledger unique block-hash count exceeds u64")?;
    if manifest.ledger.sha256 != ledger.identity.sha256
        || manifest.ledger.bytes != ledger.identity.bytes
        || manifest.ledger.rows != ledger.rows
        || manifest.ledger.unique_block_hashes != unique_block_hashes
    {
        return Err("terminal manifest ledger identity or counts do not match raw ledger".into());
    }

    let expected_work_items = validated
        .config
        .expected_inventory_records
        .checked_add(validated.config.additional_roots.len())
        .ok_or("expected work-item count overflow")?;
    if manifest.work_items != expected_work_items {
        return Err("terminal manifest work-item count does not match config and inventory".into());
    }
    manifest
        .summary
        .validate_checkpoint(manifest.work_items, manifest.inventory.records)
        .map_err(|error| format!("terminal manifest summary is inconsistent: {error}"))?;
    if manifest.summary.block_references != manifest.ledger.rows {
        return Err("terminal manifest summary blockReferences differs from ledger rows".into());
    }
    if !manifest.summary.release_ready(manifest.work_items) {
        return Err("terminal manifest summary is not release-ready".into());
    }

    canonical_hash(
        "terminal manifest Pool SHA256",
        &manifest.target.pool_manifest_sha256,
    )?;
    let expected_pool_member_ids = validated
        .expected_pool_members
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let target_member_ids = validated
        .target_members
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let fallback_tier_names = validated
        .config
        .fallback_tiers
        .iter()
        .map(|tier| tier.name.clone())
        .collect::<Vec<_>>();
    if manifest.target.expected_pool_member_ids != expected_pool_member_ids {
        return Err("terminal manifest full Pool member IDs differ from config".into());
    }
    if manifest.target.target_member_ids != target_member_ids {
        return Err("terminal manifest target member IDs differ from config".into());
    }
    if manifest.target.fallback_tier_names != fallback_tier_names {
        return Err("terminal manifest fallback tier names differ from config".into());
    }
    Ok(())
}

fn read_ledger_snapshot(path: &Path) -> Result<LedgerSnapshot, Box<dyn std::error::Error>> {
    let file = open_regular_file(path, "ledger")?;
    let mut input = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut bytes = 0u64;
    let mut rows = 0u64;
    let mut hashes = BTreeSet::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = input.read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        hasher.update(&line);
        bytes = bytes
            .checked_add(u64::try_from(read)?)
            .ok_or("ledger byte count overflow")?;
        if line.last() != Some(&b'\n') {
            return Err(format!("ledger row {} is not newline-terminated", rows + 1).into());
        }
        if line.contains(&b'\r') {
            return Err(format!("ledger row {} contains a carriage return", rows + 1).into());
        }
        line.pop();
        if line.is_empty() {
            return Err(format!("ledger row {} is empty", rows + 1).into());
        }
        let row: ExistingLedgerRow = serde_json::from_slice(&line)
            .map_err(|error| format!("invalid ledger row {}: {error}", rows + 1))?;
        if row.schema != LEDGER_ROW_SCHEMA {
            return Err(format!(
                "ledger row {} has unsupported schema {}",
                rows + 1,
                row.schema
            )
            .into());
        }
        let hash = canonical_hash("ledger blockHash", &row.block_hash)
            .map_err(|error| format!("invalid ledger row {}: {error}", rows + 1))?;
        hashes.insert(hash);
        rows = rows.checked_add(1).ok_or("ledger row count overflow")?;
    }
    Ok(LedgerSnapshot {
        identity: RawFileIdentity {
            sha256: format!("{:x}", hasher.finalize()),
            bytes,
        },
        hashes: hashes.into_iter().collect(),
        rows,
    })
}

fn read_regular_file(path: &Path, label: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut file = open_regular_file(path, label)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn open_regular_file(path: &Path, label: &str) -> Result<File, Box<dyn std::error::Error>> {
    let path_metadata = std::fs::symlink_metadata(path)?;
    if path_metadata.file_type().is_symlink() {
        return Err(format!(
            "{label} input must not be a symbolic link: {}",
            path.display()
        )
        .into());
    }
    if !path_metadata.is_file() {
        return Err(format!("{label} input is not a regular file: {}", path.display()).into());
    }
    let file = File::open(path)?;
    let opened_metadata = file.metadata()?;
    if !opened_metadata.is_file() {
        return Err(format!("{label} input is not a regular file: {}", path.display()).into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        if path_metadata.dev() != opened_metadata.dev()
            || path_metadata.ino() != opened_metadata.ino()
        {
            return Err(format!("{label} input changed while it was being opened").into());
        }
    }
    Ok(file)
}

fn identity_from_bytes(bytes: &[u8]) -> Result<RawFileIdentity, Box<dyn std::error::Error>> {
    Ok(RawFileIdentity {
        sha256: to_hex(&sha256(bytes)),
        bytes: u64::try_from(bytes.len())?,
    })
}

fn verify_raw_input_unchanged(
    label: &str,
    path: &Path,
    initial: &RawFileIdentity,
) -> Result<(), Box<dyn std::error::Error>> {
    let terminal = identity_from_reader(open_regular_file(path, label)?)?;
    if terminal != *initial {
        return Err(format!(
            "{label} input changed during live target-Pool verification: initial SHA256 {} bytes {}, terminal SHA256 {} bytes {}",
            initial.sha256, initial.bytes, terminal.sha256, terminal.bytes
        )
        .into());
    }
    Ok(())
}

fn identity_from_reader(file: File) -> Result<RawFileIdentity, Box<dyn std::error::Error>> {
    let mut input = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut bytes = 0u64;
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        bytes = bytes
            .checked_add(u64::try_from(read)?)
            .ok_or("input byte count overflow")?;
    }
    Ok(RawFileIdentity {
        sha256: format!("{:x}", hasher.finalize()),
        bytes,
    })
}

fn current_unix_millis() -> Result<u64, Box<dyn std::error::Error>> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "system clock is before the Unix epoch")?
        .as_millis();
    Ok(u64::try_from(millis)?)
}

fn canonical_utc_millis(unix_millis: u64) -> Result<String, Box<dyn std::error::Error>> {
    let seconds = unix_millis / 1000;
    let millis = unix_millis % 1000;
    let days = i64::try_from(seconds / 86_400)?;
    let seconds_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_unix_days(days);
    if !(0..=9999).contains(&year) {
        return Err(format!("UTC timestamp year {year} is outside canonical ISO range").into());
    }
    let hour = seconds_of_day / 3600;
    let minute = (seconds_of_day % 3600) / 60;
    let second = seconds_of_day % 60;
    Ok(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z"
    ))
}

fn civil_from_unix_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_utc_timestamp_has_fixed_millisecond_precision() {
        assert_eq!(
            canonical_utc_millis(0).expect("Unix epoch"),
            "1970-01-01T00:00:00.000Z"
        );
        assert_eq!(
            canonical_utc_millis(951_827_696_789).expect("leap day"),
            "2000-02-29T12:34:56.789Z"
        );
    }
}
