mod common;

use anyhow::{bail, Context, Result};
use common::{htree_bin, write_keys_file};
use hashtree_cli::HashtreeStore;
use hashtree_config::StorageBackend;
use hashtree_core::{sha256, Hash};
use nostr::nips::nip19::ToBech32;
use nostr::{Event, EventBuilder, Keys, Kind, Tag, TagKind};
use nostr_sdk::ClientBuilder;
use serde_json::Value;
use std::fs::{self, File};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use walkdir::WalkDir;

const TEST_STORAGE_MAX_SIZE_BYTES: u64 = 16 * 1024 * 1024 * 1024;
const POOL_AUDIT_REASON: &str = "pool-audit-read-only";

struct RunningDaemon {
    child: Child,
    log_path: PathBuf,
}

impl RunningDaemon {
    fn spawn(
        home: &Path,
        config_dir: &Path,
        data_dir: &Path,
        addr: &str,
        audit_read_only: bool,
    ) -> Result<Self> {
        let log_path = home.join("daemon.log");
        let log = File::create(&log_path).context("create daemon log")?;
        let mut command = Command::new(htree_bin());
        command
            .arg("--data-dir")
            .arg(data_dir)
            .arg("start")
            .arg("--addr")
            .arg(addr)
            .env("HOME", home)
            .env("HTREE_CONFIG_DIR", config_dir)
            .env("RUST_LOG", "warn")
            .env_remove("HTREE_LMDB_HOT_BLOB_DIR")
            .env_remove("HTREE_LMDB_HOT_BLOB_LEGACY_DIR")
            .env_remove("HTREE_LMDB_HOT_EXTERNAL_BLOB_DIR")
            .env_remove("HTREE_LMDB_LEGACY_EXTERNAL_BLOB_DIR")
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log));
        if audit_read_only {
            command.env(hashtree_lmdb::POOL_AUDIT_READ_ONLY_ENV, "1");
        } else {
            command.env_remove(hashtree_lmdb::POOL_AUDIT_READ_ONLY_ENV);
        }
        let child = command.spawn().context("spawn htree start")?;
        Ok(Self { child, log_path })
    }

    async fn wait_for_health(&mut self, addr: &str, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let url = format!("http://{addr}/health");
        let client = reqwest::Client::new();
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().context("poll htree start")? {
                bail!(
                    "htree start exited with {status}\n{}",
                    fs::read_to_string(&self.log_path).unwrap_or_default()
                );
            }
            if client
                .get(&url)
                .send()
                .await
                .is_ok_and(|response| response.status().is_success())
            {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        bail!(
            "timed out waiting for {url}\n{}",
            fs::read_to_string(&self.log_path).unwrap_or_default()
        )
    }

    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for RunningDaemon {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Debug, Eq, PartialEq)]
struct DurableFileSnapshot {
    path: PathBuf,
    len: u64,
    sha256: Hash,
    modified: std::time::SystemTime,
}

fn durable_file_snapshot(root: &Path) -> Vec<DurableFileSnapshot> {
    let mut snapshot = WalkDir::new(root)
        .into_iter()
        .filter_map(std::result::Result::ok)
        .filter(|entry| {
            entry.file_type().is_file()
                && entry.file_name() != "lock.mdb"
                && entry.file_name() != "pool.lock"
        })
        .map(|entry| {
            let absolute = entry.into_path();
            let bytes = fs::read(&absolute).expect("read durable data file");
            let metadata = fs::metadata(&absolute).expect("read durable data metadata");
            DurableFileSnapshot {
                path: absolute
                    .strip_prefix(root)
                    .expect("snapshot path below root")
                    .to_path_buf(),
                len: metadata.len(),
                sha256: sha256(&bytes),
                modified: metadata.modified().expect("durable data mtime"),
            }
        })
        .collect::<Vec<_>>();
    snapshot.sort_unstable_by(|left, right| left.path.cmp(&right.path));
    snapshot
}

fn free_local_addr() -> Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0").context("bind ephemeral port")?;
    Ok(listener.local_addr()?.to_string())
}

fn write_daemon_config(config_dir: &Path, relays: &[String]) -> Result<()> {
    fs::create_dir_all(config_dir).context("create config directory")?;
    let relays = relays
        .iter()
        .map(|relay| format!("\"{relay}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let config = format!(
        r#"
[server]
enable_auth = false
enable_fips = false
public_writes = true
public_plaintext_reads = true
fips_request_timeout_ms = 500

[storage]
backend = "lmdb"
max_size_gb = 16

[nostr]
enabled = true
event_transport = "relay"
relays = [{relays}]
bootstrap_follows = []
social_graph_crawl_depth = 0
db_max_size_gb = 1
spambox_max_size_gb = 0

[blossom]
servers = []
read_servers = []
write_servers = []
replicate_servers = []

[sync]
enabled = false
sync_own = false
sync_followed = false

[updater]
auto_check = false
"#
    );
    fs::write(config_dir.join("config.toml"), config).context("write daemon config")?;
    Ok(())
}

async fn publish_to_real_htree_relay(relay_url: &str, event: &Event) -> Result<()> {
    let client = ClientBuilder::default().build();
    client
        .add_relay(relay_url)
        .await
        .context("add real htree relay")?;
    client.connect().await;
    client
        .send_event(event)
        .await
        .context("publish generated root event to real htree relay")?;
    client.disconnect().await;
    Ok(())
}

async fn assert_audit_rejection(response: reqwest::Response) -> Result<()> {
    assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response
            .headers()
            .get("x-hashtree-maintenance-reason")
            .and_then(|value| value.to_str().ok()),
        Some(POOL_AUDIT_REASON)
    );
    Ok(())
}

#[tokio::test]
async fn real_start_resolves_external_root_without_pool_audit_writes() -> Result<()> {
    let source_home = TempDir::new().context("create source home")?;
    let source_config = source_home.path().join(".hashtree");
    let source_data = source_home.path().join("data");
    let source_addr = free_local_addr()?;
    let source_keys = Keys::generate();
    write_daemon_config(&source_config, &[])?;
    write_keys_file(
        &source_config,
        &source_keys
            .secret_key()
            .to_bech32()
            .context("encode nsec")?,
    )?;
    let mut source = RunningDaemon::spawn(
        source_home.path(),
        &source_config,
        &source_data,
        &source_addr,
        false,
    )?;
    source
        .wait_for_health(&source_addr, Duration::from_secs(15))
        .await?;

    let target_home = TempDir::new().context("create target home")?;
    let target_config = target_home.path().join(".hashtree");
    let target_data = target_home.path().join("data");
    let target_addr = free_local_addr()?;
    let source_relay_url = format!("ws://{source_addr}/ws");
    write_daemon_config(&target_config, &[source_relay_url])?;
    let target_keys = Keys::generate();
    write_keys_file(
        &target_config,
        &target_keys
            .secret_key()
            .to_bech32()
            .context("encode nsec")?,
    )?;

    let store = HashtreeStore::new_with_backend(
        &target_data,
        StorageBackend::Lmdb,
        TEST_STORAGE_MAX_SIZE_BYTES,
    )
    .context("create target PoolStore")?;
    let hash_hex = store.put_blob(b"strict pool audit production start")?;
    store.force_sync()?;
    drop(store);

    let tree_name = "pool-audit-production-start";
    let root_event = EventBuilder::new(Kind::Custom(30064), "")
        .tags(vec![
            Tag::identifier(tree_name),
            Tag::custom(TagKind::custom("l"), vec!["hashtree".to_string()]),
            Tag::custom(TagKind::custom("hash"), vec![hash_hex.clone()]),
        ])
        .sign_with_keys(&source_keys)?;
    let profile_event = EventBuilder::new(
        Kind::Metadata,
        serde_json::json!({ "name": "Pool Audit Source" }).to_string(),
    )
    .sign_with_keys(&source_keys)?;
    publish_to_real_htree_relay(&format!("ws://{source_addr}/ws"), &root_event).await?;
    publish_to_real_htree_relay(&format!("ws://{source_addr}/ws"), &profile_event).await?;

    let before = durable_file_snapshot(&target_data);
    assert!(
        before.iter().any(|entry| entry.path.ends_with("data.mdb")),
        "expected generated PoolStore data files"
    );

    let mut target = RunningDaemon::spawn(
        target_home.path(),
        &target_config,
        &target_data,
        &target_addr,
        true,
    )?;
    target
        .wait_for_health(&target_addr, Duration::from_secs(15))
        .await?;

    let client = reqwest::Client::new();
    let base = format!("http://{target_addr}");
    let status: Value = client
        .get(format!("{base}/api/status"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(status["pool_audit_read_only"], true);
    assert_eq!(status["capabilities"]["writes"], false);

    let npub = source_keys.public_key().to_bech32()?;
    let resolved: Value = client
        .get(format!(
            "{base}/api/nostr/resolve/{npub}/{tree_name}?refresh=1"
        ))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(resolved["hash"], hash_hex);
    assert_eq!(resolved["event_id"], root_event.id.to_hex());
    assert_eq!(resolved["source"], "nostr-relay");

    let profile_url = format!(
        "{base}/api/nostr/profile/{}",
        source_keys.public_key().to_hex()
    );
    let profile: Value = client
        .get(&profile_url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(profile["event_id"], profile_event.id.to_hex());
    assert_eq!(profile["profile"]["name"], "Pool Audit Source");

    // The first profile query flowed through the production router and was
    // ingested into the target's maintenance relay projection. It must remain
    // queryable after the real upstream daemon disappears, without creating a
    // durable target event index.
    source.stop();
    let cached_profile: Value = client
        .get(&profile_url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(cached_profile["event_id"], profile_event.id.to_hex());
    assert_eq!(cached_profile["profile"]["name"], "Pool Audit Source");

    let blob = client
        .get(format!("{base}/{hash_hex}"))
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    assert_eq!(blob.as_ref(), b"strict pool audit production start");

    assert_audit_rejection(
        client
            .post(format!("{base}/api/nostr/events"))
            .json(&root_event)
            .send()
            .await?,
    )
    .await?;
    assert_audit_rejection(
        client
            .put(format!("{base}/upload"))
            .body(b"blocked audit upload".to_vec())
            .send()
            .await?,
    )
    .await?;
    assert_audit_rejection(client.get(format!("{base}/ws")).send().await?).await?;

    target.stop();
    let after = durable_file_snapshot(&target_data);
    assert_eq!(
        after, before,
        "strict audit-serving through real htree start mutated durable target data"
    );
    Ok(())
}
