use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use crate::{ListEventsOptions, NostrEventStore, NostrEventStoreError, StoredNostrEvent};
use futures::{stream, StreamExt};
use hashtree_core::{Cid, Store};
use nostr_sdk::{
    pool::RelayLimits, Client, EventId, Filter, Keys, Kind, NegentropyOptions, Options, PublicKey,
    Timestamp,
};
use nostr_social_graph::SocialGraphBackend;

const NEGENTROPY_FETCH_CHUNK_SIZE: usize = 256;
const METADATA_KIND: u32 = 0;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayFetchMode {
    AuthorBatches,
    GlobalRecent,
}

#[derive(Debug, Clone)]
pub struct CrawlConfig {
    pub relays: Vec<String>,
    pub author_allowlist: Option<Vec<String>>,
    pub max_live_bytes: Option<u64>,
    pub max_events_seen: Option<usize>,
    pub max_authors: Option<usize>,
    pub max_follow_distance: Option<u32>,
    pub author_batch_size: usize,
    pub per_author_event_limit: usize,
    pub per_author_live_bytes: Option<u64>,
    pub fetch_timeout: Duration,
    pub kinds: Option<Vec<u16>>,
    pub relay_fetch_mode: RelayFetchMode,
    pub require_negentropy: bool,
    pub relay_event_max_size: Option<u32>,
    pub relay_page_size: usize,
    pub max_relay_pages: usize,
    pub full_author_history: bool,
}

impl Default for CrawlConfig {
    fn default() -> Self {
        Self {
            relays: Vec::new(),
            author_allowlist: None,
            max_live_bytes: None,
            max_events_seen: None,
            max_authors: None,
            max_follow_distance: Some(1),
            author_batch_size: 64,
            per_author_event_limit: 256,
            per_author_live_bytes: None,
            fetch_timeout: Duration::from_secs(10),
            kinds: None,
            relay_fetch_mode: RelayFetchMode::AuthorBatches,
            require_negentropy: false,
            relay_event_max_size: None,
            relay_page_size: 1_000,
            max_relay_pages: 10,
            full_author_history: false,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CrawlReport {
    pub root: Option<Cid>,
    pub authors_considered: usize,
    pub authors_processed: usize,
    pub events_seen: usize,
    pub events_selected: usize,
    pub live_bytes_selected: u64,
}

pub trait EventSelectionPolicy: Send + Sync {
    fn priority(&self, event: &StoredNostrEvent) -> i32;
}

#[derive(Debug, Clone)]
pub struct KindPriorityPolicy {
    default_priority: i32,
    priorities: BTreeMap<u32, i32>,
}

impl Default for KindPriorityPolicy {
    fn default() -> Self {
        let mut priorities = BTreeMap::new();
        priorities.insert(1, 1_000);
        priorities.insert(0, 900);
        priorities.insert(3, 800);
        priorities.insert(10_000, 750);
        priorities.insert(6, 600);
        priorities.insert(7, 500);
        Self {
            default_priority: 100,
            priorities,
        }
    }
}

impl KindPriorityPolicy {
    pub fn with_priority(mut self, kind: u32, priority: i32) -> Self {
        self.priorities.insert(kind, priority);
        self
    }
}

impl EventSelectionPolicy for KindPriorityPolicy {
    fn priority(&self, event: &StoredNostrEvent) -> i32 {
        self.priorities
            .get(&event.kind)
            .copied()
            .unwrap_or(self.default_priority)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CrawlError {
    #[error("event store error: {0}")]
    EventStore(#[from] NostrEventStoreError),
    #[error("crawl requires at least one relay")]
    MissingRelays,
    #[error("per-author event limit must be greater than zero")]
    InvalidPerAuthorLimit,
    #[error("per-author live byte cap must be greater than zero")]
    InvalidPerAuthorLiveBytes,
    #[error("author batch size must be greater than zero")]
    InvalidAuthorBatchSize,
    #[error("relay page size must be greater than zero")]
    InvalidRelayPageSize,
    #[error("max relay pages must be greater than zero")]
    InvalidMaxRelayPages,
    #[error("max events seen must be greater than zero")]
    InvalidMaxEventsSeen,
    #[error("relay event max size must be greater than zero")]
    InvalidRelayEventMaxSize,
    #[error("nostr error: {0}")]
    Nostr(String),
    #[error("social graph error: {0}")]
    SocialGraph(String),
}

pub type Result<T> = std::result::Result<T, CrawlError>;

#[derive(Debug, Default)]
struct RelayFetchResult {
    events_seen: usize,
    events: Vec<StoredNostrEvent>,
    supports_negentropy: bool,
}

#[derive(Debug, Default)]
struct BatchCrawlReport {
    events_seen: usize,
    events_selected: usize,
    events: Vec<StoredNostrEvent>,
    live_bytes_selected: u64,
}

#[derive(Debug, Default)]
struct ProfileBatchReport {
    events_seen: usize,
    events_by_author: BTreeMap<String, Vec<StoredNostrEvent>>,
}

#[derive(Debug, Default)]
struct GlobalRecentState {
    current_root: Option<Cid>,
    retained_by_author: BTreeMap<String, Vec<StoredNostrEvent>>,
    events_selected: usize,
    live_bytes_selected: u64,
}

pub struct NostrBridge<S: Store> {
    event_store: NostrEventStore<S>,
    config: CrawlConfig,
    policy: Arc<dyn EventSelectionPolicy>,
}

impl<S: Store> NostrBridge<S> {
    pub fn new(store: Arc<S>, config: CrawlConfig) -> Self {
        Self {
            event_store: NostrEventStore::new(store),
            config,
            policy: Arc::new(KindPriorityPolicy::default()),
        }
    }

    pub fn with_policy(mut self, policy: Arc<dyn EventSelectionPolicy>) -> Self {
        self.policy = policy;
        self
    }

    pub async fn crawl<G: SocialGraphBackend>(
        &self,
        graph: &G,
        existing_root: Option<&Cid>,
    ) -> Result<CrawlReport> {
        self.crawl_with_progress(graph, existing_root, |_| {}).await
    }

    pub async fn crawl_with_progress<G, F>(
        &self,
        graph: &G,
        existing_root: Option<&Cid>,
        mut on_progress: F,
    ) -> Result<CrawlReport>
    where
        G: SocialGraphBackend,
        F: FnMut(&CrawlReport),
    {
        self.validate_config()?;

        let authors = self.collect_authors(graph)?;
        if authors.is_empty() {
            return Ok(CrawlReport::default());
        }

        let client = self.connect_client().await?;

        if self.config.relay_fetch_mode == RelayFetchMode::AuthorBatches {
            return self
                .crawl_author_batches(&client, &authors, existing_root, &mut on_progress)
                .await;
        }

        let state = self
            .load_existing_global_state(existing_root, &authors)
            .await?;

        let report = self
            .crawl_global_recent_incremental(&client, &authors, state, &mut on_progress)
            .await?;
        on_progress(&report);
        Ok(report)
    }

    async fn crawl_author_batches(
        &self,
        client: &Client,
        authors: &[String],
        existing_root: Option<&Cid>,
        on_progress: &mut impl FnMut(&CrawlReport),
    ) -> Result<CrawlReport> {
        let mut relay_negentropy_support = BTreeMap::<String, bool>::new();
        let mut failed_relays = BTreeSet::<String>::new();
        let mut current_root = existing_root.cloned();
        let mut events_seen = 0usize;
        let mut events_selected = 0usize;
        let mut live_bytes_selected = 0u64;
        let mut authors_processed = 0usize;

        for author_batch in authors.chunks(self.config.author_batch_size) {
            let batch = self
                .crawl_author_batch(
                    client,
                    author_batch,
                    current_root.as_ref(),
                    &mut relay_negentropy_support,
                    &mut failed_relays,
                    live_bytes_selected,
                )
                .await?;
            events_seen = events_seen.saturating_add(batch.events_seen);
            events_selected = events_selected.saturating_add(batch.events_selected);
            live_bytes_selected = batch.live_bytes_selected;
            authors_processed = authors_processed.saturating_add(author_batch.len());
            if !batch.events.is_empty() {
                current_root = self
                    .event_store
                    .build(current_root.as_ref(), batch.events)
                    .await?;
            }
            on_progress(&CrawlReport {
                root: current_root.clone(),
                authors_considered: authors.len(),
                authors_processed,
                events_seen,
                events_selected,
                live_bytes_selected,
            });
            if self.reached_events_seen_limit(events_seen) {
                break;
            }
        }

        Ok(CrawlReport {
            root: current_root,
            authors_considered: authors.len(),
            authors_processed,
            events_seen,
            events_selected,
            live_bytes_selected,
        })
    }

    async fn load_existing_global_state(
        &self,
        root: Option<&Cid>,
        authors: &[String],
    ) -> Result<GlobalRecentState> {
        let Some(root) = root else {
            return Ok(GlobalRecentState::default());
        };

        match self
            .event_store
            .list_recent(Some(root), ListEventsOptions::default())
            .await
        {
            Ok(events) => {
                let author_set = authors.iter().map(String::as_str).collect::<BTreeSet<_>>();
                let mut retained_by_author = BTreeMap::<String, Vec<StoredNostrEvent>>::new();
                for event in events {
                    if !author_set.contains(event.pubkey.as_str()) || !self.kind_allowed(event.kind)
                    {
                        continue;
                    }
                    if !self.is_valid_stored_event(&event) {
                        continue;
                    }
                    retained_by_author
                        .entry(event.pubkey.clone())
                        .or_default()
                        .push(event);
                }

                let mut state = GlobalRecentState {
                    current_root: Some(root.clone()),
                    ..GlobalRecentState::default()
                };
                for (author, events) in retained_by_author {
                    let selected = self.select_author_events(events)?;
                    state.events_selected = state.events_selected.saturating_add(selected.len());
                    state.live_bytes_selected = state
                        .live_bytes_selected
                        .saturating_add(self.encoded_events_size(&selected)?);
                    state.retained_by_author.insert(author, selected);
                }
                Ok(state)
            }
            Err(NostrEventStoreError::Validation(message))
                if message == "stored nostr event blob is missing" =>
            {
                eprintln!(
                    "Falling back to per-author resume for existing root due to missing event blobs"
                );
                let mut state = self
                    .load_existing_global_state_by_author(Some(root), authors)
                    .await?;
                state.current_root = self.rebuild_root_from_retained_state(&state).await?;
                Ok(state)
            }
            Err(err) => Err(err.into()),
        }
    }

    async fn load_existing_global_state_by_author(
        &self,
        root: Option<&Cid>,
        authors: &[String],
    ) -> Result<GlobalRecentState> {
        let mut state = GlobalRecentState {
            current_root: root.cloned(),
            ..GlobalRecentState::default()
        };
        for author in authors {
            let retained = self
                .load_retained_events(root, author)
                .await?
                .into_iter()
                .filter(|event| self.kind_allowed(event.kind))
                .filter(|event| self.is_valid_stored_event(event))
                .collect::<Vec<_>>();
            let retained = self.select_author_events(retained)?;
            state.events_selected = state.events_selected.saturating_add(retained.len());
            state.live_bytes_selected = state
                .live_bytes_selected
                .saturating_add(self.encoded_events_size(&retained)?);
            state.retained_by_author.insert(author.clone(), retained);
        }
        Ok(state)
    }

    async fn rebuild_root_from_retained_state(
        &self,
        state: &GlobalRecentState,
    ) -> Result<Option<Cid>> {
        let events = state
            .retained_by_author
            .values()
            .flat_map(|events| events.iter().cloned())
            .collect::<Vec<_>>();
        self.event_store
            .build(None, events)
            .await
            .map_err(Into::into)
    }

    fn validate_config(&self) -> Result<()> {
        if self.config.relays.is_empty() {
            return Err(CrawlError::MissingRelays);
        }
        if self.config.per_author_event_limit == 0 {
            return Err(CrawlError::InvalidPerAuthorLimit);
        }
        if self.config.per_author_live_bytes == Some(0) {
            return Err(CrawlError::InvalidPerAuthorLiveBytes);
        }
        if self.config.author_batch_size == 0 {
            return Err(CrawlError::InvalidAuthorBatchSize);
        }
        if self.config.relay_page_size == 0 {
            return Err(CrawlError::InvalidRelayPageSize);
        }
        if self.config.max_relay_pages == 0 && !self.config.full_author_history {
            return Err(CrawlError::InvalidMaxRelayPages);
        }
        if self.config.max_events_seen == Some(0) {
            return Err(CrawlError::InvalidMaxEventsSeen);
        }
        if self.config.relay_event_max_size == Some(0) {
            return Err(CrawlError::InvalidRelayEventMaxSize);
        }
        Ok(())
    }

    fn collect_authors<G: SocialGraphBackend>(&self, graph: &G) -> Result<Vec<String>> {
        if let Some(author_allowlist) = &self.config.author_allowlist {
            let mut seen = HashSet::new();
            let mut authors = Vec::new();
            for author in author_allowlist {
                if !is_valid_hex_pubkey(author) {
                    continue;
                }
                if seen.insert(author.clone()) {
                    authors.push(author.clone());
                }
            }
            if let Some(max_authors) = self.config.max_authors {
                authors.truncate(max_authors);
            }
            return Ok(authors);
        }

        let root = graph
            .get_root()
            .map_err(|err| CrawlError::SocialGraph(err.to_string()))?;
        let mut visited = BTreeSet::new();
        let mut authors = Vec::new();
        let mut queue = VecDeque::from([(root.clone(), 0u32)]);
        visited.insert(root);

        while let Some((author, distance)) = queue.pop_front() {
            if !is_valid_hex_pubkey(&author) {
                continue;
            }
            authors.push(author.clone());
            if self
                .config
                .max_authors
                .is_some_and(|max_authors| authors.len() >= max_authors)
            {
                break;
            }
            if self
                .config
                .max_follow_distance
                .is_some_and(|max_distance| distance >= max_distance)
            {
                continue;
            }

            let mut follows = graph
                .get_followed_by_user(&author)
                .map_err(|err| CrawlError::SocialGraph(err.to_string()))?;
            follows.retain(|followed| is_valid_hex_pubkey(followed));
            follows.sort();
            for followed in follows {
                if visited.insert(followed.clone()) {
                    queue.push_back((followed, distance.saturating_add(1)));
                }
            }
        }

        Ok(authors)
    }

    async fn connect_client(&self) -> Result<Client> {
        let client = if let Some(max_size) = self.config.relay_event_max_size {
            let mut limits = RelayLimits::default();
            limits.events.max_size = Some(max_size);
            Client::with_opts(Keys::generate(), Options::new().relay_limits(limits))
        } else {
            Client::new(Keys::generate())
        };
        for relay in &self.config.relays {
            client
                .add_relay(relay)
                .await
                .map_err(|err| CrawlError::Nostr(err.to_string()))?;
        }
        client.connect().await;
        tokio::time::sleep(Duration::from_millis(250)).await;
        Ok(client)
    }

    async fn crawl_author_batch(
        &self,
        client: &Client,
        author_batch: &[String],
        current_root: Option<&Cid>,
        relay_negentropy_support: &mut BTreeMap<String, bool>,
        failed_relays: &mut BTreeSet<String>,
        live_bytes_selected_so_far: u64,
    ) -> Result<BatchCrawlReport> {
        let mut existing_by_author = BTreeMap::<String, Vec<StoredNostrEvent>>::new();
        let mut known = BTreeMap::<String, StoredNostrEvent>::new();
        for author in author_batch {
            let retained = self
                .load_retained_events(current_root, author)
                .await?
                .into_iter()
                .filter(|event| self.kind_allowed(event.kind))
                .filter(|event| self.is_valid_stored_event(event))
                .collect::<Vec<_>>();
            for event in &retained {
                known.insert(event.id.clone(), event.clone());
            }
            existing_by_author.insert(author.clone(), retained);
        }

        let pubkeys: Vec<PublicKey> = author_batch
            .iter()
            .filter_map(|author| author.parse::<PublicKey>().ok())
            .collect();
        if pubkeys.is_empty() {
            return Ok(BatchCrawlReport {
                events_seen: 0,
                events_selected: 0,
                events: Vec::new(),
                live_bytes_selected: live_bytes_selected_so_far,
            });
        }

        let initial_known_ids = known.keys().cloned().collect::<BTreeSet<_>>();
        if self.config.full_author_history {
            return self
                .crawl_full_author_history_batch(
                    client,
                    author_batch,
                    existing_by_author,
                    known,
                    initial_known_ids,
                    relay_negentropy_support,
                    failed_relays,
                    live_bytes_selected_so_far,
                )
                .await;
        }

        let filter = self.batch_filter(pubkeys);
        let mut fetched = BTreeMap::<String, StoredNostrEvent>::new();
        let mut events_seen = 0usize;

        for relay in &self.config.relays {
            if failed_relays.contains(relay) {
                continue;
            }
            let local_items = self.local_items_for_batch(known.values(), author_batch);
            let relay_support = relay_negentropy_support.get(relay).copied();
            let fetched_from_relay = match self
                .fetch_events_from_relay(client, relay, filter.clone(), local_items, relay_support)
                .await
            {
                Ok(result) => result,
                Err(err) => {
                    eprintln!("Skipping relay {relay}: {err}");
                    failed_relays.insert(relay.clone());
                    continue;
                }
            };
            relay_negentropy_support.insert(relay.clone(), fetched_from_relay.supports_negentropy);
            events_seen = events_seen.saturating_add(fetched_from_relay.events_seen);
            for event in fetched_from_relay.events {
                if self.kind_allowed(event.kind)
                    && known.insert(event.id.clone(), event.clone()).is_none()
                {
                    fetched.insert(event.id.clone(), event);
                }
            }
        }

        let mut fetched_by_author = BTreeMap::<String, Vec<StoredNostrEvent>>::new();
        for event in fetched.into_values() {
            fetched_by_author
                .entry(event.pubkey.clone())
                .or_default()
                .push(event);
        }

        let mut selected = Vec::new();
        for author in author_batch {
            let mut merged: BTreeMap<String, StoredNostrEvent> = BTreeMap::new();
            if let Some(existing_events) = existing_by_author.remove(author) {
                for event in existing_events {
                    merged.insert(event.id.clone(), event);
                }
            }
            if let Some(events) = fetched_by_author.remove(author) {
                for event in events {
                    merged.insert(event.id.clone(), event);
                }
            }
            selected.extend(self.select_author_events(merged.into_values().collect())?);
        }

        let selected = selected
            .into_iter()
            .filter(|event| self.is_valid_stored_event(event))
            .collect::<Vec<_>>();
        let (selected, live_bytes_selected) =
            self.apply_live_byte_cap_from(selected, live_bytes_selected_so_far)?;
        let events_selected = selected.len();
        let events_to_apply = selected
            .into_iter()
            .filter(|event| !initial_known_ids.contains(&event.id))
            .collect::<Vec<_>>();

        Ok(BatchCrawlReport {
            events_seen,
            events_selected,
            events: events_to_apply,
            live_bytes_selected,
        })
    }

    async fn crawl_full_author_history_batch(
        &self,
        client: &Client,
        author_batch: &[String],
        mut existing_by_author: BTreeMap<String, Vec<StoredNostrEvent>>,
        mut known: BTreeMap<String, StoredNostrEvent>,
        initial_known_ids: BTreeSet<String>,
        relay_negentropy_support: &mut BTreeMap<String, bool>,
        failed_relays: &mut BTreeSet<String>,
        live_bytes_selected_so_far: u64,
    ) -> Result<BatchCrawlReport> {
        let mut events_seen = 0usize;
        let mut selected = Vec::new();
        let author_set = author_batch
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        let pubkeys = author_batch
            .iter()
            .filter_map(|author| author.parse::<PublicKey>().ok())
            .collect::<Vec<_>>();

        if pubkeys.is_empty() {
            return Ok(BatchCrawlReport {
                events_seen: 0,
                events_selected: 0,
                events: Vec::new(),
                live_bytes_selected: live_bytes_selected_so_far,
            });
        }

        let local_items = self.local_items_for_batch(known.values(), author_batch);
        let mut fetched_by_author = BTreeMap::<String, Vec<StoredNostrEvent>>::new();

        let relays_to_fetch = self
            .config
            .relays
            .iter()
            .filter(|relay| !failed_relays.contains(*relay))
            .map(|relay| (relay.clone(), relay_negentropy_support.get(relay).copied()))
            .collect::<Vec<_>>();
        let relay_fetches = relays_to_fetch.into_iter().map(|(relay, relay_support)| {
            let local_items = local_items.clone();
            let pubkeys = &pubkeys;
            async move {
                let result = self
                    .fetch_full_history_from_relay(
                        client,
                        &relay,
                        pubkeys,
                        local_items,
                        relay_support,
                    )
                    .await;
                (relay, result)
            }
        });
        let mut relay_fetches =
            stream::iter(relay_fetches).buffer_unordered(self.config.relays.len().max(1));

        while let Some((relay, result)) = relay_fetches.next().await {
            match result {
                Ok(fetched_from_relay) => {
                    relay_negentropy_support
                        .insert(relay.clone(), fetched_from_relay.supports_negentropy);
                    events_seen = events_seen.saturating_add(fetched_from_relay.events_seen);
                    for event in fetched_from_relay.events {
                        if self.kind_allowed(event.kind)
                            && author_set.contains(event.pubkey.as_str())
                            && known.insert(event.id.clone(), event.clone()).is_none()
                        {
                            fetched_by_author
                                .entry(event.pubkey.clone())
                                .or_default()
                                .push(event);
                        }
                    }
                }
                Err(err) => {
                    eprintln!("Skipping relay {relay}: {err}");
                    failed_relays.insert(relay.clone());
                }
            }
        }

        for author in author_batch {
            let mut merged = BTreeMap::<String, StoredNostrEvent>::new();
            if let Some(existing_events) = existing_by_author.remove(author) {
                for event in existing_events {
                    merged.insert(event.id.clone(), event);
                }
            }
            if let Some(events) = fetched_by_author.remove(author) {
                for event in events {
                    merged.insert(event.id.clone(), event);
                }
            }
            let author_selected = self
                .select_author_events_with_limits(
                    merged.into_values().collect(),
                    usize::MAX,
                    self.config.per_author_live_bytes,
                )?
                .into_iter()
                .filter(|event| self.is_valid_stored_event(event))
                .collect::<Vec<_>>();
            selected.extend(author_selected);
        }

        let (events, live_bytes_selected) =
            self.apply_live_byte_cap_from(selected, live_bytes_selected_so_far)?;
        let events_selected = events.len();
        let events_to_apply = events
            .into_iter()
            .filter(|event| !initial_known_ids.contains(&event.id))
            .collect::<Vec<_>>();
        Ok(BatchCrawlReport {
            events_seen,
            events_selected,
            events: events_to_apply,
            live_bytes_selected,
        })
    }

    async fn crawl_global_recent_incremental(
        &self,
        client: &Client,
        authors: &[String],
        mut state: GlobalRecentState,
        on_progress: &mut impl FnMut(&CrawlReport),
    ) -> Result<CrawlReport> {
        let author_set = authors.iter().map(String::as_str).collect::<BTreeSet<_>>();
        let mut known_ids = state
            .retained_by_author
            .values()
            .flat_map(|events| events.iter().map(|event| event.id.clone()))
            .collect::<BTreeSet<_>>();
        let mut authors_processed = state
            .retained_by_author
            .values()
            .filter(|events| !events.is_empty())
            .count();
        let mut failed_relays = BTreeSet::<String>::new();
        let mut relay_negentropy_support = BTreeMap::<String, bool>::new();
        let mut events_seen = 0usize;

        self.hydrate_global_recent_profiles(
            client,
            authors,
            &mut state,
            &mut known_ids,
            &mut authors_processed,
            &mut relay_negentropy_support,
            &mut failed_relays,
            &mut events_seen,
            on_progress,
        )
        .await?;
        if self.reached_events_seen_limit(events_seen) {
            return Ok(CrawlReport {
                root: state.current_root,
                authors_considered: authors.len(),
                authors_processed,
                events_seen,
                events_selected: state.events_selected,
                live_bytes_selected: state.live_bytes_selected,
            });
        }

        for relay in &self.config.relays {
            if failed_relays.contains(relay) {
                continue;
            }
            let mut until = None;
            for _ in 0..self.config.max_relay_pages {
                let filter = self.global_recent_filter(until);
                let events = match client
                    .get_events_from([relay], vec![filter], Some(self.config.fetch_timeout))
                    .await
                {
                    Ok(events) => events,
                    Err(err) => {
                        eprintln!("Skipping relay {relay}: {}", err);
                        failed_relays.insert(relay.clone());
                        break;
                    }
                };
                let fetched_count = events.len();
                events_seen = events_seen.saturating_add(fetched_count);
                if fetched_count == 0 {
                    break;
                }

                let mut min_created_at = u64::MAX;
                let mut incoming_by_author = BTreeMap::<String, Vec<StoredNostrEvent>>::new();
                for event in events {
                    min_created_at = min_created_at.min(event.created_at.as_u64());
                    if event.kind.is_ephemeral() {
                        continue;
                    }

                    let stored = stored_event_from_nostr(&event);
                    if !author_set.contains(stored.pubkey.as_str())
                        || !self.kind_allowed(stored.kind)
                    {
                        continue;
                    }
                    incoming_by_author
                        .entry(stored.pubkey.clone())
                        .or_default()
                        .push(stored);
                }

                let pending_apply = self.merge_author_events_into_state(
                    &mut state,
                    incoming_by_author,
                    &mut known_ids,
                    &mut authors_processed,
                )?;
                if !pending_apply.is_empty() {
                    state.current_root = self
                        .event_store
                        .build(state.current_root.as_ref(), pending_apply)
                        .await?;
                }

                on_progress(&CrawlReport {
                    root: state.current_root.clone(),
                    authors_considered: authors.len(),
                    authors_processed,
                    events_seen,
                    events_selected: state.events_selected,
                    live_bytes_selected: state.live_bytes_selected,
                });

                if min_created_at == u64::MAX || min_created_at == 0 {
                    break;
                }
                let next_until = min_created_at.saturating_sub(1);
                if until == Some(next_until) {
                    break;
                }
                until = Some(next_until);
                if self.reached_events_seen_limit(events_seen) {
                    break;
                }
            }
            if self.reached_events_seen_limit(events_seen) {
                break;
            }
        }

        Ok(CrawlReport {
            root: state.current_root,
            authors_considered: authors.len(),
            authors_processed,
            events_seen,
            events_selected: state.events_selected,
            live_bytes_selected: state.live_bytes_selected,
        })
    }

    fn batch_filter(&self, pubkeys: Vec<PublicKey>) -> Filter {
        let mut filter = Filter::new().authors(pubkeys);
        if let Some(kinds) = &self.config.kinds {
            filter = filter.kinds(kinds.iter().copied().map(Kind::from));
        }
        let mut relay_limit = self
            .config
            .author_batch_size
            .saturating_mul(self.config.per_author_event_limit);
        let relay_page_budget = self
            .config
            .relay_page_size
            .saturating_mul(self.config.max_relay_pages.max(1));
        if relay_page_budget > 0 {
            relay_limit = relay_limit.min(relay_page_budget);
        }
        if relay_limit > 0 {
            filter = filter.limit(relay_limit);
        }
        filter
    }

    fn full_history_negentropy_filter(&self, pubkeys: Vec<PublicKey>) -> Filter {
        let mut filter = Filter::new().authors(pubkeys);
        if let Some(kinds) = &self.config.kinds {
            filter = filter.kinds(kinds.iter().copied().map(Kind::from));
        }
        filter
    }

    fn global_recent_filter(&self, until: Option<u64>) -> Filter {
        let mut filter = Filter::new().limit(self.config.relay_page_size);
        if let Some(kinds) = &self.config.kinds {
            filter = filter.kinds(kinds.iter().copied().map(Kind::from));
        }
        if let Some(until) = until {
            filter = filter.until(Timestamp::from_secs(until));
        }
        filter
    }

    fn reached_events_seen_limit(&self, events_seen: usize) -> bool {
        self.config
            .max_events_seen
            .is_some_and(|limit| events_seen >= limit)
    }

    fn is_valid_stored_event(&self, event: &StoredNostrEvent) -> bool {
        self.event_store.encode_event(event).is_ok()
    }

    fn encoded_events_size(&self, events: &[StoredNostrEvent]) -> Result<u64> {
        let mut total = 0u64;
        for event in events {
            total = total.saturating_add(self.event_store.encode_event(event)?.len() as u64);
        }
        Ok(total)
    }

    fn local_items_for_batch<'a, I>(
        &self,
        known_events: I,
        author_batch: &[String],
    ) -> Vec<(EventId, Timestamp)>
    where
        I: Iterator<Item = &'a StoredNostrEvent>,
    {
        let authors = author_batch
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();

        known_events
            .filter(|event| {
                authors.contains(event.pubkey.as_str()) && self.kind_allowed(event.kind)
            })
            .filter_map(|event| {
                let event_id = EventId::parse(&event.id).ok()?;
                Some((event_id, Timestamp::from_secs(event.created_at)))
            })
            .collect()
    }

    fn local_items_for_batch_by_kind<'a, I>(
        &self,
        known_events: I,
        author_batch: &[String],
        kind: u32,
    ) -> Vec<(EventId, Timestamp)>
    where
        I: Iterator<Item = &'a StoredNostrEvent>,
    {
        let authors = author_batch
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();

        known_events
            .filter(|event| {
                authors.contains(event.pubkey.as_str())
                    && event.kind == kind
                    && self.kind_allowed(event.kind)
            })
            .filter_map(|event| {
                let event_id = EventId::parse(&event.id).ok()?;
                Some((event_id, Timestamp::from_secs(event.created_at)))
            })
            .collect()
    }

    async fn load_retained_events(
        &self,
        root: Option<&Cid>,
        author: &str,
    ) -> Result<Vec<StoredNostrEvent>> {
        self.event_store
            .list_by_author_lossy(root, author, ListEventsOptions::default())
            .await
            .map_err(Into::into)
    }

    fn merge_author_events_into_state(
        &self,
        state: &mut GlobalRecentState,
        incoming_by_author: BTreeMap<String, Vec<StoredNostrEvent>>,
        known_ids: &mut BTreeSet<String>,
        authors_processed: &mut usize,
    ) -> Result<Vec<StoredNostrEvent>> {
        let mut pending_apply = Vec::new();

        for (author, incoming) in incoming_by_author {
            let retained = state.retained_by_author.entry(author).or_default();
            let was_empty = retained.is_empty();
            let old_len = retained.len();
            let old_live_bytes = self.encoded_events_size(retained)?;

            let mut merged = BTreeMap::<String, StoredNostrEvent>::new();
            for existing in retained.drain(..) {
                merged.insert(existing.id.clone(), existing);
            }
            for event in incoming {
                merged.insert(event.id.clone(), event);
            }

            let selected = self.select_author_events(merged.into_values().collect())?;
            let selected_live_bytes = self.encoded_events_size(&selected)?;

            for selected_event in &selected {
                if known_ids.insert(selected_event.id.clone()) {
                    pending_apply.push(selected_event.clone());
                }
            }

            state.events_selected = state
                .events_selected
                .saturating_sub(old_len)
                .saturating_add(selected.len());
            state.live_bytes_selected = state
                .live_bytes_selected
                .saturating_sub(old_live_bytes)
                .saturating_add(selected_live_bytes);
            if was_empty && !selected.is_empty() {
                *authors_processed = authors_processed.saturating_add(1);
            }
            *retained = selected;
        }

        Ok(pending_apply)
    }

    async fn hydrate_global_recent_profiles(
        &self,
        client: &Client,
        authors: &[String],
        state: &mut GlobalRecentState,
        known_ids: &mut BTreeSet<String>,
        authors_processed: &mut usize,
        relay_negentropy_support: &mut BTreeMap<String, bool>,
        failed_relays: &mut BTreeSet<String>,
        events_seen: &mut usize,
        on_progress: &mut impl FnMut(&CrawlReport),
    ) -> Result<()> {
        if !self.kind_allowed(METADATA_KIND) {
            return Ok(());
        }

        let authors_missing_profiles = authors
            .iter()
            .filter(|author| {
                !state
                    .retained_by_author
                    .get(*author)
                    .is_some_and(|events| events.iter().any(|event| event.kind == METADATA_KIND))
            })
            .cloned()
            .collect::<Vec<_>>();
        if authors_missing_profiles.is_empty() {
            return Ok(());
        }

        for author_batch in authors_missing_profiles.chunks(self.config.author_batch_size.max(1)) {
            let batch = self
                .crawl_profile_batch(
                    client,
                    author_batch,
                    state
                        .retained_by_author
                        .values()
                        .flat_map(|events| events.iter()),
                    relay_negentropy_support,
                    failed_relays,
                )
                .await?;

            *events_seen = events_seen.saturating_add(batch.events_seen);
            let pending_apply = self.merge_author_events_into_state(
                state,
                batch.events_by_author,
                known_ids,
                authors_processed,
            )?;
            if !pending_apply.is_empty() {
                state.current_root = self
                    .event_store
                    .build(state.current_root.as_ref(), pending_apply)
                    .await?;
            }

            on_progress(&CrawlReport {
                root: state.current_root.clone(),
                authors_considered: authors.len(),
                authors_processed: *authors_processed,
                events_seen: *events_seen,
                events_selected: state.events_selected,
                live_bytes_selected: state.live_bytes_selected,
            });

            if self.reached_events_seen_limit(*events_seen) {
                break;
            }
        }

        Ok(())
    }

    async fn crawl_profile_batch<'a, I>(
        &self,
        client: &Client,
        author_batch: &[String],
        known_events: I,
        relay_negentropy_support: &mut BTreeMap<String, bool>,
        failed_relays: &mut BTreeSet<String>,
    ) -> Result<ProfileBatchReport>
    where
        I: Iterator<Item = &'a StoredNostrEvent>,
    {
        let pubkeys: Vec<PublicKey> = author_batch
            .iter()
            .filter_map(|author| author.parse::<PublicKey>().ok())
            .collect();
        if pubkeys.is_empty() {
            return Ok(ProfileBatchReport::default());
        }

        let filter = Filter::new()
            .authors(pubkeys)
            .kind(Kind::Metadata)
            .limit(author_batch.len().saturating_mul(2).max(1));
        let local_items =
            self.local_items_for_batch_by_kind(known_events, author_batch, METADATA_KIND);
        let mut fetched_by_author = BTreeMap::<String, Vec<StoredNostrEvent>>::new();
        let mut events_seen = 0usize;

        for relay in &self.config.relays {
            if failed_relays.contains(relay) {
                continue;
            }
            let relay_support = relay_negentropy_support.get(relay).copied();
            let fetched_from_relay = match self
                .fetch_events_from_relay(
                    client,
                    relay,
                    filter.clone(),
                    local_items.clone(),
                    relay_support,
                )
                .await
            {
                Ok(result) => result,
                Err(err) => {
                    eprintln!("Skipping relay {relay}: {err}");
                    failed_relays.insert(relay.clone());
                    continue;
                }
            };
            relay_negentropy_support.insert(relay.clone(), fetched_from_relay.supports_negentropy);
            events_seen = events_seen.saturating_add(fetched_from_relay.events_seen);
            for event in fetched_from_relay.events {
                if event.kind == METADATA_KIND && self.kind_allowed(event.kind) {
                    fetched_by_author
                        .entry(event.pubkey.clone())
                        .or_default()
                        .push(event);
                }
            }
        }

        Ok(ProfileBatchReport {
            events_seen,
            events_by_author: fetched_by_author,
        })
    }

    async fn fetch_events_from_relay(
        &self,
        client: &Client,
        relay: &str,
        filter: Filter,
        local_items: Vec<(EventId, Timestamp)>,
        supports_negentropy: Option<bool>,
    ) -> Result<RelayFetchResult> {
        if supports_negentropy == Some(false) {
            if self.config.require_negentropy {
                return Ok(RelayFetchResult {
                    events_seen: 0,
                    events: Vec::new(),
                    supports_negentropy: false,
                });
            }
            return self
                .fetch_full_filter(client, relay, filter)
                .await
                .map(|events| RelayFetchResult {
                    events_seen: events.len(),
                    events,
                    supports_negentropy: false,
                });
        }

        match client
            .reconcile_advanced(
                [relay],
                filter.clone(),
                local_items,
                NegentropyOptions::default().dry_run(),
            )
            .await
        {
            Ok(output) if !output.success.is_empty() => {
                let missing = output.remote.iter().cloned().collect::<Vec<_>>();
                self.fetch_missing_ids(client, relay, missing).await.map(
                    |RelayFetchResult {
                         events_seen,
                         events,
                         ..
                     }| RelayFetchResult {
                        events_seen,
                        events,
                        supports_negentropy: true,
                    },
                )
            }
            Ok(_) | Err(_) => {
                if self.config.require_negentropy {
                    Ok(RelayFetchResult {
                        events_seen: 0,
                        events: Vec::new(),
                        supports_negentropy: false,
                    })
                } else {
                    self.fetch_full_filter(client, relay, filter)
                        .await
                        .map(|events| RelayFetchResult {
                            events_seen: events.len(),
                            events,
                            supports_negentropy: false,
                        })
                }
            }
        }
    }

    async fn fetch_missing_ids(
        &self,
        client: &Client,
        relay: &str,
        missing_ids: Vec<EventId>,
    ) -> Result<RelayFetchResult> {
        if missing_ids.is_empty() {
            return Ok(RelayFetchResult {
                events_seen: 0,
                events: Vec::new(),
                supports_negentropy: true,
            });
        }

        let mut out = BTreeMap::<String, StoredNostrEvent>::new();
        let mut events_seen = 0usize;
        for chunk in missing_ids.chunks(NEGENTROPY_FETCH_CHUNK_SIZE) {
            let filter = Filter::new().ids(chunk.iter().cloned());
            let events = client
                .get_events_from([relay], vec![filter], Some(self.config.fetch_timeout))
                .await
                .map_err(|err| CrawlError::Nostr(err.to_string()))?;
            events_seen = events_seen.saturating_add(events.len());
            for event in events {
                if event.kind.is_ephemeral() {
                    continue;
                }
                let stored = stored_event_from_nostr(&event);
                out.insert(stored.id.clone(), stored);
            }
        }
        Ok(RelayFetchResult {
            events_seen,
            events: out.into_values().collect(),
            supports_negentropy: true,
        })
    }

    async fn fetch_full_filter(
        &self,
        client: &Client,
        relay: &str,
        filter: Filter,
    ) -> Result<Vec<StoredNostrEvent>> {
        let mut out = Vec::new();
        let events = client
            .get_events_from([relay], vec![filter], Some(self.config.fetch_timeout))
            .await
            .map_err(|err| CrawlError::Nostr(err.to_string()))?;

        for event in events {
            if event.kind.is_ephemeral() {
                continue;
            }
            out.push(stored_event_from_nostr(&event));
        }

        Ok(out)
    }

    async fn fetch_full_history_from_relay(
        &self,
        client: &Client,
        relay: &str,
        pubkeys: &[PublicKey],
        local_items: Vec<(EventId, Timestamp)>,
        supports_negentropy: Option<bool>,
    ) -> Result<RelayFetchResult> {
        if supports_negentropy != Some(false) {
            let filter = self.full_history_negentropy_filter(pubkeys.to_vec());
            match client
                .reconcile_advanced(
                    [relay],
                    filter,
                    local_items,
                    NegentropyOptions::default().dry_run(),
                )
                .await
            {
                Ok(output) if !output.success.is_empty() => {
                    let missing = output.remote.iter().cloned().collect::<Vec<_>>();
                    return match self.fetch_missing_ids(client, relay, missing).await {
                        Ok(RelayFetchResult {
                            events_seen,
                            events,
                            ..
                        }) => Ok(RelayFetchResult {
                            events_seen,
                            events,
                            supports_negentropy: true,
                        }),
                        Err(err)
                            if !self.config.require_negentropy
                                && self.config.max_relay_pages > 0 =>
                        {
                            self.fetch_full_history_by_paging_from_relay(client, relay, pubkeys)
                                .await
                                .map_err(|_| err)
                        }
                        Err(err) => Err(err),
                    };
                }
                Ok(_) | Err(_) if self.config.require_negentropy => {
                    return Ok(RelayFetchResult {
                        events_seen: 0,
                        events: Vec::new(),
                        supports_negentropy: false,
                    });
                }
                Ok(_) | Err(_) => {}
            }
        }

        if self.config.require_negentropy || self.config.max_relay_pages == 0 {
            return Ok(RelayFetchResult {
                events_seen: 0,
                events: Vec::new(),
                supports_negentropy: false,
            });
        }
        self.fetch_full_history_by_paging_from_relay(client, relay, pubkeys)
            .await
    }

    async fn fetch_full_history_by_paging_from_relay(
        &self,
        client: &Client,
        relay: &str,
        pubkeys: &[PublicKey],
    ) -> Result<RelayFetchResult> {
        let mut out = BTreeMap::<String, StoredNostrEvent>::new();
        let mut events_seen = 0usize;

        for pubkey in pubkeys {
            let fetched = self
                .fetch_full_author_history_by_paging_from_relay(client, relay, *pubkey)
                .await?;
            events_seen = events_seen.saturating_add(fetched.events_seen);
            for event in fetched.events {
                out.insert(event.id.clone(), event);
            }
            if self.reached_events_seen_limit(events_seen) {
                break;
            }
        }

        Ok(RelayFetchResult {
            events_seen,
            events: out.into_values().collect(),
            supports_negentropy: false,
        })
    }

    async fn fetch_full_author_history_by_paging_from_relay(
        &self,
        client: &Client,
        relay: &str,
        pubkey: PublicKey,
    ) -> Result<RelayFetchResult> {
        let mut out = BTreeMap::<String, StoredNostrEvent>::new();
        let mut events_seen = 0usize;
        let mut until = None;

        for _ in 0..self.config.max_relay_pages {
            let mut filter = Filter::new()
                .author(pubkey)
                .limit(self.config.relay_page_size);
            if let Some(kinds) = &self.config.kinds {
                filter = filter.kinds(kinds.iter().copied().map(Kind::from));
            }
            if let Some(until) = until {
                filter = filter.until(Timestamp::from_secs(until));
            }

            let events = client
                .get_events_from([relay], vec![filter], Some(self.config.fetch_timeout))
                .await
                .map_err(|err| CrawlError::Nostr(err.to_string()))?;
            let fetched_count = events.len();
            events_seen = events_seen.saturating_add(fetched_count);
            if fetched_count == 0 {
                break;
            }

            let mut min_created_at = u64::MAX;
            for event in events {
                min_created_at = min_created_at.min(event.created_at.as_u64());
                if event.kind.is_ephemeral() {
                    continue;
                }
                let stored = stored_event_from_nostr(&event);
                out.insert(stored.id.clone(), stored);
            }

            if min_created_at == u64::MAX || min_created_at == 0 {
                break;
            }
            let next_until = min_created_at.saturating_sub(1);
            if until == Some(next_until) {
                break;
            }
            until = Some(next_until);
            if self.reached_events_seen_limit(events_seen) {
                break;
            }
        }

        Ok(RelayFetchResult {
            events_seen,
            events: out.into_values().collect(),
            supports_negentropy: false,
        })
    }

    fn select_author_events(&self, events: Vec<StoredNostrEvent>) -> Result<Vec<StoredNostrEvent>> {
        self.select_author_events_with_limits(
            events,
            self.config.per_author_event_limit,
            self.config.per_author_live_bytes,
        )
    }

    fn select_author_events_with_limits(
        &self,
        mut events: Vec<StoredNostrEvent>,
        event_limit: usize,
        live_byte_limit: Option<u64>,
    ) -> Result<Vec<StoredNostrEvent>> {
        let sticky_events = self.select_sticky_author_events(&events);
        let sticky_ids = sticky_events
            .iter()
            .map(|event| event.id.clone())
            .collect::<HashSet<_>>();
        events.retain(|event| !sticky_ids.contains(&event.id));
        events.sort_by(|left, right| {
            self.policy
                .priority(right)
                .cmp(&self.policy.priority(left))
                .then_with(|| right.created_at.cmp(&left.created_at))
                .then_with(|| left.id.cmp(&right.id))
        });

        if let Some(max_live_bytes) = live_byte_limit {
            let mut selected = sticky_events.clone();
            let mut live_bytes_selected = self.encoded_events_size(&selected)?;
            if live_bytes_selected > max_live_bytes {
                return Ok(selected);
            }
            for event in events {
                let encoded_len = self.event_store.encode_event(&event)?.len() as u64;
                if live_bytes_selected.saturating_add(encoded_len) > max_live_bytes {
                    continue;
                }
                live_bytes_selected = live_bytes_selected.saturating_add(encoded_len);
                selected.push(event);
                if selected.len().saturating_sub(sticky_events.len()) >= event_limit {
                    break;
                }
            }
            return Ok(selected);
        }

        let mut selected = sticky_events;
        selected.extend(events.into_iter().take(event_limit));
        Ok(selected)
    }

    fn select_sticky_author_events(&self, events: &[StoredNostrEvent]) -> Vec<StoredNostrEvent> {
        let latest_metadata = events
            .iter()
            .filter(|event| event.kind == METADATA_KIND)
            .cloned()
            .max_by(|left, right| {
                left.created_at
                    .cmp(&right.created_at)
                    .then_with(|| right.id.cmp(&left.id))
            });

        latest_metadata.into_iter().collect()
    }

    fn apply_live_byte_cap_from(
        &self,
        mut events: Vec<StoredNostrEvent>,
        live_bytes_selected_so_far: u64,
    ) -> Result<(Vec<StoredNostrEvent>, u64)> {
        events.sort_by(|left, right| {
            self.policy
                .priority(right)
                .cmp(&self.policy.priority(left))
                .then_with(|| right.created_at.cmp(&left.created_at))
                .then_with(|| left.id.cmp(&right.id))
        });

        let Some(max_live_bytes) = self.config.max_live_bytes else {
            let live_bytes_selected =
                events
                    .iter()
                    .try_fold(live_bytes_selected_so_far, |total, event| {
                        let encoded = self.event_store.encode_event(event)?;
                        Ok::<u64, NostrEventStoreError>(total.saturating_add(encoded.len() as u64))
                    })?;
            return Ok((events, live_bytes_selected));
        };

        let mut selected = Vec::new();
        let mut live_bytes_selected = live_bytes_selected_so_far;
        for event in events {
            let encoded_len = self.event_store.encode_event(&event)?.len() as u64;
            if live_bytes_selected.saturating_add(encoded_len) > max_live_bytes {
                continue;
            }
            live_bytes_selected = live_bytes_selected.saturating_add(encoded_len);
            selected.push(event);
        }

        Ok((selected, live_bytes_selected))
    }

    fn kind_allowed(&self, kind: u32) -> bool {
        self.config.kinds.as_ref().is_none_or(|allowed| {
            allowed
                .iter()
                .any(|candidate| u32::from(*candidate) == kind)
        })
    }
}

fn stored_event_from_nostr(event: &nostr_sdk::Event) -> StoredNostrEvent {
    StoredNostrEvent {
        id: event.id.to_hex(),
        pubkey: event.pubkey.to_hex(),
        created_at: event.created_at.as_u64(),
        kind: event.kind.as_u16() as u32,
        tags: event
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect(),
        content: event.content.clone(),
        sig: event.sig.to_string(),
    }
}

fn is_valid_hex_pubkey(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use hashtree_core::MemoryStore;
    use nostr_social_graph::{NostrEvent, SocialGraphBackend as NostrSocialGraphBackend};

    use super::{CrawlConfig, NostrBridge, StoredNostrEvent};

    #[derive(Default)]
    struct FakeGraphBackend;

    impl NostrSocialGraphBackend for FakeGraphBackend {
        type Error = std::io::Error;

        fn get_root(&self) -> std::result::Result<String, Self::Error> {
            Ok("0".repeat(64))
        }

        fn set_root(&mut self, _root: &str) -> std::result::Result<(), Self::Error> {
            Ok(())
        }

        fn handle_event(
            &mut self,
            _event: &NostrEvent,
            _allow_unknown_authors: bool,
            _overmute_threshold: f64,
        ) -> std::result::Result<(), Self::Error> {
            Ok(())
        }

        fn get_follow_distance(&self, _user: &str) -> std::result::Result<u32, Self::Error> {
            Ok(0)
        }

        fn is_following(
            &self,
            _follower: &str,
            _followed_user: &str,
        ) -> std::result::Result<bool, Self::Error> {
            Ok(false)
        }

        fn get_followed_by_user(
            &self,
            user: &str,
        ) -> std::result::Result<Vec<String>, Self::Error> {
            if user == "0".repeat(64) {
                return Ok(vec![
                    "1".repeat(64),
                    "NOT-HEX".to_string(),
                    "a".repeat(63),
                    "A".repeat(64),
                ]);
            }
            Ok(Vec::new())
        }

        fn get_followers_by_user(
            &self,
            _user: &str,
        ) -> std::result::Result<Vec<String>, Self::Error> {
            Ok(Vec::new())
        }

        fn get_muted_by_user(&self, _user: &str) -> std::result::Result<Vec<String>, Self::Error> {
            Ok(Vec::new())
        }

        fn get_user_muted_by(&self, _user: &str) -> std::result::Result<Vec<String>, Self::Error> {
            Ok(Vec::new())
        }

        fn get_follow_list_created_at(
            &self,
            _user: &str,
        ) -> std::result::Result<Option<u64>, Self::Error> {
            Ok(None)
        }

        fn get_mute_list_created_at(
            &self,
            _user: &str,
        ) -> std::result::Result<Option<u64>, Self::Error> {
            Ok(None)
        }

        fn is_overmuted(
            &self,
            _user: &str,
            _threshold: f64,
        ) -> std::result::Result<bool, Self::Error> {
            Ok(false)
        }
    }

    #[test]
    fn rejects_invalid_stored_event_shape() {
        let bridge = NostrBridge::new(Arc::new(MemoryStore::new()), CrawlConfig::default());
        let invalid = StoredNostrEvent {
            id: "f".repeat(64),
            pubkey: "not-hex".to_string(),
            created_at: 1,
            kind: 1,
            tags: Vec::new(),
            content: String::new(),
            sig: "f".repeat(128),
        };

        assert!(!bridge.is_valid_stored_event(&invalid));
    }

    #[test]
    fn collect_authors_skips_invalid_graph_pubkeys() {
        let bridge = NostrBridge::new(Arc::new(MemoryStore::new()), CrawlConfig::default());
        let authors = bridge
            .collect_authors(&FakeGraphBackend)
            .expect("collect authors");

        assert_eq!(authors, vec!["0".repeat(64), "1".repeat(64)]);
    }

    #[test]
    fn collect_authors_prefers_allowlist_and_applies_limits() {
        let bridge = NostrBridge::new(
            Arc::new(MemoryStore::new()),
            CrawlConfig {
                author_allowlist: Some(vec![
                    "1".repeat(64),
                    "NOT-HEX".to_string(),
                    "0".repeat(64),
                    "1".repeat(64),
                ]),
                max_authors: Some(1),
                ..CrawlConfig::default()
            },
        );
        let authors = bridge
            .collect_authors(&FakeGraphBackend)
            .expect("collect authors");

        assert_eq!(authors, vec!["1".repeat(64)]);
    }
}
