use std::collections::HashMap;
use std::io;
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use hashtree_core::{MemoryStore, Store};
use hashtree_index::{BTree, BTreeOptions};
use hashtree_nostr::{
    CrawlConfig, CrawlReport, ListEventsOptions, NostrBridge, NostrEventStore, RelayFetchMode,
    StoredNostrEvent,
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

macro_rules! event_builder {
    ($kind:expr, $content:expr $(,)?) => {
        EventBuilder::new($kind, $content)
    };
    ($kind:expr, $content:expr, $tags:expr $(,)?) => {
        EventBuilder::new($kind, $content).tags($tags)
    };
}

fn p_tag(pubkey: nostr::PublicKey) -> Tag {
    Tag::parse(vec!["p".to_string(), pubkey.to_hex()]).expect("p tag")
}

fn t_tag(value: &str) -> Tag {
    Tag::parse(vec!["t".to_string(), value.to_string()]).expect("t tag")
}

#[derive(Debug, Default)]
struct SharedRelayState {
    events: Vec<Event>,
    requested_id_batches: Vec<Vec<String>>,
    filter_requests: usize,
    supports_negentropy: bool,
    negentropy_open_attempts: usize,
    negentropy_sessions_started: usize,
    server_page_cap: Option<usize>,
    disconnect_on_id_request: bool,
    id_response_cap: Option<usize>,
    non_id_response_cap: Option<usize>,
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

    fn with_partial_id_responses(id_response_cap: usize) -> Self {
        let relay = Self::with_options(true, None);
        relay
            .state
            .lock()
            .expect("relay state lock")
            .id_response_cap = Some(id_response_cap);
        relay
    }

    fn with_partial_id_and_paging_responses(
        id_response_cap: usize,
        non_id_response_cap: usize,
    ) -> Self {
        let relay = Self::with_partial_id_responses(id_response_cap);
        relay
            .state
            .lock()
            .expect("relay state lock")
            .non_id_response_cap = Some(non_id_response_cap);
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

    fn filter_requests(&self) -> usize {
        self.state.lock().expect("relay state lock").filter_requests
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
            filters.is_empty()
                || filters
                    .iter()
                    .any(|filter| filter.match_event(event, Default::default()))
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
                event.created_at.as_secs(),
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
    message: RelayMessage<'_>,
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
                let event = event.into_owned();
                state
                    .lock()
                    .expect("relay state lock")
                    .events
                    .push(event.clone());
                send_relay_message(&mut write, RelayMessage::ok(event.id, true, "")).await;
            }
            ClientMessage::Req {
                subscription_id,
                filters,
            } => {
                let subscription_id = subscription_id.into_owned();
                let filters = filters
                    .into_iter()
                    .map(|filter| filter.into_owned())
                    .collect::<Vec<_>>();
                if filters.iter().all(|filter| filter.ids.is_none()) {
                    state.lock().expect("relay state lock").filter_requests += 1;
                }
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
                let mut events = matching_events(&state, &filters);
                let response_cap = {
                    let guard = state.lock().expect("relay state lock");
                    if filters.iter().any(|filter| filter.ids.as_ref().is_some()) {
                        guard.id_response_cap
                    } else {
                        guard.non_id_response_cap
                    }
                };
                if let Some(cap) = response_cap {
                    events.truncate(cap);
                }
                for event in events {
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
                let subscription_id = subscription_id.into_owned();
                let filter = filter.into_owned();
                let initial_message = initial_message.into_owned();
                let supports_negentropy = {
                    let mut guard = state.lock().expect("relay state lock");
                    guard.negentropy_open_attempts += 1;
                    guard.supports_negentropy
                };
                if !supports_negentropy {
                    send_relay_message(
                        &mut write,
                        RelayMessage::notice("bad msg: unknown cmd negentropy"),
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
                        subscription_id: std::borrow::Cow::Owned(subscription_id),
                        message: hex::encode(response).into(),
                    },
                )
                .await;
            }
            ClientMessage::NegMsg {
                subscription_id,
                message,
            } => {
                let subscription_id = subscription_id.into_owned();
                let message = message.into_owned();
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
                        subscription_id: std::borrow::Cow::Owned(subscription_id),
                        message: hex::encode(response).into(),
                    },
                )
                .await;
            }
            ClientMessage::NegClose { subscription_id } | ClientMessage::Close(subscription_id) => {
                negentropy_sessions.remove(&subscription_id.to_string());
            }
            ClientMessage::Count {
                subscription_id,
                filter,
            } => {
                let filter = filter.into_owned();
                let count = matching_events(&state, std::slice::from_ref(&filter)).len();
                send_relay_message(
                    &mut write,
                    RelayMessage::count(subscription_id.into_owned(), count),
                )
                .await;
            }
            ClientMessage::Auth(_) => {}
        }
    }
}

fn graph_event_from_nostr(event: &Event) -> GraphEvent {
    GraphEvent {
        created_at: event.created_at.as_secs(),
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
        created_at: event.created_at.as_secs(),
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
    let contact_list = event_builder!(Kind::ContactList, "", [p_tag(alice_keys.public_key())],)
        .custom_created_at(Timestamp::from_secs(10))
        .sign_with_keys(&root_keys)
        .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let publisher = Client::new(Keys::generate());
    publisher.add_relay(&relay_url).await.expect("add relay");
    publisher.connect().await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    let alice_old = event_builder!(Kind::TextNote, "older nostr note", [t_tag("nostr")],)
        .custom_created_at(Timestamp::from_secs(20))
        .sign_with_keys(&alice_keys)
        .expect("alice old");
    let alice_new = event_builder!(Kind::TextNote, "newer nostr note", [t_tag("nostr")],)
        .custom_created_at(Timestamp::from_secs(30))
        .sign_with_keys(&alice_keys)
        .expect("alice new");
    let alice_low_priority = event_builder!(Kind::Custom(7), "reaction-ish", [t_tag("nostr")],)
        .custom_created_at(Timestamp::from_secs(40))
        .sign_with_keys(&alice_keys)
        .expect("alice low priority");
    let bob_note = event_builder!(Kind::TextNote, "bob note", [t_tag("nostr")],)
        .custom_created_at(Timestamp::from_secs(50))
        .sign_with_keys(&bob_keys)
        .expect("bob note");

    for event in [&alice_old, &alice_new, &alice_low_priority, &bob_note] {
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
    let contact_list = event_builder!(Kind::ContactList, "", [p_tag(alice_keys.public_key())],)
        .custom_created_at(Timestamp::from_secs(10))
        .sign_with_keys(&root_keys)
        .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let publisher = Client::new(Keys::generate());
    publisher.add_relay(&relay_url).await.expect("add relay");
    publisher.connect().await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    let note_one = event_builder!(Kind::TextNote, "note one", [t_tag("nostr")],)
        .custom_created_at(Timestamp::from_secs(20))
        .sign_with_keys(&alice_keys)
        .expect("note one");
    let note_two = event_builder!(Kind::TextNote, "note two", [t_tag("nostr")],)
        .custom_created_at(Timestamp::from_secs(30))
        .sign_with_keys(&alice_keys)
        .expect("note two");
    let note_three = event_builder!(Kind::TextNote, "note three", [t_tag("nostr")],)
        .custom_created_at(Timestamp::from_secs(40))
        .sign_with_keys(&alice_keys)
        .expect("note three");

    for event in [&note_one, &note_two, &note_three] {
        publisher
            .send_event(event)
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
    let contact_list = event_builder!(Kind::ContactList, "", [p_tag(alice_keys.public_key())],)
        .custom_created_at(Timestamp::from_secs(10))
        .sign_with_keys(&root_keys)
        .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let publisher = Client::new(Keys::generate());
    publisher.add_relay(&relay_url).await.expect("add relay");
    publisher.connect().await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    let note_one = event_builder!(Kind::TextNote, "note one", [t_tag("nostr")],)
        .custom_created_at(Timestamp::from_secs(20))
        .sign_with_keys(&alice_keys)
        .expect("note one");
    let note_two = event_builder!(Kind::TextNote, "note two", [t_tag("nostr")],)
        .custom_created_at(Timestamp::from_secs(30))
        .sign_with_keys(&alice_keys)
        .expect("note two");
    let note_three = event_builder!(Kind::TextNote, "note three", [t_tag("nostr")],)
        .custom_created_at(Timestamp::from_secs(40))
        .sign_with_keys(&alice_keys)
        .expect("note three");

    for event in [&note_one, &note_two, &note_three] {
        publisher
            .send_event(event)
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
    let contact_list = event_builder!(Kind::ContactList, "", [p_tag(alice_keys.public_key())],)
        .custom_created_at(Timestamp::from_secs(10))
        .sign_with_keys(&root_keys)
        .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let publisher = Client::new(Keys::generate());
    publisher.add_relay(&relay_url).await.expect("add relay");
    publisher.connect().await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    for created_at in 20..25 {
        let note = event_builder!(
            Kind::TextNote,
            format!("note {created_at}"),
            [t_tag("nostr")],
        )
        .custom_created_at(Timestamp::from_secs(created_at))
        .sign_with_keys(&alice_keys)
        .expect("note");
        publisher
            .send_event(&note)
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
async fn full_author_history_retains_per_author_limit() -> io::Result<()> {
    let relay = TestRelay::new();
    let relay_url = relay.url();

    let root_keys = Keys::generate();
    let alice_keys = Keys::generate();

    let mut graph = SocialGraph::new(&root_keys.public_key().to_hex());
    let contact_list = event_builder!(Kind::ContactList, "", [p_tag(alice_keys.public_key())],)
        .custom_created_at(Timestamp::from_secs(10))
        .sign_with_keys(&root_keys)
        .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let publisher = Client::new(Keys::generate());
    publisher.add_relay(&relay_url).await.expect("add relay");
    publisher.connect().await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    let mut expected_ids = Vec::new();
    for created_at in 20..25 {
        let note = event_builder!(
            Kind::TextNote,
            format!("note {created_at}"),
            [t_tag("nostr")],
        )
        .custom_created_at(Timestamp::from_secs(created_at))
        .sign_with_keys(&alice_keys)
        .expect("note");
        expected_ids.push(note.id.to_hex());
        publisher
            .send_event(&note)
            .await
            .expect("publish test event");
    }

    let store = Arc::new(MemoryStore::new());
    let bridge = NostrBridge::new(
        store.clone(),
        CrawlConfig {
            relays: vec![relay_url],
            author_batch_size: 1,
            per_author_event_limit: 2,
            full_author_history: true,
            relay_page_size: 2,
            max_relay_pages: 10,
            kinds: Some(vec![1]),
            ..CrawlConfig::default()
        },
    );

    let report = bridge.crawl(&graph, None).await.expect("crawl report");
    assert_eq!(report.events_selected, 2);
    assert_eq!(report.events_seen, 2);

    let root = report.root.expect("index root");
    let event_store = NostrEventStore::new(store);
    let nostr_events = event_store
        .list_by_author_and_kind(
            Some(&root),
            &alice_keys.public_key().to_hex(),
            1,
            ListEventsOptions::default(),
        )
        .await
        .expect("list author notes");
    let indexed_ids = nostr_events
        .into_iter()
        .map(|event| event.id)
        .collect::<Vec<_>>();

    expected_ids = expected_ids.into_iter().rev().take(2).collect();
    assert_eq!(indexed_ids, expected_ids);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_author_history_retains_more_than_256_events_across_pages() -> io::Result<()> {
    let relay = TestRelay::new();
    let relay_url = relay.url();

    let root_keys = Keys::generate();
    let alice_keys = Keys::generate();

    let mut graph = SocialGraph::new(&root_keys.public_key().to_hex());
    let contact_list = event_builder!(Kind::ContactList, "", [p_tag(alice_keys.public_key())],)
        .custom_created_at(Timestamp::from_secs(10))
        .sign_with_keys(&root_keys)
        .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let events = (20..277)
        .map(|created_at| {
            event_builder!(Kind::TextNote, format!("note {created_at}"))
                .custom_created_at(Timestamp::from_secs(created_at))
                .sign_with_keys(&alice_keys)
                .expect("note")
        })
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 257);
    relay
        .state
        .lock()
        .expect("relay state lock")
        .events
        .extend(events);

    let bridge = NostrBridge::new(
        Arc::new(MemoryStore::new()),
        CrawlConfig {
            relays: vec![relay_url],
            author_batch_size: 1,
            per_author_event_limit: 257,
            per_author_kind_event_limit: Some(257),
            full_author_history: true,
            relay_page_size: 128,
            max_relay_pages: 3,
            kinds: Some(vec![1]),
            ..CrawlConfig::default()
        },
    );

    let report = bridge.crawl(&graph, None).await.expect("crawl report");
    // Inclusive timestamp pagination re-fetches the boundary event on each
    // later page so same-second peers cannot be skipped.
    assert_eq!(report.events_seen, 259);
    assert_eq!(report.events_selected, 257);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_author_history_keeps_same_second_page_boundary_events() -> io::Result<()> {
    let relay = TestRelay::new();
    let relay_url = relay.url();

    let root_keys = Keys::generate();
    let alice_keys = Keys::generate();

    let mut graph = SocialGraph::new(&root_keys.public_key().to_hex());
    let contact_list = event_builder!(Kind::ContactList, "", [p_tag(alice_keys.public_key())],)
        .custom_created_at(Timestamp::from_secs(10))
        .sign_with_keys(&root_keys)
        .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let events = [100, 100, 100, 100, 90, 89]
        .into_iter()
        .enumerate()
        .map(|(index, created_at)| {
            event_builder!(Kind::TextNote, format!("note {index}"))
                .custom_created_at(Timestamp::from_secs(created_at))
                .sign_with_keys(&alice_keys)
                .expect("note")
        })
        .collect::<Vec<_>>();
    relay
        .state
        .lock()
        .expect("relay state lock")
        .events
        .extend(events);

    let bridge = NostrBridge::new(
        Arc::new(MemoryStore::new()),
        CrawlConfig {
            relays: vec![relay_url],
            author_batch_size: 1,
            per_author_event_limit: 6,
            per_author_kind_event_limit: Some(6),
            full_author_history: true,
            relay_page_size: 3,
            max_relay_pages: 2,
            kinds: Some(vec![1]),
            ..CrawlConfig::default()
        },
    );

    let report = bridge.crawl(&graph, None).await.expect("crawl report");
    assert_eq!(report.events_selected, 6);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_author_history_runs_separate_per_kind_passes() -> io::Result<()> {
    let relay = TestRelay::new();
    let relay_url = relay.url();

    let root_keys = Keys::generate();
    let alice_keys = Keys::generate();

    let mut graph = SocialGraph::new(&root_keys.public_key().to_hex());
    let contact_list = event_builder!(Kind::ContactList, "", [p_tag(alice_keys.public_key())],)
        .custom_created_at(Timestamp::from_secs(10))
        .sign_with_keys(&root_keys)
        .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let events = [
        (Kind::TextNote, "note 30", 30),
        (Kind::TextNote, "note 20", 20),
        (Kind::TextNote, "note 10", 10),
        (Kind::EventDeletion, "deletion", 5),
    ]
    .into_iter()
    .map(|(kind, content, created_at)| {
        event_builder!(kind, content)
            .custom_created_at(Timestamp::from_secs(created_at))
            .sign_with_keys(&alice_keys)
            .expect("event")
    })
    .collect::<Vec<_>>();
    relay
        .state
        .lock()
        .expect("relay state lock")
        .events
        .extend(events);

    let bridge = NostrBridge::new(
        Arc::new(MemoryStore::new()),
        CrawlConfig {
            relays: vec![relay_url],
            author_batch_size: 1,
            per_author_event_limit: 2,
            per_author_kind_event_limit: Some(2),
            full_author_history: true,
            relay_page_size: 2,
            max_relay_pages: 1,
            kinds: Some(vec![1, 5]),
            ..CrawlConfig::default()
        },
    );

    let report = bridge.crawl(&graph, None).await.expect("crawl report");
    assert_eq!(report.events_seen, 3);
    assert_eq!(report.events_selected, 3);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn per_kind_quota_isolated_by_author_at_the_relay() -> io::Result<()> {
    let relay = TestRelay::new();
    let relay_url = relay.url();

    let root_keys = Keys::generate();
    let alice_keys = Keys::generate();
    let bob_keys = Keys::generate();

    let mut graph = SocialGraph::new(&root_keys.public_key().to_hex());
    let contact_list = event_builder!(
        Kind::ContactList,
        "",
        [p_tag(alice_keys.public_key()), p_tag(bob_keys.public_key()),],
    )
    .custom_created_at(Timestamp::from_secs(10))
    .sign_with_keys(&root_keys)
    .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let mut events = Vec::new();
    for (keys, start) in [(&alice_keys, 100), (&bob_keys, 90)] {
        for created_at in start..start + 6 {
            events.push(
                event_builder!(Kind::TextNote, format!("busy {created_at}"))
                    .custom_created_at(Timestamp::from_secs(created_at))
                    .sign_with_keys(keys)
                    .expect("text note"),
            );
        }
        events.push(
            event_builder!(Kind::EventDeletion, "quiet older event")
                .custom_created_at(Timestamp::from_secs(1))
                .sign_with_keys(keys)
                .expect("deletion"),
        );
    }
    relay
        .state
        .lock()
        .expect("relay state lock")
        .events
        .extend(events);

    let bridge = NostrBridge::new(
        Arc::new(MemoryStore::new()),
        CrawlConfig {
            relays: vec![relay_url],
            author_batch_size: 3,
            per_author_event_limit: 2,
            per_author_kind_event_limit: Some(2),
            kinds: Some(vec![1, 5]),
            ..CrawlConfig::default()
        },
    );

    let report = bridge.crawl(&graph, None).await.expect("crawl report");
    assert_eq!(report.events_selected, 6);
    for pubkey in [
        alice_keys.public_key().to_hex(),
        bob_keys.public_key().to_hex(),
    ] {
        assert!(report
            .applied_events
            .iter()
            .any(|event| event.pubkey == pubkey && event.kind == 5));
    }
    assert!(relay.filter_requests() > 2);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sparse_per_kind_sync_uses_one_relay_filter_per_kind() -> io::Result<()> {
    let relay = TestRelay::new();
    let relay_url = relay.url();

    let root_keys = Keys::generate();
    let alice_keys = Keys::generate();
    let bob_keys = Keys::generate();
    let mut graph = SocialGraph::new(&root_keys.public_key().to_hex());
    let contact_list = event_builder!(
        Kind::ContactList,
        "",
        [p_tag(alice_keys.public_key()), p_tag(bob_keys.public_key()),],
    )
    .custom_created_at(Timestamp::from_secs(10))
    .sign_with_keys(&root_keys)
    .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let bridge = NostrBridge::new(
        Arc::new(MemoryStore::new()),
        CrawlConfig {
            relays: vec![relay_url],
            author_batch_size: 3,
            per_author_event_limit: 2,
            per_author_kind_event_limit: Some(2),
            kinds: Some(vec![1, 5]),
            ..CrawlConfig::default()
        },
    );

    let report = bridge.crawl(&graph, None).await.expect("crawl report");
    assert_eq!(report.events_selected, 0);
    assert_eq!(relay.filter_requests(), 2);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saturated_negentropy_splits_fully_local_cap_to_find_quiet_author() -> io::Result<()> {
    let relay = TestRelay::with_negentropy(true);
    let relay_url = relay.url();

    let root_keys = Keys::generate();
    let alice_keys = Keys::generate();
    let bob_keys = Keys::generate();
    let mut graph = SocialGraph::new(&root_keys.public_key().to_hex());
    let contact_list = event_builder!(
        Kind::ContactList,
        "",
        [p_tag(alice_keys.public_key()), p_tag(bob_keys.public_key()),],
    )
    .custom_created_at(Timestamp::from_secs(10))
    .sign_with_keys(&root_keys)
    .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let mut hot_events = Vec::new();
    for created_at in 100..106 {
        hot_events.push(
            event_builder!(Kind::TextNote, format!("hot {created_at}"))
                .custom_created_at(Timestamp::from_secs(created_at))
                .sign_with_keys(&alice_keys)
                .expect("hot text note"),
        );
    }
    let quiet_event = event_builder!(Kind::TextNote, "quiet older event")
        .custom_created_at(Timestamp::from_secs(1))
        .sign_with_keys(&bob_keys)
        .expect("quiet text note");
    let mut relay_events = hot_events.clone();
    relay_events.push(quiet_event.clone());
    relay
        .state
        .lock()
        .expect("relay state lock")
        .events
        .extend(relay_events);

    let store = Arc::new(MemoryStore::new());
    let existing_root = NostrEventStore::new(store.clone())
        .build(None, hot_events.iter().map(stored_event_from_nostr))
        .await
        .expect("existing root");
    let bridge = NostrBridge::new(
        store,
        CrawlConfig {
            relays: vec![relay_url],
            author_batch_size: 3,
            per_author_event_limit: 2,
            per_author_kind_event_limit: Some(2),
            kinds: Some(vec![1]),
            ..CrawlConfig::default()
        },
    );

    let report = bridge
        .crawl(&graph, existing_root.as_ref())
        .await
        .expect("crawl report");
    assert_eq!(report.events_selected, 3);
    assert!(report.applied_events.iter().any(|event| {
        event.id == quiet_event.id.to_hex() && event.pubkey == bob_keys.public_key().to_hex()
    }));
    assert!(relay.negentropy_open_attempts() > 1);
    assert!(relay.negentropy_sessions_started() > 1);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeat_crawl_does_not_split_rich_local_state_against_sparse_relay() -> io::Result<()> {
    let relay = TestRelay::with_negentropy(true);
    let root_keys = Keys::generate();
    let alice_keys = Keys::generate();
    let bob_keys = Keys::generate();
    let mut graph = SocialGraph::new(&root_keys.public_key().to_hex());
    let contact_list = event_builder!(
        Kind::ContactList,
        "",
        [p_tag(alice_keys.public_key()), p_tag(bob_keys.public_key()),],
    )
    .custom_created_at(Timestamp::from_secs(10))
    .sign_with_keys(&root_keys)
    .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let mut events = Vec::new();
    for (author_index, keys) in [&root_keys, &alice_keys, &bob_keys].into_iter().enumerate() {
        for offset in 0..2 {
            events.push(
                event_builder!(Kind::TextNote, format!("{author_index}-{offset}"))
                    .custom_created_at(Timestamp::from_secs(
                        100 + (author_index * 2 + offset) as u64,
                    ))
                    .sign_with_keys(keys)
                    .expect("text note"),
            );
        }
    }
    let sparse_event_id = events[0].id;
    relay
        .state
        .lock()
        .expect("relay state lock")
        .events
        .extend(events);

    let store = Arc::new(MemoryStore::new());
    let bridge = NostrBridge::new(
        store,
        CrawlConfig {
            relays: vec![relay.url()],
            author_batch_size: 3,
            per_author_event_limit: 2,
            per_author_kind_event_limit: Some(2),
            kinds: Some(vec![1]),
            require_negentropy: true,
            ..CrawlConfig::default()
        },
    );
    let first = bridge.crawl(&graph, None).await.expect("first crawl");
    let first_root = first.root.expect("first root");
    assert_eq!(first.events_selected, 6);

    relay
        .state
        .lock()
        .expect("relay state lock")
        .events
        .retain(|event| event.id == sparse_event_id);
    let sessions_before = relay.negentropy_sessions_started();
    let second = bridge
        .crawl(&graph, Some(&first_root))
        .await
        .expect("repeat crawl");

    assert_eq!(second.events_selected, 6);
    assert!(second.applied_events.is_empty());
    assert_eq!(relay.negentropy_sessions_started() - sessions_before, 1);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_author_history_can_skip_paging_fallback() -> io::Result<()> {
    let relay = TestRelay::new();
    let relay_url = relay.url();

    let root_keys = Keys::generate();
    let alice_keys = Keys::generate();

    let mut graph = SocialGraph::new(&root_keys.public_key().to_hex());
    let contact_list = event_builder!(Kind::ContactList, "", [p_tag(alice_keys.public_key())],)
        .custom_created_at(Timestamp::from_secs(10))
        .sign_with_keys(&root_keys)
        .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let publisher = Client::new(Keys::generate());
    publisher.add_relay(&relay_url).await.expect("add relay");
    publisher.connect().await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    let note = event_builder!(Kind::TextNote, "alice note")
        .custom_created_at(Timestamp::from_secs(20))
        .sign_with_keys(&alice_keys)
        .expect("note");
    publisher
        .send_event(&note)
        .await
        .expect("publish test event");

    let bridge = NostrBridge::new(
        Arc::new(MemoryStore::new()),
        CrawlConfig {
            relays: vec![relay_url],
            author_batch_size: 1,
            full_author_history: true,
            relay_page_size: 2,
            max_relay_pages: 0,
            kinds: Some(vec![1]),
            ..CrawlConfig::default()
        },
    );

    let report = bridge.crawl(&graph, None).await.expect("crawl report");
    assert_eq!(report.events_selected, 0);
    assert_eq!(report.events_seen, 0);
    assert_eq!(relay.negentropy_open_attempts(), 1);
    assert!(relay.requested_id_batches().is_empty());

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_author_history_uses_negentropy_with_local_items() -> io::Result<()> {
    let relay = TestRelay::with_negentropy(true);
    let relay_url = relay.url();

    let root_keys = Keys::generate();
    let alice_keys = Keys::generate();

    let mut graph = SocialGraph::new(&root_keys.public_key().to_hex());
    let contact_list = event_builder!(Kind::ContactList, "", [p_tag(alice_keys.public_key())],)
        .custom_created_at(Timestamp::from_secs(10))
        .sign_with_keys(&root_keys)
        .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let publisher = Client::new(Keys::generate());
    publisher.add_relay(&relay_url).await.expect("add relay");
    publisher.connect().await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    let mut notes = Vec::new();
    for created_at in 20..25 {
        let note = event_builder!(Kind::TextNote, format!("note {created_at}"))
            .custom_created_at(Timestamp::from_secs(created_at))
            .sign_with_keys(&alice_keys)
            .expect("note");
        publisher
            .send_event(&note)
            .await
            .expect("publish test event");
        notes.push(note);
    }

    let store = Arc::new(MemoryStore::new());
    let event_store = NostrEventStore::new(store.clone());
    let existing_root = event_store
        .build(None, notes.iter().take(3).map(stored_event_from_nostr))
        .await
        .expect("build existing root")
        .expect("existing root");
    let bridge = NostrBridge::new(
        store.clone(),
        CrawlConfig {
            relays: vec![relay_url],
            full_author_history: true,
            relay_page_size: 2,
            max_relay_pages: 10,
            per_author_event_limit: 2,
            per_author_kind_event_limit: Some(2),
            kinds: Some(vec![1]),
            ..CrawlConfig::default()
        },
    );

    let report = bridge
        .crawl(&graph, Some(&existing_root))
        .await
        .expect("crawl report");
    assert_eq!(report.events_selected, 2);
    assert!(relay.negentropy_sessions_started() >= 1);

    let requested_ids = relay
        .requested_id_batches()
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    let mut expected_missing_ids = notes
        .iter()
        .skip(3)
        .map(|event| event.id.to_hex())
        .collect::<Vec<_>>();
    expected_missing_ids.sort();
    let mut actual_requested_ids = requested_ids;
    actual_requested_ids.sort();
    assert_eq!(actual_requested_ids, expected_missing_ids);
    assert_eq!(report.events_seen, 2);

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
    let contact_list = event_builder!(
        Kind::ContactList,
        "",
        [p_tag(alice_keys.public_key()), p_tag(bob_keys.public_key()),],
    )
    .custom_created_at(Timestamp::from_secs(10))
    .sign_with_keys(&root_keys)
    .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let publisher = Client::new(Keys::generate());
    publisher.add_relay(&relay_url).await.expect("add relay");
    publisher.connect().await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    for (created_at, keys) in [(20, &alice_keys), (21, &bob_keys)] {
        let note = event_builder!(Kind::TextNote, format!("note {created_at}"))
            .custom_created_at(Timestamp::from_secs(created_at))
            .sign_with_keys(keys)
            .expect("note");
        publisher
            .send_event(&note)
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
async fn require_negentropy_errors_when_no_relay_supports_it() -> io::Result<()> {
    let relay = TestRelay::new();
    let root_keys = Keys::generate();
    let graph = SocialGraph::new(&root_keys.public_key().to_hex());
    let bridge = NostrBridge::new(
        Arc::new(MemoryStore::new()),
        CrawlConfig {
            relays: vec![relay.url()],
            author_batch_size: 1,
            per_author_kind_event_limit: Some(2),
            kinds: Some(vec![1]),
            require_negentropy: true,
            ..CrawlConfig::default()
        },
    );

    let error = bridge
        .crawl(&graph, None)
        .await
        .expect_err("unsupported-only batch must be retryable");
    assert!(error
        .to_string()
        .contains("does not support required negentropy"));
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
    let contact_list = event_builder!(Kind::ContactList, "", [p_tag(alice_keys.public_key())],)
        .custom_created_at(Timestamp::from_secs(10))
        .sign_with_keys(&root_keys)
        .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let note = event_builder!(Kind::TextNote, "alice note")
        .custom_created_at(Timestamp::from_secs(20))
        .sign_with_keys(&alice_keys)
        .expect("note");

    let publisher = Client::new(Keys::generate());
    publisher
        .add_relay(&supported_relay.url())
        .await
        .expect("add supported relay");
    publisher.connect().await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    publisher.send_event(&note).await.expect("publish event");

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
    let contact_list = event_builder!(Kind::ContactList, "", [p_tag(alice_keys.public_key())],)
        .custom_created_at(Timestamp::from_secs(10))
        .sign_with_keys(&root_keys)
        .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let note = event_builder!(Kind::TextNote, "alice note")
        .custom_created_at(Timestamp::from_secs(20))
        .sign_with_keys(&alice_keys)
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
        publisher.send_event(&note).await.expect("publish note");
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
async fn partial_missing_id_response_keeps_batch_retryable() -> io::Result<()> {
    let relay = TestRelay::with_partial_id_responses(1);
    let root_keys = Keys::generate();
    let alice_keys = Keys::generate();
    let mut graph = SocialGraph::new(&root_keys.public_key().to_hex());
    let contact_list = event_builder!(Kind::ContactList, "", [p_tag(alice_keys.public_key())],)
        .custom_created_at(Timestamp::from_secs(10))
        .sign_with_keys(&root_keys)
        .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let notes = [20, 21]
        .into_iter()
        .map(|created_at| {
            event_builder!(Kind::TextNote, format!("note {created_at}"))
                .custom_created_at(Timestamp::from_secs(created_at))
                .sign_with_keys(&alice_keys)
                .expect("text note")
        })
        .collect::<Vec<_>>();
    relay
        .state
        .lock()
        .expect("relay state lock")
        .events
        .extend(notes);

    let bridge = NostrBridge::new(
        Arc::new(MemoryStore::new()),
        CrawlConfig {
            relays: vec![relay.url()],
            author_batch_size: 2,
            per_author_event_limit: 4,
            per_author_kind_event_limit: Some(4),
            kinds: Some(vec![1]),
            require_negentropy: true,
            ..CrawlConfig::default()
        },
    );

    let error = bridge
        .crawl(&graph, None)
        .await
        .expect_err("partial ID response must not complete the batch");
    assert!(error.to_string().contains("omitted 1 of 2"));
    assert_eq!(relay.requested_id_batches().len(), 3);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_history_partial_id_fallback_preserves_negentropy_support() -> io::Result<()> {
    let relay = TestRelay::with_partial_id_responses(1);
    let root_keys = Keys::generate();
    let alice_keys = Keys::generate();
    let mut graph = SocialGraph::new(&root_keys.public_key().to_hex());
    let contact_list = event_builder!(Kind::ContactList, "", [p_tag(alice_keys.public_key())],)
        .custom_created_at(Timestamp::from_secs(10))
        .sign_with_keys(&root_keys)
        .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    for (kind, prefix) in [(Kind::TextNote, "note"), (Kind::EventDeletion, "delete")] {
        for created_at in [20, 21] {
            relay.state.lock().expect("relay state lock").events.push(
                event_builder!(kind, format!("{prefix} {created_at}"))
                    .custom_created_at(Timestamp::from_secs(created_at))
                    .sign_with_keys(&alice_keys)
                    .expect("archive event"),
            );
        }
    }

    let bridge = NostrBridge::new(
        Arc::new(MemoryStore::new()),
        CrawlConfig {
            relays: vec![relay.url()],
            author_batch_size: 2,
            per_author_event_limit: 2,
            per_author_kind_event_limit: Some(2),
            kinds: Some(vec![1, 5]),
            full_author_history: true,
            relay_page_size: 10,
            max_relay_pages: 1,
            ..CrawlConfig::default()
        },
    );

    let report = bridge
        .crawl(&graph, None)
        .await
        .expect("full-history crawl");
    assert_eq!(report.events_selected, 4);
    assert_eq!(
        relay.requested_id_batches().len(),
        2,
        "both kind passes should retain proven negentropy support after paging fallback"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_history_rejects_incomplete_paging_fallback() -> io::Result<()> {
    let relay = TestRelay::with_partial_id_and_paging_responses(1, 1);
    let root_keys = Keys::generate();
    let alice_keys = Keys::generate();
    let mut graph = SocialGraph::new(&root_keys.public_key().to_hex());
    let contact_list = event_builder!(Kind::ContactList, "", [p_tag(alice_keys.public_key())],)
        .custom_created_at(Timestamp::from_secs(10))
        .sign_with_keys(&root_keys)
        .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);
    for created_at in [20, 21] {
        relay.state.lock().expect("relay state lock").events.push(
            event_builder!(Kind::TextNote, format!("note {created_at}"))
                .custom_created_at(Timestamp::from_secs(created_at))
                .sign_with_keys(&alice_keys)
                .expect("text note"),
        );
    }

    let bridge = NostrBridge::new(
        Arc::new(MemoryStore::new()),
        CrawlConfig {
            relays: vec![relay.url()],
            author_batch_size: 2,
            per_author_event_limit: 2,
            per_author_kind_event_limit: Some(2),
            kinds: Some(vec![1]),
            full_author_history: true,
            relay_page_size: 10,
            max_relay_pages: 1,
            ..CrawlConfig::default()
        },
    );

    let error = bridge
        .crawl(&graph, None)
        .await
        .expect_err("paging fallback must recover every reconciled ID");
    assert!(error
        .to_string()
        .contains("paging fallback omitted 1 of 2 reconciled event IDs"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_event_max_size_allows_moderately_large_events() -> io::Result<()> {
    let relay = TestRelay::new();
    let relay_url = relay.url();

    let root_keys = Keys::generate();
    let alice_keys = Keys::generate();

    let mut graph = SocialGraph::new(&root_keys.public_key().to_hex());
    let contact_list = event_builder!(Kind::ContactList, "", [p_tag(alice_keys.public_key())],)
        .custom_created_at(Timestamp::from_secs(10))
        .sign_with_keys(&root_keys)
        .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let publisher = Client::new(Keys::generate());
    publisher.add_relay(&relay_url).await.expect("add relay");
    publisher.connect().await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    let large_note = event_builder!(Kind::TextNote, "x".repeat(90_000))
        .custom_created_at(Timestamp::from_secs(20))
        .sign_with_keys(&alice_keys)
        .expect("large note");
    publisher
        .send_event(&large_note)
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
    let contact_list = event_builder!(Kind::ContactList, "", [p_tag(alice_keys.public_key())],)
        .custom_created_at(Timestamp::from_secs(10))
        .sign_with_keys(&root_keys)
        .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let publisher = Client::new(Keys::generate());
    publisher.add_relay(&relay_url).await.expect("add relay");
    publisher.connect().await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    let mut events = Vec::new();
    for created_at in 20..22 {
        events.push(
            event_builder!(Kind::TextNote, format!("alice {created_at}"))
                .custom_created_at(Timestamp::from_secs(created_at))
                .sign_with_keys(&alice_keys)
                .expect("alice note"),
        );
    }
    for created_at in 30..33 {
        events.push(
            event_builder!(Kind::TextNote, format!("bob {created_at}"))
                .custom_created_at(Timestamp::from_secs(created_at))
                .sign_with_keys(&bob_keys)
                .expect("bob note"),
        );
    }
    for event in events {
        publisher
            .send_event(&event)
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
    let contact_list = event_builder!(Kind::ContactList, "", [p_tag(alice_keys.public_key())],)
        .custom_created_at(Timestamp::from_secs(10))
        .sign_with_keys(&root_keys)
        .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let publisher = Client::new(Keys::generate());
    publisher.add_relay(&relay_url).await.expect("add relay");
    publisher.connect().await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    for created_at in 20..22 {
        let note = event_builder!(Kind::TextNote, format!("alice {created_at}"))
            .custom_created_at(Timestamp::from_secs(created_at))
            .sign_with_keys(&alice_keys)
            .expect("alice note");
        publisher
            .send_event(&note)
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
    assert_progress_matches_report_without_applied_events(progress.last(), &report);

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
        let note = event_builder!(Kind::TextNote, label)
            .custom_created_at(Timestamp::from_secs(created_at))
            .sign_with_keys(keys)
            .expect("note");
        publisher
            .send_event(&note)
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
    let contact_list = event_builder!(Kind::ContactList, "", [p_tag(alice_keys.public_key())],)
        .custom_created_at(Timestamp::from_secs(10))
        .sign_with_keys(&root_keys)
        .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let publisher = Client::new(Keys::generate());
    publisher.add_relay(&relay_url).await.expect("add relay");
    publisher.connect().await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    for created_at in 20..25 {
        let note = event_builder!(Kind::TextNote, format!("note {created_at}"))
            .custom_created_at(Timestamp::from_secs(created_at))
            .sign_with_keys(&alice_keys)
            .expect("note");
        publisher
            .send_event(&note)
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
    let contact_list = event_builder!(Kind::ContactList, "", [p_tag(alice_keys.public_key())],)
        .custom_created_at(Timestamp::from_secs(10))
        .sign_with_keys(&root_keys)
        .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let publisher = Client::new(Keys::generate());
    publisher.add_relay(&relay_url).await.expect("add relay");
    publisher.connect().await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    for created_at in 20..25 {
        let note = event_builder!(Kind::TextNote, format!("note {created_at}"))
            .custom_created_at(Timestamp::from_secs(created_at))
            .sign_with_keys(&alice_keys)
            .expect("note");
        publisher
            .send_event(&note)
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
    let contact_list = event_builder!(Kind::ContactList, "", [p_tag(alice_keys.public_key())],)
        .custom_created_at(Timestamp::from_secs(10))
        .sign_with_keys(&root_keys)
        .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let publisher = Client::new(Keys::generate());
    publisher.add_relay(&relay_url).await.expect("add relay");
    publisher.connect().await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    for created_at in 20..25 {
        let note = event_builder!(Kind::TextNote, format!("note {created_at}"))
            .custom_created_at(Timestamp::from_secs(created_at))
            .sign_with_keys(&alice_keys)
            .expect("note");
        publisher
            .send_event(&note)
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
    // The inclusive boundary event is fetched twice but consumes the event
    // budget only once, so the second page contributes two new IDs.
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
    let contact_list = event_builder!(Kind::ContactList, "", [p_tag(alice_keys.public_key())],)
        .custom_created_at(Timestamp::from_secs(10))
        .sign_with_keys(&root_keys)
        .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let note_one = event_builder!(Kind::TextNote, "note one")
        .custom_created_at(Timestamp::from_secs(20))
        .sign_with_keys(&alice_keys)
        .expect("note one");
    let note_two = event_builder!(Kind::TextNote, "note two")
        .custom_created_at(Timestamp::from_secs(30))
        .sign_with_keys(&alice_keys)
        .expect("note two");
    let note_three = event_builder!(Kind::TextNote, "note three")
        .custom_created_at(Timestamp::from_secs(40))
        .sign_with_keys(&alice_keys)
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
            .send_event(event)
            .await
            .expect("publish relay one event");
    }

    for event in [&note_one, &note_two, &note_three] {
        publisher_two
            .send_event(event)
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
    let root_contact_list = event_builder!(
        Kind::ContactList,
        "",
        [
            p_tag(alice_keys.public_key()),
            p_tag(bob_keys.public_key()),
            p_tag(carol_keys.public_key()),
        ],
    )
    .custom_created_at(Timestamp::from_secs(10))
    .sign_with_keys(&root_keys)
    .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&root_contact_list), true, 1.0);

    let publisher = Client::new(Keys::generate());
    publisher.add_relay(&relay_url).await.expect("add relay");
    publisher.connect().await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    let alice_note = event_builder!(Kind::TextNote, "alice")
        .custom_created_at(Timestamp::from_secs(20))
        .sign_with_keys(&alice_keys)
        .expect("alice note");
    let bob_note = event_builder!(Kind::TextNote, "bob")
        .custom_created_at(Timestamp::from_secs(21))
        .sign_with_keys(&bob_keys)
        .expect("bob note");
    let carol_note = event_builder!(Kind::TextNote, "carol")
        .custom_created_at(Timestamp::from_secs(22))
        .sign_with_keys(&carol_keys)
        .expect("carol note");

    for event in [&alice_note, &bob_note, &carol_note] {
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
    let root_contact_list = event_builder!(
        Kind::ContactList,
        "",
        [p_tag(alice_keys.public_key()), p_tag(bob_keys.public_key()),],
    )
    .custom_created_at(Timestamp::from_secs(10))
    .sign_with_keys(&root_keys)
    .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&root_contact_list), true, 1.0);

    let publisher = Client::new(Keys::generate());
    publisher.add_relay(&relay_url).await.expect("add relay");
    publisher.connect().await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    for (keys, content, created_at) in [(&alice_keys, "alice", 20u64), (&bob_keys, "bob", 21u64)] {
        let note = event_builder!(Kind::TextNote, content)
            .custom_created_at(Timestamp::from_secs(created_at))
            .sign_with_keys(keys)
            .expect("note");
        publisher
            .send_event(&note)
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
    assert_progress_matches_report_without_applied_events(progress.last(), &report);

    Ok(())
}

fn assert_progress_matches_report_without_applied_events(
    progress: Option<&CrawlReport>,
    report: &CrawlReport,
) {
    let mut actual = progress.cloned();
    if let Some(actual) = actual.as_mut() {
        actual.applied_events.clear();
    }
    let mut expected = report.clone();
    expected.applied_events.clear();
    assert_eq!(actual, Some(expected));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ignores_missing_local_event_blobs_from_existing_root_in_global_scan() -> io::Result<()> {
    let relay = TestRelay::new();
    let relay_url = relay.url();

    let root_keys = Keys::generate();
    let alice_keys = Keys::generate();

    let mut graph = SocialGraph::new(&root_keys.public_key().to_hex());
    let contact_list = event_builder!(Kind::ContactList, "", [p_tag(alice_keys.public_key())],)
        .custom_created_at(Timestamp::from_secs(10))
        .sign_with_keys(&root_keys)
        .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let old_note = event_builder!(Kind::TextNote, "old")
        .custom_created_at(Timestamp::from_secs(20))
        .sign_with_keys(&alice_keys)
        .expect("old note");
    let new_note = event_builder!(Kind::TextNote, "new")
        .custom_created_at(Timestamp::from_secs(21))
        .sign_with_keys(&alice_keys)
        .expect("new note");

    let publisher = Client::new(Keys::generate());
    publisher.add_relay(&relay_url).await.expect("add relay");
    publisher.connect().await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    publisher
        .send_event(&new_note)
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
    let contact_list = event_builder!(Kind::ContactList, "", [p_tag(alice_keys.public_key())],)
        .custom_created_at(Timestamp::from_secs(10))
        .sign_with_keys(&root_keys)
        .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let old_note = event_builder!(Kind::TextNote, "old")
        .custom_created_at(Timestamp::from_secs(20))
        .sign_with_keys(&alice_keys)
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
    let contact_list = event_builder!(Kind::ContactList, "", [p_tag(alice_keys.public_key())],)
        .custom_created_at(Timestamp::from_secs(10))
        .sign_with_keys(&root_keys)
        .expect("contact list");
    graph.handle_event(&graph_event_from_nostr(&contact_list), true, 1.0);

    let alice_profile = event_builder!(Kind::Metadata, r#"{"name":"Alice"}"#)
        .custom_created_at(Timestamp::from_secs(20))
        .sign_with_keys(&alice_keys)
        .expect("alice profile");
    let alice_note = event_builder!(Kind::TextNote, "alice note")
        .custom_created_at(Timestamp::from_secs(199))
        .sign_with_keys(&alice_keys)
        .expect("alice note");
    let bob_note = event_builder!(Kind::TextNote, "bob noise")
        .custom_created_at(Timestamp::from_secs(200))
        .sign_with_keys(&bob_keys)
        .expect("bob note");

    let publisher = Client::new(Keys::generate());
    publisher.add_relay(&relay_url).await.expect("add relay");
    publisher.connect().await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    for event in [&alice_profile, &alice_note, &bob_note] {
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
