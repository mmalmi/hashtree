use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
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
use tokio::sync::{watch, Mutex as AsyncMutex};
use tracing::{debug, info, warn};

use crate::blossom_push::background_blossom_push_incremental_with_store;
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

const KIND_LONG_FORM_CONTENT: u16 = 30_023;
const DEFAULT_HISTORY_KINDS: [u16; 7] = [0, 1, 3, 6, 7, 9735, KIND_LONG_FORM_CONTENT];
const DEFAULT_EVENT_TREE_NAME: &str = "nostr-event-index";
const DEFAULT_PROFILE_SEARCH_TREE_NAME: &str = "profile-search";
const DEFAULT_PROFILES_BY_PUBKEY_TREE_NAME: &str = "profiles-by-pubkey";
const MIRROR_UPLOAD_STATE_DIR: &str = "nostr-mirror";
const MIRROR_UPLOADED_ROOT_SUFFIX: &str = ".uploaded-root";
const METADATA_HISTORY_SYNC_PER_AUTHOR_EVENT_LIMIT: usize = 1;
const METADATA_HISTORY_SYNC_AUTHOR_BATCH_SIZE: usize = 64;
const DEFAULT_FULL_TEXT_NOTE_HISTORY_FOLLOW_DISTANCE: u32 = 2;
const DEFAULT_FULL_TEXT_NOTE_HISTORY_MAX_RELAY_PAGES: usize = 0;
const FULL_TEXT_HISTORY_PRIORITY_MAX_DISTANCE: u32 = 1;
const FULL_TEXT_HISTORY_PRIORITY_SAMPLE_LIMIT: usize = 32;
const LARGE_HISTORY_SYNC_AUTHOR_MULTIPLIER: usize = 8;
const LARGE_HISTORY_SYNC_PER_AUTHOR_EVENT_LIMIT: usize = 16;
const LARGE_HISTORY_SYNC_MAX_RELAY_PAGES: usize = 20;

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

#[cfg(not(test))]
const MIRROR_ROOT_UPLOAD_RETRY_INTERVAL: Duration = Duration::from_secs(60);
#[cfg(test)]
const MIRROR_ROOT_UPLOAD_RETRY_INTERVAL: Duration = Duration::from_millis(100);

#[cfg(not(test))]
const MIRROR_ROOT_PUBLISH_PRIMARY_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(test)]
const MIRROR_ROOT_PUBLISH_PRIMARY_TIMEOUT: Duration = Duration::from_millis(250);

#[cfg(not(test))]
const MIRROR_ROOT_PUBLISH_RETRY_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(test)]
const MIRROR_ROOT_PUBLISH_RETRY_TIMEOUT: Duration = Duration::from_secs(2);

const MISSING_LOCAL_BLOB_PUSH_ERROR: &str = "missing local blob";

fn decode_hex_pubkey(value: &str) -> Option<[u8; 32]> {
    let bytes = hex::decode(value).ok()?;
    <[u8; 32]>::try_from(bytes.as_slice()).ok()
}

#[derive(Debug, Clone)]
pub struct NostrMirrorConfig {
    pub relays: Vec<String>,
    pub publish_relays: Vec<String>,
    pub blossom_write_servers: Vec<String>,
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
    pub full_text_note_history_follow_distance: Option<u32>,
    pub full_text_note_history_max_relay_pages: usize,
    pub published_event_tree_name: Option<String>,
    pub published_profile_search_tree_name: Option<String>,
    pub published_profiles_by_pubkey_tree_name: Option<String>,
}

impl Default for NostrMirrorConfig {
    fn default() -> Self {
        Self {
            relays: Vec::new(),
            publish_relays: Vec::new(),
            blossom_write_servers: Vec::new(),
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
            full_text_note_history_follow_distance: Some(
                DEFAULT_FULL_TEXT_NOTE_HISTORY_FOLLOW_DISTANCE,
            ),
            full_text_note_history_max_relay_pages: DEFAULT_FULL_TEXT_NOTE_HISTORY_MAX_RELAY_PAGES,
            published_event_tree_name: Some(DEFAULT_EVENT_TREE_NAME.to_string()),
            published_profile_search_tree_name: Some(DEFAULT_PROFILE_SEARCH_TREE_NAME.to_string()),
            published_profiles_by_pubkey_tree_name: Some(
                DEFAULT_PROFILES_BY_PUBKEY_TREE_NAME.to_string(),
            ),
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
    last_published_created_at: Option<Timestamp>,
    last_uploaded_root: Option<hashtree_core::Cid>,
    last_uploaded_at: Option<Instant>,
    upload_in_progress_root: Option<hashtree_core::Cid>,
    last_upload_failed_at: Option<Instant>,
    last_upload_error: Option<String>,
    missing_blob_rebuild_required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HistorySyncPlan {
    relay_fetch_mode: RelayFetchMode,
    author_batch_size: usize,
    per_author_event_limit: usize,
    relay_page_size: usize,
    max_relay_pages: usize,
}

pub struct BackgroundNostrMirror {
    config: NostrMirrorConfig,
    store: Arc<HashtreeStore>,
    graph_store: Arc<SocialGraphStore>,
    client: Client,
    publish_client: Option<Client>,
    event_publish_state: Arc<Mutex<RootPublishState>>,
    profile_search_publish_state: Arc<Mutex<RootPublishState>>,
    profiles_by_pubkey_publish_state: Arc<Mutex<RootPublishState>>,
    pending_live_events: Mutex<BTreeMap<String, Event>>,
    missing_profile_cursor: Mutex<usize>,
    history_sync_lock: AsyncMutex<()>,
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
                let client = Client::with_opts(keys, Options::new().wait_for_send(false));
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
        let event_publish_state = Self::root_publish_state_from_disk(
            store.base_path(),
            config.published_event_tree_name.as_deref(),
            "event root",
        );
        let profile_search_publish_state = Self::root_publish_state_from_disk(
            store.base_path(),
            config.published_profile_search_tree_name.as_deref(),
            "profile-search root",
        );
        let profiles_by_pubkey_publish_state = Self::root_publish_state_from_disk(
            store.base_path(),
            config.published_profiles_by_pubkey_tree_name.as_deref(),
            "profiles-by-pubkey root",
        );
        Ok(Self {
            config,
            store,
            graph_store,
            client,
            publish_client,
            event_publish_state: Arc::new(Mutex::new(event_publish_state)),
            profile_search_publish_state: Arc::new(Mutex::new(profile_search_publish_state)),
            profiles_by_pubkey_publish_state: Arc::new(Mutex::new(
                profiles_by_pubkey_publish_state,
            )),
            pending_live_events: Mutex::new(BTreeMap::new()),
            missing_profile_cursor: Mutex::new(0),
            history_sync_lock: AsyncMutex::new(()),
            shutdown_tx,
            shutdown_rx,
        })
    }

    fn root_publish_state_from_disk(
        base_path: &std::path::Path,
        tree_name: Option<&str>,
        log_label: &str,
    ) -> RootPublishState {
        let Some(tree_name) = tree_name else {
            return RootPublishState::default();
        };
        let path = Self::uploaded_root_state_path(base_path, tree_name);
        let Some(root) = Self::read_uploaded_root_state(&path, log_label) else {
            return RootPublishState::default();
        };

        RootPublishState {
            last_uploaded_root: Some(root),
            ..RootPublishState::default()
        }
    }

    fn uploaded_root_state_path(base_path: &std::path::Path, tree_name: &str) -> PathBuf {
        let safe_tree_name = tree_name
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                    ch
                } else {
                    '_'
                }
            })
            .collect::<String>();
        base_path
            .join(MIRROR_UPLOAD_STATE_DIR)
            .join(format!("{safe_tree_name}{MIRROR_UPLOADED_ROOT_SUFFIX}"))
    }

    fn read_uploaded_root_state(
        path: &std::path::Path,
        log_label: &str,
    ) -> Option<hashtree_core::Cid> {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
            Err(err) => {
                warn!(
                    "Nostr mirror failed to read uploaded {} state {}: {}",
                    log_label,
                    path.display(),
                    err
                );
                return None;
            }
        };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        match hashtree_core::Cid::parse(trimmed) {
            Ok(root) => Some(root),
            Err(err) => {
                warn!(
                    "Nostr mirror ignored invalid uploaded {} state {}: {}",
                    log_label,
                    path.display(),
                    err
                );
                None
            }
        }
    }

    fn write_uploaded_root_state(
        path: &std::path::Path,
        root: &hashtree_core::Cid,
        log_label: &str,
    ) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        std::fs::write(path, format!("{root}\n"))
            .with_context(|| format!("write uploaded {log_label} state {}", path.display()))
    }

    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    fn sync_publish_roots_from_store(&self) -> Result<()> {
        self.note_public_events_root_change()?;
        self.note_profile_search_root_change()?;
        self.note_profiles_by_pubkey_root_change()?;
        Ok(())
    }

    async fn publish_pending_roots(
        &self,
        force_event: bool,
        force_profile_search: bool,
        force_profiles_by_pubkey: bool,
    ) -> (Result<()>, Result<()>, Result<()>) {
        tokio::join!(
            self.maybe_publish_event_root(force_event),
            self.maybe_publish_profile_search_root(force_profile_search),
            self.maybe_publish_profiles_by_pubkey_root(force_profiles_by_pubkey),
        )
    }

    async fn publish_priority_roots(
        &self,
        force_event: bool,
        force_profile_search: bool,
        force_profiles_by_pubkey: bool,
    ) -> (Result<()>, Result<()>, Result<()>) {
        let (profile_search_result, profiles_by_pubkey_result) = tokio::join!(
            async {
                if force_profile_search {
                    self.maybe_publish_profile_search_root(true).await
                } else {
                    Ok(())
                }
            },
            async {
                if force_profiles_by_pubkey {
                    self.maybe_publish_profiles_by_pubkey_root(true).await
                } else {
                    Ok(())
                }
            },
        );
        let event_result = if force_event {
            self.maybe_publish_event_root(true).await
        } else {
            Ok(())
        };
        (
            event_result,
            profile_search_result,
            profiles_by_pubkey_result,
        )
    }

    pub async fn run(self: Arc<Self>) -> Result<()> {
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
        let (event_result, profile_search_result, profiles_by_pubkey_result) =
            self.publish_priority_roots(true, true, true).await;
        if let Err(err) = event_result {
            warn!(
                "Nostr mirror event-root publish failed on startup: {:#}",
                err
            );
        }
        if let Err(err) = profile_search_result {
            warn!(
                "Nostr mirror profile-search publish failed on startup: {:#}",
                err
            );
        }
        if let Err(err) = profiles_by_pubkey_result {
            warn!(
                "Nostr mirror profiles-by-pubkey publish failed on startup: {:#}",
                err
            );
        }

        let initial_authors = self.collect_authors()?;
        if initial_authors.is_empty() {
            info!("Nostr mirror: no social-graph authors to mirror yet");
        }

        let mut subscribed_authors = HashSet::new();
        self.subscribe_authors_since(&initial_authors, live_since, &mut subscribed_authors)
            .await?;

        if !initial_authors.is_empty() && self.config.history_sync_on_start {
            self.spawn_startup_history_sync(initial_authors.clone());
        }

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
                        self.subscribe_authors_since(
                            &new_authors,
                            Timestamp::now(),
                            &mut subscribed_authors,
                        )
                        .await?;
                        self.spawn_author_history_sync(
                            "new-author catch-up",
                            new_authors.clone(),
                            true,
                            true,
                        );
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
                            self.spawn_missing_profile_backfill(missing_profile_authors);
                            last_missing_profile_backfill_at = Some(Instant::now());
                        }
                    }
                }
                _ = publish_interval.tick() => {
                    self.sync_publish_roots_from_store()?;
                    if let Err(err) = self.flush_live_events().await {
                        warn!("Nostr mirror live event flush failed: {:#}", err);
                    }
                    let (event_result, profile_search_result, profiles_by_pubkey_result) = self
                        .publish_pending_roots(false, false, false)
                        .await;
                    if let Err(err) = event_result {
                        warn!("Nostr mirror event-root publish failed: {:#}", err);
                    }
                    if let Err(err) = profile_search_result {
                        warn!("Nostr mirror profile-search publish failed: {:#}", err);
                    }
                    if let Err(err) = profiles_by_pubkey_result {
                        warn!("Nostr mirror profiles-by-pubkey publish failed: {:#}", err);
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
                                    self.spawn_author_history_sync(
                                        "relay reconnect catch-up",
                                        authors,
                                        false,
                                        false,
                                    );
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
        let (event_result, profile_search_result, profiles_by_pubkey_result) =
            self.publish_pending_roots(true, true, true).await;
        if let Err(err) = event_result {
            warn!(
                "Nostr mirror event-root publish failed during shutdown: {:#}",
                err
            );
        }
        if let Err(err) = profile_search_result {
            warn!(
                "Nostr mirror profile-search publish failed during shutdown: {:#}",
                err
            );
        }
        if let Err(err) = profiles_by_pubkey_result {
            warn!(
                "Nostr mirror profiles-by-pubkey publish failed during shutdown: {:#}",
                err
            );
        }
        let _ = self.client.disconnect().await;
        if let Some(client) = self.publish_client.as_ref() {
            let _ = client.disconnect().await;
        }
        Ok(())
    }

    fn spawn_startup_history_sync(self: &Arc<Self>, initial_authors: Vec<String>) {
        let mirror = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build nostr mirror startup history sync runtime");
            runtime.block_on(async move {
                let _guard = mirror.history_sync_lock.lock().await;
                if let Err(err) = mirror.run_startup_history_sync(initial_authors).await {
                    warn!("Nostr mirror startup history sync failed: {:#}", err);
                }
            });
        });
    }

    async fn run_startup_history_sync(&self, initial_authors: Vec<String>) -> Result<()> {
        self.history_sync_full_text_notes_for_reachable_authors()
            .await?;
        self.history_sync_authors(initial_authors).await?;
        if self.should_backfill_missing_profiles(None) {
            let missing_profile_authors = self
                .collect_missing_profile_authors(self.config.missing_profile_backfill_batch_size)?;
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
        Ok(())
    }

    fn spawn_author_history_sync(
        self: &Arc<Self>,
        label: &'static str,
        authors: Vec<String>,
        include_full_text_notes: bool,
        wait_for_existing_sync: bool,
    ) {
        let mirror = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build nostr mirror author history sync runtime");
            runtime.block_on(async move {
                if wait_for_existing_sync {
                    let _guard = mirror.history_sync_lock.lock().await;
                    if let Err(err) = mirror
                        .run_author_history_sync(authors, include_full_text_notes)
                        .await
                    {
                        warn!("Nostr mirror {label} failed: {:#}", err);
                    }
                    return;
                }

                let Ok(_guard) = mirror.history_sync_lock.try_lock() else {
                    info!("Nostr mirror {label} skipped; another history sync is running");
                    return;
                };
                if let Err(err) = mirror
                    .run_author_history_sync(authors, include_full_text_notes)
                    .await
                {
                    warn!("Nostr mirror {label} failed: {:#}", err);
                }
            });
        });
    }

    async fn run_author_history_sync(
        &self,
        authors: Vec<String>,
        include_full_text_notes: bool,
    ) -> Result<()> {
        if include_full_text_notes {
            self.history_sync_full_text_notes_for_authors(authors.clone())
                .await?;
        }
        self.history_sync_authors(authors).await
    }

    fn spawn_missing_profile_backfill(self: &Arc<Self>, authors: Vec<String>) {
        let mirror = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build nostr mirror missing profile runtime");
            runtime.block_on(async move {
                let Ok(_guard) = mirror.history_sync_lock.try_lock() else {
                    info!(
                        "Nostr mirror missing-profile backfill skipped; another history sync is running"
                    );
                    return;
                };
                if let Err(err) = mirror
                    .history_sync_authors_with_kinds(authors, &[Kind::Metadata.as_u16()])
                    .await
                {
                    warn!("Nostr mirror missing-profile backfill failed: {:#}", err);
                }
            });
        });
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
        self.collect_authors_with_max_distance(self.config.max_follow_distance)
    }

    fn collect_authors_with_max_distance(&self, max_distance: u32) -> Result<Vec<String>> {
        let mut authors = Vec::new();
        let mut seen = HashSet::new();
        for distance in 0..=max_distance {
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

    async fn prioritize_full_text_note_history_authors(
        &self,
        authors: Vec<String>,
    ) -> Result<Vec<String>> {
        let Some(root) = self.graph_store.public_events_root()? else {
            return Ok(authors);
        };

        let event_store = NostrEventStore::new(self.store.store_arc());
        let mut prioritized = Vec::with_capacity(authors.len());
        let mut sampled = 0usize;
        for (index, author) in authors.into_iter().enumerate() {
            let distance = match decode_hex_pubkey(&author) {
                Some(pubkey) => self
                    .graph_store
                    .follow_distance(&pubkey)?
                    .unwrap_or(u32::MAX),
                None => u32::MAX,
            };
            let indexed_text_sample = if distance <= FULL_TEXT_HISTORY_PRIORITY_MAX_DISTANCE {
                sampled = sampled.saturating_add(1);
                event_store
                    .list_by_author_and_kind(
                        Some(&root),
                        &author,
                        Kind::TextNote.as_u16() as u32,
                        ListEventsOptions {
                            limit: Some(FULL_TEXT_HISTORY_PRIORITY_SAMPLE_LIMIT),
                            ..ListEventsOptions::default()
                        },
                    )
                    .await
                    .with_context(|| {
                        format!("sample indexed text-note history for author {author}")
                    })?
                    .len()
            } else {
                FULL_TEXT_HISTORY_PRIORITY_SAMPLE_LIMIT
            };
            prioritized.push((distance, indexed_text_sample, index, author));
        }

        prioritized.sort_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then_with(|| left.1.cmp(&right.1))
                .then_with(|| left.2.cmp(&right.2))
        });
        info!(
            "Nostr mirror full text content history prioritized authors: authors={} sampled_distance_le_{}={}",
            prioritized.len(),
            FULL_TEXT_HISTORY_PRIORITY_MAX_DISTANCE,
            sampled
        );
        Ok(prioritized
            .into_iter()
            .map(|(_, _, _, author)| author)
            .collect())
    }

    fn full_text_note_history_follow_distance(&self) -> Option<u32> {
        let distance = self.config.full_text_note_history_follow_distance?;
        if self
            .config
            .kinds
            .iter()
            .any(|kind| *kind == Kind::TextNote.as_u16() || *kind == KIND_LONG_FORM_CONTENT)
        {
            Some(distance.min(self.config.max_follow_distance))
        } else {
            None
        }
    }

    fn full_text_note_history_max_relay_pages(&self) -> Option<usize> {
        Self::full_text_note_history_max_relay_pages_for_config(&self.config)
    }

    fn full_text_note_history_max_relay_pages_for_config(
        config: &NostrMirrorConfig,
    ) -> Option<usize> {
        let pages = config.full_text_note_history_max_relay_pages;
        if pages == 0 {
            None
        } else {
            Some(pages)
        }
    }

    fn is_text_content_history_kind(kind: u16) -> bool {
        kind == Kind::TextNote.as_u16() || kind == KIND_LONG_FORM_CONTENT
    }

    fn history_sync_kinds_for_config(config: &NostrMirrorConfig) -> Vec<u16> {
        let mut kinds = config.kinds.clone();
        kinds.retain(|kind| !Self::is_text_content_history_kind(*kind));
        kinds
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

    fn is_metadata_only_history_sync(kinds: &[u16]) -> bool {
        !kinds.is_empty() && kinds.iter().all(|kind| *kind == Kind::Metadata.as_u16())
    }

    fn history_sync_kinds_affect_profile_or_graph(kinds: &[u16]) -> bool {
        kinds.is_empty()
            || kinds.iter().any(|kind| {
                *kind == Kind::Metadata.as_u16()
                    || *kind == Kind::ContactList.as_u16()
                    || *kind == Kind::MuteList.as_u16()
            })
    }

    fn history_sync_plan_for(
        config: &NostrMirrorConfig,
        authors: usize,
        kinds: &[u16],
    ) -> HistorySyncPlan {
        let author_batch_size = config.author_batch_size.max(1);
        let per_author_event_limit = config.history_sync_per_author_event_limit.max(1);
        let relay_page_size = 1_000;
        let max_relay_pages = 10;

        if Self::is_metadata_only_history_sync(kinds) {
            return HistorySyncPlan {
                relay_fetch_mode: RelayFetchMode::AuthorBatches,
                author_batch_size: author_batch_size.min(METADATA_HISTORY_SYNC_AUTHOR_BATCH_SIZE),
                per_author_event_limit: METADATA_HISTORY_SYNC_PER_AUTHOR_EVENT_LIMIT,
                relay_page_size,
                max_relay_pages,
            };
        }

        if authors > author_batch_size.saturating_mul(LARGE_HISTORY_SYNC_AUTHOR_MULTIPLIER) {
            return HistorySyncPlan {
                relay_fetch_mode: RelayFetchMode::GlobalRecent,
                author_batch_size,
                per_author_event_limit: per_author_event_limit
                    .min(LARGE_HISTORY_SYNC_PER_AUTHOR_EVENT_LIMIT)
                    .max(1),
                relay_page_size,
                max_relay_pages: LARGE_HISTORY_SYNC_MAX_RELAY_PAGES,
            };
        }

        HistorySyncPlan {
            relay_fetch_mode: RelayFetchMode::AuthorBatches,
            author_batch_size,
            per_author_event_limit,
            relay_page_size,
            max_relay_pages,
        }
    }

    fn history_sync_plan(&self, authors: usize, kinds: &[u16]) -> HistorySyncPlan {
        Self::history_sync_plan_for(&self.config, authors, kinds)
    }

    fn history_sync_chunk_size_for_config(
        config: &NostrMirrorConfig,
        authors: usize,
        kinds: &[u16],
        full_author_history: bool,
        chunk_size_override: Option<usize>,
    ) -> usize {
        let configured_chunk_size = chunk_size_override
            .unwrap_or(config.history_sync_author_chunk_size)
            .max(1);
        if full_author_history {
            return 1;
        }
        if !full_author_history
            && Self::history_sync_plan_for(config, authors, kinds).relay_fetch_mode
                == RelayFetchMode::GlobalRecent
        {
            return authors.max(1);
        }
        configured_chunk_size
    }

    async fn history_sync_authors(&self, authors: Vec<String>) -> Result<()> {
        let kinds = Self::history_sync_kinds_for_config(&self.config);
        if kinds.is_empty() {
            info!("Nostr mirror history sync skipped: no enabled history kinds");
            return Ok(());
        }
        self.history_sync_authors_with_kinds(authors, &kinds).await
    }

    async fn history_sync_authors_with_kinds(
        &self,
        authors: Vec<String>,
        kinds: &[u16],
    ) -> Result<()> {
        self.history_sync_authors_with_kinds_and_mode(authors, kinds, false, None)
            .await
    }

    async fn history_sync_full_text_notes_for_reachable_authors(&self) -> Result<()> {
        let Some(distance) = self.full_text_note_history_follow_distance() else {
            return Ok(());
        };
        if self.full_text_note_history_max_relay_pages().is_none() {
            info!("Nostr mirror full text content history sync skipped: max_relay_pages=0");
            return Ok(());
        }
        info!(
            "Nostr mirror full text content history author collection starting: max_follow_distance={distance}"
        );
        let authors = self
            .prioritize_full_text_note_history_authors(
                self.collect_authors_with_max_distance(distance)?,
            )
            .await?;
        let Some(max_relay_pages) = self.full_text_note_history_max_relay_pages() else {
            info!("Nostr mirror full text content history sync skipped: max_relay_pages=0");
            return Ok(());
        };
        self.history_sync_distance_filtered_full_text_notes(authors, distance, max_relay_pages)
            .await
    }

    async fn history_sync_full_text_notes_for_authors(&self, authors: Vec<String>) -> Result<()> {
        let Some(distance) = self.full_text_note_history_follow_distance() else {
            return Ok(());
        };
        let Some(max_relay_pages) = self.full_text_note_history_max_relay_pages() else {
            info!("Nostr mirror full text content history sync skipped: max_relay_pages=0");
            return Ok(());
        };
        let mut close_authors = Vec::new();
        for author in authors {
            let Some(pubkey) = decode_hex_pubkey(&author) else {
                continue;
            };
            if self
                .graph_store
                .follow_distance(&pubkey)?
                .is_some_and(|actual_distance| actual_distance <= distance)
            {
                close_authors.push(author);
            }
        }
        if close_authors.is_empty() {
            return Ok(());
        }
        let close_authors = self
            .prioritize_full_text_note_history_authors(close_authors)
            .await?;

        self.history_sync_distance_filtered_full_text_notes(
            close_authors,
            distance,
            max_relay_pages,
        )
        .await
    }

    async fn history_sync_distance_filtered_full_text_notes(
        &self,
        close_authors: Vec<String>,
        distance: u32,
        max_relay_pages: usize,
    ) -> Result<()> {
        if close_authors.is_empty() {
            return Ok(());
        }

        info!(
            "Nostr mirror full text content history sync starting: authors={} max_follow_distance={} max_relay_pages={}",
            close_authors.len(),
            distance,
            max_relay_pages
        );
        let kinds = [Kind::TextNote.as_u16(), KIND_LONG_FORM_CONTENT];
        self.history_sync_authors_with_kinds_and_mode(
            close_authors,
            &kinds,
            true,
            Some(max_relay_pages),
        )
        .await
    }

    async fn history_sync_authors_with_kinds_and_mode(
        &self,
        authors: Vec<String>,
        kinds: &[u16],
        full_author_history: bool,
        max_relay_pages: Option<usize>,
    ) -> Result<()> {
        let update_profile_and_graph = Self::history_sync_kinds_affect_profile_or_graph(kinds);
        let chunk_size = Self::history_sync_chunk_size_for_config(
            &self.config,
            authors.len(),
            kinds,
            full_author_history,
            None,
        );
        self.history_sync_authors_chunked(
            authors,
            |current_root, author_chunk| async move {
                self.history_sync_author_chunk(
                    current_root,
                    author_chunk,
                    kinds,
                    full_author_history,
                    max_relay_pages,
                )
                .await
            },
            update_profile_and_graph,
            Some(chunk_size),
        )
        .await
    }

    async fn history_sync_authors_chunked<F, Fut>(
        &self,
        authors: Vec<String>,
        mut run_chunk: F,
        update_profile_and_graph: bool,
        chunk_size_override: Option<usize>,
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

        let mut current_root = self.graph_store.public_events_root_for_write()?;
        let mut last_error = None;
        let mut applied_chunks = 0usize;
        let mut failed_chunks = 0usize;
        let chunk_size = chunk_size_override
            .unwrap_or(self.config.history_sync_author_chunk_size)
            .max(1);
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
            let mut report = match run_chunk(current_root.clone(), author_chunk.clone()).await {
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

            let latest_root = self.graph_store.public_events_root_for_write()?;
            if latest_root != current_root {
                info!(
                    "Nostr mirror history sync root advanced while chunk was fetching; merging chunk into latest root: chunk={}/{} authors={} events_applied={}",
                    chunk_index + 1,
                    total_chunks,
                    author_count,
                    report.applied_events.len()
                );
                if report.applied_events.is_empty() {
                    report.root = latest_root.clone();
                } else {
                    let event_store = NostrEventStore::new(self.store.store_arc());
                    report.root = event_store
                        .build(latest_root.as_ref(), report.applied_events.clone())
                        .await
                        .context("merge history chunk into latest mirrored event root")?;
                }
                current_root = latest_root;
            }

            if report.root != current_root {
                self.apply_history_root_with_options(
                    report.root.as_ref(),
                    update_profile_and_graph,
                    true,
                    Some(&report.applied_events),
                )
                .await?;
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
                applied_chunks, failed_chunks
            );
        }
        Ok(())
    }

    async fn history_sync_author_chunk(
        &self,
        current_root: Option<hashtree_core::Cid>,
        authors: Vec<String>,
        kinds: &[u16],
        full_author_history: bool,
        max_relay_pages: Option<usize>,
    ) -> Result<CrawlReport> {
        let mut last_error = None;
        let mut report = None;
        let mut plan = self.history_sync_plan(authors.len(), kinds);
        if full_author_history {
            plan.relay_fetch_mode = RelayFetchMode::AuthorBatches;
            plan.max_relay_pages = max_relay_pages.unwrap_or(plan.max_relay_pages);
        }
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
                    author_batch_size: plan.author_batch_size,
                    per_author_event_limit: plan.per_author_event_limit,
                    per_author_live_bytes: None,
                    fetch_timeout: self.config.fetch_timeout,
                    kinds: Some(kinds.to_vec()),
                    relay_fetch_mode: plan.relay_fetch_mode,
                    require_negentropy: self.config.require_negentropy,
                    relay_event_max_size: self.config.relay_event_max_size,
                    relay_page_size: plan.relay_page_size,
                    max_relay_pages: plan.max_relay_pages,
                    full_author_history,
                },
            );

            let crawl = bridge.crawl_with_progress(
                self.graph_store.as_ref(),
                current_root.as_ref(),
                |progress| {
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
                },
            );
            let crawl_result: Result<CrawlReport> = if full_author_history {
                let timeout = self.config.fetch_timeout.saturating_mul(4);
                match tokio::time::timeout(timeout, crawl).await {
                    Ok(result) => result.map_err(Into::into),
                    Err(_) => Err(anyhow::anyhow!(
                        "full author history crawl timed out after {:?} for {} author(s)",
                        timeout,
                        authors.len()
                    )),
                }
            } else {
                crawl.await.map_err(Into::into)
            };

            match crawl_result {
                Ok(next_report) => {
                    report = Some(next_report);
                    break;
                }
                Err(err) => {
                    last_error = Some(err);
                    if full_author_history {
                        break;
                    }
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

    #[cfg(test)]
    async fn apply_history_root(&self, root: Option<&hashtree_core::Cid>) -> Result<()> {
        self.apply_history_root_with_options(root, true, true, None)
            .await
    }

    async fn apply_history_root_with_options(
        &self,
        root: Option<&hashtree_core::Cid>,
        update_profile_and_graph: bool,
        publish_roots: bool,
        applied_events: Option<&[hashtree_nostr::StoredNostrEvent]>,
    ) -> Result<()> {
        self.graph_store.write_public_events_root(root)?;
        let Some(root) = root else {
            return Ok(());
        };

        self.note_public_events_root_change()?;
        if update_profile_and_graph {
            let events = match applied_events {
                Some(events) => events
                    .iter()
                    .cloned()
                    .map(socialgraph::stored_event_to_nostr_event)
                    .collect::<Result<Vec<_>>>()?,
                None => {
                    let event_store = NostrEventStore::new(self.store.store_arc());
                    event_store
                        .list_recent_lossy(Some(root), ListEventsOptions::default())
                        .await
                        .context("list trusted mirrored events")?
                        .into_iter()
                        .map(socialgraph::stored_event_to_nostr_event)
                        .collect::<Result<Vec<_>>>()?
                }
            };

            socialgraph::ingest_graph_parsed_events(self.graph_store.as_ref(), &events)
                .context("sync mirrored social graph state")?;
            if applied_events.is_some() {
                self.graph_store
                    .sync_profile_index_for_events(&events)
                    .context("update mirrored profile search index")?;
            } else {
                self.graph_store
                    .rebuild_profile_index_for_events(&events)
                    .context("rebuild mirrored profile search index")?;
            }
            self.note_profile_search_root_change()?;
            self.note_profiles_by_pubkey_root_change()?;
        }
        if !publish_roots {
            return Ok(());
        }
        let (event_result, profile_search_result, profiles_by_pubkey_result) = self
            .publish_priority_roots(true, update_profile_and_graph, update_profile_and_graph)
            .await;
        if let Err(err) = event_result {
            warn!(
                "Nostr mirror event-root publish failed after root update: {:#}",
                err
            );
        }
        if let Err(err) = profile_search_result {
            warn!(
                "Nostr mirror profile-search publish failed after root update: {:#}",
                err
            );
        }
        if let Err(err) = profiles_by_pubkey_result {
            warn!(
                "Nostr mirror profiles-by-pubkey publish failed after root update: {:#}",
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

            if let Err(err) = self.client.subscribe(vec![filter], None).await {
                warn!(
                    "Nostr mirror author subscription failed: authors={} error={:#}",
                    chunk.len(),
                    err
                );
                continue;
            }
            subscribed_authors.extend(chunk.iter().cloned());
        }
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
        let previous_profiles_by_pubkey_root = self.graph_store.profiles_by_pubkey_root()?;

        socialgraph::ingest_parsed_events_with_storage_class(
            self.graph_store.as_ref(),
            &events,
            socialgraph::EventStorageClass::Public,
        )
        .context("ingest live mirrored event batch")?;

        let next_event_root = self.graph_store.public_events_root()?;
        let next_profile_search_root = self.graph_store.profile_search_root()?;
        let next_profiles_by_pubkey_root = self.graph_store.profiles_by_pubkey_root()?;
        let event_root_changed = next_event_root != previous_event_root;
        let profile_search_root_changed = next_profile_search_root != previous_profile_search_root;
        let profiles_by_pubkey_root_changed =
            next_profiles_by_pubkey_root != previous_profiles_by_pubkey_root;

        if event_root_changed {
            self.note_public_events_root_change()?;
        }
        if profile_search_root_changed {
            self.note_profile_search_root_change()?;
        }
        if profiles_by_pubkey_root_changed {
            self.note_profiles_by_pubkey_root_change()?;
        }
        if profile_search_root_changed {
            self.maybe_publish_profile_search_root(true).await?;
        }
        if profiles_by_pubkey_root_changed {
            self.maybe_publish_profiles_by_pubkey_root(true).await?;
        }
        if event_root_changed {
            self.maybe_publish_event_root(true).await?;
        }
        info!(
            "Nostr mirror flushed live events: events={} event_root_changed={} profile_search_root_changed={} profiles_by_pubkey_root_changed={}",
            event_count,
            event_root_changed,
            profile_search_root_changed,
            profiles_by_pubkey_root_changed
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

    fn note_profiles_by_pubkey_root_change(&self) -> Result<()> {
        let root = self.graph_store.profiles_by_pubkey_root()?;
        Self::note_root_change(
            self.config
                .published_profiles_by_pubkey_tree_name
                .as_deref(),
            &self.profiles_by_pubkey_publish_state,
            root,
        )
    }

    fn note_root_change(
        tree_name: Option<&str>,
        publish_state: &Arc<Mutex<RootPublishState>>,
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
        state.last_upload_failed_at = None;
        state.last_upload_error = None;
        state.last_changed_at = Some(now);
        if state.dirty_since.is_none() {
            state.dirty_since = Some(now);
        }
        Ok(())
    }

    async fn maybe_publish_event_root(&self, force: bool) -> Result<()> {
        if self.take_missing_blob_event_upload_error() {
            return self.rebuild_event_indexes_after_missing_blobs(force).await;
        }
        let result = self
            .maybe_publish_root(
                self.config.published_event_tree_name.as_deref(),
                &self.event_publish_state,
                "event root",
                force,
                false,
            )
            .await;
        let Err(error) = result else {
            if self.take_missing_blob_event_upload_error() {
                return self.rebuild_event_indexes_after_missing_blobs(force).await;
            }
            return Ok(());
        };
        if !is_missing_local_blob_push_error(&error) {
            return Err(error);
        }

        self.rebuild_event_indexes_after_missing_blobs(force).await
    }

    fn take_missing_blob_event_upload_error(&self) -> bool {
        let mut state = self
            .event_publish_state
            .lock()
            .expect("event root publish state");
        if state.upload_in_progress_root.is_some() {
            return false;
        }
        if !state.missing_blob_rebuild_required {
            return false;
        }
        state.missing_blob_rebuild_required = false;
        state.last_upload_error = None;
        state.last_upload_failed_at = None;
        true
    }

    async fn rebuild_event_indexes_after_missing_blobs(&self, force: bool) -> Result<()> {
        warn!(
            "Nostr mirror event root DAG references missing local blobs; rebuilding event indexes from stored events"
        );
        let (public_count, ambient_count) = self
            .graph_store
            .rebuild_event_indexes_from_stored_events_async()
            .await
            .context("rebuild event indexes after missing event blobs")?;
        info!(
            "Nostr mirror rebuilt event indexes after missing blobs: public={} ambient={}",
            public_count, ambient_count
        );
        self.sync_publish_roots_from_store()?;

        self.maybe_publish_root(
            self.config.published_event_tree_name.as_deref(),
            &self.event_publish_state,
            "event root",
            force,
            false,
        )
        .await
    }

    async fn maybe_publish_profile_search_root(&self, force: bool) -> Result<()> {
        self.maybe_publish_root(
            self.config.published_profile_search_tree_name.as_deref(),
            &self.profile_search_publish_state,
            "profile search root",
            force,
            true,
        )
        .await
    }

    async fn maybe_publish_profiles_by_pubkey_root(&self, force: bool) -> Result<()> {
        self.maybe_publish_root(
            self.config
                .published_profiles_by_pubkey_tree_name
                .as_deref(),
            &self.profiles_by_pubkey_publish_state,
            "profiles-by-pubkey root",
            force,
            true,
        )
        .await
    }

    async fn maybe_publish_root(
        &self,
        tree_name: Option<&str>,
        publish_state: &Arc<Mutex<RootPublishState>>,
        log_label: &str,
        force: bool,
        publish_before_upload_ready_on_force: bool,
    ) -> Result<()> {
        let Some(tree_name) = tree_name else {
            return Ok(());
        };

        let pending_root = {
            let state = publish_state.lock().expect("root publish state");
            let Some(pending_root) = state.pending_root.clone() else {
                return Ok(());
            };

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

        let upload_started = self.maybe_start_background_root_upload(
            tree_name,
            &pending_root,
            publish_state,
            log_label,
        );
        let upload_required = !self.config.blossom_write_servers.is_empty();
        let (upload_ready, publish_root) = {
            let state = publish_state.lock().expect("root publish state");
            let upload_ready =
                !upload_required || state.last_uploaded_root.as_ref() == Some(&pending_root);
            let publish_root = if upload_ready {
                Some(pending_root.clone())
            } else {
                state.last_uploaded_root.clone().filter(|uploaded_root| {
                    state.last_published_root.as_ref() != Some(uploaded_root)
                })
            };
            (upload_ready, publish_root)
        };
        let publish_before_upload_ready =
            force && upload_required && !upload_ready && publish_before_upload_ready_on_force;
        let publish_root = if let Some(publish_root) = publish_root {
            publish_root
        } else if publish_before_upload_ready {
            pending_root.clone()
        } else {
            if upload_started {
                info!(
                    "Nostr mirror uploading {} DAG before publish: tree={} hash={}",
                    log_label,
                    tree_name,
                    hex::encode(pending_root.hash),
                );
            }
            return Ok(());
        };

        let mut successful_relays = Vec::new();
        let mut failed_relays = Vec::new();
        let mut published_now = false;
        let publish_required =
            self.publish_client.is_some() && !self.config.publish_relays.is_empty();
        if publish_required {
            let Some(publish_client) = self.publish_client.as_ref() else {
                unreachable!("publish_required implies publish_client");
            };
            if !self.has_connected_publish_relay().await {
                return Ok(());
            }
            if publish_before_upload_ready {
                info!(
                    "Nostr mirror publishing {} before Blossom upload completes: tree={} hash={}",
                    log_label,
                    tree_name,
                    hex::encode(pending_root.hash),
                );
            } else if !upload_ready {
                info!(
                    "Nostr mirror publishing uploaded {} while newer root is still uploading: tree={} published_hash={} pending_hash={}",
                    log_label,
                    tree_name,
                    hex::encode(publish_root.hash),
                    hex::encode(pending_root.hash),
                );
            }

            let already_published = {
                let state = publish_state.lock().expect("root publish state");
                state.last_published_root.as_ref() == Some(&publish_root)
            };
            if !already_published {
                let publish_relays = self.config.publish_relays.clone();
                let latest_known_created_at = {
                    let state = publish_state.lock().expect("root publish state");
                    state.last_published_created_at
                };
                let publish_created_at =
                    next_replaceable_created_at(Timestamp::now(), latest_known_created_at);
                let event = publish_client
                    .sign_event_builder(Self::build_public_root_event(
                        tree_name,
                        &publish_root,
                        publish_created_at,
                    ))
                    .await
                    .with_context(|| format!("sign {log_label} event"))?;
                let publish_result = self
                    .publish_root_event_to_relays(publish_client, &publish_relays, &event)
                    .await
                    .with_context(|| format!("publish {log_label} event"))?;
                successful_relays = publish_result.0;
                failed_relays = publish_result.1;
                if successful_relays.is_empty() {
                    let failure_summary = if failed_relays.is_empty() {
                        "no publish relays accepted the event".to_string()
                    } else {
                        failed_relays.join("; ")
                    };
                    anyhow::bail!("no publish relays accepted the event ({failure_summary})");
                }

                let mut state = publish_state.lock().expect("root publish state");
                if state.pending_root.as_ref() == Some(&pending_root)
                    || state.last_uploaded_root.as_ref() == Some(&publish_root)
                    || publish_before_upload_ready
                {
                    state.last_published_root = Some(publish_root.clone());
                    state.last_published_at = Some(Instant::now());
                    state.last_published_created_at = Some(event.created_at);
                }
                published_now = true;
            }
        }

        {
            let mut state = publish_state.lock().expect("root publish state");
            if state.pending_root.as_ref() == Some(&pending_root) {
                let upload_satisfied = self.config.blossom_write_servers.is_empty()
                    || state.last_uploaded_root.as_ref() == Some(&pending_root);
                let publish_satisfied =
                    !publish_required || state.last_published_root.as_ref() == Some(&pending_root);
                if upload_satisfied && publish_satisfied {
                    state.dirty_since = None;
                }
            }
        }

        if published_now {
            info!(
                "Nostr mirror published {}: tree={} hash={} relays={:?}",
                log_label,
                tree_name,
                hex::encode(publish_root.hash),
                successful_relays,
            );
        }
        if !failed_relays.is_empty() {
            warn!(
                "Nostr mirror publish had relay failures: tree={} failures={:?}",
                tree_name, failed_relays
            );
        }
        Ok(())
    }

    fn maybe_start_background_root_upload(
        &self,
        tree_name: &str,
        pending_root: &hashtree_core::Cid,
        publish_state: &Arc<Mutex<RootPublishState>>,
        log_label: &str,
    ) -> bool {
        if self.config.blossom_write_servers.is_empty() {
            return false;
        }

        let previous_uploaded_root = {
            let mut state = publish_state.lock().expect("root publish state");
            if state.last_uploaded_root.as_ref() == Some(pending_root)
                || state.upload_in_progress_root.is_some()
            {
                return false;
            }
            if state
                .last_upload_failed_at
                .is_some_and(|failed_at| failed_at.elapsed() < MIRROR_ROOT_UPLOAD_RETRY_INTERVAL)
            {
                return false;
            }
            state.upload_in_progress_root = Some(pending_root.clone());
            state.last_uploaded_root.clone()
        };

        let store = Arc::clone(&self.store);
        let upload_state_path = Self::uploaded_root_state_path(store.base_path(), tree_name);
        let servers = self.config.blossom_write_servers.clone();
        let root = pending_root.clone();
        let publish_state = Arc::clone(publish_state);
        let log_label = log_label.to_string();
        tokio::task::spawn_blocking(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build nostr mirror root upload runtime");
            runtime.block_on(async move {
                let result = background_blossom_push_incremental_with_store(
                    store,
                    root.clone(),
                    previous_uploaded_root,
                    &servers,
                )
                .await;
                let mut state = publish_state.lock().expect("root publish state");
                if state.upload_in_progress_root.as_ref() == Some(&root) {
                    state.upload_in_progress_root = None;
                }
                match result {
                    Ok(()) => {
                        if let Err(err) =
                            Self::write_uploaded_root_state(&upload_state_path, &root, &log_label)
                        {
                            warn!("Nostr mirror failed to persist uploaded root state: {err:#}");
                        }
                        state.last_uploaded_root = Some(root.clone());
                        state.last_uploaded_at = Some(Instant::now());
                        if state.pending_root.as_ref() == Some(&root) {
                            state.last_upload_failed_at = None;
                            state.last_upload_error = None;
                            state.missing_blob_rebuild_required = false;
                        }
                        info!(
                            "Nostr mirror uploaded {} DAG to Blossom: hash={}",
                            log_label,
                            hex::encode(root.hash)
                        );
                    }
                    Err(err) => {
                        if state.pending_root.as_ref() == Some(&root) {
                            state.last_upload_failed_at = Some(Instant::now());
                            state.last_upload_error = Some(format!("{err:#}"));
                        }
                        if is_missing_local_blob_message(&format!("{err:#}")) {
                            state.missing_blob_rebuild_required = true;
                        }
                        warn!(
                            "Nostr mirror {} DAG upload failed: hash={} error={:#}",
                            log_label,
                            hex::encode(root.hash),
                            err
                        );
                    }
                }
            });
        });

        true
    }

    async fn publish_root_event_to_relays(
        &self,
        publish_client: &Client,
        relays: &[String],
        event: &Event,
    ) -> Result<(Vec<String>, Vec<String>)> {
        let primary_send = Self::send_root_event_to_relays(publish_client, relays, event);
        let (mut successful_relays, mut failed_relays) =
            match tokio::time::timeout(MIRROR_ROOT_PUBLISH_PRIMARY_TIMEOUT, primary_send).await {
                Ok(result) => result,
                Err(_) => (
                    Vec::new(),
                    vec![format!(
                        "publish relays: primary publish timed out after {:?}",
                        MIRROR_ROOT_PUBLISH_PRIMARY_TIMEOUT
                    )],
                ),
            };

        if successful_relays.is_empty() {
            let (retry_successes, retry_failures) = self
                .publish_root_event_with_fresh_client(relays, event)
                .await;
            successful_relays.extend(retry_successes);
            failed_relays.extend(retry_failures);
        }

        Ok((successful_relays, failed_relays))
    }

    async fn send_root_event_to_relays(
        publish_client: &Client,
        relays: &[String],
        event: &Event,
    ) -> (Vec<String>, Vec<String>) {
        let mut successful_relays = Vec::new();
        let mut failed_relays = Vec::new();

        match publish_client
            .send_event_to(relays.iter().map(|relay| relay.as_str()), event.clone())
            .await
        {
            Ok(output) => {
                for relay in relays {
                    let relay_url = relay.trim_end_matches('/');
                    if output
                        .success
                        .iter()
                        .any(|url| url.as_str().trim_end_matches('/') == relay_url)
                    {
                        successful_relays.push(relay.clone());
                    }
                }
                failed_relays.extend(output.failed.into_iter().map(|(url, reason)| match reason {
                    Some(reason) => format!("{url}: {reason}"),
                    None => format!("{url}: relay rejected publish"),
                }));
            }
            Err(err) => {
                failed_relays.push(format!("publish relays: {err}"));
            }
        }

        (successful_relays, failed_relays)
    }

    async fn publish_root_event_with_fresh_client(
        &self,
        relays: &[String],
        event: &Event,
    ) -> (Vec<String>, Vec<String>) {
        let client = Client::with_opts(Keys::generate(), Options::new().wait_for_send(false));
        let mut setup_failures = Vec::new();
        for relay in relays {
            if let Err(err) = client.add_relay(relay).await {
                setup_failures.push(format!("{relay}: add relay failed: {err}"));
            }
        }

        client.connect().await;
        let publish = Self::send_root_event_to_relays(&client, relays, event);
        let retry_timeout = self
            .config
            .fetch_timeout
            .min(MIRROR_ROOT_PUBLISH_RETRY_TIMEOUT);
        let result = tokio::time::timeout(retry_timeout, publish).await;
        let _ = client.disconnect().await;

        match result {
            Ok((successful_relays, mut failed_relays)) => {
                failed_relays.extend(setup_failures);
                (successful_relays, failed_relays)
            }
            Err(_) => {
                setup_failures.push(format!(
                    "fresh publish client timed out after {:?}",
                    retry_timeout
                ));
                (Vec::new(), setup_failures)
            }
        }
    }

    fn build_public_root_event(
        tree_name: &str,
        cid: &hashtree_core::Cid,
        created_at: Timestamp,
    ) -> EventBuilder {
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

        EventBuilder::new(Kind::Custom(30078), "", tags).custom_created_at(created_at)
    }
}

fn is_missing_local_blob_push_error(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.to_string().contains(MISSING_LOCAL_BLOB_PUSH_ERROR))
}

fn is_missing_local_blob_message(message: &str) -> bool {
    message.contains(MISSING_LOCAL_BLOB_PUSH_ERROR)
}

fn next_replaceable_created_at(now: Timestamp, latest_existing: Option<Timestamp>) -> Timestamp {
    match latest_existing {
        Some(latest) if latest >= now => Timestamp::from_secs(latest.as_u64().saturating_add(1)),
        _ => now,
    }
}

#[cfg(test)]
mod tests;
