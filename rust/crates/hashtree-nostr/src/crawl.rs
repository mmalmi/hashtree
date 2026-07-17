use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use crate::{ListEventsOptions, NostrEventStore, NostrEventStoreError, StoredNostrEvent};
use futures::{stream, StreamExt};
use hashtree_core::{Cid, Store};
use nostr_sdk::{
    pool::RelayLimits, Client, ClientOptions, EventId, Filter, Keys, Kind, PublicKey, SyncOptions,
    Timestamp,
};
use nostr_social_graph::SocialGraphBackend;

const NEGENTROPY_FETCH_CHUNK_SIZE: usize = 256;
const NEGENTROPY_FETCH_CHUNK_CONCURRENCY: usize = 16;
const NEGENTROPY_INITIAL_TIMEOUT: Duration = Duration::from_secs(1);
const FULL_HISTORY_PAGING_CONCURRENCY_PER_RELAY: usize = 64;
const PER_AUTHOR_KIND_FETCH_CONCURRENCY_PER_RELAY: usize = 64;
const RELAY_QUERY_ATTEMPTS: usize = 3;
const RELAY_QUERY_RETRY_DELAY: Duration = Duration::from_millis(100);
const RELAY_BATCH_TIMEOUT_MULTIPLIER: u32 = 16;
const RELAY_BATCH_TIMEOUT_MAX: Duration = Duration::from_secs(300);
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
    /// Optional limit applied independently to each event kind for an author.
    /// None preserves the legacy shared per-author limit.
    pub per_author_kind_event_limit: Option<usize>,
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
            per_author_kind_event_limit: None,
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
    pub applied_events: Vec<StoredNostrEvent>,
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
    #[error("per-author per-kind event limit must be greater than zero")]
    InvalidPerAuthorKindLimit,
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
    #[error("relay does not support required negentropy: {0}")]
    NegentropyUnsupported(String),
    #[error("social graph error: {0}")]
    SocialGraph(String),
}

pub type Result<T> = std::result::Result<T, CrawlError>;

async fn retry_relay_query<T, F, Fut>(mut operation: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    for attempt in 1..=RELAY_QUERY_ATTEMPTS {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(err @ CrawlError::NegentropyUnsupported(_)) => return Err(err),
            Err(err) if attempt == RELAY_QUERY_ATTEMPTS => return Err(err),
            Err(_) => tokio::time::sleep(RELAY_QUERY_RETRY_DELAY).await,
        }
    }
    unreachable!("relay query attempt loop always returns")
}

fn reconciliation_supported<'a>(
    successful_relays: usize,
    failed_messages: impl IntoIterator<Item = &'a str>,
) -> Result<bool> {
    let failed_messages = failed_messages.into_iter().collect::<Vec<_>>();
    if successful_relays > 0 && failed_messages.is_empty() {
        return Ok(true);
    }
    if successful_relays == 0
        && !failed_messages.is_empty()
        && failed_messages.iter().all(|message| {
            message
                .trim()
                .eq_ignore_ascii_case("negentropy not supported")
        })
    {
        return Ok(false);
    }

    let detail = if failed_messages.is_empty() {
        "relay reconciliation was skipped without a result".to_string()
    } else {
        failed_messages.join("; ")
    };
    Err(CrawlError::Nostr(format!(
        "relay reconciliation failed: {detail}"
    )))
}

#[derive(Debug, Default)]
struct RelayFetchResult {
    events_seen: usize,
    events: Vec<StoredNostrEvent>,
    supports_negentropy: bool,
    remote_cardinality: Option<usize>,
}

#[derive(Debug)]
struct ReconciliationResult {
    remote_missing: Vec<EventId>,
    remote_cardinality: usize,
}

#[derive(Debug, Clone)]
struct FullHistoryPass {
    kind: Option<u16>,
    event_limit: usize,
    authors: Vec<(PublicKey, Vec<(EventId, Timestamp)>)>,
}

#[derive(Debug, Clone)]
struct AuthorKindQuery {
    kind: u16,
    authors: Vec<(PublicKey, Vec<(EventId, Timestamp)>)>,
    limit: usize,
}

#[derive(Debug, Default)]
struct InclusiveTimestampCursor {
    until: Option<u64>,
    boundary_ids: BTreeSet<String>,
}

impl InclusiveTimestampCursor {
    fn request_limit(&self, new_event_limit: usize) -> usize {
        self.boundary_ids.len().saturating_add(new_event_limit)
    }

    fn advance<'a, I>(&mut self, events: I) -> bool
    where
        I: IntoIterator<Item = &'a nostr_sdk::Event>,
    {
        let events = events.into_iter().collect::<Vec<_>>();
        let Some(boundary) = events.iter().map(|event| event.created_at.as_secs()).min() else {
            return false;
        };
        let page_boundary_ids = events
            .into_iter()
            .filter(|event| event.created_at.as_secs() == boundary)
            .map(|event| event.id.to_hex())
            .collect::<BTreeSet<_>>();

        let previous_until = self.until;
        let previous_boundary_count = if previous_until == Some(boundary) {
            self.boundary_ids.len()
        } else {
            0
        };
        if previous_until == Some(boundary) {
            self.boundary_ids.extend(page_boundary_ids);
        } else {
            self.boundary_ids = page_boundary_ids;
        }
        self.until = Some(boundary);

        previous_until != Some(boundary) || self.boundary_ids.len() > previous_boundary_count
    }
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

        let existing_root = self.usable_existing_root(existing_root).await?;
        let client = self.connect_client().await?;

        let result = if self.config.relay_fetch_mode == RelayFetchMode::AuthorBatches {
            self.crawl_author_batches(&client, &authors, existing_root.as_ref(), &mut on_progress)
                .await
        } else {
            let state = self
                .load_existing_global_state(existing_root.as_ref(), &authors)
                .await?;

            let report = self
                .crawl_global_recent_incremental(&client, &authors, state, &mut on_progress)
                .await?;
            on_progress(&report);
            Ok(report)
        };

        let _ = client.disconnect().await;
        result
    }

    async fn usable_existing_root(&self, root: Option<&Cid>) -> Result<Option<Cid>> {
        let Some(root) = root else {
            return Ok(None);
        };

        match self.event_store.validate_index_root(Some(root)).await {
            Ok(()) => Ok(Some(root.clone())),
            Err(err) => {
                eprintln!(
                    "Ignoring invalid existing Nostr event index root {}: {err}",
                    hex::encode(root.hash)
                );
                Ok(None)
            }
        }
    }

    async fn crawl_author_batches(
        &self,
        client: &Client,
        authors: &[String],
        existing_root: Option<&Cid>,
        on_progress: &mut impl FnMut(&CrawlReport),
    ) -> Result<CrawlReport> {
        let mut relay_negentropy_support = BTreeMap::<String, bool>::new();
        let mut current_root = existing_root.cloned();
        let mut events_seen = 0usize;
        let mut events_selected = 0usize;
        let mut live_bytes_selected = 0u64;
        let mut authors_processed = 0usize;
        let mut applied_events = Vec::new();

        for author_batch in authors.chunks(self.config.author_batch_size) {
            let batch = self
                .crawl_author_batch(
                    client,
                    author_batch,
                    current_root.as_ref(),
                    &mut relay_negentropy_support,
                    live_bytes_selected,
                )
                .await?;
            events_seen = events_seen.saturating_add(batch.events_seen);
            events_selected = events_selected.saturating_add(batch.events_selected);
            live_bytes_selected = batch.live_bytes_selected;
            authors_processed = authors_processed.saturating_add(author_batch.len());
            if !batch.events.is_empty() {
                applied_events.extend(batch.events.clone());
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
                applied_events: Vec::new(),
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
            applied_events,
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

    fn relay_batch_timeout(&self) -> Duration {
        self.config
            .fetch_timeout
            .checked_mul(RELAY_BATCH_TIMEOUT_MULTIPLIER)
            .unwrap_or(RELAY_BATCH_TIMEOUT_MAX)
            .min(RELAY_BATCH_TIMEOUT_MAX)
    }

    fn validate_config(&self) -> Result<()> {
        if self.config.relays.is_empty() {
            return Err(CrawlError::MissingRelays);
        }
        if self.config.per_author_event_limit == 0 {
            return Err(CrawlError::InvalidPerAuthorLimit);
        }
        if self.config.per_author_kind_event_limit == Some(0) {
            return Err(CrawlError::InvalidPerAuthorKindLimit);
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
            Client::builder()
                .signer(Keys::generate())
                .opts(ClientOptions::new().relay_limits(limits))
                .build()
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
                    live_bytes_selected_so_far,
                )
                .await;
        }

        let per_author_kind_queries = self.per_author_kind_queries(&pubkeys, &known);
        let filter = per_author_kind_queries
            .is_none()
            .then(|| self.batch_filter(pubkeys));
        let mut fetched = BTreeMap::<String, StoredNostrEvent>::new();
        let mut events_seen = 0usize;
        let mut successful_relays = 0usize;
        let mut last_relay_error = None;

        for relay in &self.config.relays {
            let relay_support = relay_negentropy_support.get(relay).copied();
            let relay_fetch = async {
                if let Some(queries) = &per_author_kind_queries {
                    self.fetch_per_author_kind_from_relay(
                        client,
                        relay,
                        queries.clone(),
                        relay_support,
                    )
                    .await
                } else {
                    let local_items = self.local_items_for_batch(known.values(), author_batch);
                    self.fetch_events_from_relay(
                        client,
                        relay,
                        filter.clone().expect("legacy batch filter"),
                        local_items,
                        relay_support,
                    )
                    .await
                }
            };
            let fetched_from_relay =
                match tokio::time::timeout(self.relay_batch_timeout(), relay_fetch).await {
                    Ok(Ok(result)) => result,
                    Ok(Err(err)) => {
                        if matches!(&err, CrawlError::NegentropyUnsupported(_)) {
                            relay_negentropy_support.insert(relay.clone(), false);
                        }
                        eprintln!("Skipping relay {relay} for this author batch: {err}");
                        last_relay_error = Some(err);
                        continue;
                    }
                    Err(_) => {
                        let err = CrawlError::Nostr(format!(
                            "relay {relay} exceeded the per-author-batch deadline"
                        ));
                        eprintln!("Skipping relay {relay} for this author batch: {err}");
                        last_relay_error = Some(err);
                        continue;
                    }
                };
            successful_relays = successful_relays.saturating_add(1);
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
        if successful_relays == 0 {
            return Err(last_relay_error.unwrap_or_else(|| {
                CrawlError::Nostr("all relays were unavailable for the author batch".to_string())
            }));
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

    #[allow(clippy::too_many_arguments)]
    async fn crawl_full_author_history_batch(
        &self,
        client: &Client,
        author_batch: &[String],
        mut existing_by_author: BTreeMap<String, Vec<StoredNostrEvent>>,
        mut known: BTreeMap<String, StoredNostrEvent>,
        initial_known_ids: BTreeSet<String>,
        relay_negentropy_support: &mut BTreeMap<String, bool>,
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

        let full_history_passes = self.full_history_passes(known.values(), &pubkeys);
        let mut fetched_by_author = BTreeMap::<String, Vec<StoredNostrEvent>>::new();

        let relays_to_fetch = self
            .config
            .relays
            .iter()
            .map(|relay| (relay.clone(), relay_negentropy_support.get(relay).copied()))
            .collect::<Vec<_>>();
        let relay_fetches = relays_to_fetch.into_iter().map(|(relay, relay_support)| {
            let passes = full_history_passes.clone();
            let batch_timeout = self.relay_batch_timeout();
            async move {
                let result = match tokio::time::timeout(
                    batch_timeout,
                    self.fetch_full_history_passes_from_relay(
                        client,
                        &relay,
                        passes,
                        relay_support,
                    ),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => Err(CrawlError::Nostr(format!(
                        "relay {relay} exceeded the full-history author-batch deadline"
                    ))),
                };
                (relay, result)
            }
        });
        let mut relay_fetches =
            stream::iter(relay_fetches).buffer_unordered(self.config.relays.len().max(1));
        let mut successful_relays = 0usize;
        let mut last_relay_error = None;

        while let Some((relay, result)) = relay_fetches.next().await {
            match result {
                Ok(fetched_from_relay) => {
                    successful_relays = successful_relays.saturating_add(1);
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
                    if matches!(&err, CrawlError::NegentropyUnsupported(_)) {
                        relay_negentropy_support.insert(relay.clone(), false);
                    }
                    eprintln!("Skipping relay {relay} for this full-history batch: {err}");
                    last_relay_error = Some(err);
                }
            }
        }
        if successful_relays == 0 {
            return Err(last_relay_error.unwrap_or_else(|| {
                CrawlError::Nostr("all full-history relays were unavailable".to_string())
            }));
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
                .select_author_events(merged.into_values().collect())?
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
        let mut seen_event_ids = BTreeSet::<String>::new();
        let mut applied_events = Vec::new();

        self.hydrate_global_recent_profiles(
            client,
            authors,
            &mut state,
            &mut known_ids,
            &mut authors_processed,
            &mut applied_events,
            &mut relay_negentropy_support,
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
                applied_events,
            });
        }

        for relay in &self.config.relays {
            if failed_relays.contains(relay) {
                continue;
            }
            let mut cursor = InclusiveTimestampCursor::default();
            for _ in 0..self.config.max_relay_pages {
                let filter = self.global_recent_filter(
                    cursor.until,
                    cursor.request_limit(self.config.relay_page_size),
                );
                let events = match client
                    .fetch_events_from([relay], filter, self.config.fetch_timeout)
                    .await
                    .map(|events| events.to_vec())
                {
                    Ok(events) => events,
                    Err(err) => {
                        eprintln!("Skipping relay {relay}: {}", err);
                        failed_relays.insert(relay.clone());
                        break;
                    }
                };
                let fetched_count = events.len();
                let unique_fetched_count = events
                    .iter()
                    .filter(|event| seen_event_ids.insert(event.id.to_hex()))
                    .count();
                events_seen = events_seen.saturating_add(unique_fetched_count);
                if fetched_count == 0 {
                    break;
                }
                let cursor_advanced = cursor.advance(events.iter());

                let mut incoming_by_author = BTreeMap::<String, Vec<StoredNostrEvent>>::new();
                for event in events {
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
                    applied_events.extend(pending_apply.clone());
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
                    applied_events: Vec::new(),
                });

                if !cursor_advanced {
                    break;
                }
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
            applied_events,
        })
    }

    fn batch_filter(&self, pubkeys: Vec<PublicKey>) -> Filter {
        let mut filter = Filter::new().authors(pubkeys);
        if let Some(kinds) = &self.config.kinds {
            filter = filter.kinds(kinds.iter().copied().map(Kind::from));
        }
        let per_author_relay_limit = match self.config.per_author_kind_event_limit {
            Some(per_kind_limit) => per_kind_limit.saturating_mul(
                self.config
                    .kinds
                    .as_ref()
                    .map_or(1, |kinds| kinds.len().max(1)),
            ),
            None => self.config.per_author_event_limit,
        };
        let mut relay_limit = self
            .config
            .author_batch_size
            .saturating_mul(per_author_relay_limit);
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

    fn per_author_kind_queries(
        &self,
        pubkeys: &[PublicKey],
        known: &BTreeMap<String, StoredNostrEvent>,
    ) -> Option<Vec<AuthorKindQuery>> {
        let per_kind_limit = self.config.per_author_kind_event_limit?;
        let kinds = self.config.kinds.as_ref()?;
        Some(
            kinds
                .iter()
                .copied()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .map(|kind| {
                    let authors = pubkeys
                        .iter()
                        .copied()
                        .map(|pubkey| {
                            let local_items = self.local_items_for_author_and_kind(
                                known.values(),
                                pubkey.to_hex().as_str(),
                                u32::from(kind),
                            );
                            (pubkey, local_items)
                        })
                        .collect::<Vec<_>>();
                    self.author_kind_query(kind, authors, per_kind_limit)
                })
                .collect(),
        )
    }

    fn author_kind_query(
        &self,
        kind: u16,
        authors: Vec<(PublicKey, Vec<(EventId, Timestamp)>)>,
        per_kind_limit: usize,
    ) -> AuthorKindQuery {
        let query_limit_cap = self
            .config
            .relay_page_size
            .saturating_mul(self.config.max_relay_pages.max(1))
            .max(1);
        AuthorKindQuery {
            kind,
            limit: per_kind_limit
                .saturating_mul(authors.len())
                .min(query_limit_cap)
                .max(1),
            authors,
        }
    }

    fn full_history_negentropy_filter(&self, pubkeys: Vec<PublicKey>, kind: Option<u16>) -> Filter {
        let mut filter = Filter::new().authors(pubkeys);
        if let Some(kind) = kind {
            filter = filter.kind(Kind::from(kind));
        } else if let Some(kinds) = &self.config.kinds {
            filter = filter.kinds(kinds.iter().copied().map(Kind::from));
        }
        filter
    }

    fn global_recent_filter(&self, until: Option<u64>, limit: usize) -> Filter {
        let mut filter = Filter::new().limit(limit);
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

    fn full_history_passes<'a, I>(
        &self,
        known_events: I,
        pubkeys: &[PublicKey],
    ) -> Vec<FullHistoryPass>
    where
        I: Iterator<Item = &'a StoredNostrEvent> + Clone,
    {
        match (
            self.config.per_author_kind_event_limit,
            self.config.kinds.as_ref(),
        ) {
            (Some(event_limit), Some(kinds)) => kinds
                .iter()
                .copied()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .map(|kind| FullHistoryPass {
                    kind: Some(kind),
                    event_limit,
                    authors: pubkeys
                        .iter()
                        .copied()
                        .map(|pubkey| {
                            let local_items = self.local_items_for_author_and_kind(
                                known_events.clone(),
                                pubkey.to_hex().as_str(),
                                u32::from(kind),
                            );
                            (pubkey, local_items)
                        })
                        .collect(),
                })
                .collect(),
            _ => vec![FullHistoryPass {
                kind: None,
                event_limit: self.config.per_author_event_limit,
                authors: pubkeys
                    .iter()
                    .copied()
                    .map(|pubkey| {
                        let local_items = self
                            .local_items_for_author(known_events.clone(), pubkey.to_hex().as_str());
                        (pubkey, local_items)
                    })
                    .collect(),
            }],
        }
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

    fn local_items_for_author_and_kind<'a, I>(
        &self,
        known_events: I,
        author: &str,
        kind: u32,
    ) -> Vec<(EventId, Timestamp)>
    where
        I: Iterator<Item = &'a StoredNostrEvent>,
    {
        known_events
            .filter(|event| event.pubkey == author && event.kind == kind)
            .filter_map(|event| {
                let event_id = EventId::parse(&event.id).ok()?;
                Some((event_id, Timestamp::from_secs(event.created_at)))
            })
            .collect()
    }

    fn local_items_for_author<'a, I>(
        &self,
        known_events: I,
        author: &str,
    ) -> Vec<(EventId, Timestamp)>
    where
        I: Iterator<Item = &'a StoredNostrEvent>,
    {
        known_events
            .filter(|event| event.pubkey == author && self.kind_allowed(event.kind))
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
        if self.config.per_author_kind_event_limit.is_some() {
            if let Some(kinds) = self.config.kinds.as_ref() {
                let mut events = Vec::new();
                for kind in kinds.iter().copied().collect::<BTreeSet<_>>() {
                    events.extend(
                        self.event_store
                            .list_by_author_and_kind_lossy(
                                root,
                                author,
                                u32::from(kind),
                                ListEventsOptions::default(),
                            )
                            .await?,
                    );
                }
                return Ok(events);
            }
        } else if let Some([kind]) = self.config.kinds.as_deref() {
            return Ok(self
                .event_store
                .list_by_author_and_kind_lossy(
                    root,
                    author,
                    u32::from(*kind),
                    ListEventsOptions::default(),
                )
                .await?);
        }
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

    #[allow(clippy::too_many_arguments)]
    async fn hydrate_global_recent_profiles(
        &self,
        client: &Client,
        authors: &[String],
        state: &mut GlobalRecentState,
        known_ids: &mut BTreeSet<String>,
        authors_processed: &mut usize,
        applied_events: &mut Vec<StoredNostrEvent>,
        relay_negentropy_support: &mut BTreeMap<String, bool>,
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
                applied_events.extend(pending_apply.clone());
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
                applied_events: Vec::new(),
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
        let mut successful_relays = 0usize;
        let mut last_relay_error = None;

        for relay in &self.config.relays {
            let relay_support = relay_negentropy_support.get(relay).copied();
            let relay_fetch = retry_relay_query(|| {
                self.fetch_events_from_relay(
                    client,
                    relay,
                    filter.clone(),
                    local_items.clone(),
                    relay_support,
                )
            });
            let fetched_from_relay =
                match tokio::time::timeout(self.relay_batch_timeout(), relay_fetch).await {
                    Ok(Ok(result)) => result,
                    Ok(Err(err)) => {
                        if matches!(&err, CrawlError::NegentropyUnsupported(_)) {
                            relay_negentropy_support.insert(relay.clone(), false);
                        }
                        eprintln!("Skipping relay {relay} for this profile batch: {err}");
                        last_relay_error = Some(err);
                        continue;
                    }
                    Err(_) => {
                        let err = CrawlError::Nostr(format!(
                            "relay {relay} exceeded the profile author-batch deadline"
                        ));
                        eprintln!("Skipping relay {relay} for this profile batch: {err}");
                        last_relay_error = Some(err);
                        continue;
                    }
                };
            successful_relays = successful_relays.saturating_add(1);
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
        if successful_relays == 0 {
            return Err(last_relay_error.unwrap_or_else(|| {
                CrawlError::Nostr("all relays were unavailable for the profile batch".to_string())
            }));
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
                return Err(CrawlError::NegentropyUnsupported(relay.to_string()));
            }
            return self
                .fetch_full_filter(client, relay, filter)
                .await
                .map(|events| RelayFetchResult {
                    events_seen: events.len(),
                    events,
                    supports_negentropy: false,
                    remote_cardinality: None,
                });
        }

        match self
            .reconcile_missing_ids(client, relay, filter.clone(), local_items)
            .await
        {
            Ok(Some(reconciliation)) => self
                .fetch_missing_ids(client, relay, reconciliation.remote_missing)
                .await
                .map(
                    |RelayFetchResult {
                         events_seen,
                         events,
                         ..
                     }| RelayFetchResult {
                        events_seen,
                        events,
                        supports_negentropy: true,
                        remote_cardinality: Some(reconciliation.remote_cardinality),
                    },
                ),
            Ok(None) => {
                if self.config.require_negentropy {
                    Err(CrawlError::NegentropyUnsupported(relay.to_string()))
                } else {
                    self.fetch_full_filter(client, relay, filter)
                        .await
                        .map(|events| RelayFetchResult {
                            events_seen: events.len(),
                            events,
                            supports_negentropy: false,
                            remote_cardinality: None,
                        })
                }
            }
            Err(err) if self.config.require_negentropy => Err(err),
            // Some ordinary relays silently ignore NIP-77. Optional
            // reconciliation must not disable the configured bounded REQ path.
            Err(_) => self
                .fetch_full_filter(client, relay, filter)
                .await
                .map(|events| RelayFetchResult {
                    events_seen: events.len(),
                    events,
                    supports_negentropy: false,
                    remote_cardinality: None,
                }),
        }
    }

    async fn fetch_per_author_kind_from_relay(
        &self,
        client: &Client,
        relay: &str,
        queries: Vec<AuthorKindQuery>,
        supports_negentropy: Option<bool>,
    ) -> Result<RelayFetchResult> {
        let per_kind_limit = self
            .config
            .per_author_kind_event_limit
            .expect("per-kind query limit");
        let mut pending = VecDeque::from(queries);
        let mut out = BTreeMap::<String, StoredNostrEvent>::new();
        let mut events_seen = 0usize;
        let mut relay_support = supports_negentropy;

        while !pending.is_empty() {
            let wave_len = PER_AUTHOR_KIND_FETCH_CONCURRENCY_PER_RELAY.min(pending.len());
            let wave = pending.drain(..wave_len).collect::<Vec<_>>();
            let support_hint = relay_support;
            let fetches = wave.into_iter().map(|query| async move {
                let filter = Filter::new()
                    .authors(
                        query
                            .authors
                            .iter()
                            .map(|(pubkey, _)| *pubkey)
                            .collect::<Vec<_>>(),
                    )
                    .kind(Kind::from(query.kind))
                    .limit(query.limit);
                let local_items = query
                    .authors
                    .iter()
                    .flat_map(|(_, items)| items.iter().cloned())
                    .collect::<Vec<_>>();
                let result = retry_relay_query(|| {
                    self.fetch_events_from_relay(
                        client,
                        relay,
                        filter.clone(),
                        local_items.clone(),
                        support_hint,
                    )
                })
                .await;
                (query, result)
            });
            let mut fetches = stream::iter(fetches).buffer_unordered(wave_len.max(1));

            while let Some((mut query, result)) = fetches.next().await {
                let fetched = result?;
                let response_saturated = if fetched.supports_negentropy {
                    fetched
                        .remote_cardinality
                        .is_some_and(|cardinality| cardinality >= query.limit)
                } else {
                    fetched.events.len() >= query.limit
                };
                events_seen = events_seen.saturating_add(fetched.events_seen);
                relay_support = Some(relay_support.unwrap_or(true) && fetched.supports_negentropy);

                for event in fetched.events {
                    out.insert(event.id.clone(), event);
                }

                if response_saturated && query.authors.len() > 1 {
                    let right_authors = query.authors.split_off(query.authors.len() / 2);
                    pending.push_back(self.author_kind_query(
                        query.kind,
                        query.authors,
                        per_kind_limit,
                    ));
                    pending.push_back(self.author_kind_query(
                        query.kind,
                        right_authors,
                        per_kind_limit,
                    ));
                }
            }
        }

        Ok(RelayFetchResult {
            events_seen,
            events: out.into_values().collect(),
            supports_negentropy: relay_support.unwrap_or(false),
            remote_cardinality: None,
        })
    }

    async fn fetch_missing_ids(
        &self,
        client: &Client,
        relay: &str,
        missing_ids: Vec<EventId>,
    ) -> Result<RelayFetchResult> {
        let expected_ids = missing_ids
            .iter()
            .map(EventId::to_hex)
            .collect::<BTreeSet<_>>();
        if expected_ids.is_empty() {
            return Ok(RelayFetchResult {
                events_seen: 0,
                events: Vec::new(),
                supports_negentropy: true,
                remote_cardinality: None,
            });
        }

        let mut out = BTreeMap::<String, StoredNostrEvent>::new();
        let filters = missing_ids
            .chunks(NEGENTROPY_FETCH_CHUNK_SIZE)
            .map(|chunk| Filter::new().ids(chunk.iter().cloned()))
            .collect::<Vec<_>>();
        let fetches = filters.into_iter().map(|filter| async move {
            client
                .fetch_events_from([relay], filter, self.config.fetch_timeout)
                .await
                .map(|events| events.to_vec())
                .map_err(|err| CrawlError::Nostr(err.to_string()))
        });
        let mut fetches =
            stream::iter(fetches).buffer_unordered(NEGENTROPY_FETCH_CHUNK_CONCURRENCY);
        while let Some(result) = fetches.next().await {
            let events = result?;
            for event in events {
                let event_id = event.id.to_hex();
                if !expected_ids.contains(&event_id)
                    || event.kind.is_ephemeral()
                    || !self.kind_allowed(u32::from(event.kind.as_u16()))
                {
                    continue;
                }
                let stored = stored_event_from_nostr(&event);
                out.insert(stored.id.clone(), stored);
            }
        }
        let missing_count = expected_ids
            .iter()
            .filter(|event_id| !out.contains_key(*event_id))
            .count();
        if missing_count > 0 {
            return Err(CrawlError::Nostr(format!(
                "relay {relay} omitted {missing_count} of {} reconciled event IDs",
                expected_ids.len()
            )));
        }
        let events_seen = out.len();
        Ok(RelayFetchResult {
            events_seen,
            events: out.into_values().collect(),
            supports_negentropy: true,
            remote_cardinality: None,
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
            .fetch_events_from([relay], filter, self.config.fetch_timeout)
            .await
            .map(|events| events.to_vec())
            .map_err(|err| CrawlError::Nostr(err.to_string()))?;

        for event in events {
            if event.kind.is_ephemeral() {
                continue;
            }
            out.push(stored_event_from_nostr(&event));
        }

        Ok(out)
    }

    async fn fetch_full_history_passes_from_relay(
        &self,
        client: &Client,
        relay: &str,
        passes: Vec<FullHistoryPass>,
        supports_negentropy: Option<bool>,
    ) -> Result<RelayFetchResult> {
        let mut out = BTreeMap::<String, StoredNostrEvent>::new();
        let mut events_seen = 0usize;
        let mut relay_support = supports_negentropy;

        for pass in passes {
            let fetched = retry_relay_query(|| {
                self.fetch_full_history_from_relay(client, relay, pass.clone(), relay_support)
            })
            .await?;
            events_seen = events_seen.saturating_add(fetched.events_seen);
            relay_support = Some(fetched.supports_negentropy);
            for event in fetched.events {
                out.insert(event.id.clone(), event);
            }
        }

        Ok(RelayFetchResult {
            events_seen,
            events: out.into_values().collect(),
            supports_negentropy: relay_support.unwrap_or(false),
            remote_cardinality: None,
        })
    }

    async fn fetch_full_history_from_relay(
        &self,
        client: &Client,
        relay: &str,
        pass: FullHistoryPass,
        supports_negentropy: Option<bool>,
    ) -> Result<RelayFetchResult> {
        let mut pending = VecDeque::from([pass]);
        let mut out = BTreeMap::<String, StoredNostrEvent>::new();
        let mut events_seen = 0usize;
        let mut relay_support = supports_negentropy;

        while let Some(mut query) = pending.pop_front() {
            let pubkeys = query
                .authors
                .iter()
                .map(|(pubkey, _)| *pubkey)
                .collect::<Vec<_>>();
            if relay_support == Some(false) {
                if self.config.require_negentropy {
                    return Err(CrawlError::NegentropyUnsupported(relay.to_string()));
                }
                if self.config.max_relay_pages == 0 {
                    continue;
                }
                let fetched = self
                    .fetch_full_history_by_paging_from_relay(
                        client,
                        relay,
                        &pubkeys,
                        query.kind,
                        query.event_limit,
                    )
                    .await?;
                events_seen = events_seen.saturating_add(fetched.events_seen);
                for event in fetched.events {
                    out.insert(event.id.clone(), event);
                }
                continue;
            }

            let query_limit = query.event_limit.max(1);
            let filter = self
                .full_history_negentropy_filter(pubkeys.clone(), query.kind)
                .limit(query_limit);
            let local_items = query
                .authors
                .iter()
                .flat_map(|(_, items)| items.iter().cloned())
                .collect::<Vec<_>>();
            let missing = match self
                .reconcile_missing_ids(client, relay, filter, local_items)
                .await
            {
                Ok(missing) => missing,
                Err(err) if self.config.require_negentropy || self.config.max_relay_pages == 0 => {
                    return Err(err);
                }
                // Full-history callers explicitly configured a finite paging
                // fallback, so a silent NIP-77 relay can still make progress.
                Err(_) => None,
            };
            let Some(reconciliation) = missing else {
                relay_support = Some(false);
                if self.config.require_negentropy {
                    return Err(CrawlError::NegentropyUnsupported(relay.to_string()));
                }
                if self.config.max_relay_pages == 0 {
                    continue;
                }
                let fetched = self
                    .fetch_full_history_by_paging_from_relay(
                        client,
                        relay,
                        &pubkeys,
                        query.kind,
                        query.event_limit,
                    )
                    .await?;
                events_seen = events_seen.saturating_add(fetched.events_seen);
                for event in fetched.events {
                    out.insert(event.id.clone(), event);
                }
                continue;
            };

            if reconciliation.remote_cardinality >= query_limit && query.authors.len() > 1 {
                let right_authors = query.authors.split_off(query.authors.len() / 2);
                pending.push_back(FullHistoryPass {
                    kind: query.kind,
                    event_limit: query.event_limit,
                    authors: query.authors,
                });
                pending.push_back(FullHistoryPass {
                    kind: query.kind,
                    event_limit: query.event_limit,
                    authors: right_authors,
                });
                continue;
            }

            let expected_missing = reconciliation.remote_missing;
            let fetched = match self
                .fetch_missing_ids(client, relay, expected_missing.clone())
                .await
            {
                Ok(fetched) => fetched,
                Err(err) if !self.config.require_negentropy && self.config.max_relay_pages > 0 => {
                    let mut fetched = self
                        .fetch_full_history_by_paging_from_relay(
                            client,
                            relay,
                            &pubkeys,
                            query.kind,
                            query.event_limit,
                        )
                        .await
                        .map_err(|paging_err| {
                            CrawlError::Nostr(format!(
                                "relay {relay} direct reconciled-ID fetch failed: {err}; paging fallback failed: {paging_err}"
                            ))
                        })?;
                    let fetched_ids = fetched
                        .events
                        .iter()
                        .map(|event| event.id.clone())
                        .collect::<BTreeSet<_>>();
                    let missing_count = expected_missing
                        .iter()
                        .filter(|event_id| !fetched_ids.contains(&event_id.to_hex()))
                        .count();
                    if missing_count > 0 {
                        return Err(CrawlError::Nostr(format!(
                            "relay {relay} paging fallback omitted {missing_count} of {} reconciled event IDs after direct fetch failed: {err}",
                            expected_missing.len()
                        )));
                    }
                    fetched.supports_negentropy = true;
                    fetched
                }
                Err(err) => return Err(err),
            };
            relay_support = Some(fetched.supports_negentropy);
            events_seen = events_seen.saturating_add(fetched.events_seen);
            for event in fetched.events {
                out.insert(event.id.clone(), event);
            }
        }

        Ok(RelayFetchResult {
            events_seen,
            events: out.into_values().collect(),
            supports_negentropy: relay_support.unwrap_or(false),
            remote_cardinality: None,
        })
    }

    async fn reconcile_missing_ids(
        &self,
        client: &Client,
        relay: &str,
        filter: Filter,
        local_items: Vec<(EventId, Timestamp)>,
    ) -> Result<Option<ReconciliationResult>> {
        let initial_timeout = self.config.fetch_timeout.min(NEGENTROPY_INITIAL_TIMEOUT);
        let opts = SyncOptions::default()
            .initial_timeout(initial_timeout)
            .dry_run();
        let unique_local_ids = local_items
            .iter()
            .map(|(event_id, _)| *event_id)
            .collect::<BTreeSet<_>>();
        let targets = [(relay.to_owned(), (filter, local_items))];
        let sync = client.pool().sync_targeted(targets, &opts);
        let output = match tokio::time::timeout(self.config.fetch_timeout, sync).await {
            Ok(Ok(output)) => output,
            Ok(Err(err)) => return Err(CrawlError::Nostr(err.to_string())),
            Err(_) => {
                return Err(CrawlError::Nostr(format!(
                    "negentropy reconciliation with relay {relay} timed out"
                )))
            }
        };

        if !reconciliation_supported(
            output.success.len(),
            output.failed.values().map(String::as_str),
        )? {
            return Ok(None);
        }

        let mut remote_ids = unique_local_ids;
        for local_only_id in &output.local {
            remote_ids.remove(local_only_id);
        }
        remote_ids.extend(output.remote.iter().copied());
        Ok(Some(ReconciliationResult {
            remote_missing: output.remote.iter().copied().collect(),
            remote_cardinality: remote_ids.len(),
        }))
    }

    async fn fetch_full_history_by_paging_from_relay(
        &self,
        client: &Client,
        relay: &str,
        pubkeys: &[PublicKey],
        kind: Option<u16>,
        per_author_event_limit: usize,
    ) -> Result<RelayFetchResult> {
        let mut out = BTreeMap::<String, StoredNostrEvent>::new();
        let mut events_seen = 0usize;
        let concurrency = FULL_HISTORY_PAGING_CONCURRENCY_PER_RELAY
            .min(pubkeys.len().max(1))
            .max(1);
        let fetches = pubkeys.iter().copied().map(|pubkey| async move {
            self.fetch_full_author_history_by_paging_from_relay(
                client,
                relay,
                pubkey,
                kind,
                per_author_event_limit,
            )
            .await
        });
        let mut fetches = stream::iter(fetches).buffer_unordered(concurrency);

        while let Some(result) = fetches.next().await {
            let fetched = result?;
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
            remote_cardinality: None,
        })
    }

    async fn fetch_full_author_history_by_paging_from_relay(
        &self,
        client: &Client,
        relay: &str,
        pubkey: PublicKey,
        kind: Option<u16>,
        per_author_event_limit: usize,
    ) -> Result<RelayFetchResult> {
        let mut out = BTreeMap::<String, StoredNostrEvent>::new();
        let mut events_seen = 0usize;
        let mut cursor = InclusiveTimestampCursor::default();

        for _ in 0..self.config.max_relay_pages {
            let remaining = per_author_event_limit.saturating_sub(out.len());
            if remaining == 0 {
                break;
            }
            let mut filter = Filter::new()
                .author(pubkey)
                .limit(cursor.request_limit(self.config.relay_page_size.min(remaining)));
            if let Some(kind) = kind {
                filter = filter.kind(Kind::from(kind));
            } else if let Some(kinds) = &self.config.kinds {
                filter = filter.kinds(kinds.iter().copied().map(Kind::from));
            }
            if let Some(until) = cursor.until {
                filter = filter.until(Timestamp::from_secs(until));
            }

            let events = client
                .fetch_events_from([relay], filter, self.config.fetch_timeout)
                .await
                .map(|events| events.to_vec())
                .map_err(|err| CrawlError::Nostr(err.to_string()))?;
            let fetched_count = events.len();
            events_seen = events_seen.saturating_add(fetched_count);
            if fetched_count == 0 {
                break;
            }
            let cursor_advanced = cursor.advance(events.iter());

            for event in events {
                if event.kind.is_ephemeral() {
                    continue;
                }
                let stored = stored_event_from_nostr(&event);
                out.insert(stored.id.clone(), stored);
                if out.len() >= per_author_event_limit {
                    break;
                }
            }

            if out.len() >= per_author_event_limit {
                break;
            }
            if !cursor_advanced {
                break;
            }
            if self.reached_events_seen_limit(events_seen) {
                break;
            }
        }

        Ok(RelayFetchResult {
            events_seen,
            events: out.into_values().collect(),
            supports_negentropy: false,
            remote_cardinality: None,
        })
    }

    fn select_author_events(&self, events: Vec<StoredNostrEvent>) -> Result<Vec<StoredNostrEvent>> {
        if let Some(per_kind_limit) = self.config.per_author_kind_event_limit {
            let mut events_by_kind = BTreeMap::<u32, Vec<StoredNostrEvent>>::new();
            for event in events {
                events_by_kind.entry(event.kind).or_default().push(event);
            }
            let mut selected = Vec::new();
            for kind_events in events_by_kind.into_values() {
                selected.extend(self.select_author_events_with_limits(
                    kind_events,
                    per_kind_limit,
                    None,
                )?);
            }
            return self.select_author_events_with_limits(
                selected,
                usize::MAX,
                self.config.per_author_live_bytes,
            );
        }
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
        created_at: event.created_at.as_secs(),
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use hashtree_core::MemoryStore;
    use nostr_sdk::pool::{Output, Reconciliation};
    use nostr_sdk::{Keys, RelayUrl};
    use nostr_social_graph::{NostrEvent, SocialGraphBackend as NostrSocialGraphBackend};

    use super::{
        reconciliation_supported, retry_relay_query, CrawlConfig, CrawlError, NostrBridge,
        StoredNostrEvent,
    };

    #[derive(Default)]
    struct FakeGraphBackend;

    #[test]
    fn reconciliation_output_distinguishes_unsupported_from_failed_and_skipped() {
        let relay = RelayUrl::parse("wss://relay.example").expect("relay URL");
        let mut transient: Output<Reconciliation> = Output::default();
        transient
            .failed
            .insert(relay.clone(), "connection timed out".to_string());
        assert!(reconciliation_supported(
            transient.success.len(),
            transient.failed.values().map(String::as_str),
        )
        .is_err());

        let skipped: Output<Reconciliation> = Output::default();
        assert!(reconciliation_supported(
            skipped.success.len(),
            skipped.failed.values().map(String::as_str),
        )
        .is_err());

        let mut unsupported: Output<Reconciliation> = Output::default();
        unsupported
            .failed
            .insert(relay.clone(), "negentropy not supported".to_string());
        assert!(!reconciliation_supported(
            unsupported.success.len(),
            unsupported.failed.values().map(String::as_str),
        )
        .expect("explicit unsupported result"));

        let mut supported: Output<Reconciliation> = Output::default();
        supported.success.insert(relay);
        assert!(reconciliation_supported(
            supported.success.len(),
            supported.failed.values().map(String::as_str),
        )
        .expect("successful reconciliation"));
    }

    #[tokio::test]
    async fn relay_query_retry_is_scoped_and_bounded() {
        let attempts = AtomicUsize::new(0);
        let value = retry_relay_query(|| {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            async move {
                if attempt < 2 {
                    Err(CrawlError::Nostr("transient".to_string()))
                } else {
                    Ok(42)
                }
            }
        })
        .await
        .expect("third query attempt succeeds");

        assert_eq!(value, 42);
        assert_eq!(attempts.load(Ordering::SeqCst), 3);

        let failed_attempts = AtomicUsize::new(0);
        retry_relay_query(|| {
            failed_attempts.fetch_add(1, Ordering::SeqCst);
            async { Err::<(), _>(CrawlError::Nostr("persistent".to_string())) }
        })
        .await
        .expect_err("persistent failure exhausts the bounded attempts");
        assert_eq!(failed_attempts.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn relay_batch_timeout_scales_and_caps() {
        let scaled = NostrBridge::new(
            Arc::new(MemoryStore::new()),
            CrawlConfig {
                fetch_timeout: Duration::from_secs(10),
                ..CrawlConfig::default()
            },
        );
        let capped = NostrBridge::new(
            Arc::new(MemoryStore::new()),
            CrawlConfig {
                fetch_timeout: Duration::from_secs(60),
                ..CrawlConfig::default()
            },
        );

        assert_eq!(scaled.relay_batch_timeout(), Duration::from_secs(160));
        assert_eq!(capped.relay_batch_timeout(), Duration::from_secs(300));
    }

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

    #[test]
    fn per_kind_quota_prevents_busy_kinds_from_starving_other_kinds() {
        let events = vec![
            stored_event("1", 1, 30),
            stored_event("2", 1, 20),
            stored_event("3", 1, 10),
            stored_event("4", 5, 5),
        ];
        let shared = NostrBridge::new(
            Arc::new(MemoryStore::new()),
            CrawlConfig {
                per_author_event_limit: 2,
                ..CrawlConfig::default()
            },
        );
        let per_kind = NostrBridge::new(
            Arc::new(MemoryStore::new()),
            CrawlConfig {
                per_author_event_limit: 2,
                per_author_kind_event_limit: Some(2),
                ..CrawlConfig::default()
            },
        );

        let shared_kinds = shared
            .select_author_events(events.clone())
            .expect("shared selection")
            .into_iter()
            .map(|event| event.kind)
            .collect::<Vec<_>>();
        let per_kind_kinds = per_kind
            .select_author_events(events)
            .expect("per-kind selection")
            .into_iter()
            .map(|event| event.kind)
            .collect::<Vec<_>>();

        assert_eq!(shared_kinds, vec![1, 1]);
        assert_eq!(per_kind_kinds, vec![1, 1, 5]);
    }

    #[test]
    fn per_kind_queries_start_batched_by_kind_not_author() {
        let bridge = NostrBridge::new(
            Arc::new(MemoryStore::new()),
            CrawlConfig {
                per_author_kind_event_limit: Some(2),
                kinds: Some(vec![1, 5]),
                ..CrawlConfig::default()
            },
        );
        let pubkeys = (0..3)
            .map(|_| Keys::generate().public_key())
            .collect::<Vec<_>>();

        let queries = bridge
            .per_author_kind_queries(&pubkeys, &Default::default())
            .expect("per-kind queries");

        assert_eq!(queries.len(), 2);
        assert!(queries.iter().all(|query| query.authors.len() == 3));
    }

    #[test]
    fn per_kind_quota_can_retain_more_than_the_legacy_shared_limit() {
        let bridge = NostrBridge::new(
            Arc::new(MemoryStore::new()),
            CrawlConfig {
                per_author_event_limit: 3_000,
                per_author_kind_event_limit: Some(3_000),
                full_author_history: true,
                ..CrawlConfig::default()
            },
        );
        let events = (0..300)
            .map(|index| stored_event(&format!("{index:064x}"), 1, index))
            .collect();

        assert_eq!(
            bridge
                .select_author_events(events)
                .expect("full selection")
                .len(),
            300
        );
    }

    fn stored_event(id: &str, kind: u32, created_at: u64) -> StoredNostrEvent {
        StoredNostrEvent {
            id: id.to_string(),
            pubkey: "a".repeat(64),
            created_at,
            kind,
            tags: Vec::new(),
            content: String::new(),
            sig: "b".repeat(128),
        }
    }
}
