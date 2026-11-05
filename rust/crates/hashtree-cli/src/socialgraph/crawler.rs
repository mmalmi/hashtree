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

const DEFAULT_AUTHOR_BATCH_SIZE: usize = 128;
const MAX_GRAPH_BATCH_BYTES: usize = 8 * 1024 * 1024;
const MAX_GRAPH_IDS_PER_REQUEST: usize = 64;
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
    negentropy: bool,
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
            negentropy: false,
            known_since: None,
        }
    }

    pub fn with_spambox(mut self, spambox: Arc<dyn SocialGraphBackend>) -> Self {
        self.spambox = Some(spambox);
        self
    }

    pub fn with_author_batch_size(mut self, author_batch_size: usize) -> Self {
        self.author_batch_size = author_batch_size.clamp(1, 128);
        self
    }

    pub fn with_concurrent_batches(mut self, concurrent_batches: usize) -> Self {
        self.concurrent_batches = concurrent_batches.max(1);
        self
    }

    /// Reconcile complete follow/mute IDs before downloading changed lists.
    /// Failed reconciliation and oversized differences skip the download.
    /// Ordinary author queries require explicitly disabling reconciliation.
    pub fn with_negentropy(mut self, enabled: bool) -> Self {
        self.negentropy = enabled;
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
        use nostr::JsonUtil;
        // Reconciliation covers the complete current heads, including old events
        // absent from a partial snapshot. A `since` filter could hide those gaps.
        let Some(filter) =
            Self::graph_filter_for_pubkeys(pubkeys, if self.negentropy { None } else { since })
        else {
            return Vec::new();
        };
        let limit = pubkeys.len().saturating_mul(2).max(1);
        let mut queries = vec![filter.clone().limit(limit)];
        let mut pending = HashSet::new();
        let mut reconciled_relays = Vec::new();
        if self.negentropy {
            let items = self
                .graph_store
                .list_revision_items(pubkeys)
                .unwrap_or_default();
            let options = nostr_sdk::SyncOptions::new()
                .dry_run()
                .initial_timeout(Duration::from_secs(1));
            let mut missing = HashSet::new();
            // Dry-run only exchanges IDs: the SDK must never upload local events.
            let mut available = client.relays().await;
            let selected = self
                .relays
                .iter()
                .filter_map(|url| nostr::RelayUrl::parse(url).ok())
                .filter_map(|url| available.remove(&url))
                .take(2)
                .collect::<Vec<_>>();
            for relay in selected {
                match tokio::time::timeout(
                    GRAPH_FETCH_TIMEOUT,
                    relay.sync_with_items(filter.clone(), items.clone(), &options),
                )
                .await
                {
                    Ok(Ok(result)) => {
                        reconciled_relays.push(relay.url().clone());
                        missing.extend(result.remote);
                    }
                    _ => continue,
                }
            }
            // Never broaden a failed or over-budget reconciliation into an
            // author/kind request. A later reconciliation may retry the batch.
            if missing.is_empty() || missing.len() > limit {
                return Vec::new();
            }
            let ids = missing.iter().copied().collect::<Vec<_>>();
            queries = ids
                .chunks(MAX_GRAPH_IDS_PER_REQUEST)
                .map(|chunk| {
                    nostr::Filter::new()
                        .ids(chunk.iter().copied())
                        .limit(chunk.len())
                })
                .collect();
            pending = missing;
        }
        // All ID chunks share one deadline, byte budget and event cap. A relay's
        // small REQ cap must not masquerade as a complete reconciled batch.
        tokio::time::timeout(GRAPH_FETCH_TIMEOUT, async {
            let mut events = Vec::new();
            let mut seen = HashSet::new();
            let mut bytes = 0usize;
            for query in queries {
                let stream = if self.negentropy {
                    client
                        .stream_events_from(
                            reconciled_relays.clone(),
                            query.clone(),
                            GRAPH_FETCH_TIMEOUT,
                        )
                        .await
                } else {
                    client
                        .stream_events(query.clone(), GRAPH_FETCH_TIMEOUT)
                        .await
                };
                let Ok(mut stream) = stream else {
                    return Vec::new();
                };
                while let Some(event) = stream.next().await {
                    if !filter.match_event(&event, Default::default())
                        || !query.match_event(&event, Default::default())
                        || !seen.insert(event.id)
                    {
                        continue;
                    }
                    bytes = bytes.saturating_add(event.as_json().len());
                    if bytes > MAX_GRAPH_BATCH_BYTES || events.len() >= limit {
                        return if self.negentropy { Vec::new() } else { events };
                    }
                    pending.remove(&event.id);
                    events.push(event);
                }
            }
            if !pending.is_empty() {
                return Vec::new();
            }
            events
        })
        .await
        .unwrap_or_default()
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
        let mut shutdown = shutdown_rx.clone();
        while !*shutdown.borrow() {
            let events = tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { return; }
                    continue;
                }
                events = in_flight.next() => match events {
                    Some(events) => events,
                    None => break,
                }
            };
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
        let refresh_known_authors = self.negentropy || self.known_since.is_some();
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

        if *shutdown_rx.borrow() {
            client.disconnect().await;
            return;
        }
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
            let mut crawler = SocialGraphCrawler::new(graph_store.clone(), keys, relays, max_depth)
                .with_negentropy(true);
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
                                SocialGraphCrawler::new(graph_store.clone(), keys.clone(), relays.clone(), max_depth).with_negentropy(true);
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

    #[derive(Debug, Default, Clone, Copy)]
    enum NegentropyMode {
        #[default]
        Supported,
        Rejected,
        Silent,
        Malformed,
        Disconnected,
    }

    #[derive(Debug, Default)]
    struct RelayState {
        events: Vec<nostr::Event>,
        request_author_counts: Vec<usize>,
        requested_authors: Vec<Vec<String>>,
        negentropy: NegentropyMode,
        neg_opens: usize,
        requested_filters: Vec<nostr::Filter>,
        published_events: usize,
        default_response_limit: Option<usize>,
        max_response_limit: Option<usize>,
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
                negentropy: NegentropyMode::Supported,
                neg_opens: 0,
                requested_filters: Vec::new(),
                published_events: 0,
                default_response_limit: None,
                max_response_limit: None,
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

        let mut sessions = std::collections::HashMap::new();
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
                nostr::ClientMessage::Event(_) => {
                    state.lock().unwrap().published_events += 1;
                }
                nostr::ClientMessage::NegOpen {
                    subscription_id,
                    filter,
                    initial_message,
                    ..
                } => {
                    let subscription_id = subscription_id.into_owned();
                    let mode = {
                        let mut state = state.lock().unwrap();
                        state.neg_opens += 1;
                        state.negentropy
                    };
                    match mode {
                        NegentropyMode::Rejected => {
                            send_relay_message(
                                &mut write,
                                nostr::RelayMessage::NegErr {
                                    subscription_id: std::borrow::Cow::Owned(subscription_id),
                                    message: "unsupported".into(),
                                },
                            )
                            .await;
                            continue;
                        }
                        NegentropyMode::Silent => continue,
                        NegentropyMode::Malformed => {
                            send_relay_message(
                                &mut write,
                                nostr::RelayMessage::NegMsg {
                                    subscription_id: std::borrow::Cow::Owned(subscription_id),
                                    message: "00".into(),
                                },
                            )
                            .await;
                            continue;
                        }
                        NegentropyMode::Disconnected => {
                            let _ = write.send(Message::Close(None)).await;
                            break;
                        }
                        NegentropyMode::Supported => {}
                    }
                    let mut storage = negentropy::NegentropyStorageVector::new();
                    for event in matching_events(&state, &[filter.into_owned()]) {
                        storage
                            .insert(
                                event.created_at.as_secs(),
                                negentropy::Id::from_byte_array(event.id.to_bytes()),
                            )
                            .unwrap();
                    }
                    storage.seal().unwrap();
                    let mut neg = negentropy::Negentropy::owned(storage, 4096).unwrap();
                    let reply = neg
                        .reconcile(&hex::decode(initial_message.as_ref()).unwrap())
                        .unwrap();
                    send_relay_message(
                        &mut write,
                        nostr::RelayMessage::NegMsg {
                            subscription_id: std::borrow::Cow::Owned(subscription_id.clone()),
                            message: hex::encode(reply).into(),
                        },
                    )
                    .await;
                    sessions.insert(subscription_id, neg);
                }
                nostr::ClientMessage::NegMsg {
                    subscription_id,
                    message,
                } => {
                    if let Some(neg) = sessions.get_mut(subscription_id.as_ref()) {
                        let reply = neg
                            .reconcile(&hex::decode(message.as_ref()).unwrap())
                            .unwrap();
                        send_relay_message(
                            &mut write,
                            nostr::RelayMessage::NegMsg {
                                subscription_id: std::borrow::Cow::Owned(
                                    subscription_id.into_owned(),
                                ),
                                message: hex::encode(reply).into(),
                            },
                        )
                        .await;
                    }
                }
                nostr::ClientMessage::NegClose { subscription_id } => {
                    sessions.remove(subscription_id.as_ref());
                }
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
                        guard.requested_filters.extend(filters.clone());
                    }

                    let (default_limit, maximum_limit) = {
                        let guard = state.lock().unwrap();
                        (guard.default_response_limit, guard.max_response_limit)
                    };
                    let response_limit = filters
                        .iter()
                        .filter_map(|filter| filter.limit)
                        .max()
                        .or(default_limit)
                        .unwrap_or(usize::MAX)
                        .min(maximum_limit.unwrap_or(usize::MAX));
                    for event in matching_events(&state, &filters)
                        .into_iter()
                        .take(response_limit)
                    {
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
    async fn negentropy_refreshes_both_heads_without_fallback_or_uploads() {
        let _guard = crate::socialgraph::test_lock().await;
        let tmp = TempDir::new().unwrap();
        let store = crate::socialgraph::open_test_social_graph_store(tmp.path()).unwrap();
        let keys = nostr::Keys::generate();
        let owner = keys.public_key().to_bytes();
        crate::socialgraph::set_social_graph_root(&store, &owner);
        let follow = EventBuilder::new(Kind::ContactList, "")
            .custom_created_at(Timestamp::from(10))
            .sign_with_keys(&keys)
            .unwrap();
        let mute = EventBuilder::new(Kind::MuteList, "")
            .custom_created_at(Timestamp::from(11))
            .sign_with_keys(&keys)
            .unwrap();
        crate::socialgraph::ingest_parsed_event(&store, &follow).unwrap();
        // The relay lacks our local follow head: dry-run must never upload it.
        let relay = TestRelay::new(vec![mute.clone()]);
        let crawler = SocialGraphCrawler::new(store.clone(), keys, vec![relay.url()], 1)
            .with_negentropy(true);
        let client = crawler.connect_client().await.unwrap();
        client.wait_for_connection(Duration::from_secs(2)).await;
        // An old missing mute head must be fetched even with a recent cursor.
        let changed = crawler
            .fetch_graph_events_for_pubkeys(&client, &[owner], Some(Timestamp::from(100)))
            .await;
        assert_eq!(
            changed.iter().map(|event| event.id).collect::<Vec<_>>(),
            vec![mute.id]
        );
        crate::socialgraph::ingest_parsed_events(&store, &changed).unwrap();
        assert_eq!(store.list_revision_items(&[owner]).unwrap().len(), 2);
        let requests = relay.request_author_counts().len();
        assert!(crawler
            .fetch_graph_events_for_pubkeys(&client, &[owner], None)
            .await
            .is_empty());
        assert_eq!(
            relay.request_author_counts().len(),
            requests,
            "unchanged heads require no event download"
        );
        {
            let mut graph = store.graph.lock().unwrap();
            let mut state = graph.export_state().unwrap();
            state
                .list_revisions
                .iter_mut()
                .find(|(_, kind, _)| *kind == 10_000)
                .unwrap()
                .2
                .complete = false;
            graph.replace_state(&state).unwrap();
        }
        assert_eq!(store.list_revision_items(&[owner]).unwrap().len(), 1);
        let repaired = crawler
            .fetch_graph_events_for_pubkeys(&client, &[owner], None)
            .await;
        assert_eq!(
            repaired.iter().map(|event| event.id).collect::<Vec<_>>(),
            vec![mute.id]
        );
        let requests = relay.request_author_counts().len();
        relay.state.lock().unwrap().negentropy = NegentropyMode::Rejected;
        let failed = crawler
            .fetch_graph_events_for_pubkeys(&client, &[owner], None)
            .await;
        assert!(failed.is_empty());
        assert_eq!(relay.request_author_counts().len(), requests);
        assert_eq!(relay.state.lock().unwrap().published_events, 0);
        client.disconnect().await;
    }

    #[tokio::test]
    async fn negentropy_fetches_over_200_ids_in_bounded_chunks_and_configured_relay_order() {
        let _guard = crate::socialgraph::test_lock().await;
        let tmp = TempDir::new().unwrap();
        let store = crate::socialgraph::open_test_social_graph_store(tmp.path()).unwrap();
        let keys = nostr::Keys::generate();
        let mut authors = Vec::new();
        let mut events = Vec::new();
        for _ in 0..128 {
            let author = nostr::Keys::generate();
            authors.push(author.public_key().to_bytes());
            for kind in [Kind::ContactList, Kind::MuteList] {
                events.push(EventBuilder::new(kind, "").sign_with_keys(&author).unwrap());
            }
        }
        let preferred = TestRelay::new(events.clone());
        preferred.state.lock().unwrap().default_response_limit = Some(100);
        preferred.state.lock().unwrap().max_response_limit = Some(200);
        let second = TestRelay::new(Vec::new());
        let third = TestRelay::new(Vec::new());
        let crawler = SocialGraphCrawler::new(
            store,
            keys,
            vec![preferred.url(), second.url(), third.url()],
            1,
        )
        .with_negentropy(true);
        let client = crawler.connect_client().await.unwrap();
        client.wait_for_connection(Duration::from_secs(2)).await;
        let fetched = crawler
            .fetch_graph_events_for_pubkeys(&client, &authors, None)
            .await;
        assert_eq!(
            fetched.iter().map(|e| e.id).collect::<HashSet<_>>(),
            events.iter().map(|e| e.id).collect()
        );
        {
            let state = preferred.state.lock().unwrap();
            assert_eq!(state.neg_opens, 1);
            assert!(state
                .requested_filters
                .iter()
                .all(|f| f.authors.is_none() && f.kinds.is_none()));
            assert_eq!(state.requested_filters.len(), 4);
            assert!(state.requested_filters.iter().all(|f| f.limit == Some(64)));
            let requested = state
                .requested_filters
                .iter()
                .flat_map(|f| {
                    assert_eq!(f.ids.as_ref().unwrap().len(), 64);
                    f.ids.as_ref().unwrap().iter().copied()
                })
                .collect::<HashSet<_>>();
            assert_eq!(requested, events.iter().map(|e| e.id).collect());
            assert_eq!(state.published_events, 0);
        }
        assert_eq!(second.state.lock().unwrap().neg_opens, 1);
        assert_eq!(
            third.state.lock().unwrap().neg_opens,
            0,
            "configured priority must beat HashMap iteration"
        );
        crate::socialgraph::ingest_graph_parsed_events(crawler.graph_store.as_ref(), &fetched)
            .unwrap();
        assert!(crawler
            .fetch_graph_events_for_pubkeys(&client, &authors, None)
            .await
            .is_empty());
        assert_eq!(
            preferred.state.lock().unwrap().requested_filters.len(),
            4,
            "warm sync must not download event bodies"
        );
        client.disconnect().await;
    }

    #[tokio::test]
    async fn negentropy_incomplete_id_download_preserves_cache_without_fallback() {
        let _guard = crate::socialgraph::test_lock().await;
        let tmp = TempDir::new().unwrap();
        let store = crate::socialgraph::open_test_social_graph_store(tmp.path()).unwrap();
        let keys = nostr::Keys::generate();
        let events = [Kind::ContactList, Kind::MuteList]
            .into_iter()
            .map(|kind| EventBuilder::new(kind, "").sign_with_keys(&keys).unwrap())
            .collect::<Vec<_>>();
        let relay = TestRelay::new(events);
        relay.state.lock().unwrap().max_response_limit = Some(1);
        let crawler = SocialGraphCrawler::new(store, keys.clone(), vec![relay.url()], 1)
            .with_negentropy(true);
        let client = crawler.connect_client().await.unwrap();
        client.wait_for_connection(Duration::from_secs(2)).await;
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        crawler
            .fetch_contact_lists_for_pubkeys(
                &client,
                &[keys.public_key().to_bytes()],
                None,
                &shutdown_rx,
            )
            .await;
        assert!(crawler
            .graph_store
            .list_revision_items(&[keys.public_key().to_bytes()])
            .unwrap()
            .is_empty());
        let state = relay.state.lock().unwrap();
        assert_eq!(state.requested_filters.len(), 1);
        assert!(state.requested_filters[0].authors.is_none());
        assert_eq!(state.published_events, 0);
        drop(state);
        drop(shutdown_tx);
        client.disconnect().await;
    }

    #[tokio::test]
    async fn negentropy_shutdown_drops_inflight_batches_without_download_or_commit() {
        let _guard = crate::socialgraph::test_lock().await;
        let tmp = TempDir::new().unwrap();
        let store = crate::socialgraph::open_test_social_graph_store(tmp.path()).unwrap();
        let keys = nostr::Keys::generate();
        let author = keys.public_key().to_bytes();
        let relay = TestRelay::new(Vec::new());
        relay.state.lock().unwrap().negentropy = NegentropyMode::Silent;
        let crawler =
            SocialGraphCrawler::new(store, keys, vec![relay.url()], 1).with_negentropy(true);
        let client = crawler.connect_client().await.unwrap();
        client.wait_for_connection(Duration::from_secs(2)).await;
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let authors = [author];
        let fetch = crawler.fetch_contact_lists_for_pubkeys(&client, &authors, None, &shutdown_rx);
        let cancel = async {
            tokio::time::timeout(Duration::from_secs(2), async {
                while relay.state.lock().unwrap().neg_opens == 0 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            shutdown_tx.send(true).unwrap();
        };
        tokio::time::timeout(Duration::from_millis(500), async {
            tokio::join!(fetch, cancel);
        })
        .await
        .expect("shutdown must not wait for negotiation timeout");
        assert!(relay.state.lock().unwrap().requested_filters.is_empty());
        assert!(crawler
            .graph_store
            .list_revision_items(&[author])
            .unwrap()
            .is_empty());
        client.disconnect().await;
    }

    async fn assert_negentropy_failure_preserves_cache(mode: NegentropyMode) {
        let _guard = crate::socialgraph::test_lock().await;
        let tmp = TempDir::new().unwrap();
        let store = crate::socialgraph::open_test_social_graph_store(tmp.path()).unwrap();
        let keys = nostr::Keys::generate();
        let owner = keys.public_key().to_bytes();
        crate::socialgraph::set_social_graph_root(&store, &owner);
        let follow = EventBuilder::new(Kind::ContactList, "")
            .custom_created_at(Timestamp::from(10))
            .sign_with_keys(&keys)
            .unwrap();
        let mute = EventBuilder::new(Kind::MuteList, "")
            .custom_created_at(Timestamp::from(11))
            .sign_with_keys(&keys)
            .unwrap();
        crate::socialgraph::ingest_parsed_event(&store, &follow).unwrap();
        let before = store.graph.lock().unwrap().export_state().unwrap();
        let relay = TestRelay::new(vec![follow, mute]);
        relay.state.lock().unwrap().negentropy = mode;
        let crawler = SocialGraphCrawler::new(store.clone(), keys, vec![relay.url()], 1)
            .with_negentropy(true);
        let client = crawler.connect_client().await.unwrap();
        client.wait_for_connection(Duration::from_secs(2)).await;
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        crawler
            .fetch_contact_lists_for_pubkeys(&client, &[owner], None, &shutdown_rx)
            .await;
        assert_eq!(store.graph.lock().unwrap().export_state().unwrap(), before);
        {
            let state = relay.state.lock().unwrap();
            assert!(
                state.neg_opens > 0,
                "the relay must receive a real NEG-OPEN"
            );
            assert!(
                state.requested_filters.is_empty(),
                "failed NEG must not send REQ"
            );
            assert_eq!(state.published_events, 0);
        }
        client.disconnect().await;
    }

    #[tokio::test]
    async fn negentropy_rejection_skips_download_and_preserves_cache() {
        assert_negentropy_failure_preserves_cache(NegentropyMode::Rejected).await;
    }

    #[tokio::test]
    async fn negentropy_timeout_skips_download_and_preserves_cache() {
        assert_negentropy_failure_preserves_cache(NegentropyMode::Silent).await;
    }

    #[tokio::test]
    async fn negentropy_malformed_reply_skips_download_and_preserves_cache() {
        assert_negentropy_failure_preserves_cache(NegentropyMode::Malformed).await;
    }

    #[tokio::test]
    async fn negentropy_disconnect_skips_download_and_preserves_cache() {
        assert_negentropy_failure_preserves_cache(NegentropyMode::Disconnected).await;
    }

    #[tokio::test]
    async fn negentropy_mixed_relays_fetch_exact_ids_only_from_successful_relay() {
        let _guard = crate::socialgraph::test_lock().await;
        let tmp = TempDir::new().unwrap();
        let store = crate::socialgraph::open_test_social_graph_store(tmp.path()).unwrap();
        let keys = nostr::Keys::generate();
        let owner = keys.public_key().to_bytes();
        crate::socialgraph::set_social_graph_root(&store, &owner);
        let follow = EventBuilder::new(Kind::ContactList, "")
            .custom_created_at(Timestamp::from(10))
            .sign_with_keys(&keys)
            .unwrap();
        let mute = EventBuilder::new(Kind::MuteList, "")
            .custom_created_at(Timestamp::from(11))
            .sign_with_keys(&keys)
            .unwrap();
        crate::socialgraph::ingest_parsed_event(&store, &follow).unwrap();
        let healthy = TestRelay::new(vec![follow.clone(), mute.clone()]);
        let failed = TestRelay::new(vec![follow, mute.clone()]);
        failed.state.lock().unwrap().negentropy = NegentropyMode::Rejected;
        let crawler =
            SocialGraphCrawler::new(store.clone(), keys, vec![healthy.url(), failed.url()], 1)
                .with_negentropy(true);
        let client = crawler.connect_client().await.unwrap();
        client.wait_for_connection(Duration::from_secs(2)).await;
        let changed = crawler
            .fetch_graph_events_for_pubkeys(&client, &[owner], None)
            .await;
        assert_eq!(
            changed.iter().map(|event| event.id).collect::<Vec<_>>(),
            vec![mute.id]
        );
        crate::socialgraph::ingest_parsed_events(&store, &changed).unwrap();
        assert_eq!(store.list_revision_items(&[owner]).unwrap().len(), 2);
        {
            let state = healthy.state.lock().unwrap();
            assert!(state.neg_opens > 0);
            assert_eq!(state.requested_filters.len(), 1);
            let query = &state.requested_filters[0];
            assert!(query.authors.is_none() && query.kinds.is_none());
            assert_eq!(
                query
                    .ids
                    .as_ref()
                    .unwrap()
                    .iter()
                    .copied()
                    .collect::<Vec<_>>(),
                vec![mute.id]
            );
            assert_eq!(state.published_events, 0);
        }
        {
            let state = failed.state.lock().unwrap();
            assert!(state.neg_opens > 0);
            assert!(state.requested_filters.is_empty());
            assert_eq!(state.published_events, 0);
        }
        client.disconnect().await;
    }

    #[tokio::test]
    async fn negentropy_oversized_difference_skips_download_and_preserves_cache() {
        let _guard = crate::socialgraph::test_lock().await;
        let tmp = TempDir::new().unwrap();
        let store = crate::socialgraph::open_test_social_graph_store(tmp.path()).unwrap();
        let keys = nostr::Keys::generate();
        let owner = keys.public_key().to_bytes();
        crate::socialgraph::set_social_graph_root(&store, &owner);
        let before = store.graph.lock().unwrap().export_state().unwrap();
        // More historical revisions than the existing two-events-per-author budget.
        let events = (10..13)
            .map(|created_at| {
                EventBuilder::new(Kind::ContactList, "")
                    .custom_created_at(Timestamp::from(created_at))
                    .sign_with_keys(&keys)
                    .unwrap()
            })
            .collect();
        let relay = TestRelay::new(events);
        let crawler = SocialGraphCrawler::new(store.clone(), keys, vec![relay.url()], 1)
            .with_negentropy(true);
        let client = crawler.connect_client().await.unwrap();
        client.wait_for_connection(Duration::from_secs(2)).await;
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        crawler
            .fetch_contact_lists_for_pubkeys(&client, &[owner], None, &shutdown_rx)
            .await;
        assert_eq!(store.graph.lock().unwrap().export_state().unwrap(), before);
        {
            let state = relay.state.lock().unwrap();
            assert!(state.neg_opens > 0);
            assert!(state.requested_filters.is_empty());
            assert_eq!(state.published_events, 0);
        }
        client.disconnect().await;
    }

    #[tokio::test]
    async fn negentropy_disabled_preserves_explicit_author_crawl() {
        let _guard = crate::socialgraph::test_lock().await;
        let tmp = TempDir::new().unwrap();
        let store = crate::socialgraph::open_test_social_graph_store(tmp.path()).unwrap();
        let keys = nostr::Keys::generate();
        let owner = keys.public_key().to_bytes();
        let follow = EventBuilder::new(Kind::ContactList, "")
            .sign_with_keys(&keys)
            .unwrap();
        let relay = TestRelay::new(vec![follow.clone()]);
        relay.state.lock().unwrap().negentropy = NegentropyMode::Rejected;
        let crawler =
            SocialGraphCrawler::new(store, keys, vec![relay.url()], 1).with_negentropy(false);
        let client = crawler.connect_client().await.unwrap();
        client.wait_for_connection(Duration::from_secs(2)).await;
        let fetched = crawler
            .fetch_graph_events_for_pubkeys(&client, &[owner], None)
            .await;
        assert_eq!(
            fetched.iter().map(|event| event.id).collect::<Vec<_>>(),
            vec![follow.id]
        );
        {
            let state = relay.state.lock().unwrap();
            assert_eq!(state.neg_opens, 0);
            assert_eq!(state.request_author_counts, vec![1]);
            assert_eq!(state.published_events, 0);
        }
        client.disconnect().await;
    }

    #[tokio::test]
    async fn test_crawler_routes_untrusted_to_spambox() {
        let _guard = crate::socialgraph::test_lock().await;
        let tmp = TempDir::new().unwrap();
        let graph_store = crate::socialgraph::open_test_social_graph_store(tmp.path()).unwrap();
        let spambox = crate::socialgraph::open_test_social_graph_store_at_path(
            &tmp.path().join("spambox"),
            None,
        )
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
    async fn test_crawler_batches_graph_fetches_by_author_chunk() {
        let _guard = crate::socialgraph::test_lock().await;
        let tmp = TempDir::new().unwrap();
        let graph_store = crate::socialgraph::open_test_social_graph_store(tmp.path()).unwrap();

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
    async fn test_crawler_expands_from_existing_graph_without_root_refetch() {
        let _guard = crate::socialgraph::test_lock().await;
        let tmp = TempDir::new().unwrap();
        let graph_store = crate::socialgraph::open_test_social_graph_store(tmp.path()).unwrap();

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
    async fn test_crawler_full_recrawl_refetches_root() {
        let _guard = crate::socialgraph::test_lock().await;
        let tmp = TempDir::new().unwrap();
        let graph_store = crate::socialgraph::open_test_social_graph_store(tmp.path()).unwrap();

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
    async fn test_crawler_revisits_known_authors_with_since_cursor() {
        let _guard = crate::socialgraph::test_lock().await;
        let tmp = TempDir::new().unwrap();
        let graph_store = crate::socialgraph::open_test_social_graph_store(tmp.path()).unwrap();

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
    async fn test_crawler_warm_once_completes_initial_sync_without_shutdown() {
        let _guard = crate::socialgraph::test_lock().await;
        let tmp = TempDir::new().unwrap();
        let graph_store = crate::socialgraph::open_test_social_graph_store(tmp.path()).unwrap();

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
        let _guard = crate::socialgraph::test_lock().await;
        let tmp = TempDir::new().unwrap();
        let graph_store = crate::socialgraph::open_test_social_graph_store(tmp.path()).unwrap();

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
    async fn test_spawned_social_graph_tasks_sync_local_contacts_on_startup() {
        let _guard = crate::socialgraph::test_lock().await;
        let tmp = TempDir::new().unwrap();
        let graph_store = crate::socialgraph::open_test_social_graph_store(tmp.path()).unwrap();

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
    async fn test_spawned_social_graph_tasks_refresh_when_contacts_change() {
        let _guard = crate::socialgraph::test_lock().await;
        let tmp = TempDir::new().unwrap();
        let graph_store = crate::socialgraph::open_test_social_graph_store(tmp.path()).unwrap();

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
