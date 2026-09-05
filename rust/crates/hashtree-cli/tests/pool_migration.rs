mod common;

use common::htree_bin;
use hashtree_core::{sha256, to_hex};
use hashtree_lmdb::{
    migrate_lmdb_batch, ExternalBlobOptions, LmdbBlobReader, LmdbBlobStore, PoolMemberConfig,
    PoolStore, PoolStoreConfig, PoolStoreReader, SHARED_BLOB_POOL_DIR_NAME,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
#[cfg(target_os = "linux")]
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(target_os = "linux")]
use std::process::Output;
#[cfg(target_os = "linux")]
use std::sync::{Mutex, OnceLock};
#[cfg(target_os = "linux")]
use std::thread;
use std::time::SystemTime;
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};

const TEST_INVOCATION_ID: &str = "0123456789abcdef0123456789abcdef";
const TEST_NONCE: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
#[cfg(target_os = "linux")]
static SYSTEM_MANAGER_TEST: Mutex<()> = Mutex::new(());
#[cfg(target_os = "linux")]
static CONTROLLER_UNIT_SEQUENCE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

#[test]
fn migration_includes_a_real_first_byte_zero_hash_and_crosses_its_cursor_boundary() {
    let temp = tempfile::tempdir().expect("tempdir");
    let source_path = temp.path().join("source-first-byte-zero");
    let source = LmdbBlobStore::with_map_size(&source_path, 64 * 1024 * 1024)
        .expect("open real source LMDB");
    let first = (0u64..)
        .map(|counter| {
            let mut bytes = b"generated Pool migration range-boundary event ".to_vec();
            bytes.extend_from_slice(&counter.to_be_bytes());
            (sha256(&bytes), bytes)
        })
        .find(|(hash, _)| hash[0] == 0)
        .expect("find generated first-byte-00 hash");
    let later = (0u64..)
        .map(|counter| {
            let mut bytes = b"generated Pool migration later event ".to_vec();
            bytes.extend_from_slice(&counter.to_be_bytes());
            (sha256(&bytes), bytes)
        })
        .find(|(hash, _)| hash[0] >= 0x80)
        .expect("find generated later hash");
    assert_eq!(
        source
            .put_many_sync(&[first.clone(), later.clone()])
            .unwrap(),
        2
    );
    source.force_sync().expect("sync real source LMDB");
    drop(source);

    let target = PoolStore::open(
        temp.path().join("target-catalog"),
        PoolStoreConfig::default(),
    )
    .expect("open real target Pool");
    target
        .add_member(PoolMemberConfig::new(
            temp.path().join("target-member"),
            64 * 1024 * 1024,
        ))
        .expect("add real target member");
    let reader = LmdbBlobReader::open(&source_path, None).expect("open real source reader");

    let first_page =
        migrate_lmdb_batch(&reader, &target, None, 1).expect("migrate initial range page");
    assert_eq!(first_page.last_hash, Some(first.0));
    assert_eq!(first_page.inserted, 1);
    assert_eq!(target.get_sync(&first.0).unwrap(), Some(first.1));

    let second_page = migrate_lmdb_batch(&reader, &target, first_page.last_hash, 1)
        .expect("migrate range page after first-byte-00 cursor");
    assert_eq!(second_page.last_hash, Some(later.0));
    assert_eq!(second_page.inserted, 1);
    assert_eq!(target.get_sync(&later.0).unwrap(), Some(later.1));
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct MigrationScenario {
    _temp: tempfile::TempDir,
    pool_path: PathBuf,
    member_id: hashtree_lmdb::PoolMemberId,
    member_external: PathBuf,
    source_dir: PathBuf,
    source_external: PathBuf,
    controller_state: PathBuf,
    source_baseline: PathBuf,
    safety_cas: PathBuf,
    config_dir: PathBuf,
    cursor: PathBuf,
    request: PathBuf,
    ack: PathBuf,
    pool_topology: PathBuf,
    args: Vec<String>,
    blobs: Vec<([u8; 32], Vec<u8>)>,
}

impl MigrationScenario {
    fn generated(max_items: Option<usize>) -> Self {
        Self::generated_with_external_sync(max_items, true)
    }

    fn generated_with_external_sync(max_items: Option<usize>, external_blob_sync: bool) -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().canonicalize().expect("canonical tempdir");
        let source_dir = root.join("source");
        let source_external = root.join("source-external");
        let data_dir = root.join("target-data");
        let member_dir = root.join("target-member");
        let member_external = root.join("target-external");
        let cursor_dir = root.join("migration-cursor");
        fs::create_dir(&cursor_dir).expect("migration cursor directory");
        let cursor = cursor_dir.join("migration.cursor");
        let config_dir = root.join("config-must-remain-absent");

        let source = LmdbBlobStore::with_map_size_and_external_blob_options(
            &source_dir,
            64 * 1024 * 1024,
            Some(ExternalBlobOptions {
                base_path: source_external.clone(),
                min_bytes: 1,
                sync: true,
                pack_target_bytes: None,
            }),
        )
        .expect("open source");
        let blobs = (0..5u8)
            .map(|value| {
                let bytes = vec![value; 4 * 1024 + usize::from(value)];
                (sha256(&bytes), bytes)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            source.put_many_sync(&blobs).expect("populate source"),
            blobs.len()
        );
        source.force_sync().expect("sync source");
        drop(source);

        fs::create_dir_all(&data_dir).expect("target data directory");
        let pool_path = data_dir.join(SHARED_BLOB_POOL_DIR_NAME);
        let pool = PoolStore::open(&pool_path, PoolStoreConfig::default()).expect("open pool");
        let member_id = pool
            .add_member(
                PoolMemberConfig::new(member_dir.clone(), 64 * 1024 * 1024).with_external_blobs(
                    member_external.clone(),
                    1,
                    external_blob_sync,
                    None,
                ),
            )
            .expect("add member");
        drop(pool);
        let mut reader_config = PoolStoreConfig::default();
        reader_config.temperature.enabled = false;
        let manifest_identity = PoolStoreReader::open(&pool_path, reader_config)
            .expect("open generated Pool reader")
            .manifest_identity();

        let rollout = root.join("rollout-v3-test");
        let attempts = rollout.join("attempts-v3");
        let attempt = attempts.join(TEST_NONCE);
        fs::create_dir_all(&attempt).expect("attempt directory");
        let request = attempt.join("launch-request.json");
        let ack = attempt.join("launch-ack.json");

        let controller_state = rollout.join("state.json");
        let phase = if max_items.is_some() {
            "online-bounded"
        } else {
            "final-stopped-source"
        };
        let mut controller_state_json = json!({
            "schema": "hashtree-pool-migration-controller-state/v3",
            "rolloutId": "rollout-v3-test",
            "phase": phase,
            "bootId": current_boot_id(),
            "sourceLmdbIdentity": lmdb_identity(&source_dir),
            "sourceExternalIdentity": file_identity(&source_external),
            "poolLmdbIdentity": lmdb_identity(&pool_path),
            "poolManifestSha256": to_hex(&manifest_identity.sha256),
            "sourceWritersFenced": max_items.is_none(),
            "targetWritersFenced": max_items.is_none(),
            "fenceHeldUntilCompletion": max_items.is_none(),
            "sourceWriterProcessesWithOpenHandles": 0,
            "targetWriterProcessesWithOpenHandles": 0,
            "stoppedWriterUnits": ["hashtree-pool-writer-placeholder.service"],
            "writerUnitMasks": [],
            "sourceTerminalReceiptSha256": [],
        });
        let source_baseline = rollout.join("source-baseline.txt");
        write_file(&source_baseline, b"generated source baseline\n");
        let pool_topology = rollout.join("pool-topology.txt");
        let pool_topology_json = json!({
            "schema": "hashtree-pool-migration-topology/v3",
            "poolPath": pool_path,
            "manifestSha256": to_hex(&manifest_identity.sha256),
            "members": [{
                "id": member_id.to_string(),
                "path": member_dir,
                "directoryIdentity": file_identity(&member_dir),
                "lmdbIdentity": lmdb_identity(&member_dir),
                "marker": file_authority(&member_dir.join(".hashtree-pool-member-v1")),
                "externalPath": member_external,
                "externalDirectoryIdentity": file_identity(&member_external),
                "externalMarker": file_authority(
                    &member_external.join(".hashtree-pool-external-v1")
                ),
            }],
        });
        let mut pool_topology_bytes =
            serde_json::to_vec(&pool_topology_json).expect("serialize generated Pool topology");
        pool_topology_bytes.push(b'\n');
        write_file(&pool_topology, &pool_topology_bytes);
        controller_state_json["poolTopologySha256"] = Value::String(file_sha256(&pool_topology));
        let mut controller_state_bytes = serde_json::to_vec(&controller_state_json)
            .expect("serialize generated controller state");
        controller_state_bytes.push(b'\n');
        write_file(&controller_state, &controller_state_bytes);
        let safety_cas = rollout.join("safety.cas");
        write_file(&safety_cas, b"generated safety authority\n");

        let binary = PathBuf::from(htree_bin())
            .canonicalize()
            .expect("canonical htree binary");
        let mut args = vec![
            binary.display().to_string(),
            "--data-dir".to_string(),
            data_dir.display().to_string(),
            "storage".to_string(),
            "pool".to_string(),
            "migrate-lmdb".to_string(),
            "--launch-request".to_string(),
            request.display().to_string(),
            "--launch-request-wait-seconds".to_string(),
            "30".to_string(),
            "--source".to_string(),
            source_dir.display().to_string(),
            "--source-external-dir".to_string(),
            source_external.display().to_string(),
            "--state-file".to_string(),
            cursor.display().to_string(),
            "--batch-size".to_string(),
            "1".to_string(),
            "--reopen-batches".to_string(),
            "2".to_string(),
        ];
        if let Some(max_items) = max_items {
            args.extend(["--max-items".to_string(), max_items.to_string()]);
        }
        args.push("--resume".to_string());

        Self {
            _temp: temp,
            pool_path,
            member_id,
            member_external,
            source_dir,
            source_external,
            controller_state,
            source_baseline,
            safety_cas,
            config_dir,
            cursor,
            request,
            ack,
            pool_topology,
            args,
            blobs,
        }
    }

    #[cfg(target_os = "linux")]
    fn run(&self) -> (Output, PathBuf) {
        run_with_controller_mutations(self, json!([]))
    }

    #[cfg(target_os = "linux")]
    fn bind_controller_state_to_current_pool_topology(&self) {
        let controller_state_path = &self.controller_state;
        let mut controller_state: Value = serde_json::from_slice(
            &fs::read(&controller_state_path).expect("read generated controller state"),
        )
        .expect("parse generated controller state");
        controller_state["poolTopologySha256"] = Value::String(file_sha256(&self.pool_topology));
        let mut bytes =
            serde_json::to_vec(&controller_state).expect("serialize rebound controller state");
        bytes.push(b'\n');
        write_file(&controller_state_path, &bytes);
    }

    fn pool_count(&self) -> u64 {
        let pool =
            PoolStore::open(&self.pool_path, PoolStoreConfig::default()).expect("reopen Pool");
        pool.stats().expect("Pool stats").count
    }
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "release gate: requires passwordless sudo and system systemd"]
fn migration_reopens_live_mappings_and_completes_external_blob_copy() {
    let scenario = MigrationScenario::generated(Some(64));
    let (output, request) = run_fenced_migration(&scenario);
    assert_success(&output);

    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let ack_offset = stdout
        .find("Pool migration launch acknowledged:")
        .expect("acknowledgement output");
    let first_batch_offset = stdout
        .find("Migration batch:")
        .expect("migration batch output");
    assert!(
        ack_offset < first_batch_offset,
        "durable launch acknowledgement must precede migration work\n{stdout}"
    );
    assert_eq!(
        stdout.matches("Migration mappings reopened").count(),
        2,
        "five one-item batches should close and reopen the live LMDB mappings twice\n{stdout}"
    );
    assert!(
        stdout.contains("source bodies read+verified 5, inserted 5 blobs"),
        "unexpected migration report\n{stdout}"
    );
    assert_eq!(
        fs::read_to_string(&scenario.cursor).expect("cursor"),
        "complete\n"
    );
    let terminal_receipt: Value = serde_json::from_slice(
        &fs::read(&request.with_file_name("terminal-audit.json")).expect("terminal audit receipt"),
    )
    .expect("parse terminal audit receipt");
    assert_eq!(
        terminal_receipt["schema"],
        "hashtree-pool-migration-terminal-audit/v3"
    );
    assert_eq!(terminal_receipt["status"], "verified");
    assert_eq!(
        terminal_receipt["sourceEntries"],
        scenario.blobs.len() as u64
    );
    assert_eq!(
        terminal_receipt["targetStoredLocations"],
        scenario.blobs.len() as u64
    );
    let ack_bytes = fs::read(&request.with_file_name("launch-ack.json"))
        .expect("durable launch acknowledgement");
    let ack: Value = serde_json::from_slice(&ack_bytes).expect("parse launch acknowledgement");
    assert_eq!(ack["schema"], "hashtree-pool-migration-launch-ack/v3");
    assert_eq!(ack["status"], "acknowledged");
    assert_eq!(
        ack["systemdInvocationId"]
            .as_str()
            .expect("systemd invocation ID")
            .len(),
        32
    );
    assert_eq!(ack["requestPath"], request.display().to_string());
    assert_eq!(
        ack["requestSha256"],
        file_sha256(&request),
        "acknowledgement must bind the exact request bytes"
    );
    #[cfg(unix)]
    assert_eq!(
        fs::metadata(&request.with_file_name("launch-ack.json"))
            .expect("ack metadata")
            .mode()
            & 0o777,
        0o600
    );

    let reopened =
        PoolStore::open(&scenario.pool_path, PoolStoreConfig::default()).expect("reopen pool");
    let stats = reopened.stats().expect("pool stats");
    assert_eq!(stats.count, scenario.blobs.len() as u64);
    assert_eq!(
        stats.bytes,
        scenario
            .blobs
            .iter()
            .map(|(_, bytes)| bytes.len() as u64)
            .sum::<u64>()
    );
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "release gate: requires passwordless sudo and system systemd"]
fn target_only_catalog_corruption_blocks_online_certification_and_final_completion() {
    let scenario = MigrationScenario::generated(Some(64));
    let target_only = b"target-only body omitted from the source migration";
    let target_only_hash = sha256(target_only);
    let pool =
        PoolStore::open(&scenario.pool_path, PoolStoreConfig::default()).expect("reopen Pool");
    pool.put_sync(target_only_hash, target_only)
        .expect("write target-only body");
    pool.force_sync().expect("sync target-only body");
    drop(pool);
    let hex = to_hex(&target_only_hash);
    let external_path = scenario
        .member_external
        .join(&hex[..2])
        .join(&hex[2..4])
        .join(&hex[4..]);
    fs::remove_file(&external_path).expect("remove target-only external body");

    let (output, request) = run_fenced_migration(&scenario);
    assert!(!output.status.success(), "corrupt target reached complete");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("root target audit body differs from checkpoint hash/size authority")
            || stderr.contains("root-read online target audit bodies"),
        "unexpected terminal target audit failure\n{stderr}"
    );
    let authority: Value = serde_json::from_slice(&fs::read(&request).unwrap()).unwrap();
    assert_eq!(authority["controller"]["phase"], "online-bounded");
    assert!(request.with_file_name("launch-ack.json").exists());
    assert!(
        !request
            .with_file_name("online-target-audit-certification.json")
            .exists(),
        "failed target audit must not certify a final-source prerequisite"
    );
    assert!(
        !scenario.cursor.exists(),
        "terminal audit failure must precede complete cursor publication"
    );
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "release gate: requires passwordless sudo and system systemd"]
fn stopped_source_rejects_a_blob_omitted_from_online_metadata_reconciliation() {
    let scenario = MigrationScenario::generated(Some(64));
    let omitted_hash = scenario.blobs[0].0;
    let mut options = heed::EnvOpenOptions::new();
    options.max_dbs(5);
    let env = unsafe { options.open(&scenario.source_dir) }.expect("open source for keyset fault");
    let mut wtxn = env.write_txn().expect("source write transaction");
    let metadata: heed::Database<heed::types::Bytes, heed::types::Bytes> = env
        .open_database(&wtxn, Some("metadata"))
        .expect("open metadata database")
        .expect("metadata database");
    metadata
        .delete(&mut wtxn, &omitted_hash)
        .expect("remove one legacy metadata row");
    wtxn.commit().expect("commit mixed source keyset");
    env.force_sync().expect("sync mixed source keyset");
    env.prepare_for_closing().wait();

    let (output, request) = run_fenced_migration(&scenario);
    assert!(
        !output.status.success(),
        "mixed source keysets reached complete"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("stopped source key")
            && stderr.contains("is absent from the certified online target audit"),
        "unexpected terminal source audit failure\n{stderr}"
    );
    let authority: Value = serde_json::from_slice(&fs::read(&request).unwrap()).unwrap();
    assert_eq!(authority["controller"]["phase"], "final-stopped-source");
    assert!(request.with_file_name("launch-ack.json").exists());
    assert!(
        !request.with_file_name("source-terminal.json").exists(),
        "failed terminal source audit must not publish a verified receipt"
    );
    assert!(
        !scenario.cursor.exists() && !scenario.cursor.with_extension("source").exists(),
        "terminal source audit failure must precede any final-pass cursor publication"
    );
    assert_eq!(
        scenario.pool_count(),
        scenario.blobs.len() as u64 - 1,
        "the compact scan demonstrates that one blob would otherwise be silently omitted"
    );
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "release gate: requires passwordless sudo and system systemd"]
fn final_launch_requires_both_writer_fences_before_pool_open() {
    let scenario = MigrationScenario::generated(None);
    let state_path = &scenario.controller_state;
    let mut state: Value =
        serde_json::from_slice(&fs::read(&state_path).expect("read controller state"))
            .expect("parse controller state");
    state["targetWritersFenced"] = Value::Bool(false);
    let mut bytes = serde_json::to_vec(&state).expect("serialize changed controller state");
    bytes.push(b'\n');
    write_file(&state_path, &bytes);
    let pool_before = file_snapshot(&scenario.pool_path.join("data.mdb"));

    let (output, request) = scenario.run();
    assert!(!output.status.success(), "missing target fence launched");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("source and target writer fences"),
        "unexpected writer-fence failure\n{stderr}"
    );
    assert!(!request.with_file_name("launch-ack.json").exists());
    assert!(!scenario.cursor.exists());
    assert_eq!(
        file_snapshot(&scenario.pool_path.join("data.mdb")),
        pool_before,
        "writer-fence rejection must precede writable Pool open"
    );
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "release gate: requires passwordless sudo and system systemd"]
fn launch_ack_is_single_use_and_blocks_a_second_pool_open() {
    let _serialized = SYSTEM_MANAGER_TEST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let scenario = MigrationScenario::generated(Some(1));
    let (guard, arguments) = prepare_root_controller_systemd(&scenario);
    let first = run_root_controller(&guard, &arguments, false);
    assert_success(&first);
    let completed: Value = serde_json::from_slice(&first.stdout).expect("controller completion");
    let request = PathBuf::from(completed["requestPath"].as_str().unwrap());
    let cursor_after_first = fs::read(&scenario.cursor).expect("first cursor");
    let count_after_first = scenario.pool_count();
    assert_eq!(count_after_first, 1);

    let mut replay_args = scenario.args.clone();
    let request_index = replay_args
        .iter()
        .position(|arg| arg == "--launch-request")
        .unwrap()
        + 1;
    replay_args[request_index] = request.display().to_string();
    let second = migration_command(&replay_args[0])
        .args(&replay_args[1..])
        .env("INVOCATION_ID", TEST_INVOCATION_ID)
        .output()
        .expect("retry consumed launch request");
    assert!(
        !second.status.success(),
        "reused launch unexpectedly passed"
    );
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        stderr.contains("launch acknowledgement already exists"),
        "unexpected reuse error\n{stderr}"
    );
    assert_eq!(
        fs::read(&scenario.cursor).expect("cursor after rejected reuse"),
        cursor_after_first,
        "a rejected reused request must not advance the cursor"
    );
    assert_eq!(
        scenario.pool_count(),
        count_after_first,
        "a rejected reused request must not open the migration data path"
    );
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "release gate: requires passwordless sudo and system systemd"]
fn changed_pool_cas_fails_before_ack_or_migration() {
    let scenario = MigrationScenario::generated(Some(1));
    let pool_data = scenario.pool_path.join("data.mdb");
    let pool_sha_before = file_sha256(&pool_data);
    let pool_mtime_before = fs::metadata(&pool_data)
        .expect("Pool catalog metadata")
        .modified()
        .expect("Pool catalog mtime");

    let (output, request) = run_with_controller_mutations(
        &scenario,
        json!([
            { "pointer": "/pool/topology/sha256", "value": "f".repeat(64) }
        ]),
    );
    assert!(
        !output.status.success(),
        "changed Pool CAS unexpectedly passed"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Pool topology SHA-256 mismatch"),
        "unexpected CAS failure\n{stderr}"
    );
    assert!(
        request.exists() && !request.with_file_name("launch-ack.json").exists(),
        "failed authority must not be acknowledged"
    );
    assert!(
        !scenario.cursor.exists(),
        "failed authority must not create a migration cursor"
    );
    assert_eq!(scenario.pool_count(), 0);
    assert_eq!(file_sha256(&pool_data), pool_sha_before);
    assert_eq!(
        fs::metadata(&pool_data)
            .expect("Pool catalog metadata after failure")
            .modified()
            .expect("Pool catalog mtime after failure"),
        pool_mtime_before,
        "failed pre-open authority must not touch the Pool catalog"
    );
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "release gate: requires passwordless sudo and system systemd"]
fn live_pool_manifest_must_match_the_pinned_member_topology() {
    let scenario = MigrationScenario::generated(Some(1));
    let mut topology: Value =
        serde_json::from_slice(&fs::read(&scenario.pool_topology).expect("read topology"))
            .expect("parse topology");
    let original_member = PathBuf::from(
        topology["members"][0]["path"]
            .as_str()
            .expect("member path"),
    );
    let alternate_member = original_member.with_file_name("alternate-target-member");
    fs::create_dir(&alternate_member).expect("create alternate member");
    fs::copy(
        original_member.join(".hashtree-pool-member-v1"),
        alternate_member.join(".hashtree-pool-member-v1"),
    )
    .expect("copy generated member marker");
    for name in ["data.mdb", "lock.mdb"] {
        fs::copy(original_member.join(name), alternate_member.join(name))
            .expect("copy generated member LMDB file");
    }
    topology["members"][0]["path"] = Value::String(alternate_member.display().to_string());
    topology["members"][0]["directoryIdentity"] = file_identity(&alternate_member);
    topology["members"][0]["lmdbIdentity"] = lmdb_identity(&alternate_member);
    topology["members"][0]["marker"] =
        file_authority(&alternate_member.join(".hashtree-pool-member-v1"));
    let mut bytes = serde_json::to_vec(&topology).expect("serialize changed topology");
    bytes.push(b'\n');
    write_file(&scenario.pool_topology, &bytes);
    scenario.bind_controller_state_to_current_pool_topology();

    let (output, request) = scenario.run();
    assert!(
        !output.status.success(),
        "mismatched live Pool manifest unexpectedly passed"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("paths differ from pinned topology"),
        "unexpected live topology failure\n{stderr}"
    );
    assert!(
        request.with_file_name("launch-ack.json").exists(),
        "the Pool must only be opened after the durable acknowledgement"
    );
    assert!(!scenario.cursor.exists());
    assert_eq!(scenario.pool_count(), 0);
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "release gate: requires passwordless sudo and system systemd"]
fn exact_pool_manifest_hash_rejects_a_post_request_config_change() {
    let scenario = MigrationScenario::generated(Some(1));
    let pool =
        PoolStore::open(&scenario.pool_path, PoolStoreConfig::default()).expect("reopen Pool");
    pool.update_member_limits(scenario.member_id, 64 * 1024 * 1024, 63, 15)
        .expect("change generated member configuration");
    pool.force_sync().expect("sync changed Pool manifest");
    drop(pool);

    let (output, request) = scenario.run();
    assert!(
        !output.status.success(),
        "changed live Pool manifest unexpectedly passed"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("manifest SHA-256 differs from controlled authority"),
        "unexpected manifest identity failure\n{stderr}"
    );
    assert!(
        request.with_file_name("launch-ack.json").exists(),
        "the controlled Pool must only be opened after durable acknowledgement"
    );
    assert!(!scenario.cursor.exists());
    assert_eq!(scenario.pool_count(), 0);
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "release gate: requires passwordless sudo and system systemd"]
fn migration_rejects_a_manifest_member_without_durable_external_sync() {
    let scenario = MigrationScenario::generated_with_external_sync(Some(1), false);
    let (output, request) = scenario.run();
    assert!(
        !output.status.success(),
        "non-durable Pool member unexpectedly passed"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("requires external_blob_sync=true"),
        "unexpected external sync failure\n{stderr}"
    );
    assert!(request.with_file_name("launch-ack.json").exists());
    assert!(!scenario.cursor.exists());
    assert_eq!(scenario.pool_count(), 0);
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "release gate: requires passwordless sudo and system systemd"]
fn complete_cursor_is_a_non_launchable_terminal_state() {
    let scenario = MigrationScenario::generated(Some(1));
    write_file(&scenario.cursor, b"complete\n");
    let (output, request) = scenario.run();

    assert!(
        !output.status.success(),
        "complete cursor unexpectedly launched"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("complete migration cursor is terminal"),
        "unexpected terminal cursor failure\n{stderr}"
    );
    assert!(!request.with_file_name("launch-ack.json").exists());
    assert_eq!(scenario.pool_count(), 0);
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "release gate: requires passwordless sudo and system systemd"]
fn exact_argv_authority_rejects_an_unrequested_argument_value() {
    let scenario = MigrationScenario::generated(Some(1));
    let batch_size = scenario
        .args
        .iter()
        .position(|argument| argument == "--batch-size")
        .expect("batch-size argument");
    let (output, request) = run_with_controller_mutations(
        &scenario,
        json!([
            { "pointer": format!("/argv/{}", batch_size + 1), "value": "2" }
        ]),
    );
    assert!(!output.status.success(), "changed argv unexpectedly passed");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("argv does not match this process exactly"),
        "unexpected argv failure\n{stderr}"
    );
    assert!(request.exists());
    assert!(!request.with_file_name("launch-ack.json").exists());
    assert!(!scenario.cursor.exists());
    assert_eq!(scenario.pool_count(), 0);
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "release gate: requires passwordless sudo and system systemd"]
fn source_lmdb_leaf_symlink_is_rejected_before_ack() {
    let scenario = MigrationScenario::generated(Some(1));
    let source_data = scenario.source_dir.join("data.mdb");
    let retained_data = scenario.source_dir.join("retained-data.mdb");
    fs::rename(&source_data, &retained_data).expect("retain generated source data");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&retained_data, &source_data).expect("preplant source data symlink");

    let (output, request) = scenario.run();
    assert!(
        !output.status.success(),
        "symlinked source LMDB data unexpectedly passed"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("source LMDB data.mdb") || stderr.contains("Too many levels"),
        "unexpected source symlink failure\n{stderr}"
    );
    assert!(!request.with_file_name("launch-ack.json").exists());
    assert!(!scenario.cursor.exists());
    assert_eq!(scenario.pool_count(), 0);
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "release gate: requires passwordless sudo and system systemd"]
fn lmdb_leaf_hardlink_alias_is_rejected_before_ack() {
    let scenario = MigrationScenario::generated(Some(1));
    let mut topology: Value =
        serde_json::from_slice(&fs::read(&scenario.pool_topology).expect("read topology"))
            .expect("parse topology");
    let member = PathBuf::from(
        topology["members"][0]["path"]
            .as_str()
            .expect("member path"),
    );
    let member_data = member.join("data.mdb");
    fs::remove_file(&member_data).expect("remove generated member data");
    fs::hard_link(scenario.source_dir.join("data.mdb"), &member_data)
        .expect("hardlink source data as member data");
    topology["members"][0]["lmdbIdentity"] = lmdb_identity(&member);
    let mut bytes = serde_json::to_vec(&topology).expect("serialize aliased topology");
    bytes.push(b'\n');
    write_file(&scenario.pool_topology, &bytes);
    scenario.bind_controller_state_to_current_pool_topology();
    let pool_catalog_before = file_snapshot(&scenario.pool_path.join("data.mdb"));

    let (output, request) = scenario.run();
    assert!(!output.status.success(), "hardlinked LMDB leaves passed");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("LMDB leaf identity alias is forbidden"),
        "unexpected hardlink alias failure\n{stderr}"
    );
    assert!(!request.with_file_name("launch-ack.json").exists());
    assert!(!scenario.cursor.exists());
    assert_eq!(
        file_snapshot(&scenario.pool_path.join("data.mdb")),
        pool_catalog_before,
        "pre-ack alias rejection mutated the Pool catalog"
    );
}

#[test]
#[cfg_attr(
    target_os = "linux",
    ignore = "release gate: requires passwordless sudo for the root-owned attempt"
)]
fn rendezvous_timeout_is_pre_open_and_pre_mutation() {
    let scenario = MigrationScenario::generated(Some(1));
    #[cfg(target_os = "linux")]
    let _authority = RootAttemptAuthorityGuard::prepare(&scenario.request);
    let pool_data = scenario.pool_path.join("data.mdb");
    let source_data = scenario.source_dir.join("data.mdb");
    let pool_before = file_snapshot(&pool_data);
    let source_before = file_snapshot(&source_data);

    let mut args = scenario.args.clone();
    let wait_index = args
        .iter()
        .position(|argument| argument == "--launch-request-wait-seconds")
        .expect("wait flag");
    args[wait_index + 1] = "1".to_string();
    let output = migration_command(&args[0])
        .args(&args[1..])
        .env("INVOCATION_ID", TEST_INVOCATION_ID)
        .env("HTREE_CONFIG_DIR", &scenario.config_dir)
        .output()
        .expect("run request timeout");
    assert!(
        !output.status.success(),
        "missing request unexpectedly passed"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("timed out after 1 seconds"),
        "unexpected timeout failure\n{stderr}"
    );
    assert!(!scenario.request.exists());
    assert!(!scenario.ack.exists());
    let start_claim = scenario.request.with_file_name("launch-started.json");
    assert!(
        start_claim.exists(),
        "a timed-out process must durably consume its attempt nonce"
    );
    assert!(!scenario.cursor.exists());
    assert!(
        !scenario.config_dir.exists(),
        "rendezvous timeout must not create global config"
    );
    assert_eq!(file_snapshot(&pool_data), pool_before);
    assert_eq!(file_snapshot(&source_data), source_before);
    assert_eq!(scenario.pool_count(), 0);

    let reused = migration_command(&args[0])
        .args(&args[1..])
        .env("INVOCATION_ID", TEST_INVOCATION_ID)
        .env("HTREE_CONFIG_DIR", &scenario.config_dir)
        .output()
        .expect("rerun consumed timeout attempt");
    assert!(!reused.status.success(), "timed-out attempt was reusable");
    assert!(
        String::from_utf8_lossy(&reused.stderr).contains("launch start claim already exists"),
        "unexpected consumed-attempt error\n{}",
        String::from_utf8_lossy(&reused.stderr)
    );
}

#[test]
fn unsafe_lmdb_durability_environment_is_rejected_before_rendezvous() {
    for variable in ["HTREE_LMDB_NO_SYNC", "HTREE_LMDB_NO_META_SYNC"] {
        let scenario = MigrationScenario::generated(Some(1));
        let pool_data = scenario.pool_path.join("data.mdb");
        let source_data = scenario.source_dir.join("data.mdb");
        let pool_before = file_snapshot(&pool_data);
        let source_before = file_snapshot(&source_data);
        let output = migration_command(&scenario.args[0])
            .args(&scenario.args[1..])
            .env("INVOCATION_ID", TEST_INVOCATION_ID)
            .env("HTREE_CONFIG_DIR", &scenario.config_dir)
            .env(variable, "1")
            .output()
            .expect("run migration with unsafe LMDB durability environment");
        assert!(
            !output.status.success(),
            "{variable}=1 unexpectedly launched migration"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!(
                "{variable} must be absent from the Pool migration process environment"
            )),
            "unexpected durability environment failure\n{stderr}"
        );
        assert!(!scenario.request.exists());
        assert!(!scenario.ack.exists());
        assert!(!scenario.cursor.exists());
        assert!(!scenario.config_dir.exists());
        assert_eq!(file_snapshot(&pool_data), pool_before);
        assert_eq!(file_snapshot(&source_data), source_before);
        assert_eq!(scenario.pool_count(), 0);
    }
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "release gate: requires passwordless sudo and system systemd"]
fn main_pid_and_starttime_are_both_required() {
    let scenario = MigrationScenario::generated(Some(1));
    let (output, request) = run_with_controller_mutations(
        &scenario,
        json!([
            { "pointer": "/procStartTimeTicks", "value": 1 }
        ]),
    );
    assert!(
        !output.status.success(),
        "wrong process starttime unexpectedly passed"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("/proc starttime does not match"),
        "unexpected process identity failure\n{stderr}"
    );
    assert!(
        request.exists(),
        "the real controller must publish the malformed authority"
    );
    assert!(!request.with_file_name("launch-ack.json").exists());
    assert!(!scenario.cursor.exists());
    assert_eq!(scenario.pool_count(), 0);
}

#[cfg(target_os = "linux")]
fn controller_fixture_binary() -> &'static Path {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY
        .get_or_init(|| {
            // Cargo releases its build lock before running integration tests. Build
            // the unit-test harness once to obtain the controller's cfg(test) hook;
            // the worker still runs the ordinary production htree binary.
            let output = Command::new(env!("CARGO"))
                .current_dir(env!("CARGO_MANIFEST_DIR"))
                .args([
                    "test",
                    "--locked",
                    "--bin",
                    "htree",
                    "--no-run",
                    "--message-format=json",
                ])
                .output()
                .expect("build controller fixture test process");
            assert!(
                output.status.success(),
                "controller fixture build failed:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout)
                .expect("UTF-8 Cargo artifacts")
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .find_map(|artifact| {
                    if artifact["reason"] == "compiler-artifact"
                        && artifact["target"]["name"] == "htree"
                        && artifact["profile"]["test"] == true
                    {
                        artifact["executable"].as_str().map(PathBuf::from)
                    } else {
                        None
                    }
                })
                .expect("Cargo reported the htree unit-test executable")
        })
        .as_path()
}

#[cfg(target_os = "linux")]
fn run_with_controller_mutations(
    scenario: &MigrationScenario,
    mutations: Value,
) -> (Output, PathBuf) {
    let fixture_binary = (mutations != json!([])).then(controller_fixture_binary);
    let _serialized = SYSTEM_MANAGER_TEST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (guard, arguments) = prepare_root_controller_with_binary(scenario, fixture_binary);
    write_file(
        &guard
            .rollout
            .parent()
            .expect("fixture root")
            .join("controller-mutations.json"),
        &serde_json::to_vec(&mutations).expect("serialize controlled request mutations"),
    );
    let mut output = run_root_controller(&guard, &arguments, false);
    if guard.stderr_path.exists() {
        output
            .stderr
            .extend_from_slice(sudo_read_to_string(&guard.stderr_path).as_bytes());
    }
    if guard.stdout_path.exists() {
        output
            .stdout
            .extend_from_slice(sudo_read_to_string(&guard.stdout_path).as_bytes());
    }
    let attempts = guard.rollout.join("attempts-v3");
    drop(guard);
    let request = fs::read_dir(attempts)
        .expect("read controller attempts")
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("launch-request.json"))
        .find(|path| path.is_file())
        .unwrap_or_else(|| scenario.request.clone());
    (output, request)
}

#[cfg(target_os = "linux")]
fn set_argument(arguments: &mut [String], flag: &str, value: impl ToString) {
    let index = arguments
        .iter()
        .position(|arg| arg == flag)
        .expect("controller option")
        + 1;
    arguments[index] = value.to_string();
}

#[cfg(target_os = "linux")]
fn prepare_next_worker(
    guard: &mut RootControllerSystemdGuard,
    arguments: &mut [String],
    phase: &str,
) {
    let unit = guard.unit.replace(".service", &format!("-{phase}.service"));
    let fragment = guard.fragment.with_file_name(&unit);
    let environment = guard
        .installed_environment
        .with_extension(format!("{phase}.env"));
    assert!(
        !fragment.exists() && !environment.exists(),
        "fresh phase worker already exists"
    );
    let template = fs::read_to_string(&guard.fragment)
        .expect("previous worker fragment")
        .replace(
            guard.installed_environment.to_str().unwrap(),
            environment.to_str().unwrap(),
        );
    let local = guard.rollout.parent().unwrap().join("next-worker.service");
    write_file(&local, template.as_bytes());
    sudo_success(&[
        "/usr/bin/install",
        "-o",
        "root",
        "-g",
        "root",
        "-m",
        "0644",
        local.to_str().unwrap(),
        fragment.to_str().unwrap(),
    ]);
    guard.previous_workers.push((
        guard.unit.clone(),
        guard.fragment.clone(),
        guard.installed_environment.clone(),
    ));
    guard.unit = unit;
    guard.fragment = fragment;
    guard.installed_environment = environment;
    set_argument(arguments, "--systemd-unit", &guard.unit);
    set_argument(arguments, "--systemd-fragment", guard.fragment.display());
    set_argument(
        arguments,
        "--systemd-environment-file",
        guard.installed_environment.display(),
    );
}

#[cfg(target_os = "linux")]
fn run_fenced_migration(scenario: &MigrationScenario) -> (Output, PathBuf) {
    let _serialized = SYSTEM_MANAGER_TEST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let state_path = &scenario.controller_state;
    let mut state: Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    state["sourceWritersFenced"] = json!(true);
    state["targetWritersFenced"] = json!(true);
    state["fenceHeldUntilCompletion"] = json!(true);
    write_file(&state_path, &serde_json::to_vec(&state).unwrap());
    let (mut guard, mut arguments) = prepare_root_controller_systemd(scenario);
    set_argument(
        &mut arguments,
        "--state-file",
        scenario.cursor.with_extension("online").display(),
    );
    let mut combined_stdout = Vec::new();
    let mut launched_phase = "online-bounded";
    let mut output = run_root_controller(&guard, &arguments, false);
    for phase in ["final-stopped-source", "final-stopped-full"] {
        if !output.status.success() {
            break;
        }
        let completed: Value =
            serde_json::from_slice(&output.stdout).expect("completed controller JSON");
        assert_eq!(completed["status"], "completed");
        combined_stdout.extend_from_slice(&output.stdout);
        let (field, prefix) = if phase == "final-stopped-source" {
            ("onlineTargetAuditCertification", "online-target-audit-")
        } else {
            ("sourceTerminalCertification", "source-terminal-")
        };
        let certificate = &completed[field];
        let certificate_path = certificate["path"]
            .as_str()
            .expect("real prerequisite certification");
        if phase == "final-stopped-full" {
            let index = arguments
                .iter()
                .position(|arg| arg.starts_with("online-target-audit-"))
                .unwrap();
            assert_eq!(arguments[index - 1], "--cas");
            arguments.drain(index - 1..=index);
        }
        arguments.extend([
            "--cas".into(),
            format!(
                "{prefix}{}={certificate_path}",
                completed["nonce"].as_str().unwrap()
            ),
        ]);
        state = serde_json::from_slice(sudo_read_to_string(&state_path).as_bytes()).unwrap();
        state["phase"] = json!(phase);
        if phase == "final-stopped-full" {
            state["sourceTerminalReceiptSha256"] = json!([certificate["sha256"]]);
        }
        let input = guard
            .rollout
            .parent()
            .unwrap()
            .join("next-controller-state.json");
        write_file(&input, &serde_json::to_vec(&state).unwrap());
        sudo_success(&[
            "/usr/bin/install",
            "-o",
            "root",
            "-g",
            &unsafe { libc::getegid() }.to_string(),
            "-m",
            "0440",
            input.to_str().unwrap(),
            state_path.to_str().unwrap(),
        ]);
        set_argument(&mut arguments, "--phase", phase);
        set_argument(&mut arguments, "--batch-size", "4096");
        set_argument(
            &mut arguments,
            "--state-file",
            if phase == "final-stopped-source" {
                scenario.cursor.with_extension("source")
            } else {
                scenario.cursor.clone()
            }
            .display(),
        );
        if let Some(index) = arguments.iter().position(|arg| arg == "--max-items") {
            arguments.drain(index..index + 2);
        }
        prepare_next_worker(&mut guard, &mut arguments, phase);
        launched_phase = phase;
        output = run_root_controller(&guard, &arguments, false);
    }
    combined_stdout.extend_from_slice(&output.stdout);
    output.stdout = combined_stdout;
    if guard.stdout_path.exists() {
        output
            .stdout
            .extend_from_slice(sudo_read_to_string(&guard.stdout_path).as_bytes());
    }
    if guard.stderr_path.exists() {
        output
            .stderr
            .extend_from_slice(sudo_read_to_string(&guard.stderr_path).as_bytes());
    }
    let attempts = guard.rollout.join("attempts-v3");
    drop(guard);
    let mut requests = fs::read_dir(attempts)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("launch-request.json"))
        .filter(|path| path.is_file())
        .filter(|path| {
            let request: Value = serde_json::from_slice(&fs::read(path).expect("phase request"))
                .expect("parse phase request");
            request["controller"]["phase"] == launched_phase
        });
    let request = requests.next().unwrap_or_else(|| scenario.request.clone());
    assert!(
        requests.next().is_none(),
        "more than one attempt in a fixture phase"
    );
    (output, request)
}

#[cfg(unix)]
#[test]
fn rendezvous_rejects_a_symlinked_attempt_directory() {
    let scenario = MigrationScenario::generated(Some(1));
    let attempt = scenario.request.parent().expect("attempt directory");
    let real_attempt = attempt.with_file_name(format!("{TEST_NONCE}-real"));
    fs::rename(attempt, &real_attempt).expect("move real attempt");
    std::os::unix::fs::symlink(&real_attempt, attempt).expect("symlink attempt");

    let output = migration_command(&scenario.args[0])
        .args(&scenario.args[1..])
        .env("INVOCATION_ID", TEST_INVOCATION_ID)
        .env("HTREE_CONFIG_DIR", &scenario.config_dir)
        .output()
        .expect("run symlinked rendezvous");
    assert!(
        !output.status.success(),
        "symlinked rendezvous unexpectedly passed"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("must be an exact canonical path") || stderr.contains("traverses symlink"),
        "unexpected symlink failure\n{stderr}"
    );
    assert!(!real_attempt.join("launch-ack.json").exists());
    assert!(!scenario.cursor.exists());
    assert_eq!(scenario.pool_count(), 0);
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "release gate: requires passwordless sudo and system systemd"]
fn manager_scope_mismatch_is_rejected_by_the_real_unit_cgroup() {
    let scenario = MigrationScenario::generated(Some(1));
    let (output, request) = run_with_controller_mutations(
        &scenario,
        json!([
            { "pointer": "/systemdManager", "value": "user" }
        ]),
    );
    assert!(
        !output.status.success(),
        "wrong manager unexpectedly passed"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("systemd manager must be exactly system"),
        "unexpected manager failure\n{stderr}"
    );
    assert!(request.exists());
    assert!(!request.with_file_name("launch-ack.json").exists());
    assert!(!scenario.cursor.exists());
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "release gate: verifies direct processes cannot forge systemd ownership"]
fn direct_process_cannot_forge_systemd_ownership() {
    let scenario = MigrationScenario::generated(Some(1));
    let (output, request) =
        run_with_controller_mutations(&scenario, json!({ "directWorker": true }));
    assert!(
        !output.status.success(),
        "direct process forged systemd ownership"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("is not in the exact requested systemd service cgroup"),
        "unexpected direct-process failure\n{stderr}"
    );
    assert!(request.exists());
    assert!(!request.with_file_name("launch-ack.json").exists());
    assert!(!scenario.cursor.exists());
    assert_eq!(scenario.pool_count(), 0);
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "release gate: requires passwordless sudo and system systemd"]
fn migration_launch_is_bound_to_a_real_systemd_invocation() {
    let scenario = MigrationScenario::generated(Some(1));
    let (output, request) = scenario.run();
    assert_success(&output);
    let ack: Value = serde_json::from_slice(
        &fs::read(&request.with_file_name("launch-ack.json")).expect("systemd ack"),
    )
    .expect("parse systemd ack");
    assert_eq!(
        ack["systemdInvocationId"]
            .as_str()
            .expect("invocation ID")
            .len(),
        32
    );
    assert!(ack["pid"].as_u64().expect("ack pid") > 0);
    assert_eq!(
        ack["systemdManager"], "system",
        "ack must bind the system manager"
    );
    assert_eq!(ack["bootId"], current_boot_id());
    assert_eq!(scenario.pool_count(), 1);
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "release gate: requires passwordless sudo and system systemd"]
fn root_v3_controller_preflight_is_non_mutating_then_launches_the_real_worker() {
    let _serialized = SYSTEM_MANAGER_TEST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let scenario = MigrationScenario::generated(Some(1));
    let (guard, controller_args) = prepare_root_controller_systemd(&scenario);
    let pool_data = scenario.pool_path.join("data.mdb");
    let source_data = scenario.source_dir.join("data.mdb");
    let pool_before = file_snapshot(&pool_data);
    let source_before = file_snapshot(&source_data);
    let rollout_before = authority_tree_snapshot(&guard.rollout);

    let preflight = run_root_controller(&guard, &controller_args, true);
    assert_success(&preflight);
    let preflight_json: Value =
        serde_json::from_slice(&preflight.stdout).expect("parse controller preflight JSON");
    assert_eq!(preflight_json["status"], "ok");
    assert_eq!(preflight_json["mutation"], false);
    for unit in [&guard.controller_unit, &guard.unit] {
        assert_eq!(systemd_property(unit, "PrivateNetwork"), "yes");
        assert_eq!(systemd_property(unit, "PrivateMounts"), "no");
    }
    assert!(
        !guard.installed_environment.exists(),
        "preflight created the systemd environment file"
    );
    assert_eq!(
        authority_tree_snapshot(&guard.rollout),
        rollout_before,
        "preflight mutated the rollout authority tree"
    );
    assert_eq!(file_snapshot(&pool_data), pool_before);
    assert_eq!(file_snapshot(&source_data), source_before);
    assert_eq!(
        systemd_property(&guard.unit, "InvocationID"),
        "",
        "preflight started the systemd unit"
    );

    let launched = run_root_controller(&guard, &controller_args, false);
    assert_success(&launched);
    let result: Value =
        serde_json::from_slice(&launched.stdout).expect("parse controller result JSON");
    assert_eq!(result["status"], "completed");
    let request = PathBuf::from(result["requestPath"].as_str().expect("request path"));
    let ack = PathBuf::from(result["ackPath"].as_str().expect("ack path"));
    assert!(request.is_file(), "controller request is not durable");
    assert!(ack.is_file(), "worker acknowledgement is not durable");
    assert_eq!(
        file_sha256(&request),
        result["requestSha256"].as_str().expect("request SHA-256")
    );
    assert_eq!(
        file_sha256(&ack),
        result["ackSha256"].as_str().expect("ack SHA-256")
    );
    let authority: Value = serde_json::from_slice(&fs::read(&request).expect("launch request"))
        .expect("parse current controller launch authority");
    assert_eq!(
        authority["checkpointBroker"]["systemdUnit"],
        guard.controller_unit
    );
    assert_eq!(authority["systemdUnit"], guard.unit);
    assert_eq!(
        authority["executionNamespaces"]["mount"],
        host_mount_namespace_identity(),
        "controller request must bind the host mount namespace"
    );
    assert!(result["authorizedCheckpoints"].as_u64().unwrap() > 0);
    let worker_exit_status = wait_for_systemd_terminal(&guard.unit);
    let worker_stdout = sudo_read_to_string(&guard.stdout_path);
    let worker_stderr = sudo_read_to_string(&guard.stderr_path);
    assert_eq!(
        worker_exit_status, 0,
        "controller-launched worker failed\nstdout:\n{}\nstderr:\n{}",
        worker_stdout, worker_stderr
    );
    assert_eq!(scenario.pool_count(), 1);
}

#[cfg(target_os = "linux")]
struct RootControllerSystemdGuard {
    unit: String,
    fragment: PathBuf,
    installed_binary: PathBuf,
    installed_environment: PathBuf,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    rollout: PathBuf,
    controller_unit: String,
    controller_fragment: PathBuf,
    controller_environment: PathBuf,
    controller_stdout: PathBuf,
    controller_stderr: PathBuf,
    legacy_mask: PathBuf,
    controller_binary: PathBuf,
    controller_fixture: bool,
    writer_mask: Option<PathBuf>,
    source_mounts: Vec<PathBuf>,
    previous_workers: Vec<(String, PathBuf, PathBuf)>,
}

#[cfg(target_os = "linux")]
impl Drop for RootControllerSystemdGuard {
    fn drop(&mut self) {
        sudo_best_effort(&[
            "/usr/bin/systemctl",
            "--system",
            "stop",
            &self.controller_unit,
        ]);
        sudo_best_effort(&[
            "/usr/bin/systemctl",
            "--system",
            "reset-failed",
            &self.controller_unit,
        ]);
        sudo_best_effort(&["/usr/bin/systemctl", "--system", "stop", &self.unit]);
        sudo_best_effort(&["/usr/bin/systemctl", "--system", "reset-failed", &self.unit]);
        for (unit, fragment, environment) in &self.previous_workers {
            sudo_best_effort(&["/usr/bin/systemctl", "--system", "stop", unit]);
            sudo_best_effort(&["/usr/bin/systemctl", "--system", "reset-failed", unit]);
            for path in [fragment, environment] {
                sudo_best_effort(&["/usr/bin/rm", "-f", "--", path.to_str().unwrap()]);
            }
        }
        for path in [
            &self.fragment,
            &self.installed_binary,
            &self.controller_binary,
            &self.installed_environment,
            &self.stdout_path,
            &self.stderr_path,
            &self.controller_fragment,
            &self.controller_environment,
            &self.controller_stdout,
            &self.controller_stderr,
            &self.legacy_mask,
        ] {
            sudo_best_effort(&[
                "/usr/bin/rm",
                "-f",
                "--",
                path.to_str().expect("UTF-8 generated controller path"),
            ]);
        }
        for path in &self.source_mounts {
            if Command::new("/usr/bin/mountpoint")
                .args(["-q", path.to_str().unwrap()])
                .status()
                .is_ok_and(|status| status.success())
            {
                sudo_best_effort(&["/usr/bin/umount", "--", path.to_str().unwrap()]);
            }
        }
        if let Some(mask) = &self.writer_mask {
            sudo_best_effort(&["/usr/bin/rm", "-f", "--", mask.to_str().unwrap()]);
        }
        sudo_best_effort(&["/usr/bin/systemctl", "--system", "daemon-reload"]);
        restore_test_tree_ownership(&self.rollout);
        restore_test_tree_ownership(self.rollout.parent().expect("fixture root"));
    }
}

#[cfg(target_os = "linux")]
fn prepare_root_controller_systemd(
    scenario: &MigrationScenario,
) -> (RootControllerSystemdGuard, Vec<String>) {
    prepare_root_controller_with_binary(scenario, None)
}

#[cfg(target_os = "linux")]
fn prepare_root_controller_with_binary(
    scenario: &MigrationScenario,
    fixture_binary: Option<&Path>,
) -> (RootControllerSystemdGuard, Vec<String>) {
    let sequence = CONTROLLER_UNIT_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let unit = format!(
        "hashtree-pool-migration-worker@controller-test-{}-{sequence}.service",
        std::process::id()
    );
    let runtime_stem = format!(
        "/run/hashtree-pool-controller-test-{}-{sequence}",
        std::process::id()
    );
    let fragment = PathBuf::from(format!("/run/systemd/system/{unit}"));
    assert!(
        !fragment.exists(),
        "refusing to overwrite pre-existing migration template {}",
        fragment.display()
    );
    let installed_binary = PathBuf::from(format!("{runtime_stem}-htree"));
    let controller_binary = fixture_binary
        .map(|_| PathBuf::from(format!("{runtime_stem}-controller")))
        .unwrap_or_else(|| installed_binary.clone());
    let installed_environment = PathBuf::from(format!("{runtime_stem}.env"));
    let stdout_path = PathBuf::from(format!("{runtime_stem}.stdout"));
    let stderr_path = PathBuf::from(format!("{runtime_stem}.stderr"));
    let controller_unit = format!(
        "hashtree-pool-migration-controller@controller-test-{}-{sequence}.service",
        std::process::id()
    );
    let controller_fragment = PathBuf::from(format!("/run/systemd/system/{controller_unit}"));
    let controller_environment = PathBuf::from(format!("{runtime_stem}-controller.env"));
    let controller_stdout = PathBuf::from(format!("{runtime_stem}-controller.stdout"));
    let controller_stderr = PathBuf::from(format!("{runtime_stem}-controller.stderr"));
    let legacy_mask = PathBuf::from("/run/systemd/system/hashtree-pool-migrate@.service");
    for path in [
        &installed_binary,
        &controller_binary,
        &installed_environment,
        &stdout_path,
        &stderr_path,
        &controller_fragment,
        &controller_environment,
        &controller_stdout,
        &controller_stderr,
        &legacy_mask,
    ] {
        assert!(
            !path.exists(),
            "refusing to overwrite generated controller path {}",
            path.display()
        );
    }

    let rollout = scenario
        .request
        .parent()
        .expect("attempt")
        .parent()
        .expect("attempt namespace")
        .parent()
        .expect("rollout")
        .to_path_buf();
    let local_fragment = rollout.join("generated-root-controller-template.service");
    let template = format!(
        "[Unit]\nDescription=Generated root Pool migration v3 controller integration\nBindsTo={controller_unit}\n\n\
[Service]\nType=oneshot\nUser={}\nGroup={}\nEnvironmentFile={}\n\
ExecStart={} --data-dir ${{HTREE_POOL_TARGET_DATA_DIR}} storage pool migrate-lmdb --launch-request ${{HTREE_POOL_LAUNCH_REQUEST}} --launch-request-wait-seconds ${{HTREE_POOL_LAUNCH_WAIT_SECONDS}} --source ${{HTREE_POOL_SOURCE_LMDB_DIR}} $HTREE_POOL_SOURCE_EXTERNAL_ARGS --state-file ${{HTREE_POOL_STATE_FILE}} --batch-size ${{HTREE_POOL_BATCH_SIZE}} --max-buffer-mib ${{HTREE_POOL_MAX_BUFFER_MIB}} --source-read-concurrency ${{HTREE_POOL_SOURCE_READ_CONCURRENCY}} --reopen-batches ${{HTREE_POOL_REOPEN_BATCHES}} $HTREE_POOL_LIMIT_ARGS --resume\n\
Restart=no\nTimeoutStartSec=infinity\nNoNewPrivileges=true\nPrivateNetwork=true\nPrivateMounts=false\n\
UnsetEnvironment=LD_PRELOAD LD_AUDIT LD_LIBRARY_PATH DYLD_INSERT_LIBRARIES DYLD_LIBRARY_PATH HTREE_LMDB_NO_SYNC HTREE_LMDB_NO_META_SYNC\n\
UMask=0027\nStandardOutput=append:{}\nStandardError=append:{}\n",
        unsafe { libc::geteuid() },
        unsafe { libc::getegid() },
        installed_environment.display(),
        installed_binary.display(),
        stdout_path.display(),
        stderr_path.display(),
    );
    write_file(&local_fragment, template.as_bytes());
    sudo_success(&[
        "/usr/bin/install",
        "-s",
        "-o",
        "root",
        "-g",
        "root",
        "-m",
        "0555",
        &scenario.args[0],
        installed_binary.to_str().expect("UTF-8 installed binary"),
    ]);
    if let Some(fixture_binary) = fixture_binary {
        sudo_success(&[
            "/usr/bin/install",
            "-s",
            "-o",
            "root",
            "-g",
            "root",
            "-m",
            "0555",
            fixture_binary.to_str().expect("fixture controller binary"),
            controller_binary
                .to_str()
                .expect("installed fixture controller"),
        ]);
    }
    sudo_success(&[
        "/usr/bin/install",
        "-o",
        "root",
        "-g",
        "root",
        "-m",
        "0644",
        local_fragment.to_str().expect("UTF-8 local fragment"),
        fragment.to_str().expect("UTF-8 installed fragment"),
    ]);

    let controller_state = &scenario.controller_state;
    sudo_success(&[
        "/usr/bin/ln",
        "-s",
        "/dev/null",
        legacy_mask.to_str().expect("legacy mask"),
    ]);
    let mut state: Value =
        serde_json::from_slice(&fs::read(&controller_state).expect("controller state"))
            .expect("parse controller state");
    let writer_mask = if state["sourceWritersFenced"] == true
        || state["targetWritersFenced"] == true
    {
        let writer_unit = format!(
            "hashtree-pool-test-writer-{}-{sequence}.service",
            std::process::id()
        );
        let path = PathBuf::from(format!("/run/systemd/system/{writer_unit}"));
        assert!(!path.exists(), "generated writer mask already exists");
        sudo_success(&["/usr/bin/ln", "-s", "/dev/null", path.to_str().unwrap()]);
        state["stoppedWriterUnits"] = json!([writer_unit]);
        state["writerUnitMasks"] = json!([{
            "unit": writer_unit, "path": path, "identity": symlink_identity(&path), "target": "/dev/null"
        }]);
        Some(path)
    } else {
        state["stoppedWriterUnits"] = json!([]);
        None
    };
    state["legacyWorkerTemplateMask"] = json!({
        "unit": "hashtree-pool-migrate@.service",
        "path": legacy_mask,
        "identity": symlink_identity(&legacy_mask),
        "target": "/dev/null",
    });
    state["legacyWorkerInstanceMasks"] = json!([]);
    write_file(
        &controller_state,
        &serde_json::to_vec(&state).expect("serialize controller state"),
    );
    let local_controller_environment = rollout.join("generated-controller.env");
    write_file(
        &local_controller_environment,
        b"# Generated controller authority\n",
    );
    sudo_success(&[
        "/usr/bin/install",
        "-o",
        "root",
        "-g",
        "root",
        "-m",
        "0644",
        local_controller_environment
            .to_str()
            .expect("controller environment source"),
        controller_environment
            .to_str()
            .expect("controller environment"),
    ]);
    let source_baseline = &scenario.source_baseline;
    let safety_cas = &scenario.safety_cas;
    let source_external = &scenario.source_external;
    let group = unsafe { libc::getegid() }.to_string();
    sudo_success(&[
        "/usr/bin/chown",
        "-R",
        "root:root",
        rollout.to_str().expect("UTF-8 rollout"),
    ]);
    sudo_success(&[
        "/usr/bin/chmod",
        "0755",
        rollout.to_str().expect("UTF-8 rollout"),
    ]);
    sudo_success(&[
        "/usr/bin/chmod",
        "0755",
        scenario
            .request
            .parent()
            .expect("attempt")
            .parent()
            .expect("attempt namespace")
            .to_str()
            .expect("UTF-8 attempt namespace"),
    ]);
    for path in [
        controller_state,
        source_baseline,
        &scenario.pool_topology,
        safety_cas,
    ] {
        sudo_success(&[
            "/usr/bin/chown",
            &format!("root:{group}"),
            path.to_str().expect("UTF-8 controller evidence"),
        ]);
        sudo_success(&[
            "/usr/bin/chmod",
            "0440",
            path.to_str().expect("UTF-8 controller evidence"),
        ]);
    }
    sudo_success(&["/usr/bin/systemctl", "--system", "daemon-reload"]);

    let target_data = scenario.pool_path.parent().expect("target data directory");
    let final_phase = state["phase"] != "online-bounded";
    let mut arguments = vec![
        "storage".to_string(),
        "pool".to_string(),
        "launch-migrate-lmdb-v3".to_string(),
        "--rollout-dir".to_string(),
        rollout.display().to_string(),
        "--rollout-id".to_string(),
        "rollout-v3-test".to_string(),
        "--phase".to_string(),
        state["phase"].as_str().unwrap().to_string(),
        "--controller-executable".to_string(),
        controller_binary.display().to_string(),
        "--controller-systemd-unit".to_string(),
        controller_unit.clone(),
        "--controller-systemd-fragment".to_string(),
        controller_fragment.display().to_string(),
        "--controller-systemd-environment-file".to_string(),
        controller_environment.display().to_string(),
        "--controller-state-input".to_string(),
        controller_state.display().to_string(),
        "--source-baseline-input".to_string(),
        source_baseline.display().to_string(),
        "--pool-topology-input".to_string(),
        scenario.pool_topology.display().to_string(),
        "--cas".to_string(),
        format!("generated-safety-authority={}", safety_cas.display()),
        "--systemd-unit".to_string(),
        unit.clone(),
        "--systemctl".to_string(),
        "/usr/bin/systemctl".to_string(),
        "--systemd-fragment".to_string(),
        fragment.display().to_string(),
        "--systemd-environment-file".to_string(),
        installed_environment.display().to_string(),
        "--service-gid".to_string(),
        group,
        "--migration-binary".to_string(),
        installed_binary.display().to_string(),
        "--target-data-dir".to_string(),
        target_data.display().to_string(),
        "--pool".to_string(),
        scenario.pool_path.display().to_string(),
        "--source".to_string(),
        scenario.source_dir.display().to_string(),
        "--source-external-dir".to_string(),
        source_external.display().to_string(),
        "--state-file".to_string(),
        scenario.cursor.display().to_string(),
        "--batch-size".to_string(),
        if final_phase { "4096" } else { "1" }.to_string(),
        "--max-buffer-mib".to_string(),
        "64".to_string(),
        "--source-read-concurrency".to_string(),
        "4".to_string(),
        "--reopen-batches".to_string(),
        "2".to_string(),
        "--launch-request-wait-seconds".to_string(),
        "30".to_string(),
        "--acknowledgement-wait-seconds".to_string(),
        "30".to_string(),
    ];
    if !final_phase {
        let limit = scenario
            .args
            .iter()
            .position(|arg| arg == "--max-items")
            .expect("online limit");
        arguments.extend(["--max-items".to_string(), scenario.args[limit + 1].clone()]);
    }
    for unit in state["stoppedWriterUnits"].as_array().unwrap() {
        arguments.extend([
            "--writer-unit".to_string(),
            unit.as_str().unwrap().to_string(),
        ]);
    }
    (
        RootControllerSystemdGuard {
            unit,
            fragment,
            installed_binary,
            installed_environment,
            stdout_path,
            stderr_path,
            rollout,
            controller_unit,
            controller_fragment,
            controller_environment,
            controller_stdout,
            controller_stderr,
            legacy_mask,
            controller_binary,
            controller_fixture: fixture_binary.is_some(),
            writer_mask,
            source_mounts: vec![scenario.source_dir.clone(), source_external.clone()],
            previous_workers: Vec::new(),
        },
        arguments,
    )
}

#[cfg(target_os = "linux")]
fn run_root_controller(
    guard: &RootControllerSystemdGuard,
    arguments: &[String],
    preflight: bool,
) -> Output {
    let mut arguments = arguments.to_vec();
    if preflight {
        arguments.push("--preflight".to_string());
    }
    if guard.controller_fixture {
        let path = guard
            .rollout
            .parent()
            .expect("fixture root")
            .join("controller-arguments.json");
        write_file(
            &path,
            &serde_json::to_vec(&arguments).expect("serialize fixture controller args"),
        );
        let environment = guard
            .rollout
            .parent()
            .expect("fixture root")
            .join("controller-environment");
        write_file(&environment, format!(
            "HTREE_POOL_CONTROLLER_TEST_ARGUMENTS={}\nHTREE_POOL_CONTROLLER_TEST_MUTATIONS={}\n",
            path.display(), guard.rollout.parent().expect("fixture root").join("controller-mutations.json").display()
        ).as_bytes());
        sudo_success(&[
            "/usr/bin/install",
            "-o",
            "root",
            "-g",
            "root",
            "-m",
            "0644",
            environment
                .to_str()
                .expect("fixture controller environment"),
            guard
                .controller_environment
                .to_str()
                .expect("installed controller environment"),
        ]);
        arguments = vec![
            "--exact".into(),
            "app::pool_migration_controller::linux::publication_test_fixture::controller_process"
                .into(),
            "--ignored".into(),
            "--nocapture".into(),
        ];
    }
    let exec_start = std::iter::once(guard.controller_binary.display().to_string())
        .chain(arguments)
        .map(|arg| systemd_quote(&arg))
        .collect::<Vec<_>>()
        .join(" ");
    let template = format!(
        "[Unit]\nDescription=Generated dedicated migration controller\n\n[Service]\nType=exec\nUser=root\nGroup=root\nEnvironmentFile={}\nExecStart={exec_start}\nRestart=no\nTimeoutStartSec=infinity\nNoNewPrivileges=true\nPrivateNetwork=true\nPrivateMounts=false\nUnsetEnvironment=LD_PRELOAD LD_AUDIT LD_LIBRARY_PATH DYLD_INSERT_LIBRARIES DYLD_LIBRARY_PATH HTREE_LMDB_NO_SYNC HTREE_LMDB_NO_META_SYNC\nUMask=0027\nStandardOutput=truncate:{}\nStandardError=truncate:{}\n",
        guard.controller_environment.display(), guard.controller_stdout.display(), guard.controller_stderr.display()
    );
    // Keep generated service fragments outside the rollout evidence tree so a
    // preflight's non-mutation assertion covers the complete rollout authority.
    let local_fragment = guard
        .rollout
        .parent()
        .expect("fixture root")
        .join("controller.service");
    write_file(&local_fragment, template.as_bytes());
    sudo_success(&[
        "/usr/bin/install",
        "-o",
        "root",
        "-g",
        "root",
        "-m",
        "0644",
        local_fragment.to_str().expect("local controller fragment"),
        guard
            .controller_fragment
            .to_str()
            .expect("installed controller fragment"),
    ]);
    sudo_success(&["/usr/bin/systemctl", "--system", "daemon-reload"]);
    sudo_success(&[
        "/usr/bin/systemctl",
        "--system",
        "start",
        &guard.controller_unit,
    ]);
    let exit_code = wait_for_systemd_terminal(&guard.controller_unit);
    Output {
        status: std::process::ExitStatus::from_raw(exit_code << 8),
        stdout: sudo_read_to_string(&guard.controller_stdout).into_bytes(),
        stderr: sudo_read_to_string(&guard.controller_stderr).into_bytes(),
    }
}

#[cfg(target_os = "linux")]
fn authority_tree_snapshot(root: &Path) -> Vec<(PathBuf, u32, u64, String)> {
    let mut snapshot = walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .map(|entry| entry.expect("walk authority tree"))
        .filter(|entry| entry.path() != root)
        .map(|entry| {
            let path = entry.path();
            let relative = path.strip_prefix(root).expect("relative authority path");
            let metadata = fs::symlink_metadata(path).expect("authority metadata");
            let digest = if metadata.file_type().is_file() {
                file_sha256(path)
            } else if metadata.file_type().is_dir() {
                "directory".to_string()
            } else {
                "other".to_string()
            };
            (
                relative.to_path_buf(),
                metadata.mode(),
                metadata.len(),
                digest,
            )
        })
        .collect::<Vec<_>>();
    snapshot.sort_by(|left, right| left.0.cmp(&right.0));
    snapshot
}

#[cfg(target_os = "linux")]
fn host_mount_namespace_identity() -> Value {
    // Reading PID 1's namespace link requires root on hardened Linux hosts.
    let output = Command::new("/usr/bin/sudo")
        .args([
            "-n",
            "/usr/bin/stat",
            "--dereference",
            "--format=%d %i",
            "/proc/1/ns/mnt",
        ])
        .output()
        .expect("inspect host mount namespace");
    assert_success(&output);
    let values = String::from_utf8(output.stdout)
        .expect("namespace identity")
        .split_whitespace()
        .map(|part| part.parse::<u64>().expect("namespace device/inode"))
        .collect::<Vec<_>>();
    assert_eq!(values.len(), 2);
    json!({ "device": values[0], "inode": values[1] })
}

#[cfg(target_os = "linux")]
fn systemd_property(unit: &str, property: &str) -> String {
    let output = Command::new("/usr/bin/systemctl")
        .args([
            "--system",
            "--no-pager",
            "show",
            unit,
            &format!("--property={property}"),
            "--value",
        ])
        .output()
        .expect("query systemd property");
    assert!(
        output.status.success(),
        "systemctl property query failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("UTF-8 systemd property")
        .trim()
        .to_string()
}

#[cfg(target_os = "linux")]
fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "migration failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn migration_command(program: &str) -> Command {
    let mut command = Command::new(program);
    for variable in [
        "LD_PRELOAD",
        "LD_AUDIT",
        "LD_LIBRARY_PATH",
        "DYLD_INSERT_LIBRARIES",
        "DYLD_LIBRARY_PATH",
        "HTREE_LMDB_NO_SYNC",
        "HTREE_LMDB_NO_META_SYNC",
        "HTREE_POOL_TARGET_DATA_DIR",
        "HTREE_POOL_LAUNCH_REQUEST",
        "HTREE_POOL_LAUNCH_WAIT_SECONDS",
        "HTREE_POOL_SOURCE_LMDB_DIR",
        "HTREE_POOL_SOURCE_EXTERNAL_ARGS",
        "HTREE_POOL_STATE_FILE",
        "HTREE_POOL_BATCH_SIZE",
        "HTREE_POOL_MAX_BUFFER_MIB",
        "HTREE_POOL_SOURCE_READ_CONCURRENCY",
        "HTREE_POOL_REOPEN_BATCHES",
        "HTREE_POOL_LIMIT_ARGS",
    ] {
        command.env_remove(variable);
    }
    command
}

fn write_file(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
    fs::File::open(path)
        .unwrap_or_else(|error| panic!("open {}: {error}", path.display()))
        .sync_all()
        .unwrap_or_else(|error| panic!("sync {}: {error}", path.display()));
}

#[cfg(target_os = "linux")]
fn prepare_root_attempt_authority(request: &Path) {
    let attempt = request.parent().expect("request attempt directory");
    let attempts = attempt.parent().expect("attempt namespace");
    let group = unsafe { libc::getegid() }.to_string();
    sudo_success(&[
        "/usr/bin/chown",
        "root:root",
        attempts.to_str().expect("UTF-8 attempt namespace"),
    ]);
    sudo_success(&[
        "/usr/bin/chmod",
        "0755",
        attempts.to_str().expect("UTF-8 attempt namespace"),
    ]);
    sudo_success(&[
        "/usr/bin/chown",
        &format!("root:{group}"),
        attempt.to_str().expect("UTF-8 attempt directory"),
    ]);
    sudo_success(&[
        "/usr/bin/chmod",
        "1770",
        attempt.to_str().expect("UTF-8 attempt directory"),
    ]);
}

#[cfg(target_os = "linux")]
struct RootAttemptAuthorityGuard {
    attempts: PathBuf,
}

#[cfg(target_os = "linux")]
impl RootAttemptAuthorityGuard {
    fn prepare(request: &Path) -> Self {
        prepare_root_attempt_authority(request);
        Self {
            attempts: request
                .parent()
                .expect("attempt")
                .parent()
                .expect("attempt namespace")
                .to_path_buf(),
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for RootAttemptAuthorityGuard {
    fn drop(&mut self) {
        restore_test_tree_ownership(&self.attempts);
    }
}

#[cfg(target_os = "linux")]
fn restore_test_tree_ownership(path: &Path) {
    let owner = format!("{}:{}", unsafe { libc::geteuid() }, unsafe {
        libc::getegid()
    });
    sudo_best_effort(&[
        "/usr/bin/chown",
        "-R",
        &owner,
        path.to_str().expect("UTF-8 generated attempts path"),
    ]);
}

#[cfg(target_os = "linux")]
fn sudo_success(arguments: &[&str]) {
    let output = Command::new("/usr/bin/sudo")
        .arg("-n")
        .args(arguments)
        .output()
        .unwrap_or_else(|error| panic!("run sudo {}: {error}", arguments.join(" ")));
    assert!(
        output.status.success(),
        "sudo command failed: {}\nstdout:\n{}\nstderr:\n{}",
        arguments.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(target_os = "linux")]
fn sudo_best_effort(arguments: &[&str]) {
    let _ = Command::new("/usr/bin/sudo")
        .arg("-n")
        .args(arguments)
        .output();
}

#[cfg(target_os = "linux")]
fn sudo_read_to_string(path: &Path) -> String {
    let output = Command::new("/usr/bin/sudo")
        .args([
            "-n",
            "/usr/bin/cat",
            "--",
            path.to_str().expect("UTF-8 generated systemd log path"),
        ])
        .output()
        .expect("read generated systemd log as root");
    if output.status.success() {
        String::from_utf8_lossy(&output.stdout).into_owned()
    } else {
        format!(
            "unable to read {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
}

#[cfg(target_os = "linux")]
fn systemd_quote(value: &str) -> String {
    assert!(
        !value.contains('\n') && !value.contains('\r'),
        "generated systemd argument contains a newline"
    );
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
    )
}

fn file_authority(path: &Path) -> Value {
    json!({
        "path": path,
        "sha256": file_sha256(path),
    })
}

#[cfg(unix)]
fn file_identity(path: &Path) -> Value {
    let metadata = fs::metadata(path)
        .unwrap_or_else(|error| panic!("inspect identity {}: {error}", path.display()));
    json!({
        "device": metadata.dev(),
        "inode": metadata.ino(),
    })
}

#[cfg(unix)]
fn symlink_identity(path: &Path) -> Value {
    let metadata = fs::symlink_metadata(path)
        .unwrap_or_else(|error| panic!("inspect symlink identity {}: {error}", path.display()));
    assert!(
        metadata.file_type().is_symlink(),
        "{} is not a generated symlink authority",
        path.display()
    );
    json!({
        "device": metadata.dev(),
        "inode": metadata.ino(),
    })
}

#[cfg(not(unix))]
fn file_identity(_path: &Path) -> Value {
    json!({ "device": 1, "inode": 1 })
}

fn lmdb_identity(path: &Path) -> Value {
    json!({
        "directory": file_identity(path),
        "data": file_identity(&path.join("data.mdb")),
        "lock": file_identity(&path.join("lock.mdb")),
    })
}

fn file_sha256(path: &Path) -> String {
    let bytes = fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    hex::encode(Sha256::digest(bytes))
}

fn file_snapshot(path: &Path) -> (String, u64, SystemTime) {
    let metadata =
        fs::metadata(path).unwrap_or_else(|error| panic!("stat {}: {error}", path.display()));
    (
        file_sha256(path),
        metadata.len(),
        metadata
            .modified()
            .unwrap_or_else(|error| panic!("read mtime for {}: {error}", path.display())),
    )
}

#[cfg(target_os = "linux")]
fn current_boot_id() -> String {
    fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .expect("Linux boot ID")
        .trim()
        .to_ascii_lowercase()
}

#[cfg(target_os = "macos")]
fn current_boot_id() -> String {
    let output = Command::new("/usr/sbin/sysctl")
        .args(["-n", "kern.bootsessionuuid"])
        .output()
        .expect("macOS boot session UUID");
    assert!(output.status.success(), "sysctl bootsessionuuid failed");
    String::from_utf8(output.stdout)
        .expect("UTF-8 boot session UUID")
        .trim()
        .to_ascii_lowercase()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn current_boot_id() -> String {
    panic!("Pool migration launch tests require a supported OS boot ID");
}

#[cfg(target_os = "linux")]
fn wait_for_systemd_terminal(unit: &str) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let output = Command::new("/usr/bin/systemctl")
            .args([
                "--system",
                "show",
                unit,
                "--property",
                "ActiveState",
                "--property",
                "Result",
                "--property",
                "ExecMainCode",
                "--property",
                "ExecMainStatus",
            ])
            .output()
            .expect("inspect systemd terminal result");
        assert!(
            output.status.success(),
            "systemctl show failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).expect("UTF-8 systemd terminal properties");
        if text.contains("ActiveState=inactive") || text.contains("ActiveState=failed") {
            let status = text
                .lines()
                .find_map(|line| line.strip_prefix("ExecMainStatus="))
                .and_then(|value| value.parse::<i32>().ok())
                .expect("systemd terminal state has ExecMainStatus");
            return status.clamp(0, 255);
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for real systemd migration\n{text}"
        );
        thread::sleep(Duration::from_millis(25));
    }
}
