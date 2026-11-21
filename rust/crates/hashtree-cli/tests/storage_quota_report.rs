#![cfg(feature = "lmdb")]

use anyhow::Result;
use hashtree_core::{nhash_encode, sha256};
use hashtree_lmdb::{
    LmdbBlobStore, PoolMemberConfig, PoolStore, PoolStoreConfig, SHARED_BLOB_POOL_DIR_NAME,
};
use std::{fs, process::Command};

const MIB: usize = 1024 * 1024;

#[test]
fn storage_stats_counts_unindexed_writable_bytes_and_excludes_legacy_reads() -> Result<()> {
    let home = tempfile::tempdir()?;
    let data = home.path().join("data");
    let config = home.path().join("config");
    fs::create_dir_all(&config)?;
    fs::write(
        config.join("config.toml"),
        "[storage]\nbackend = \"lmdb\"\nmax_size_gb = 1\n[updater]\nauto_check = false\n",
    )?;
    let member = home.path().join("member");
    let pool = PoolStore::open(
        data.join(SHARED_BLOB_POOL_DIR_NAME),
        PoolStoreConfig::default(),
    )?;
    pool.add_member(PoolMemberConfig::new(member.clone(), (16 * MIB) as u64))?;
    let catalogued = vec![1; MIB];
    pool.put_sync(sha256(&catalogued), &catalogued)?;
    drop(pool);

    // Direct member writes consume quota before they appear in the pool catalog.
    let writable = LmdbBlobStore::with_map_size_and_external_blob_options(member, 16 * MIB, None)?;
    let uncatalogued = vec![2; 2 * MIB];
    writable.put_sync(sha256(&uncatalogued), &uncatalogued)?;
    drop(writable);

    let legacy_dir = data.join("blobs");
    let legacy =
        LmdbBlobStore::with_map_size_and_external_blob_options(&legacy_dir, 16 * MIB, None)?;
    let old_blob = vec![3; MIB];
    let old_hash = sha256(&old_blob);
    legacy.put_sync(old_hash, &old_blob)?;
    drop(legacy);

    let cli = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_htree"))
            .env_clear()
            .env("HOME", home.path())
            .env("HTREE_CONFIG_DIR", &config)
            .env("HTREE_LMDB_HOT_BLOB_LEGACY_DIR", &legacy_dir)
            .arg("--data-dir")
            .arg(&data)
            .args(args)
            .output()
    };
    let read = cli(&["cat", &nhash_encode(&old_hash)?])?;
    assert!(
        read.status.success(),
        "{}",
        String::from_utf8_lossy(&read.stderr)
    );
    assert_eq!(sha256(&read.stdout), old_hash);
    let output = cli(&["storage", "stats"])?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = String::from_utf8(output.stdout)?;
    assert!(report.contains("Indexed trees: 0"), "{report}");
    assert!(report.contains("Total size:   1.0 MiB"), "{report}");
    assert!(report.contains("Utilization: 0.3%"), "{report}");
    assert!(report.contains("Writable quota used: 3.0 MiB"), "{report}");
    Ok(())
}
