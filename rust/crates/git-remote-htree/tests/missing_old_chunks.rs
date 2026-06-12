//! Missing old-chunk repair tests
//!
//! Reproduces a server state where old data disappeared, but initial HEAD
//! sampling still reports success. Push must still repair missing old chunks.

mod common;

use axum::{
    body::{Body, Bytes},
    extract::{Path as AxumPath, State},
    http::{header, HeaderMap, Response, StatusCode},
    routing::put,
    Router,
};
use common::{create_test_repo, skip_if_no_binary, test_relay::TestRelay, TestEnv};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::sync::oneshot;

#[derive(Default)]
struct FakeBlossomState {
    blobs: HashMap<String, Vec<u8>>,
    spoof_head_remaining: usize,
    drop_all_after_spoof: bool,
    drop_events: usize,
    head_requests: usize,
}

struct FakeBlossomServer {
    port: u16,
    state: Arc<Mutex<FakeBlossomState>>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    server_thread: Option<JoinHandle<()>>,
}

impl FakeBlossomServer {
    fn new(port: u16) -> Self {
        let state = Arc::new(Mutex::new(FakeBlossomState::default()));
        let state_clone = Arc::clone(&state);
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        let server_thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("Failed to build tokio runtime for fake blossom server");

            rt.block_on(async move {
                let app = Router::new()
                    .route("/upload", put(upload_blob))
                    .route("/:id", axum::routing::get(get_blob).head(head_blob))
                    .with_state(state_clone);

                let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{}", port))
                    .await
                    .expect("Failed to bind fake blossom server");

                axum::serve(listener, app)
                    .with_graceful_shutdown(async {
                        let _ = shutdown_rx.await;
                    })
                    .await
                    .expect("Fake blossom server failed");
            });
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return Self {
                    port,
                    state,
                    shutdown_tx: Some(shutdown_tx),
                    server_thread: Some(server_thread),
                };
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("Fake blossom server did not start on port {}", port);
    }

    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn spoof_next_head_checks_then_drop_all(&self, count: usize) {
        let mut state = self.state.lock().expect("state lock poisoned");
        state.spoof_head_remaining = count;
        state.drop_all_after_spoof = true;
    }

    fn drop_event_count(&self) -> usize {
        let state = self.state.lock().expect("state lock poisoned");
        state.drop_events
    }

    fn head_request_count(&self) -> usize {
        let state = self.state.lock().expect("state lock poisoned");
        state.head_requests
    }
}

impl Drop for FakeBlossomServer {
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
    State(state): State<Arc<Mutex<FakeBlossomState>>>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let mut hasher = Sha256::new();
    hasher.update(&body);
    let computed_hash = hex::encode(hasher.finalize());

    if let Some(expected_hash) = headers
        .get("x-sha-256")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_ascii_lowercase())
    {
        if expected_hash != computed_hash {
            return StatusCode::BAD_REQUEST;
        }
    }

    let mut state = state.lock().expect("state lock poisoned");
    let was_new = state.blobs.insert(computed_hash, body.to_vec()).is_none();
    if was_new {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    }
}

async fn head_blob(
    State(state): State<Arc<Mutex<FakeBlossomState>>>,
    AxumPath(id): AxumPath<String>,
) -> Response<Body> {
    let Some(hash) = parse_hash_from_path(&id) else {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::empty())
            .unwrap();
    };

    let mut state = state.lock().expect("state lock poisoned");
    state.head_requests += 1;
    if state.spoof_head_remaining > 0 {
        state.spoof_head_remaining -= 1;
        if state.spoof_head_remaining == 0 && state.drop_all_after_spoof {
            state.blobs.clear();
            state.drop_all_after_spoof = false;
            state.drop_events += 1;
        }
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .header(header::CONTENT_LENGTH, "1")
            .body(Body::empty())
            .unwrap();
    }

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
    State(state): State<Arc<Mutex<FakeBlossomState>>>,
    AxumPath(id): AxumPath<String>,
) -> Response<Body> {
    let Some(hash) = parse_hash_from_path(&id) else {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::empty())
            .unwrap();
    };

    let data = {
        let state = state.lock().expect("state lock poisoned");
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

#[test]
fn test_push_repairs_missing_old_chunks_after_sample_check() {
    if skip_if_no_binary() {
        return;
    }

    let relay = TestRelay::new(19320);
    let fake_blossom = FakeBlossomServer::new(19321);
    println!(
        "Started local nostr relay at {}, fake blossom at {}",
        relay.url(),
        fake_blossom.base_url()
    );

    let test_env = TestEnv::new(Some(&fake_blossom.base_url()), Some(&relay.url()));
    let env_vars: Vec<_> = test_env.env();

    let repo = create_test_repo();
    // Grow the initial history so second push reuses lots of old objects.
    let bulk_dir = repo.path().join("bulk");
    std::fs::create_dir_all(&bulk_dir).expect("Failed to create bulk dir");
    for i in 0..300 {
        let path = bulk_dir.join(format!("file-{i:04}.txt"));
        let content = format!("bulk file {i}\n{}\n", "x".repeat(256));
        std::fs::write(path, content).expect("Failed to write bulk file");
    }
    Command::new("git")
        .args(["add", "bulk"])
        .current_dir(repo.path())
        .output()
        .expect("Failed to git add bulk files");
    Command::new("git")
        .args(["commit", "-m", "Add bulk files"])
        .current_dir(repo.path())
        .stdout(Stdio::null())
        .output()
        .expect("Failed to commit bulk files");

    let remote_url = "htree://self/missing-old-chunks";

    Command::new("git")
        .args(["remote", "add", "htree", remote_url])
        .current_dir(repo.path())
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed to add remote");

    // First push seeds old tree state.
    let push1 = Command::new("git")
        .args(["push", "htree", "master"])
        .current_dir(repo.path())
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed first push");
    let stderr1 = String::from_utf8_lossy(&push1.stderr);
    if !push1.status.success() && !stderr1.contains("-> master") {
        panic!("First push failed: {}", stderr1);
    }

    // Second push should still repair old chunks, even if initial sample checks are spoofed.
    // Use an empty commit so almost all required objects are old/unchanged.
    Command::new("git")
        .args(["commit", "--allow-empty", "-m", "empty change"])
        .current_dir(repo.path())
        .stdout(Stdio::null())
        .output()
        .expect("Failed git commit");

    // Spoof exactly the first 5 HEAD checks (server sample check), then drop all old blobs.
    fake_blossom.spoof_next_head_checks_then_drop_all(5);
    let head_requests_before_push2 = fake_blossom.head_request_count();

    let push2 = Command::new("git")
        .args(["push", "htree", "master"])
        .current_dir(repo.path())
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed second push");
    let stderr2 = String::from_utf8_lossy(&push2.stderr);
    println!("Second push stderr:\n{}", stderr2);
    if !push2.status.success() && !stderr2.contains("-> master") {
        panic!("Second push failed: {}", stderr2);
    }
    assert!(
        fake_blossom.drop_event_count() > 0,
        "Expected fake blossom to drop old blobs after sample checks"
    );
    let head_requests_after_push2 = fake_blossom.head_request_count();
    let head_requests_during_push2 = head_requests_after_push2 - head_requests_before_push2;
    assert!(
        head_requests_during_push2 <= 64,
        "Expected second push to use bounded HEAD checks, got {}",
        head_requests_during_push2
    );
    assert!(
        stderr2.contains("Computing diff") || stderr2.contains("unchanged"),
        "Expected diff path on second push, stderr:\n{}",
        stderr2
    );

    // Clone from a fresh repo; should succeed if old chunks were repaired.
    let clone_url = format!("htree://{}/missing-old-chunks", test_env.npub);
    let clone_dir = TempDir::new().expect("Failed to create clone dir");
    let clone_path = clone_dir.path().join("clone");
    let clone = Command::new("git")
        .args(["clone", &clone_url, clone_path.to_str().unwrap()])
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed git clone");
    println!("Clone stderr:\n{}", String::from_utf8_lossy(&clone.stderr));
    if !clone.status.success() {
        panic!(
            "Clone failed after second push:\n{}",
            String::from_utf8_lossy(&clone.stderr)
        );
    }

    // Some test runs may not set remote HEAD; explicitly checkout origin/master.
    let checkout = Command::new("git")
        .args(["checkout", "-f", "-B", "master", "origin/master"])
        .current_dir(&clone_path)
        .output()
        .expect("Failed to checkout origin/master");
    if !checkout.status.success() {
        panic!(
            "Checkout failed after clone:\n{}",
            String::from_utf8_lossy(&checkout.stderr)
        );
    }

    assert!(
        clone_path.join("README.md").exists(),
        "Expected README.md after successful clone"
    );
}
