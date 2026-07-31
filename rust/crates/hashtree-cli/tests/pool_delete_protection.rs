mod common;

use common::htree_bin;
use hashtree_core::{sha256, to_hex};
use hashtree_lmdb::{
    PoolMemberConfig, PoolStore, PoolStoreConfig, POOL_DELETE_PROTECTED, SHARED_BLOB_POOL_DIR_NAME,
};
use serde_json::Value;
use std::process::Command;

#[test]
fn real_cli_persists_delete_only_protection_while_pool_writes_continue() {
    let temp = tempfile::tempdir().expect("tempdir");
    let data_dir = temp.path().join("data");
    let catalog = data_dir.join(SHARED_BLOB_POOL_DIR_NAME);
    let member = temp.path().join("member");
    let pool = PoolStore::open(&catalog, PoolStoreConfig::default()).expect("open PoolStore");
    pool.add_member(PoolMemberConfig::new(member, 16 * 1024 * 1024))
        .expect("add member");
    let existing_hash = sha256(b"real-cli-delete-protected-blob");
    pool.put_sync(existing_hash, b"real-cli-delete-protected-blob")
        .expect("put existing blob");
    drop(pool);

    let lease_id = sha256(b"real-cli-legacy-retirement-lease");
    let uppercase = Command::new(htree_bin())
        .arg("--data-dir")
        .arg(&data_dir)
        .args([
            "storage",
            "pool",
            "protect-deletes",
            "--lease-id",
            &to_hex(&lease_id).to_uppercase(),
            "--reason",
            "legacy-source-retirement",
        ])
        .output()
        .expect("reject uppercase protection identity");
    assert!(!uppercase.status.success());
    assert!(String::from_utf8_lossy(&uppercase.stderr)
        .contains("exactly 64 lowercase hexadecimal characters"));

    let acquire = Command::new(htree_bin())
        .arg("--data-dir")
        .arg(&data_dir)
        .args([
            "storage",
            "pool",
            "protect-deletes",
            "--lease-id",
            &to_hex(&lease_id),
            "--reason",
            "legacy-source-retirement",
        ])
        .output()
        .expect("run protect-deletes");
    assert!(
        acquire.status.success(),
        "protect-deletes failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&acquire.stdout),
        String::from_utf8_lossy(&acquire.stderr),
    );
    let receipt: Value =
        serde_json::from_slice(&acquire.stdout).expect("parse acquisition receipt");
    assert_eq!(receipt["schema"], "hashtree-pool-delete-protection/v1");
    assert_eq!(receipt["action"], "acquire");
    assert_eq!(receipt["changed"], true);
    assert_eq!(receipt["leaseId"], to_hex(&lease_id));
    let record_sha256 = receipt["recordSha256"]
        .as_str()
        .expect("record identity")
        .to_owned();

    let reopened = PoolStore::open(&catalog, PoolStoreConfig::default()).expect("reopen PoolStore");
    let delete_error = reopened
        .delete_sync(&existing_hash)
        .expect_err("protected delete must fail");
    assert!(delete_error.to_string().contains(POOL_DELETE_PROTECTED));
    let new_hash = sha256(b"real-cli-write-during-delete-protection");
    assert!(reopened
        .put_sync(new_hash, b"real-cli-write-during-delete-protection")
        .expect("write during delete protection"));
    drop(reopened);

    let status = Command::new(htree_bin())
        .arg("--data-dir")
        .arg(&data_dir)
        .args(["storage", "pool", "status"])
        .output()
        .expect("run Pool status");
    assert!(status.status.success());
    let status = String::from_utf8(status.stdout).expect("UTF-8 status");
    assert!(status.contains("Delete protection: active"));
    assert!(status.contains(&to_hex(&lease_id)));
    assert!(status.contains(&record_sha256));

    let release = Command::new(htree_bin())
        .arg("--data-dir")
        .arg(&data_dir)
        .args([
            "storage",
            "pool",
            "release-delete-protection",
            "--lease-id",
            &to_hex(&lease_id),
            "--record-sha256",
            &record_sha256,
        ])
        .output()
        .expect("run exact delete-protection release");
    assert!(
        release.status.success(),
        "release failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&release.stdout),
        String::from_utf8_lossy(&release.stderr),
    );
    let release: Value = serde_json::from_slice(&release.stdout).expect("parse release receipt");
    assert_eq!(release["action"], "release");
    assert_eq!(release["changed"], true);

    let reopened = PoolStore::open(&catalog, PoolStoreConfig::default()).expect("final reopen");
    assert!(reopened
        .delete_protection_status()
        .expect("final protection status")
        .is_none());
    assert!(reopened
        .delete_sync(&existing_hash)
        .expect("delete after exact release"));
}
