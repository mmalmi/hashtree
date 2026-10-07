//! Standalone Git root discovery through signed native indexes and live relays.

use anyhow::{Context, Result};
use axum::{
    extract::{ws::Message, Path, State, WebSocketUpgrade},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use git_remote_htree::nostr_client::{NostrClient, KIND_HASHTREE_ROOT};
use hashtree_config::Config;
use hashtree_core::{Cid, Hash, HashTree, HashTreeConfig, LinkType, MemoryStore, Store};
use hashtree_nostr::{stored_event_from_nostr_sdk_event, NostrEventStore};
use nostr::{Event, EventBuilder, Filter, Keys, Kind, Tag, Timestamp};
use std::{
    collections::{HashMap, HashSet},
    path::Path as FsPath,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{sync::watch, task::JoinHandle};

#[derive(Clone)]
struct Fixture {
    store: Arc<MemoryStore>,
    historical: Vec<Event>,
    after_eose: Vec<Event>,
    corrupt: HashSet<Hash>,
    reads: Arc<AtomicUsize>,
    delivered_after_eose: Arc<AtomicUsize>,
    shutdown: watch::Sender<bool>,
}

impl Fixture {
    fn new(store: Arc<MemoryStore>, historical: Vec<Event>) -> Self {
        let (shutdown, _) = watch::channel(false);
        Self {
            store,
            historical,
            after_eose: Vec::new(),
            corrupt: HashSet::new(),
            reads: Arc::new(AtomicUsize::new(0)),
            delivered_after_eose: Arc::new(AtomicUsize::new(0)),
            shutdown,
        }
    }
}

async fn relay(State(state): State<Fixture>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |mut socket| async move {
        let mut shutdown = state.shutdown.subscribe();
        loop {
            let message = tokio::select! {
                _ = shutdown.changed() => return,
                message = socket.recv() => message,
            };
            let Some(Ok(message)) = message else {
                return;
            };
            let text = match message {
                Message::Text(text) => text,
                Message::Ping(payload) => {
                    if socket.send(Message::Pong(payload)).await.is_err() {
                        return;
                    }
                    continue;
                }
                Message::Pong(_) | Message::Binary(_) => continue,
                Message::Close(_) => return,
            };
            let Ok(request) = serde_json::from_str::<serde_json::Value>(&text) else {
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
                .filter_map(|value| serde_json::from_value(value.clone()).ok())
                .collect();
            let matches = |event: &Event| {
                filters
                    .iter()
                    .any(|filter| filter.match_event(event, Default::default()))
            };
            let id = &request[1];
            for event in state.historical.iter().filter(|event| matches(event)) {
                if socket
                    .send(Message::Text(
                        serde_json::json!(["EVENT", id, event]).to_string(),
                    ))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            if socket
                .send(Message::Text(serde_json::json!(["EOSE", id]).to_string()))
                .await
                .is_err()
            {
                return;
            }
            // A historical-only fetch returns before these live observations.
            if !state.after_eose.is_empty() {
                tokio::select! {
                    _ = shutdown.changed() => return,
                    _ = tokio::time::sleep(Duration::from_millis(250)) => {},
                }
            }
            for event in state.after_eose.iter().filter(|event| matches(event)) {
                if socket
                    .send(Message::Text(
                        serde_json::json!(["EVENT", id, event]).to_string(),
                    ))
                    .await
                    .is_err()
                {
                    return;
                }
                state.delivered_after_eose.fetch_add(1, Ordering::SeqCst);
            }
        }
    })
}

async fn blob(State(state): State<Fixture>, Path(value): Path<String>) -> Response {
    state.reads.fetch_add(1, Ordering::SeqCst);
    let value = value.strip_suffix(".bin").unwrap_or(&value);
    let Some(hash) = hex::decode(value)
        .ok()
        .and_then(|bytes| <Hash>::try_from(bytes).ok())
    else {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    };
    if state.corrupt.contains(&hash) {
        return b"corrupt index block".to_vec().into_response();
    }
    match state.store.get(&hash).await {
        Ok(Some(bytes)) => bytes.into_response(),
        Ok(None) => axum::http::StatusCode::NOT_FOUND.into_response(),
        Err(_) => axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

struct Server {
    url: String,
    fixture: Fixture,
    task: Option<JoinHandle<()>>,
}

impl Server {
    async fn start(fixture: Fixture) -> Result<Self> {
        let app = Router::new()
            .route("/ws", get(relay))
            .route("/:hash", get(blob))
            .with_state(fixture.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}", listener.local_addr()?);
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve local fixture");
        });
        Ok(Self {
            url,
            fixture,
            task: Some(task),
        })
    }

    fn relay_url(&self) -> String {
        self.url.replacen("http:", "ws:", 1) + "/ws"
    }

    async fn stop(&mut self) {
        self.fixture.shutdown.send_replace(true);
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.fixture.shutdown.send_replace(true);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

fn config(data: &FsPath, server: &Server) -> Config {
    let mut config = Config::default();
    config.storage.data_dir = data.to_string_lossy().into_owned();
    // Keep automatic daemon detection away from another locally running app.
    config.server.bind_address = "127.0.0.1:1".to_string();
    config.nostr.relays = vec![server.relay_url()];
    config.blossom.servers.clear();
    config.blossom.read_servers = vec![server.url.clone()];
    config.blossom.write_servers.clear();
    config
}

fn root_event(keys: &Keys, name: &str, root: &Cid, created_at: u64) -> Result<Event> {
    let mut tags = vec![
        Tag::identifier(name),
        Tag::parse(["l", "hashtree"])?,
        Tag::parse(["hash", &hex::encode(root.hash)])?,
    ];
    if let Some(key) = root.key {
        tags.push(Tag::parse(["key", &hex::encode(key)])?);
    }
    Ok(EventBuilder::new(Kind::Custom(KIND_HASHTREE_ROOT), "")
        .tags(tags)
        .custom_created_at(Timestamp::from_secs(created_at))
        .sign_with_keys(keys)?)
}

async fn index_root(store: Arc<MemoryStore>, events: &[Event]) -> Result<Cid> {
    NostrEventStore::new(store)
        .build(None, events.iter().map(stored_event_from_nostr_sdk_event))
        .await?
        .context("fixture event index must have a root")
}

async fn git_root(store: Arc<MemoryStore>, commit: &str) -> Result<Cid> {
    let tree = HashTree::new(HashTreeConfig::new(store).public());
    let empty = tree.put_directory(vec![]).await?;
    let (value, size) = tree.put(format!("{commit}\n").as_bytes()).await?;
    let heads = tree
        .set_entry(&empty, &[], "main", &value, size, LinkType::Blob)
        .await?;
    let refs = tree
        .set_entry(&empty, &[], "heads", &heads, 0, LinkType::Dir)
        .await?;
    let git = tree
        .set_entry(&empty, &[], "refs", &refs, 0, LinkType::Dir)
        .await?;
    Ok(tree
        .set_entry(&empty, &[], ".git", &git, 0, LinkType::Dir)
        .await?)
}

type FetchedRefs = (HashMap<String, String>, Option<String>, Option<[u8; 32]>);

async fn fetch(config: Config, author: &Keys, name: &str) -> Result<FetchedRefs> {
    let author = author.public_key().to_hex();
    let name = name.to_string();
    tokio::task::spawn_blocking(move || {
        NostrClient::new(&author, None, None, false, &config)?.fetch_refs_with_root(&name)
    })
    .await?
}

#[tokio::test]
async fn repository_root_is_discovered_only_from_an_independent_native_index() -> Result<()> {
    let author = Keys::generate();
    let indexer = Keys::generate();
    assert_ne!(author.public_key(), indexer.public_key());
    let store = Arc::new(MemoryStore::new());
    let commit = "11".repeat(20);
    let root = git_root(store.clone(), &commit).await?;
    let event = root_event(&author, "index-only", &root, 100)?;
    let index = index_root(store.clone(), &[event]).await?;
    // This is the daemon mirror format: no l=nostr-event-index label.
    let advertisement = root_event(&indexer, "nostr-event-index", &index, 200)?;
    let server = Server::start(Fixture::new(store, vec![advertisement])).await?;
    let data = tempfile::tempdir()?;
    let (refs, resolved, key) = fetch(config(data.path(), &server), &author, "index-only").await?;
    assert_eq!(refs.get("refs/heads/main"), Some(&commit));
    assert_eq!(resolved, Some(hex::encode(root.hash)));
    assert_eq!(key, root.key);
    assert!(server.fixture.reads.load(Ordering::SeqCst) > 0);
    Ok(())
}

#[tokio::test]
async fn delayed_newer_repository_root_after_eose_wins_over_the_historical_root() -> Result<()> {
    let author = Keys::generate();
    let store = Arc::new(MemoryStore::new());
    let old_root = git_root(store.clone(), &"22".repeat(20)).await?;
    let commit = "33".repeat(20);
    let newer_root = git_root(store.clone(), &commit).await?;
    let mut fixture = Fixture::new(
        store,
        vec![root_event(&author, "live-root", &old_root, 100)?],
    );
    fixture.after_eose = vec![root_event(&author, "live-root", &newer_root, 200)?];
    let server = Server::start(fixture).await?;
    let data = tempfile::tempdir()?;
    let (refs, resolved, _) = fetch(config(data.path(), &server), &author, "live-root").await?;
    assert!(server.fixture.delivered_after_eose.load(Ordering::SeqCst) > 0);
    assert_eq!(resolved, Some(hex::encode(newer_root.hash)));
    assert_eq!(refs.get("refs/heads/main"), Some(&commit));
    Ok(())
}

#[tokio::test]
async fn a_new_client_resolves_cached_signed_roots_after_external_services_stop() -> Result<()> {
    let author = Keys::generate();
    let store = Arc::new(MemoryStore::new());
    let root = git_root(store.clone(), &"44".repeat(20)).await?;
    let event = root_event(&author, "cached-root", &root, 100)?;
    let mut server = Server::start(Fixture::new(store, vec![event])).await?;
    let data = tempfile::tempdir()?;
    let mut config = config(data.path(), &server);
    let (_, resolved, _) = fetch(config.clone(), &author, "cached-root").await?;
    assert_eq!(resolved, Some(hex::encode(root.hash)));
    assert!(data.path().join("git-root-events/head.nhash").is_file());
    server.stop().await;
    config.nostr.relays.clear();
    // A new client has no in-memory refs. Git content is intentionally offline:
    // reaching this stage proves the persisted event cache resolved its root.
    let author = author.public_key().to_hex();
    let (error, cached_root) = tokio::task::spawn_blocking(move || -> Result<_> {
        let mut reader = NostrClient::new(&author, None, None, false, &config)?;
        let error = reader
            .fetch_refs_with_root("cached-root")
            .unwrap_err()
            .to_string();
        let cached_root = reader.get_cached_root_hash("cached-root").cloned();
        Ok((error, cached_root))
    })
    .await??;
    assert_eq!(cached_root, Some(hex::encode(root.hash)));
    assert!(
        error.contains("Failed to download root hash"),
        "cached root must survive offline lookup: {error}"
    );
    assert!(error.contains(&hex::encode(root.hash)[..12]));
    Ok(())
}

#[tokio::test]
async fn corrupt_and_empty_indexes_do_not_hide_an_independent_matching_index() -> Result<()> {
    let author = Keys::generate();
    let store = Arc::new(MemoryStore::new());
    let commit = "55".repeat(20);
    let root = git_root(store.clone(), &commit).await?;
    let wanted = root_event(&author, "additive-indexes", &root, 100)?;
    let good = index_root(store.clone(), &[wanted]).await?;
    let irrelevant = root_event(&author, "different-repository", &root, 200)?;
    let empty = index_root(store.clone(), &[irrelevant]).await?;
    let bad_event = root_event(&author, "different-repository", &root, 300)?;
    let corrupt = index_root(store.clone(), &[bad_event]).await?;
    let advertisements = vec![
        root_event(&Keys::generate(), "nostr-event-index", &corrupt, 600)?,
        root_event(&Keys::generate(), "nostr-event-index", &empty, 500)?,
        root_event(&Keys::generate(), "nostr-event-index", &good, 400)?,
    ];
    let mut fixture = Fixture::new(store, advertisements);
    fixture.corrupt.insert(corrupt.hash);
    let server = Server::start(fixture).await?;
    let data = tempfile::tempdir()?;
    let (refs, resolved, _) =
        fetch(config(data.path(), &server), &author, "additive-indexes").await?;
    assert_eq!(resolved, Some(hex::encode(root.hash)));
    assert_eq!(refs.get("refs/heads/main"), Some(&commit));
    Ok(())
}

#[tokio::test]
async fn direct_relay_root_resolves_with_a_malformed_cache_head() -> Result<()> {
    let author = Keys::generate();
    let store = Arc::new(MemoryStore::new());
    let commit = "66".repeat(20);
    let root = git_root(store.clone(), &commit).await?;
    let event = root_event(&author, "malformed-cache-head", &root, 100)?;
    let server = Server::start(Fixture::new(store, vec![event])).await?;
    let data = tempfile::tempdir()?;
    let cache = data.path().join("git-root-events");
    std::fs::create_dir_all(&cache)?;
    std::fs::write(cache.join("head.nhash"), "malformed cache checkpoint")?;

    let (refs, resolved, _) = fetch(
        config(data.path(), &server),
        &author,
        "malformed-cache-head",
    )
    .await?;
    assert_eq!(resolved, Some(hex::encode(root.hash)));
    assert_eq!(refs.get("refs/heads/main"), Some(&commit));
    Ok(())
}

#[tokio::test]
async fn direct_relay_root_resolves_when_cache_checkpoint_replacement_fails() -> Result<()> {
    let author = Keys::generate();
    let store = Arc::new(MemoryStore::new());
    let commit = "77".repeat(20);
    let root = git_root(store.clone(), &commit).await?;
    let event = root_event(&author, "blocked-cache-checkpoint", &root, 100)?;
    let server = Server::start(Fixture::new(store, vec![event])).await?;
    let data = tempfile::tempdir()?;
    let head = data.path().join("git-root-events/head.nhash");
    std::fs::create_dir_all(&head)?;

    let (refs, resolved, _) = fetch(
        config(data.path(), &server),
        &author,
        "blocked-cache-checkpoint",
    )
    .await?;
    assert_eq!(resolved, Some(hex::encode(root.hash)));
    assert_eq!(refs.get("refs/heads/main"), Some(&commit));
    assert!(
        head.is_dir(),
        "the blocking checkpoint directory is preserved"
    );
    Ok(())
}

#[tokio::test]
async fn exceeded_live_observation_budget_is_reported_as_incomplete() -> Result<()> {
    let author = Keys::generate();
    let indexer = Keys::generate();
    let store = Arc::new(MemoryStore::new());
    let root = git_root(store.clone(), &"88".repeat(20)).await?;
    let unrelated = root_event(&author, "another-repository", &root, 100)?;
    let index = index_root(store.clone(), &[unrelated]).await?;
    let mut fixture = Fixture::new(store, vec![]);
    // Exceed the 256-event observation budget after EOSE, where historical
    // filter limits cannot conceal a noncompliant relay's live event flood.
    // One publisher/tree and one healthy index keep index-count limits separate.
    fixture.after_eose = (0..300)
        .map(|number| root_event(&indexer, "nostr-event-index", &index, 200 + number))
        .collect::<Result<Vec<_>>>()?;
    let server = Server::start(fixture).await?;
    let data = tempfile::tempdir()?;

    let error = fetch(
        config(data.path(), &server),
        &author,
        "unobserved-overflow-root",
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        server.fixture.delivered_after_eose.load(Ordering::SeqCst) > 256,
        "fixture must send enough unique events to exceed the observation budget"
    );
    assert!(
        error.contains("incomplete"),
        "known event loss must be reported: {error}"
    );
    assert!(
        error.contains("budget"),
        "the exhausted budget must explain the failure: {error}"
    );
    assert!(
        !error.contains("No repository root observed"),
        "known event loss must not become a quiet repository miss: {error}"
    );
    Ok(())
}

#[tokio::test]
async fn exceeded_independent_index_budget_is_reported_as_incomplete() -> Result<()> {
    let author = Keys::generate();
    let store = Arc::new(MemoryStore::new());
    let root = git_root(store.clone(), &"99".repeat(20)).await?;
    let unrelated = root_event(&author, "another-repository", &root, 100)?;
    let index = index_root(store.clone(), &[unrelated]).await?;
    let mut fixture = Fixture::new(store, vec![]);
    // Nine independent publishers exceed the eight-index lookup budget while
    // staying well within the separate event observation budget.
    fixture.after_eose = (0..9)
        .map(|number| root_event(&Keys::generate(), "nostr-event-index", &index, 200 + number))
        .collect::<Result<Vec<_>>>()?;
    let server = Server::start(fixture).await?;
    let data = tempfile::tempdir()?;

    let error = fetch(
        config(data.path(), &server),
        &author,
        "unobserved-index-budget-root",
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        server.fixture.delivered_after_eose.load(Ordering::SeqCst) >= 9,
        "fixture must advertise every independent index"
    );
    assert!(
        error.contains("incomplete"),
        "skipped sources must be reported: {error}"
    );
    assert!(
        error.contains("index") && error.contains("budget"),
        "the index budget must explain the failure: {error}"
    );
    assert!(
        !error.contains("No repository root observed"),
        "skipped indexes must not become a quiet repository miss: {error}"
    );
    Ok(())
}
