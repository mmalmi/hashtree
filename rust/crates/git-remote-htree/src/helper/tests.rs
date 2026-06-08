use super::*;
use axum::{
    body::{Body, Bytes},
    extract::{Path as AxumPath, State},
    http::{header, HeaderMap, Response, StatusCode},
    routing::put,
    Router,
};
use hashtree_core::{
    collect_hashes, DirEntry, HashTree, HashTreeConfig, Link, LinkType, MemoryStore, Store,
};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::thread::JoinHandle;
use tempfile::TempDir;
use tokio::sync::oneshot;

const TEST_PUBKEY: &str = "4523be58d395b1b196a9b8c82b038b6895cb02b683d0c253a955068dba1facd0";
static ENV_LOCK: Mutex<()> = Mutex::new(());

#[derive(Default)]
struct CountingBlossomState {
    blobs: HashMap<String, Vec<u8>>,
    get_requests: usize,
    head_requests: usize,
    upload_requests: usize,
    fail_uploads: bool,
}

struct CountingBlossomServer {
    state: Arc<Mutex<CountingBlossomState>>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    server_thread: Option<JoinHandle<()>>,
    base_url: String,
}

impl CountingBlossomServer {
    fn new() -> Self {
        Self::with_upload_failures(false)
    }

    fn failing_uploads() -> Self {
        Self::with_upload_failures(true)
    }

    fn with_upload_failures(fail_uploads: bool) -> Self {
        let state = Arc::new(Mutex::new(CountingBlossomState {
            fail_uploads,
            ..CountingBlossomState::default()
        }));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fake blossom");
        let port = listener.local_addr().expect("fake blossom addr").port();
        listener
            .set_nonblocking(true)
            .expect("set fake blossom nonblocking");
        let state_clone = Arc::clone(&state);
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        let server_thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("build fake blossom runtime");

            rt.block_on(async move {
                let app = Router::new()
                    .route("/upload", put(upload_blob))
                    .route("/:id", axum::routing::get(get_blob).head(head_blob))
                    .with_state(state_clone);

                let listener = tokio::net::TcpListener::from_std(listener)
                    .expect("tokio fake blossom listener");

                axum::serve(listener, app)
                    .with_graceful_shutdown(async {
                        let _ = shutdown_rx.await;
                    })
                    .await
                    .expect("fake blossom serve");
            });
        });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return Self {
                    state,
                    shutdown_tx: Some(shutdown_tx),
                    server_thread: Some(server_thread),
                    base_url: format!("http://127.0.0.1:{}", port),
                };
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        panic!("fake blossom did not start");
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn get_request_count(&self) -> usize {
        self.state.lock().expect("state lock").get_requests
    }

    fn get_head_request_count(&self) -> usize {
        self.state.lock().expect("state lock").head_requests
    }

    fn get_upload_request_count(&self) -> usize {
        self.state.lock().expect("state lock").upload_requests
    }

    fn has_blob(&self, hash: &[u8; 32]) -> bool {
        let hash = hex::encode(hash);
        self.state
            .lock()
            .expect("state lock")
            .blobs
            .contains_key(&hash)
    }

    fn insert_blob(&self, data: Vec<u8>) -> String {
        let mut hasher = Sha256::new();
        hasher.update(&data);
        let hash = hex::encode(hasher.finalize());
        self.state
            .lock()
            .expect("state lock")
            .blobs
            .insert(hash.clone(), data);
        hash
    }
}

impl Drop for CountingBlossomServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.server_thread.take() {
            let _ = handle.join();
        }
    }
}

fn parse_hash_from_path(id: &str) -> Option<String> {
    let hash = id.strip_suffix(".bin").unwrap_or(id);
    if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(hash.to_ascii_lowercase())
    } else {
        None
    }
}

async fn upload_blob(
    State(state): State<Arc<Mutex<CountingBlossomState>>>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    {
        let mut state = state.lock().expect("state lock");
        state.upload_requests += 1;
        if state.fail_uploads {
            return StatusCode::FORBIDDEN;
        }
    }

    let mut hasher = Sha256::new();
    hasher.update(&body);
    let computed_hash = hex::encode(hasher.finalize());

    if let Some(expected_hash) = headers
        .get("x-sha-256")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_ascii_lowercase())
    {
        if expected_hash != computed_hash {
            return StatusCode::BAD_REQUEST;
        }
    }

    let mut state = state.lock().expect("state lock");
    if state.blobs.insert(computed_hash, body.to_vec()).is_some() {
        StatusCode::CONFLICT
    } else {
        StatusCode::OK
    }
}

async fn head_blob(
    State(state): State<Arc<Mutex<CountingBlossomState>>>,
    AxumPath(id): AxumPath<String>,
) -> Response<Body> {
    let Some(hash) = parse_hash_from_path(&id) else {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::empty())
            .unwrap();
    };

    let mut state = state.lock().expect("state lock");
    state.head_requests += 1;
    if let Some(data) = state.blobs.get(&hash) {
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .header(header::CONTENT_LENGTH, data.len().to_string())
            .body(Body::empty())
            .unwrap();
    }

    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Body::empty())
        .unwrap()
}

async fn get_blob(
    State(state): State<Arc<Mutex<CountingBlossomState>>>,
    AxumPath(id): AxumPath<String>,
) -> Response<Body> {
    let Some(hash) = parse_hash_from_path(&id) else {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::empty())
            .unwrap();
    };

    let data = {
        let mut state = state.lock().expect("state lock");
        state.get_requests += 1;
        state.blobs.get(&hash).cloned()
    };

    match data {
        Some(bytes) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .header(header::CONTENT_LENGTH, bytes.len().to_string())
            .body(Body::from(bytes))
            .unwrap(),
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::empty())
            .unwrap(),
    }
}

struct HomeGuard {
    previous: Option<String>,
}

impl HomeGuard {
    fn set(path: &std::path::Path) -> Self {
        let previous = std::env::var("HOME").ok();
        std::env::set_var("HOME", path);
        Self { previous }
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.as_deref() {
            std::env::set_var("HOME", previous);
        } else {
            std::env::remove_var("HOME");
        }
    }
}

struct CwdGuard {
    previous: std::path::PathBuf,
}

impl CwdGuard {
    fn set(path: &std::path::Path) -> Self {
        let previous = std::env::current_dir().expect("current dir");
        std::env::set_current_dir(path).expect("set current dir");
        Self { previous }
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.previous).expect("restore current dir");
    }
}

struct EnvGuard {
    key: &'static str,
    previous: Option<String>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var(key).ok();
        std::env::set_var(key, value);
        Self { key, previous }
    }

    fn clear(key: &'static str) -> Self {
        let previous = std::env::var(key).ok();
        std::env::remove_var(key);
        Self { key, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.as_deref() {
            std::env::set_var(self.key, previous);
        } else {
            std::env::remove_var(self.key);
        }
    }
}

fn git(repo: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap_or_else(|err| panic!("git {:?} failed to start: {}", args, err))
}

fn create_repo_with_diverged_master_and_dev() -> (TempDir, TempDir, String, String, String) {
    let home = TempDir::new().expect("temp home");
    let _home_guard = HomeGuard::set(home.path());

    let repo = TempDir::new().expect("temp repo");
    assert!(git(repo.path(), &["init", "-b", "master"]).status.success());
    assert!(
        git(repo.path(), &["config", "user.email", "test@example.com"])
            .status
            .success()
    );
    assert!(git(repo.path(), &["config", "user.name", "Test User"])
        .status
        .success());

    std::fs::write(repo.path().join("README.md"), "initial\n").unwrap();
    assert!(git(repo.path(), &["add", "README.md"]).status.success());
    assert!(git(repo.path(), &["commit", "-m", "Initial commit"])
        .status
        .success());
    let base_sha = String::from_utf8_lossy(&git(repo.path(), &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string();

    assert!(git(repo.path(), &["checkout", "-b", "dev"])
        .status
        .success());
    std::fs::write(repo.path().join("dev-only.txt"), "dev-only\n").unwrap();
    assert!(git(repo.path(), &["add", "dev-only.txt"]).status.success());
    assert!(git(repo.path(), &["commit", "-m", "Dev commit"])
        .status
        .success());
    let dev_sha = String::from_utf8_lossy(&git(repo.path(), &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string();

    assert!(git(repo.path(), &["checkout", "master"]).status.success());
    std::fs::write(repo.path().join("master-only.txt"), "master-only\n").unwrap();
    assert!(git(repo.path(), &["add", "master-only.txt"])
        .status
        .success());
    assert!(git(repo.path(), &["commit", "-m", "Master commit"])
        .status
        .success());
    let master_sha = String::from_utf8_lossy(&git(repo.path(), &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string();

    (home, repo, base_sha, master_sha, dev_sha)
}

fn create_repo_with_large_base_and_small_increment() -> (TempDir, TempDir, String, String) {
    let home = TempDir::new().expect("temp home");
    let _home_guard = HomeGuard::set(home.path());

    let repo = TempDir::new().expect("temp repo");
    assert!(git(repo.path(), &["init", "-b", "master"]).status.success());
    assert!(
        git(repo.path(), &["config", "user.email", "test@example.com"])
            .status
            .success()
    );
    assert!(git(repo.path(), &["config", "user.name", "Test User"])
        .status
        .success());

    for index in 0..20 {
        std::fs::write(
            repo.path().join(format!("base-{index:02}.txt")),
            format!("base file {index}\n"),
        )
        .unwrap();
    }
    assert!(git(repo.path(), &["add", "."]).status.success());
    assert!(git(repo.path(), &["commit", "-m", "Large base"])
        .status
        .success());
    let base_sha = String::from_utf8_lossy(&git(repo.path(), &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string();

    std::fs::write(repo.path().join("increment.txt"), "small increment\n").unwrap();
    assert!(git(repo.path(), &["add", "increment.txt"]).status.success());
    assert!(git(repo.path(), &["commit", "-m", "Small increment"])
        .status
        .success());
    let master_sha = String::from_utf8_lossy(&git(repo.path(), &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string();

    (home, repo, base_sha, master_sha)
}

fn create_repo_with_linear_history(commit_count: usize) -> (TempDir, TempDir, Vec<String>) {
    let home = TempDir::new().expect("temp home");
    let _home_guard = HomeGuard::set(home.path());

    let repo = TempDir::new().expect("temp repo");
    assert!(git(repo.path(), &["init", "-b", "master"]).status.success());
    assert!(
        git(repo.path(), &["config", "user.email", "test@example.com"])
            .status
            .success()
    );
    assert!(git(repo.path(), &["config", "user.name", "Test User"])
        .status
        .success());

    let mut shas = Vec::new();
    for index in 0..commit_count {
        std::fs::write(
            repo.path().join(format!("file-{index:02}.txt")),
            format!("file {index}\n"),
        )
        .unwrap();
        assert!(git(repo.path(), &["add", &format!("file-{index:02}.txt")])
            .status
            .success());
        assert!(
            git(repo.path(), &["commit", "-m", &format!("Commit {index}")])
                .status
                .success()
        );
        shas.push(
            String::from_utf8_lossy(&git(repo.path(), &["rev-parse", "HEAD"]).stdout)
                .trim()
                .to_string(),
        );
    }

    (home, repo, shas)
}

fn create_test_helper() -> Option<RemoteHelper> {
    let config = Config::default();
    RemoteHelper::new(TEST_PUBKEY, "test-repo", None, None, false, config).ok()
}

fn create_test_helper_with_config(config: Config) -> Option<RemoteHelper> {
    RemoteHelper::new(TEST_PUBKEY, "test-repo", None, None, false, config).ok()
}

#[test]
fn test_cached_fetch_tree_reuses_open_git_storage_store() {
    let _env_lock = ENV_LOCK.lock().expect("env lock");
    let home = TempDir::new().expect("temp home");
    let _home_guard = HomeGuard::set(home.path());
    let helper = create_test_helper().expect("helper");

    let storage_store = helper.storage.store().clone();
    let before = Arc::strong_count(&storage_store);
    let (_tree, _eviction_store) = helper.build_cached_fetch_tree().expect("cached fetch tree");

    assert_eq!(
        Arc::strong_count(&storage_store),
        before + 2,
        "cached fetch tree should reuse the already-open GitStorage blob store instead of reopening the shared LMDB environment",
    );
}

#[test]
fn test_collect_git_object_locations_errors_on_bad_objects_tree_key() {
    let _env_lock = ENV_LOCK.lock().expect("env lock");
    let home = TempDir::new().expect("temp home");
    let _home_guard = HomeGuard::set(home.path());
    let fake_blossom = CountingBlossomServer::new();

    let mut config = Config::default();
    config.blossom.read_servers = vec![fake_blossom.base_url().to_string()];
    config.blossom.write_servers = config.blossom.read_servers.clone();
    let helper = create_test_helper_with_config(config).expect("helper");

    let rt = tokio::runtime::Runtime::new().unwrap();
    let store = Arc::new(MemoryStore::new());
    let tree = HashTree::new(HashTreeConfig::new(store.clone()));
    let root_cid = rt.block_on(async {
        let objects_cid = tree.put_directory(vec![]).await.expect("objects directory");
        let refs_cid = tree.put_directory(vec![]).await.expect("refs directory");

        let mut bad_objects_key = objects_cid.key.expect("objects tree key");
        bad_objects_key[0] ^= 0x01;
        let git_cid = tree
            .put_directory(vec![
                DirEntry::from_cid("objects", &objects_cid)
                    .with_key(bad_objects_key)
                    .with_link_type(LinkType::Dir),
                DirEntry::from_cid("refs", &refs_cid).with_link_type(LinkType::Dir),
            ])
            .await
            .expect(".git directory");
        let root_cid = tree
            .put_directory(vec![
                DirEntry::from_cid(".git", &git_cid).with_link_type(LinkType::Dir)
            ])
            .await
            .expect("root directory");

        for cid in [&objects_cid, &refs_cid, &git_cid, &root_cid] {
            let blob = store
                .get(&cid.hash)
                .await
                .expect("read test blob")
                .expect("test blob exists");
            fake_blossom.insert_blob(blob);
        }

        root_cid
    });

    let root_hash = hex::encode(root_cid.hash);
    let err = match rt
        .block_on(helper.collect_git_object_locations_async(&root_hash, root_cid.key.as_ref()))
    {
        Ok(_) => panic!("bad .git/objects key should fail the object tree load"),
        Err(err) => err,
    };
    assert!(
        err.to_string().contains("Failed to resolve .git/objects")
            || err.to_string().contains("resolve .git/objects/info/packs"),
        "unexpected error: {err}"
    );
}

fn write_test_config(home: &std::path::Path, blossom_url: &str, force_upload: bool) {
    write_test_config_for_servers(home, &[blossom_url], force_upload);
}

fn write_test_config_for_servers(
    home: &std::path::Path,
    blossom_urls: &[&str],
    force_upload: bool,
) {
    let config_dir = home.join(".hashtree");
    std::fs::create_dir_all(&config_dir).expect("create config dir");
    let servers = blossom_urls
        .iter()
        .map(|url| format!("\"{url}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let config = format!(
        r#"
[server]
enable_auth = false
stun_port = 0

[nostr]
relays = []
social_graph_crawl_depth = 0

[blossom]
read_servers = [{servers}]
write_servers = [{servers}]
force_upload = {force_upload}
"#
    );
    std::fs::write(config_dir.join("config.toml"), config).expect("write config");
}

#[test]
fn test_build_repo_viewer_url_uses_git_host() {
    assert_eq!(
        build_repo_viewer_url("npub1example/repo", None),
        "https://git.iris.to/#/npub1example/repo"
    );
}

#[test]
fn test_build_repo_viewer_url_preserves_link_key() {
    let url_secret = [0xab; 32];
    assert_eq!(
        build_repo_viewer_url("npub1example/repo", Some(&url_secret)),
        format!(
            "https://git.iris.to/#/npub1example/repo?k={}",
            "ab".repeat(32)
        )
    );
}

#[test]
fn test_capabilities() {
    let Some(helper) = create_test_helper() else {
        return;
    };

    let caps = helper.capabilities();
    assert!(caps.contains(&"fetch".to_string()));
    assert!(caps.contains(&"push".to_string()));
    assert!(caps.contains(&"option".to_string()));
    assert_eq!(caps.last(), Some(&String::new()));
}

#[test]
fn test_handle_capabilities_command() {
    let Some(mut helper) = create_test_helper() else {
        return;
    };

    let result = helper.handle_command("capabilities").unwrap();
    assert!(result.is_some());
    let caps = result.unwrap();
    assert!(caps.contains(&"fetch".to_string()));
    assert!(caps.contains(&"push".to_string()));
}

#[test]
fn test_handle_list_command() {
    let Some(mut helper) = create_test_helper() else {
        return;
    };

    match helper.handle_command("list") {
        Ok(Some(lines)) => {
            assert_eq!(lines.last(), Some(&String::new()));
        }
        Ok(None) => panic!("list should return output lines"),
        Err(err) => {
            assert!(
                err.to_string().contains("not found"),
                "unexpected list error: {}",
                err
            );
        }
    }
}

#[test]
fn test_handle_list_for_push_command() {
    let mut config = Config::default();
    config.blossom.force_upload = true;
    let Some(mut helper) = create_test_helper_with_config(config) else {
        return;
    };

    let result = helper.handle_command("list for-push").unwrap();
    assert!(result.is_some());
}

#[test]
fn test_handle_option_command() {
    let Some(mut helper) = create_test_helper() else {
        return;
    };

    let result = helper.handle_command("option verbosity 1").unwrap();
    assert!(result.is_some());
    let lines = result.unwrap();
    assert!(lines.contains(&"ok".to_string()));
}

#[test]
fn test_handle_unknown_command() {
    let Some(mut helper) = create_test_helper() else {
        return;
    };

    let result = helper.handle_command("unknown-command").unwrap();
    assert!(result.is_none());
}

#[test]
fn test_handle_empty_line_exits() {
    let Some(mut helper) = create_test_helper() else {
        return;
    };

    assert!(!helper.should_exit());
    let _ = helper.handle_command("").unwrap();
    assert!(helper.should_exit());
}

#[test]
fn test_queue_fetch() {
    let Some(mut helper) = create_test_helper() else {
        return;
    };

    let result = helper
        .handle_command("fetch abc123def456 refs/heads/main")
        .unwrap();
    assert!(result.is_none());

    assert_eq!(helper.fetch_specs.len(), 1);
    assert_eq!(helper.fetch_specs[0].sha, "abc123def456");
    assert_eq!(helper.fetch_specs[0].name, "refs/heads/main");
}

#[test]
fn test_queue_multiple_fetches() {
    let Some(mut helper) = create_test_helper() else {
        return;
    };

    helper
        .handle_command("fetch abc123 refs/heads/main")
        .unwrap();
    helper
        .handle_command("fetch def456 refs/heads/feature")
        .unwrap();

    assert_eq!(helper.fetch_specs.len(), 2);
}

#[test]
fn test_queue_fetch_invalid() {
    let Some(mut helper) = create_test_helper() else {
        return;
    };

    let result = helper.handle_command("fetch abc123");
    assert!(result.is_err());
}

#[test]
fn test_queue_push() {
    let Some(mut helper) = create_test_helper() else {
        return;
    };

    let result = helper
        .handle_command("push refs/heads/main:refs/heads/main")
        .unwrap();
    assert!(result.is_none());

    assert_eq!(helper.push_specs.len(), 1);
    assert_eq!(helper.push_specs[0].src, "refs/heads/main");
    assert_eq!(helper.push_specs[0].dst, "refs/heads/main");
    assert!(!helper.push_specs[0].force);
}

#[test]
fn test_queue_force_push() {
    let Some(mut helper) = create_test_helper() else {
        return;
    };

    helper
        .handle_command("push +refs/heads/main:refs/heads/main")
        .unwrap();

    assert_eq!(helper.push_specs.len(), 1);
    assert!(helper.push_specs[0].force);
}

#[test]
fn test_queue_delete_push() {
    let Some(mut helper) = create_test_helper() else {
        return;
    };

    helper
        .handle_command("push :refs/heads/old-branch")
        .unwrap();

    assert_eq!(helper.push_specs.len(), 1);
    assert_eq!(helper.push_specs[0].src, "");
    assert_eq!(helper.push_specs[0].dst, "refs/heads/old-branch");
}

#[test]
fn test_queue_push_invalid() {
    let Some(mut helper) = create_test_helper() else {
        return;
    };

    let result = helper.handle_command("push refs/heads/main");
    assert!(result.is_err());
}

#[test]
fn test_push_spec_parsing() {
    let Some(mut helper) = create_test_helper() else {
        return;
    };

    helper.queue_push("src:dst").unwrap();
    assert_eq!(helper.push_specs[0].src, "src");
    assert_eq!(helper.push_specs[0].dst, "dst");
    assert!(!helper.push_specs[0].force);

    helper.push_specs.clear();

    helper.queue_push("+src:dst").unwrap();
    assert!(helper.push_specs[0].force);
    assert_eq!(helper.push_specs[0].src, "src");

    helper.push_specs.clear();

    helper.queue_push(":dst").unwrap();
    assert_eq!(helper.push_specs[0].src, "");
    assert_eq!(helper.push_specs[0].dst, "dst");
}

#[test]
fn test_fetch_spec_parsing() {
    let Some(mut helper) = create_test_helper() else {
        return;
    };

    helper
        .queue_fetch("abc123def456789 refs/heads/main")
        .unwrap();

    assert_eq!(helper.fetch_specs[0].sha, "abc123def456789");
    assert_eq!(helper.fetch_specs[0].name, "refs/heads/main");
}

#[test]
fn test_fetch_spec_with_tag() {
    let Some(mut helper) = create_test_helper() else {
        return;
    };

    helper.queue_fetch("abc123 refs/tags/v1.0.0").unwrap();
    assert_eq!(helper.fetch_specs[0].name, "refs/tags/v1.0.0");
}

#[test]
fn test_should_exit_initially_false() {
    let Some(helper) = create_test_helper() else {
        return;
    };

    assert!(!helper.should_exit());
}

#[test]
fn test_get_hashtree_data_dir() {
    let dir = get_hashtree_data_dir();
    assert!(dir.ends_with("data"));
    assert!(dir.to_string_lossy().contains(".hashtree"));
}

#[test]
fn test_command_parsing_with_spaces() {
    let Some(mut helper) = create_test_helper() else {
        return;
    };

    let result = helper.handle_command("option verbosity 1").unwrap();
    assert!(result.is_some());
}

#[test]
fn test_list_clears_remote_refs() {
    let Some(mut helper) = create_test_helper() else {
        return;
    };

    helper
        .remote_refs
        .insert("refs/heads/old".to_string(), "abc".to_string());

    let _ = helper.handle_command("list");
    assert!(helper.remote_refs.is_empty());
}

#[test]
fn test_classify_merge_base_result_code_zero_is_ancestor() {
    let result = RemoteHelper::classify_merge_base_result(Some(0), b"");
    assert_eq!(result, AncestorCheck::Ancestor);
}

#[test]
fn test_classify_merge_base_result_code_one_is_not_ancestor() {
    let result = RemoteHelper::classify_merge_base_result(Some(1), b"");
    assert_eq!(result, AncestorCheck::NotAncestor);
}

#[test]
fn test_classify_merge_base_result_other_code_is_error() {
    let result = RemoteHelper::classify_merge_base_result(Some(2), b"fatal: bad object");
    match result {
        AncestorCheck::Unknown(reason) => {
            assert!(reason.contains("exit code 2"));
            assert!(reason.contains("fatal: bad object"));
        }
        _ => panic!("Expected Unknown result"),
    }
}

#[test]
fn test_classify_merge_base_result_missing_exit_code_is_error() {
    let result = RemoteHelper::classify_merge_base_result(None, b"terminated by signal");
    match result {
        AncestorCheck::Unknown(reason) => {
            assert!(reason.contains("no exit code"));
            assert!(reason.contains("terminated by signal"));
        }
        _ => panic!("Expected Unknown result"),
    }
}

#[test]
fn test_queue_hash_if_new_counts_unique_hashes_when_queued() {
    let mut queue = Vec::new();
    let mut queued = HashSet::new();
    let hash_a = [0x11; 32];
    let hash_b = [0x22; 32];

    assert!(queue_hash_if_new(&mut queue, &mut queued, hash_a, None));
    assert!(!queue_hash_if_new(
        &mut queue,
        &mut queued,
        hash_a,
        Some([0x33; 32])
    ));
    assert!(queue_hash_if_new(
        &mut queue,
        &mut queued,
        hash_b,
        Some([0x44; 32])
    ));

    assert_eq!(queue.len(), 2);
    assert_eq!(queued.len(), 2);
    assert_eq!(queue[0], (hash_a, None));
    assert_eq!(queue[1], (hash_b, Some([0x44; 32])));
}

#[test]
fn test_list_objects_for_shas_excludes_shared_history() {
    let _env_lock = ENV_LOCK.lock().expect("env lock");
    let (home, repo, base_sha, master_sha, dev_sha) = create_repo_with_diverged_master_and_dev();
    let _home_guard = HomeGuard::set(home.path());
    let _cwd_guard = CwdGuard::set(repo.path());

    let helper = create_test_helper().expect("helper");
    let full = helper
        .list_objects_for_shas(std::slice::from_ref(&dev_sha), &[])
        .expect("list full objects");
    let exclusive = helper
        .list_objects_for_shas(
            std::slice::from_ref(&dev_sha),
            std::slice::from_ref(&master_sha),
        )
        .expect("list exclusive objects");

    assert!(full.contains(&base_sha));
    assert!(full.contains(&dev_sha));
    assert!(exclusive.contains(&dev_sha));
    assert!(
        !exclusive.contains(&base_sha),
        "shared base history should be excluded"
    );
    assert!(
        exclusive.len() < full.len(),
        "excluding pushed history should reduce preserved-object count"
    );
}

#[test]
fn test_import_preserved_remote_objects_from_local_git_uses_exclusive_history() {
    let _env_lock = ENV_LOCK.lock().expect("env lock");
    let (home, repo, _base_sha, master_sha, dev_sha) = create_repo_with_diverged_master_and_dev();
    let _home_guard = HomeGuard::set(home.path());
    let _cwd_guard = CwdGuard::set(repo.path());

    let mut helper = create_test_helper().expect("helper");
    helper.push_specs.push(PushSpec {
        src: "master".to_string(),
        dst: "refs/heads/master".to_string(),
        force: false,
    });

    let exclusive = helper
        .list_objects_for_shas(
            std::slice::from_ref(&dev_sha),
            std::slice::from_ref(&master_sha),
        )
        .expect("list exclusive objects");

    let imported = helper
        .import_preserved_remote_objects_from_local_git(&[(
            "refs/heads/dev".to_string(),
            dev_sha.clone(),
        )])
        .expect("import preserved objects");

    assert!(imported, "local git should satisfy preserved ref import");
    assert_eq!(
        helper.storage.object_count().expect("object count"),
        exclusive.len()
    );
}

#[test]
fn test_push_objects_skips_exact_remote_tip_without_local_tree_rebuild() {
    let _env_lock = ENV_LOCK.lock().expect("env lock");
    let (home, repo, _base_sha, master_sha, _dev_sha) = create_repo_with_diverged_master_and_dev();
    let _home_guard = HomeGuard::set(home.path());
    let _cwd_guard = CwdGuard::set(repo.path());

    let mut helper = create_test_helper().expect("helper");
    helper
        .storage
        .import_ref("refs/tags/v0.1.0", &master_sha)
        .expect("import preserved tag ref");

    helper
        .push_objects(&master_sha, "refs/heads/master", false, Some(&master_sha))
        .expect("exact no-op push should succeed");

    assert_eq!(
        helper.storage.object_count().expect("object count"),
        0,
        "no-op push should not import local git objects just to rebuild an unchanged tree"
    );
    assert!(
        !helper
            .storage
            .has_ref("refs/heads/master")
            .expect("branch ref presence"),
        "no-op push should leave the in-memory tree untouched"
    );
}

#[test]
fn test_checkpoint_push_import_selection_keeps_current_tree_but_skips_old_history() {
    let _env_lock = ENV_LOCK.lock().expect("env lock");
    let (home, repo, base_sha, master_sha, _dev_sha) = create_repo_with_diverged_master_and_dev();
    let _home_guard = HomeGuard::set(home.path());
    let _cwd_guard = CwdGuard::set(repo.path());

    let helper = create_test_helper().expect("helper");
    let full = helper
        .list_objects_for_shas(std::slice::from_ref(&master_sha), &[])
        .expect("list full objects");
    let checkpoint_covered: HashSet<String> = full.iter().cloned().collect();
    let current_tree =
        RemoteHelper::current_tree_object_ids(&master_sha).expect("current tree object ids");

    let selected = helper
        .select_objects_to_import_for_push(&master_sha, &full, &checkpoint_covered, false)
        .expect("select import objects");
    let selected: HashSet<String> = selected.into_iter().collect();

    assert!(
        current_tree.iter().all(|oid| selected.contains(oid)),
        "current checkout tree objects are needed to build the hashtree view"
    );
    assert!(
        !selected.contains(&base_sha),
        "old history covered by the checkpoint pack should not be imported as loose helper state"
    );
    assert!(
        selected.len() < full.len(),
        "checkpoint import should be smaller than the full history"
    );
}

#[test]
fn test_new_tag_push_uses_existing_remote_branch_as_delta_base() {
    let _env_lock = ENV_LOCK.lock().expect("env lock");
    let (home, repo, _base_sha, master_sha, _dev_sha) = create_repo_with_diverged_master_and_dev();
    let _home_guard = HomeGuard::set(home.path());
    let _cwd_guard = CwdGuard::set(repo.path());

    assert!(git(repo.path(), &["tag", "-a", "v1.0.0", "-m", "release"])
        .status
        .success());
    let tag_sha =
        String::from_utf8_lossy(&git(repo.path(), &["rev-parse", "refs/tags/v1.0.0"]).stdout)
            .trim()
            .to_string();

    let mut helper = create_test_helper().expect("helper");
    helper
        .remote_refs
        .insert("refs/heads/master".to_string(), master_sha.clone());

    let delta_base = helper
        .delta_base_for_push(&tag_sha, false, None)
        .expect("new tag should reuse existing remote branch as delta base");
    assert_eq!(delta_base, master_sha);

    let objects = helper
        .list_objects_for_shas(
            std::slice::from_ref(&tag_sha),
            std::slice::from_ref(&delta_base),
        )
        .expect("list tag delta objects");
    assert_eq!(
        objects,
        vec![tag_sha],
        "annotated tag push should not relist the whole repository when its target is already remote"
    );
}

#[test]
fn test_pack_backed_delta_import_keeps_current_tree_objects() {
    let _env_lock = ENV_LOCK.lock().expect("env lock");
    let (home, repo, base_sha, master_sha) = create_repo_with_large_base_and_small_increment();
    let _home_guard = HomeGuard::set(home.path());
    let _cwd_guard = CwdGuard::set(repo.path());

    let helper = create_test_helper().expect("helper");
    let delta = helper
        .list_objects_for_shas(
            std::slice::from_ref(&master_sha),
            std::slice::from_ref(&base_sha),
        )
        .expect("list delta objects");
    let current_tree =
        RemoteHelper::current_tree_object_ids(&master_sha).expect("current tree object ids");
    let current_tree_trees =
        RemoteHelper::current_tree_tree_object_ids(&master_sha).expect("current tree tree ids");

    let selected_without_pack = helper
        .select_objects_to_import_for_push(&master_sha, &delta, &HashSet::new(), false)
        .expect("select delta import objects");
    let selected_without_pack: HashSet<String> = selected_without_pack.into_iter().collect();
    assert!(
        !current_tree
            .iter()
            .all(|oid| selected_without_pack.contains(oid)),
        "fixture should have current tree objects outside the commit delta"
    );

    let selected_with_pack = helper
        .select_objects_to_import_for_push(&master_sha, &delta, &HashSet::new(), true)
        .expect("select pack-backed import objects");
    let selected_with_pack: HashSet<String> = selected_with_pack.into_iter().collect();
    assert!(
        current_tree_trees
            .iter()
            .all(|oid| selected_with_pack.contains(oid)),
        "pack-backed delta merge should import current tree objects needed for the browsable tree"
    );

    let delta_set: HashSet<String> = delta.iter().cloned().collect();
    let unchanged_current_blobs = current_tree
        .difference(&current_tree_trees)
        .filter(|oid| !delta_set.contains(*oid))
        .collect::<Vec<_>>();
    assert!(
        !unchanged_current_blobs.is_empty(),
        "fixture should have unchanged current blobs already covered by the base pack"
    );
    assert!(
        unchanged_current_blobs
            .iter()
            .all(|oid| !selected_with_pack.contains(*oid)),
        "pack-backed delta merge should not re-import unchanged pack-covered blobs as loose objects"
    );

    let inherited_pack_covered =
        RemoteHelper::inherited_pack_covered_imported_tree_candidates(&master_sha, &delta)
            .expect("pack-covered imported tree ids");
    assert!(
        current_tree_trees
            .iter()
            .filter(|oid| !delta_set.contains(*oid))
            .all(|oid| inherited_pack_covered.contains(oid)),
        "unchanged tree objects imported for view building should be marked pack-covered"
    );
    assert!(
        inherited_pack_covered
            .iter()
            .all(|oid| !delta_set.contains(oid)),
        "new delta tree objects must still be written as loose objects when no new pack is built"
    );
}

#[test]
fn test_git_pack_checkpoint_generation_is_deterministic() {
    let _env_lock = ENV_LOCK.lock().expect("env lock");
    let (_home, repo, base_sha, _master_sha, _dev_sha) = create_repo_with_diverged_master_and_dev();
    let _cwd_guard = CwdGuard::set(repo.path());

    let first = RemoteHelper::generate_git_pack_checkpoint(&base_sha, None)
        .expect("generate first checkpoint pack");
    assert!(
        git(repo.path(), &["repack", "-ad", "--depth=1", "--window=1"])
            .status
            .success(),
        "local repack should succeed"
    );
    let second = RemoteHelper::generate_git_pack_checkpoint(&base_sha, None)
        .expect("generate second checkpoint pack");

    assert_eq!(
        first.keys().collect::<Vec<_>>(),
        second.keys().collect::<Vec<_>>(),
        "checkpoint pack filenames should converge for the same tip"
    );
    assert_eq!(
        first, second,
        "checkpoint pack bytes should converge for the same tip"
    );
}

#[test]
fn test_git_pack_checkpoint_plans_incremental_chain() {
    let _env_lock = ENV_LOCK.lock().expect("env lock");
    let (_home, repo, shas) = create_repo_with_linear_history(6);
    let _cwd_guard = CwdGuard::set(repo.path());
    let head = shas.last().expect("head sha");

    let total_objects =
        RemoteHelper::reachable_git_object_count(head).expect("count current reachable objects");
    let plan = RemoteHelper::plan_git_pack_checkpoint(head, total_objects, None, 3, false)
        .expect("plan checkpoint")
        .expect("checkpoint should be planned");

    assert!(
        plan.packs.len() >= 3,
        "linear history should produce several checkpoint pack ranges"
    );
    for pair in plan.packs.windows(2) {
        assert_eq!(
            pair[1].exclude_tip.as_deref(),
            Some(pair[0].tip.as_str()),
            "each checkpoint pack should exclude the previous checkpoint tip"
        );
    }
    let unique_tips = plan
        .packs
        .iter()
        .map(|pack| pack.tip.as_str())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        unique_tips.len(),
        plan.packs.len(),
        "duplicate checkpoint tips should be collapsed"
    );
}

#[test]
fn test_git_pack_checkpoint_delta_pack_excludes_previous_tip() {
    let _env_lock = ENV_LOCK.lock().expect("env lock");
    let (_home, repo, shas) = create_repo_with_linear_history(3);
    let _cwd_guard = CwdGuard::set(repo.path());
    let base_sha = &shas[0];
    let head_sha = shas.last().expect("head sha");

    let pack_files = RemoteHelper::generate_git_pack_checkpoint(head_sha, Some(base_sha))
        .expect("generate delta checkpoint pack");
    let pack_dir = TempDir::new().expect("temp pack dir");
    let idx_name = pack_files
        .keys()
        .find(|name| name.ends_with(".idx"))
        .cloned()
        .expect("idx file");
    for (name, bytes) in pack_files {
        std::fs::write(pack_dir.path().join(name), bytes).expect("write pack file");
    }

    let idx_path = pack_dir.path().join(idx_name);
    let verify = Command::new("git")
        .args(["verify-pack", "-v"])
        .arg(&idx_path)
        .output()
        .expect("run git verify-pack");
    assert!(
        verify.status.success(),
        "git verify-pack failed: {}",
        String::from_utf8_lossy(&verify.stderr)
    );
    let verify_output = String::from_utf8_lossy(&verify.stdout);
    assert!(
        !verify_output.contains(base_sha),
        "delta checkpoint pack should not contain objects reachable from the previous checkpoint"
    );
    assert!(
        verify_output.contains(head_sha),
        "delta checkpoint pack should contain the new checkpoint tip"
    );
}

#[test]
fn test_git_pack_checkpoint_rebuilds_chain_without_tail_when_base_root_has_no_pack() {
    let _env_lock = ENV_LOCK.lock().expect("env lock");
    let (_home, repo, base_sha, master_sha) = create_repo_with_large_base_and_small_increment();
    let _cwd_guard = CwdGuard::set(repo.path());

    let total_objects = RemoteHelper::reachable_git_object_count(&master_sha)
        .expect("count current reachable objects");
    let interval = 10;
    assert!(
        total_objects >= interval,
        "fixture should exceed checkpoint threshold"
    );

    let skipped =
        RemoteHelper::plan_git_pack_checkpoint(&master_sha, 1, Some(&base_sha), interval, false)
            .expect("plan checkpoint without force");
    assert!(
        skipped.is_none(),
        "small increment in the same bucket should not normally rebuild a checkpoint"
    );

    let rebuilt =
        RemoteHelper::plan_git_pack_checkpoint(&master_sha, 1, Some(&base_sha), interval, true)
            .expect("plan rebuilt checkpoint")
            .expect("missing base checkpoint should rebuild deterministic checkpoints");
    assert!(
        rebuilt
            .packs
            .iter()
            .all(|pack| pack.tip.as_str() != master_sha.as_str()),
        "rebuilding a missing base checkpoint should not add a current-tip tail pack"
    );
    assert!(
        !rebuilt.covered_objects.contains(&master_sha),
        "current commit should remain in the loose frontier unless it is a deterministic checkpoint boundary"
    );
}

#[test]
fn test_git_pack_checkpoint_skips_bucket_without_new_boundary_tip() {
    let _env_lock = ENV_LOCK.lock().expect("env lock");
    let (_home, repo, base_sha, master_sha) = create_repo_with_large_base_and_small_increment();
    let _cwd_guard = CwdGuard::set(repo.path());

    let skipped = RemoteHelper::plan_git_pack_checkpoint(&master_sha, 1, Some(&base_sha), 6, false)
        .expect("plan checkpoint");
    assert!(
        skipped.is_none(),
        "a bucket increase should not publish a duplicate checkpoint pack when no newer commit boundary is below the target object count"
    );
}

#[test]
fn test_git_pack_checkpoint_ignores_untracked_gitignored_files() {
    let _env_lock = ENV_LOCK.lock().expect("env lock");
    let home = TempDir::new().expect("temp home");
    let _home_guard = HomeGuard::set(home.path());
    let repo = TempDir::new().expect("temp repo");
    assert!(git(repo.path(), &["init", "-b", "master"]).status.success());
    assert!(
        git(repo.path(), &["config", "user.email", "test@example.com"])
            .status
            .success()
    );
    assert!(git(repo.path(), &["config", "user.name", "Test User"])
        .status
        .success());

    std::fs::write(repo.path().join(".gitignore"), "ignored.txt\n").unwrap();
    std::fs::write(repo.path().join("tracked.txt"), "tracked\n").unwrap();
    assert!(git(repo.path(), &["add", ".gitignore", "tracked.txt"])
        .status
        .success());
    assert!(git(repo.path(), &["commit", "-m", "Tracked files"])
        .status
        .success());

    std::fs::write(repo.path().join("ignored.txt"), "ignored and untracked\n").unwrap();
    let status = git(repo.path(), &["status", "--ignored", "--short"]);
    assert!(status.status.success());
    assert!(
        String::from_utf8_lossy(&status.stdout).contains("!! ignored.txt"),
        "fixture should have an ignored untracked file"
    );

    let ignored_oid_output = git(repo.path(), &["hash-object", "ignored.txt"]);
    assert!(ignored_oid_output.status.success());
    let ignored_oid = String::from_utf8_lossy(&ignored_oid_output.stdout)
        .trim()
        .to_string();
    let head_sha = String::from_utf8_lossy(&git(repo.path(), &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string();

    let _cwd_guard = CwdGuard::set(repo.path());
    let pack_files = RemoteHelper::generate_git_pack_checkpoint(&head_sha, None)
        .expect("generate checkpoint pack");
    let pack_dir = TempDir::new().expect("temp pack dir");
    let idx_name = pack_files
        .keys()
        .find(|name| name.ends_with(".idx"))
        .cloned()
        .expect("idx file");
    for (name, bytes) in pack_files {
        std::fs::write(pack_dir.path().join(name), bytes).expect("write pack file");
    }

    let idx_path = pack_dir.path().join(idx_name);
    let verify = Command::new("git")
        .args(["verify-pack", "-v"])
        .arg(&idx_path)
        .output()
        .expect("run git verify-pack");
    assert!(
        verify.status.success(),
        "git verify-pack failed: {}",
        String::from_utf8_lossy(&verify.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&verify.stdout).contains(&ignored_oid),
        "ignored untracked file blob should not be in checkpoint pack"
    );
}

#[test]
fn test_git_pack_install_streams_pack_and_index_files() {
    let _env_lock = ENV_LOCK.lock().expect("env lock");
    let home = TempDir::new().expect("temp home");
    let _home_guard = HomeGuard::set(home.path());
    let repo = TempDir::new().expect("temp repo");
    assert!(git(repo.path(), &["init", "-b", "master"]).status.success());
    let _cwd_guard = CwdGuard::set(repo.path());

    let helper = create_test_helper().expect("helper");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");

    let pack_bytes = (0..5000).map(|i| (i % 251) as u8).collect::<Vec<_>>();
    let idx_bytes = b"synthetic index bytes".to_vec();
    let store = Arc::new(MemoryStore::new());
    let tree = HashTree::new(HashTreeConfig::new(store).public().with_chunk_size(97));
    let (pack_cid, pack_size) = rt
        .block_on(tree.put(&pack_bytes))
        .expect("write test pack to tree");
    let (idx_cid, idx_size) = rt
        .block_on(tree.put(&idx_bytes))
        .expect("write test idx to tree");

    let pack_hash = "0123456789abcdef0123456789abcdef01234567";
    let pack_name = format!("pack-{pack_hash}.pack");
    let idx_name = format!("pack-{pack_hash}.idx");
    let locations = vec![GitPackLocation {
        pack_name: pack_name.clone(),
        pack_cid,
        pack_size,
        idx_name: idx_name.clone(),
        idx_cid: Some(idx_cid),
        idx_size: Some(idx_size),
    }];

    let installed = rt
        .block_on(helper.install_git_pack_files_async(&tree, &locations))
        .expect("install git pack files");

    assert_eq!(installed, 1);
    assert_eq!(
        std::fs::read(repo.path().join(".git/objects/pack").join(pack_name)).unwrap(),
        pack_bytes
    );
    assert_eq!(
        std::fs::read(repo.path().join(".git/objects/pack").join(idx_name)).unwrap(),
        idx_bytes
    );
}

#[test]
fn test_push_to_file_servers_with_diff_does_not_fetch_old_tree_from_blossom() {
    let _env_lock = ENV_LOCK.lock().expect("env lock");
    let home = TempDir::new().expect("temp home");
    let _home_guard = HomeGuard::set(home.path());
    let fake_blossom = CountingBlossomServer::new();
    write_test_config(home.path(), fake_blossom.base_url(), true);

    let mut config = Config::default();
    config.nostr.relays = vec![];
    config.blossom.read_servers = vec![fake_blossom.base_url().to_string()];
    config.blossom.write_servers = vec![fake_blossom.base_url().to_string()];
    config.blossom.force_upload = true;

    let helper = create_test_helper_with_config(config).expect("helper");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");

    let (old_cid, new_cid) = rt.block_on(async {
        let old_store = Arc::new(MemoryStore::new());
        let old_tree = HashTree::new(HashTreeConfig::new(old_store.clone()).public());
        let (old_cid, _) = old_tree
            .put(b"old tree exists only on blossom")
            .await
            .expect("build old tree");
        let old_bytes = old_store
            .get(&old_cid.hash)
            .await
            .expect("read old root")
            .expect("old root bytes");

        hashtree_blossom::BlossomClient::new_empty(nostr::Keys::generate())
            .with_servers(vec![fake_blossom.base_url().to_string()])
            .upload(&old_bytes)
            .await
            .expect("upload old tree to fake blossom");

        let new_store = helper.storage.store().clone();
        let new_tree = HashTree::new(HashTreeConfig::new(new_store).public());
        let (new_cid, _) = new_tree
            .put(b"new tree exists only locally")
            .await
            .expect("build new tree");

        (old_cid, new_cid)
    });

    let result = helper.push_to_file_servers_with_diff(
        &hex::encode(new_cid.hash),
        None,
        Some(&hex::encode(old_cid.hash)),
        None,
        true,
    );

    assert!(
        result.failed.is_empty(),
        "diff upload should succeed without remote old-tree fetches: {:?}",
        result.failed
    );
    assert_eq!(
        fake_blossom.get_request_count(),
        0,
        "diff collection should not fetch the old tree from Blossom when it is missing locally"
    );
}

#[test]
fn test_push_to_file_servers_with_unavailable_old_root_probes_before_reupload() {
    let _env_lock = ENV_LOCK.lock().expect("env lock");
    let home = TempDir::new().expect("temp home");
    let _home_guard = HomeGuard::set(home.path());
    let fake_blossom = CountingBlossomServer::new();
    write_test_config(home.path(), fake_blossom.base_url(), true);

    let mut config = Config::default();
    config.nostr.relays = vec![];
    config.blossom.read_servers = vec![fake_blossom.base_url().to_string()];
    config.blossom.write_servers = vec![fake_blossom.base_url().to_string()];

    let helper = create_test_helper_with_config(config).expect("helper");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");

    let (old_cid, new_cid, new_hash_count) = rt.block_on(async {
        let old_store = Arc::new(MemoryStore::new());
        let old_tree = HashTree::new(HashTreeConfig::new(old_store).public());
        let (old_cid, _) = old_tree
            .put(b"old root is not in the local cache or file server")
            .await
            .expect("build old tree");

        let new_store = helper.storage.store().clone();
        let new_tree = HashTree::new(HashTreeConfig::new(new_store.clone()).public());
        let mut entries = Vec::new();
        for idx in 0..48 {
            let body = format!("unchanged small blob {idx:02}\n{}", "x".repeat(128));
            let (cid, size) = new_tree
                .put_file(body.as_bytes())
                .await
                .expect("write new file");
            entries.push(DirEntry::from_cid(format!("file-{idx:02}.txt"), &cid).with_size(size));
        }
        let new_cid = new_tree
            .put_directory(entries)
            .await
            .expect("write new directory");
        let new_hashes = collect_hashes(&new_tree, &new_cid, 32)
            .await
            .expect("collect new hashes");

        for hash in &new_hashes {
            let data = new_store
                .get(hash)
                .await
                .expect("read new blob")
                .expect("new blob exists");
            fake_blossom.insert_blob(data);
        }

        (old_cid, new_cid, new_hashes.len())
    });

    let result = helper.push_to_file_servers_with_diff(
        &hex::encode(new_cid.hash),
        new_cid.key.as_ref(),
        Some(&hex::encode(old_cid.hash)),
        old_cid.key.as_ref(),
        true,
    );

    assert!(
        result.failed.is_empty(),
        "push should succeed even when old root is unavailable: {:?}",
        result.failed
    );
    assert!(
        fake_blossom.get_head_request_count() >= new_hash_count,
        "fallback should probe server state before uploading existing blobs"
    );
    assert_eq!(
        fake_blossom.get_upload_request_count(),
        0,
        "existing small blobs should be skipped by HEAD instead of re-sent as upload bodies"
    );
}

#[test]
fn test_push_to_file_servers_with_diff_trusts_sampled_old_tree_coverage() {
    let _env_lock = ENV_LOCK.lock().expect("env lock");
    let home = TempDir::new().expect("temp home");
    let _home_guard = HomeGuard::set(home.path());
    let fake_blossom = CountingBlossomServer::new();
    write_test_config(home.path(), fake_blossom.base_url(), true);

    let mut config = Config::default();
    config.nostr.relays = vec![];
    config.blossom.read_servers = vec![fake_blossom.base_url().to_string()];
    config.blossom.write_servers = vec![fake_blossom.base_url().to_string()];

    let helper = create_test_helper_with_config(config).expect("helper");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");

    let (old_cid, new_cid, old_hash_count) = rt.block_on(async {
        let store = helper.storage.store().clone();
        let tree = HashTree::new(HashTreeConfig::new(store.clone()).public());

        let mut old_entries = Vec::new();
        for idx in 0..64 {
            let content = format!("old-file-{idx:02}-{}", "x".repeat(64));
            let (file_cid, file_size) = tree
                .put_file(content.as_bytes())
                .await
                .expect("write old file");
            old_entries.push(
                DirEntry::from_cid(format!("file-{idx:02}.txt"), &file_cid).with_size(file_size),
            );
        }

        let old_cid = tree
            .put_directory(old_entries.clone())
            .await
            .expect("write old directory");
        let old_hashes = collect_hashes(&tree, &old_cid, 32)
            .await
            .expect("collect old hashes");

        let blossom = hashtree_blossom::BlossomClient::new_empty(nostr::Keys::generate())
            .with_servers(vec![fake_blossom.base_url().to_string()]);
        for hash in &old_hashes {
            let data = store
                .get(hash)
                .await
                .expect("read old blob")
                .expect("old blob exists");
            blossom.upload(&data).await.expect("upload old blob");
        }

        let (new_file_cid, new_file_size) =
            tree.put_file(b"new file").await.expect("write new file");
        let mut new_entries = old_entries;
        new_entries.push(DirEntry::from_cid("new.txt", &new_file_cid).with_size(new_file_size));
        let new_cid = tree
            .put_directory(new_entries)
            .await
            .expect("write new directory");

        (old_cid, new_cid, old_hashes.len())
    });

    let result = helper.push_to_file_servers_with_diff(
        &hex::encode(new_cid.hash),
        None,
        Some(&hex::encode(old_cid.hash)),
        None,
        true,
    );

    assert!(
        result.failed.is_empty(),
        "diff upload should succeed when old tree is already on blossom: {:?}",
        result.failed
    );
    assert_eq!(
        fake_blossom.get_request_count(),
        0,
        "push diff should not need GET requests when old tree is already local"
    );
    assert!(
        fake_blossom.get_head_request_count() <= old_hash_count.min(32),
        "expected only sampled HEAD probes for a single write server, got {} for {} old hashes",
        fake_blossom.get_head_request_count(),
        old_hash_count
    );
}

#[test]
fn test_push_diff_prunes_when_only_previous_root_blob_is_missing_on_server() {
    let _env_lock = ENV_LOCK.lock().expect("env lock");
    let home = TempDir::new().expect("temp home");
    let _home_guard = HomeGuard::set(home.path());
    let fake_blossom = CountingBlossomServer::new();
    write_test_config(home.path(), fake_blossom.base_url(), true);

    let mut config = Config::default();
    config.nostr.relays = vec![];
    config.blossom.read_servers = vec![fake_blossom.base_url().to_string()];
    config.blossom.write_servers = vec![fake_blossom.base_url().to_string()];

    let helper = create_test_helper_with_config(config).expect("helper");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");

    let (old_cid, new_cid, old_hash_count) = rt.block_on(async {
        let store = helper.storage.store().clone();
        let tree = HashTree::new(HashTreeConfig::new(store.clone()).public());

        let mut old_entries = Vec::new();
        for idx in 0..64 {
            let content = format!("old-file-{idx:02}-{}", "x".repeat(64));
            let (file_cid, file_size) = tree
                .put_file(content.as_bytes())
                .await
                .expect("write old file");
            old_entries.push(
                DirEntry::from_cid(format!("file-{idx:02}.txt"), &file_cid).with_size(file_size),
            );
        }

        let old_cid = tree
            .put_directory(old_entries.clone())
            .await
            .expect("write old directory");
        let old_hashes = collect_hashes(&tree, &old_cid, 32)
            .await
            .expect("collect old hashes");

        for hash in &old_hashes {
            if hash == &old_cid.hash {
                continue;
            }
            let data = store
                .get(hash)
                .await
                .expect("read old blob")
                .expect("old blob exists");
            fake_blossom.insert_blob(data);
        }

        let (new_file_cid, new_file_size) =
            tree.put_file(b"new file").await.expect("write new file");
        let mut new_entries = old_entries;
        new_entries.push(DirEntry::from_cid("new.txt", &new_file_cid).with_size(new_file_size));
        let new_cid = tree
            .put_directory(new_entries)
            .await
            .expect("write new directory");

        (old_cid, new_cid, old_hashes.len())
    });

    let result = helper.push_to_file_servers_with_diff(
        &hex::encode(new_cid.hash),
        None,
        Some(&hex::encode(old_cid.hash)),
        None,
        true,
    );

    assert!(
        result.failed.is_empty(),
        "diff upload should succeed when reusable old children are on Blossom: {:?}",
        result.failed
    );
    assert!(
        fake_blossom.get_head_request_count() <= old_hash_count.min(32),
        "coverage probe should sample old children without forcing a full walk"
    );
    assert!(
        fake_blossom.get_upload_request_count() <= 2,
        "missing previous root alone should not re-upload unchanged old children"
    );
}

#[test]
fn test_push_to_file_servers_with_diff_force_upload_skips_old_tree_probes() {
    let _env_lock = ENV_LOCK.lock().expect("env lock");
    let home = TempDir::new().expect("temp home");
    let _home_guard = HomeGuard::set(home.path());
    let fake_blossom = CountingBlossomServer::new();
    write_test_config(home.path(), fake_blossom.base_url(), true);

    let mut config = Config::default();
    config.nostr.relays = vec![];
    config.blossom.read_servers = vec![fake_blossom.base_url().to_string()];
    config.blossom.write_servers = vec![fake_blossom.base_url().to_string()];
    config.blossom.force_upload = true;

    let helper = create_test_helper_with_config(config).expect("helper");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");

    let (old_cid, new_cid) = rt.block_on(async {
        let store = helper.storage.store().clone();
        let tree = HashTree::new(HashTreeConfig::new(store).public());

        let (old_file_cid, old_file_size) = tree
            .put_file(b"old file kept in the new tree")
            .await
            .expect("write old file");
        let old_entries =
            vec![DirEntry::from_cid("shared.txt", &old_file_cid).with_size(old_file_size)];
        let old_cid = tree
            .put_directory(old_entries.clone())
            .await
            .expect("write old directory");

        let (new_file_cid, new_file_size) =
            tree.put_file(b"new file").await.expect("write new file");
        let mut new_entries = old_entries;
        new_entries.push(DirEntry::from_cid("new.txt", &new_file_cid).with_size(new_file_size));
        let new_cid = tree
            .put_directory(new_entries)
            .await
            .expect("write new directory");

        (old_cid, new_cid)
    });

    let result = helper.push_to_file_servers_with_diff(
        &hex::encode(new_cid.hash),
        None,
        Some(&hex::encode(old_cid.hash)),
        None,
        true,
    );

    assert!(
        result.failed.is_empty(),
        "force upload should succeed without old-tree probes: {:?}",
        result.failed
    );
    assert_eq!(
        fake_blossom.get_head_request_count(),
        0,
        "force upload should upload the new tree directly instead of probing every old hash"
    );
}

#[test]
fn test_push_to_file_servers_with_diff_uploads_new_hashes_to_any_write_server() {
    let _env_lock = ENV_LOCK.lock().expect("env lock");
    let home = TempDir::new().expect("temp home");
    let _home_guard = HomeGuard::set(home.path());
    let server_a = CountingBlossomServer::new();
    let server_b = CountingBlossomServer::new();
    write_test_config_for_servers(
        home.path(),
        &[server_a.base_url(), server_b.base_url()],
        false,
    );

    let mut config = Config::default();
    config.nostr.relays = vec![];
    config.blossom.read_servers = vec![
        server_a.base_url().to_string(),
        server_b.base_url().to_string(),
    ];
    config.blossom.write_servers = vec![
        server_a.base_url().to_string(),
        server_b.base_url().to_string(),
    ];

    let helper = create_test_helper_with_config(config).expect("helper");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");

    let root_cid = rt.block_on(async {
        let store = helper.storage.store().clone();
        let tree = HashTree::new(HashTreeConfig::new(store).public());
        tree.put(b"new root must land on at least one write server")
            .await
            .expect("write root")
            .0
    });

    let result =
        helper.push_to_file_servers_with_diff(&hex::encode(root_cid.hash), None, None, None, true);

    assert!(
        result.failed.is_empty(),
        "push upload should succeed when at least one write server accepts it: {:?}",
        result.failed
    );
    assert!(
        server_a.has_blob(&root_cid.hash) || server_b.has_blob(&root_cid.hash),
        "at least one write server should have the new root"
    );
}

#[test]
fn test_push_to_file_servers_with_diff_reports_degraded_local_only_upload() {
    let _env_lock = ENV_LOCK.lock().expect("env lock");
    let home = TempDir::new().expect("temp home");
    let _home_guard = HomeGuard::set(home.path());
    let failing_server_a = CountingBlossomServer::failing_uploads();
    let failing_server_b = CountingBlossomServer::failing_uploads();
    write_test_config_for_servers(
        home.path(),
        &[failing_server_a.base_url(), failing_server_b.base_url()],
        false,
    );

    let mut config = Config::default();
    config.nostr.relays = vec![];
    config.blossom.read_servers = vec![
        failing_server_a.base_url().to_string(),
        failing_server_b.base_url().to_string(),
    ];
    config.blossom.write_servers = vec![
        failing_server_a.base_url().to_string(),
        failing_server_b.base_url().to_string(),
    ];

    let helper = create_test_helper_with_config(config).expect("helper");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");

    let root_cid = rt.block_on(async {
        let store = helper.storage.store().clone();
        let tree = HashTree::new(HashTreeConfig::new(store).public());
        tree.put(b"new root can publish from local store only")
            .await
            .expect("write root")
            .0
    });

    let result =
        helper.push_to_file_servers_with_diff(&hex::encode(root_cid.hash), None, None, None, true);

    assert!(
        result.local_complete,
        "local store should still be a complete availability source"
    );
    assert!(
        result.degraded,
        "all write server failures should be reported as degraded replication"
    );
    assert!(
        !result.failed.is_empty(),
        "write server failures should be reported"
    );
    assert!(
        !failing_server_a.has_blob(&root_cid.hash) && !failing_server_b.has_blob(&root_cid.hash),
        "failing servers should not receive the blob"
    );
}

#[test]
fn test_queue_links_for_diff_upload_prunes_known_subtrees() {
    let old_hash = [1u8; 32];
    let new_hash = [2u8; 32];
    let links = vec![Link::new(old_hash), Link::new(new_hash)];
    let old_hashes = HashSet::from([old_hash]);
    let mut queue = Vec::new();
    let mut queued = HashSet::new();
    let discovered = std::sync::atomic::AtomicUsize::new(0);

    super::push::queue_links_for_diff_upload(
        &mut queue,
        &mut queued,
        &links,
        &old_hashes,
        true,
        &discovered,
    );

    assert_eq!(queue.len(), 1, "known old subtrees should not be queued");
    assert_eq!(queue[0].0, new_hash);
    assert_eq!(
        discovered.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "only the new child should count as discovered work"
    );
}

#[test]
fn test_repo_not_found_error_classifier() {
    let missing = anyhow::anyhow!(
        "Repository 'bench' not found (no hashtree event published by npub1example)"
    );
    assert!(RemoteHelper::is_repo_not_found_error(&missing));

    let timeout = anyhow::anyhow!("relay query timed out");
    assert!(!RemoteHelper::is_repo_not_found_error(&timeout));
}

#[test]
fn test_git_tree_walk_concurrency_defaults_and_caps_env() {
    let _env_lock = ENV_LOCK.lock().expect("env lock");
    let _clear = EnvGuard::clear("HTREE_GIT_TREE_WALK_CONCURRENCY");
    assert_eq!(
        git_tree_walk_concurrency(),
        DEFAULT_GIT_TREE_WALK_CONCURRENCY
    );

    {
        let _set = EnvGuard::set("HTREE_GIT_TREE_WALK_CONCURRENCY", "12");
        assert_eq!(git_tree_walk_concurrency(), 12);
    }

    let _set = EnvGuard::set("HTREE_GIT_TREE_WALK_CONCURRENCY", "999");
    assert_eq!(git_tree_walk_concurrency(), MAX_GIT_TREE_WALK_CONCURRENCY);
}

#[test]
fn test_git_object_download_concurrency_defaults_and_caps_env() {
    let _env_lock = ENV_LOCK.lock().expect("env lock");
    let _clear = EnvGuard::clear("HTREE_GIT_OBJECT_DOWNLOAD_CONCURRENCY");
    let direct = vec!["https://upload.example".to_string()];
    let multi = vec![
        "https://cdn.example".to_string(),
        "https://upload.example".to_string(),
    ];
    let local_first = vec![
        "http://127.0.0.1:8080".to_string(),
        "https://cdn.example".to_string(),
    ];

    assert_eq!(
        git_object_download_concurrency_for_read_servers(&direct),
        DEFAULT_DIRECT_GIT_OBJECT_DOWNLOAD_CONCURRENCY
    );
    assert_eq!(
        git_object_download_concurrency_for_read_servers(&multi),
        DEFAULT_GIT_OBJECT_DOWNLOAD_CONCURRENCY
    );
    assert_eq!(
        git_object_download_concurrency_for_read_servers(&local_first),
        DEFAULT_GIT_OBJECT_DOWNLOAD_CONCURRENCY
    );

    {
        let _set = EnvGuard::set("HTREE_GIT_OBJECT_DOWNLOAD_CONCURRENCY", "96");
        assert_eq!(
            git_object_download_concurrency_for_read_servers(&direct),
            96
        );
        assert_eq!(git_object_download_concurrency_for_read_servers(&multi), 96);
    }

    {
        let _set = EnvGuard::set("HTREE_GIT_OBJECT_DOWNLOAD_CONCURRENCY", "0");
        assert_eq!(
            git_object_download_concurrency_for_read_servers(&direct),
            DEFAULT_DIRECT_GIT_OBJECT_DOWNLOAD_CONCURRENCY
        );
    }

    let _set = EnvGuard::set("HTREE_GIT_OBJECT_DOWNLOAD_CONCURRENCY", "999");
    assert_eq!(
        git_object_download_concurrency_for_read_servers(&direct),
        MAX_GIT_OBJECT_DOWNLOAD_CONCURRENCY
    );
}
