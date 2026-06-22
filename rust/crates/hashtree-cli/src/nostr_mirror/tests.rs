use super::*;
use axum::{
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
    routing::{head, put},
    Router,
};
use futures::{SinkExt, StreamExt};
use hashtree_resolver::RootResolver;
use nostr::{EventBuilder, JsonUtil, Tag};
use nostr_sdk::ToBech32;
use std::collections::{HashMap, HashSet};
use std::net::TcpListener;
use std::sync::Mutex;
use tempfile::TempDir;
use tokio::net::TcpStream;
use tokio::sync::{broadcast, oneshot, Notify};
use tokio_tungstenite::{accept_async, tungstenite::Message};

use crate::socialgraph::{open_social_graph_store_with_storage, set_social_graph_root};

macro_rules! event_builder {
    ($kind:expr, $content:expr $(,)?) => {
        EventBuilder::new($kind, $content)
    };
    ($kind:expr, $content:expr, $tags:expr $(,)?) => {
        EventBuilder::new($kind, $content).tags($tags)
    };
}

struct TestRelay {
    url: String,
    shutdown: broadcast::Sender<()>,
    events: Arc<Mutex<Vec<Event>>>,
    broadcaster: broadcast::Sender<Event>,
    request_count: Arc<std::sync::atomic::AtomicUsize>,
}

impl TestRelay {
    fn new(events: Vec<Event>) -> Self {
        let events = Arc::new(Mutex::new(events));
        let (shutdown, _) = broadcast::channel(1);
        let (broadcaster, _) = broadcast::channel(32);
        let request_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let std_listener = TcpListener::bind("127.0.0.1:0").expect("bind relay");
        let port = std_listener.local_addr().expect("relay addr").port();
        std_listener
            .set_nonblocking(true)
            .expect("listener nonblocking");

        let events_for_thread = Arc::clone(&events);
        let shutdown_for_thread = shutdown.clone();
        let broadcaster_for_thread = broadcaster.clone();
        let request_count_for_thread = Arc::clone(&request_count);
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("runtime");
            runtime.block_on(async move {
                let listener =
                    tokio::net::TcpListener::from_std(std_listener).expect("tokio listener");
                let mut shutdown_rx = shutdown_for_thread.subscribe();

                loop {
                    tokio::select! {
                        _ = shutdown_rx.recv() => break,
                        accept = listener.accept() => {
                            if let Ok((stream, _)) = accept {
                                let events = Arc::clone(&events_for_thread);
                                let broadcaster = broadcaster_for_thread.clone();
                                let request_count = Arc::clone(&request_count_for_thread);
                                tokio::spawn(async move {
                                    handle_connection(stream, events, broadcaster, request_count)
                                        .await;
                                });
                            }
                        }
                    }
                }
            });
        });

        std::thread::sleep(Duration::from_millis(100));

        Self {
            url: format!("ws://127.0.0.1:{port}"),
            shutdown,
            events,
            broadcaster,
            request_count,
        }
    }

    fn url(&self) -> String {
        self.url.clone()
    }

    fn publish(&self, event: Event) {
        self.events
            .lock()
            .expect("relay events")
            .push(event.clone());
        let _ = self.broadcaster.send(event);
    }

    fn request_count(&self) -> usize {
        self.request_count
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Drop for TestRelay {
    fn drop(&mut self) {
        let _ = self.shutdown.send(());
        std::thread::sleep(Duration::from_millis(50));
    }
}

struct TestBlossom {
    base_url: String,
    uploaded_hashes: Arc<Mutex<HashSet<String>>>,
    put_holds: Arc<Mutex<HashMap<String, Arc<Notify>>>>,
    put_started: Arc<Mutex<HashSet<String>>>,
    shutdown: Option<oneshot::Sender<()>>,
}

impl TestBlossom {
    async fn new() -> Self {
        let uploaded_hashes = Arc::new(Mutex::new(HashSet::new()));
        let put_holds = Arc::new(Mutex::new(HashMap::new()));
        let put_started = Arc::new(Mutex::new(HashSet::new()));
        let std_listener = TcpListener::bind("127.0.0.1:0").expect("bind blossom");
        let port = std_listener.local_addr().expect("blossom addr").port();
        std_listener
            .set_nonblocking(true)
            .expect("blossom listener nonblocking");
        let (shutdown, shutdown_rx) = oneshot::channel();
        let state = Arc::clone(&uploaded_hashes);
        let holds = Arc::clone(&put_holds);
        let started = Arc::clone(&put_started);

        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("build blossom test runtime");
            runtime.block_on(async move {
                let listener =
                    tokio::net::TcpListener::from_std(std_listener).expect("tokio listener");
                let app =
                    Router::new()
                        .route(
                            "/upload",
                            put(
                                |State(state): State<TestBlossomState>,
                                 headers: axum::http::HeaderMap,
                                 _body: Bytes| async move {
                                    let Some(hash) = headers
                                        .get("x-sha-256")
                                        .and_then(|value| value.to_str().ok())
                                        .map(|value| value.to_lowercase())
                                    else {
                                        return StatusCode::BAD_REQUEST;
                                    };
                                    state
                                        .put_started
                                        .lock()
                                        .expect("put started")
                                        .insert(hash.clone());
                                    let hold = state
                                        .put_holds
                                        .lock()
                                        .expect("put holds")
                                        .get(&hash)
                                        .cloned();
                                    if let Some(hold) = hold {
                                        hold.notified().await;
                                    }
                                    state
                                        .uploaded_hashes
                                        .lock()
                                        .expect("uploaded hashes")
                                        .insert(hash);
                                    StatusCode::CREATED
                                },
                            ),
                        )
                        .route(
                            "/:hash.bin",
                            head(
                                |State(state): State<TestBlossomState>,
                                 Path(hash): Path<String>| async move {
                                    if state
                                        .uploaded_hashes
                                        .lock()
                                        .expect("uploaded hashes")
                                        .contains(&hash.to_lowercase())
                                    {
                                        StatusCode::OK
                                    } else {
                                        StatusCode::NOT_FOUND
                                    }
                                },
                            ),
                        )
                        .with_state(TestBlossomState {
                            uploaded_hashes: state,
                            put_holds: holds,
                            put_started: started,
                        });

                let server = axum::serve(listener, app).with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                });
                let _ = server.await;
            });
        });
        std::thread::sleep(Duration::from_millis(100));

        Self {
            base_url: format!("http://127.0.0.1:{port}"),
            uploaded_hashes,
            put_holds,
            put_started,
            shutdown: Some(shutdown),
        }
    }

    fn base_url(&self) -> String {
        self.base_url.clone()
    }

    fn has_hash(&self, hash: &str) -> bool {
        self.uploaded_hashes
            .lock()
            .expect("uploaded hashes")
            .contains(&hash.to_lowercase())
    }

    fn hold_put(&self, hash: &str) {
        self.put_holds
            .lock()
            .expect("put holds")
            .insert(hash.to_lowercase(), Arc::new(Notify::new()));
    }

    fn put_started(&self, hash: &str) -> bool {
        self.put_started
            .lock()
            .expect("put started")
            .contains(&hash.to_lowercase())
    }

    fn release_put(&self, hash: &str) {
        if let Some(hold) = self
            .put_holds
            .lock()
            .expect("put holds")
            .remove(&hash.to_lowercase())
        {
            hold.notify_waiters();
        }
    }
}

#[derive(Clone)]
struct TestBlossomState {
    uploaded_hashes: Arc<Mutex<HashSet<String>>>,
    put_holds: Arc<Mutex<HashMap<String, Arc<Notify>>>>,
    put_started: Arc<Mutex<HashSet<String>>>,
}

impl Drop for TestBlossom {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

fn published_root_event_count(relay: &TestRelay, tree_name: &str) -> usize {
    relay
        .events
        .lock()
        .expect("relay events")
        .iter()
        .filter(|event| {
            event.kind == Kind::Custom(HASHTREE_ROOT_KIND as u16)
                && event.tags.iter().any(|tag| {
                    let values = tag.as_slice();
                    values.first().is_some_and(|value| value == "d")
                        && values.get(1).is_some_and(|value| value == tree_name)
                })
        })
        .count()
}

fn latest_published_root_event(relay: &TestRelay, tree_name: &str) -> Option<Event> {
    relay
        .events
        .lock()
        .expect("relay events")
        .iter()
        .filter(|event| {
            event.kind == Kind::Custom(HASHTREE_ROOT_KIND as u16)
                && event.tags.iter().any(|tag| {
                    let values = tag.as_slice();
                    values.first().is_some_and(|value| value == "d")
                        && values.get(1).is_some_and(|value| value == tree_name)
                })
        })
        .max_by_key(|event| (event.created_at, event.id))
        .cloned()
}

async fn handle_connection(
    stream: TcpStream,
    events: Arc<Mutex<Vec<Event>>>,
    broadcaster: broadcast::Sender<Event>,
    request_count: Arc<std::sync::atomic::AtomicUsize>,
) {
    let ws = match accept_async(stream).await {
        Ok(ws) => ws,
        Err(_) => return,
    };
    let (mut write, mut read) = ws.split();
    let mut subscriptions = HashMap::<String, Vec<nostr::Filter>>::new();
    let mut event_rx = broadcaster.subscribe();
    loop {
        tokio::select! {
            maybe_message = read.next() => {
                let Some(message) = maybe_message else {
                    break;
                };
                let text = match message {
                    Ok(Message::Text(text)) => text,
                    Ok(Message::Ping(data)) => {
                        let _ = write.send(Message::Pong(data)).await;
                        continue;
                    }
                    Ok(Message::Close(_)) => break,
                    _ => continue,
                };

                let parsed = match nostr::ClientMessage::from_json(text.as_bytes()) {
                    Ok(message) => message,
                    Err(_) => continue,
                };

                match parsed {
                    nostr::ClientMessage::Req {
                        subscription_id,
                        filters,
                    } => {
                        let subscription_id = subscription_id.into_owned();
                        let filters = filters
                            .into_iter()
                            .map(|filter| filter.into_owned())
                            .collect::<Vec<_>>();
                        request_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        subscriptions.insert(subscription_id.to_string(), filters.clone());
                        let current = events.lock().expect("relay events").clone();
                        for event in current {
                            if filters.iter().any(|filter| filter.match_event(&event, Default::default())) {
                                let _ = write
                                    .send(Message::Text(
                                        nostr::RelayMessage::event(subscription_id.clone(), event)
                                            .as_json(),
                                    ))
                                    .await;
                            }
                        }
                        let _ = write
                            .send(Message::Text(
                                nostr::RelayMessage::eose(subscription_id).as_json(),
                            ))
                            .await;
                    }
                    nostr::ClientMessage::Close(subscription_id) => {
                        let subscription_id = subscription_id.into_owned();
                        subscriptions.remove(&subscription_id.to_string());
                        let _ = write
                            .send(Message::Text(
                                nostr::RelayMessage::closed(subscription_id, "").as_json(),
                            ))
                            .await;
                    }
                    nostr::ClientMessage::Event(event) => {
                        let event = event.into_owned();
                        events.lock().expect("relay events").push(event.clone());
                        let _ = broadcaster.send(event.clone());
                        let _ = write
                            .send(Message::Text(
                                nostr::RelayMessage::ok(event.id, true, "").as_json(),
                            ))
                            .await;
                    }
                    _ => {}
                }
            }
            Ok(event) = event_rx.recv() => {
                for (subscription_id, filters) in &subscriptions {
                    if filters.iter().any(|filter| filter.match_event(&event, Default::default())) {
                        let _ = write
                            .send(Message::Text(
                                nostr::RelayMessage::event(
                                    nostr::SubscriptionId::new(subscription_id.clone()),
                                    event.clone(),
                                )
                                .as_json(),
                            ))
                            .await;
                    }
                }
            }
        }
    }
}

async fn wait_until<F>(label: &str, timeout: Duration, condition: F)
where
    F: FnMut() -> bool,
{
    if wait_until_bool(timeout, condition).await {
        return;
    }
    panic!("{label}: condition not met within {:?}", timeout);
}

async fn wait_until_bool<F>(timeout: Duration, mut condition: F) -> bool
where
    F: FnMut() -> bool,
{
    let started = std::time::Instant::now();
    while started.elapsed() < timeout {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

#[tokio::test]
async fn apply_history_root_updates_profile_index() -> Result<()> {
    let _guard = crate::socialgraph::test_lock();
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(HashtreeStore::new(tmp.path())?);
    let graph_store = open_social_graph_store_with_storage(
        tmp.path(),
        store.store_arc(),
        Some(64 * 1024 * 1024),
    )?;

    let root_keys = nostr::Keys::generate();
    let root_pubkey = root_keys.public_key().to_bytes();
    set_social_graph_root(&graph_store, &root_pubkey);

    let alice_keys = nostr::Keys::generate();
    let root_contacts = event_builder!(
        Kind::ContactList,
        "",
        vec![Tag::public_key(alice_keys.public_key())],
    )
    .custom_created_at(Timestamp::from(10))
    .sign_with_keys(&root_keys)
    .expect("root contacts");
    socialgraph::ingest_parsed_event(graph_store.as_ref(), &root_contacts)?;

    let alice_profile = event_builder!(Kind::Metadata, r#"{"name":"Alice Mirror"}"#)
        .custom_created_at(Timestamp::from(11))
        .sign_with_keys(&alice_keys)
        .expect("alice profile");
    let stored = hashtree_nostr::StoredNostrEvent {
        id: alice_profile.id.to_hex(),
        pubkey: alice_profile.pubkey.to_hex(),
        created_at: alice_profile.created_at.as_secs(),
        kind: alice_profile.kind.as_u16() as u32,
        tags: alice_profile
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect(),
        content: alice_profile.content.clone(),
        sig: alice_profile.sig.to_string(),
    };
    let event_store = NostrEventStore::new(store.store_arc());
    let root = event_store.build(None, vec![stored]).await?;
    let mirror = BackgroundNostrMirror::new(
        NostrMirrorConfig::default(),
        store,
        graph_store.clone(),
        None,
    )
    .await?;
    mirror.apply_history_root(root.as_ref()).await?;

    let alice_hex = alice_keys.public_key().to_hex();
    assert!(graph_store.latest_profile_event(&alice_hex)?.is_some());
    assert!(graph_store.profile_search_root()?.is_some());
    Ok(())
}

#[tokio::test]
async fn apply_history_root_publishes_profile_search_tree() -> Result<()> {
    let _guard = crate::socialgraph::test_lock();
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(HashtreeStore::new(tmp.path())?);
    let graph_store = open_social_graph_store_with_storage(
        tmp.path(),
        store.store_arc(),
        Some(64 * 1024 * 1024),
    )?;

    let root_keys = nostr::Keys::generate();
    let root_pubkey = root_keys.public_key().to_bytes();
    set_social_graph_root(&graph_store, &root_pubkey);

    let alice_keys = nostr::Keys::generate();
    let alice_profile = event_builder!(Kind::Metadata, r#"{"name":"Alice Published Search"}"#)
        .custom_created_at(Timestamp::from(11))
        .sign_with_keys(&alice_keys)
        .expect("alice profile");
    let stored = hashtree_nostr::StoredNostrEvent {
        id: alice_profile.id.to_hex(),
        pubkey: alice_profile.pubkey.to_hex(),
        created_at: alice_profile.created_at.as_secs(),
        kind: alice_profile.kind.as_u16() as u32,
        tags: alice_profile
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect(),
        content: alice_profile.content.clone(),
        sig: alice_profile.sig.to_string(),
    };
    let event_store = NostrEventStore::new(store.store_arc());
    let root = event_store.build(None, vec![stored]).await?;

    let relay = TestRelay::new(Vec::new());
    let publish_keys = nostr_sdk::Keys::parse(&root_keys.secret_key().to_bech32()?)
        .context("parse mirror publish keys")?;
    let mirror = BackgroundNostrMirror::new(
        NostrMirrorConfig {
            relays: vec![relay.url()],
            publish_relays: vec![relay.url()],
            history_sync_on_start: false,
            published_profile_search_tree_name: Some("profile-search".to_string()),
            ..NostrMirrorConfig::default()
        },
        store,
        graph_store.clone(),
        Some(publish_keys),
    )
    .await?;

    let connected_started = std::time::Instant::now();
    while connected_started.elapsed() < Duration::from_secs(5) {
        if mirror.has_connected_publish_relay().await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        mirror.has_connected_publish_relay().await,
        "publisher relay should connect"
    );
    mirror.apply_history_root(root.as_ref()).await?;
    tokio::time::sleep(MIRROR_ROOT_PUBLISH_DEBOUNCE + Duration::from_millis(20)).await;
    mirror.maybe_publish_profile_search_root(false).await?;

    let resolver = crate::NostrRootResolver::new(crate::NostrResolverConfig {
        relays: vec![relay.url()],
        resolve_timeout: Duration::from_secs(2),
        secret_key: None,
    })
    .await?;
    let npub = root_keys.public_key().to_bech32()?;
    let resolved = resolver
        .resolve(&format!("{npub}/profile-search"))
        .await?
        .expect("published profile-search root");

    assert_eq!(
        resolved,
        graph_store.profile_search_root()?.expect("search root")
    );
    resolver.stop().await?;
    Ok(())
}

#[tokio::test]
async fn apply_history_root_publishes_profiles_by_pubkey_tree() -> Result<()> {
    let _guard = crate::socialgraph::test_lock();
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(HashtreeStore::new(tmp.path())?);
    let graph_store = open_social_graph_store_with_storage(
        tmp.path(),
        store.store_arc(),
        Some(64 * 1024 * 1024),
    )?;

    let root_keys = nostr::Keys::generate();
    let root_pubkey = root_keys.public_key().to_bytes();
    set_social_graph_root(&graph_store, &root_pubkey);

    let alice_keys = nostr::Keys::generate();
    let alice_profile = event_builder!(
        Kind::Metadata,
        r#"{"name":"Alice Published Profile Tree"}"#,
        [],
    )
    .custom_created_at(Timestamp::from(11))
    .sign_with_keys(&alice_keys)
    .expect("alice profile");
    let stored = hashtree_nostr::StoredNostrEvent {
        id: alice_profile.id.to_hex(),
        pubkey: alice_profile.pubkey.to_hex(),
        created_at: alice_profile.created_at.as_secs(),
        kind: alice_profile.kind.as_u16() as u32,
        tags: alice_profile
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect(),
        content: alice_profile.content.clone(),
        sig: alice_profile.sig.to_string(),
    };
    let event_store = NostrEventStore::new(store.store_arc());
    let root = event_store.build(None, vec![stored]).await?;

    let relay = TestRelay::new(Vec::new());
    let publish_keys = nostr_sdk::Keys::parse(&root_keys.secret_key().to_bech32()?)
        .context("parse mirror publish keys")?;
    let mirror = BackgroundNostrMirror::new(
        NostrMirrorConfig {
            relays: vec![relay.url()],
            publish_relays: vec![relay.url()],
            history_sync_on_start: false,
            published_profiles_by_pubkey_tree_name: Some("profiles-by-pubkey".to_string()),
            ..NostrMirrorConfig::default()
        },
        store,
        graph_store.clone(),
        Some(publish_keys),
    )
    .await?;

    let connected_started = std::time::Instant::now();
    while connected_started.elapsed() < Duration::from_secs(5) {
        if mirror.has_connected_publish_relay().await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        mirror.has_connected_publish_relay().await,
        "publisher relay should connect"
    );
    mirror.apply_history_root(root.as_ref()).await?;
    tokio::time::sleep(MIRROR_ROOT_PUBLISH_DEBOUNCE + Duration::from_millis(20)).await;
    mirror.maybe_publish_profiles_by_pubkey_root(false).await?;

    let resolver = crate::NostrRootResolver::new(crate::NostrResolverConfig {
        relays: vec![relay.url()],
        resolve_timeout: Duration::from_secs(2),
        secret_key: None,
    })
    .await?;
    let npub = root_keys.public_key().to_bech32()?;
    let resolved = resolver
        .resolve(&format!("{npub}/profiles-by-pubkey"))
        .await?
        .expect("published profiles-by-pubkey root");

    assert_eq!(
        resolved,
        graph_store
            .profiles_by_pubkey_root()?
            .expect("profiles-by-pubkey root")
    );
    resolver.stop().await?;
    Ok(())
}

#[tokio::test]
async fn apply_history_root_uploads_profile_search_root_to_blossom_before_publish() -> Result<()> {
    let _guard = crate::socialgraph::test_lock();
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(HashtreeStore::new(tmp.path())?);
    let graph_store = open_social_graph_store_with_storage(
        tmp.path(),
        store.store_arc(),
        Some(64 * 1024 * 1024),
    )?;

    let root_keys = nostr::Keys::generate();
    let root_pubkey = root_keys.public_key().to_bytes();
    set_social_graph_root(&graph_store, &root_pubkey);

    let alice_keys = nostr::Keys::generate();
    let alice_profile = event_builder!(Kind::Metadata, r#"{"name":"Alice Uploaded Search"}"#)
        .custom_created_at(Timestamp::from(11))
        .sign_with_keys(&alice_keys)
        .expect("alice profile");
    let stored = hashtree_nostr::StoredNostrEvent {
        id: alice_profile.id.to_hex(),
        pubkey: alice_profile.pubkey.to_hex(),
        created_at: alice_profile.created_at.as_secs(),
        kind: alice_profile.kind.as_u16() as u32,
        tags: alice_profile
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect(),
        content: alice_profile.content.clone(),
        sig: alice_profile.sig.to_string(),
    };
    let event_store = NostrEventStore::new(store.store_arc());
    let root = event_store.build(None, vec![stored]).await?;

    let relay = TestRelay::new(Vec::new());
    let blossom = TestBlossom::new().await;
    let publish_keys = nostr_sdk::Keys::parse(&root_keys.secret_key().to_bech32()?)
        .context("parse mirror publish keys")?;
    let mirror = BackgroundNostrMirror::new(
        NostrMirrorConfig {
            relays: vec![relay.url()],
            publish_relays: vec![relay.url()],
            blossom_write_servers: vec![blossom.base_url()],
            history_sync_on_start: false,
            published_profile_search_tree_name: Some("profile-search".to_string()),
            ..NostrMirrorConfig::default()
        },
        store,
        graph_store.clone(),
        Some(publish_keys),
    )
    .await?;

    let connected_started = std::time::Instant::now();
    while connected_started.elapsed() < Duration::from_secs(5) {
        if mirror.has_connected_publish_relay().await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        mirror.has_connected_publish_relay().await,
        "publisher relay should connect"
    );

    mirror.apply_history_root(root.as_ref()).await?;
    tokio::time::sleep(MIRROR_ROOT_PUBLISH_DEBOUNCE + Duration::from_millis(20)).await;
    mirror.maybe_publish_profile_search_root(false).await?;

    let published_root = graph_store
        .profile_search_root()?
        .expect("profile-search root");
    wait_until("profile-search DAG upload", Duration::from_secs(5), || {
        mirror
            .profile_search_publish_state
            .lock()
            .expect("profile-search publish state")
            .last_uploaded_root
            .as_ref()
            == Some(&published_root)
    })
    .await;
    assert!(
        blossom.has_hash(&hex::encode(published_root.hash)),
        "expected profile-search DAG root blob to be uploaded before publish"
    );
    mirror.maybe_publish_profile_search_root(false).await?;
    assert_eq!(published_root_event_count(&relay, "profile-search"), 1);
    Ok(())
}

#[tokio::test]
async fn apply_history_root_holds_event_root_until_event_upload_finishes() -> Result<()> {
    let _guard = crate::socialgraph::test_lock();
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(HashtreeStore::new(tmp.path())?);
    let graph_store = open_social_graph_store_with_storage(
        tmp.path(),
        store.store_arc(),
        Some(64 * 1024 * 1024),
    )?;

    let root_keys = nostr::Keys::generate();
    let root_pubkey = root_keys.public_key().to_bytes();
    set_social_graph_root(&graph_store, &root_pubkey);

    let alice_keys = nostr::Keys::generate();
    let alice_profile = event_builder!(
        Kind::Metadata,
        r#"{"name":"Alice Concurrent Publish","picture":"https://example.com/alice.png"}"#,
        [],
    )
    .custom_created_at(Timestamp::from(11))
    .sign_with_keys(&alice_keys)
    .expect("alice profile");
    let stored = hashtree_nostr::StoredNostrEvent {
        id: alice_profile.id.to_hex(),
        pubkey: alice_profile.pubkey.to_hex(),
        created_at: alice_profile.created_at.as_secs(),
        kind: alice_profile.kind.as_u16() as u32,
        tags: alice_profile
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect(),
        content: alice_profile.content.clone(),
        sig: alice_profile.sig.to_string(),
    };
    let event_store = NostrEventStore::new(store.store_arc());
    let root = event_store.build(None, vec![stored]).await?;

    let relay = TestRelay::new(Vec::new());
    let blossom = TestBlossom::new().await;
    let delayed_hashes =
        crate::blossom_push::collect_cids_for_push(&store, root.clone().expect("event root"), None)
            .await?
            .into_iter()
            .map(|cid| hex::encode(cid.hash))
            .collect::<Vec<_>>();
    for hash in &delayed_hashes {
        blossom.hold_put(hash);
    }
    let publish_keys = nostr_sdk::Keys::parse(&root_keys.secret_key().to_bech32()?)
        .context("parse mirror publish keys")?;
    let mirror = Arc::new(
        BackgroundNostrMirror::new(
            NostrMirrorConfig {
                relays: vec![relay.url()],
                publish_relays: vec![relay.url()],
                blossom_write_servers: vec![blossom.base_url()],
                history_sync_on_start: false,
                published_event_tree_name: Some("nostr-event-index".to_string()),
                published_profile_search_tree_name: Some("profile-search".to_string()),
                published_profiles_by_pubkey_tree_name: Some("profiles-by-pubkey".to_string()),
                ..NostrMirrorConfig::default()
            },
            store,
            graph_store.clone(),
            Some(publish_keys),
        )
        .await?,
    );

    let connected_started = std::time::Instant::now();
    while connected_started.elapsed() < Duration::from_secs(5) {
        if mirror.has_connected_publish_relay().await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        mirror.has_connected_publish_relay().await,
        "publisher relay should connect"
    );

    mirror.apply_history_root(root.as_ref()).await?;
    wait_until("event DAG upload task", Duration::from_secs(5), || {
        mirror
            .event_publish_state
            .lock()
            .expect("event root publish state")
            .upload_in_progress_root
            .as_ref()
            == root.as_ref()
    })
    .await;
    let profile_publish_started = std::time::Instant::now();
    while profile_publish_started.elapsed() < Duration::from_secs(5) {
        mirror.maybe_publish_profile_search_root(false).await?;
        mirror.maybe_publish_profiles_by_pubkey_root(false).await?;
        mirror.maybe_publish_event_root(false).await?;
        if published_root_event_count(&relay, "profile-search") == 1
            && published_root_event_count(&relay, "profiles-by-pubkey") == 1
            && published_root_event_count(&relay, "nostr-event-index") == 0
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        delayed_hashes.iter().all(|hash| !blossom.has_hash(hash)),
        "delayed event upload completed before event root holdback was checked"
    );
    assert_eq!(published_root_event_count(&relay, "profile-search"), 1);
    assert_eq!(published_root_event_count(&relay, "profiles-by-pubkey"), 1);
    assert_eq!(published_root_event_count(&relay, "nostr-event-index"), 0);

    if !wait_until_bool(Duration::from_secs(30), || {
        delayed_hashes.iter().all(|hash| blossom.put_started(hash))
    })
    .await
    {
        let state = mirror
            .event_publish_state
            .lock()
            .expect("event root publish state");
        panic!(
            "event DAG upload PUTs: condition not met within 30s; in_progress={:?} last_error={:?}",
            state
                .upload_in_progress_root
                .as_ref()
                .map(|root| hex::encode(root.hash)),
            state.last_upload_error
        );
    }
    for hash in &delayed_hashes {
        blossom.release_put(hash);
    }
    wait_until("event DAG upload", Duration::from_secs(5), || {
        delayed_hashes.iter().all(|hash| blossom.has_hash(hash))
    })
    .await;
    let event_publish_started = std::time::Instant::now();
    while event_publish_started.elapsed() < Duration::from_secs(5) {
        mirror.maybe_publish_event_root(false).await?;
        if published_root_event_count(&relay, "nostr-event-index") == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(published_root_event_count(&relay, "nostr-event-index"), 1);
    Ok(())
}

#[tokio::test]
async fn uploaded_event_root_state_is_reused_after_restart() -> Result<()> {
    let _guard = crate::socialgraph::test_lock();
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(HashtreeStore::new(tmp.path())?);
    let graph_store = open_social_graph_store_with_storage(
        tmp.path(),
        store.store_arc(),
        Some(64 * 1024 * 1024),
    )?;

    let root_keys = nostr::Keys::generate();
    set_social_graph_root(&graph_store, &root_keys.public_key().to_bytes());

    let alice_keys = nostr::Keys::generate();
    let alice_note = event_builder!(Kind::TextNote, "persisted upload")
        .custom_created_at(Timestamp::from(22))
        .sign_with_keys(&alice_keys)
        .expect("alice note");
    let stored = hashtree_nostr::StoredNostrEvent {
        id: alice_note.id.to_hex(),
        pubkey: alice_note.pubkey.to_hex(),
        created_at: alice_note.created_at.as_secs(),
        kind: alice_note.kind.as_u16() as u32,
        tags: alice_note
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect(),
        content: alice_note.content.clone(),
        sig: alice_note.sig.to_string(),
    };
    let event_store = NostrEventStore::new(store.store_arc());
    let root = event_store
        .build(None, vec![stored])
        .await?
        .expect("event root");

    let blossom = TestBlossom::new().await;
    let config = NostrMirrorConfig {
        blossom_write_servers: vec![blossom.base_url()],
        history_sync_on_start: false,
        published_event_tree_name: Some("nostr-event-index".to_string()),
        published_profile_search_tree_name: None,
        published_profiles_by_pubkey_tree_name: None,
        ..NostrMirrorConfig::default()
    };
    let mirror = BackgroundNostrMirror::new(
        config.clone(),
        Arc::clone(&store),
        graph_store.clone(),
        None,
    )
    .await?;

    mirror.apply_history_root(Some(&root)).await?;
    let state_path =
        BackgroundNostrMirror::uploaded_root_state_path(store.base_path(), "nostr-event-index");
    wait_until("uploaded root state", Duration::from_secs(5), || {
        state_path.exists() && blossom.has_hash(&hex::encode(root.hash))
    })
    .await;

    let restarted = BackgroundNostrMirror::new(config, store, graph_store, None).await?;
    assert_eq!(
        restarted
            .event_publish_state
            .lock()
            .expect("event publish state")
            .last_uploaded_root
            .as_ref(),
        Some(&root)
    );
    Ok(())
}

#[tokio::test]
async fn event_publish_uses_last_uploaded_root_while_newer_root_uploads() -> Result<()> {
    let _guard = crate::socialgraph::test_lock();
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(HashtreeStore::new(tmp.path())?);
    let graph_store = open_social_graph_store_with_storage(
        tmp.path(),
        store.store_arc(),
        Some(64 * 1024 * 1024),
    )?;

    let root_keys = nostr::Keys::generate();
    set_social_graph_root(&graph_store, &root_keys.public_key().to_bytes());
    let alice_keys = nostr::Keys::generate();
    let note_one = event_builder!(Kind::TextNote, "uploaded")
        .custom_created_at(Timestamp::from(1))
        .sign_with_keys(&alice_keys)
        .expect("note one");
    let note_two = event_builder!(Kind::TextNote, "pending")
        .custom_created_at(Timestamp::from(2))
        .sign_with_keys(&alice_keys)
        .expect("note two");
    let stored_one = hashtree_nostr::StoredNostrEvent {
        id: note_one.id.to_hex(),
        pubkey: note_one.pubkey.to_hex(),
        created_at: note_one.created_at.as_secs(),
        kind: note_one.kind.as_u16() as u32,
        tags: note_one
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect(),
        content: note_one.content.clone(),
        sig: note_one.sig.to_string(),
    };
    let stored_two = hashtree_nostr::StoredNostrEvent {
        id: note_two.id.to_hex(),
        pubkey: note_two.pubkey.to_hex(),
        created_at: note_two.created_at.as_secs(),
        kind: note_two.kind.as_u16() as u32,
        tags: note_two
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect(),
        content: note_two.content.clone(),
        sig: note_two.sig.to_string(),
    };
    let event_store = NostrEventStore::new(store.store_arc());
    let uploaded_root = event_store
        .build(None, vec![stored_one])
        .await?
        .expect("uploaded root");
    let pending_root = event_store
        .build(Some(&uploaded_root), vec![stored_two])
        .await?
        .expect("pending root");

    let relay = TestRelay::new(Vec::new());
    let blossom = TestBlossom::new().await;
    let publish_keys = nostr_sdk::Keys::parse(&root_keys.secret_key().to_bech32()?)
        .context("parse mirror publish keys")?;
    let mirror = BackgroundNostrMirror::new(
        NostrMirrorConfig {
            relays: vec![relay.url()],
            publish_relays: vec![relay.url()],
            blossom_write_servers: vec![blossom.base_url()],
            history_sync_on_start: false,
            published_event_tree_name: Some("nostr-event-index".to_string()),
            published_profile_search_tree_name: None,
            published_profiles_by_pubkey_tree_name: None,
            ..NostrMirrorConfig::default()
        },
        store,
        graph_store,
        Some(publish_keys),
    )
    .await?;

    let connected_started = std::time::Instant::now();
    while connected_started.elapsed() < Duration::from_secs(5) {
        if mirror.has_connected_publish_relay().await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        mirror.has_connected_publish_relay().await,
        "publisher relay should connect"
    );
    {
        let mut state = mirror
            .event_publish_state
            .lock()
            .expect("event publish state");
        state.pending_root = Some(pending_root.clone());
        state.last_uploaded_root = Some(uploaded_root.clone());
        state.last_changed_at = Some(Instant::now() - MIRROR_ROOT_PUBLISH_DEBOUNCE);
        state.dirty_since = Some(Instant::now() - MIRROR_ROOT_PUBLISH_MAX_STALENESS);
    }

    mirror.maybe_publish_event_root(false).await?;
    let resolver = crate::NostrRootResolver::new(crate::NostrResolverConfig {
        relays: vec![relay.url()],
        resolve_timeout: Duration::from_secs(2),
        secret_key: None,
    })
    .await?;
    let npub = root_keys.public_key().to_bech32()?;
    let resolved = resolver
        .resolve(&format!("{npub}/nostr-event-index"))
        .await?
        .expect("published event root");
    assert_eq!(resolved, uploaded_root);
    resolver.stop().await?;
    Ok(())
}

#[tokio::test]
async fn root_publish_retries_with_fresh_client_when_primary_publish_client_misses() -> Result<()> {
    let _guard = crate::socialgraph::test_lock();
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(HashtreeStore::new(tmp.path())?);
    let graph_store = open_social_graph_store_with_storage(
        tmp.path(),
        store.store_arc(),
        Some(64 * 1024 * 1024),
    )?;

    let root_keys = nostr::Keys::generate();
    let root_pubkey = root_keys.public_key().to_bytes();
    set_social_graph_root(&graph_store, &root_pubkey);

    let mirror = BackgroundNostrMirror::new(
        NostrMirrorConfig {
            relays: Vec::new(),
            publish_relays: Vec::new(),
            fetch_timeout: Duration::from_secs(2),
            history_sync_on_start: false,
            ..NostrMirrorConfig::default()
        },
        store,
        graph_store,
        None,
    )
    .await?;

    let relay = TestRelay::new(Vec::new());
    let primary_client = Client::new(Keys::generate());
    let root = hashtree_core::Cid {
        hash: [7u8; 32],
        key: None,
    };
    let event = BackgroundNostrMirror::build_public_root_event(
        "nostr-event-index",
        &root,
        Timestamp::from(42),
    )
    .sign_with_keys(&root_keys)?;

    let (successful_relays, failed_relays) = mirror
        .publish_root_event_to_relays(&primary_client, &[relay.url()], &event)
        .await?;

    assert_eq!(successful_relays, vec![relay.url()]);
    assert_eq!(published_root_event_count(&relay, "nostr-event-index"), 1);
    assert!(
        failed_relays
            .iter()
            .any(|failure| failure.contains("publish relays")),
        "primary publish miss should be retained for diagnostics: {failed_relays:?}"
    );
    Ok(())
}

#[tokio::test]
async fn apply_history_root_publishes_profile_search_over_existing_stale_event() -> Result<()> {
    let _guard = crate::socialgraph::test_lock();
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(HashtreeStore::new(tmp.path())?);
    let graph_store = open_social_graph_store_with_storage(
        tmp.path(),
        store.store_arc(),
        Some(64 * 1024 * 1024),
    )?;

    let root_keys = nostr::Keys::generate();
    let root_pubkey = root_keys.public_key().to_bytes();
    set_social_graph_root(&graph_store, &root_pubkey);

    let alice_keys = nostr::Keys::generate();
    let alice_profile = event_builder!(
        Kind::Metadata,
        r#"{"name":"Alice Replaceable Timestamp"}"#,
        [],
    )
    .custom_created_at(Timestamp::from(11))
    .sign_with_keys(&alice_keys)
    .expect("alice profile");
    let stored = hashtree_nostr::StoredNostrEvent {
        id: alice_profile.id.to_hex(),
        pubkey: alice_profile.pubkey.to_hex(),
        created_at: alice_profile.created_at.as_secs(),
        kind: alice_profile.kind.as_u16() as u32,
        tags: alice_profile
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect(),
        content: alice_profile.content.clone(),
        sig: alice_profile.sig.to_string(),
    };
    let event_store = NostrEventStore::new(store.store_arc());
    let root = event_store.build(None, vec![stored]).await?;

    let stale_created_at = Timestamp::from_secs(1);
    let stale_event = event_builder!(
        Kind::Custom(30078),
        "",
        [
            Tag::identifier("profile-search".to_string()),
            Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::L)),
                vec!["hashtree"],
            ),
            Tag::custom(TagKind::Custom("hash".into()), vec!["11".repeat(32)]),
        ],
    )
    .custom_created_at(stale_created_at)
    .sign_with_keys(&root_keys)
    .expect("stale root event");

    let relay = TestRelay::new(vec![stale_event]);
    let publish_keys = nostr_sdk::Keys::parse(&root_keys.secret_key().to_bech32()?)
        .context("parse mirror publish keys")?;
    let mirror = BackgroundNostrMirror::new(
        NostrMirrorConfig {
            relays: vec![relay.url()],
            publish_relays: vec![relay.url()],
            history_sync_on_start: false,
            published_profile_search_tree_name: Some("profile-search".to_string()),
            ..NostrMirrorConfig::default()
        },
        store,
        graph_store.clone(),
        Some(publish_keys),
    )
    .await?;

    let connected_started = std::time::Instant::now();
    while connected_started.elapsed() < Duration::from_secs(5) {
        if mirror.has_connected_publish_relay().await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        mirror.has_connected_publish_relay().await,
        "publisher relay should connect"
    );

    mirror.apply_history_root(root.as_ref()).await?;

    let resolver = crate::NostrRootResolver::new(crate::NostrResolverConfig {
        relays: vec![relay.url()],
        resolve_timeout: Duration::from_secs(2),
        secret_key: None,
    })
    .await?;
    let npub = root_keys.public_key().to_bech32()?;
    let resolved = resolver
        .resolve(&format!("{npub}/profile-search"))
        .await?
        .expect("published profile-search root");
    resolver.stop().await?;

    let latest = latest_published_root_event(&relay, "profile-search")
        .expect("latest published profile-search event");
    assert!(
        latest.created_at > stale_created_at,
        "new publish should beat stale relay state"
    );
    assert_eq!(
        resolved,
        graph_store
            .profile_search_root()?
            .expect("profile-search root")
    );
    Ok(())
}

#[tokio::test]
async fn apply_history_root_publishes_event_tree() -> Result<()> {
    let _guard = crate::socialgraph::test_lock();
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(HashtreeStore::new(tmp.path())?);
    let graph_store = open_social_graph_store_with_storage(
        tmp.path(),
        store.store_arc(),
        Some(64 * 1024 * 1024),
    )?;

    let root_keys = nostr::Keys::generate();
    let root_pubkey = root_keys.public_key().to_bytes();
    set_social_graph_root(&graph_store, &root_pubkey);

    let alice_keys = nostr::Keys::generate();
    let alice_note = event_builder!(Kind::TextNote, "hello event tree")
        .custom_created_at(Timestamp::from(11))
        .sign_with_keys(&alice_keys)
        .expect("alice note");
    let stored = hashtree_nostr::StoredNostrEvent {
        id: alice_note.id.to_hex(),
        pubkey: alice_note.pubkey.to_hex(),
        created_at: alice_note.created_at.as_secs(),
        kind: alice_note.kind.as_u16() as u32,
        tags: alice_note
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect(),
        content: alice_note.content.clone(),
        sig: alice_note.sig.to_string(),
    };
    let event_store = NostrEventStore::new(store.store_arc());
    let root = event_store.build(None, vec![stored]).await?;

    let relay = TestRelay::new(Vec::new());
    let publish_keys = nostr_sdk::Keys::parse(&root_keys.secret_key().to_bech32()?)
        .context("parse mirror publish keys")?;
    let mirror = BackgroundNostrMirror::new(
        NostrMirrorConfig {
            relays: vec![relay.url()],
            publish_relays: vec![relay.url()],
            history_sync_on_start: false,
            published_profile_search_tree_name: None,
            published_event_tree_name: Some("nostr-event-index".to_string()),
            ..NostrMirrorConfig::default()
        },
        store,
        graph_store.clone(),
        Some(publish_keys),
    )
    .await?;

    let connected_started = std::time::Instant::now();
    while connected_started.elapsed() < Duration::from_secs(5) {
        if mirror.has_connected_publish_relay().await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        mirror.has_connected_publish_relay().await,
        "publisher relay should connect"
    );

    mirror.apply_history_root(root.as_ref()).await?;

    let resolver = crate::NostrRootResolver::new(crate::NostrResolverConfig {
        relays: vec![relay.url()],
        resolve_timeout: Duration::from_secs(2),
        secret_key: None,
    })
    .await?;
    let npub = root_keys.public_key().to_bech32()?;
    let resolved = resolver
        .resolve(&format!("{npub}/nostr-event-index"))
        .await?
        .expect("published event root");

    assert_eq!(
        resolved,
        graph_store.public_events_root()?.expect("event root")
    );
    resolver.stop().await?;
    Ok(())
}

#[tokio::test]
async fn startup_publish_sends_existing_profile_search_root() -> Result<()> {
    let _guard = crate::socialgraph::test_lock();
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(HashtreeStore::new(tmp.path())?);
    let graph_store = open_social_graph_store_with_storage(
        tmp.path(),
        store.store_arc(),
        Some(64 * 1024 * 1024),
    )?;

    let root_keys = nostr::Keys::generate();
    let root_pubkey = root_keys.public_key().to_bytes();
    set_social_graph_root(&graph_store, &root_pubkey);

    let alice_keys = nostr::Keys::generate();
    let alice_profile = event_builder!(Kind::Metadata, r#"{"name":"Alice Existing Search"}"#)
        .custom_created_at(Timestamp::from(11))
        .sign_with_keys(&alice_keys)
        .expect("alice profile");
    let stored = hashtree_nostr::StoredNostrEvent {
        id: alice_profile.id.to_hex(),
        pubkey: alice_profile.pubkey.to_hex(),
        created_at: alice_profile.created_at.as_secs(),
        kind: alice_profile.kind.as_u16() as u32,
        tags: alice_profile
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect(),
        content: alice_profile.content.clone(),
        sig: alice_profile.sig.to_string(),
    };
    let event_store = NostrEventStore::new(store.store_arc());
    let root = event_store.build(None, vec![stored]).await?;
    graph_store.write_public_events_root(root.as_ref())?;
    graph_store.rebuild_profile_index_for_events(&[alice_profile.clone()])?;

    let relay = TestRelay::new(Vec::new());
    let publish_keys = nostr_sdk::Keys::parse(&root_keys.secret_key().to_bech32()?)
        .context("parse mirror publish keys")?;
    let mirror = BackgroundNostrMirror::new(
        NostrMirrorConfig {
            relays: vec![relay.url()],
            publish_relays: vec![relay.url()],
            history_sync_on_start: false,
            published_profile_search_tree_name: Some("profile-search".to_string()),
            ..NostrMirrorConfig::default()
        },
        store,
        graph_store.clone(),
        Some(publish_keys),
    )
    .await?;

    let connected_started = std::time::Instant::now();
    while connected_started.elapsed() < Duration::from_secs(5) {
        if mirror.has_connected_publish_relay().await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        mirror.has_connected_publish_relay().await,
        "publisher relay should connect"
    );

    mirror.note_profile_search_root_change()?;
    tokio::time::sleep(MIRROR_ROOT_PUBLISH_DEBOUNCE + Duration::from_millis(20)).await;
    mirror.maybe_publish_profile_search_root(false).await?;

    let resolver = crate::NostrRootResolver::new(crate::NostrResolverConfig {
        relays: vec![relay.url()],
        resolve_timeout: Duration::from_secs(2),
        secret_key: None,
    })
    .await?;
    let npub = root_keys.public_key().to_bech32()?;
    let resolved = resolver
        .resolve(&format!("{npub}/profile-search"))
        .await?
        .expect("published profile-search root");

    assert_eq!(
        resolved,
        graph_store.profile_search_root()?.expect("search root")
    );
    resolver.stop().await?;
    Ok(())
}

#[tokio::test]
async fn history_sync_checkpoints_root_before_later_chunk_failure() -> Result<()> {
    let _guard = crate::socialgraph::test_lock();
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(HashtreeStore::new(tmp.path())?);
    let graph_store = open_social_graph_store_with_storage(
        tmp.path(),
        store.store_arc(),
        Some(64 * 1024 * 1024),
    )?;

    let root_keys = nostr::Keys::generate();
    let root_pubkey = root_keys.public_key().to_bytes();
    set_social_graph_root(&graph_store, &root_pubkey);

    let alice_keys = nostr::Keys::generate();
    let alice_profile = event_builder!(Kind::Metadata, r#"{"name":"Alice Checkpoint"}"#)
        .custom_created_at(Timestamp::from(11))
        .sign_with_keys(&alice_keys)
        .expect("alice profile");
    let alice_stored = hashtree_nostr::StoredNostrEvent {
        id: alice_profile.id.to_hex(),
        pubkey: alice_profile.pubkey.to_hex(),
        created_at: alice_profile.created_at.as_secs(),
        kind: alice_profile.kind.as_u16() as u32,
        tags: alice_profile
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect(),
        content: alice_profile.content.clone(),
        sig: alice_profile.sig.to_string(),
    };
    let event_store = NostrEventStore::new(store.store_arc());
    let root = event_store.build(None, vec![alice_stored.clone()]).await?;
    let mirror = BackgroundNostrMirror::new(
        NostrMirrorConfig::default(),
        store,
        graph_store.clone(),
        None,
    )
    .await?;
    let call_index = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    mirror
        .history_sync_authors_chunked(
            vec!["author-a".to_string(), "author-b".to_string()],
            {
                let call_index = Arc::clone(&call_index);
                let root = root.clone();
                move |_current_root, author_chunk| {
                    let call_index = Arc::clone(&call_index);
                    let root = root.clone();
                    let alice_stored = alice_stored.clone();
                    std::future::ready(
                        match call_index.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                            0 => Ok(CrawlReport {
                                root: root.clone(),
                                authors_considered: 2,
                                authors_processed: author_chunk.len(),
                                events_seen: 1,
                                events_selected: 1,
                                live_bytes_selected: 0,
                                applied_events: vec![alice_stored],
                            }),
                            _ => Err(anyhow::anyhow!("boom")),
                        },
                    )
                }
            },
            true,
            None,
        )
        .await?;

    let alice_hex = alice_keys.public_key().to_hex();
    assert!(graph_store.latest_profile_event(&alice_hex)?.is_some());
    assert_eq!(
        graph_store.public_events_root()?,
        root,
        "expected first successful chunk to checkpoint trusted root"
    );
    Ok(())
}

#[tokio::test]
async fn history_sync_merges_chunk_when_live_root_advances() -> Result<()> {
    let _guard = crate::socialgraph::test_lock();
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(HashtreeStore::new(tmp.path())?);
    let graph_store = open_social_graph_store_with_storage(
        tmp.path(),
        store.store_arc(),
        Some(64 * 1024 * 1024),
    )?;

    let root_keys = nostr::Keys::generate();
    set_social_graph_root(&graph_store, &root_keys.public_key().to_bytes());

    let initial_keys = nostr::Keys::generate();
    let initial_event = event_builder!(Kind::TextNote, "initial")
        .custom_created_at(Timestamp::from(10))
        .sign_with_keys(&initial_keys)
        .expect("initial event");
    let initial_stored = hashtree_nostr::StoredNostrEvent {
        id: initial_event.id.to_hex(),
        pubkey: initial_event.pubkey.to_hex(),
        created_at: initial_event.created_at.as_secs(),
        kind: initial_event.kind.as_u16() as u32,
        tags: initial_event
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect(),
        content: initial_event.content.clone(),
        sig: initial_event.sig.to_string(),
    };

    let live_keys = nostr::Keys::generate();
    let live_event = event_builder!(Kind::TextNote, "live")
        .custom_created_at(Timestamp::from(11))
        .sign_with_keys(&live_keys)
        .expect("live event");
    let live_stored = hashtree_nostr::StoredNostrEvent {
        id: live_event.id.to_hex(),
        pubkey: live_event.pubkey.to_hex(),
        created_at: live_event.created_at.as_secs(),
        kind: live_event.kind.as_u16() as u32,
        tags: live_event
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect(),
        content: live_event.content.clone(),
        sig: live_event.sig.to_string(),
    };

    let history_keys = nostr::Keys::generate();
    let history_event = event_builder!(Kind::TextNote, "history")
        .custom_created_at(Timestamp::from(12))
        .sign_with_keys(&history_keys)
        .expect("history event");
    let history_stored = hashtree_nostr::StoredNostrEvent {
        id: history_event.id.to_hex(),
        pubkey: history_event.pubkey.to_hex(),
        created_at: history_event.created_at.as_secs(),
        kind: history_event.kind.as_u16() as u32,
        tags: history_event
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect(),
        content: history_event.content.clone(),
        sig: history_event.sig.to_string(),
    };

    let event_store = NostrEventStore::new(store.store_arc());
    let initial_root = event_store.build(None, vec![initial_stored]).await?;
    graph_store.write_public_events_root(initial_root.as_ref())?;
    let live_root = event_store
        .build(initial_root.as_ref(), vec![live_stored.clone()])
        .await?;
    let history_root = event_store
        .build(initial_root.as_ref(), vec![history_stored.clone()])
        .await?;

    let mirror = BackgroundNostrMirror::new(
        NostrMirrorConfig::default(),
        store,
        graph_store.clone(),
        None,
    )
    .await?;
    let call_index = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    mirror
        .history_sync_authors_chunked(
            vec!["author-a".to_string()],
            {
                let call_index = Arc::clone(&call_index);
                let graph_store = graph_store.clone();
                let live_root = live_root.clone();
                let history_root = history_root.clone();
                let history_stored = history_stored.clone();
                move |_current_root, author_chunk| {
                    let call_index = Arc::clone(&call_index);
                    let graph_store = graph_store.clone();
                    let live_root = live_root.clone();
                    let history_root = history_root.clone();
                    let history_stored = history_stored.clone();
                    async move {
                        assert_eq!(
                            call_index.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
                            0,
                            "history chunk should not be refetched after live root churn"
                        );
                        graph_store.write_public_events_root(live_root.as_ref())?;
                        Ok(CrawlReport {
                            root: history_root.clone(),
                            authors_considered: 1,
                            authors_processed: author_chunk.len(),
                            events_seen: 1,
                            events_selected: 1,
                            live_bytes_selected: 0,
                            applied_events: vec![history_stored.clone()],
                        })
                    }
                }
            },
            false,
            Some(1),
        )
        .await?;

    assert_eq!(call_index.load(std::sync::atomic::Ordering::SeqCst), 1);
    let final_root = graph_store.public_events_root()?.expect("final root");
    let events = event_store
        .list_recent(Some(&final_root), ListEventsOptions::default())
        .await?;
    let event_ids = events
        .into_iter()
        .map(|event| event.id)
        .collect::<HashSet<_>>();
    assert!(event_ids.contains(&live_event.id.to_hex()));
    assert!(event_ids.contains(&history_event.id.to_hex()));
    Ok(())
}

#[tokio::test]
async fn event_only_history_sync_skips_profile_rebuild() -> Result<()> {
    let _guard = crate::socialgraph::test_lock();
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(HashtreeStore::new(tmp.path())?);
    let graph_store = open_social_graph_store_with_storage(
        tmp.path(),
        store.store_arc(),
        Some(64 * 1024 * 1024),
    )?;

    let root_keys = nostr::Keys::generate();
    let root_pubkey = root_keys.public_key().to_bytes();
    set_social_graph_root(&graph_store, &root_pubkey);

    let alice_keys = nostr::Keys::generate();
    let alice_profile = event_builder!(Kind::Metadata, r#"{"name":"Alice Ignored"}"#)
        .custom_created_at(Timestamp::from(12))
        .sign_with_keys(&alice_keys)
        .expect("alice profile");
    let alice_stored = hashtree_nostr::StoredNostrEvent {
        id: alice_profile.id.to_hex(),
        pubkey: alice_profile.pubkey.to_hex(),
        created_at: alice_profile.created_at.as_secs(),
        kind: alice_profile.kind.as_u16() as u32,
        tags: alice_profile
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect(),
        content: alice_profile.content.clone(),
        sig: alice_profile.sig.to_string(),
    };
    let event_store = NostrEventStore::new(store.store_arc());
    let root = event_store.build(None, vec![alice_stored]).await?;
    let mirror = BackgroundNostrMirror::new(
        NostrMirrorConfig::default(),
        store,
        graph_store.clone(),
        None,
    )
    .await?;

    mirror
        .history_sync_authors_chunked(
            vec!["author-a".to_string()],
            {
                let root = root.clone();
                move |_current_root, author_chunk| {
                    let root = root.clone();
                    std::future::ready(Ok(CrawlReport {
                        root: root.clone(),
                        authors_considered: 1,
                        authors_processed: author_chunk.len(),
                        events_seen: 1,
                        events_selected: 1,
                        live_bytes_selected: 0,
                        applied_events: Vec::new(),
                    }))
                }
            },
            false,
            None,
        )
        .await?;

    let alice_hex = alice_keys.public_key().to_hex();
    assert!(graph_store.latest_profile_event(&alice_hex)?.is_none());
    assert_eq!(
        graph_store.public_events_root()?,
        root,
        "event-only history sync should still checkpoint trusted root"
    );
    Ok(())
}

#[tokio::test]
async fn mirror_history_sync_accepts_large_contact_list_events() -> Result<()> {
    let _guard = crate::socialgraph::test_lock();
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(HashtreeStore::new(tmp.path())?);
    let graph_store = open_social_graph_store_with_storage(
        tmp.path(),
        store.store_arc(),
        Some(64 * 1024 * 1024),
    )?;

    let root_keys = nostr::Keys::generate();
    let root_pubkey = root_keys.public_key().to_bytes();
    set_social_graph_root(&graph_store, &root_pubkey);

    let followed_keys = (0..1_600)
        .map(|_| nostr::Keys::generate())
        .collect::<Vec<_>>();
    let tags = followed_keys
        .iter()
        .map(|keys| Tag::public_key(keys.public_key()))
        .collect::<Vec<_>>();
    let root_contacts = event_builder!(Kind::ContactList, "")
        .tags(tags)
        .custom_created_at(Timestamp::from(10))
        .sign_with_keys(&root_keys)
        .expect("root contacts");
    assert!(
        root_contacts.as_json().len() > 70_000,
        "test event should exceed nostr-sdk default size limit"
    );

    let relay = TestRelay::new(vec![root_contacts]);
    let mirror = BackgroundNostrMirror::new(
        NostrMirrorConfig {
            relays: vec![relay.url()],
            max_follow_distance: 1,
            kinds: vec![3],
            history_sync_on_start: false,
            ..NostrMirrorConfig::default()
        },
        store,
        graph_store.clone(),
        None,
    )
    .await?;

    mirror
        .history_sync_authors(vec![root_keys.public_key().to_hex()])
        .await?;

    let follows = crate::socialgraph::get_follows(&graph_store, &root_pubkey);
    let first_pk = followed_keys
        .first()
        .expect("first followed key")
        .public_key()
        .to_bytes();
    let last_pk = followed_keys
        .last()
        .expect("last followed key")
        .public_key()
        .to_bytes();
    assert!(
        follows.contains(&first_pk) && follows.contains(&last_pk),
        "expected history sync to ingest oversized contact list event"
    );
    Ok(())
}

#[tokio::test]
async fn mirror_collect_authors_skips_overmuted_users() -> Result<()> {
    let _guard = crate::socialgraph::test_lock();
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(HashtreeStore::new(tmp.path())?);
    let graph_store = open_social_graph_store_with_storage(
        tmp.path(),
        store.store_arc(),
        Some(64 * 1024 * 1024),
    )?;

    let root_keys = nostr::Keys::generate();
    let target_keys = nostr::Keys::generate();
    set_social_graph_root(&graph_store, &root_keys.public_key().to_bytes());

    let follow = event_builder!(
        Kind::ContactList,
        "",
        vec![Tag::public_key(target_keys.public_key())],
    )
    .custom_created_at(Timestamp::from_secs(10))
    .sign_with_keys(&root_keys)
    .expect("follow");
    crate::socialgraph::ingest_parsed_event(&graph_store, &follow)?;

    let mute = event_builder!(
        Kind::MuteList,
        "",
        vec![Tag::public_key(target_keys.public_key())],
    )
    .custom_created_at(Timestamp::from_secs(11))
    .sign_with_keys(&root_keys)
    .expect("mute");
    crate::socialgraph::ingest_parsed_event(&graph_store, &mute)?;

    let mirror = BackgroundNostrMirror::new(
        NostrMirrorConfig {
            max_follow_distance: 1,
            overmute_threshold: 1.0,
            ..NostrMirrorConfig::default()
        },
        store,
        graph_store.clone(),
        None,
    )
    .await?;

    let authors = mirror.collect_authors()?;
    assert!(authors.contains(&root_keys.public_key().to_hex()));
    assert!(!authors.contains(&target_keys.public_key().to_hex()));
    Ok(())
}

#[tokio::test]
async fn full_text_history_prioritizes_low_indexed_direct_follows() -> Result<()> {
    let _guard = crate::socialgraph::test_lock();
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(HashtreeStore::new(tmp.path())?);
    let graph_store = open_social_graph_store_with_storage(
        tmp.path(),
        store.store_arc(),
        Some(64 * 1024 * 1024),
    )?;

    let root_keys = nostr::Keys::generate();
    let prolific_keys = nostr::Keys::generate();
    let sparse_keys = nostr::Keys::generate();
    set_social_graph_root(&graph_store, &root_keys.public_key().to_bytes());

    let follow = event_builder!(
        Kind::ContactList,
        "",
        vec![
            Tag::public_key(prolific_keys.public_key()),
            Tag::public_key(sparse_keys.public_key()),
        ],
    )
    .custom_created_at(Timestamp::from_secs(10))
    .sign_with_keys(&root_keys)
    .expect("follow");
    crate::socialgraph::ingest_parsed_event(&graph_store, &follow)?;

    let mut stored_notes = Vec::new();
    for created_at in 1..=40 {
        let note = event_builder!(Kind::TextNote, format!("note {created_at}"))
            .custom_created_at(Timestamp::from_secs(created_at))
            .sign_with_keys(&prolific_keys)
            .expect("prolific note");
        stored_notes.push(hashtree_nostr::StoredNostrEvent {
            id: note.id.to_hex(),
            pubkey: note.pubkey.to_hex(),
            created_at: note.created_at.as_secs(),
            kind: note.kind.as_u16() as u32,
            tags: note
                .tags
                .iter()
                .map(|tag| tag.as_slice().to_vec())
                .collect(),
            content: note.content.clone(),
            sig: note.sig.to_string(),
        });
    }
    let event_store = NostrEventStore::new(store.store_arc());
    let event_root = event_store.build(None, stored_notes).await?;
    graph_store.write_public_events_root(event_root.as_ref())?;

    let mirror = BackgroundNostrMirror::new(
        NostrMirrorConfig {
            max_follow_distance: 1,
            history_sync_on_start: false,
            ..NostrMirrorConfig::default()
        },
        store,
        graph_store,
        None,
    )
    .await?;

    let prolific = prolific_keys.public_key().to_hex();
    let sparse = sparse_keys.public_key().to_hex();
    let prioritized = mirror
        .prioritize_full_text_note_history_authors(vec![prolific.clone(), sparse.clone()])
        .await?;
    assert_eq!(prioritized, vec![sparse, prolific]);
    Ok(())
}

#[tokio::test]
async fn mirror_collect_missing_profile_authors_skips_existing_profiles() -> Result<()> {
    let _guard = crate::socialgraph::test_lock();
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(HashtreeStore::new(tmp.path())?);
    let graph_store = open_social_graph_store_with_storage(
        tmp.path(),
        store.store_arc(),
        Some(64 * 1024 * 1024),
    )?;

    let root_keys = nostr::Keys::generate();
    let existing_keys = nostr::Keys::generate();
    let missing_keys = nostr::Keys::generate();
    set_social_graph_root(&graph_store, &root_keys.public_key().to_bytes());

    let root_profile = event_builder!(Kind::Metadata, r#"{"name":"root"}"#)
        .custom_created_at(Timestamp::from_secs(5))
        .sign_with_keys(&root_keys)
        .expect("root profile");
    crate::socialgraph::ingest_parsed_event(&graph_store, &root_profile)?;

    let follow = event_builder!(
        Kind::ContactList,
        "",
        vec![
            Tag::public_key(existing_keys.public_key()),
            Tag::public_key(missing_keys.public_key()),
        ],
    )
    .custom_created_at(Timestamp::from_secs(10))
    .sign_with_keys(&root_keys)
    .expect("follow");
    crate::socialgraph::ingest_parsed_event(&graph_store, &follow)?;

    let existing_profile = event_builder!(Kind::Metadata, r#"{"name":"existing"}"#)
        .custom_created_at(Timestamp::from_secs(11))
        .sign_with_keys(&existing_keys)
        .expect("existing profile");
    crate::socialgraph::ingest_parsed_event(&graph_store, &existing_profile)?;

    let mirror = BackgroundNostrMirror::new(
        NostrMirrorConfig {
            max_follow_distance: 1,
            kinds: vec![0, 1, 3, 6, 7, 9_735],
            history_sync_on_start: false,
            ..NostrMirrorConfig::default()
        },
        store,
        graph_store.clone(),
        None,
    )
    .await?;

    let authors = mirror.collect_missing_profile_authors(10)?;
    assert!(authors.contains(&missing_keys.public_key().to_hex()));
    assert!(!authors.contains(&existing_keys.public_key().to_hex()));
    assert!(!authors.contains(&root_keys.public_key().to_hex()));
    Ok(())
}

#[tokio::test]
async fn live_event_flush_publishes_only_for_new_public_root() -> Result<()> {
    let _guard = crate::socialgraph::test_lock();
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(HashtreeStore::new(tmp.path())?);
    let graph_store = open_social_graph_store_with_storage(
        tmp.path(),
        store.store_arc(),
        Some(64 * 1024 * 1024),
    )?;

    let root_keys = nostr::Keys::generate();
    let root_pubkey = root_keys.public_key().to_bytes();
    set_social_graph_root(&graph_store, &root_pubkey);

    let relay = TestRelay::new(Vec::new());
    let publish_keys = nostr_sdk::Keys::parse(&root_keys.secret_key().to_bech32()?)
        .context("parse mirror publish keys")?;
    let mirror = BackgroundNostrMirror::new(
        NostrMirrorConfig {
            relays: vec![relay.url()],
            publish_relays: vec![relay.url()],
            history_sync_on_start: false,
            published_profile_search_tree_name: None,
            published_event_tree_name: Some("nostr-event-index".to_string()),
            ..NostrMirrorConfig::default()
        },
        store.clone(),
        graph_store.clone(),
        Some(publish_keys),
    )
    .await?;

    let connected_started = std::time::Instant::now();
    while connected_started.elapsed() < Duration::from_secs(5) {
        if mirror.has_connected_publish_relay().await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        mirror.has_connected_publish_relay().await,
        "publisher relay should connect"
    );

    let alice_keys = nostr::Keys::generate();
    let alice_note = event_builder!(Kind::TextNote, "hello live flush")
        .custom_created_at(Timestamp::from(21))
        .sign_with_keys(&alice_keys)
        .expect("alice note");

    mirror.ingest_live_event(&alice_note)?;
    mirror.flush_live_events().await?;
    mirror.maybe_publish_event_root(true).await?;

    let public_root = graph_store
        .public_events_root()?
        .expect("public event root");
    let event_store = NostrEventStore::new(store.store_arc());
    let stored = event_store
        .get_by_id(Some(&public_root), &alice_note.id.to_hex())
        .await?
        .expect("stored mirrored note");
    assert_eq!(stored.id, alice_note.id.to_hex());

    let resolver = crate::NostrRootResolver::new(crate::NostrResolverConfig {
        relays: vec![relay.url()],
        resolve_timeout: Duration::from_secs(2),
        secret_key: None,
    })
    .await?;
    let npub = root_keys.public_key().to_bech32()?;
    let resolved = resolver
        .resolve(&format!("{npub}/nostr-event-index"))
        .await?
        .expect("published event root");
    assert_eq!(resolved, public_root);

    let published_count = published_root_event_count(&relay, "nostr-event-index");
    assert_eq!(published_count, 1);

    mirror.ingest_live_event(&alice_note)?;
    mirror.flush_live_events().await?;
    mirror.maybe_publish_event_root(true).await?;
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(
        published_root_event_count(&relay, "nostr-event-index"),
        published_count,
        "expected duplicate live flush to avoid republishing the same root"
    );

    resolver.stop().await?;
    Ok(())
}

#[tokio::test]
async fn live_event_flush_recovers_invalid_public_event_root_before_publish() -> Result<()> {
    let _guard = crate::socialgraph::test_lock();
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(HashtreeStore::new(tmp.path())?);
    let graph_store = open_social_graph_store_with_storage(
        tmp.path(),
        store.store_arc(),
        Some(64 * 1024 * 1024),
    )?;

    let root_keys = nostr::Keys::generate();
    let root_pubkey = root_keys.public_key().to_bytes();
    set_social_graph_root(&graph_store, &root_pubkey);

    let tree = hashtree_core::HashTree::new(hashtree_core::HashTreeConfig::new(store.store_arc()));
    let (invalid_root, _) = tree
        .put_file(b"this is a file blob, not a nostr event index")
        .await?;
    graph_store.write_public_events_root(Some(&invalid_root))?;

    let relay = TestRelay::new(Vec::new());
    let publish_keys = nostr_sdk::Keys::parse(&root_keys.secret_key().to_bech32()?)
        .context("parse mirror publish keys")?;
    let mirror = BackgroundNostrMirror::new(
        NostrMirrorConfig {
            relays: vec![relay.url()],
            publish_relays: vec![relay.url()],
            history_sync_on_start: false,
            published_profile_search_tree_name: None,
            published_event_tree_name: Some("nostr-event-index".to_string()),
            ..NostrMirrorConfig::default()
        },
        store.clone(),
        graph_store.clone(),
        Some(publish_keys),
    )
    .await?;

    let connected_started = std::time::Instant::now();
    while connected_started.elapsed() < Duration::from_secs(5) {
        if mirror.has_connected_publish_relay().await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        mirror.has_connected_publish_relay().await,
        "publisher relay should connect"
    );

    let alice_keys = nostr::Keys::generate();
    let alice_note = event_builder!(Kind::TextNote, "hello recovered index")
        .custom_created_at(Timestamp::from(22))
        .sign_with_keys(&alice_keys)
        .expect("alice note");

    mirror.ingest_live_event(&alice_note)?;
    mirror.flush_live_events().await?;
    mirror.maybe_publish_event_root(true).await?;

    let public_root = graph_store
        .public_events_root()?
        .expect("public event root");
    assert_ne!(
        public_root, invalid_root,
        "flush should replace the invalid root before publishing"
    );
    let event_store = NostrEventStore::new(store.store_arc());
    event_store.validate_index_root(Some(&public_root)).await?;
    assert!(event_store
        .get_by_id(Some(&public_root), &alice_note.id.to_hex())
        .await?
        .is_some());

    let resolver = crate::NostrRootResolver::new(crate::NostrResolverConfig {
        relays: vec![relay.url()],
        resolve_timeout: Duration::from_secs(2),
        secret_key: None,
    })
    .await?;
    let npub = root_keys.public_key().to_bech32()?;
    let resolved = resolver
        .resolve(&format!("{npub}/nostr-event-index"))
        .await?
        .expect("published event root");
    assert_eq!(resolved, public_root);
    assert_eq!(published_root_event_count(&relay, "nostr-event-index"), 1);

    resolver.stop().await?;
    Ok(())
}

#[tokio::test]
async fn mirror_live_ingest_updates_profile_index() -> Result<()> {
    let _guard = crate::socialgraph::test_lock();
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(HashtreeStore::new(tmp.path())?);
    let graph_store = open_social_graph_store_with_storage(
        tmp.path(),
        store.store_arc(),
        Some(64 * 1024 * 1024),
    )?;

    let root_keys = nostr::Keys::generate();
    let root_pubkey = root_keys.public_key().to_bytes();
    set_social_graph_root(&graph_store, &root_pubkey);

    let alice_keys = nostr::Keys::generate();
    let root_contacts = event_builder!(
        Kind::ContactList,
        "",
        vec![Tag::public_key(alice_keys.public_key())],
    )
    .custom_created_at(Timestamp::from(10))
    .sign_with_keys(&root_keys)
    .expect("root contacts");
    socialgraph::ingest_parsed_event(graph_store.as_ref(), &root_contacts)?;

    let relay = TestRelay::new(Vec::new());
    let mirror = Arc::new(
        BackgroundNostrMirror::new(
            NostrMirrorConfig {
                relays: vec![relay.url()],
                max_follow_distance: 1,
                author_batch_size: 32,
                history_sync_on_start: false,
                missing_profile_backfill_batch_size: 0,
                ..NostrMirrorConfig::default()
            },
            store,
            graph_store.clone(),
            None,
        )
        .await?,
    );

    let mirror_task = {
        let mirror = Arc::clone(&mirror);
        tokio::task::spawn_blocking(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build test mirror runtime");
            runtime.block_on(async { mirror.run().await })
        })
    };

    wait_until("subscription", Duration::from_secs(5), || {
        relay.request_count() > 0
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let updated_profile = event_builder!(Kind::Metadata, r#"{"name":"Alice Mirror Updated"}"#)
        .sign_with_keys(&alice_keys)
        .expect("updated profile");
    relay.publish(updated_profile);

    let alice_hex = alice_keys.public_key().to_hex();
    wait_until("live profile update", Duration::from_secs(5), || {
        graph_store
            .latest_profile_event(&alice_hex)
            .ok()
            .flatten()
            .is_some_and(|event| event.content.contains("Updated"))
    })
    .await;

    mirror.shutdown();
    mirror_task.await.expect("mirror join")?;
    Ok(())
}

#[tokio::test]
async fn mirror_republishes_roots_changed_outside_the_mirror() -> Result<()> {
    let _guard = crate::socialgraph::test_lock();
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(HashtreeStore::new(tmp.path())?);
    let graph_store = open_social_graph_store_with_storage(
        tmp.path(),
        store.store_arc(),
        Some(64 * 1024 * 1024),
    )?;

    let root_keys = nostr::Keys::generate();
    let root_pubkey = root_keys.public_key().to_bytes();
    set_social_graph_root(&graph_store, &root_pubkey);

    let relay = TestRelay::new(Vec::new());
    let publish_keys = nostr_sdk::Keys::parse(&root_keys.secret_key().to_bech32()?)
        .context("parse mirror publish keys")?;
    let mirror = Arc::new(
        BackgroundNostrMirror::new(
            NostrMirrorConfig {
                relays: vec![relay.url()],
                publish_relays: vec![relay.url()],
                history_sync_on_start: false,
                missing_profile_backfill_batch_size: 0,
                ..NostrMirrorConfig::default()
            },
            store,
            graph_store.clone(),
            Some(publish_keys),
        )
        .await?,
    );

    let mirror_task = {
        let mirror = Arc::clone(&mirror);
        tokio::task::spawn_blocking(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build test mirror runtime");
            runtime.block_on(async { mirror.run().await })
        })
    };

    wait_until("publisher relay connection", Duration::from_secs(5), || {
        futures::executor::block_on(mirror.has_connected_publish_relay())
    })
    .await;

    let root_profile = event_builder!(Kind::Metadata, r#"{"name":"Root Out Of Band"}"#)
        .custom_created_at(Timestamp::from(42))
        .sign_with_keys(&root_keys)
        .expect("root profile");
    socialgraph::ingest_parsed_event(graph_store.as_ref(), &root_profile)?;

    wait_until(
        "out-of-band root publication",
        Duration::from_secs(5),
        || {
            published_root_event_count(&relay, "nostr-event-index") > 0
                && published_root_event_count(&relay, "profile-search") > 0
        },
    )
    .await;

    let resolver = crate::NostrRootResolver::new(crate::NostrResolverConfig {
        relays: vec![relay.url()],
        resolve_timeout: Duration::from_secs(2),
        secret_key: None,
    })
    .await?;
    let npub = root_keys.public_key().to_bech32()?;

    let published_event_root = resolver
        .resolve(&format!("{npub}/nostr-event-index"))
        .await?
        .expect("published event root");
    let published_profile_root = resolver
        .resolve(&format!("{npub}/profile-search"))
        .await?
        .expect("published profile root");

    assert_eq!(
        published_event_root,
        graph_store
            .public_events_root()?
            .expect("current public event root")
    );
    assert_eq!(
        published_profile_root,
        graph_store
            .profile_search_root()?
            .expect("current profile search root")
    );

    mirror.shutdown();
    mirror_task.await.expect("mirror join")?;
    resolver.stop().await?;
    Ok(())
}

#[test]
fn relay_connected_after_disconnect_triggers_reconnect_history_sync() {
    assert!(BackgroundNostrMirror::should_history_sync_on_reconnect(
        true,
        Some(RelayStatus::Disconnected),
        RelayStatus::Connected
    ));
    assert!(!BackgroundNostrMirror::should_history_sync_on_reconnect(
        true,
        Some(RelayStatus::Connected),
        RelayStatus::Connected
    ));
    assert!(!BackgroundNostrMirror::should_history_sync_on_reconnect(
        true,
        None,
        RelayStatus::Connected
    ));
    assert!(!BackgroundNostrMirror::should_history_sync_on_reconnect(
        false,
        Some(RelayStatus::Disconnected),
        RelayStatus::Connected
    ));
}

#[test]
fn reconnect_history_sync_respects_cooldown() {
    let now = Instant::now();
    assert!(BackgroundNostrMirror::should_run_reconnect_history_sync(
        None
    ));
    assert!(!BackgroundNostrMirror::should_run_reconnect_history_sync(
        Some(&now)
    ));
}

#[test]
fn metadata_only_history_sync_uses_small_author_batches() {
    let plan = BackgroundNostrMirror::history_sync_plan_for(
        &NostrMirrorConfig::default(),
        5_000,
        &[Kind::Metadata.as_u16()],
    );

    assert_eq!(plan.relay_fetch_mode, RelayFetchMode::AuthorBatches);
    assert_eq!(plan.per_author_event_limit, 1);
    assert_eq!(plan.author_batch_size, 64);
}

#[test]
fn text_note_history_sync_is_event_root_only() {
    assert!(
        !BackgroundNostrMirror::history_sync_kinds_affect_profile_or_graph(&[
            Kind::TextNote.as_u16(),
            30_023,
        ])
    );
    assert!(
        BackgroundNostrMirror::history_sync_kinds_affect_profile_or_graph(&[
            Kind::Metadata.as_u16()
        ])
    );
    assert!(
        BackgroundNostrMirror::history_sync_kinds_affect_profile_or_graph(&[
            Kind::ContactList.as_u16()
        ])
    );
    assert!(
        BackgroundNostrMirror::history_sync_kinds_affect_profile_or_graph(&[
            Kind::MuteList.as_u16()
        ])
    );
}

#[test]
fn zero_full_text_history_pages_disables_startup_text_history() {
    let config = NostrMirrorConfig {
        full_text_note_history_max_relay_pages: 0,
        ..NostrMirrorConfig::default()
    };
    assert_eq!(
        BackgroundNostrMirror::full_text_note_history_max_relay_pages_for_config(&config),
        None
    );

    let config = NostrMirrorConfig {
        full_text_note_history_max_relay_pages: 3,
        ..NostrMirrorConfig::default()
    };
    assert_eq!(
        BackgroundNostrMirror::full_text_note_history_max_relay_pages_for_config(&config),
        Some(3)
    );
}

#[test]
fn general_history_sync_excludes_text_content_kinds() {
    let config = NostrMirrorConfig {
        full_text_note_history_max_relay_pages: 0,
        ..NostrMirrorConfig::default()
    };
    let kinds = BackgroundNostrMirror::history_sync_kinds_for_config(&config);
    assert!(!kinds.contains(&Kind::TextNote.as_u16()));
    assert!(!kinds.contains(&30_023));
    assert!(kinds.contains(&Kind::Metadata.as_u16()));
    assert!(kinds.contains(&Kind::ContactList.as_u16()));

    let config = NostrMirrorConfig {
        full_text_note_history_max_relay_pages: 3,
        ..NostrMirrorConfig::default()
    };
    let kinds = BackgroundNostrMirror::history_sync_kinds_for_config(&config);
    assert!(!kinds.contains(&Kind::TextNote.as_u16()));
    assert!(!kinds.contains(&30_023));
    assert!(kinds.contains(&Kind::Metadata.as_u16()));
    assert!(kinds.contains(&Kind::ContactList.as_u16()));
}

#[test]
fn large_history_sync_prefers_global_recent() {
    let config = NostrMirrorConfig::default();
    let plan = BackgroundNostrMirror::history_sync_plan_for(
        &config,
        config.author_batch_size * 9,
        &config.kinds,
    );

    assert_eq!(plan.relay_fetch_mode, RelayFetchMode::GlobalRecent);
    assert_eq!(plan.per_author_event_limit, 16);
    assert_eq!(plan.max_relay_pages, 20);
}

#[test]
fn large_global_recent_history_sync_uses_one_chunk() {
    let config = NostrMirrorConfig {
        history_sync_author_chunk_size: 128,
        ..NostrMirrorConfig::default()
    };
    let kinds = BackgroundNostrMirror::history_sync_kinds_for_config(&config);
    let authors = config.author_batch_size * 9;
    let chunk_size = BackgroundNostrMirror::history_sync_chunk_size_for_config(
        &config, authors, &kinds, false, None,
    );

    assert_eq!(chunk_size, authors);
}

#[test]
fn full_author_history_flushes_each_author() {
    let config = NostrMirrorConfig {
        history_sync_author_chunk_size: 128,
        ..NostrMirrorConfig::default()
    };
    let kinds = [Kind::TextNote.as_u16(), 30_023];
    let authors = config.author_batch_size * 9;
    let chunk_size = BackgroundNostrMirror::history_sync_chunk_size_for_config(
        &config, authors, &kinds, true, None,
    );

    assert_eq!(chunk_size, 1);
}
