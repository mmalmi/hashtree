use crate::audit::{run, RunPaths};
use crate::model::{CONFIG_SCHEMA, INVENTORY_IDENTITY_SCHEMA, LEDGER_ROW_SCHEMA};
use crate::witness::{
    verify_existing, witness_json_line, WitnessPaths, MAX_WITNESS_JSON_BYTES, WITNESS_SCHEMA,
};
use hashtree_core::{
    nhash_encode_full, to_hex, DirEntry, HashTree, HashTreeConfig, LinkType, NHashData,
};
use hashtree_lmdb::{LmdbBlobStore, PoolMemberConfig, PoolMemberId, PoolStore, PoolStoreConfig};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const CHALLENGE: &str = "27c9a4c795e8ce301c0fe45f03dd34720de5cb5cdf830183b0340b84347b045a";

struct ReleaseReadyScenario {
    _temp: tempfile::TempDir,
    paths: RunPaths,
    pool_path: PathBuf,
    member_path: PathBuf,
    member: PoolMemberId,
    song_root_hash: hashtree_core::Hash,
}

impl ReleaseReadyScenario {
    fn witness_paths(&self) -> WitnessPaths {
        WitnessPaths {
            config: self.paths.config.clone(),
            inventory: self.paths.inventory.clone(),
            ledger: self.paths.ledger.clone(),
            manifest: self.paths.manifest.clone(),
        }
    }

    fn authoritative_snapshot(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut snapshot = BTreeMap::new();
        for path in [
            &self.paths.config,
            &self.paths.inventory,
            &self.paths.ledger,
            &self.paths.manifest,
        ] {
            snapshot.insert(
                path.clone(),
                fs::read(path).expect("read generated authoritative input"),
            );
        }
        snapshot_non_lock_files(&self.pool_path, &mut snapshot);
        snapshot_non_lock_files(&self.member_path, &mut snapshot);
        snapshot
    }
}

#[tokio::test]
async fn live_witness_revalidates_real_pool_without_mutating_authority_files() {
    let scenario = release_ready_scenario().await;
    let before = scenario.authoritative_snapshot();

    let witness =
        verify_existing(&scenario.witness_paths(), CHALLENGE).expect("live target-Pool witness");

    assert_eq!(witness.schema, WITNESS_SCHEMA);
    assert_eq!(witness.challenge, CHALLENGE);
    assert!(canonical_utc_millis(&witness.started_at));
    assert!(canonical_utc_millis(&witness.verified_at));
    assert_eq!(witness.inventory_identity.schema, INVENTORY_IDENTITY_SCHEMA);
    assert_eq!(witness.inventory_identity.records, 1);
    assert_eq!(witness.ledger.schema, LEDGER_ROW_SCHEMA);
    assert!(witness.ledger.bytes > 0);
    assert!(witness.ledger.rows > 0);
    assert!(witness.ledger.unique_block_hashes > 0);
    assert_eq!(
        witness.verified_unique_block_hashes,
        witness.ledger.unique_block_hashes
    );
    assert_eq!(
        witness.pool_manifest.member_ids,
        vec![scenario.member.to_string()]
    );
    assert_eq!(
        witness.pool_manifest.target_member_ids,
        vec![scenario.member.to_string()]
    );
    assert!(witness.release_ready);

    let line = witness_json_line(&witness).expect("bounded witness line");
    assert!(line.len() <= MAX_WITNESS_JSON_BYTES);
    assert_eq!(line.last(), Some(&b'\n'));
    assert_eq!(
        line[..line.len() - 1]
            .iter()
            .filter(|byte| **byte == b'\n' || **byte == b'\r')
            .count(),
        0,
        "stdout witness must be exactly one LF-terminated compact JSON line"
    );
    let decoded: serde_json::Value = serde_json::from_slice(&line).expect("witness stdout JSON");
    assert_eq!(decoded["schema"], WITNESS_SCHEMA);
    assert_eq!(decoded["challenge"], CHALLENGE);

    assert_eq!(
        scenario.authoritative_snapshot(),
        before,
        "the verifier must leave every non-lock Pool/member file and every raw input byte-identical"
    );
}

#[tokio::test]
async fn live_witness_rejects_deleted_target_bytes_with_unchanged_catalog_and_manifest() {
    let scenario = release_ready_scenario().await;
    let manifest_before = fs::read(&scenario.paths.manifest).expect("terminal manifest");
    let catalog_before = fs::read(scenario.pool_path.join("data.mdb")).expect("Pool catalog bytes");
    let target = LmdbBlobStore::new(&scenario.member_path).expect("open exact target member");
    assert!(target
        .delete_sync(&scenario.song_root_hash)
        .expect("delete generated target bytes"));
    target.force_sync().expect("sync target deletion");
    drop(target);

    let error = verify_existing(&scenario.witness_paths(), CHALLENGE)
        .expect_err("physically missing target bytes must fail closed");
    assert!(
        error
            .to_string()
            .contains("terminal target residency changed"),
        "unexpected physical deletion error: {error}"
    );
    assert_eq!(
        fs::read(&scenario.paths.manifest).expect("unchanged terminal manifest"),
        manifest_before
    );
    assert_eq!(
        fs::read(scenario.pool_path.join("data.mdb")).expect("unchanged Pool catalog"),
        catalog_before
    );
}

#[tokio::test]
async fn live_witness_rejects_catalog_size_regression_with_unchanged_pool_manifest() {
    let scenario = release_ready_scenario().await;
    let manifest_before = fs::read(&scenario.paths.manifest).expect("terminal manifest");
    let audited_size = uniquely_represented_catalog_size(&scenario.paths.ledger);
    mutate_stored_location_sizes(
        &scenario.pool_path,
        scenario.member,
        audited_size,
        audited_size + 1,
    );

    let error = verify_existing(&scenario.witness_paths(), CHALLENGE)
        .expect_err("catalog size regression must fail closed");
    assert!(
        error
            .to_string()
            .contains("terminal target residency changed"),
        "unexpected catalog regression error: {error}"
    );
    assert_eq!(
        fs::read(&scenario.paths.manifest).expect("unchanged terminal manifest"),
        manifest_before
    );
}

#[tokio::test]
async fn live_witness_rejects_changed_pool_manifest_member_configuration() {
    let scenario = release_ready_scenario().await;
    let manifest_before = fs::read(&scenario.paths.manifest).expect("terminal manifest");
    let mut config = PoolStoreConfig::default();
    config.temperature.enabled = false;
    let pool = PoolStore::open(&scenario.pool_path, config).expect("reopen generated Pool");
    let status = pool.member(scenario.member).expect("target member status");
    pool.update_member_limits(
        scenario.member,
        status.capacity_bytes + 4096,
        status.max_read_concurrency,
        status.max_write_concurrency,
    )
    .expect("change exact member configuration");
    pool.force_sync().expect("sync changed Pool manifest");
    drop(pool);

    let error = verify_existing(&scenario.witness_paths(), CHALLENGE)
        .expect_err("changed Pool manifest must fail closed");
    assert!(
        error.to_string().contains("live Pool manifest"),
        "unexpected manifest mutation error: {error}"
    );
    assert_eq!(
        fs::read(&scenario.paths.manifest).expect("unchanged terminal manifest"),
        manifest_before
    );
}

#[tokio::test]
async fn live_witness_rejects_changed_full_pool_member_set() {
    let scenario = release_ready_scenario().await;
    let mut config = PoolStoreConfig::default();
    config.temperature.enabled = false;
    let pool = PoolStore::open(&scenario.pool_path, config).expect("reopen generated Pool");
    pool.add_member(PoolMemberConfig::new(
        scenario._temp.path().join("unexpected-member"),
        64 * 1024 * 1024,
    ))
    .expect("add unexpected Pool member");
    pool.force_sync().expect("sync changed Pool member set");
    drop(pool);

    let error = verify_existing(&scenario.witness_paths(), CHALLENGE)
        .expect_err("changed full Pool member set must fail closed");
    assert!(
        error
            .to_string()
            .contains("Pool manifest member set does not match"),
        "unexpected member-set mutation error: {error}"
    );
}

#[tokio::test]
async fn live_witness_rejects_non_release_ready_terminal_manifest() {
    let scenario = release_ready_scenario().await;
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&scenario.paths.manifest).expect("terminal manifest"))
            .expect("terminal manifest JSON");
    manifest["releaseReady"] = serde_json::Value::Bool(false);
    fs::write(
        &scenario.paths.manifest,
        serde_json::to_vec_pretty(&manifest).expect("changed manifest JSON"),
    )
    .expect("write changed terminal manifest");

    let error = verify_existing(&scenario.witness_paths(), CHALLENGE)
        .expect_err("non-release-ready terminal manifest must fail closed");
    assert!(
        error.to_string().contains("releaseReady"),
        "unexpected releaseReady error: {error}"
    );
}

#[tokio::test]
async fn live_witness_rejects_noncanonical_challenge_before_pool_probe() {
    let scenario = release_ready_scenario().await;
    for challenge in ["ABC", &"A".repeat(64), &"0".repeat(63)] {
        let error = verify_existing(&scenario.witness_paths(), challenge)
            .expect_err("noncanonical challenge must fail");
        assert!(
            error.to_string().contains("challenge"),
            "unexpected challenge error: {error}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn live_witness_rejects_symlinked_and_non_regular_raw_inputs() {
    use std::os::unix::fs::symlink;

    let scenario = release_ready_scenario().await;
    for (label, path) in [
        ("config", &scenario.paths.config),
        ("inventory", &scenario.paths.inventory),
        ("ledger", &scenario.paths.ledger),
        ("manifest", &scenario.paths.manifest),
    ] {
        let real_path = path.with_extension(format!("{label}.real"));
        fs::rename(path, &real_path).expect("move generated raw input");
        symlink(&real_path, path).expect("symlink generated raw input");
        let error = verify_existing(&scenario.witness_paths(), CHALLENGE)
            .expect_err("symlinked raw input must fail closed");
        assert!(
            error.to_string().contains("symbolic link"),
            "unexpected {label} symlink error: {error}"
        );
        fs::remove_file(path).expect("remove generated symlink");
        fs::rename(real_path, path).expect("restore generated raw input");
    }

    let real_manifest = scenario.paths.manifest.with_extension("manifest.real");
    fs::rename(&scenario.paths.manifest, &real_manifest).expect("move generated manifest");
    fs::create_dir(&scenario.paths.manifest).expect("create non-regular manifest input");
    let error = verify_existing(&scenario.witness_paths(), CHALLENGE)
        .expect_err("non-regular raw input must fail closed");
    assert!(
        error.to_string().contains("not a regular file"),
        "unexpected non-regular input error: {error}"
    );
    fs::remove_dir(&scenario.paths.manifest).expect("remove generated directory input");
    fs::rename(real_manifest, &scenario.paths.manifest).expect("restore generated manifest");
}

async fn release_ready_scenario() -> ReleaseReadyScenario {
    let temp = tempfile::tempdir().expect("temporary real state");
    let pool_path = temp.path().join("pool");
    let member_path = temp.path().join("member");
    let mut pool_config = PoolStoreConfig::default();
    pool_config.temperature.enabled = false;
    let pool = PoolStore::open(&pool_path, pool_config).expect("real PoolStore");
    let member = pool
        .add_member(PoolMemberConfig::new(member_path.clone(), 64 * 1024 * 1024))
        .expect("real target member");
    let tree = HashTree::new(
        HashTreeConfig::new(Arc::new(pool.clone()))
            .with_chunk_size(4)
            .with_max_links(2),
    );
    let song_root = generated_song_tree(&tree, "live-witness").await;
    let catalog_root = tree
        .put_directory(vec![
            DirEntry::from_cid("live-witness", &song_root).with_link_type(LinkType::Dir)
        ])
        .await
        .expect("real catalog DAG");
    pool.force_sync().expect("sync real Pool");
    drop(tree);
    drop(pool);

    let inventory = format!(
        "sourceKey\tsongId\thash\tkey\nsource:live-witness\tlive-witness\t{}\t{}\n",
        to_hex(&song_root.hash),
        to_hex(&song_root.key.expect("encrypted song root")),
    );
    let inventory_path = temp.path().join("inventory.tsv");
    fs::write(&inventory_path, inventory.as_bytes()).expect("generated inventory");
    let config_path = temp.path().join("config.json");
    fs::write(
        &config_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema": CONFIG_SCHEMA,
            "poolCatalog": pool_path,
            "expectedPoolMembers": [member.to_string()],
            "targetMembers": [member.to_string()],
            "fallbackTiers": [],
            "expectedInventorySha256": to_hex(&hashtree_core::sha256(inventory.as_bytes())),
            "expectedInventoryRecords": 1,
            "additionalRoots": [{
                "id": "catalog",
                "role": "catalog",
                "hash": to_hex(&catalog_root.hash),
                "key": catalog_root.key.map(|key| to_hex(&key)),
            }],
            "workItemBatchSize": 8,
            "readLimitBytes": 1024,
        }))
        .expect("config JSON"),
    )
    .expect("generated config");
    let paths = RunPaths {
        config: config_path,
        inventory: inventory_path,
        ledger: temp.path().join("ledger.jsonl"),
        checkpoint: temp.path().join("checkpoint.json"),
        manifest: temp.path().join("manifest.json"),
    };
    let outcome = run(&paths, None).expect("real release-ready audit");
    assert!(outcome.complete);
    assert!(outcome.release_ready);

    ReleaseReadyScenario {
        _temp: temp,
        paths,
        pool_path,
        member_path,
        member,
        song_root_hash: song_root.hash,
    }
}

async fn generated_song_tree<S: hashtree_core::store::Store>(
    tree: &HashTree<S>,
    id: &str,
) -> hashtree_core::Cid {
    let (image, _) = tree
        .put_file(format!("generated-image-{id}-bytes").as_bytes())
        .await
        .expect("image DAG");
    let (audio, audio_size) = tree
        .put_file(format!("generated-audio-{id}-bytes-with-chunks").as_bytes())
        .await
        .expect("audio DAG");
    let image_url = format!(
        "htree://{}",
        nhash_encode_full(&NHashData {
            hash: image.hash,
            decrypt_key: image.key,
        })
        .expect("image nhash")
    );
    let mut meta = HashMap::new();
    meta.insert(
        "schema".into(),
        serde_json::Value::String("iris-audio-track-entry/v1".into()),
    );
    meta.insert("coverImageUrl".into(), serde_json::Value::String(image_url));
    tree.put_directory(vec![DirEntry::from_cid(format!("{id}.mp3"), &audio)
        .with_size(audio_size)
        .with_link_type(LinkType::File)
        .with_meta(meta)])
        .await
        .expect("song directory")
}

fn snapshot_non_lock_files(path: &Path, snapshot: &mut BTreeMap<PathBuf, Vec<u8>>) {
    for entry in fs::read_dir(path).expect("read generated storage directory") {
        let entry = entry.expect("generated storage entry");
        let entry_path = entry.path();
        if entry.file_type().expect("storage entry type").is_dir() {
            snapshot_non_lock_files(&entry_path, snapshot);
        } else if entry.file_name() != "lock.mdb" {
            snapshot.insert(
                entry_path.clone(),
                fs::read(entry_path).expect("snapshot storage bytes"),
            );
        }
    }
}

fn mutate_stored_location_sizes(
    pool_path: &Path,
    member: PoolMemberId,
    previous_size: u64,
    next_size: u64,
) {
    let catalog_file = pool_path.join("data.mdb");
    let mut bytes = fs::read(&catalog_file).expect("read generated Pool catalog");
    let mut previous = Vec::with_capacity(25);
    previous.push(2);
    previous.extend_from_slice(member.as_bytes());
    previous.extend_from_slice(&previous_size.to_be_bytes());
    let matches = bytes
        .windows(previous.len())
        .enumerate()
        .filter_map(|(offset, value)| (value == previous).then_some(offset))
        .collect::<Vec<_>>();
    assert!(
        !matches.is_empty(),
        "generated Stored location must have an exact encoded record"
    );
    for offset in matches {
        let size_offset = offset + 17;
        bytes[size_offset..size_offset + 8].copy_from_slice(&next_size.to_be_bytes());
    }
    fs::write(catalog_file, bytes).expect("mutate generated Stored location size");
}

fn uniquely_represented_catalog_size(ledger: &Path) -> u64 {
    let mut hashes_by_size = BTreeMap::<u64, Vec<String>>::new();
    for line in fs::read_to_string(ledger)
        .expect("read generated ledger")
        .lines()
    {
        let row: serde_json::Value = serde_json::from_str(line).expect("generated ledger row");
        hashes_by_size
            .entry(
                row["catalogDeclaredSize"]
                    .as_u64()
                    .expect("Stored row declared size"),
            )
            .or_default()
            .push(row["blockHash"].as_str().expect("block hash").to_owned());
    }
    hashes_by_size
        .into_iter()
        .find_map(|(size, hashes)| (hashes.len() == 1).then_some(size))
        .expect("generated ledger must contain a uniquely represented Stored size")
}

fn canonical_utc_millis(value: &str) -> bool {
    value.len() == 24
        && value.as_bytes()[4] == b'-'
        && value.as_bytes()[7] == b'-'
        && value.as_bytes()[10] == b'T'
        && value.as_bytes()[13] == b':'
        && value.as_bytes()[16] == b':'
        && value.as_bytes()[19] == b'.'
        && value.as_bytes()[23] == b'Z'
        && value.bytes().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7 | 10 | 13 | 16 | 19 | 23) || byte.is_ascii_digit()
        })
}
