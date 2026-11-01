//! Real signed relay messages and CLI processes, with no external services.
use axum::{
    extract::{ws::Message, State, WebSocketUpgrade},
    response::Response,
    routing::get,
    Router,
};
use git_remote_htree::discovery::{search_with, Query, SearchOptions};
use nostr::{Event, EventBuilder, Filter, Keys, Kind, Tag, Timestamp, ToBech32};
use std::{
    path::Path,
    process::{Command, Output, Stdio},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tempfile::TempDir;

#[derive(Clone)]
struct Fixture {
    events: Vec<Event>,
    late: Vec<Event>,
    delivered: Arc<AtomicUsize>,
    blobs: Option<Arc<git_remote_htree::git::storage::LocalStore>>,
}

async fn blob(
    State(state): State<Fixture>,
    axum::extract::Path(hash): axum::extract::Path<String>,
) -> impl axum::response::IntoResponse {
    let bytes = hashtree_core::from_hex(hash.strip_suffix(".bin").unwrap_or(&hash))
        .ok()
        .and_then(|hash| state.blobs.as_ref()?.get_sync(&hash).ok().flatten());
    match bytes {
        Some(bytes) => (axum::http::StatusCode::OK, bytes),
        None => (axum::http::StatusCode::NOT_FOUND, Vec::new()),
    }
}

async fn relay(State(state): State<Fixture>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |mut socket| async move {
        while let Some(Ok(message)) = socket.recv().await {
            let message = match message {
                Message::Text(message) => message,
                Message::Ping(payload) => {
                    if socket.send(Message::Pong(payload)).await.is_err() {
                        return;
                    }
                    continue;
                }
                Message::Close(_) => return,
                _ => continue,
            };
            let Ok(request) = serde_json::from_str::<serde_json::Value>(&message) else {
                continue;
            };
            if request[0] != "REQ" {
                continue;
            }
            let Some(parts) = request.as_array() else {
                continue;
            };
            let filters: Vec<Filter> = parts
                .iter()
                .skip(2)
                .filter_map(|v| serde_json::from_value(v.clone()).ok())
                .collect();
            for event in &state.events {
                if filters
                    .iter()
                    .any(|filter| filter.match_event(event, Default::default()))
                {
                    if socket
                        .send(Message::Text(
                            serde_json::json!(["EVENT", request[1], event]).to_string(),
                        ))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
            if socket
                .send(Message::Text(
                    serde_json::json!(["EOSE", request[1]]).to_string(),
                ))
                .await
                .is_err()
            {
                return;
            }
            // These events intentionally arrive only AFTER the initial result set.
            for event in &state.late {
                if filters
                    .iter()
                    .any(|filter| filter.match_event(event, Default::default()))
                {
                    if socket
                        .send(Message::Text(
                            serde_json::json!(["EVENT", request[1], event]).to_string(),
                        ))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    state.delivered.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    })
}

fn event(keys: &Keys, kind: Kind, content: &str, tags: Vec<Tag>, time: u64) -> Event {
    EventBuilder::new(kind, content)
        .tags(tags)
        .custom_created_at(Timestamp::from(time))
        .sign_with_keys(keys)
        .unwrap()
}

async fn fixture() -> (String, Keys, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let me = Keys::generate();
    let author = Keys::generate();
    let delivered = Arc::new(AtomicUsize::new(0));
    let state = Fixture {
        events: vec![
            event(
                &me,
                Kind::ContactList,
                "",
                vec![Tag::public_key(author.public_key())],
                1,
            ),
            event(&author, Kind::Metadata, r#"{"name":"alice"}"#, vec![], 1),
        ],
        late: vec![event(
            &author,
            Kind::Custom(30064),
            "",
            vec![
                Tag::identifier("project"),
                Tag::parse(["l", "hashtree"]).unwrap(),
                Tag::parse(["l", "git"]).unwrap(),
                Tag::parse(["hash", &"11".repeat(32)]).unwrap(),
            ],
            1,
        )],
        delivered: delivered.clone(),
        blobs: None,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/", get(relay))
                .route("/:hash", get(blob))
                .with_state(state),
        )
        .await
        .unwrap();
    });
    (url, me, delivered, server)
}

#[tokio::test]
async fn discovers_followed_publishers_and_roots_after_eose() {
    let (url, me, delivered, server) = fixture().await;
    let results = search_with(
        &Query::parse("alice/project/docs/readme.txt").unwrap(),
        true,
        SearchOptions {
            relays: vec![url],
            roots: vec![me.public_key()],
            window: Duration::from_secs(1),
            local_follows: None,
            local_mutes: vec![],
        },
    )
    .await
    .unwrap();
    server.abort();
    assert!(delivered.load(Ordering::Relaxed) > 0);
    assert_eq!(results.candidates.len(), 1);
    assert_eq!(results.candidates[0].distance, 1);
    assert_eq!(results.candidates[0].publisher, "alice");
    assert!(results.candidates[0]
        .url
        .ends_with("/project/docs/readme.txt"));
}

fn config(dir: &Path, relay: &str, root: &Keys) {
    std::fs::write(dir.join("config.toml"), format!("[nostr]\nrelays = [\"{relay}\"]\nsocialgraph_root = \"{}\"\n[updater]\nauto_check = false\n[blossom]\nread_servers = []\nwrite_servers = []\n", root.public_key().to_bech32().unwrap())).unwrap();
}

async fn command(bin: &str, args: &[&str], config: &Path) -> Output {
    tokio::process::Command::new(bin)
        .args(args)
        .env("HTREE_CONFIG_DIR", config)
        .env("HTREE_DATA_DIR", config.join("data"))
        .env_remove("NOSTR_RELAYS")
        .env_remove("HTREE_LOCAL_DAEMON_ONLY")
        .env("NOSTR_PREFER_LOCAL", "0")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .unwrap()
}

#[tokio::test]
async fn scripts_get_choices_without_downloading_or_cloning() {
    let (url, me, _, server) = fixture().await;
    let dir = TempDir::new().unwrap();
    config(dir.path(), &url, &me);
    let destination = dir.path().join("download");
    let output = command(
        env!("CARGO_BIN_EXE_htree"),
        &[
            "get",
            "project",
            "--non-interactive",
            "-o",
            destination.to_str().unwrap(),
        ],
        dir.path(),
    )
    .await;
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("htree get 'htree://npub1"), "{stderr}");
    assert!(!destination.exists());
    let output = command(
        env!("CARGO_BIN_EXE_htree"),
        &[
            "clone",
            "project",
            destination.to_str().unwrap(),
            "--non-interactive",
        ],
        dir.path(),
    )
    .await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("git clone 'htree://npub1"));
    assert!(!destination.exists());
    let output = command(
        env!("CARGO_BIN_EXE_htree"),
        &["search", "alice/project", "--git", "--wait", "1"],
        dir.path(),
    )
    .await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("git clone 'htree://npub1"));
    server.abort();
}

#[cfg(feature = "git-remote-wrapper")]
#[tokio::test]
async fn helper_suggestions_never_write_to_git_protocol_output() {
    let (url, me, _, server) = fixture().await;
    let dir = TempDir::new().unwrap();
    config(dir.path(), &url, &me);
    let output = command(
        env!("CARGO_BIN_EXE_git-remote-htree"),
        &["origin", "htree://project"],
        dir.path(),
    )
    .await;
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("git clone 'htree://npub1"), "{stderr}");
    assert!(stderr.contains("htree clone <name>"));
    server.abort();
}

#[test]
fn immutable_get_still_works_offline() {
    let dir = TempDir::new().unwrap();
    config(dir.path(), "ws://127.0.0.1:1", &Keys::generate());
    let input = dir.path().join("input.txt");
    let output = dir.path().join("output.txt");
    std::fs::write(&input, "hello content").unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_htree"))
            .args(args)
            .env("HTREE_CONFIG_DIR", dir.path())
            .env("HTREE_DATA_DIR", dir.path().join("data"))
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    let added = run(&["add", input.to_str().unwrap(), "--local"]);
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    let stdout = String::from_utf8(added.stdout).unwrap();
    let cid = stdout
        .split_whitespace()
        .find(|word| word.starts_with("nhash1"))
        .expect("CID");
    let downloaded = run(&[
        "get",
        cid,
        "-o",
        output.to_str().unwrap(),
        "--non-interactive",
    ]);
    assert!(
        downloaded.status.success(),
        "{}",
        String::from_utf8_lossy(&downloaded.stderr)
    );
    assert_eq!(std::fs::read_to_string(output).unwrap(), "hello content");
}

#[cfg(all(unix, feature = "git-remote-wrapper"))]
#[path = "social_discovery/terminal.rs"]
mod terminal;
