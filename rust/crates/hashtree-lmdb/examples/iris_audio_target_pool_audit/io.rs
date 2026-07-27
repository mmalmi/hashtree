use crate::config::canonical_hash;
use crate::model::InventoryRow;
use hashtree_core::{key_from_hex, sha256, to_hex};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

pub fn load_inventory(
    path: &Path,
    expected_sha256: &str,
    expected_records: usize,
) -> Result<(Vec<InventoryRow>, String), Box<dyn std::error::Error>> {
    let bytes = fs::read(path)?;
    let actual_sha256 = to_hex(&sha256(&bytes));
    if actual_sha256 != expected_sha256 {
        return Err(format!(
            "inventory SHA256 mismatch: expected {expected_sha256}, got {actual_sha256}"
        )
        .into());
    }

    let mut lines = BufReader::new(bytes.as_slice()).lines();
    let header = lines.next().transpose()?.ok_or("inventory TSV is empty")?;
    if header != "sourceKey\tsongId\thash\tkey" {
        return Err(format!("unexpected inventory header: {header}").into());
    }
    let mut rows = Vec::with_capacity(expected_records);
    for (index, line) in lines.enumerate() {
        let line = line?;
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.len() != 4 {
            return Err(format!("invalid inventory row {}: {line}", index + 2).into());
        }
        if fields[0].is_empty() || fields[1].is_empty() {
            return Err(format!("empty sourceKey or songId on row {}", index + 2).into());
        }
        let hash = canonical_hash("inventory hash", fields[2])
            .map_err(|error| format!("invalid hash on row {}: {error}", index + 2))?;
        let key = key_from_hex(fields[3])
            .map_err(|error| format!("invalid CHK key on row {}: {error}", index + 2))?;
        if to_hex(&key) != fields[3] {
            return Err(format!("non-canonical CHK key on row {}", index + 2).into());
        }
        rows.push(InventoryRow {
            source_key: fields[0].to_string(),
            song_id: fields[1].to_string(),
            hash,
            key,
            input_line: index + 2,
        });
    }
    if rows.len() != expected_records {
        return Err(format!(
            "inventory record count mismatch: expected {expected_records}, got {}",
            rows.len()
        )
        .into());
    }
    Ok((rows, actual_sha256))
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T, Box<dyn std::error::Error>> {
    Ok(serde_json::from_reader(BufReader::new(File::open(path)?))?)
}

pub fn write_atomic_json<T: serde::Serialize>(
    path: &Path,
    value: &T,
) -> Result<(), Box<dyn std::error::Error>> {
    ensure_parent(path)?;
    let temporary = temporary_path(path);
    {
        let mut output = BufWriter::new(
            OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&temporary)?,
        );
        serde_json::to_writer_pretty(&mut output, value)?;
        output.write_all(b"\n")?;
        output.flush()?;
        output.get_ref().sync_all()?;
    }
    fs::rename(&temporary, path)?;
    sync_parent(path)?;
    Ok(())
}

pub fn ensure_parent(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

pub fn sync_parent(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

pub fn sha256_file(path: &Path) -> Result<(String, u64), Box<dyn std::error::Error>> {
    let mut file = BufReader::new(File::open(path)?);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut bytes = 0u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        bytes = bytes.saturating_add(read as u64);
    }
    Ok((format!("{:x}", hasher.finalize()), bytes))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LedgerHashRow {
    block_hash: String,
}

pub fn ledger_hash_stats(path: &Path) -> Result<(u64, u64), Box<dyn std::error::Error>> {
    let mut hashes = HashSet::new();
    let mut rows = 0u64;
    for (index, line) in BufReader::new(File::open(path)?).lines().enumerate() {
        let line = line?;
        let row: LedgerHashRow = serde_json::from_str(&line)
            .map_err(|error| format!("invalid ledger row {}: {error}", index + 1))?;
        canonical_hash("ledger blockHash", &row.block_hash)
            .map_err(|error| format!("invalid ledger row {}: {error}", index + 1))?;
        hashes.insert(row.block_hash);
        rows += 1;
    }
    Ok((hashes.len() as u64, rows))
}

pub fn append_json_line<T: serde::Serialize>(
    output: &mut BufWriter<File>,
    value: &T,
) -> Result<(), Box<dyn std::error::Error>> {
    serde_json::to_writer(&mut *output, value)?;
    output.write_all(b"\n")?;
    Ok(())
}
