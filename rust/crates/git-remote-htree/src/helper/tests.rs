use super::*;
use axum::{
    body::{Body, Bytes},
    extract::{Path as AxumPath, State},
    http::{header, HeaderMap, Response, StatusCode},
    routing::put,
    Router,
};
use hashtree_core::{collect_hashes, DirEntry, HashTree, HashTreeConfig, Link, MemoryStore, Store};
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
}

struct CountingBlossomServer {
    state: Arc<Mutex<CountingBlossomState>>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    server_thread: Option<JoinHandle<()>>,
    base_url: String,
}

impl CountingBlossomServer {
    fn new() -> Self {
        let state = Arc::new(Mutex::new(CountingBlossomState::default()));
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

fn create_test_helper() -> Option<RemoteHelper> {
    let config = Config::default();
    RemoteHelper::new(TEST_PUBKEY, "test-repo", None, None, false, config).ok()
}

fn create_test_helper_with_config(config: Config) -> Option<RemoteHelper> {
    RemoteHelper::new(TEST_PUBKEY, "test-repo", None, None, false, config).ok()
}

fn write_test_config(home: &std::path::Path, blossom_url: &str, force_upload: bool) {
    let config_dir = home.join(".hashtree");
    std::fs::create_dir_all(&config_dir).expect("create config dir");
    let config = format!(
        r#"
[server]
enable_auth = false
stun_port = 0

[nostr]
relays = []
social_graph_crawl_depth = 0

[blossom]
read_servers = ["{blossom_url}"]
write_servers = ["{blossom_url}"]
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
    let Some(mut helper) = create_test_helper() else {
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
        fake_blossom.get_head_request_count() <= old_hash_count.min(SERVER_COVERAGE_SAMPLE_SIZE),
        "expected only sampled HEAD probes, got {} for {} old hashes",
        fake_blossom.get_head_request_count(),
        old_hash_count
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
