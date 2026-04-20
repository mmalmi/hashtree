use std::collections::HashMap;
use std::io;
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use hashtree_core::{MemoryStore, Store};
use hashtree_index::{BTree, BTreeOptions};
use hashtree_nostr::{
    CrawlConfig, ListEventsOptions, NostrBridge, NostrEventStore, RelayFetchMode, StoredNostrEvent,
};
use negentropy::{Id, Negentropy, NegentropyStorageVector};
use nostr::prelude::{
    ClientMessage, Event, EventBuilder, Filter, JsonUtil, Kind, RelayMessage, Tag, Timestamp,
};
use nostr_sdk::{Client, Keys};
use nostr_social_graph::{NostrEvent as GraphEvent, SocialGraph};
use tokio::net::TcpStream;
use tokio::sync::broadcast;
use tokio_tungstenite::{accept_async, tungstenite::Message};

#[derive(Debug, Default)]
struct SharedRelayState {
    events: Vec<Event>,
    requested_id_batches: Vec<Vec<String>>,
    supports_negentropy: bool,
    negentropy_open_attempts: usize,
    negentropy_sessions_started: usize,
    server_page_cap: Option<usize>,
    disconnect_on_id_request: bool,
}

struct TestRelay {
    port: u16,
    shutdown: broadcast::Sender<()>,
    state: Arc<Mutex<SharedRelayState>>,
}

impl TestRelay {
    fn new() -> Self {
        Self::with_negentropy(false)
    }

    fn with_negentropy(supports_negentropy: bool) -> Self {
        Self::with_options(supports_negentropy, None)
    }

    fn with_options(supports_negentropy: bool, server_page_cap: Option<usize>) -> Self {
        let state = Arc::new(Mutex::new(SharedRelayState {
            supports_negentropy,
            server_page_cap,
            ..SharedRelayState::default()
        }));
        let (shutdown, _) = broadcast::channel(1);

        let std_listener = TcpListener::bind("127.0.0.1:0").expect("bind relay listener");
        let port = std_listener.local_addr().expect("relay local addr").port();
        std_listener.set_nonblocking(true).expect("set nonblocking");

        let state_for_thread = Arc::clone(&state);
        let shutdown_for_thread = shutdown.clone();

        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("build tokio runtime");

            rt.block_on(async move {
                let listener =
                    tokio::net::TcpListener::from_std(std_listener).expect("tokio listener");
                let mut shutdown_rx = shutdown_for_thread.subscribe();

                loop {
                    tokio::select! {
                        _ = shutdown_rx.recv() => break,
                        accept = listener.accept() => {
                            if let Ok((stream, _)) = accept {
                                let state = Arc::clone(&state_for_thread);
                                tokio::spawn(async move {
                                    handle_connection(stream, state).await;
                                });
                            }
                        }
                    }
                }
            });
        });

        std::thread::sleep(Duration::from_millis(100));

        Self {
            port,
            shutdown,
            state,
        }
    }

    fn with_page_cap(server_page_cap: usize) -> Self {
        Self::with_options(false, Some(server_page_cap))
    }

    fn with_negentropy_disconnect_on_id_request() -> Self {
        let relay = Self::with_options(true, None);
        relay
            .state
            .lock()
            .expect("relay state lock")
            .disconnect_on_id_request = true;
        relay
    }

    fn url(&self) -> String {
        format!("ws://127.0.0.1:{}", self.port)
    }

    fn requested_id_batches(&self) -> Vec<Vec<String>> {
        self.state
            .lock()
            .expect("relay state lock")
            .requested_id_batches
            .clone()
    }

    fn negentropy_sessions_started(&self) -> usize {
        self.state
            .lock()
            .expect("relay state lock")
            .negentropy_sessions_started
    }

    fn negentropy_open_attempts(&self) -> usize {
        self.state
            .lock()
            .expect("relay state lock")
            .negentropy_open_attempts
    }
}

impl Drop for TestRelay {
    fn drop(&mut self) {
        let _ = self.shutdown.send(());
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn matching_events(state: &Arc<Mutex<SharedRelayState>>, filters: &[Filter]) -> Vec<Event> {
    let mut matched = state
        .lock()
        .expect("relay state lock")
        .events
        .clone()
        .into_iter()
        .filter(|event| {
            filters.is_empty() || filters.iter().any(|filter| filter.match_event(event))
        })
        .collect::<Vec<_>>();

    matched.sort_by(|left, right| {
        right
            .created_at
            .cmp(&left.created_at)
            .then_with(|| left.id.cmp(&right.id))
    });

    let server_page_cap = state.lock().expect("relay state lock").server_page_cap;
    let effective_limit = match (
        filters.iter().filter_map(|filter| filter.limit).min(),
        server_page_cap,
    ) {
        (Some(filter_limit), Some(server_limit)) => Some(filter_limit.min(server_limit)),
        (Some(filter_limit), None) => Some(filter_limit),
        (None, Some(server_limit)) => Some(server_limit),
        (None, None) => None,
    };
    if let Some(limit) = effective_limit {
        matched.truncate(limit);
    }

    matched
}

fn build_negentropy_storage(
    state: &Arc<Mutex<SharedRelayState>>,
    filter: &Filter,
) -> NegentropyStorageVector {
    let mut events = matching_events(state, std::slice::from_ref(filter));
    events.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.id.cmp(&right.id))
    });

    let mut storage = NegentropyStorageVector::with_capacity(events.len());
    for event in events {
        storage
            .insert(
                event.created_at.as_u64(),
                Id::from_slice(event.id.as_bytes()).expect("negentropy id"),
            )
            .expect("insert negentropy item");
    }
    storage.seal().expect("seal negentropy storage");
    storage
}

fn record_requested_ids(state: &Arc<Mutex<SharedRelayState>>, filters: &[Filter]) {
    let mut requested_ids = filters
        .iter()
        .filter_map(|filter| filter.ids.as_ref())
        .flat_map(|ids| ids.iter().map(|id| id.to_hex()))
        .collect::<Vec<_>>();
    if requested_ids.is_empty() {
        return;
    }
    requested_ids.sort();
    requested_ids.dedup();
    state
        .lock()
        .expect("relay state lock")
        .requested_id_batches
        .push(requested_ids);
}

async fn send_relay_message(
    write: &mut futures::stream::SplitSink<tokio_tungstenite::WebSocketStream<TcpStream>, Message>,
    message: RelayMessage,
) {
    let _ = write.send(Message::Text(message.as_json())).await;
}

async fn handle_connection(stream: TcpStream, state: Arc<Mutex<SharedRelayState>>) {
    let ws_stream = match accept_async(stream).await {
        Ok(ws) => ws,
        Err(_) => return,
    };

    let (mut write, mut read) = ws_stream.split();
    let mut negentropy_sessions: HashMap<String, Negentropy<'static, NegentropyStorageVector>> =
        HashMap::new();

    while let Some(msg) = read.next().await {
        let msg = match msg {
            Ok(Message::Text(text)) => text,
            Ok(Message::Ping(data)) => {
                let _ = write.send(Message::Pong(data)).await;
                continue;
            }
            Ok(Message::Close(_)) => break,
            _ => continue,
        };

        let parsed = match ClientMessage::from_json(msg.as_bytes()) {
            Ok(value) => value,
            Err(_) => continue,
        };

        match parsed {
            ClientMessage::Event(event) => {
                state
                    .lock()
                    .expect("relay state lock")
                    .events
                    .push(*event.clone());
                send_relay_message(&mut write, RelayMessage::ok(event.id, true, "")).await;
            }
            ClientMessage::Req {
                subscription_id,
                filters,
            } => {
                record_requested_ids(&state, &filters);
                let disconnect_on_id_request = {
                    let guard = state.lock().expect("relay state lock");
                    guard.disconnect_on_id_request
                        && filters.iter().any(|filter| filter.ids.as_ref().is_some())
                };
                if disconnect_on_id_request {
                    let _ = write.close().await;
                    break;
                }
                for event in matching_events(&state, &filters) {
                    send_relay_message(
                        &mut write,
                        RelayMessage::event(subscription_id.clone(), event),
                    )
                    .await;
                }
                send_relay_message(&mut write, RelayMessage::eose(subscription_id)).await;
            }
            ClientMessage::NegOpen {
                subscription_id,
                filter,
                initial_message,
                ..
            } => {
                let supports_negentropy = {
                    let mut guard = state.lock().expect("relay state lock");
                    guard.negentropy_open_attempts += 1;
                    guard.supports_negentropy
                };
                if !supports_negentropy {
                    send_relay_message(
                        &mut write,
                        RelayMessage::notice("negentropy not supported"),
                    )
                    .await;
                    continue;
                }

                let storage = build_negentropy_storage(&state, &filter);
                let mut negentropy =
                    Negentropy::owned(storage, 0).expect("build relay negentropy state");
                let response = negentropy
                    .reconcile(&hex::decode(initial_message).expect("parse negentropy open"))
                    .expect("reconcile negentropy open");

                state
                    .lock()
                    .expect("relay state lock")
                    .negentropy_sessions_started += 1;
                negentropy_sessions.insert(subscription_id.to_string(), negentropy);

                send_relay_message(
                    &mut write,
                    RelayMessage::NegMsg {
                        subscription_id,
                        message: hex::encode(response),
                    },
                )
                .await;
            }
            ClientMessage::NegMsg {
                subscription_id,
                message,
            } => {
                let Some(negentropy) = negentropy_sessions.get_mut(&subscription_id.to_string())
                else {
                    continue;
                };
                let response = negentropy
                    .reconcile(&hex::decode(message).expect("parse negentropy message"))
                    .expect("reconcile negentropy message");
                send_relay_message(
                    &mut write,
                    RelayMessage::NegMsg {
                        subscription_id,
                        message: hex::encode(response),
                    },
                )
                .await;
            }
            ClientMessage::NegClose { subscription_id } | ClientMessage::Close(subscription_id) => {
                negentropy_sessions.remove(&subscription_id.to_string());
            }
            ClientMessage::Count {
                subscription_id,
                filters,
            } => {
                let count = matching_events(&state, &filters).len();
                send_relay_message(&mut write, RelayMessage::count(subscription_id, count)).await;
            }
            ClientMessage::Auth(_) => {}
        }
    }
}

fn graph_event_from_nostr(event: &Event) -> GraphEvent {
    GraphEvent {
        created_at: event.created_at.as_u64(),
        content: event.content.clone(),
        tags: event
            .tags
            .iter()
            .map(|tag: &Tag| tag.as_slice().to_vec())
            .collect(),
        kind: event.kind.as_u16() as u32,
        pubkey: event.pubkey.to_hex(),
        id: event.id.to_hex(),
        sig: event.sig.to_string(),
    }
}

fn stored_event_from_nostr(event: &Event) -> StoredNostrEvent {
    StoredNostrEvent {
        id: event.id.to_hex(),
        pubkey: event.pubkey.to_hex(),
        created_at: event.created_at.as_u64(),
        kind: event.kind.as_u16() as u32,
        tags: event
            .tags
            .iter()
            .map(|tag: &Tag| tag.as_slice().to_vec())
            .collect(),
        content: event.content.clone(),
        sig: event.sig.to_string(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crawls_followed_authors_and_applies_per_author_priority_limit() -> io::Result<()> {
    let relay = TestRelay::new();
    let relay_url = relay.url();

    let root_keys = Keys::generate();
    let alice_keys = Keys::generate();
    let bob_keys = Keys::generate();

    let mut graph = SocialGraph::new(&root_keys.public_key().to_hex());
    let contact_list = EventBuilder::new(
        Kind::ContactList,
        "",
        [Tag::parse(&["p", &alice_keys.public_key().to_hex()]).expect("p tag")],
    )
    .custom_created_at(Timestamp::from_secs(10))
    .to_event(&root_keys)
    .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let publisher = Client::new(Keys::generate());
    publisher.add_relay(&relay_url).await.expect("add relay");
    publisher.connect().await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    let alice_old = EventBuilder::new(
        Kind::TextNote,
        "older nostr note",
        [Tag::parse(&["t", "nostr"]).expect("t tag")],
    )
    .custom_created_at(Timestamp::from_secs(20))
    .to_event(&alice_keys)
    .expect("alice old");
    let alice_new = EventBuilder::new(
        Kind::TextNote,
        "newer nostr note",
        [Tag::parse(&["t", "nostr"]).expect("t tag")],
    )
    .custom_created_at(Timestamp::from_secs(30))
    .to_event(&alice_keys)
    .expect("alice new");
    let alice_low_priority = EventBuilder::new(
        Kind::Custom(7),
        "reaction-ish",
        [Tag::parse(&["t", "nostr"]).expect("t tag")],
    )
    .custom_created_at(Timestamp::from_secs(40))
    .to_event(&alice_keys)
    .expect("alice low priority");
    let bob_note = EventBuilder::new(
        Kind::TextNote,
        "bob note",
        [Tag::parse(&["t", "nostr"]).expect("t tag")],
    )
    .custom_created_at(Timestamp::from_secs(50))
    .to_event(&bob_keys)
    .expect("bob note");

    for event in [&alice_old, &alice_new, &alice_low_priority, &bob_note] {
        publisher
            .send_event(event.clone())
            .await
            .expect("publish test event");
    }

    let store = Arc::new(MemoryStore::new());
    let bridge = NostrBridge::new(
        store.clone(),
        CrawlConfig {
            relays: vec![relay_url],
            per_author_event_limit: 2,
            kinds: Some(vec![1, 7]),
            ..CrawlConfig::default()
        },
    );

    let report = bridge.crawl(&graph, None).await.expect("crawl report");
    let root = report.root.expect("index root");
    let event_store = hashtree_nostr::NostrEventStore::new(store);

    let nostr_events = event_store
        .list_by_tag(
            Some(&root),
            "t",
            "nostr",
            ListEventsOptions {
                limit: Some(10),
                ..Default::default()
            },
        )
        .await
        .expect("query hashtag");

    assert_eq!(nostr_events.len(), 2);
    assert!(nostr_events
        .iter()
        .all(|event| event.pubkey == alice_keys.public_key().to_hex()));
    assert!(nostr_events.iter().all(|event| event.kind == 1));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enforces_global_live_byte_cap_after_priority_selection() -> io::Result<()> {
    let relay = TestRelay::new();
    let relay_url = relay.url();

    let root_keys = Keys::generate();
    let alice_keys = Keys::generate();

    let mut graph = SocialGraph::new(&root_keys.public_key().to_hex());
    let contact_list = EventBuilder::new(
        Kind::ContactList,
        "",
        [Tag::parse(&["p", &alice_keys.public_key().to_hex()]).expect("p tag")],
    )
    .custom_created_at(Timestamp::from_secs(10))
    .to_event(&root_keys)
    .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let publisher = Client::new(Keys::generate());
    publisher.add_relay(&relay_url).await.expect("add relay");
    publisher.connect().await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    let note_one = EventBuilder::new(
        Kind::TextNote,
        "note one",
        [Tag::parse(&["t", "nostr"]).expect("t tag")],
    )
    .custom_created_at(Timestamp::from_secs(20))
    .to_event(&alice_keys)
    .expect("note one");
    let note_two = EventBuilder::new(
        Kind::TextNote,
        "note two",
        [Tag::parse(&["t", "nostr"]).expect("t tag")],
    )
    .custom_created_at(Timestamp::from_secs(30))
    .to_event(&alice_keys)
    .expect("note two");
    let note_three = EventBuilder::new(
        Kind::TextNote,
        "note three",
        [Tag::parse(&["t", "nostr"]).expect("t tag")],
    )
    .custom_created_at(Timestamp::from_secs(40))
    .to_event(&alice_keys)
    .expect("note three");

    for event in [&note_one, &note_two, &note_three] {
        publisher
            .send_event(event.clone())
            .await
            .expect("publish test event");
    }

    let sizing_store = NostrEventStore::new(Arc::new(MemoryStore::new()));
    let retained_size = sizing_store
        .encode_event(&stored_event_from_nostr(&note_three))
        .expect("encode newest")
        .len() as u64
        + sizing_store
            .encode_event(&stored_event_from_nostr(&note_two))
            .expect("encode middle")
            .len() as u64;

    let store = Arc::new(MemoryStore::new());
    let bridge = NostrBridge::new(
        store.clone(),
        CrawlConfig {
            relays: vec![relay_url],
            per_author_event_limit: 8,
            max_live_bytes: Some(retained_size),
            kinds: Some(vec![1]),
            ..CrawlConfig::default()
        },
    );

    let report = bridge.crawl(&graph, None).await.expect("crawl report");
    let root = report.root.expect("index root");
    let event_store = NostrEventStore::new(store);

    let nostr_events = event_store
        .list_by_tag(
            Some(&root),
            "t",
            "nostr",
            ListEventsOptions {
                limit: Some(10),
                ..Default::default()
            },
        )
        .await
        .expect("query hashtag");

    assert_eq!(report.events_selected, 2);
    assert_eq!(nostr_events.len(), 2);
    assert_eq!(nostr_events[0].id, note_three.id.to_hex());
    assert_eq!(nostr_events[1].id, note_two.id.to_hex());

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enforces_per_author_live_byte_cap_after_priority_selection() -> io::Result<()> {
    let relay = TestRelay::new();
    let relay_url = relay.url();

    let root_keys = Keys::generate();
    let alice_keys = Keys::generate();

    let mut graph = SocialGraph::new(&root_keys.public_key().to_hex());
    let contact_list = EventBuilder::new(
        Kind::ContactList,
        "",
        [Tag::parse(&["p", &alice_keys.public_key().to_hex()]).expect("p tag")],
    )
    .custom_created_at(Timestamp::from_secs(10))
    .to_event(&root_keys)
    .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let publisher = Client::new(Keys::generate());
    publisher.add_relay(&relay_url).await.expect("add relay");
    publisher.connect().await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    let note_one = EventBuilder::new(
        Kind::TextNote,
        "note one",
        [Tag::parse(&["t", "nostr"]).expect("t tag")],
    )
    .custom_created_at(Timestamp::from_secs(20))
    .to_event(&alice_keys)
    .expect("note one");
    let note_two = EventBuilder::new(
        Kind::TextNote,
        "note two",
        [Tag::parse(&["t", "nostr"]).expect("t tag")],
    )
    .custom_created_at(Timestamp::from_secs(30))
    .to_event(&alice_keys)
    .expect("note two");
    let note_three = EventBuilder::new(
        Kind::TextNote,
        "note three",
        [Tag::parse(&["t", "nostr"]).expect("t tag")],
    )
    .custom_created_at(Timestamp::from_secs(40))
    .to_event(&alice_keys)
    .expect("note three");

    for event in [&note_one, &note_two, &note_three] {
        publisher
            .send_event(event.clone())
            .await
            .expect("publish test event");
    }

    let sizing_store = NostrEventStore::new(Arc::new(MemoryStore::new()));
    let retained_size = sizing_store
        .encode_event(&stored_event_from_nostr(&note_three))
        .expect("encode newest")
        .len() as u64
        + sizing_store
            .encode_event(&stored_event_from_nostr(&note_two))
            .expect("encode middle")
            .len() as u64;

    let store = Arc::new(MemoryStore::new());
    let bridge = NostrBridge::new(
        store.clone(),
        CrawlConfig {
            relays: vec![relay_url],
            per_author_event_limit: 8,
            per_author_live_bytes: Some(retained_size),
            kinds: Some(vec![1]),
            ..CrawlConfig::default()
        },
    );

    let report = bridge.crawl(&graph, None).await.expect("crawl report");
    let root = report.root.expect("index root");
    let event_store = NostrEventStore::new(store);

    let nostr_events = event_store
        .list_by_tag(
            Some(&root),
            "t",
            "nostr",
            ListEventsOptions {
                limit: Some(10),
                ..Default::default()
            },
        )
        .await
        .expect("query hashtag");

    assert_eq!(report.events_selected, 2);
    assert_eq!(nostr_events.len(), 2);
    assert_eq!(nostr_events[0].id, note_three.id.to_hex());
    assert_eq!(nostr_events[1].id, note_two.id.to_hex());

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn limits_relay_fetches_per_author_batch() -> io::Result<()> {
    let relay = TestRelay::new();
    let relay_url = relay.url();

    let root_keys = Keys::generate();
    let alice_keys = Keys::generate();

    let mut graph = SocialGraph::new(&root_keys.public_key().to_hex());
    let contact_list = EventBuilder::new(
        Kind::ContactList,
        "",
        [Tag::parse(&["p", &alice_keys.public_key().to_hex()]).expect("p tag")],
    )
    .custom_created_at(Timestamp::from_secs(10))
    .to_event(&root_keys)
    .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let publisher = Client::new(Keys::generate());
    publisher.add_relay(&relay_url).await.expect("add relay");
    publisher.connect().await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    for created_at in 20..25 {
        let note = EventBuilder::new(
            Kind::TextNote,
            format!("note {created_at}"),
            [Tag::parse(&["t", "nostr"]).expect("t tag")],
        )
        .custom_created_at(Timestamp::from_secs(created_at))
        .to_event(&alice_keys)
        .expect("note");
        publisher
            .send_event(note)
            .await
            .expect("publish test event");
    }

    let store = Arc::new(MemoryStore::new());
    let bridge = NostrBridge::new(
        store,
        CrawlConfig {
            relays: vec![relay_url],
            author_batch_size: 1,
            per_author_event_limit: 2,
            kinds: Some(vec![1]),
            ..CrawlConfig::default()
        },
    );

    let report = bridge.crawl(&graph, None).await.expect("crawl report");
    assert_eq!(report.events_seen, 2);
    assert_eq!(report.events_selected, 2);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn caches_relays_that_do_not_support_negentropy() -> io::Result<()> {
    let relay = TestRelay::new();
    let relay_url = relay.url();

    let root_keys = Keys::generate();
    let alice_keys = Keys::generate();
    let bob_keys = Keys::generate();

    let mut graph = SocialGraph::new(&root_keys.public_key().to_hex());
    let contact_list = EventBuilder::new(
        Kind::ContactList,
        "",
        [
            Tag::parse(&["p", &alice_keys.public_key().to_hex()]).expect("alice p tag"),
            Tag::parse(&["p", &bob_keys.public_key().to_hex()]).expect("bob p tag"),
        ],
    )
    .custom_created_at(Timestamp::from_secs(10))
    .to_event(&root_keys)
    .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let publisher = Client::new(Keys::generate());
    publisher.add_relay(&relay_url).await.expect("add relay");
    publisher.connect().await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    for (created_at, keys) in [(20, &alice_keys), (21, &bob_keys)] {
        let note = EventBuilder::new(Kind::TextNote, format!("note {created_at}"), [])
            .custom_created_at(Timestamp::from_secs(created_at))
            .to_event(keys)
            .expect("note");
        publisher
            .send_event(note)
            .await
            .expect("publish test event");
    }

    let store = Arc::new(MemoryStore::new());
    let bridge = NostrBridge::new(
        store,
        CrawlConfig {
            relays: vec![relay_url],
            author_batch_size: 1,
            per_author_event_limit: 4,
            kinds: Some(vec![1]),
            ..CrawlConfig::default()
        },
    );

    let report = bridge.crawl(&graph, None).await.expect("crawl report");
    assert_eq!(report.events_selected, 2);
    assert_eq!(relay.negentropy_open_attempts(), 1);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn require_negentropy_skips_relays_that_cannot_reconcile() -> io::Result<()> {
    let fallback_relay = TestRelay::new();
    let supported_relay = TestRelay::with_negentropy(true);

    let root_keys = Keys::generate();
    let alice_keys = Keys::generate();

    let mut graph = SocialGraph::new(&root_keys.public_key().to_hex());
    let contact_list = EventBuilder::new(
        Kind::ContactList,
        "",
        [Tag::parse(&["p", &alice_keys.public_key().to_hex()]).expect("p tag")],
    )
    .custom_created_at(Timestamp::from_secs(10))
    .to_event(&root_keys)
    .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let note = EventBuilder::new(Kind::TextNote, "alice note", [])
        .custom_created_at(Timestamp::from_secs(20))
        .to_event(&alice_keys)
        .expect("note");

    let publisher = Client::new(Keys::generate());
    publisher
        .add_relay(&supported_relay.url())
        .await
        .expect("add supported relay");
    publisher.connect().await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    publisher
        .send_event(note.clone())
        .await
        .expect("publish event");

    let store = Arc::new(MemoryStore::new());
    let bridge = NostrBridge::new(
        store.clone(),
        CrawlConfig {
            relays: vec![fallback_relay.url(), supported_relay.url()],
            author_batch_size: 1,
            per_author_event_limit: 4,
            kinds: Some(vec![1]),
            require_negentropy: true,
            ..CrawlConfig::default()
        },
    );

    let report = bridge.crawl(&graph, None).await.expect("crawl report");
    let root = report.root.expect("index root");
    let retained = NostrEventStore::new(store)
        .list_recent(
            Some(&root),
            ListEventsOptions {
                limit: Some(10),
                ..Default::default()
            },
        )
        .await
        .expect("list retained");

    assert_eq!(report.events_selected, 1);
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0].id, note.id.to_hex());
    assert_eq!(fallback_relay.negentropy_open_attempts(), 1);
    assert!(fallback_relay.requested_id_batches().is_empty());
    assert!(supported_relay.negentropy_sessions_started() >= 1);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_disconnect_during_missing_id_fetch_does_not_abort_crawl() -> io::Result<()> {
    let flaky_relay = TestRelay::with_negentropy_disconnect_on_id_request();
    let good_relay = TestRelay::with_negentropy(true);

    let root_keys = Keys::generate();
    let alice_keys = Keys::generate();

    let mut graph = SocialGraph::new(&root_keys.public_key().to_hex());
    let contact_list = EventBuilder::new(
        Kind::ContactList,
        "",
        [Tag::parse(&["p", &alice_keys.public_key().to_hex()]).expect("p tag")],
    )
    .custom_created_at(Timestamp::from_secs(10))
    .to_event(&root_keys)
    .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let note = EventBuilder::new(Kind::TextNote, "alice note", [])
        .custom_created_at(Timestamp::from_secs(20))
        .to_event(&alice_keys)
        .expect("note");

    let flaky_publisher = Client::new(Keys::generate());
    flaky_publisher
        .add_relay(&flaky_relay.url())
        .await
        .expect("add flaky relay");
    flaky_publisher.connect().await;

    let good_publisher = Client::new(Keys::generate());
    good_publisher
        .add_relay(&good_relay.url())
        .await
        .expect("add good relay");
    good_publisher.connect().await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    for publisher in [&flaky_publisher, &good_publisher] {
        publisher
            .send_event(note.clone())
            .await
            .expect("publish note");
    }

    let store = Arc::new(MemoryStore::new());
    let bridge = NostrBridge::new(
        store.clone(),
        CrawlConfig {
            relays: vec![flaky_relay.url(), good_relay.url()],
            author_batch_size: 1,
            per_author_event_limit: 4,
            kinds: Some(vec![1]),
            require_negentropy: true,
            ..CrawlConfig::default()
        },
    );

    let report = bridge.crawl(&graph, None).await.expect("crawl report");
    let root = report.root.expect("index root");
    let retained = NostrEventStore::new(store)
        .list_recent(
            Some(&root),
            ListEventsOptions {
                limit: Some(10),
                ..Default::default()
            },
        )
        .await
        .expect("list retained");

    assert_eq!(report.events_selected, 1);
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0].id, note.id.to_hex());
    assert!(flaky_relay.negentropy_sessions_started() >= 1);
    assert!(good_relay.negentropy_sessions_started() >= 1);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_event_max_size_allows_moderately_large_events() -> io::Result<()> {
    let relay = TestRelay::new();
    let relay_url = relay.url();

    let root_keys = Keys::generate();
    let alice_keys = Keys::generate();

    let mut graph = SocialGraph::new(&root_keys.public_key().to_hex());
    let contact_list = EventBuilder::new(
        Kind::ContactList,
        "",
        [Tag::parse(&["p", &alice_keys.public_key().to_hex()]).expect("p tag")],
    )
    .custom_created_at(Timestamp::from_secs(10))
    .to_event(&root_keys)
    .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let publisher = Client::new(Keys::generate());
    publisher.add_relay(&relay_url).await.expect("add relay");
    publisher.connect().await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    let large_note = EventBuilder::new(Kind::TextNote, "x".repeat(90_000), [])
        .custom_created_at(Timestamp::from_secs(20))
        .to_event(&alice_keys)
        .expect("large note");
    publisher
        .send_event(large_note.clone())
        .await
        .expect("publish large event");

    let store = Arc::new(MemoryStore::new());
    let bridge = NostrBridge::new(
        store.clone(),
        CrawlConfig {
            relays: vec![relay_url],
            per_author_event_limit: 4,
            kinds: Some(vec![1]),
            relay_event_max_size: Some(128_000),
            ..CrawlConfig::default()
        },
    );

    let report = bridge.crawl(&graph, None).await.expect("crawl report");
    let root = report.root.expect("index root");
    let retained = NostrEventStore::new(store)
        .list_recent(
            Some(&root),
            ListEventsOptions {
                limit: Some(10),
                ..Default::default()
            },
        )
        .await
        .expect("list retained");

    assert_eq!(report.events_selected, 1);
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0].id, large_note.id.to_hex());

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn global_recent_scan_filters_locally_by_social_graph() -> io::Result<()> {
    let relay = TestRelay::new();
    let relay_url = relay.url();

    let root_keys = Keys::generate();
    let alice_keys = Keys::generate();
    let bob_keys = Keys::generate();

    let mut graph = SocialGraph::new(&root_keys.public_key().to_hex());
    let contact_list = EventBuilder::new(
        Kind::ContactList,
        "",
        [Tag::parse(&["p", &alice_keys.public_key().to_hex()]).expect("p tag")],
    )
    .custom_created_at(Timestamp::from_secs(10))
    .to_event(&root_keys)
    .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let publisher = Client::new(Keys::generate());
    publisher.add_relay(&relay_url).await.expect("add relay");
    publisher.connect().await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    let mut events = Vec::new();
    for created_at in 20..22 {
        events.push(
            EventBuilder::new(Kind::TextNote, format!("alice {created_at}"), [])
                .custom_created_at(Timestamp::from_secs(created_at))
                .to_event(&alice_keys)
                .expect("alice note"),
        );
    }
    for created_at in 30..33 {
        events.push(
            EventBuilder::new(Kind::TextNote, format!("bob {created_at}"), [])
                .custom_created_at(Timestamp::from_secs(created_at))
                .to_event(&bob_keys)
                .expect("bob note"),
        );
    }
    for event in events {
        publisher
            .send_event(event)
            .await
            .expect("publish test event");
    }

    let store = Arc::new(MemoryStore::new());
    let bridge = NostrBridge::new(
        store.clone(),
        CrawlConfig {
            relays: vec![relay_url],
            relay_fetch_mode: RelayFetchMode::GlobalRecent,
            relay_page_size: 16,
            max_relay_pages: 1,
            per_author_event_limit: 8,
            kinds: Some(vec![1]),
            ..CrawlConfig::default()
        },
    );

    let report = bridge.crawl(&graph, None).await.expect("crawl report");
    let root = report.root.expect("index root");
    let event_store = NostrEventStore::new(store);
    let retained = event_store
        .list_recent(
            Some(&root),
            ListEventsOptions {
                limit: Some(10),
                ..Default::default()
            },
        )
        .await
        .expect("list retained");

    assert_eq!(report.events_seen, 5);
    assert_eq!(report.events_selected, 2);
    assert_eq!(retained.len(), 2);
    assert!(retained
        .iter()
        .all(|event| event.pubkey == alice_keys.public_key().to_hex()));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reports_global_recent_progress() -> io::Result<()> {
    let relay = TestRelay::new();
    let relay_url = relay.url();

    let root_keys = Keys::generate();
    let alice_keys = Keys::generate();

    let mut graph = SocialGraph::new(&root_keys.public_key().to_hex());
    let contact_list = EventBuilder::new(
        Kind::ContactList,
        "",
        [Tag::parse(&["p", &alice_keys.public_key().to_hex()]).expect("p tag")],
    )
    .custom_created_at(Timestamp::from_secs(10))
    .to_event(&root_keys)
    .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let publisher = Client::new(Keys::generate());
    publisher.add_relay(&relay_url).await.expect("add relay");
    publisher.connect().await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    for created_at in 20..22 {
        let note = EventBuilder::new(Kind::TextNote, format!("alice {created_at}"), [])
            .custom_created_at(Timestamp::from_secs(created_at))
            .to_event(&alice_keys)
            .expect("alice note");
        publisher
            .send_event(note)
            .await
            .expect("publish test event");
    }

    let store = Arc::new(MemoryStore::new());
    let bridge = NostrBridge::new(
        store,
        CrawlConfig {
            relays: vec![relay_url],
            relay_fetch_mode: RelayFetchMode::GlobalRecent,
            relay_page_size: 1,
            max_relay_pages: 4,
            per_author_event_limit: 8,
            kinds: Some(vec![1]),
            ..CrawlConfig::default()
        },
    );

    let mut progress = Vec::new();
    let report = bridge
        .crawl_with_progress(&graph, None, |checkpoint| progress.push(checkpoint.clone()))
        .await
        .expect("crawl report");

    assert!(progress.len() >= 2);
    assert!(progress.iter().skip(1).any(|item| item.root.is_some()));
    assert!(progress
        .iter()
        .take(progress.len() - 1)
        .all(|item| item.events_seen > 0));
    assert!(report.root.is_some());
    assert_eq!(progress.last(), Some(&report));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn global_recent_scan_can_use_external_author_allowlist() -> io::Result<()> {
    let relay = TestRelay::new();
    let relay_url = relay.url();

    let root_keys = Keys::generate();
    let alice_keys = Keys::generate();
    let bob_keys = Keys::generate();
    let graph = SocialGraph::new(&root_keys.public_key().to_hex());

    let publisher = Client::new(Keys::generate());
    publisher.add_relay(&relay_url).await.expect("add relay");
    publisher.connect().await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    for (keys, created_at, label) in [(&alice_keys, 20, "alice"), (&bob_keys, 21, "bob")] {
        let note = EventBuilder::new(Kind::TextNote, label, [])
            .custom_created_at(Timestamp::from_secs(created_at))
            .to_event(keys)
            .expect("note");
        publisher
            .send_event(note)
            .await
            .expect("publish test event");
    }

    let store = Arc::new(MemoryStore::new());
    let bridge = NostrBridge::new(
        store.clone(),
        CrawlConfig {
            relays: vec![relay_url],
            author_allowlist: Some(vec![alice_keys.public_key().to_hex()]),
            relay_fetch_mode: RelayFetchMode::GlobalRecent,
            relay_page_size: 16,
            max_relay_pages: 1,
            per_author_event_limit: 8,
            kinds: Some(vec![1]),
            ..CrawlConfig::default()
        },
    );

    let report = bridge.crawl(&graph, None).await.expect("crawl report");
    let root = report.root.expect("index root");
    let event_store = NostrEventStore::new(store);
    let retained = event_store
        .list_recent(
            Some(&root),
            ListEventsOptions {
                limit: Some(10),
                ..Default::default()
            },
        )
        .await
        .expect("list retained");

    assert_eq!(report.authors_considered, 1);
    assert_eq!(report.events_seen, 2);
    assert_eq!(report.events_selected, 1);
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0].pubkey, alice_keys.public_key().to_hex());

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn global_recent_scan_paginates_older_pages() -> io::Result<()> {
    let relay = TestRelay::new();
    let relay_url = relay.url();

    let root_keys = Keys::generate();
    let alice_keys = Keys::generate();

    let mut graph = SocialGraph::new(&root_keys.public_key().to_hex());
    let contact_list = EventBuilder::new(
        Kind::ContactList,
        "",
        [Tag::parse(&["p", &alice_keys.public_key().to_hex()]).expect("p tag")],
    )
    .custom_created_at(Timestamp::from_secs(10))
    .to_event(&root_keys)
    .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let publisher = Client::new(Keys::generate());
    publisher.add_relay(&relay_url).await.expect("add relay");
    publisher.connect().await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    for created_at in 20..25 {
        let note = EventBuilder::new(Kind::TextNote, format!("note {created_at}"), [])
            .custom_created_at(Timestamp::from_secs(created_at))
            .to_event(&alice_keys)
            .expect("note");
        publisher
            .send_event(note)
            .await
            .expect("publish test event");
    }

    let store = Arc::new(MemoryStore::new());
    let bridge = NostrBridge::new(
        store,
        CrawlConfig {
            relays: vec![relay_url],
            relay_fetch_mode: RelayFetchMode::GlobalRecent,
            relay_page_size: 2,
            max_relay_pages: 3,
            per_author_event_limit: 8,
            kinds: Some(vec![1]),
            ..CrawlConfig::default()
        },
    );

    let report = bridge.crawl(&graph, None).await.expect("crawl report");
    assert_eq!(report.events_seen, 5);
    assert_eq!(report.events_selected, 5);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn global_recent_scan_pages_past_relay_side_caps() -> io::Result<()> {
    let relay = TestRelay::with_page_cap(2);
    let relay_url = relay.url();

    let root_keys = Keys::generate();
    let alice_keys = Keys::generate();

    let mut graph = SocialGraph::new(&root_keys.public_key().to_hex());
    let contact_list = EventBuilder::new(
        Kind::ContactList,
        "",
        [Tag::parse(&["p", &alice_keys.public_key().to_hex()]).expect("p tag")],
    )
    .custom_created_at(Timestamp::from_secs(10))
    .to_event(&root_keys)
    .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let publisher = Client::new(Keys::generate());
    publisher.add_relay(&relay_url).await.expect("add relay");
    publisher.connect().await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    for created_at in 20..25 {
        let note = EventBuilder::new(Kind::TextNote, format!("note {created_at}"), [])
            .custom_created_at(Timestamp::from_secs(created_at))
            .to_event(&alice_keys)
            .expect("note");
        publisher
            .send_event(note)
            .await
            .expect("publish test event");
    }

    let store = Arc::new(MemoryStore::new());
    let bridge = NostrBridge::new(
        store,
        CrawlConfig {
            relays: vec![relay_url],
            relay_fetch_mode: RelayFetchMode::GlobalRecent,
            relay_page_size: 5,
            max_relay_pages: 4,
            per_author_event_limit: 8,
            kinds: Some(vec![1]),
            ..CrawlConfig::default()
        },
    );

    let report = bridge.crawl(&graph, None).await.expect("crawl report");
    assert_eq!(report.events_seen, 5);
    assert_eq!(report.events_selected, 5);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn global_recent_scan_stops_after_max_events_seen() -> io::Result<()> {
    let relay = TestRelay::new();
    let relay_url = relay.url();

    let root_keys = Keys::generate();
    let alice_keys = Keys::generate();

    let mut graph = SocialGraph::new(&root_keys.public_key().to_hex());
    let contact_list = EventBuilder::new(
        Kind::ContactList,
        "",
        [Tag::parse(&["p", &alice_keys.public_key().to_hex()]).expect("p tag")],
    )
    .custom_created_at(Timestamp::from_secs(10))
    .to_event(&root_keys)
    .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let publisher = Client::new(Keys::generate());
    publisher.add_relay(&relay_url).await.expect("add relay");
    publisher.connect().await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    for created_at in 20..25 {
        let note = EventBuilder::new(Kind::TextNote, format!("note {created_at}"), [])
            .custom_created_at(Timestamp::from_secs(created_at))
            .to_event(&alice_keys)
            .expect("note");
        publisher
            .send_event(note)
            .await
            .expect("publish test event");
    }

    let store = Arc::new(MemoryStore::new());
    let bridge = NostrBridge::new(
        store,
        CrawlConfig {
            relays: vec![relay_url],
            relay_fetch_mode: RelayFetchMode::GlobalRecent,
            relay_page_size: 2,
            max_relay_pages: 10,
            max_events_seen: Some(3),
            per_author_event_limit: 8,
            kinds: Some(vec![1]),
            ..CrawlConfig::default()
        },
    );

    let report = bridge.crawl(&graph, None).await.expect("crawl report");
    assert_eq!(report.events_seen, 4);
    assert_eq!(report.events_selected, 4);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconciles_per_relay_and_fetches_only_missing_ids() -> io::Result<()> {
    let relay_one = TestRelay::with_negentropy(true);
    let relay_two = TestRelay::with_negentropy(true);

    let root_keys = Keys::generate();
    let alice_keys = Keys::generate();

    let mut graph = SocialGraph::new(&root_keys.public_key().to_hex());
    let contact_list = EventBuilder::new(
        Kind::ContactList,
        "",
        [Tag::parse(&["p", &alice_keys.public_key().to_hex()]).expect("p tag")],
    )
    .custom_created_at(Timestamp::from_secs(10))
    .to_event(&root_keys)
    .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let note_one = EventBuilder::new(Kind::TextNote, "note one", [])
        .custom_created_at(Timestamp::from_secs(20))
        .to_event(&alice_keys)
        .expect("note one");
    let note_two = EventBuilder::new(Kind::TextNote, "note two", [])
        .custom_created_at(Timestamp::from_secs(30))
        .to_event(&alice_keys)
        .expect("note two");
    let note_three = EventBuilder::new(Kind::TextNote, "note three", [])
        .custom_created_at(Timestamp::from_secs(40))
        .to_event(&alice_keys)
        .expect("note three");

    let publisher_one = Client::new(Keys::generate());
    publisher_one
        .add_relay(&relay_one.url())
        .await
        .expect("add relay one");
    publisher_one.connect().await;

    let publisher_two = Client::new(Keys::generate());
    publisher_two
        .add_relay(&relay_two.url())
        .await
        .expect("add relay two");
    publisher_two.connect().await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    for event in [&note_one, &note_two] {
        publisher_one
            .send_event(event.clone())
            .await
            .expect("publish relay one event");
    }

    for event in [&note_one, &note_two, &note_three] {
        publisher_two
            .send_event(event.clone())
            .await
            .expect("publish relay two event");
    }

    let store = Arc::new(MemoryStore::new());
    let event_store = NostrEventStore::new(store.clone());
    let existing_root = event_store
        .build(None, vec![stored_event_from_nostr(&note_one)])
        .await
        .expect("build existing root")
        .expect("existing root cid");

    let bridge = NostrBridge::new(
        store.clone(),
        CrawlConfig {
            relays: vec![relay_one.url(), relay_two.url()],
            author_batch_size: 1,
            per_author_event_limit: 8,
            kinds: Some(vec![1]),
            ..CrawlConfig::default()
        },
    );

    let report = bridge
        .crawl(&graph, Some(&existing_root))
        .await
        .expect("crawl report");
    let root = report.root.expect("index root");
    let retained = event_store
        .list_by_author(
            Some(&root),
            &alice_keys.public_key().to_hex(),
            ListEventsOptions::default(),
        )
        .await
        .expect("list retained events");

    assert_eq!(report.events_seen, 2);
    assert_eq!(report.events_selected, 3);
    assert_eq!(retained.len(), 3);
    assert!(relay_one.negentropy_sessions_started() >= 1);
    assert!(relay_two.negentropy_sessions_started() >= 1);
    assert_eq!(
        relay_one.requested_id_batches(),
        vec![vec![note_two.id.to_hex()]]
    );
    assert_eq!(
        relay_two.requested_id_batches(),
        vec![vec![note_three.id.to_hex()]]
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn limits_authors_considered_by_bfs_order() -> io::Result<()> {
    let relay = TestRelay::new();
    let relay_url = relay.url();

    let root_keys = Keys::generate();
    let alice_keys = Keys::generate();
    let bob_keys = Keys::generate();
    let carol_keys = Keys::generate();

    let mut graph = SocialGraph::new(&root_keys.public_key().to_hex());
    let root_contact_list = EventBuilder::new(
        Kind::ContactList,
        "",
        [
            Tag::parse(&["p", &alice_keys.public_key().to_hex()]).expect("alice p tag"),
            Tag::parse(&["p", &bob_keys.public_key().to_hex()]).expect("bob p tag"),
            Tag::parse(&["p", &carol_keys.public_key().to_hex()]).expect("carol p tag"),
        ],
    )
    .custom_created_at(Timestamp::from_secs(10))
    .to_event(&root_keys)
    .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&root_contact_list), true, 1.0);

    let publisher = Client::new(Keys::generate());
    publisher.add_relay(&relay_url).await.expect("add relay");
    publisher.connect().await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    let alice_note = EventBuilder::new(Kind::TextNote, "alice", [])
        .custom_created_at(Timestamp::from_secs(20))
        .to_event(&alice_keys)
        .expect("alice note");
    let bob_note = EventBuilder::new(Kind::TextNote, "bob", [])
        .custom_created_at(Timestamp::from_secs(21))
        .to_event(&bob_keys)
        .expect("bob note");
    let carol_note = EventBuilder::new(Kind::TextNote, "carol", [])
        .custom_created_at(Timestamp::from_secs(22))
        .to_event(&carol_keys)
        .expect("carol note");

    for event in [&alice_note, &bob_note, &carol_note] {
        publisher
            .send_event(event.clone())
            .await
            .expect("publish test event");
    }

    let store = Arc::new(MemoryStore::new());
    let bridge = NostrBridge::new(
        store.clone(),
        CrawlConfig {
            relays: vec![relay_url],
            max_authors: Some(2),
            author_batch_size: 1,
            per_author_event_limit: 4,
            kinds: Some(vec![1]),
            ..CrawlConfig::default()
        },
    );

    let report = bridge.crawl(&graph, None).await.expect("crawl report");
    let root = report.root.expect("index root");
    let event_store = NostrEventStore::new(store);
    let recent = event_store
        .list_recent(
            Some(&root),
            ListEventsOptions {
                limit: Some(10),
                ..Default::default()
            },
        )
        .await
        .expect("list recent");

    assert_eq!(report.authors_considered, 2);
    assert_eq!(recent.len(), 1);
    let retained_id = recent[0].id.as_str();
    assert!(
        retained_id == alice_note.id.to_hex()
            || retained_id == bob_note.id.to_hex()
            || retained_id == carol_note.id.to_hex()
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reports_author_batch_progress() -> io::Result<()> {
    let relay = TestRelay::new();
    let relay_url = relay.url();

    let root_keys = Keys::generate();
    let alice_keys = Keys::generate();
    let bob_keys = Keys::generate();

    let mut graph = SocialGraph::new(&root_keys.public_key().to_hex());
    let root_contact_list = EventBuilder::new(
        Kind::ContactList,
        "",
        [
            Tag::parse(&["p", &alice_keys.public_key().to_hex()]).expect("alice p tag"),
            Tag::parse(&["p", &bob_keys.public_key().to_hex()]).expect("bob p tag"),
        ],
    )
    .custom_created_at(Timestamp::from_secs(10))
    .to_event(&root_keys)
    .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&root_contact_list), true, 1.0);

    let publisher = Client::new(Keys::generate());
    publisher.add_relay(&relay_url).await.expect("add relay");
    publisher.connect().await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    for (keys, content, created_at) in [(&alice_keys, "alice", 20u64), (&bob_keys, "bob", 21u64)] {
        let note = EventBuilder::new(Kind::TextNote, content, [])
            .custom_created_at(Timestamp::from_secs(created_at))
            .to_event(keys)
            .expect("note");
        publisher
            .send_event(note)
            .await
            .expect("publish test event");
    }

    let store = Arc::new(MemoryStore::new());
    let bridge = NostrBridge::new(
        store,
        CrawlConfig {
            relays: vec![relay_url],
            author_batch_size: 1,
            per_author_event_limit: 4,
            kinds: Some(vec![1]),
            ..CrawlConfig::default()
        },
    );

    let mut progress = Vec::new();
    let report = bridge
        .crawl_with_progress(&graph, None, |checkpoint| progress.push(checkpoint.clone()))
        .await
        .expect("crawl report");

    assert_eq!(report.authors_processed, 3);
    assert_eq!(progress.len(), 3);
    assert_eq!(progress[0].authors_processed, 1);
    assert_eq!(progress[1].authors_processed, 2);
    assert_eq!(progress[2].authors_processed, 3);
    assert!(progress.iter().skip(1).all(|item| item.root.is_some()));
    assert_eq!(progress.last(), Some(&report));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ignores_missing_local_event_blobs_from_existing_root_in_global_scan() -> io::Result<()> {
    let relay = TestRelay::new();
    let relay_url = relay.url();

    let root_keys = Keys::generate();
    let alice_keys = Keys::generate();

    let mut graph = SocialGraph::new(&root_keys.public_key().to_hex());
    let contact_list = EventBuilder::new(
        Kind::ContactList,
        "",
        [Tag::parse(&["p", &alice_keys.public_key().to_hex()]).expect("alice p tag")],
    )
    .custom_created_at(Timestamp::from_secs(10))
    .to_event(&root_keys)
    .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let old_note = EventBuilder::new(Kind::TextNote, "old", [])
        .custom_created_at(Timestamp::from_secs(20))
        .to_event(&alice_keys)
        .expect("old note");
    let new_note = EventBuilder::new(Kind::TextNote, "new", [])
        .custom_created_at(Timestamp::from_secs(21))
        .to_event(&alice_keys)
        .expect("new note");

    let publisher = Client::new(Keys::generate());
    publisher.add_relay(&relay_url).await.expect("add relay");
    publisher.connect().await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    publisher
        .send_event(new_note.clone())
        .await
        .expect("publish new note");

    let store = Arc::new(MemoryStore::new());
    let event_store = NostrEventStore::new(store.clone());
    let old_stored = stored_event_from_nostr(&old_note);
    let existing_root = event_store
        .build(None, vec![old_stored.clone()])
        .await
        .expect("build root")
        .expect("existing root");
    let manifest = event_store
        .get_manifest(Some(&existing_root))
        .await
        .expect("get manifest");
    let by_id = manifest.by_id.as_ref().expect("by-id root");
    let index = BTree::new(store.clone(), BTreeOptions::default());
    let old_event_cid = index
        .get_link(Some(by_id), &old_stored.id)
        .await
        .expect("get old event cid")
        .expect("old event cid");
    let deleted = store
        .delete(&old_event_cid.hash)
        .await
        .expect("delete old blob");
    assert!(deleted);

    let bridge = NostrBridge::new(
        store.clone(),
        CrawlConfig {
            relays: vec![relay_url],
            per_author_event_limit: 8,
            kinds: Some(vec![1]),
            relay_fetch_mode: RelayFetchMode::GlobalRecent,
            relay_page_size: 10,
            max_relay_pages: 2,
            ..CrawlConfig::default()
        },
    );

    let report = bridge
        .crawl(&graph, Some(&existing_root))
        .await
        .expect("crawl report");
    let root = report.root.expect("new root");
    let recent = event_store
        .list_recent(
            Some(&root),
            ListEventsOptions {
                limit: Some(10),
                ..Default::default()
            },
        )
        .await
        .expect("list recent");

    assert_eq!(report.events_selected, 1);
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].id, new_note.id.to_hex());

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn global_recent_scan_reuses_existing_root_events_before_fetching() -> io::Result<()> {
    let relay = TestRelay::new();
    let relay_url = relay.url();

    let root_keys = Keys::generate();
    let alice_keys = Keys::generate();

    let mut graph = SocialGraph::new(&root_keys.public_key().to_hex());
    let contact_list = EventBuilder::new(
        Kind::ContactList,
        "",
        [Tag::parse(&["p", &alice_keys.public_key().to_hex()]).expect("alice p tag")],
    )
    .custom_created_at(Timestamp::from_secs(10))
    .to_event(&root_keys)
    .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let old_note = EventBuilder::new(Kind::TextNote, "old", [])
        .custom_created_at(Timestamp::from_secs(20))
        .to_event(&alice_keys)
        .expect("old note");

    let store = Arc::new(MemoryStore::new());
    let event_store = NostrEventStore::new(store.clone());
    let existing_root = event_store
        .build(None, vec![stored_event_from_nostr(&old_note)])
        .await
        .expect("build root")
        .expect("existing root");

    let bridge = NostrBridge::new(
        store.clone(),
        CrawlConfig {
            relays: vec![relay_url],
            per_author_event_limit: 8,
            kinds: Some(vec![1]),
            relay_fetch_mode: RelayFetchMode::GlobalRecent,
            relay_page_size: 10,
            max_relay_pages: 2,
            ..CrawlConfig::default()
        },
    );

    let report = bridge
        .crawl(&graph, Some(&existing_root))
        .await
        .expect("crawl report");

    assert_eq!(report.events_seen, 0);
    assert_eq!(report.events_selected, 1);
    assert_eq!(report.root, Some(existing_root.clone()));

    let recent = event_store
        .list_recent(
            Some(&existing_root),
            ListEventsOptions {
                limit: Some(10),
                ..Default::default()
            },
        )
        .await
        .expect("list recent");
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].id, old_note.id.to_hex());

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn global_recent_scan_keeps_latest_metadata_available_for_feed_authors() -> io::Result<()> {
    let relay = TestRelay::new();
    let relay_url = relay.url();

    let root_keys = Keys::generate();
    let alice_keys = Keys::generate();
    let bob_keys = Keys::generate();

    let mut graph = SocialGraph::new(&root_keys.public_key().to_hex());
    let contact_list = EventBuilder::new(
        Kind::ContactList,
        "",
        [Tag::parse(&["p", &alice_keys.public_key().to_hex()]).expect("alice p tag")],
    )
    .custom_created_at(Timestamp::from_secs(10))
    .to_event(&root_keys)
    .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let alice_profile = EventBuilder::new(Kind::Metadata, r#"{"name":"Alice"}"#, [])
        .custom_created_at(Timestamp::from_secs(20))
        .to_event(&alice_keys)
        .expect("alice profile");
    let alice_note = EventBuilder::new(Kind::TextNote, "alice note", [])
        .custom_created_at(Timestamp::from_secs(199))
        .to_event(&alice_keys)
        .expect("alice note");
    let bob_note = EventBuilder::new(Kind::TextNote, "bob noise", [])
        .custom_created_at(Timestamp::from_secs(200))
        .to_event(&bob_keys)
        .expect("bob note");

    let publisher = Client::new(Keys::generate());
    publisher.add_relay(&relay_url).await.expect("add relay");
    publisher.connect().await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    for event in [&alice_profile, &alice_note, &bob_note] {
        publisher
            .send_event(event.clone())
            .await
            .expect("publish test event");
    }

    let store = Arc::new(MemoryStore::new());
    let bridge = NostrBridge::new(
        store.clone(),
        CrawlConfig {
            relays: vec![relay_url],
            relay_fetch_mode: RelayFetchMode::GlobalRecent,
            relay_page_size: 2,
            max_relay_pages: 1,
            per_author_event_limit: 1,
            kinds: Some(vec![0, 1]),
            ..CrawlConfig::default()
        },
    );

    let report = bridge.crawl(&graph, None).await.expect("crawl report");
    let root = report.root.expect("index root");
    let retained = NostrEventStore::new(store)
        .list_by_author(
            Some(&root),
            &alice_keys.public_key().to_hex(),
            ListEventsOptions {
                limit: Some(10),
                ..Default::default()
            },
        )
        .await
        .expect("list retained");

    assert_eq!(report.events_selected, 2);
    assert_eq!(retained.len(), 2);
    assert!(retained.iter().any(|event| event.kind == 0));
    assert!(retained
        .iter()
        .any(|event| event.id == alice_note.id.to_hex()));

    Ok(())
}
