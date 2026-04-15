use super::*;
use futures::{SinkExt, StreamExt};
use hashtree_resolver::RootResolver;
use nostr::{EventBuilder, JsonUtil, Tag};
use nostr_sdk::ToBech32;
use std::collections::HashMap;
use std::net::TcpListener;
use std::sync::Mutex;
use tempfile::TempDir;
use tokio::net::TcpStream;
use tokio::sync::broadcast;
use tokio_tungstenite::{accept_async, tungstenite::Message};

use crate::socialgraph::{open_social_graph_store_with_storage, set_social_graph_root};

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

fn published_root_event_count(relay: &TestRelay, tree_name: &str) -> usize {
    relay
        .events
        .lock()
        .expect("relay events")
        .iter()
        .filter(|event| {
            event.kind == Kind::Custom(30078)
                && event.tags.iter().any(|tag| {
                    let values = tag.as_slice();
                    values.first().is_some_and(|value| value == "d")
                        && values.get(1).is_some_and(|value| value == tree_name)
                })
        })
        .count()
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
                        request_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        subscriptions.insert(subscription_id.to_string(), filters.clone());
                        let current = events.lock().expect("relay events").clone();
                        for event in current {
                            if filters.iter().any(|filter| filter.match_event(&event)) {
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
                        subscriptions.remove(&subscription_id.to_string());
                        let _ = write
                            .send(Message::Text(
                                nostr::RelayMessage::closed(subscription_id, "").as_json(),
                            ))
                            .await;
                    }
                    nostr::ClientMessage::Event(event) => {
                        let event = *event;
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
                    if filters.iter().any(|filter| filter.match_event(&event)) {
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

async fn wait_until<F>(label: &str, timeout: Duration, mut condition: F)
where
    F: FnMut() -> bool,
{
    let started = std::time::Instant::now();
    while started.elapsed() < timeout {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{label}: condition not met within {:?}", timeout);
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
    let root_contacts = EventBuilder::new(
        Kind::ContactList,
        "",
        vec![Tag::public_key(alice_keys.public_key())],
    )
    .custom_created_at(Timestamp::from(10))
    .to_event(&root_keys)
    .expect("root contacts");
    socialgraph::ingest_parsed_event(graph_store.as_ref(), &root_contacts)?;

    let alice_profile = EventBuilder::new(Kind::Metadata, r#"{"name":"Alice Mirror"}"#, [])
        .custom_created_at(Timestamp::from(11))
        .to_event(&alice_keys)
        .expect("alice profile");
    let stored = hashtree_nostr::StoredNostrEvent {
        id: alice_profile.id.to_hex(),
        pubkey: alice_profile.pubkey.to_hex(),
        created_at: alice_profile.created_at.as_u64(),
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
    let alice_profile =
        EventBuilder::new(Kind::Metadata, r#"{"name":"Alice Published Search"}"#, [])
            .custom_created_at(Timestamp::from(11))
            .to_event(&alice_keys)
            .expect("alice profile");
    let stored = hashtree_nostr::StoredNostrEvent {
        id: alice_profile.id.to_hex(),
        pubkey: alice_profile.pubkey.to_hex(),
        created_at: alice_profile.created_at.as_u64(),
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
    let alice_note = EventBuilder::new(Kind::TextNote, "hello event tree", [])
        .custom_created_at(Timestamp::from(11))
        .to_event(&alice_keys)
        .expect("alice note");
    let stored = hashtree_nostr::StoredNostrEvent {
        id: alice_note.id.to_hex(),
        pubkey: alice_note.pubkey.to_hex(),
        created_at: alice_note.created_at.as_u64(),
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
    let alice_profile =
        EventBuilder::new(Kind::Metadata, r#"{"name":"Alice Existing Search"}"#, [])
            .custom_created_at(Timestamp::from(11))
            .to_event(&alice_keys)
            .expect("alice profile");
    let stored = hashtree_nostr::StoredNostrEvent {
        id: alice_profile.id.to_hex(),
        pubkey: alice_profile.pubkey.to_hex(),
        created_at: alice_profile.created_at.as_u64(),
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
    let alice_profile = EventBuilder::new(Kind::Metadata, r#"{"name":"Alice Checkpoint"}"#, [])
        .custom_created_at(Timestamp::from(11))
        .to_event(&alice_keys)
        .expect("alice profile");
    let alice_stored = hashtree_nostr::StoredNostrEvent {
        id: alice_profile.id.to_hex(),
        pubkey: alice_profile.pubkey.to_hex(),
        created_at: alice_profile.created_at.as_u64(),
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
    let call_index = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    mirror
        .history_sync_authors_chunked(vec!["author-a".to_string(), "author-b".to_string()], {
            let call_index = Arc::clone(&call_index);
            let root = root.clone();
            move |_current_root, author_chunk| {
                let call_index = Arc::clone(&call_index);
                let root = root.clone();
                std::future::ready(
                    match call_index.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                        0 => Ok(CrawlReport {
                            root: root.clone(),
                            authors_considered: 2,
                            authors_processed: author_chunk.len(),
                            events_seen: 1,
                            events_selected: 1,
                            live_bytes_selected: 0,
                        }),
                        _ => Err(anyhow::anyhow!("boom")),
                    },
                )
            }
        })
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
    let root_contacts = EventBuilder::new(Kind::ContactList, "", tags)
        .custom_created_at(Timestamp::from(10))
        .to_event(&root_keys)
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

    let follow = EventBuilder::new(
        Kind::ContactList,
        "",
        vec![Tag::public_key(target_keys.public_key())],
    )
    .custom_created_at(Timestamp::from_secs(10))
    .to_event(&root_keys)
    .expect("follow");
    crate::socialgraph::ingest_parsed_event(&graph_store, &follow)?;

    let mute = EventBuilder::new(
        Kind::MuteList,
        "",
        vec![Tag::public_key(target_keys.public_key())],
    )
    .custom_created_at(Timestamp::from_secs(11))
    .to_event(&root_keys)
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

    let root_profile = EventBuilder::new(Kind::Metadata, r#"{"name":"root"}"#, [])
        .custom_created_at(Timestamp::from_secs(5))
        .to_event(&root_keys)
        .expect("root profile");
    crate::socialgraph::ingest_parsed_event(&graph_store, &root_profile)?;

    let follow = EventBuilder::new(
        Kind::ContactList,
        "",
        vec![
            Tag::public_key(existing_keys.public_key()),
            Tag::public_key(missing_keys.public_key()),
        ],
    )
    .custom_created_at(Timestamp::from_secs(10))
    .to_event(&root_keys)
    .expect("follow");
    crate::socialgraph::ingest_parsed_event(&graph_store, &follow)?;

    let existing_profile = EventBuilder::new(Kind::Metadata, r#"{"name":"existing"}"#, [])
        .custom_created_at(Timestamp::from_secs(11))
        .to_event(&existing_keys)
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
    let alice_note = EventBuilder::new(Kind::TextNote, "hello live flush", [])
        .custom_created_at(Timestamp::from(21))
        .to_event(&alice_keys)
        .expect("alice note");

    mirror.ingest_live_event(&alice_note)?;
    mirror.flush_live_events().await?;

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
    let root_contacts = EventBuilder::new(
        Kind::ContactList,
        "",
        vec![Tag::public_key(alice_keys.public_key())],
    )
    .custom_created_at(Timestamp::from(10))
    .to_event(&root_keys)
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
    let updated_profile =
        EventBuilder::new(Kind::Metadata, r#"{"name":"Alice Mirror Updated"}"#, [])
            .to_event(&alice_keys)
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

    let root_profile = EventBuilder::new(Kind::Metadata, r#"{"name":"Root Out Of Band"}"#, [])
        .custom_created_at(Timestamp::from(42))
        .to_event(&root_keys)
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
