mod common;

use anyhow::{Context, Result};
use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::Response,
    routing::{get, put},
    Router,
};
use futures::executor::block_on as sync_block_on;
use hashtree_blossom::BlossomClient;
use hashtree_cli::{
    HashtreeStore, NostrKeys, NostrResolverConfig, NostrRootResolver, NostrToBech32, RootResolver,
};
use hashtree_config::StorageBackend;
use hashtree_core::{Cid, HashTree, HashTreeConfig, Link};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path as FsPath, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::sync::oneshot;

const TEST_STORAGE_MAX_SIZE_BYTES: u64 = 10 * 1024 * 1024 * 1024;

#[derive(Clone)]
struct BlossomState {
    blobs: Arc<Mutex<HashMap<String, Vec<u8>>>>,
}

struct TestBlossom {
    base_url: String,
    shutdown: Option<oneshot::Sender<()>>,
}

impl TestBlossom {
    fn new() -> Self {
        let state = BlossomState {
            blobs: Arc::new(Mutex::new(HashMap::new())),
        };
        let std_listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind blossom listener");
        let port = std_listener.local_addr().expect("blossom addr").port();
        std_listener
            .set_nonblocking(true)
            .expect("blossom listener nonblocking");
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build blossom runtime");
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(std_listener)
                    .expect("tokio blossom listener");
                let app = Router::new()
                    .route("/upload", put(put_blob))
                    .route("/:name", get(get_blob).head(head_blob))
                    .with_state(state);
                let server = axum::serve(listener, app).with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                });
                let _ = server.await;
            });
        });

        std::thread::sleep(Duration::from_millis(100));

        Self {
            base_url: format!("http://127.0.0.1:{port}"),
            shutdown: Some(shutdown_tx),
        }
    }

    fn base_url(&self) -> String {
        self.base_url.clone()
    }
}

impl Drop for TestBlossom {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

async fn put_blob(
    State(state): State<BlossomState>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let Some(hash) = headers
        .get("x-sha-256")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_lowercase())
    else {
        return StatusCode::BAD_REQUEST;
    };

    state
        .blobs
        .lock()
        .expect("blossom blobs")
        .insert(hash, body.to_vec());
    StatusCode::CREATED
}

async fn head_blob(
    State(state): State<BlossomState>,
    Path(name): Path<String>,
) -> Result<Response, StatusCode> {
    let hash = blob_hash_from_path(&name)?;
    let Some(data) = state
        .blobs
        .lock()
        .expect("blossom blobs")
        .get(&hash)
        .cloned()
    else {
        return Err(StatusCode::NOT_FOUND);
    };

    let mut response = Response::new(axum::body::Body::empty());
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        "content-type",
        HeaderValue::from_static("application/octet-stream"),
    );
    response.headers_mut().insert(
        "content-length",
        HeaderValue::from_str(&data.len().to_string()).expect("content-length header"),
    );
    Ok(response)
}

async fn get_blob(
    State(state): State<BlossomState>,
    Path(name): Path<String>,
) -> Result<Response, StatusCode> {
    let hash = blob_hash_from_path(&name)?;
    let Some(data) = state
        .blobs
        .lock()
        .expect("blossom blobs")
        .get(&hash)
        .cloned()
    else {
        return Err(StatusCode::NOT_FOUND);
    };

    let mut response = Response::new(axum::body::Body::from(data.clone()));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        "content-type",
        HeaderValue::from_static("application/octet-stream"),
    );
    response.headers_mut().insert(
        "content-length",
        HeaderValue::from_str(&data.len().to_string()).expect("content-length header"),
    );
    Ok(response)
}

fn blob_hash_from_path(name: &str) -> Result<String, StatusCode> {
    let Some(hash) = name.strip_suffix(".bin") else {
        return Err(StatusCode::NOT_FOUND);
    };
    Ok(hash.to_lowercase())
}

struct MirrorDaemon {
    _home_dir: TempDir,
    data_path: PathBuf,
    config_dir: PathBuf,
    pid_file: PathBuf,
    htree_bin: String,
    pid: i32,
}

impl MirrorDaemon {
    fn new(
        htree_bin: &str,
        port: u16,
        keys_file_contents: &str,
        relay_urls: &[String],
        blossom_read_servers: &[String],
        mirrored_authors: &[String],
    ) -> Result<Self> {
        let home_dir = TempDir::new().context("create daemon temp dir")?;
        let home_path = home_dir.path().to_path_buf();
        let data_path = home_path.join("data");
        fs::create_dir_all(&data_path).context("create daemon data dir")?;

        let config_dir = home_path.join(".hashtree");
        fs::create_dir_all(&config_dir).context("create daemon config dir")?;
        write_test_config(&config_dir, relay_urls, blossom_read_servers)?;
        fs::write(config_dir.join("keys"), keys_file_contents).context("write daemon keys")?;

        let store = HashtreeStore::new_with_backend(
            &data_path,
            StorageBackend::Lmdb,
            TEST_STORAGE_MAX_SIZE_BYTES,
        )
        .context("open mirror daemon store")?;
        for npub in mirrored_authors {
            store
                .add_tracked_author(npub)
                .with_context(|| format!("track mirrored author {npub}"))?;
        }

        let addr = format!("127.0.0.1:{port}");
        let pid_file = home_path.join(format!("htree-{port}.pid"));
        let log_file = home_path.join(format!("htree-{port}.log"));

        let output = Command::new(htree_bin)
            .arg("--data-dir")
            .arg(&data_path)
            .arg("start")
            .arg("--addr")
            .arg(&addr)
            .arg("--daemon")
            .arg("--pid-file")
            .arg(&pid_file)
            .arg("--log-file")
            .arg(&log_file)
            .env("HOME", &home_path)
            .env("HTREE_CONFIG_DIR", &config_dir)
            .env("RUST_LOG", "warn")
            .output()
            .context("start mirror daemon")?;

        if !output.status.success() {
            anyhow::bail!(
                "htree start failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let pid = wait_for_pid_file(&pid_file, Duration::from_secs(5))?;
        wait_for_health(&addr, Duration::from_secs(10))?;

        Ok(Self {
            _home_dir: home_dir,
            data_path,
            config_dir,
            pid_file,
            htree_bin: htree_bin.to_string(),
            pid,
        })
    }
}

impl Drop for MirrorDaemon {
    fn drop(&mut self) {
        let _ = Command::new(&self.htree_bin)
            .arg("stop")
            .arg("--pid-file")
            .arg(&self.pid_file)
            .env("HOME", self._home_dir.path())
            .env("HTREE_CONFIG_DIR", &self.config_dir)
            .output();

        if is_process_running(self.pid) {
            unsafe {
                let _ = libc::kill(self.pid, libc::SIGKILL);
            }
            let _ = fs::remove_file(&self.pid_file);
        }
    }
}

fn write_test_config(
    config_dir: &FsPath,
    relay_urls: &[String],
    blossom_read_servers: &[String],
) -> Result<()> {
    let relays = toml_array(relay_urls);
    let blossom_reads = toml_array(blossom_read_servers);
    let config_content = format!(
        r#"
[server]
enable_auth = false
stun_port = 0
enable_webrtc = false
enable_multicast = false
max_multicast_peers = 0
enable_wifi_aware = false
max_wifi_aware_peers = 0
enable_bluetooth = false
max_bluetooth_peers = 0
public_writes = true

[storage]
backend = "lmdb"
max_size_gb = 10

[nostr]
relays = {relays}

[blossom]
servers = []
read_servers = {blossom_reads}
write_servers = []

[sync]
enabled = true
sync_own = false
sync_followed = false
max_concurrent = 1
webrtc_timeout_ms = 250
blossom_timeout_ms = 2000
"#
    );
    fs::write(config_dir.join("config.toml"), config_content).context("write daemon config")?;
    Ok(())
}

fn toml_array(values: &[String]) -> String {
    if values.is_empty() {
        "[]".to_string()
    } else {
        let quoted: Vec<String> = values.iter().map(|value| format!("\"{value}\"")).collect();
        format!("[{}]", quoted.join(", "))
    }
}

fn wait_for_pid_file(path: &FsPath, timeout: Duration) -> Result<i32> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return read_pid_file(path);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    anyhow::bail!("timed out waiting for pid file {}", path.display());
}

fn read_pid_file(path: &FsPath) -> Result<i32> {
    let pid: i32 = fs::read_to_string(path)
        .with_context(|| format!("read pid file {}", path.display()))?
        .trim()
        .parse()
        .context("parse pid file")?;
    if pid <= 0 {
        anyhow::bail!("pid must be positive");
    }
    Ok(pid)
}

fn is_process_running(pid: i32) -> bool {
    let result = unsafe { libc::kill(pid, 0) };
    if result == 0 {
        return true;
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(code) if code == libc::EPERM => true,
        Some(code) if code == libc::ESRCH => false,
        _ => false,
    }
}

fn wait_for_health(addr: &str, timeout: Duration) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .context("build health client")?;
    let url = format!("http://{addr}/health");
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        if let Ok(response) = client.get(&url).send() {
            if response.status().is_success() {
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    anyhow::bail!("daemon did not become healthy at {addr}");
}

fn wait_for_mirrored_tree(
    data_path: &FsPath,
    published_key: &str,
    root_cid: &Cid,
    file_path: &str,
    expected_contents: &[u8],
    timeout: Duration,
) -> Result<()> {
    let store = HashtreeStore::new_with_backend(
        data_path,
        StorageBackend::Lmdb,
        TEST_STORAGE_MAX_SIZE_BYTES,
    )
    .context("open mirrored store")?;
    let deadline = Instant::now() + timeout;
    let mut last_tree_ref = None;
    let mut last_listing = None;
    let mut last_file_size = None;
    let mut last_pinned = None;

    while Instant::now() < deadline {
        last_tree_ref = store.get_tree_ref(published_key)?;
        last_pinned = Some(store.is_pinned(&root_cid.hash)?);
        if last_tree_ref == Some(root_cid.hash) && last_pinned == Some(true) {
            last_listing = store
                .get_directory_listing_by_cid(root_cid)?
                .map(|listing| {
                    listing
                        .entries
                        .into_iter()
                        .map(|entry| entry.name)
                        .collect::<Vec<_>>()
                });
            if let Some(file_cid) = store.resolve_path(root_cid, file_path)? {
                if let Some(bytes) = store.get_file_by_cid(&file_cid)? {
                    last_file_size = Some(bytes.len());
                    if bytes == expected_contents {
                        return Ok(());
                    }
                }
            }
        }

        std::thread::sleep(Duration::from_millis(200));
    }

    anyhow::bail!(
        "timed out waiting for mirrored tree {} (last tree ref: {:?}, pinned: {:?}, listing: {:?}, last_file_size: {:?})",
        published_key,
        last_tree_ref.map(hex::encode),
        last_pinned,
        last_listing,
        last_file_size,
    );
}

fn child_cid(parent: &Cid, link: &Link) -> Cid {
    let inherits_parent_key = link
        .name
        .as_deref()
        .map(|name| {
            name.starts_with("_chunk_")
                || (name.starts_with('_') && name.chars().count() == 2 && link.link_type.is_tree())
        })
        .unwrap_or(false);

    Cid {
        hash: link.hash,
        key: link.key.or(if inherits_parent_key {
            parent.key
        } else {
            None
        }),
    }
}

fn collect_tree_cids(store: &HashtreeStore, root_cid: &Cid) -> Result<Vec<Cid>> {
    let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());
    let mut queue = vec![root_cid.clone()];
    let mut visited = HashSet::new();
    let mut cids = Vec::new();

    while let Some(cid) = queue.pop() {
        if !visited.insert(cid.hash) {
            continue;
        }

        store
            .get_blob(&cid.hash)?
            .ok_or_else(|| anyhow::anyhow!("missing local blob for {}", cid))?;
        cids.push(cid.clone());

        if let Some(node) = sync_block_on(tree.get_node(&cid))
            .map_err(|error| anyhow::anyhow!("inspect {}: {}", cid, error))?
        {
            for link in node.links {
                queue.push(child_cid(&cid, &link));
            }
        }
    }

    Ok(cids)
}

fn push_tree_to_blossom(
    store: &HashtreeStore,
    root_cid: &Cid,
    keys: &NostrKeys,
    blossom_url: &str,
) -> Result<()> {
    let client = BlossomClient::new_empty(keys.clone()).with_servers(vec![blossom_url.to_string()]);
    let cids = collect_tree_cids(store, root_cid)?;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build blossom upload runtime")?;
    runtime.block_on(async move {
        for cid in cids {
            let data = store
                .get_blob(&cid.hash)?
                .ok_or_else(|| anyhow::anyhow!("read blob {} for blossom upload", cid))?;
            client
                .upload_if_missing(&data)
                .await
                .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        }
        let downloaded = client
            .download(&hex::encode(root_cid.hash))
            .await
            .map_err(|error| anyhow::anyhow!("download mirrored root from blossom: {error}"))?;
        let expected = store
            .get_blob(&root_cid.hash)?
            .ok_or_else(|| anyhow::anyhow!("read uploaded root {} after blossom push", root_cid))?;
        anyhow::ensure!(
            downloaded == expected,
            "blossom returned different bytes for root {}",
            root_cid
        );
        Ok::<(), anyhow::Error>(())
    })?;

    Ok(())
}

fn build_published_tree(
    store: &HashtreeStore,
    private: bool,
    file_path: &str,
    contents: &[u8],
) -> Result<Cid> {
    let dir = TempDir::new().context("create publisher tree dir")?;
    let full_path = dir.path().join(file_path);
    if let Some(parent) = full_path.parent() {
        fs::create_dir_all(parent).context("create publisher tree subdir")?;
    }
    fs::write(&full_path, contents).context("write publisher tree file")?;

    let cid = if private {
        store
            .upload_dir_encrypted_with_options(dir.path(), true)
            .context("upload encrypted published tree")?
    } else {
        store
            .upload_dir_with_options(dir.path(), true)
            .context("upload public published tree")?
    };

    Cid::parse(&cid).context("parse published root cid")
}

fn publish_tree(
    root_key: &str,
    root_cid: &Cid,
    keys: &NostrKeys,
    relay_url: &str,
    private: bool,
) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build publish runtime")?;
    runtime.block_on(async move {
        let resolver = NostrRootResolver::new(NostrResolverConfig {
            relays: vec![relay_url.to_string()],
            resolve_timeout: Duration::from_secs(2),
            secret_key: Some(keys.clone()),
        })
        .await
        .context("create publisher resolver")?;

        let publish_result = if private {
            resolver.publish_private(root_key, root_cid).await
        } else {
            RootResolver::publish(&resolver, root_key, root_cid).await
        };

        match publish_result {
            Ok(true) => {}
            Ok(false) => anyhow::bail!("no relay accepted published event"),
            Err(error) => return Err(anyhow::anyhow!(error.to_string())),
        }

        let _ = resolver.stop().await;
        Ok::<(), anyhow::Error>(())
    })?;

    Ok(())
}

fn run_author_mirror_case(private: bool) -> Result<()> {
    let htree_bin = common::htree_bin();
    let relay = common::test_relay::TestRelay::new();
    let blossom = TestBlossom::new();
    let relay_url = relay.url();
    let blossom_url = blossom.base_url();
    let publisher_keys = NostrKeys::generate();
    let mirror_keys = NostrKeys::generate();
    let publisher_npub =
        NostrToBech32::to_bech32(&publisher_keys.public_key()).context("publisher npub")?;
    let published_key = format!("{publisher_npub}/backup");
    let file_path = if private {
        "vault/secret.txt"
    } else {
        "docs/note.txt"
    };
    let expected = if private {
        b"private mirrored payload\n".to_vec()
    } else {
        b"public mirrored payload\n".to_vec()
    };

    let publisher_dir = TempDir::new().context("create publisher data dir")?;
    let publisher_store = HashtreeStore::new_with_backend(
        publisher_dir.path(),
        StorageBackend::Lmdb,
        TEST_STORAGE_MAX_SIZE_BYTES,
    )
    .context("open publisher store")?;
    let root_cid = build_published_tree(&publisher_store, private, file_path, &expected)?;
    push_tree_to_blossom(&publisher_store, &root_cid, &publisher_keys, &blossom_url)?;
    publish_tree(
        &published_key,
        &root_cid,
        &publisher_keys,
        &relay_url,
        private,
    )?;

    let port_listener =
        std::net::TcpListener::bind("127.0.0.1:0").context("bind daemon port probe")?;
    let daemon_port = port_listener
        .local_addr()
        .context("read daemon probe addr")?
        .port();
    drop(port_listener);

    let mirror_keys_content = if private {
        format!(
            "{} self\n{} publisher\n",
            mirror_keys
                .secret_key()
                .to_bech32()
                .context("mirror nsec")?,
            publisher_keys
                .secret_key()
                .to_bech32()
                .context("publisher backup nsec")?
        )
    } else {
        format!(
            "{} self\n",
            mirror_keys
                .secret_key()
                .to_bech32()
                .context("mirror nsec")?
        )
    };

    let daemon = MirrorDaemon::new(
        &htree_bin,
        daemon_port,
        &mirror_keys_content,
        std::slice::from_ref(&relay_url),
        std::slice::from_ref(&blossom_url),
        std::slice::from_ref(&publisher_npub),
    )?;

    wait_for_mirrored_tree(
        &daemon.data_path,
        &published_key,
        &root_cid,
        file_path,
        &expected,
        Duration::from_secs(20),
    )?;

    Ok(())
}

#[test]
fn mirrors_public_author_tree_end_to_end() -> Result<()> {
    run_author_mirror_case(false)
}

#[test]
fn mirrors_private_author_tree_when_author_key_is_available() -> Result<()> {
    run_author_mirror_case(true)
}
