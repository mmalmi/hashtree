//! Missing old-chunk repair tests
//!
//! The writer loses old chunks between pushes and still answers HEAD positively.
//! A changed repository must restore those chunks for a reader with empty storage.

#[path = "common/blossom_claims.rs"]
mod blossom_claims;
mod common;
use blossom_claims::{signed_author, ListQuery, OwnerClaims};

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
    claims: OwnerClaims,
    stale_head: bool,
}

struct FakeBlossomServer {
    port: u16,
    state: Arc<Mutex<FakeBlossomState>>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    server_thread: Option<JoinHandle<()>>,
}

impl FakeBlossomServer {
    fn new() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
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
                    .route("/list/:owner", axum::routing::get(list_claims))
                    .route("/upload", put(upload_blob))
                    .route("/:id", axum::routing::get(get_blob).head(head_blob))
                    .with_state(state_clone);

                let listener = tokio::net::TcpListener::from_std(listener)
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

    fn lose_stored_blobs(&self, owner: &str) {
        let mut state = self.state.lock().unwrap();
        assert!(!state.blobs.is_empty(), "initial push must seed the writer");
        assert!(state
            .blobs
            .keys()
            .all(|hash| state.claims.contains(owner, hash)));
        state.blobs.clear();
        state.claims = OwnerClaims::default();
        state.stale_head = true;
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

async fn list_claims(
    State(state): State<Arc<Mutex<FakeBlossomState>>>,
    AxumPath(owner): AxumPath<String>,
    axum::extract::Query(query): axum::extract::Query<ListQuery>,
    headers: HeaderMap,
) -> Response<Body> {
    state.lock().unwrap().claims.list(&headers, &owner, &query)
}

async fn upload_blob(
    State(state): State<Arc<Mutex<FakeBlossomState>>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    let mut hasher = Sha256::new();
    hasher.update(&body);
    let computed_hash = hex::encode(hasher.finalize());

    if let Some(expected_hash) = headers
        .get("x-sha-256")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_ascii_lowercase())
    {
        if expected_hash != computed_hash {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::empty())
                .unwrap();
        }
    }

    let mut state = state.lock().expect("state lock poisoned");
    state
        .claims
        .record(signed_author(&headers, "upload").as_deref(), &computed_hash);
    let was_new = state
        .blobs
        .insert(computed_hash.clone(), body.to_vec())
        .is_none();
    let status = if was_new {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::json!({"sha256": computed_hash, "size": body.len()}).to_string(),
        ))
        .unwrap()
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

    let state = state.lock().expect("state lock poisoned");
    if state.stale_head && !state.blobs.contains_key(&hash) {
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
fn test_push_repairs_lost_chunks_despite_stale_head_responses() {
    if skip_if_no_binary() {
        return;
    }

    let relay = TestRelay::new();
    let fake_blossom = FakeBlossomServer::new();
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
    if !push1.status.success() {
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

    fake_blossom.lose_stored_blobs(&nostr::PublicKey::parse(&test_env.npub).unwrap().to_hex());

    let push2 = Command::new("git")
        .args(["push", "htree", "master"])
        .current_dir(repo.path())
        .envs(env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .output()
        .expect("Failed second push");
    let stderr2 = String::from_utf8_lossy(&push2.stderr);
    println!("Second push stderr:\n{}", stderr2);
    if !push2.status.success() {
        panic!("Second push failed: {}", stderr2);
    }
    // Clone from a fresh repo; should succeed if old chunks were repaired.
    let reader = TestEnv::new(Some(&fake_blossom.base_url()), Some(&relay.url()));
    let clone_url = format!("htree://{}/missing-old-chunks", test_env.npub);
    let clone_dir = TempDir::new().expect("Failed to create clone dir");
    let clone_path = clone_dir.path().join("clone");
    let clone = Command::new("git")
        .args(["clone", &clone_url, clone_path.to_str().unwrap()])
        .envs(reader.env())
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

    let fsck = Command::new("git")
        .args(["fsck", "--full"])
        .current_dir(&clone_path)
        .output()
        .unwrap();
    assert!(
        fsck.status.success(),
        "{}",
        String::from_utf8_lossy(&fsck.stderr)
    );
    assert_eq!(
        std::fs::read_dir(clone_path.join("bulk")).unwrap().count(),
        300
    );
    assert!(
        clone_path.join("README.md").exists(),
        "Expected README.md after successful clone"
    );
}
