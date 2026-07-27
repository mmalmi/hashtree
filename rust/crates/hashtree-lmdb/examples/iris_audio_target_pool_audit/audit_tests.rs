use super::*;
use crate::model::CONFIG_SCHEMA;
use hashtree_core::{
    nhash_encode_full, to_hex, DirEntry, HashTree, HashTreeConfig, LinkType, NHashData,
};
use hashtree_lmdb::{LmdbBlobStore, PoolMemberConfig, PoolMemberId, PoolStore, PoolStoreConfig};
use std::collections::HashMap;
use std::fs;
use std::sync::Arc;

#[test]
fn audit_rejects_output_aliasing_authoritative_inventory() {
    let temp = tempfile::tempdir().expect("temporary state");
    let inventory = temp.path().join("inventory.tsv");
    let original = b"generated authoritative inventory bytes";
    fs::write(&inventory, original).expect("generated inventory");
    let paths = RunPaths {
        config: temp.path().join("config.json"),
        inventory: inventory.clone(),
        ledger: inventory.clone(),
        checkpoint: temp.path().join("checkpoint.json"),
        manifest: temp.path().join("manifest.json"),
    };
    let error = run(&paths, None).expect_err("aliased ledger must fail before any write");
    assert!(error.to_string().contains("aliases inventory"));
    assert_eq!(
        fs::read(&inventory).expect("authoritative bytes"),
        original,
        "path validation must run before opening an output"
    );
}

#[cfg(unix)]
#[test]
fn audit_rejects_hardlinked_output_aliasing_authoritative_inventory() {
    let temp = tempfile::tempdir().expect("temporary state");
    let inventory = temp.path().join("inventory.tsv");
    let ledger = temp.path().join("ledger.jsonl");
    let original = b"generated authoritative inventory bytes";
    fs::write(&inventory, original).expect("generated inventory");
    fs::hard_link(&inventory, &ledger).expect("hardlinked generated output");
    let paths = RunPaths {
        config: temp.path().join("config.json"),
        inventory: inventory.clone(),
        ledger,
        checkpoint: temp.path().join("checkpoint.json"),
        manifest: temp.path().join("manifest.json"),
    };

    let error = run(&paths, None).expect_err("hardlinked ledger must fail before any write");
    assert!(
        error.to_string().contains("aliases inventory"),
        "unexpected alias error: {error}"
    );
    assert_eq!(
        fs::read(&inventory).expect("authoritative bytes"),
        original,
        "path validation must run before opening a hardlinked output"
    );
}

#[cfg(unix)]
#[test]
fn audit_rejects_output_below_symlinked_storage_with_missing_parent() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("temporary state");
    let pool_path = temp.path().join("pool");
    let storage_alias = temp.path().join("pool-alias");
    fs::create_dir(&pool_path).expect("generated Pool directory");
    symlink(&pool_path, &storage_alias).expect("generated storage symlink");
    let config = temp.path().join("config.json");
    fs::write(
        &config,
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema": CONFIG_SCHEMA,
            "poolCatalog": pool_path,
            "expectedPoolMembers": ["00000000-0000-0000-0000-000000000001"],
            "targetMembers": ["00000000-0000-0000-0000-000000000001"],
            "fallbackTiers": [],
            "expectedInventorySha256": "00".repeat(32),
            "expectedInventoryRecords": 1,
            "additionalRoots": [{
                "id": "catalog",
                "role": "catalog",
                "hash": "11".repeat(32),
            }],
        }))
        .expect("config JSON"),
    )
    .expect("generated config");
    let paths = RunPaths {
        config,
        inventory: temp.path().join("inventory.tsv"),
        ledger: storage_alias.join("missing-parent").join("ledger.jsonl"),
        checkpoint: temp.path().join("checkpoint.json"),
        manifest: temp.path().join("manifest.json"),
    };

    let error = run(&paths, None).expect_err("symlinked Pool output must fail before any write");
    assert!(
        error
            .to_string()
            .contains("audit outputs must be outside Pool"),
        "unexpected storage-separation error: {error}"
    );
    assert!(!pool_path.join("missing-parent").exists());
}

#[test]
fn audit_does_not_remove_a_manifest_inside_authoritative_pool_storage() {
    let temp = tempfile::tempdir().expect("temporary state");
    let pool_path = temp.path().join("pool");
    let member_path = temp.path().join("member");
    let mut pool_config = PoolStoreConfig::default();
    pool_config.temperature.enabled = false;
    let pool = PoolStore::open(&pool_path, pool_config).expect("PoolStore");
    let member = pool
        .add_member(PoolMemberConfig::new(member_path, 64 * 1024 * 1024))
        .expect("target member");
    pool.force_sync().expect("sync Pool");
    drop(pool);

    let config = temp.path().join("config.json");
    fs::write(
        &config,
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema": CONFIG_SCHEMA,
            "poolCatalog": pool_path,
            "expectedPoolMembers": [member.to_string()],
            "targetMembers": [member.to_string()],
            "fallbackTiers": [],
            "expectedInventorySha256": "00".repeat(32),
            "expectedInventoryRecords": 1,
            "additionalRoots": [{
                "id": "catalog",
                "role": "catalog",
                "hash": "11".repeat(32),
            }],
        }))
        .expect("config JSON"),
    )
    .expect("generated config");
    let manifest = pool_path.join("authoritative-bytes");
    let original = b"must not be removed before storage separation";
    fs::write(&manifest, original).expect("generated authoritative bytes");
    let checkpoint = temp.path().join("checkpoint.json");
    fs::write(&checkpoint, b"completed checkpoint marker").expect("generated checkpoint");
    let paths = RunPaths {
        config,
        inventory: temp.path().join("inventory.tsv"),
        ledger: temp.path().join("ledger.jsonl"),
        checkpoint,
        manifest: manifest.clone(),
    };

    let error = run(&paths, None).expect_err("Pool-resident output must fail before removal");
    assert!(
        error
            .to_string()
            .contains("audit outputs must be outside Pool"),
        "unexpected storage-separation error: {error}"
    );
    assert_eq!(
        fs::read(&manifest).expect("authoritative bytes"),
        original,
        "storage separation must run before removing a stale output"
    );
}

#[test]
fn terminal_reopen_rejects_changed_pool_generation_and_member_config() {
    let temp = tempfile::tempdir().expect("temporary state");
    let pool_path = temp.path().join("pool");
    let member_path = temp.path().join("member");
    let mut pool_config = PoolStoreConfig::default();
    pool_config.temperature.enabled = false;
    let pool = PoolStore::open(&pool_path, pool_config).expect("PoolStore");
    let member = pool
        .add_member(PoolMemberConfig::new(member_path.clone(), 64 * 1024 * 1024))
        .expect("target member");
    pool.force_sync().expect("initial Pool sync");
    let before = pool.member(member).expect("initial member status");
    drop(pool);

    let config_path = temp.path().join("config.json");
    fs::write(
        &config_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema": CONFIG_SCHEMA,
            "poolCatalog": pool_path,
            "expectedPoolMembers": [member.to_string()],
            "targetMembers": [member.to_string()],
            "fallbackTiers": [],
            "expectedInventorySha256": "00".repeat(32),
            "expectedInventoryRecords": 1,
            "additionalRoots": [{
                "id": "catalog",
                "role": "catalog",
                "hash": "11".repeat(32),
            }],
        }))
        .expect("config JSON"),
    )
    .expect("generated config");
    let validated = load_config(&config_path).expect("validated config");
    let probe = ProbeContext::open(&validated).expect("initial read-only Pool snapshot");
    let initial_identity = probe.pool_manifest_identity();
    drop(probe);
    let mut reopened_config = PoolStoreConfig::default();
    reopened_config.temperature.enabled = false;
    let pool = PoolStore::open(&pool_path, reopened_config).expect("reopened PoolStore");
    pool.update_member_limits(
        member,
        before.capacity_bytes + 4096,
        before.max_read_concurrency,
        before.max_write_concurrency,
    )
    .expect("change member config");
    pool.force_sync().expect("changed Pool sync");
    drop(pool);

    let error = verify_pool_manifest_unchanged(&validated, &initial_identity)
        .expect_err("changed full manifest must invalidate the audit");
    assert!(error
        .to_string()
        .contains("Pool manifest changed during audit"));
}

#[tokio::test]
async fn real_pool_audit_resumes_and_rejects_fallback_only_dag() {
    let temp = tempfile::tempdir().expect("temporary state");
    let pool_path = temp.path().join("pool");
    let member_path = temp.path().join("member");
    let fallback_path = temp.path().join("fallback");
    let mut pool_config = PoolStoreConfig::default();
    pool_config.temperature.enabled = false;
    let pool = PoolStore::open(&pool_path, pool_config.clone()).expect("PoolStore");
    let member = pool
        .add_member(PoolMemberConfig::new(member_path, 64 * 1024 * 1024))
        .expect("target member");
    let target_tree = HashTree::new(
        HashTreeConfig::new(Arc::new(pool.clone()))
            .with_chunk_size(4)
            .with_max_links(2),
    );
    let target_root = generated_song_tree(&target_tree, "target").await;
    let catalog_root = target_tree
        .put_directory(vec![
            DirEntry::from_cid("target", &target_root).with_link_type(LinkType::Dir)
        ])
        .await
        .expect("catalog DAG");
    pool.force_sync().expect("sync target Pool");
    drop(target_tree);
    drop(pool);

    let fallback_store = Arc::new(LmdbBlobStore::new(&fallback_path).expect("fallback LMDB"));
    let fallback_tree = HashTree::new(
        HashTreeConfig::new(fallback_store.clone())
            .with_chunk_size(4)
            .with_max_links(2),
    );
    let fallback_root = generated_song_tree(&fallback_tree, "fallback").await;
    fallback_store.force_sync().expect("sync fallback");
    drop(fallback_tree);
    drop(fallback_store);

    let inventory = format!(
        "sourceKey\tsongId\thash\tkey\nsource:target\ttarget\t{}\t{}\nsource:fallback\tfallback\t{}\t{}\n",
        to_hex(&target_root.hash),
        to_hex(&target_root.key.expect("encrypted target root")),
        to_hex(&fallback_root.hash),
        to_hex(&fallback_root.key.expect("encrypted fallback root")),
    );
    let inventory_path = temp.path().join("inventory.tsv");
    fs::write(&inventory_path, inventory.as_bytes()).expect("write generated inventory");
    let inventory_sha = to_hex(&hashtree_core::sha256(inventory.as_bytes()));
    let config_path = temp.path().join("config.json");
    let config_json = serde_json::json!({
        "schema": CONFIG_SCHEMA,
        "poolCatalog": pool_path,
        "expectedPoolMembers": [member.to_string()],
        "targetMembers": [member.to_string()],
        "fallbackTiers": [{
            "name": "legacy",
            "lmdbPath": fallback_path,
        }],
        "expectedInventorySha256": inventory_sha,
        "expectedInventoryRecords": 2,
        "additionalRoots": [{
            "id": "catalog-under-repair",
            "role": "catalog",
            "hash": to_hex(&catalog_root.hash),
            "key": catalog_root.key.map(|key| to_hex(&key)),
        }],
        "workItemBatchSize": 1,
        "readLimitBytes": 1024,
    });
    fs::write(
        &config_path,
        serde_json::to_vec_pretty(&config_json).expect("config JSON"),
    )
    .expect("write config");
    let paths = RunPaths {
        config: config_path,
        inventory: inventory_path,
        ledger: temp.path().join("ledger.jsonl"),
        checkpoint: temp.path().join("checkpoint.json"),
        manifest: temp.path().join("manifest.json"),
    };
    let catalog_before = fs::read(pool_path.join("data.mdb")).expect("Pool catalog snapshot");

    let bounded = run(&paths, Some(1)).expect("bounded first pass");
    assert!(!bounded.complete);
    assert_eq!(bounded.next_work_item, 1);
    assert!(!paths.manifest.exists());
    let committed_ledger = fs::read(&paths.ledger).expect("committed ledger prefix");
    let checkpoint: serde_json::Value =
        serde_json::from_slice(&fs::read(&paths.checkpoint).expect("checkpoint bytes"))
            .expect("checkpoint JSON");
    assert_eq!(checkpoint["schema"], CHECKPOINT_SCHEMA);
    assert_eq!(
        checkpoint["ledgerBytes"],
        committed_ledger.len() as u64,
        "checkpoint must bind the exact committed prefix length"
    );
    assert_eq!(
        checkpoint["ledgerSha256"],
        to_hex(&hashtree_core::sha256(&committed_ledger)),
        "checkpoint must bind the exact committed prefix bytes"
    );

    let mut mutated_ledger =
        String::from_utf8(committed_ledger.clone()).expect("generated ledger is UTF-8");
    let block_hash_start = mutated_ledger
        .find("\"blockHash\":\"")
        .expect("ledger block hash")
        + "\"blockHash\":\"".len();
    let replacement = if &mutated_ledger[block_hash_start..block_hash_start + 1] == "0" {
        "1"
    } else {
        "0"
    };
    mutated_ledger.replace_range(block_hash_start..block_hash_start + 1, replacement);
    assert_eq!(
        mutated_ledger.len(),
        committed_ledger.len(),
        "adversarial mutation must preserve the committed byte length"
    );
    fs::write(&paths.ledger, mutated_ledger.as_bytes()).expect("mutate committed ledger prefix");
    let mutation_error =
        run(&paths, None).expect_err("same-length committed-prefix mutation must fail closed");
    assert!(
        mutation_error
            .to_string()
            .contains("committed ledger prefix SHA256 mismatch"),
        "unexpected mutation error: {mutation_error}"
    );
    fs::write(&paths.ledger, &committed_ledger).expect("restore committed ledger prefix");

    let resumed = run(&paths, None).expect("resumed audit");
    assert!(resumed.complete);
    assert!(!resumed.release_ready);

    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&paths.manifest).expect("manifest bytes"))
            .expect("manifest JSON");
    assert_eq!(manifest["schema"], MANIFEST_SCHEMA);
    assert_eq!(manifest["inventory"]["records"], 2);
    assert!(
        manifest["summary"]["fallbackOnly"]
            .as_u64()
            .expect("fallback count")
            > 0
    );
    assert_eq!(manifest["releaseReady"], false);
    assert_eq!(
        fs::read(pool_path.join("data.mdb")).expect("Pool catalog after audit"),
        catalog_before,
        "strict audit must not mutate the Pool catalog"
    );

    let ledger = fs::read_to_string(&paths.ledger).expect("ledger");
    assert!(ledger.contains("\"residency\":\"target-valid\""));
    assert!(ledger.contains("\"residency\":\"fallback-only\""));
    assert!(ledger.contains("\"role\":\"audio\""));
    assert!(ledger.contains("\"role\":\"image\""));
}

#[tokio::test]
async fn real_pool_audit_accepts_complete_catalog_song_audio_and_image_dags() {
    let temp = tempfile::tempdir().expect("temporary state");
    let pool_path = temp.path().join("pool");
    let member_path = temp.path().join("member");
    let draining_path = temp.path().join("draining");
    let mut pool_config = PoolStoreConfig::default();
    pool_config.temperature.enabled = false;
    let pool = PoolStore::open(&pool_path, pool_config).expect("PoolStore");
    let member = pool
        .add_member(PoolMemberConfig::new(member_path.clone(), 64 * 1024 * 1024))
        .expect("target member");
    let tree = HashTree::new(
        HashTreeConfig::new(Arc::new(pool.clone()))
            .with_chunk_size(4)
            .with_max_links(2),
    );
    let song_root = generated_song_tree(&tree, "ready").await;
    let catalog_root = tree
        .put_directory(vec![
            DirEntry::from_cid("ready", &song_root).with_link_type(LinkType::Dir)
        ])
        .await
        .expect("catalog DAG");
    let draining = pool
        .add_member(PoolMemberConfig::new(draining_path, 64 * 1024 * 1024))
        .expect("empty draining member");
    pool.begin_drain(draining)
        .expect("begin empty member drain");
    let draining_status = pool.member(draining).expect("draining status");
    assert_eq!(
        draining_status.state,
        hashtree_lmdb::PoolMemberState::Draining
    );
    assert_eq!(draining_status.logical_bytes, 0);
    assert_eq!(draining_status.located_blobs, 0);
    pool.force_sync().expect("sync Pool");
    drop(tree);
    drop(pool);

    let inventory = format!(
        "sourceKey\tsongId\thash\tkey\nsource:ready\tready\t{}\t{}\n",
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
            "expectedPoolMembers": [member.to_string(), draining.to_string()],
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
    let outcome = run(&paths, None).expect("complete target audit");
    assert!(outcome.complete);
    assert!(outcome.release_ready);

    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&paths.manifest).expect("manifest"))
            .expect("manifest JSON");
    assert_eq!(manifest["releaseReady"], true);
    assert_eq!(
        manifest["target"]["expectedPoolMemberIds"]
            .as_array()
            .expect("expected Pool members")
            .len(),
        2
    );
    assert_eq!(
        manifest["target"]["targetMemberIds"],
        serde_json::json!([member.to_string()])
    );
    assert_eq!(manifest["summary"]["fallbackOnly"], 0);
    assert_eq!(
        manifest["summary"]["targetValid"],
        manifest["ledger"]["rows"]
    );
    for role in ["catalog", "song", "audio", "image"] {
        assert!(
            manifest["summary"]["roleCounts"][role]
                .as_u64()
                .unwrap_or_default()
                > 0,
            "role {role} must have transitive proof rows"
        );
    }

    let mut hashes_by_size = std::collections::BTreeMap::<u64, Vec<String>>::new();
    for line in fs::read_to_string(&paths.ledger)
        .expect("read exact generated ledger")
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
            .push(row["blockHash"].as_str().expect("block hash").to_string());
    }
    let audited_root_size = hashes_by_size
        .into_iter()
        .find_map(|(size, hashes)| (hashes.len() == 1).then_some(size))
        .expect("generated ledger must contain a uniquely sized Stored block");
    let catalog_before_size_mutation =
        mutate_stored_location_sizes(&pool_path, member, audited_root_size, audited_root_size + 1);
    let size_error =
        run(&paths, None).expect_err("terminal catalog size mismatch must fail closed");
    assert!(
        size_error
            .to_string()
            .contains("terminal target residency changed"),
        "unexpected terminal size error: {size_error}"
    );
    assert!(
        !paths.manifest.exists(),
        "a catalog size mismatch must remove the stale release-ready manifest"
    );
    fs::write(pool_path.join("data.mdb"), &catalog_before_size_mutation)
        .expect("restore exact generated Pool catalog bytes");
    let restored = run(&paths, None).expect("restored catalog size must re-attest");
    assert!(restored.release_ready);

    let target_store = LmdbBlobStore::new(&member_path).expect("reopen exact target member");
    let audited_root_body = target_store
        .get_sync(&song_root.hash)
        .expect("read audited root body")
        .expect("audited root body");
    assert!(
        target_store
            .delete_sync(&song_root.hash)
            .expect("delete audited root bytes"),
        "generated target root must exist before terminal mutation"
    );
    target_store
        .force_sync()
        .expect("sync terminal target mutation");
    drop(target_store);

    let error = run(&paths, None)
        .expect_err("completed checkpoint must re-attest every target-resident block");
    assert!(
        error
            .to_string()
            .contains("terminal target residency changed"),
        "unexpected terminal residency error: {error}"
    );
    assert!(
        !paths.manifest.exists(),
        "a failed terminal re-attestation must remove the stale release-ready manifest"
    );

    let target_store = LmdbBlobStore::new(&member_path).expect("reopen exact target member");
    target_store
        .put_sync(song_root.hash, &audited_root_body)
        .expect("restore audited root body");
    target_store
        .force_sync()
        .expect("sync restored target body");
    drop(target_store);
    assert!(
        run(&paths, None)
            .expect("restored target body must re-attest")
            .release_ready
    );

    let mut reopened_config = PoolStoreConfig::default();
    reopened_config.temperature.enabled = false;
    let pool = PoolStore::open(&pool_path, reopened_config).expect("reopen generated Pool");
    let before = pool.member(member).expect("target member status");
    pool.update_member_limits(
        member,
        before.capacity_bytes + 4096,
        before.max_read_concurrency,
        before.max_write_concurrency,
    )
    .expect("mutate complete Pool manifest");
    pool.force_sync().expect("sync Pool manifest mutation");
    drop(pool);
    let manifest_error =
        run(&paths, None).expect_err("terminal Pool manifest mutation must fail closed");
    assert!(
        manifest_error
            .to_string()
            .contains("checkpoint authority does not match"),
        "unexpected terminal manifest error: {manifest_error}"
    );
    assert!(
        !paths.manifest.exists(),
        "a changed Pool manifest must not leave a stale release-ready manifest"
    );
}

fn mutate_stored_location_sizes(
    pool_path: &std::path::Path,
    member: PoolMemberId,
    previous_size: u64,
    next_size: u64,
) -> Vec<u8> {
    let catalog_file = pool_path.join("data.mdb");
    let mut bytes = fs::read(&catalog_file).expect("read generated Pool catalog");
    let original = bytes.clone();
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
    original
}

#[tokio::test]
async fn real_pool_audit_rejects_data_left_on_an_excluded_draining_member() {
    let temp = tempfile::tempdir().expect("temporary state");
    let pool_path = temp.path().join("pool");
    let excluded_path = temp.path().join("excluded-draining");
    let target_path = temp.path().join("target");
    let mut pool_config = PoolStoreConfig::default();
    pool_config.temperature.enabled = false;
    let pool = PoolStore::open(&pool_path, pool_config).expect("PoolStore");
    let excluded = pool
        .add_member(PoolMemberConfig::new(
            excluded_path.clone(),
            64 * 1024 * 1024,
        ))
        .expect("legacy member");
    let tree = HashTree::new(
        HashTreeConfig::new(Arc::new(pool.clone()))
            .with_chunk_size(4)
            .with_max_links(2),
    );
    let song_root = generated_song_tree(&tree, "excluded").await;
    let catalog_root = tree
        .put_directory(vec![
            DirEntry::from_cid("excluded", &song_root).with_link_type(LinkType::Dir)
        ])
        .await
        .expect("catalog DAG");
    let target = pool
        .add_member(PoolMemberConfig::new(target_path, 64 * 1024 * 1024))
        .expect("native target member");
    pool.begin_drain(excluded).expect("begin legacy drain");
    assert!(
        pool.member(excluded)
            .expect("excluded status")
            .located_blobs
            > 0,
        "generated DAG must still be catalogued on the excluded member"
    );
    assert_eq!(
        pool.member(target).expect("target status").located_blobs,
        0,
        "native target must start empty"
    );
    pool.force_sync().expect("sync Pool");
    drop(tree);
    drop(pool);

    let inventory = format!(
        "sourceKey\tsongId\thash\tkey\nsource:excluded\texcluded\t{}\t{}\n",
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
            "expectedPoolMembers": [excluded.to_string(), target.to_string()],
            "targetMembers": [target.to_string()],
            "fallbackTiers": [{
                "name": "excluded-pool-member",
                "lmdbPath": excluded_path,
            }],
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

    let outcome = run(&paths, None).expect("complete excluded-member audit");
    assert!(outcome.complete);
    assert!(!outcome.release_ready);
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&paths.manifest).expect("manifest"))
            .expect("manifest JSON");
    assert_eq!(manifest["releaseReady"], false);
    assert!(
        manifest["summary"]["fallbackOnly"]
            .as_u64()
            .unwrap_or_default()
            > 0
    );
    let ledger = fs::read_to_string(&paths.ledger).expect("ledger");
    assert!(ledger.contains("\"catalogState\":\"stored\""));
    assert!(ledger.contains(&format!("\"catalogCandidates\":[\"{}\"]", excluded)));
    assert!(ledger.contains("\"catalogTargetMembership\":false"));
    assert!(ledger.contains("\"residency\":\"fallback-only\""));
}

#[tokio::test]
async fn real_pool_audit_rejects_conflicting_link_types_for_one_block() {
    let temp = tempfile::tempdir().expect("temporary state");
    let pool_path = temp.path().join("pool");
    let member_path = temp.path().join("member");
    let mut pool_config = PoolStoreConfig::default();
    pool_config.temperature.enabled = false;
    let pool = PoolStore::open(&pool_path, pool_config).expect("PoolStore");
    let member = pool
        .add_member(PoolMemberConfig::new(member_path, 64 * 1024 * 1024))
        .expect("target member");
    let tree = HashTree::new(
        HashTreeConfig::new(Arc::new(pool.clone()))
            .with_chunk_size(4)
            .with_max_links(4),
    );
    let conflicted_hash = tree
        .put_blob(b"generated raw child with incompatible declared link types")
        .await
        .expect("raw child");
    let malformed_root = tree
        .put_directory(vec![
            DirEntry::new("as-blob.bin", conflicted_hash).with_link_type(LinkType::Blob),
            DirEntry::new("as-directory", conflicted_hash).with_link_type(LinkType::Dir),
        ])
        .await
        .expect("malformed directory");
    let catalog_root = tree
        .put_directory(vec![
            DirEntry::from_cid("malformed", &malformed_root).with_link_type(LinkType::Dir)
        ])
        .await
        .expect("catalog DAG");
    pool.force_sync().expect("sync Pool");
    drop(tree);
    drop(pool);

    let inventory = format!(
        "sourceKey\tsongId\thash\tkey\nsource:malformed\tmalformed\t{}\t{}\n",
        to_hex(&malformed_root.hash),
        to_hex(&malformed_root.key.expect("encrypted malformed root")),
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

    let outcome = run(&paths, None).expect("complete malformed-DAG audit");
    assert!(outcome.complete);
    assert!(!outcome.release_ready);
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&paths.manifest).expect("manifest"))
            .expect("manifest JSON");
    assert!(
        manifest["summary"]["traversalFailures"]
            .as_u64()
            .unwrap_or_default()
            > 0
    );
    let conflicted_hex = to_hex(&conflicted_hash);
    let rows = fs::read_to_string(&paths.ledger)
        .expect("ledger")
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("ledger row"))
        .filter(|row| row["blockHash"] == conflicted_hex)
        .collect::<Vec<_>>();
    assert!(
        rows.iter().any(|row| row["expectedLinkType"] == "blob"),
        "blob expectation must be audited"
    );
    assert!(
        rows.iter().any(|row| row["expectedLinkType"] == "dir"),
        "conflicting directory expectation must not be suppressed"
    );
    assert!(
        rows.iter()
            .any(|row| row["traversal"] == "tree-decode-failed"),
        "incompatible expectation must fail closed"
    );
}

#[tokio::test]
async fn real_pool_audit_classifies_corrupt_and_missing_blocks() {
    let temp = tempfile::tempdir().expect("temporary state");
    let pool_path = temp.path().join("pool");
    let member_path = temp.path().join("member");
    let mut pool_config = PoolStoreConfig::default();
    pool_config.temperature.enabled = false;
    let pool = PoolStore::open(&pool_path, pool_config).expect("PoolStore");
    let member = pool
        .add_member(PoolMemberConfig::new(member_path.clone(), 64 * 1024 * 1024))
        .expect("target member");
    let tree = HashTree::new(
        HashTreeConfig::new(Arc::new(pool.clone()))
            .with_chunk_size(4)
            .with_max_links(2),
    );
    let corrupt_root = generated_song_tree(&tree, "corrupt").await;
    let catalog_root = tree
        .put_directory(vec![
            DirEntry::from_cid("corrupt", &corrupt_root).with_link_type(LinkType::Dir)
        ])
        .await
        .expect("catalog DAG");
    pool.force_sync().expect("sync Pool");
    drop(tree);
    drop(pool);

    let member_store = LmdbBlobStore::new(&member_path).expect("member LMDB");
    member_store
        .delete_sync(&corrupt_root.hash)
        .expect("delete valid root");
    member_store
        .put_sync(corrupt_root.hash, b"generated corrupt bytes")
        .expect("write corrupt root bytes under the catalogued hash");
    member_store.force_sync().expect("sync corrupt member");
    drop(member_store);

    let missing_hash = hashtree_core::sha256(b"generated but never stored root");
    let missing_key = [9u8; 32];
    let inventory = format!(
        "sourceKey\tsongId\thash\tkey\nsource:corrupt\tcorrupt\t{}\t{}\nsource:missing\tmissing\t{}\t{}\n",
        to_hex(&corrupt_root.hash),
        to_hex(&corrupt_root.key.expect("encrypted corrupt root")),
        to_hex(&missing_hash),
        to_hex(&missing_key),
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
            "expectedInventoryRecords": 2,
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
    let outcome = run(&paths, None).expect("corrupt and missing audit");
    assert!(outcome.complete);
    assert!(!outcome.release_ready);

    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&paths.manifest).expect("manifest"))
            .expect("manifest JSON");
    assert!(manifest["summary"]["corrupt"].as_u64().unwrap_or_default() > 0);
    assert!(manifest["summary"]["missing"].as_u64().unwrap_or_default() > 0);
    let ledger = fs::read_to_string(&paths.ledger).expect("ledger");
    assert!(ledger.contains("\"residency\":\"corrupt\""));
    assert!(ledger.contains("\"residency\":\"missing\""));
}

#[tokio::test]
async fn real_pool_audit_classifies_unavailable_target_as_unknown() {
    let temp = tempfile::tempdir().expect("temporary state");
    let pool_path = temp.path().join("pool");
    let member_path = temp.path().join("member");
    let second_member_path = temp.path().join("second-member");
    let unavailable_path = temp.path().join("second-member-unavailable");
    let mut pool_config = PoolStoreConfig::default();
    pool_config.temperature.enabled = false;
    let pool = PoolStore::open(&pool_path, pool_config).expect("PoolStore");
    let member = pool
        .add_member(PoolMemberConfig::new(member_path.clone(), 64 * 1024 * 1024))
        .expect("target member");
    let tree = HashTree::new(
        HashTreeConfig::new(Arc::new(pool.clone()))
            .with_chunk_size(4)
            .with_max_links(2),
    );
    let song_root = generated_song_tree(&tree, "unknown").await;
    let catalog_root = tree
        .put_directory(vec![
            DirEntry::from_cid("unknown", &song_root).with_link_type(LinkType::Dir)
        ])
        .await
        .expect("catalog DAG");
    let second_member = pool
        .add_member(PoolMemberConfig::new(
            second_member_path.clone(),
            64 * 1024 * 1024,
        ))
        .expect("second configured target member");
    pool.force_sync().expect("sync Pool");
    drop(tree);
    drop(pool);
    fs::rename(&second_member_path, &unavailable_path)
        .expect("make second target member unavailable");

    let inventory = format!(
        "sourceKey\tsongId\thash\tkey\nsource:unknown\tunknown\t{}\t{}\n",
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
            "expectedPoolMembers": [member.to_string(), second_member.to_string()],
            "targetMembers": [member.to_string(), second_member.to_string()],
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
    let error = run(&paths, None).expect_err("unavailable terminal target must fail closed");
    assert!(
        error.to_string().contains("terminal target Pool member"),
        "unexpected terminal target error: {error}"
    );
    assert!(!paths.manifest.exists());
    let ledger = fs::read_to_string(&paths.ledger).expect("ledger");
    assert!(ledger.contains("\"residency\":\"unknown\""));
    assert!(ledger.contains("unavailable to reader"));
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
