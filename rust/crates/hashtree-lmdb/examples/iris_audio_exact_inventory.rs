use hashtree_core::{decode_tree_node, decrypt_chk, from_hex, key_from_hex, sha256, to_hex, Hash};
use hashtree_lmdb::{
    ExternalBlobOptions, LmdbBlobReader, PoolMemberId, PoolStoreConfig, PoolStoreReader,
};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

const BATCH_ROWS: usize = 1024;
const READ_LIMIT_BYTES: u64 = 128 * 1024 * 1024;
const MEMBER_01_ID: &str = "2d8fedda-28a5-4b25-af94-1e09105cad6f";
const MEMBER_02_ID: &str = "76d96b84-03e6-4978-899d-7f15a0766856";
const OUTPUT_HEADER: &str = concat!(
    "sourceKey\tsongId\thash\tkey\tinputLine\tcatalogCandidates\tpresentWitness\t",
    "bodySha256\tchkAuth\trootDecode\tpoolMember01\tpoolMember02\t",
    "legacy\thot\tstateHot\texhaustiveAllTierAbsent\tintegrityFailure\tprobeMode"
);

#[derive(Clone)]
struct InputRow {
    source_key: String,
    song_id: String,
    hash_hex: String,
    key_hex: String,
    hash: Hash,
    input_line: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProbeState {
    NotProbed,
    Absent,
    ShaMismatch,
    ShaOk,
}

impl ProbeState {
    fn label(self) -> &'static str {
        match self {
            Self::NotProbed => "not_probed",
            Self::Absent => "absent",
            Self::ShaMismatch => "sha_mismatch",
            Self::ShaOk => "sha_ok",
        }
    }
}

struct RowResult {
    catalog_candidates: Vec<PoolMemberId>,
    witness: Option<&'static str>,
    body_sha: &'static str,
    chk_auth: &'static str,
    root_decode: &'static str,
    tiers: [ProbeState; 5],
}

impl RowResult {
    fn new(catalog_candidates: Vec<PoolMemberId>) -> Self {
        Self {
            catalog_candidates,
            witness: None,
            body_sha: "not_checked",
            chk_auth: "not_checked",
            root_decode: "not_checked",
            tiers: [ProbeState::NotProbed; 5],
        }
    }

    fn resolved(&self) -> bool {
        self.witness.is_some()
    }
}

struct Tier<'a> {
    index: usize,
    label: &'static str,
    reader: &'a LmdbBlobReader,
}

fn external(base_path: impl Into<PathBuf>) -> Option<ExternalBlobOptions> {
    Some(ExternalBlobOptions {
        base_path: base_path.into(),
        min_bytes: 1,
        sync: false,
        pack_target_bytes: None,
    })
}

fn sync_checkpoint(
    checkpoint: &Path,
    input_sha: &str,
    next_row: usize,
    output_bytes: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let temporary = checkpoint.with_extension("tmp");
    {
        let mut file = File::create(&temporary)?;
        writeln!(file, "input_sha={input_sha}")?;
        writeln!(file, "next_row={next_row}")?;
        writeln!(file, "output_bytes={output_bytes}")?;
        file.sync_all()?;
    }
    fs::rename(temporary, checkpoint)?;
    File::open(checkpoint.parent().unwrap_or_else(|| Path::new(".")))?.sync_all()?;
    Ok(())
}

fn read_checkpoint(
    checkpoint: &Path,
    expected_input_sha: &str,
) -> Result<Option<(usize, u64)>, Box<dyn std::error::Error>> {
    if !checkpoint.exists() {
        return Ok(None);
    }
    let mut values = BTreeMap::new();
    for line in BufReader::new(File::open(checkpoint)?).lines() {
        let line = line?;
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("invalid checkpoint line: {line}"))?;
        values.insert(key.to_string(), value.to_string());
    }
    if values.get("input_sha").map(String::as_str) != Some(expected_input_sha) {
        return Err("checkpoint input SHA256 does not match input".into());
    }
    let next_row = values
        .get("next_row")
        .ok_or("checkpoint has no next_row")?
        .parse()?;
    let output_bytes = values
        .get("output_bytes")
        .ok_or("checkpoint has no output_bytes")?
        .parse()?;
    Ok(Some((next_row, output_bytes)))
}

fn load_rows(path: &Path) -> Result<(Vec<InputRow>, String), Box<dyn std::error::Error>> {
    let input_bytes = fs::read(path)?;
    let input_sha = to_hex(&sha256(&input_bytes));
    let mut lines = BufReader::new(input_bytes.as_slice()).lines();
    let header = lines.next().transpose()?.ok_or("input TSV is empty")?;
    if header != "sourceKey\tsongId\thash\tkey" {
        return Err(format!("unexpected input header: {header}").into());
    }
    let mut rows = Vec::new();
    for (index, line) in lines.enumerate() {
        let line = line?;
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.len() != 4 {
            return Err(format!("invalid input row {}: {line}", index + 2).into());
        }
        let hash = from_hex(fields[2])?;
        if to_hex(&hash) != fields[2] {
            return Err(format!("non-canonical hash on row {}", index + 2).into());
        }
        key_from_hex(fields[3])
            .map_err(|error| format!("invalid CHK key on row {}: {error}", index + 2))?;
        rows.push(InputRow {
            source_key: fields[0].to_string(),
            song_id: fields[1].to_string(),
            hash_hex: fields[2].to_string(),
            key_hex: fields[3].to_string(),
            hash,
            input_line: index + 2,
        });
    }
    rows.sort_unstable_by(|left, right| {
        left.hash
            .cmp(&right.hash)
            .then_with(|| left.source_key.cmp(&right.source_key))
            .then_with(|| left.song_id.cmp(&right.song_id))
            .then_with(|| left.key_hex.cmp(&right.key_hex))
            .then_with(|| left.input_line.cmp(&right.input_line))
    });
    Ok((rows, input_sha))
}

fn probe_tier(
    tier: &Tier<'_>,
    requested_rows: &[usize],
    rows: &[InputRow],
    results: &mut [RowResult],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut rows_by_hash = BTreeMap::<Hash, Vec<usize>>::new();
    for &row_index in requested_rows {
        if !results[row_index].resolved()
            && results[row_index].tiers[tier.index] == ProbeState::NotProbed
        {
            rows_by_hash
                .entry(rows[row_index].hash)
                .or_default()
                .push(row_index);
        }
    }
    if rows_by_hash.is_empty() {
        return Ok(());
    }

    let hashes = rows_by_hash.keys().copied().collect::<Vec<_>>();
    let present = tier.reader.existing_hashes_in_sorted_candidates(&hashes)?;
    let mut present_hashes = Vec::new();
    for (hash, is_present) in hashes.into_iter().zip(present) {
        if is_present {
            present_hashes.push(hash);
        } else if let Some(indices) = rows_by_hash.get(&hash) {
            for &row_index in indices {
                results[row_index].tiers[tier.index] = ProbeState::Absent;
            }
        }
    }

    let mut offset = 0usize;
    while offset < present_hashes.len() {
        let bodies = tier
            .reader
            .read_hashes_bounded(&present_hashes[offset..], READ_LIMIT_BYTES)?;
        if bodies.is_empty() {
            return Err(format!("{} batch reader made no progress", tier.label).into());
        }
        offset += bodies.len();
        for (hash, body) in bodies {
            let sha_ok = sha256(&body) == hash;
            let indices = rows_by_hash
                .get(&hash)
                .ok_or("batch reader returned an unrequested hash")?;
            for &row_index in indices {
                let result = &mut results[row_index];
                if sha_ok {
                    result.tiers[tier.index] = ProbeState::ShaOk;
                    if result.witness.is_none() {
                        result.witness = Some(tier.label);
                        result.body_sha = "ok";
                        let key = key_from_hex(&rows[row_index].key_hex)?;
                        match decrypt_chk(&body, &key) {
                            Ok(plaintext) => {
                                result.chk_auth = "ok";
                                result.root_decode = if decode_tree_node(&plaintext).is_ok() {
                                    "ok"
                                } else {
                                    "decode_failed"
                                };
                            }
                            Err(_) => {
                                result.chk_auth = "decrypt_failed";
                                result.root_decode = "not_checked";
                            }
                        }
                    }
                } else {
                    result.tiers[tier.index] = ProbeState::ShaMismatch;
                    result.body_sha = "mismatch";
                }
            }
        }
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().collect::<Vec<_>>();
    if args.len() != 4 {
        return Err("usage: iris_audio_exact_inventory INPUT_TSV OUTPUT_TSV CHECKPOINT".into());
    }
    let input_path = Path::new(&args[1]);
    let output_path = Path::new(&args[2]);
    let checkpoint_path = Path::new(&args[3]);
    let (rows, input_sha) = load_rows(input_path)?;

    let member_01 = LmdbBlobReader::open(
        "/srv/hashtree-hot/pool-members/member-01",
        external("/srv/hashtree-hot/pool-files/member-01"),
    )?;
    let member_02 = LmdbBlobReader::open(
        "/srv/hashtree-hot/pool-members/member-02",
        external("/srv/hashtree/pool-files/member-02"),
    )?;
    let legacy = LmdbBlobReader::open(
        "/srv/hashtree/state/blobs",
        external("/srv/hashtree/state/blob-files-v1"),
    )?;
    let hot = LmdbBlobReader::open(
        "/srv/hashtree-hot/blobs-hot",
        external("/srv/hashtree-hot/blob-files-v1"),
    )?;
    let state_hot = LmdbBlobReader::open(
        "/srv/hashtree/state/blobs-hot",
        external("/srv/hashtree/state/blob-files-v1"),
    )?;
    let mut pool_config = PoolStoreConfig::default();
    pool_config.temperature.enabled = false;
    let pool = PoolStoreReader::open("/srv/hashtree/state/blob-pool-v1", pool_config)?;
    let member_01_id = PoolMemberId::from_str(MEMBER_01_ID)?;
    let member_02_id = PoolMemberId::from_str(MEMBER_02_ID)?;
    let tiers = [
        Tier {
            index: 0,
            label: "pool_member_01",
            reader: &member_01,
        },
        Tier {
            index: 1,
            label: "pool_member_02",
            reader: &member_02,
        },
        Tier {
            index: 2,
            label: "legacy",
            reader: &legacy,
        },
        Tier {
            index: 3,
            label: "hot",
            reader: &hot,
        },
        Tier {
            index: 4,
            label: "state_hot",
            reader: &state_hot,
        },
    ];

    let (mut next_row, output_bytes) =
        read_checkpoint(checkpoint_path, &input_sha)?.unwrap_or((0, 0));
    if next_row > rows.len() {
        return Err("checkpoint is past end of input".into());
    }
    let mut output_file = if next_row == 0 {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(output_path)?;
        writeln!(file, "{OUTPUT_HEADER}")?;
        file.sync_data()?;
        let output_bytes = file.stream_position()?;
        sync_checkpoint(checkpoint_path, &input_sha, 0, output_bytes)?;
        file
    } else {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(output_path)?;
        file.set_len(output_bytes)?;
        file.seek(SeekFrom::Start(output_bytes))?;
        file
    };
    let mut output = BufWriter::new(&mut output_file);

    while next_row < rows.len() {
        let end = (next_row + BATCH_ROWS).min(rows.len());
        let batch_rows = &rows[next_row..end];
        let hashes = batch_rows.iter().map(|row| row.hash).collect::<Vec<_>>();
        let catalog_candidates = pool.blob_member_candidates(&hashes)?;
        if let Some(unknown) = catalog_candidates
            .iter()
            .flatten()
            .find(|member| **member != member_01_id && **member != member_02_id)
        {
            return Err(format!(
                "Pool catalog references unknown member {unknown}; refusing to classify absence"
            )
            .into());
        }
        let mut results = catalog_candidates
            .into_iter()
            .map(RowResult::new)
            .collect::<Vec<_>>();

        // First verify the Pool catalog's exact physical member candidates.
        // A full-body hash-valid witness is sufficient because SHA256 fixes the
        // bytes; only unresolved rows need the exhaustive fallback search.
        for (tier, member_id) in [(&tiers[0], member_01_id), (&tiers[1], member_02_id)] {
            let requested = results
                .iter()
                .enumerate()
                .filter(|(_, result)| result.catalog_candidates.contains(&member_id))
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            probe_tier(tier, &requested, batch_rows, &mut results)?;
        }

        // Missing or stale Pool entries must inspect every physical tier until
        // a hash-valid body is found. If no witness exists, all five tiers have
        // necessarily been probed, which makes absence exact rather than an
        // inference from catalog metadata.
        for tier in &tiers {
            let unresolved = results
                .iter()
                .enumerate()
                .filter(|(_, result)| !result.resolved())
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            probe_tier(tier, &unresolved, batch_rows, &mut results)?;
        }

        for (row, result) in batch_rows.iter().zip(results.iter()) {
            let catalog = result
                .catalog_candidates
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",");
            let exhaustive_absent = result
                .tiers
                .iter()
                .all(|state| *state == ProbeState::Absent);
            let integrity_failure = result.tiers.contains(&ProbeState::ShaMismatch)
                || result.chk_auth == "decrypt_failed"
                || result.root_decode == "decode_failed";
            let probe_mode = if let Some(witness) = result.witness {
                let witness_member = match witness {
                    "pool_member_01" => Some(member_01_id),
                    "pool_member_02" => Some(member_02_id),
                    _ => None,
                };
                if witness_member.is_some_and(|member| result.catalog_candidates.contains(&member))
                {
                    "catalog_witness"
                } else {
                    "fallback_witness"
                }
            } else if exhaustive_absent {
                "exhaustive_absent"
            } else {
                "exhaustive_integrity_failure"
            };
            writeln!(
                output,
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                row.source_key,
                row.song_id,
                row.hash_hex,
                row.key_hex,
                row.input_line,
                catalog,
                result.witness.unwrap_or("none"),
                result.body_sha,
                result.chk_auth,
                result.root_decode,
                result.tiers[0].label(),
                result.tiers[1].label(),
                result.tiers[2].label(),
                result.tiers[3].label(),
                result.tiers[4].label(),
                u8::from(exhaustive_absent),
                u8::from(integrity_failure),
                probe_mode
            )?;
        }
        output.flush()?;
        output.get_ref().sync_data()?;
        let committed_bytes = output.get_mut().stream_position()?;
        sync_checkpoint(checkpoint_path, &input_sha, end, committed_bytes)?;
        next_row = end;
        eprintln!(
            "inventory_progress={next_row}/{} output_bytes={committed_bytes}",
            rows.len()
        );
    }
    output.flush()?;
    output.get_ref().sync_data()?;
    eprintln!("inventory_complete={} input_sha256={input_sha}", rows.len());
    Ok(())
}
