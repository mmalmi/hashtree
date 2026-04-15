use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use anyhow::{Context, Result};
use hashtree_nostr::{
    CrawlConfig, CrawlReport, ListEventsOptions, NostrBridge, NostrEventStore, RelayFetchMode,
};
use nostr::{
    Alphabet, Event, EventBuilder, Filter, Kind, PublicKey, SingleLetterTag, Tag, TagKind,
    Timestamp,
};
use nostr_sdk::{
    pool::RelayLimits, prelude::RelayPoolNotification, Client, Keys, Options, RelayStatus,
};
use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::socialgraph::crawler::SOCIALGRAPH_RELAY_EVENT_MAX_SIZE;
use crate::socialgraph::{self, SocialGraphBackend, SocialGraphStore};
use crate::HashtreeStore;

#[cfg(not(test))]
const MIRROR_STARTUP_DELAY: Duration = Duration::from_secs(8);
#[cfg(test)]
const MIRROR_STARTUP_DELAY: Duration = Duration::from_millis(50);

#[cfg(not(test))]
const MIRROR_CONNECT_SETTLE_DELAY: Duration = Duration::from_secs(1);
#[cfg(test)]
const MIRROR_CONNECT_SETTLE_DELAY: Duration = Duration::from_millis(250);

#[cfg(not(test))]
const MIRROR_AUTHOR_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
#[cfg(test)]
const MIRROR_AUTHOR_REFRESH_INTERVAL: Duration = Duration::from_millis(100);

#[cfg(not(test))]
const MIRROR_RECONNECT_HISTORY_SYNC_COOLDOWN: Duration = Duration::from_secs(30);
#[cfg(test)]
const MIRROR_RECONNECT_HISTORY_SYNC_COOLDOWN: Duration = Duration::from_millis(100);

const DEFAULT_HISTORY_KINDS: [u16; 6] = [0, 1, 3, 6, 7, 9735];
const DEFAULT_EVENT_TREE_NAME: &str = "nostr-event-index";
const DEFAULT_PROFILE_SEARCH_TREE_NAME: &str = "profile-search";

#[cfg(not(test))]
const MIRROR_MISSING_PROFILE_BACKFILL_INTERVAL: Duration = Duration::from_secs(300);
#[cfg(test)]
const MIRROR_MISSING_PROFILE_BACKFILL_INTERVAL: Duration = Duration::from_millis(100);

#[cfg(not(test))]
const MIRROR_ROOT_PUBLISH_DEBOUNCE: Duration = Duration::from_secs(5);
#[cfg(test)]
const MIRROR_ROOT_PUBLISH_DEBOUNCE: Duration = Duration::from_millis(20);

#[cfg(not(test))]
const MIRROR_ROOT_PUBLISH_MAX_STALENESS: Duration = Duration::from_secs(30);
#[cfg(test)]
const MIRROR_ROOT_PUBLISH_MAX_STALENESS: Duration = Duration::from_millis(100);

#[derive(Debug, Clone)]
pub struct NostrMirrorConfig {
    pub relays: Vec<String>,
    pub publish_relays: Vec<String>,
    pub max_follow_distance: u32,
    pub overmute_threshold: f64,
    pub author_batch_size: usize,
    pub history_sync_author_chunk_size: usize,
    pub history_sync_per_author_event_limit: usize,
    pub missing_profile_backfill_batch_size: usize,
    pub fetch_timeout: Duration,
    pub relay_event_max_size: Option<u32>,
    pub require_negentropy: bool,
    pub kinds: Vec<u16>,
    pub history_sync_on_start: bool,
    pub history_sync_on_reconnect: bool,
    pub published_event_tree_name: Option<String>,
    pub published_profile_search_tree_name: Option<String>,
}

impl Default for NostrMirrorConfig {
    fn default() -> Self {
        Self {
            relays: Vec::new(),
            publish_relays: Vec::new(),
            max_follow_distance: 2,
            overmute_threshold: 1.0,
            author_batch_size: 256,
            history_sync_author_chunk_size: 5_000,
            history_sync_per_author_event_limit: 256,
            missing_profile_backfill_batch_size: 5_000,
            fetch_timeout: Duration::from_secs(15),
            relay_event_max_size: Some(SOCIALGRAPH_RELAY_EVENT_MAX_SIZE),
            require_negentropy: false,
            kinds: DEFAULT_HISTORY_KINDS.to_vec(),
            history_sync_on_start: true,
            history_sync_on_reconnect: true,
            published_event_tree_name: Some(DEFAULT_EVENT_TREE_NAME.to_string()),
            published_profile_search_tree_name: Some(DEFAULT_PROFILE_SEARCH_TREE_NAME.to_string()),
        }
    }
}

#[derive(Debug, Default)]
struct RootPublishState {
    pending_root: Option<hashtree_core::Cid>,
    last_changed_at: Option<Instant>,
    dirty_since: Option<Instant>,
    last_published_root: Option<hashtree_core::Cid>,
    last_published_at: Option<Instant>,
}

pub struct BackgroundNostrMirror {
    config: NostrMirrorConfig,
    store: Arc<HashtreeStore>,
    graph_store: Arc<SocialGraphStore>,
    client: Client,
    publish_client: Option<Client>,
    event_publish_state: Mutex<RootPublishState>,
    profile_search_publish_state: Mutex<RootPublishState>,
    pending_live_events: Mutex<BTreeMap<String, Event>>,
    missing_profile_cursor: Mutex<usize>,
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
}

impl BackgroundNostrMirror {
    pub async fn new(
        config: NostrMirrorConfig,
        store: Arc<HashtreeStore>,
        graph_store: Arc<SocialGraphStore>,
        publish_keys: Option<Keys>,
    ) -> Result<Self> {
        let client = if let Some(max_size) = config.relay_event_max_size {
            let mut limits = RelayLimits::default();
            limits.events.max_size = Some(max_size);
            Client::with_opts(Keys::generate(), Options::new().relay_limits(limits))
        } else {
            Client::new(Keys::generate())
        };
        for relay in &config.relays {
            client
                .add_relay(relay)
                .await
                .with_context(|| format!("add mirror relay {relay}"))?;
        }
        client.connect().await;

        let publish_client = if let Some(keys) = publish_keys {
            if config.publish_relays.is_empty() {
                None
            } else {
                let client = Client::new(keys);
                for relay in &config.publish_relays {
                    client
                        .add_relay(relay)
                        .await
                        .with_context(|| format!("add mirror publish relay {relay}"))?;
                }
                client.connect().await;
                Some(client)
            }
        } else {
            None
        };

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        Ok(Self {
            config,
            store,
            graph_store,
            client,
            publish_client,
            event_publish_state: Mutex::new(RootPublishState::default()),
            profile_search_publish_state: Mutex::new(RootPublishState::default()),
            pending_live_events: Mutex::new(BTreeMap::new()),
            missing_profile_cursor: Mutex::new(0),
            shutdown_tx,
            shutdown_rx,
        })
    }

    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    fn sync_publish_roots_from_store(&self) -> Result<()> {
        self.note_public_events_root_change()?;
        self.note_profile_search_root_change()?;
        Ok(())
    }

    pub async fn run(&self) -> Result<()> {
        if self.config.relays.is_empty() || self.config.max_follow_distance == 0 {
            return Ok(());
        }

        info!(
            "Nostr mirror starting: relays={} max_follow_distance={} negentropy_only={} kinds={:?} history_sync_author_chunk_size={} history_sync_on_start={} history_sync_on_reconnect={}",
            self.config.relays.len(),
            self.config.max_follow_distance,
            self.config.require_negentropy,
            self.config.kinds,
            self.config.history_sync_author_chunk_size.max(1),
            self.config.history_sync_on_start,
            self.config.history_sync_on_reconnect
        );

        tokio::time::sleep(MIRROR_STARTUP_DELAY).await;
        tokio::time::sleep(MIRROR_CONNECT_SETTLE_DELAY).await;
        let live_since = Timestamp::now();
        self.sync_publish_roots_from_store()?;

        let initial_authors = self.collect_authors()?;
        if initial_authors.is_empty() {
            info!("Nostr mirror: no social-graph authors to mirror yet");
        } else if self.config.history_sync_on_start {
            if self.should_backfill_missing_profiles(None) {
                let missing_profile_authors = self.collect_missing_profile_authors(
                    self.config.missing_profile_backfill_batch_size,
                )?;
                if !missing_profile_authors.is_empty() {
                    info!(
                        "Nostr mirror missing-profile backfill starting: authors={}",
                        missing_profile_authors.len()
                    );
                    self.history_sync_authors_with_kinds(
                        missing_profile_authors,
                        &[Kind::Metadata.as_u16()],
                    )
                    .await?;
                }
            }
            self.history_sync_authors(initial_authors.clone()).await?;
        }

        let mut subscribed_authors = HashSet::new();
        self.subscribe_authors_since(&initial_authors, live_since, &mut subscribed_authors)
            .await?;

        let mut relay_statuses = self.capture_relay_statuses().await;
        let mut last_reconnect_history_sync_at: Option<Instant> = None;
        let mut last_missing_profile_backfill_at: Option<Instant> = None;
        let mut notifications = self.client.notifications();
        let mut shutdown_rx = self.shutdown_rx.clone();
        let mut refresh_interval = tokio::time::interval(MIRROR_AUTHOR_REFRESH_INTERVAL);
        let mut publish_interval = tokio::time::interval(MIRROR_ROOT_PUBLISH_DEBOUNCE);

        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                }
                _ = refresh_interval.tick() => {
                    let authors = self.collect_authors()?;
                    let new_authors = authors
                        .into_iter()
                        .filter(|author| !subscribed_authors.contains(author))
                        .collect::<Vec<_>>();
                    if !new_authors.is_empty() {
                        debug!(
                            "Nostr mirror discovered {} newly reachable author(s)",
                            new_authors.len()
                        );
                        self.history_sync_authors(new_authors.clone()).await?;
                        self.subscribe_authors_since(
                            &new_authors,
                            Timestamp::now(),
                            &mut subscribed_authors,
                        )
                        .await?;
                    }
                    if self.should_backfill_missing_profiles(last_missing_profile_backfill_at) {
                        let missing_profile_authors = self.collect_missing_profile_authors(
                            self.config.missing_profile_backfill_batch_size,
                        )?;
                        if !missing_profile_authors.is_empty() {
                            info!(
                                "Nostr mirror missing-profile backfill starting: authors={}",
                                missing_profile_authors.len()
                            );
                            self.history_sync_authors_with_kinds(
                                missing_profile_authors,
                                &[Kind::Metadata.as_u16()],
                            )
                            .await?;
                            last_missing_profile_backfill_at = Some(Instant::now());
                        }
                    }
                }
                _ = publish_interval.tick() => {
                    self.sync_publish_roots_from_store()?;
                    if let Err(err) = self.flush_live_events().await {
                        warn!("Nostr mirror live event flush failed: {:#}", err);
                    }
                    if let Err(err) = self.maybe_publish_event_root(false).await {
                        warn!("Nostr mirror event-root publish failed: {:#}", err);
                    }
                    if let Err(err) = self.maybe_publish_profile_search_root(false).await {
                        warn!("Nostr mirror profile-search publish failed: {:#}", err);
                    }
                }
                notification = notifications.recv() => {
                    match notification {
                        Ok(RelayPoolNotification::Event { event, .. }) => {
                            self.ingest_live_event(&event)?;
                        }
                        Ok(RelayPoolNotification::RelayStatus { relay_url, status }) => {
                            let relay_url = relay_url.to_string();
                            let previous = relay_statuses.insert(relay_url.clone(), status);
                            if Self::should_history_sync_on_reconnect(
                                self.config.history_sync_on_reconnect,
                                previous,
                                status,
                            ) && Self::should_run_reconnect_history_sync(
                                    last_reconnect_history_sync_at.as_ref(),
                                )
                            {
                                let authors = self.collect_authors()?;
                                if !authors.is_empty() {
                                    info!(
                                        "Nostr mirror relay reconnected; running catch-up history sync: relay={} authors={} negentropy_only={}",
                                        relay_url,
                                        authors.len(),
                                        self.config.require_negentropy
                                    );
                                    self.history_sync_authors(authors).await?;
                                    last_reconnect_history_sync_at = Some(Instant::now());
                                }
                            }
                        }
                        Ok(RelayPoolNotification::Shutdown) => break,
                        Ok(_) => {}
                        Err(err) => {
                            warn!("Nostr mirror notification error: {}", err);
                            break;
                        }
                    }
                }
            }
        }

        if let Err(err) = self.flush_live_events().await {
            warn!(
                "Nostr mirror live event flush failed during shutdown: {:#}",
                err
            );
        }
        if let Err(err) = self.sync_publish_roots_from_store() {
            warn!(
                "Nostr mirror root-state refresh failed during shutdown: {:#}",
                err
            );
        }
        if let Err(err) = self.maybe_publish_event_root(true).await {
            warn!(
                "Nostr mirror event-root publish failed during shutdown: {:#}",
                err
            );
        }
        if let Err(err) = self.maybe_publish_profile_search_root(true).await {
            warn!(
                "Nostr mirror profile-search publish failed during shutdown: {:#}",
                err
            );
        }
        let _ = self.client.disconnect().await;
        if let Some(client) = self.publish_client.as_ref() {
            let _ = client.disconnect().await;
        }
        Ok(())
    }

    async fn capture_relay_statuses(&self) -> HashMap<String, RelayStatus> {
        let mut statuses = HashMap::new();
        for (relay_url, relay) in self.client.relays().await {
            statuses.insert(relay_url.to_string(), relay.status().await);
        }
        statuses
    }

    async fn has_connected_publish_relay(&self) -> bool {
        let Some(client) = self.publish_client.as_ref() else {
            return false;
        };
        Self::client_has_connected_relay(client).await
    }

    async fn client_has_connected_relay(client: &Client) -> bool {
        for (_relay_url, relay) in client.relays().await {
            if relay.status().await == RelayStatus::Connected {
                return true;
            }
        }
        false
    }

    fn collect_authors(&self) -> Result<Vec<String>> {
        let mut authors = Vec::new();
        let mut seen = HashSet::new();
        for distance in 0..=self.config.max_follow_distance {
            for pubkey in socialgraph::SocialGraphBackend::users_by_follow_distance(
                self.graph_store.as_ref(),
                distance,
            )
            .with_context(|| format!("load social-graph distance {distance}"))?
            {
                if self
                    .graph_store
                    .is_overmuted_user(&pubkey, self.config.overmute_threshold)?
                {
                    continue;
                }
                let hex = hex::encode(pubkey);
                if seen.insert(hex.clone()) {
                    authors.push(hex);
                }
            }
        }
        Ok(authors)
    }

    fn collect_missing_profile_authors(&self, limit: usize) -> Result<Vec<String>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let authors = self.collect_authors()?;
        if authors.is_empty() {
            return Ok(Vec::new());
        }

        let mut cursor = self
            .missing_profile_cursor
            .lock()
            .expect("missing profile cursor");
        let mut index = (*cursor).min(authors.len());
        let mut scanned = 0usize;
        let mut missing = Vec::new();

        while scanned < authors.len() && missing.len() < limit {
            let author = &authors[index];
            if self.graph_store.latest_profile_event(author)?.is_none() {
                missing.push(author.clone());
            }
            index += 1;
            if index == authors.len() {
                index = 0;
            }
            scanned += 1;
        }

        *cursor = index;
        Ok(missing)
    }

    fn should_backfill_missing_profiles(&self, last_run: Option<Instant>) -> bool {
        if self.config.missing_profile_backfill_batch_size == 0
            || !self.config.kinds.contains(&Kind::Metadata.as_u16())
        {
            return false;
        }
        match last_run {
            Some(last_run) => last_run.elapsed() >= MIRROR_MISSING_PROFILE_BACKFILL_INTERVAL,
            None => true,
        }
    }

    fn should_history_sync_on_reconnect(
        history_sync_on_reconnect: bool,
        previous: Option<RelayStatus>,
        status: RelayStatus,
    ) -> bool {
        history_sync_on_reconnect
            && status == RelayStatus::Connected
            && matches!(
                previous,
                Some(
                    RelayStatus::Initialized
                        | RelayStatus::Pending
                        | RelayStatus::Connecting
                        | RelayStatus::Disconnected
                        | RelayStatus::Terminated
                )
            )
    }

    fn should_run_reconnect_history_sync(last_run: Option<&Instant>) -> bool {
        match last_run {
            None => true,
            Some(last_run) => last_run.elapsed() >= MIRROR_RECONNECT_HISTORY_SYNC_COOLDOWN,
        }
    }

    async fn history_sync_authors(&self, authors: Vec<String>) -> Result<()> {
        self.history_sync_authors_with_kinds(authors, &self.config.kinds)
            .await
    }

    async fn history_sync_authors_with_kinds(
        &self,
        authors: Vec<String>,
        kinds: &[u16],
    ) -> Result<()> {
        self.history_sync_authors_chunked(authors, |current_root, author_chunk| async move {
            self.history_sync_author_chunk(current_root, author_chunk, kinds)
                .await
        })
        .await
    }

    async fn history_sync_authors_chunked<F, Fut>(
        &self,
        authors: Vec<String>,
        mut run_chunk: F,
    ) -> Result<()>
    where
        F: FnMut(Option<hashtree_core::Cid>, Vec<String>) -> Fut,
        Fut: std::future::Future<Output = Result<CrawlReport>>,
    {
        if authors.is_empty() {
            return Ok(());
        }

        info!(
            "Nostr mirror history sync starting: authors={} relays={} negentropy_only={}",
            authors.len(),
            self.config.relays.len(),
            self.config.require_negentropy
        );

        let mut current_root = self.graph_store.public_events_root()?;
        let mut last_error = None;
        let mut applied_chunks = 0usize;
        let mut failed_chunks = 0usize;
        let chunk_size = self.config.history_sync_author_chunk_size.max(1);
        let total_chunks = authors.len().div_ceil(chunk_size);

        for (chunk_index, author_chunk) in authors.chunks(chunk_size).enumerate() {
            let author_chunk = author_chunk.to_vec();
            let author_count = author_chunk.len();
            info!(
                "Nostr mirror history sync chunk starting: chunk={}/{} authors={}",
                chunk_index + 1,
                total_chunks,
                author_count
            );
            let report = match run_chunk(current_root.clone(), author_chunk).await {
                Ok(report) => report,
                Err(err) => {
                    failed_chunks = failed_chunks.saturating_add(1);
                    warn!(
                        "Nostr mirror history sync chunk failed: chunk={}/{} authors={} error={:#}",
                        chunk_index + 1,
                        total_chunks,
                        author_count,
                        err
                    );
                    last_error = Some(err);
                    continue;
                }
            };

            if report.root != current_root {
                self.apply_history_root(report.root.as_ref()).await?;
                current_root = report.root.clone();
                info!(
                    "Nostr mirror history sync updated trusted root: chunk={}/{} authors_processed={} events_selected={} events_seen={}",
                    chunk_index + 1,
                    total_chunks,
                    report.authors_processed,
                    report.events_selected,
                    report.events_seen
                );
            }
            applied_chunks = applied_chunks.saturating_add(1);
        }

        if applied_chunks == 0 {
            return Err(last_error
                .unwrap_or_else(|| anyhow::anyhow!("mirror history sync made no progress"))
                .context("run mirror history sync"));
        }
        if failed_chunks > 0 {
            warn!(
                "Nostr mirror history sync completed with skipped chunks: applied_chunks={} failed_chunks={}",
                applied_chunks,
                failed_chunks
            );
        }
        Ok(())
    }

    async fn history_sync_author_chunk(
        &self,
        current_root: Option<hashtree_core::Cid>,
        authors: Vec<String>,
        kinds: &[u16],
    ) -> Result<CrawlReport> {
        let mut last_error = None;
        let mut report = None;
        for attempt in 0..3 {
            let mut last_logged_authors = 0usize;
            let bridge = NostrBridge::new(
                self.store.store_arc(),
                CrawlConfig {
                    relays: self.config.relays.clone(),
                    author_allowlist: Some(authors.clone()),
                    max_live_bytes: None,
                    max_events_seen: None,
                    max_authors: None,
                    max_follow_distance: None,
                    author_batch_size: self.config.author_batch_size.max(1),
                    per_author_event_limit: self.config.history_sync_per_author_event_limit.max(1),
                    per_author_live_bytes: None,
                    fetch_timeout: self.config.fetch_timeout,
                    kinds: Some(kinds.to_vec()),
                    relay_fetch_mode: RelayFetchMode::AuthorBatches,
                    require_negentropy: self.config.require_negentropy,
                    relay_event_max_size: self.config.relay_event_max_size,
                    relay_page_size: 1_000,
                    max_relay_pages: 10,
                },
            );

            match bridge
                .crawl_with_progress(self.graph_store.as_ref(), current_root.as_ref(), |progress| {
                    let log_interval = self.config.author_batch_size.saturating_mul(8).max(2_048);
                    let should_log = progress.authors_processed == progress.authors_considered
                        || progress.authors_processed == 0
                        || progress
                            .authors_processed
                            .saturating_sub(last_logged_authors)
                            >= log_interval;
                    if should_log {
                        last_logged_authors = progress.authors_processed;
                        info!(
                            "Nostr mirror history sync progress: authors_processed={}/{} events_selected={} events_seen={}",
                            progress.authors_processed,
                            progress.authors_considered,
                            progress.events_selected,
                            progress.events_seen
                        );
                    }
                })
                .await
            {
                Ok(next_report) => {
                    report = Some(next_report);
                    break;
                }
                Err(err) => {
                    last_error = Some(err);
                    if attempt < 2 {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                }
            }
        }
        report
            .ok_or_else(|| last_error.expect("history sync retry captured error"))
            .context("run mirror history sync")
    }

    async fn apply_history_root(&self, root: Option<&hashtree_core::Cid>) -> Result<()> {
        self.graph_store.write_public_events_root(root)?;
        let Some(root) = root else {
            return Ok(());
        };

        let event_store = NostrEventStore::new(self.store.store_arc());
        let events = event_store
            .list_recent_lossy(Some(root), ListEventsOptions::default())
            .await
            .context("list trusted mirrored events")?
            .into_iter()
            .map(socialgraph::stored_event_to_nostr_event)
            .collect::<Result<Vec<_>>>()?;

        self.graph_store
            .rebuild_profile_index_for_events(&events)
            .context("rebuild mirrored profile search index")?;
        socialgraph::ingest_graph_parsed_events(self.graph_store.as_ref(), &events)
            .context("sync mirrored social graph state")?;
        self.note_public_events_root_change()?;
        self.note_profile_search_root_change()?;
        if let Err(err) = self.maybe_publish_event_root(true).await {
            warn!(
                "Nostr mirror event-root publish failed after root update: {:#}",
                err
            );
        }
        if let Err(err) = self.maybe_publish_profile_search_root(false).await {
            warn!(
                "Nostr mirror profile-search publish failed after root update: {:#}",
                err
            );
        }
        Ok(())
    }

    async fn subscribe_authors_since(
        &self,
        authors: &[String],
        since: Timestamp,
        subscribed_authors: &mut HashSet<String>,
    ) -> Result<()> {
        let new_authors = authors
            .iter()
            .filter(|author| !subscribed_authors.contains(*author))
            .cloned()
            .collect::<Vec<_>>();
        if new_authors.is_empty() {
            return Ok(());
        }

        for chunk in new_authors.chunks(self.config.author_batch_size.max(1)) {
            let pubkeys = chunk
                .iter()
                .filter_map(|author| PublicKey::from_hex(author).ok())
                .collect::<Vec<_>>();
            if pubkeys.is_empty() {
                continue;
            }

            let filter = Filter::new()
                .authors(pubkeys)
                .kinds(self.config.kinds.iter().copied().map(Kind::from))
                .since(since);

            self.client
                .subscribe(vec![filter], None)
                .await
                .context("subscribe mirror author batch")?;
        }

        subscribed_authors.extend(new_authors);
        Ok(())
    }

    fn ingest_live_event(&self, event: &Event) -> Result<()> {
        self.pending_live_events
            .lock()
            .expect("pending live events")
            .insert(event.id.to_hex(), event.clone());
        Ok(())
    }

    async fn flush_live_events(&self) -> Result<()> {
        let pending = {
            let mut pending = self
                .pending_live_events
                .lock()
                .expect("pending live events");
            if pending.is_empty() {
                return Ok(());
            }
            std::mem::take(&mut *pending)
        };
        let events = pending.into_values().collect::<Vec<_>>();
        let event_count = events.len();
        let previous_event_root = self.graph_store.public_events_root()?;
        let previous_profile_search_root = self.graph_store.profile_search_root()?;

        socialgraph::ingest_parsed_events_with_storage_class(
            self.graph_store.as_ref(),
            &events,
            socialgraph::EventStorageClass::Public,
        )
        .context("ingest live mirrored event batch")?;

        let next_event_root = self.graph_store.public_events_root()?;
        let next_profile_search_root = self.graph_store.profile_search_root()?;
        let event_root_changed = next_event_root != previous_event_root;
        let profile_search_root_changed = next_profile_search_root != previous_profile_search_root;

        if event_root_changed {
            self.note_public_events_root_change()?;
        }
        if profile_search_root_changed {
            self.note_profile_search_root_change()?;
        }
        if event_root_changed {
            self.maybe_publish_event_root(true).await?;
        }
        if profile_search_root_changed {
            self.maybe_publish_profile_search_root(true).await?;
        }

        info!(
            "Nostr mirror flushed live events: events={} event_root_changed={} profile_search_root_changed={}",
            event_count, event_root_changed, profile_search_root_changed
        );
        Ok(())
    }

    fn note_public_events_root_change(&self) -> Result<()> {
        let root = self.graph_store.public_events_root()?;
        Self::note_root_change(
            self.config.published_event_tree_name.as_deref(),
            &self.event_publish_state,
            root,
        )
    }

    fn note_profile_search_root_change(&self) -> Result<()> {
        let root = self.graph_store.profile_search_root()?;
        Self::note_root_change(
            self.config.published_profile_search_tree_name.as_deref(),
            &self.profile_search_publish_state,
            root,
        )
    }

    fn note_root_change(
        tree_name: Option<&str>,
        publish_state: &Mutex<RootPublishState>,
        root: Option<hashtree_core::Cid>,
    ) -> Result<()> {
        let Some(_tree_name) = tree_name else {
            return Ok(());
        };

        let mut state = publish_state.lock().expect("root publish state");
        let now = Instant::now();

        if state.pending_root == root {
            return Ok(());
        }

        state.pending_root = root;
        state.last_changed_at = Some(now);
        if state.dirty_since.is_none() {
            state.dirty_since = Some(now);
        }
        Ok(())
    }

    async fn maybe_publish_event_root(&self, force: bool) -> Result<()> {
        self.maybe_publish_root(
            self.config.published_event_tree_name.as_deref(),
            &self.event_publish_state,
            "event root",
            force,
        )
        .await
    }

    async fn maybe_publish_profile_search_root(&self, force: bool) -> Result<()> {
        self.maybe_publish_root(
            self.config.published_profile_search_tree_name.as_deref(),
            &self.profile_search_publish_state,
            "profile search root",
            force,
        )
        .await
    }

    async fn maybe_publish_root(
        &self,
        tree_name: Option<&str>,
        publish_state: &Mutex<RootPublishState>,
        log_label: &str,
        force: bool,
    ) -> Result<()> {
        let Some(tree_name) = tree_name else {
            return Ok(());
        };
        let Some(publish_client) = self.publish_client.as_ref() else {
            return Ok(());
        };
        if !self.has_connected_publish_relay().await {
            return Ok(());
        }

        let pending_root = {
            let state = publish_state.lock().expect("root publish state");
            let Some(pending_root) = state.pending_root.clone() else {
                return Ok(());
            };
            if state.last_published_root.as_ref() == Some(&pending_root) {
                return Ok(());
            }

            let now = Instant::now();
            let debounce_ready = state.last_changed_at.is_some_and(|changed_at| {
                now.duration_since(changed_at) >= MIRROR_ROOT_PUBLISH_DEBOUNCE
            });
            let stale_ready = state.dirty_since.is_some_and(|dirty_since| {
                now.duration_since(dirty_since) >= MIRROR_ROOT_PUBLISH_MAX_STALENESS
            });
            if !force && !debounce_ready && !stale_ready {
                return Ok(());
            }

            pending_root
        };

        let event = Self::build_public_root_event(tree_name, &pending_root);
        let output = publish_client
            .send_event_builder(event)
            .await
            .with_context(|| format!("publish {log_label} event"))?;
        if output.failed.is_empty() && output.success.is_empty() {
            return Ok(());
        }

        {
            let mut state = publish_state.lock().expect("root publish state");
            if state.pending_root.as_ref() == Some(&pending_root) {
                state.last_published_root = Some(pending_root.clone());
                state.last_published_at = Some(Instant::now());
                state.dirty_since = None;
            }
        }

        info!(
            "Nostr mirror published {}: tree={} hash={}",
            log_label,
            tree_name,
            hex::encode(pending_root.hash)
        );
        Ok(())
    }

    fn build_public_root_event(tree_name: &str, cid: &hashtree_core::Cid) -> EventBuilder {
        let mut tags = vec![
            Tag::identifier(tree_name.to_string()),
            Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::L)),
                vec!["hashtree"],
            ),
            Tag::custom(TagKind::Custom("hash".into()), vec![hex::encode(cid.hash)]),
        ];
        if let Some(key) = cid.key {
            tags.push(Tag::custom(
                TagKind::Custom("key".into()),
                vec![hex::encode(key)],
            ));
        }

        EventBuilder::new(Kind::Custom(30078), "", tags)
    }
}

#[cfg(test)]
mod tests;
