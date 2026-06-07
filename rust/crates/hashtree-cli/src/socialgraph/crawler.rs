use futures::stream::{self, StreamExt};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use nostr::Timestamp;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::{
    read_local_list_file_state, sync_local_list_files_force, sync_local_list_files_if_changed,
    LocalListFileState, SocialGraphBackend,
};

const DEFAULT_AUTHOR_BATCH_SIZE: usize = 500;
const DEFAULT_CONCURRENT_BATCHES: usize = 4;
const GRAPH_FETCH_TIMEOUT: Duration = Duration::from_secs(15);
const RELAY_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const SOCIALGRAPH_RELAY_EVENT_MAX_SIZE: u32 = 512 * 1024;
#[cfg(not(test))]
const CRAWLER_STARTUP_DELAY: Duration = Duration::from_secs(5);
#[cfg(test)]
const CRAWLER_STARTUP_DELAY: Duration = Duration::from_millis(50);
#[cfg(not(test))]
const LOCAL_LIST_POLL_INTERVAL: Duration = Duration::from_secs(5);
#[cfg(test)]
const LOCAL_LIST_POLL_INTERVAL: Duration = Duration::from_millis(100);

pub struct SocialGraphTaskHandles {
    pub shutdown_tx: watch::Sender<bool>,
    pub crawl_handle: JoinHandle<()>,
    pub local_list_handle: JoinHandle<()>,
}

pub struct SocialGraphCrawler {
    graph_store: Arc<dyn SocialGraphBackend>,
    spambox: Option<Arc<dyn SocialGraphBackend>>,
    keys: nostr::Keys,
    relays: Vec<String>,
    max_depth: u32,
    author_batch_size: usize,
    concurrent_batches: usize,
    full_recrawl: bool,
    known_since: Option<Timestamp>,
}

impl SocialGraphCrawler {
    pub fn new(
        graph_store: Arc<dyn SocialGraphBackend>,
        keys: nostr::Keys,
        relays: Vec<String>,
        max_depth: u32,
    ) -> Self {
        Self {
            graph_store,
            spambox: None,
            keys,
            relays,
            max_depth,
            author_batch_size: DEFAULT_AUTHOR_BATCH_SIZE,
            concurrent_batches: DEFAULT_CONCURRENT_BATCHES,
            full_recrawl: false,
            known_since: None,
        }
    }

    pub fn with_spambox(mut self, spambox: Arc<dyn SocialGraphBackend>) -> Self {
        self.spambox = Some(spambox);
        self
    }

    pub fn with_author_batch_size(mut self, author_batch_size: usize) -> Self {
        self.author_batch_size = author_batch_size.max(1);
        self
    }

    pub fn with_concurrent_batches(mut self, concurrent_batches: usize) -> Self {
        self.concurrent_batches = concurrent_batches.max(1);
        self
    }

    pub fn with_full_recrawl(mut self, full_recrawl: bool) -> Self {
        self.full_recrawl = full_recrawl;
        self
    }

    pub fn with_known_since(mut self, known_since: Option<u64>) -> Self {
        self.known_since = known_since.map(Timestamp::from);
        self
    }

    fn is_within_social_graph(&self, pk_bytes: &[u8; 32]) -> bool {
        if pk_bytes == &self.keys.public_key().to_bytes() {
            return true;
        }

        super::get_follow_distance(self.graph_store.as_ref(), pk_bytes)
            .map(|distance| distance <= self.max_depth)
            .unwrap_or(false)
    }

    fn ingest_events_into(
        &self,
        graph_store: &(impl SocialGraphBackend + ?Sized),
        events: &[nostr::Event],
    ) {
        if let Err(err) = super::ingest_parsed_events(graph_store, events) {
            tracing::debug!("Failed to ingest crawler event: {}", err);
        }
    }

    #[allow(deprecated)]
    fn collect_missing_root_follows(
        &self,
        event: &nostr::Event,
        fetched_contact_lists: &mut HashSet<[u8; 32]>,
    ) -> Vec<[u8; 32]> {
        if self.max_depth < 2 || event.kind != nostr::Kind::ContactList {
            return Vec::new();
        }

        let root_pk = self.keys.public_key().to_bytes();
        if event.pubkey.to_bytes() != root_pk {
            return Vec::new();
        }

        let mut missing = Vec::new();
        for tag in event.tags.iter() {
            if let Some(nostr::TagStandard::PublicKey { public_key, .. }) = tag.as_standardized() {
                let pk_bytes = public_key.to_bytes();
                if fetched_contact_lists.contains(&pk_bytes) {
                    continue;
                }

                let existing_follows = super::get_follows(self.graph_store.as_ref(), &pk_bytes);
                if !existing_follows.is_empty() {
                    fetched_contact_lists.insert(pk_bytes);
                    continue;
                }

                fetched_contact_lists.insert(pk_bytes);
                missing.push(pk_bytes);
            }
        }

        missing
    }

    fn graph_filter_for_pubkeys(
        pubkeys: &[[u8; 32]],
        since: Option<Timestamp>,
    ) -> Option<nostr::Filter> {
        let authors = pubkeys
            .iter()
            .filter_map(|pk_bytes| nostr::PublicKey::from_slice(pk_bytes).ok())
            .collect::<Vec<_>>();
        if authors.is_empty() {
            return None;
        }

        let mut filter = nostr::Filter::new()
            .authors(authors)
            .kinds(vec![nostr::Kind::ContactList, nostr::Kind::MuteList]);
        if let Some(since) = since {
            filter = filter.since(since);
        }

        Some(filter)
    }

    async fn fetch_graph_events_for_pubkeys(
        &self,
        client: &nostr_sdk::Client,
        pubkeys: &[[u8; 32]],
        since: Option<Timestamp>,
    ) -> Vec<nostr::Event> {
        let Some(filter) = Self::graph_filter_for_pubkeys(pubkeys, since) else {
            return Vec::new();
        };

        match client.fetch_events(filter, GRAPH_FETCH_TIMEOUT).await {
            Ok(events) => events.to_vec(),
            Err(err) => {
                tracing::debug!(
                    "Failed to fetch graph events for {} authors: {}",
                    pubkeys.len(),
                    err
                );
                Vec::new()
            }
        }
    }

    async fn fetch_contact_lists_for_pubkeys(
        &self,
        client: &nostr_sdk::Client,
        pubkeys: &[[u8; 32]],
        since: Option<Timestamp>,
        shutdown_rx: &watch::Receiver<bool>,
    ) {
        let chunk_futures = stream::iter(
            pubkeys
                .chunks(self.author_batch_size)
                .map(|chunk| chunk.to_vec())
                .collect::<Vec<_>>(),
        )
        .map(|chunk| async move {
            self.fetch_graph_events_for_pubkeys(client, &chunk, since)
                .await
        });

        let mut in_flight = chunk_futures.buffer_unordered(self.concurrent_batches);
        while let Some(events) = in_flight.next().await {
            if *shutdown_rx.borrow() {
                break;
            }
            if let Err(err) = super::ingest_graph_parsed_events(self.graph_store.as_ref(), &events)
            {
                tracing::debug!("Failed to ingest crawler graph batch: {}", err);
            }
        }
    }

    fn authors_to_fetch_at_distance(&self, distance: u32) -> (Vec<[u8; 32]>, Vec<[u8; 32]>) {
        let Ok(users) = self.graph_store.users_by_follow_distance(distance) else {
            return (Vec::new(), Vec::new());
        };
        if self.full_recrawl {
            return (users, Vec::new());
        }

        let mut new_authors = Vec::new();
        let mut known_authors = Vec::new();
        let refresh_known_authors = self.known_since.is_some();
        for pk_bytes in users {
            match self
                .graph_store
                .follow_list_created_at(&pk_bytes)
                .ok()
                .flatten()
            {
                Some(_) if refresh_known_authors => known_authors.push(pk_bytes),
                Some(_) => {}
                None => new_authors.push(pk_bytes),
            }
        }
        (new_authors, known_authors)
    }

    async fn connect_client(&self) -> Option<nostr_sdk::Client> {
        use nostr::nips::nip19::ToBech32;

        let secret = self.keys.secret_key().to_bech32().unwrap_or_default();
        let Ok(sdk_keys) = nostr_sdk::Keys::parse(&secret) else {
            return None;
        };

        let mut relay_limits = nostr_sdk::pool::RelayLimits::default();
        relay_limits.events.max_size = Some(SOCIALGRAPH_RELAY_EVENT_MAX_SIZE);
        let client = nostr_sdk::Client::builder()
            .signer(sdk_keys)
            .opts(nostr_sdk::ClientOptions::new().relay_limits(relay_limits))
            .build();
        for relay in &self.relays {
            if let Err(err) = client.add_relay(relay).await {
                tracing::warn!("Failed to add relay {}: {}", relay, err);
            }
        }
        let _ = tokio::time::timeout(RELAY_CONNECT_TIMEOUT, client.connect()).await;
        Some(client)
    }

    async fn sync_graph_once(
        &self,
        client: &nostr_sdk::Client,
        shutdown_rx: &watch::Receiver<bool>,
        fetched_contact_lists: &mut HashSet<[u8; 32]>,
    ) {
        for distance in 0..=self.max_depth {
            if *shutdown_rx.borrow() {
                break;
            }

            let (new_authors, known_authors) = self.authors_to_fetch_at_distance(distance);
            if new_authors.is_empty() && known_authors.is_empty() {
                continue;
            }

            tracing::debug!(
                "Social graph sync distance={} new_authors={} known_authors={}",
                distance,
                new_authors.len(),
                known_authors.len()
            );

            if !new_authors.is_empty() {
                for pk_bytes in &new_authors {
                    fetched_contact_lists.insert(*pk_bytes);
                }
                self.fetch_contact_lists_for_pubkeys(client, &new_authors, None, shutdown_rx)
                    .await;
            }

            if !known_authors.is_empty() {
                for pk_bytes in &known_authors {
                    fetched_contact_lists.insert(*pk_bytes);
                }
                self.fetch_contact_lists_for_pubkeys(
                    client,
                    &known_authors,
                    self.known_since,
                    shutdown_rx,
                )
                .await;
            }
        }
    }

    pub async fn warm_once(&self) {
        if self.relays.is_empty() {
            tracing::warn!("Social graph crawler: no relays configured, skipping");
            return;
        }

        let Some(client) = self.connect_client().await else {
            return;
        };
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut fetched_contact_lists: HashSet<[u8; 32]> = HashSet::new();
        self.sync_graph_once(&client, &shutdown_rx, &mut fetched_contact_lists)
            .await;
        let _ = client.disconnect().await;
    }

    pub(crate) fn handle_incoming_event(&self, event: &nostr::Event) {
        let is_contact_list = event.kind == nostr::Kind::ContactList;
        let is_mute_list = event.kind == nostr::Kind::MuteList;
        if !is_contact_list && !is_mute_list {
            return;
        }

        let pk_bytes = event.pubkey.to_bytes();
        if self.is_within_social_graph(&pk_bytes) {
            self.ingest_events_into(self.graph_store.as_ref(), std::slice::from_ref(event));
            return;
        }

        if let Some(spambox) = &self.spambox {
            self.ingest_events_into(spambox.as_ref(), std::slice::from_ref(event));
        }
    }

    #[allow(deprecated)]
    pub async fn crawl(&self, shutdown_rx: watch::Receiver<bool>) {
        use nostr_sdk::prelude::RelayPoolNotification;

        if self.relays.is_empty() {
            tracing::warn!("Social graph crawler: no relays configured, skipping");
            return;
        }

        let mut shutdown_rx = shutdown_rx;
        if *shutdown_rx.borrow() {
            return;
        }

        let Some(client) = self.connect_client().await else {
            return;
        };

        let mut fetched_contact_lists: HashSet<[u8; 32]> = HashSet::new();
        self.sync_graph_once(&client, &shutdown_rx, &mut fetched_contact_lists)
            .await;

        let filter = nostr::Filter::new()
            .kinds(vec![nostr::Kind::ContactList, nostr::Kind::MuteList])
            .since(nostr::Timestamp::now());

        let _ = client.subscribe(filter, None).await;

        let mut notifications = client.notifications();
        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                }
                notification = notifications.recv() => {
                    match notification {
                        Ok(RelayPoolNotification::Event { event, .. }) => {
                            self.handle_incoming_event(&event);
                            let missing = self.collect_missing_root_follows(&event, &mut fetched_contact_lists);
                            if !missing.is_empty() {
                                self.fetch_contact_lists_for_pubkeys(&client, &missing, None, &shutdown_rx).await;
                            }
                        }
                        Ok(_) => {}
                        Err(err) => {
                            tracing::warn!("Social graph crawler notification error: {}", err);
                            break;
                        }
                    }
                }
            }
        }

        let _ = client.disconnect().await;
    }
}

pub fn spawn_social_graph_tasks(
    graph_store: Arc<dyn SocialGraphBackend>,
    keys: nostr::Keys,
    relays: Vec<String>,
    max_depth: u32,
    spambox: Option<Arc<dyn SocialGraphBackend>>,
    data_dir: PathBuf,
) -> SocialGraphTaskHandles {
    let (shutdown_tx, crawl_shutdown_rx) = watch::channel(false);
    let local_list_shutdown_rx = crawl_shutdown_rx.clone();
    let crawl_data_dir = data_dir.clone();
    let local_list_data_dir = data_dir;

    let crawl_handle = tokio::spawn({
        let graph_store = Arc::clone(&graph_store);
        let keys = keys.clone();
        let relays = relays.clone();
        let spambox = spambox.clone();
        async move {
            tokio::time::sleep(CRAWLER_STARTUP_DELAY).await;
            let mut crawler = SocialGraphCrawler::new(graph_store.clone(), keys, relays, max_depth);
            if let Some(spambox) = spambox {
                crawler = crawler.with_spambox(spambox);
            }
            if let Err(err) =
                sync_local_list_files_force(graph_store.as_ref(), &crawl_data_dir, &crawler.keys)
            {
                tracing::warn!(
                    "Failed to sync local social graph lists at startup: {}",
                    err
                );
            }
            crawler.crawl(crawl_shutdown_rx).await;
        }
    });

    let local_list_handle = tokio::spawn({
        let graph_store = Arc::clone(&graph_store);
        let keys = keys.clone();
        let relays = relays.clone();
        let spambox = spambox.clone();
        async move {
            let mut shutdown_rx = local_list_shutdown_rx;
            let mut state =
                read_local_list_file_state(&local_list_data_dir).unwrap_or_else(|err| {
                    tracing::warn!("Failed to read local social graph list state: {}", err);
                    LocalListFileState::default()
                });
            let mut interval = tokio::time::interval(LOCAL_LIST_POLL_INTERVAL);
            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    _ = interval.tick() => {
                        let outcome = match sync_local_list_files_if_changed(
                            graph_store.as_ref(),
                            &local_list_data_dir,
                            &keys,
                            &mut state,
                        ) {
                            Ok(outcome) => outcome,
                            Err(err) => {
                                tracing::warn!("Failed to sync local social graph list files: {}", err);
                                continue;
                            }
                        };

                        if outcome.contacts_changed {
                            let mut crawler =
                                SocialGraphCrawler::new(graph_store.clone(), keys.clone(), relays.clone(), max_depth);
                            if let Some(spambox) = spambox.clone() {
                                crawler = crawler.with_spambox(spambox);
                            }
                            crawler.warm_once().await;
                        }
                    }
                }
            }
        }
    });

    SocialGraphTaskHandles {
        shutdown_tx,
        crawl_handle,
        local_list_handle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::Mutex;
    use std::time::Instant;

    use futures::{SinkExt, StreamExt};
    use nostr::{EventBuilder, JsonUtil, Kind, PublicKey, Tag};
    use tempfile::TempDir;
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

    #[derive(Debug, Default)]
    struct RelayState {
        events: Vec<nostr::Event>,
        request_author_counts: Vec<usize>,
        requested_authors: Vec<Vec<String>>,
    }

    struct TestRelay {
        port: u16,
        shutdown: broadcast::Sender<()>,
        state: Arc<Mutex<RelayState>>,
    }

    impl TestRelay {
        fn new(events: Vec<nostr::Event>) -> Self {
            let state = Arc::new(Mutex::new(RelayState {
                events,
                request_author_counts: Vec::new(),
                requested_authors: Vec::new(),
            }));
            let (shutdown, _) = broadcast::channel(1);

            let std_listener = TcpListener::bind("127.0.0.1:0").expect("bind relay listener");
            let port = std_listener.local_addr().expect("local addr").port();
            std_listener
                .set_nonblocking(true)
                .expect("set listener nonblocking");

            let state_for_thread = Arc::clone(&state);
            let shutdown_for_thread = shutdown.clone();
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("build tokio runtime");
                runtime.block_on(async move {
                    let listener = tokio::net::TcpListener::from_std(std_listener)
                        .expect("tokio listener from std");
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

        fn url(&self) -> String {
            format!("ws://127.0.0.1:{}", self.port)
        }

        fn request_author_counts(&self) -> Vec<usize> {
            self.state
                .lock()
                .expect("relay state lock")
                .request_author_counts
                .clone()
        }

        fn requested_authors(&self) -> Vec<Vec<String>> {
            self.state
                .lock()
                .expect("relay state lock")
                .requested_authors
                .clone()
        }
    }

    impl Drop for TestRelay {
        fn drop(&mut self) {
            let _ = self.shutdown.send(());
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn matching_events(
        state: &Arc<Mutex<RelayState>>,
        filters: &[nostr::Filter],
    ) -> Vec<nostr::Event> {
        let guard = state.lock().expect("relay state lock");
        guard
            .events
            .iter()
            .filter(|event| {
                filters.is_empty()
                    || filters
                        .iter()
                        .any(|filter| filter.match_event(event, Default::default()))
            })
            .cloned()
            .collect()
    }

    async fn send_relay_message(
        write: &mut futures::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<TcpStream>,
            Message,
        >,
        message: nostr::RelayMessage<'_>,
    ) {
        let _ = write.send(Message::Text(message.as_json())).await;
    }

    async fn handle_connection(stream: TcpStream, state: Arc<Mutex<RelayState>>) {
        let ws_stream = match accept_async(stream).await {
            Ok(ws) => ws,
            Err(_) => return,
        };
        let (mut write, mut read) = ws_stream.split();

        while let Some(message) = read.next().await {
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
                    let author_count = filters
                        .iter()
                        .filter_map(|filter| filter.authors.as_ref())
                        .map(|authors| authors.len())
                        .sum();
                    let mut requested_authors = filters
                        .iter()
                        .filter_map(|filter| filter.authors.as_ref())
                        .flat_map(|authors| authors.iter().map(|author| author.to_hex()))
                        .collect::<Vec<_>>();
                    requested_authors.sort();
                    requested_authors.dedup();
                    {
                        let mut guard = state.lock().expect("relay state lock");
                        guard.request_author_counts.push(author_count);
                        guard.requested_authors.push(requested_authors);
                    }

                    for event in matching_events(&state, &filters) {
                        send_relay_message(
                            &mut write,
                            nostr::RelayMessage::event(subscription_id.clone(), event),
                        )
                        .await;
                    }
                    send_relay_message(&mut write, nostr::RelayMessage::eose(subscription_id))
                        .await;
                }
                nostr::ClientMessage::Close(subscription_id) => {
                    send_relay_message(
                        &mut write,
                        nostr::RelayMessage::closed(subscription_id.into_owned(), ""),
                    )
                    .await;
                }
                _ => {}
            }
        }
    }

    async fn wait_until<F>(timeout: Duration, mut condition: F)
    where
        F: FnMut() -> bool,
    {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if condition() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("condition not met within {:?}", timeout);
    }

    fn write_contacts_file(path: &std::path::Path, pubkeys: &[String]) {
        std::fs::write(
            path,
            serde_json::to_string(pubkeys).expect("serialize contacts"),
        )
        .expect("write contacts file");
    }

    #[tokio::test]
    async fn test_crawler_routes_untrusted_to_spambox() {
        let _guard = crate::socialgraph::test_lock();
        let tmp = TempDir::new().unwrap();
        let graph_store = crate::socialgraph::open_social_graph_store(tmp.path()).unwrap();
        let spambox =
            crate::socialgraph::open_social_graph_store_at_path(&tmp.path().join("spambox"), None)
                .unwrap();

        let root_keys = nostr::Keys::generate();
        let root_pk = root_keys.public_key().to_bytes();
        crate::socialgraph::set_social_graph_root(&graph_store, &root_pk);
        let backend: Arc<dyn crate::socialgraph::SocialGraphBackend> = graph_store.clone();
        let spambox_backend: Arc<dyn crate::socialgraph::SocialGraphBackend> = spambox.clone();

        let crawler = SocialGraphCrawler::new(backend, root_keys.clone(), vec![], 2)
            .with_spambox(spambox_backend);

        let unknown_keys = nostr::Keys::generate();
        let follow_tag = Tag::public_key(PublicKey::from_slice(&root_pk).unwrap());
        let event = event_builder!(Kind::ContactList, "")
            .tags(vec![follow_tag])
            .sign_with_keys(&unknown_keys)
            .unwrap();

        crawler.handle_incoming_event(&event);

        let unknown_pk = unknown_keys.public_key().to_bytes();
        assert!(crate::socialgraph::get_follows(&graph_store, &unknown_pk).is_empty());
        assert_eq!(
            crate::socialgraph::get_follows(&spambox, &unknown_pk),
            vec![root_pk]
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_crawler_batches_graph_fetches_by_author_chunk() {
        let _guard = crate::socialgraph::test_lock();
        let tmp = TempDir::new().unwrap();
        let graph_store = crate::socialgraph::open_social_graph_store(tmp.path()).unwrap();

        let root_keys = nostr::Keys::generate();
        let root_pk = root_keys.public_key().to_bytes();
        crate::socialgraph::set_social_graph_root(&graph_store, &root_pk);
        let backend: Arc<dyn crate::socialgraph::SocialGraphBackend> = graph_store.clone();

        let alice_keys = nostr::Keys::generate();
        let bob_keys = nostr::Keys::generate();
        let carol_keys = nostr::Keys::generate();

        let root_event = event_builder!(
            Kind::ContactList,
            "",
            vec![
                Tag::public_key(alice_keys.public_key()),
                Tag::public_key(bob_keys.public_key()),
            ],
        )
        .custom_created_at(nostr::Timestamp::from(10))
        .sign_with_keys(&root_keys)
        .unwrap();
        let alice_event = event_builder!(
            Kind::ContactList,
            "",
            vec![Tag::public_key(carol_keys.public_key())],
        )
        .custom_created_at(nostr::Timestamp::from(11))
        .sign_with_keys(&alice_keys)
        .unwrap();
        let bob_event = event_builder!(Kind::ContactList, "")
            .custom_created_at(nostr::Timestamp::from(12))
            .sign_with_keys(&bob_keys)
            .unwrap();

        let relay = TestRelay::new(vec![root_event, alice_event, bob_event]);
        let crawler = SocialGraphCrawler::new(backend, root_keys.clone(), vec![relay.url()], 2)
            .with_author_batch_size(2);

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let handle = tokio::spawn(async move {
            crawler.crawl(shutdown_rx).await;
        });

        let alice_pk = alice_keys.public_key().to_bytes();
        let bob_pk = bob_keys.public_key().to_bytes();
        let carol_pk = carol_keys.public_key().to_bytes();
        wait_until(Duration::from_secs(5), || {
            let root_follows = crate::socialgraph::get_follows(&graph_store, &root_pk);
            let alice_follows = crate::socialgraph::get_follows(&graph_store, &alice_pk);
            root_follows.contains(&alice_pk)
                && root_follows.contains(&bob_pk)
                && alice_follows.contains(&carol_pk)
        })
        .await;

        let _ = shutdown_tx.send(true);
        handle.await.unwrap();

        let author_counts = relay.request_author_counts();
        assert!(
            author_counts.iter().any(|count| *count >= 2),
            "expected batched author REQ, got {:?}",
            author_counts
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_crawler_expands_from_existing_graph_without_root_refetch() {
        let _guard = crate::socialgraph::test_lock();
        let tmp = TempDir::new().unwrap();
        let graph_store = crate::socialgraph::open_social_graph_store(tmp.path()).unwrap();

        let root_keys = nostr::Keys::generate();
        let root_pk = root_keys.public_key().to_bytes();
        crate::socialgraph::set_social_graph_root(&graph_store, &root_pk);
        let backend: Arc<dyn crate::socialgraph::SocialGraphBackend> = graph_store.clone();

        let alice_keys = nostr::Keys::generate();
        let bob_keys = nostr::Keys::generate();
        let carol_keys = nostr::Keys::generate();

        let root_event = event_builder!(
            Kind::ContactList,
            "",
            vec![
                Tag::public_key(alice_keys.public_key()),
                Tag::public_key(bob_keys.public_key()),
            ],
        )
        .custom_created_at(nostr::Timestamp::from(10))
        .sign_with_keys(&root_keys)
        .unwrap();
        crate::socialgraph::ingest_parsed_event(&graph_store, &root_event).unwrap();

        let alice_event = event_builder!(
            Kind::ContactList,
            "",
            vec![Tag::public_key(carol_keys.public_key())],
        )
        .custom_created_at(nostr::Timestamp::from(11))
        .sign_with_keys(&alice_keys)
        .unwrap();
        let bob_event = event_builder!(Kind::ContactList, "")
            .custom_created_at(nostr::Timestamp::from(12))
            .sign_with_keys(&bob_keys)
            .unwrap();

        let relay = TestRelay::new(vec![alice_event, bob_event]);
        let crawler = SocialGraphCrawler::new(backend, root_keys.clone(), vec![relay.url()], 2)
            .with_author_batch_size(2);

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let handle = tokio::spawn(async move {
            crawler.crawl(shutdown_rx).await;
        });

        let alice_pk = alice_keys.public_key().to_bytes();
        let carol_pk = carol_keys.public_key().to_bytes();
        wait_until(Duration::from_secs(5), || {
            crate::socialgraph::get_follows(&graph_store, &alice_pk).contains(&carol_pk)
        })
        .await;

        let _ = shutdown_tx.send(true);
        handle.await.unwrap();

        let author_counts = relay.request_author_counts();
        let requested_authors = relay.requested_authors();
        assert!(
            requested_authors
                .iter()
                .flatten()
                .all(|author| author != &root_keys.public_key().to_hex()),
            "expected incremental crawl to skip root refetch, got {:?}",
            requested_authors
        );
        assert!(
            author_counts.iter().any(|count| *count >= 2),
            "expected batched distance-1 REQ, got {:?}",
            author_counts
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_crawler_full_recrawl_refetches_root() {
        let _guard = crate::socialgraph::test_lock();
        let tmp = TempDir::new().unwrap();
        let graph_store = crate::socialgraph::open_social_graph_store(tmp.path()).unwrap();

        let root_keys = nostr::Keys::generate();
        let root_pk = root_keys.public_key().to_bytes();
        crate::socialgraph::set_social_graph_root(&graph_store, &root_pk);
        let backend: Arc<dyn crate::socialgraph::SocialGraphBackend> = graph_store.clone();

        let alice_keys = nostr::Keys::generate();
        let bob_keys = nostr::Keys::generate();

        let root_event = event_builder!(
            Kind::ContactList,
            "",
            vec![
                Tag::public_key(alice_keys.public_key()),
                Tag::public_key(bob_keys.public_key()),
            ],
        )
        .custom_created_at(nostr::Timestamp::from(10))
        .sign_with_keys(&root_keys)
        .unwrap();
        crate::socialgraph::ingest_parsed_event(&graph_store, &root_event).unwrap();

        let relay = TestRelay::new(vec![root_event]);
        let crawler = SocialGraphCrawler::new(backend, root_keys.clone(), vec![relay.url()], 1)
            .with_full_recrawl(true);

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let handle = tokio::spawn(async move {
            crawler.crawl(shutdown_rx).await;
        });

        wait_until(Duration::from_secs(5), || {
            relay.request_author_counts().contains(&1)
        })
        .await;

        let _ = shutdown_tx.send(true);
        handle.await.unwrap();

        let requested_authors = relay.requested_authors();
        assert!(
            requested_authors
                .iter()
                .flatten()
                .any(|author| author == &root_keys.public_key().to_hex()),
            "expected full recrawl to refetch root, got {:?}",
            requested_authors
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_crawler_revisits_known_authors_with_since_cursor() {
        let _guard = crate::socialgraph::test_lock();
        let tmp = TempDir::new().unwrap();
        let graph_store = crate::socialgraph::open_social_graph_store(tmp.path()).unwrap();

        let root_keys = nostr::Keys::generate();
        let root_pk = root_keys.public_key().to_bytes();
        crate::socialgraph::set_social_graph_root(&graph_store, &root_pk);
        let backend: Arc<dyn crate::socialgraph::SocialGraphBackend> = graph_store.clone();

        let alice_keys = nostr::Keys::generate();
        let carol_keys = nostr::Keys::generate();
        let dave_keys = nostr::Keys::generate();

        let root_event = event_builder!(
            Kind::ContactList,
            "",
            vec![Tag::public_key(alice_keys.public_key())],
        )
        .custom_created_at(nostr::Timestamp::from(10))
        .sign_with_keys(&root_keys)
        .unwrap();
        crate::socialgraph::ingest_parsed_event(&graph_store, &root_event).unwrap();

        let alice_old_event = event_builder!(
            Kind::ContactList,
            "",
            vec![Tag::public_key(carol_keys.public_key())],
        )
        .custom_created_at(nostr::Timestamp::from(11))
        .sign_with_keys(&alice_keys)
        .unwrap();
        crate::socialgraph::ingest_parsed_event(&graph_store, &alice_old_event).unwrap();

        let alice_new_event = event_builder!(
            Kind::ContactList,
            "",
            vec![
                Tag::public_key(carol_keys.public_key()),
                Tag::public_key(dave_keys.public_key()),
            ],
        )
        .custom_created_at(nostr::Timestamp::from(20))
        .sign_with_keys(&alice_keys)
        .unwrap();

        let relay = TestRelay::new(vec![alice_new_event]);
        let crawler = SocialGraphCrawler::new(backend, root_keys.clone(), vec![relay.url()], 2)
            .with_author_batch_size(2)
            .with_known_since(Some(15));

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let handle = tokio::spawn(async move {
            crawler.crawl(shutdown_rx).await;
        });

        let alice_pk = alice_keys.public_key().to_bytes();
        let dave_pk = dave_keys.public_key().to_bytes();
        wait_until(Duration::from_secs(5), || {
            crate::socialgraph::get_follows(&graph_store, &alice_pk).contains(&dave_pk)
        })
        .await;

        let _ = shutdown_tx.send(true);
        handle.await.unwrap();

        let requested_authors = relay.requested_authors();
        assert!(
            crate::socialgraph::get_follows(&graph_store, &alice_pk).contains(&dave_pk),
            "expected incremental crawl to refresh known author follow list"
        );
        assert!(
            requested_authors
                .iter()
                .flatten()
                .any(|author| author == &alice_keys.public_key().to_hex()),
            "expected crawler to refetch known author, got {:?}",
            requested_authors
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_crawler_warm_once_completes_initial_sync_without_shutdown() {
        let _guard = crate::socialgraph::test_lock();
        let tmp = TempDir::new().unwrap();
        let graph_store = crate::socialgraph::open_social_graph_store(tmp.path()).unwrap();

        let root_keys = nostr::Keys::generate();
        let root_pk = root_keys.public_key().to_bytes();
        crate::socialgraph::set_social_graph_root(&graph_store, &root_pk);
        let backend: Arc<dyn crate::socialgraph::SocialGraphBackend> = graph_store.clone();

        let alice_keys = nostr::Keys::generate();
        let bob_keys = nostr::Keys::generate();

        let root_event = event_builder!(
            Kind::ContactList,
            "",
            vec![Tag::public_key(alice_keys.public_key())],
        )
        .custom_created_at(nostr::Timestamp::from(10))
        .sign_with_keys(&root_keys)
        .unwrap();
        let alice_event = event_builder!(
            Kind::ContactList,
            "",
            vec![Tag::public_key(bob_keys.public_key())],
        )
        .custom_created_at(nostr::Timestamp::from(11))
        .sign_with_keys(&alice_keys)
        .unwrap();

        let relay = TestRelay::new(vec![root_event, alice_event]);
        let crawler = SocialGraphCrawler::new(backend, root_keys.clone(), vec![relay.url()], 2)
            .with_author_batch_size(2);

        let started = std::time::Instant::now();
        crawler.warm_once().await;

        let alice_pk = alice_keys.public_key().to_bytes();
        let bob_pk = bob_keys.public_key().to_bytes();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "warm_once should complete finite sync promptly"
        );
        assert!(
            crate::socialgraph::get_follows(&graph_store, &alice_pk).contains(&bob_pk),
            "expected warm_once to ingest distance-1 follow list"
        );
    }

    #[tokio::test]
    async fn test_crawler_warm_once_accepts_large_contact_list_events() {
        let _guard = crate::socialgraph::test_lock();
        let tmp = TempDir::new().unwrap();
        let graph_store = crate::socialgraph::open_social_graph_store(tmp.path()).unwrap();

        let root_keys = nostr::Keys::generate();
        let root_pk = root_keys.public_key().to_bytes();
        crate::socialgraph::set_social_graph_root(&graph_store, &root_pk);
        let backend: Arc<dyn crate::socialgraph::SocialGraphBackend> = graph_store.clone();

        let followed_keys = (0..1_600)
            .map(|_| nostr::Keys::generate())
            .collect::<Vec<_>>();
        let tags = followed_keys
            .iter()
            .map(|keys| Tag::public_key(keys.public_key()))
            .collect::<Vec<_>>();
        let root_event = event_builder!(Kind::ContactList, "")
            .tags(tags)
            .custom_created_at(nostr::Timestamp::from(10))
            .sign_with_keys(&root_keys)
            .unwrap();
        assert!(
            root_event.as_json().len() > 70_000,
            "test event should exceed nostr-sdk default size limit"
        );

        let relay = TestRelay::new(vec![root_event]);
        let crawler = SocialGraphCrawler::new(backend, root_keys.clone(), vec![relay.url()], 1)
            .with_author_batch_size(1);

        crawler.warm_once().await;

        let follows = crate::socialgraph::get_follows(&graph_store, &root_pk);
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
            "expected warm_once to ingest oversized contact list event"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_spawned_social_graph_tasks_sync_local_contacts_on_startup() {
        let _guard = crate::socialgraph::test_lock();
        let tmp = TempDir::new().unwrap();
        let graph_store = crate::socialgraph::open_social_graph_store(tmp.path()).unwrap();

        let root_keys = nostr::Keys::generate();
        let root_pk = root_keys.public_key().to_bytes();
        crate::socialgraph::set_social_graph_root(&graph_store, &root_pk);

        let alice_keys = nostr::Keys::generate();
        let bob_keys = nostr::Keys::generate();

        write_contacts_file(
            &tmp.path().join("contacts.json"),
            &[alice_keys.public_key().to_hex()],
        );

        let alice_event = event_builder!(
            Kind::ContactList,
            "",
            vec![Tag::public_key(bob_keys.public_key())],
        )
        .custom_created_at(nostr::Timestamp::from(11))
        .sign_with_keys(&alice_keys)
        .unwrap();

        let relay = TestRelay::new(vec![alice_event]);
        let tasks = spawn_social_graph_tasks(
            graph_store.clone(),
            root_keys.clone(),
            vec![relay.url()],
            2,
            None,
            tmp.path().to_path_buf(),
        );

        let alice_pk = alice_keys.public_key().to_bytes();
        let bob_pk = bob_keys.public_key().to_bytes();
        wait_until(Duration::from_secs(5), || {
            let root_follows = crate::socialgraph::get_follows(&graph_store, &root_pk);
            let alice_follows = crate::socialgraph::get_follows(&graph_store, &alice_pk);
            root_follows.contains(&alice_pk) && alice_follows.contains(&bob_pk)
        })
        .await;

        let _ = tasks.shutdown_tx.send(true);
        tasks.crawl_handle.abort();
        tasks.local_list_handle.abort();
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_spawned_social_graph_tasks_refresh_when_contacts_change() {
        let _guard = crate::socialgraph::test_lock();
        let tmp = TempDir::new().unwrap();
        let graph_store = crate::socialgraph::open_social_graph_store(tmp.path()).unwrap();

        let root_keys = nostr::Keys::generate();
        let root_pk = root_keys.public_key().to_bytes();
        crate::socialgraph::set_social_graph_root(&graph_store, &root_pk);

        let alice_keys = nostr::Keys::generate();
        let bob_keys = nostr::Keys::generate();

        let alice_event = event_builder!(
            Kind::ContactList,
            "",
            vec![Tag::public_key(bob_keys.public_key())],
        )
        .custom_created_at(nostr::Timestamp::from(11))
        .sign_with_keys(&alice_keys)
        .unwrap();

        let relay = TestRelay::new(vec![alice_event]);
        let tasks = spawn_social_graph_tasks(
            graph_store.clone(),
            root_keys.clone(),
            vec![relay.url()],
            2,
            None,
            tmp.path().to_path_buf(),
        );

        let alice_pk = alice_keys.public_key().to_bytes();
        let bob_pk = bob_keys.public_key().to_bytes();
        assert!(!crate::socialgraph::get_follows(&graph_store, &root_pk).contains(&alice_pk));

        write_contacts_file(
            &tmp.path().join("contacts.json"),
            &[alice_keys.public_key().to_hex()],
        );

        wait_until(Duration::from_secs(5), || {
            let root_follows = crate::socialgraph::get_follows(&graph_store, &root_pk);
            let alice_follows = crate::socialgraph::get_follows(&graph_store, &alice_pk);
            root_follows.contains(&alice_pk) && alice_follows.contains(&bob_pk)
        })
        .await;

        let _ = tasks.shutdown_tx.send(true);
        tasks.crawl_handle.abort();
        tasks.local_list_handle.abort();
    }
}
