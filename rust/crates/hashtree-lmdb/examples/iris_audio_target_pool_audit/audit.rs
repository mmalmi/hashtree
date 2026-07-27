use crate::config::{canonical_hash, inventory_identity_sha256, load_config, ValidatedConfig};
use crate::io::{
    append_json_line, ensure_parent, ledger_hash_stats, load_inventory, read_json, sha256_file,
    sync_parent, write_atomic_json,
};
use crate::model::{
    AuditManifest, AuditSummary, BlockLedgerRow, BlockRef, Checkpoint, InventoryIdentityManifest,
    LedgerManifest, RunOutcome, TargetManifest, WorkItem, CHECKPOINT_SCHEMA,
    INVENTORY_IDENTITY_SCHEMA, LEDGER_ROW_SCHEMA, MANIFEST_SCHEMA,
};
use crate::probe::{verify_pool_manifest_unchanged, ProbeContext};
use crate::traversal::process_block;
use hashtree_core::{Hash, LinkType};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

#[derive(Debug)]
pub struct RunPaths {
    pub config: PathBuf,
    pub inventory: PathBuf,
    pub ledger: PathBuf,
    pub checkpoint: PathBuf,
    pub manifest: PathBuf,
}

struct WorkItemState {
    index: usize,
    item: WorkItem,
    frontier: Vec<BlockRef>,
    seen: HashSet<(Hash, Option<[u8; 32]>, u8)>,
    rows: Vec<BlockLedgerRow>,
    complete: bool,
}

pub fn run(
    paths: &RunPaths,
    max_batches: Option<usize>,
) -> Result<RunOutcome, Box<dyn std::error::Error>> {
    validate_run_paths(paths)?;
    let validated = load_config(&paths.config)?;
    validate_output_storage_separation(paths, &validated)?;
    let (inventory, inventory_sha256) = load_inventory(
        &paths.inventory,
        &validated.config.expected_inventory_sha256,
        validated.config.expected_inventory_records,
    )?;
    let inventory_identity_sha256 = inventory_identity_sha256(&inventory_sha256, inventory.len());
    let work_items = build_work_items(&validated, inventory)?;
    let total_work_items = work_items.len();
    let probe = ProbeContext::open(&validated)?;
    let pool_manifest_identity = probe.pool_manifest_identity();
    let pool_manifest_sha256 = probe.pool_manifest_sha256();
    let pool_manifest_generation = probe.pool_manifest_generation();
    let expected_pool_member_ids = probe.expected_pool_member_labels();
    let target_member_ids = probe.target_member_labels();
    let fallback_tier_names = probe.fallback_tier_names();

    let mut checkpoint = open_checkpoint(
        paths,
        &validated,
        &pool_manifest_sha256,
        pool_manifest_generation,
        &inventory_sha256,
        &inventory_identity_sha256,
        total_work_items,
    )?;
    ensure_parent(&paths.ledger)?;
    let mut ledger_file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&paths.ledger)?;
    let actual_ledger_bytes = ledger_file.metadata()?.len();
    if actual_ledger_bytes < checkpoint.ledger_bytes {
        return Err(format!(
            "ledger is shorter than checkpoint: {} < {}",
            actual_ledger_bytes, checkpoint.ledger_bytes
        )
        .into());
    }
    let mut ledger_hasher = hash_ledger_prefix(&paths.ledger, checkpoint.ledger_bytes)?;
    let actual_prefix_sha256 = ledger_hasher_sha256(&ledger_hasher);
    if actual_prefix_sha256 != checkpoint.ledger_sha256 {
        return Err(format!(
            "committed ledger prefix SHA256 mismatch: expected {}, got {actual_prefix_sha256}",
            checkpoint.ledger_sha256
        )
        .into());
    }
    ledger_file.set_len(checkpoint.ledger_bytes)?;
    ledger_file.seek(SeekFrom::Start(checkpoint.ledger_bytes))?;
    let mut ledger = BufWriter::new(ledger_file);

    let mut completed_batches = 0usize;
    while checkpoint.next_work_item < total_work_items
        && max_batches.is_none_or(|maximum| completed_batches < maximum)
    {
        let end = (checkpoint.next_work_item + validated.config.work_item_batch_size)
            .min(total_work_items);
        let mut states = work_items[checkpoint.next_work_item..end]
            .iter()
            .cloned()
            .enumerate()
            .map(|(offset, item)| WorkItemState {
                index: checkpoint.next_work_item + offset,
                frontier: vec![BlockRef {
                    hash: item.hash,
                    key: item.key,
                    path: ".".into(),
                    role: item.role.clone(),
                    expected_link_type: (item.role == "catalog").then_some(LinkType::Dir),
                }],
                item,
                seen: HashSet::new(),
                rows: Vec::new(),
                complete: true,
            })
            .collect::<Vec<_>>();
        audit_work_item_batch(&probe, &mut states, &mut checkpoint.summary)?;

        for state in states {
            for row in state.rows {
                append_json_line(&mut ledger, &mut ledger_hasher, &row)?;
            }
            checkpoint.summary.work_items_processed += 1;
            if state.item.kind == "inventory" {
                checkpoint.summary.inventory_records_processed += 1;
            } else {
                checkpoint.summary.additional_roots_processed += 1;
            }
            if state.complete {
                checkpoint.summary.complete_work_items += 1;
            } else {
                checkpoint.summary.incomplete_work_items += 1;
            }
        }
        ledger.flush()?;
        ledger.get_ref().sync_data()?;
        sync_parent(&paths.ledger)?;
        checkpoint.next_work_item = end;
        checkpoint.ledger_bytes = ledger.get_mut().stream_position()?;
        checkpoint.ledger_sha256 = ledger_hasher_sha256(&ledger_hasher);
        write_atomic_json(&paths.checkpoint, &checkpoint)?;
        completed_batches += 1;
        eprintln!(
            "audit_checkpoint={}/{} ledger_bytes={} ledger_rows={}",
            checkpoint.next_work_item,
            total_work_items,
            checkpoint.ledger_bytes,
            checkpoint.summary.block_references
        );
    }
    ledger.flush()?;
    ledger.get_ref().sync_data()?;
    drop(ledger);

    let complete = checkpoint.next_work_item == total_work_items;
    drop(probe);
    if complete {
        verify_pool_manifest_unchanged(&validated, &pool_manifest_identity)?;
    }
    let release_ready = complete && checkpoint.summary.release_ready(total_work_items);
    if complete {
        let (ledger_sha256, ledger_bytes) = sha256_file(&paths.ledger)?;
        if ledger_bytes != checkpoint.ledger_bytes {
            return Err("ledger size changed after terminal checkpoint".into());
        }
        if ledger_sha256 != checkpoint.ledger_sha256 {
            return Err("ledger SHA256 changed after terminal checkpoint".into());
        }
        let (unique_block_hashes, ledger_rows) = ledger_hash_stats(&paths.ledger)?;
        if ledger_rows != checkpoint.summary.block_references {
            return Err(format!(
                "ledger row count does not match checkpoint: {ledger_rows} != {}",
                checkpoint.summary.block_references
            )
            .into());
        }
        let manifest = AuditManifest {
            schema: MANIFEST_SCHEMA,
            config_sha256: validated.config_sha256.clone(),
            inventory: InventoryIdentityManifest {
                schema: INVENTORY_IDENTITY_SCHEMA,
                sha256: inventory_sha256,
                records: validated.config.expected_inventory_records,
                identity_sha256: inventory_identity_sha256,
            },
            target: TargetManifest {
                pool_manifest_sha256,
                pool_manifest_generation,
                expected_pool_member_ids,
                target_member_ids,
                fallback_tier_names,
            },
            work_items: total_work_items,
            ledger: LedgerManifest {
                schema: LEDGER_ROW_SCHEMA,
                sha256: ledger_sha256,
                bytes: ledger_bytes,
                rows: checkpoint.summary.block_references,
                unique_block_hashes,
            },
            summary: checkpoint.summary.clone(),
            release_ready,
        };
        write_atomic_json(&paths.manifest, &manifest)?;
    }

    Ok(RunOutcome {
        next_work_item: checkpoint.next_work_item,
        total_work_items,
        ledger_rows: checkpoint.summary.block_references,
        complete,
        release_ready,
    })
}

fn validate_run_paths(paths: &RunPaths) -> Result<(), Box<dyn std::error::Error>> {
    let named = [
        ("config", &paths.config),
        ("inventory", &paths.inventory),
        ("ledger", &paths.ledger),
        ("checkpoint", &paths.checkpoint),
        ("manifest", &paths.manifest),
    ];
    let mut identities = BTreeMap::new();
    for (label, path) in named {
        let identity = path_identity(path)?;
        if let Some(previous) = identities.insert(identity, label) {
            return Err(format!("{label} path aliases {previous} path").into());
        }
    }
    Ok(())
}

fn validate_output_storage_separation(
    paths: &RunPaths,
    validated: &ValidatedConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let outputs = [&paths.ledger, &paths.checkpoint, &paths.manifest]
        .into_iter()
        .map(|path| path_identity(path))
        .collect::<Result<Vec<_>, _>>()?;
    let mut storage_roots = vec![path_identity(&validated.config.pool_catalog)?];
    for tier in &validated.config.fallback_tiers {
        storage_roots.push(path_identity(&tier.lmdb_path)?);
        if let Some(external) = &tier.external_blob_dir {
            storage_roots.push(path_identity(external)?);
        }
    }
    if outputs
        .iter()
        .any(|output| storage_roots.iter().any(|root| output.starts_with(root)))
    {
        return Err("audit outputs must be outside Pool and fallback storage roots".into());
    }
    Ok(())
}

fn path_identity(path: &std::path::Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if path.exists() {
        return Ok(fs::canonicalize(path)?);
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let parent = absolute
        .parent()
        .unwrap_or_else(|| std::path::Path::new("/"));
    let file_name = absolute
        .file_name()
        .ok_or_else(|| format!("path has no file name: {}", path.display()))?;
    Ok(match fs::canonicalize(parent) {
        Ok(parent) => parent.join(file_name),
        Err(_) => absolute,
    })
}

fn build_work_items(
    validated: &ValidatedConfig,
    inventory: Vec<crate::model::InventoryRow>,
) -> Result<Vec<WorkItem>, Box<dyn std::error::Error>> {
    let mut work_items = inventory
        .into_iter()
        .map(|row| WorkItem {
            kind: "inventory",
            id: format!("{}:{}", row.source_key, row.song_id),
            source_key: Some(row.source_key),
            song_id: Some(row.song_id),
            input_line: Some(row.input_line),
            role: "song".into(),
            hash: row.hash,
            key: Some(row.key),
        })
        .collect::<Vec<_>>();
    for root in &validated.config.additional_roots {
        work_items.push(WorkItem {
            kind: "additional-root",
            id: root.id.clone(),
            source_key: None,
            song_id: None,
            input_line: None,
            role: root.role.clone(),
            hash: canonical_hash("additional root hash", &root.hash)?,
            key: root
                .key
                .as_deref()
                .map(|key| canonical_hash("additional root key", key))
                .transpose()?,
        });
    }
    Ok(work_items)
}

fn open_checkpoint(
    paths: &RunPaths,
    validated: &ValidatedConfig,
    pool_manifest_sha256: &str,
    pool_manifest_generation: u64,
    inventory_sha256: &str,
    inventory_identity_sha256: &str,
    total_work_items: usize,
) -> Result<Checkpoint, Box<dyn std::error::Error>> {
    if !paths.checkpoint.exists() {
        if paths.ledger.exists() || paths.manifest.exists() {
            return Err(
                "checkpoint is absent but ledger or manifest already exists; refusing to overwrite"
                    .into(),
            );
        }
        let checkpoint = Checkpoint {
            schema: CHECKPOINT_SCHEMA.into(),
            config_sha256: validated.config_sha256.clone(),
            pool_manifest_sha256: pool_manifest_sha256.into(),
            pool_manifest_generation,
            inventory_sha256: inventory_sha256.into(),
            inventory_records: validated.config.expected_inventory_records,
            inventory_identity_sha256: inventory_identity_sha256.into(),
            next_work_item: 0,
            ledger_bytes: 0,
            ledger_sha256: ledger_hasher_sha256(&Sha256::new()),
            summary: AuditSummary::default(),
        };
        write_atomic_json(&paths.checkpoint, &checkpoint)?;
        return Ok(checkpoint);
    }

    let checkpoint: Checkpoint = read_json(&paths.checkpoint)?;
    if checkpoint.schema != CHECKPOINT_SCHEMA {
        return Err(format!("unsupported checkpoint schema {}", checkpoint.schema).into());
    }
    if checkpoint.config_sha256 != validated.config_sha256
        || checkpoint.pool_manifest_sha256 != pool_manifest_sha256
        || checkpoint.pool_manifest_generation != pool_manifest_generation
        || checkpoint.inventory_sha256 != inventory_sha256
        || checkpoint.inventory_records != validated.config.expected_inventory_records
        || checkpoint.inventory_identity_sha256 != inventory_identity_sha256
    {
        return Err(
            "checkpoint authority does not match config, Pool manifest, and inventory identity"
                .into(),
        );
    }
    if checkpoint.next_work_item > total_work_items {
        return Err("checkpoint cursor is past the work-item set".into());
    }
    canonical_hash("checkpoint ledgerSha256", &checkpoint.ledger_sha256)
        .map_err(|error| format!("invalid checkpoint ledger digest: {error}"))?;
    checkpoint
        .summary
        .validate_checkpoint(
            checkpoint.next_work_item,
            validated.config.expected_inventory_records,
        )
        .map_err(|error| format!("checkpoint progress is inconsistent: {error}"))?;
    if !paths.ledger.exists() && checkpoint.ledger_bytes > 0 {
        return Err("checkpoint references a missing non-empty ledger".into());
    }
    if checkpoint.next_work_item < total_work_items && paths.manifest.exists() {
        return Err("incomplete checkpoint has a stale terminal manifest".into());
    }
    Ok(checkpoint)
}

fn hash_ledger_prefix(
    path: &std::path::Path,
    prefix_bytes: u64,
) -> Result<Sha256, Box<dyn std::error::Error>> {
    let mut hasher = Sha256::new();
    if prefix_bytes == 0 {
        return Ok(hasher);
    }
    let mut input = File::open(path)?;
    let mut remaining = prefix_bytes;
    let mut buffer = vec![0u8; 1024 * 1024];
    while remaining > 0 {
        let maximum = remaining.min(buffer.len() as u64) as usize;
        let read = input.read(&mut buffer[..maximum])?;
        if read == 0 {
            return Err(format!(
                "ledger ended before committed prefix: {} bytes remain",
                remaining
            )
            .into());
        }
        hasher.update(&buffer[..read]);
        remaining -= read as u64;
    }
    Ok(hasher)
}

fn ledger_hasher_sha256(hasher: &Sha256) -> String {
    format!("{:x}", hasher.clone().finalize())
}

fn audit_work_item_batch(
    probe: &ProbeContext,
    states: &mut [WorkItemState],
    summary: &mut AuditSummary,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        let mut round = Vec::new();
        for (state_index, state) in states.iter_mut().enumerate() {
            for block in state.frontier.drain(..) {
                if state.seen.insert((
                    block.hash,
                    block.key,
                    expected_link_type_key(block.expected_link_type),
                )) {
                    round.push((state_index, block));
                }
            }
        }
        if round.is_empty() {
            break;
        }
        let hashes = round
            .iter()
            .map(|(_, block)| block.hash)
            .collect::<Vec<_>>();
        let probes = probe.probe_hashes(&hashes)?;
        let mut next = vec![Vec::new(); states.len()];
        for (state_index, block) in round {
            let hash_probe = probes
                .get(&block.hash)
                .ok_or("probe result omitted a requested hash")?;
            let processed = process_block(
                states[state_index].index,
                &states[state_index].item,
                &block,
                hash_probe,
            );
            let state = &mut states[state_index];
            let row = processed.row;
            if row.residency != "target-valid" {
                state.complete = false;
            }
            summary.block_references += 1;
            summary
                .observe_residency(&row.residency)
                .map_err(|error| format!("invalid block result: {error}"))?;
            *summary.role_counts.entry(row.role.clone()).or_default() += 1;
            summary.discovered_external_roots += row.discovered_external_roots as u64;
            if processed.traversal_failed {
                summary.traversal_failures += 1;
                state.complete = false;
            }
            next[state_index].extend(processed.children);
            state.rows.push(row);
        }
        for (state, mut children) in states.iter_mut().zip(next) {
            state.frontier.append(&mut children);
        }
    }
    Ok(())
}

fn expected_link_type_key(link_type: Option<LinkType>) -> u8 {
    match link_type {
        Some(LinkType::Blob) => 0,
        Some(LinkType::File) => 1,
        Some(LinkType::Dir) => 2,
        Some(LinkType::Fanout) => 3,
        None => u8::MAX,
    }
}

#[cfg(test)]
#[path = "audit_tests.rs"]
mod tests;
