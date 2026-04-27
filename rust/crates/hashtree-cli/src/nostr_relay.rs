use std::collections::HashMap;
use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, Mutex};

#[cfg(feature = "p2p")]
use hashtree_network::{MeshEventStore, MeshRelayClient};
use nostr::{ClientMessage as NostrClientMessage, JsonUtil, RelayMessage as NostrRelayMessage};
use nostr::{Event, EventId, Filter as NostrFilter, SubscriptionId};

use crate::socialgraph;

const BLUETOOTH_EVENT_LOG_CAPACITY: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BluetoothReceivedEventRecord {
    pub event_id: String,
    pub pubkey: String,
    pub kind: u32,
    pub created_at: u64,
    pub received_at: u64,
    pub peer_id: Option<String>,
    pub cid_values: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct NostrRelayConfig {
    pub spambox_db_max_bytes: u64,
    pub max_query_limit: usize,
    pub max_subs_per_client: usize,
    pub max_filters_per_sub: usize,
    pub spambox_max_events_per_min: u32,
    pub spambox_max_reqs_per_min: u32,
}

impl Default for NostrRelayConfig {
    fn default() -> Self {
        Self {
            spambox_db_max_bytes: 1024 * 1024 * 1024,
            max_query_limit: 200,
            max_subs_per_client: 64,
            max_filters_per_sub: 32,
            spambox_max_events_per_min: 120,
            spambox_max_reqs_per_min: 120,
        }
    }
}

mod imp {
    use super::*;
    use anyhow::Result;

    use crate::diagnostics::{
        nostr_filter_summary, nostr_filters_summary, process_memory_snapshot,
    };
    use crate::socialgraph::{EventStorageClass, SocialGraphAccessControl, SocialGraphBackend};
    use hashtree_core::{nhash_decode, Cid};
    use hashtree_nostr::{is_parameterized_replaceable_kind, is_replaceable_kind};
    use tracing::{info, warn};

    fn prefers_trusted_only(filter: &NostrFilter) -> bool {
        let Some(kinds) = filter.kinds.as_ref() else {
            return false;
        };
        if kinds.len() != 1 {
            return false;
        }

        let kind = kinds.iter().next().expect("checked single kind").as_u16() as u32;
        let has_authors = filter
            .authors
            .as_ref()
            .is_some_and(|authors| !authors.is_empty());
        if !has_authors {
            return false;
        }

        if is_replaceable_kind(kind) {
            return true;
        }

        if is_parameterized_replaceable_kind(kind) {
            let d_tag = nostr::SingleLetterTag::lowercase(nostr::Alphabet::D);
            return filter
                .generic_tags
                .get(&d_tag)
                .is_some_and(|values| !values.is_empty());
        }

        false
    }

    struct NostrStore {
        store: Arc<dyn SocialGraphBackend>,
    }

    impl NostrStore {
        fn new(store: Arc<dyn SocialGraphBackend>) -> Self {
            Self { store }
        }

        async fn ingest(&self, event: Event) -> Result<()> {
            let store = Arc::clone(&self.store);
            tokio::task::spawn_blocking(move || {
                crate::socialgraph::ingest_parsed_event(store.as_ref(), &event)
            })
            .await
            .map_err(|err| anyhow::anyhow!("trusted nostr store ingest task failed: {err}"))?
        }

        async fn ingest_with_storage_class(
            &self,
            event: Event,
            storage_class: EventStorageClass,
        ) -> Result<()> {
            let store = Arc::clone(&self.store);
            tokio::task::spawn_blocking(move || {
                crate::socialgraph::ingest_parsed_event_with_storage_class(
                    store.as_ref(),
                    &event,
                    storage_class,
                )
            })
            .await
            .map_err(|err| anyhow::anyhow!("trusted nostr store ingest task failed: {err}"))?
        }

        async fn query(&self, filter: NostrFilter, limit: usize) -> Vec<Event> {
            let store = Arc::clone(&self.store);
            let filter_summary = nostr_filter_summary(&filter);
            let memory_before = process_memory_snapshot();
            let started = Instant::now();
            let result = tokio::task::spawn_blocking(move || {
                crate::socialgraph::query_events(store.as_ref(), &filter, limit)
            })
            .await;
            match result {
                Ok(events) => {
                    info!(
                        target: "hashtree_cli::nostr_relay::query",
                        limit,
                        events = events.len(),
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        filter = %filter_summary,
                        memory_before = ?memory_before,
                        memory_after = ?process_memory_snapshot(),
                        "trusted nostr store query completed",
                    );
                    events
                }
                Err(err) => {
                    warn!("trusted nostr store query task failed: {}", err);
                    Vec::new()
                }
            }
        }
    }

    #[derive(Debug, Clone)]
    struct ClientQuota {
        last_reset: Instant,
        spambox_events: u32,
        reqs: u32,
    }

    impl ClientQuota {
        fn new() -> Self {
            Self {
                last_reset: Instant::now(),
                spambox_events: 0,
                reqs: 0,
            }
        }

        fn reset_if_needed(&mut self) {
            if self.last_reset.elapsed() >= Duration::from_secs(60) {
                self.last_reset = Instant::now();
                self.spambox_events = 0;
                self.reqs = 0;
            }
        }

        fn allow_spambox_event(&mut self, limit: u32) -> bool {
            self.reset_if_needed();
            if self.spambox_events >= limit {
                return false;
            }
            self.spambox_events += 1;
            true
        }

        fn allow_req(&mut self, limit: u32) -> bool {
            self.reset_if_needed();
            if self.reqs >= limit {
                return false;
            }
            self.reqs += 1;
            true
        }
    }

    struct ClientState {
        sender: mpsc::UnboundedSender<String>,
        pubkey: Option<String>,
        quota: ClientQuota,
    }

    struct RecentEvents {
        order: VecDeque<EventId>,
        events: HashMap<EventId, Event>,
        max_len: usize,
    }

    impl RecentEvents {
        fn new(max_len: usize) -> Self {
            Self {
                order: VecDeque::new(),
                events: HashMap::new(),
                max_len: max_len.max(128),
            }
        }

        fn insert(&mut self, event: Event) {
            if self.events.contains_key(&event.id) {
                return;
            }
            self.order.push_back(event.id);
            self.events.insert(event.id, event);
            while self.order.len() > self.max_len {
                if let Some(oldest) = self.order.pop_front() {
                    self.events.remove(&oldest);
                }
            }
        }

        fn matching(&self, filter: &NostrFilter) -> Vec<Event> {
            self.events
                .values()
                .filter(|event| filter.match_event(event))
                .cloned()
                .collect()
        }
    }

    enum SpamboxStore {
        Persistent(NostrStore),
        Memory(MemorySpambox),
    }

    struct MemorySpambox {
        events: Mutex<VecDeque<Event>>,
        max_len: usize,
    }

    impl MemorySpambox {
        fn new(max_len: usize) -> Self {
            Self {
                events: Mutex::new(VecDeque::new()),
                max_len: max_len.max(128),
            }
        }

        async fn ingest(&self, event: &Event) -> bool {
            let mut events = self.events.lock().await;
            events.push_back(event.clone());
            while events.len() > self.max_len {
                events.pop_front();
            }
            true
        }
    }

    impl SpamboxStore {
        async fn ingest(&self, event: &Event) -> bool {
            match self {
                SpamboxStore::Persistent(store) => store.ingest(event.clone()).await.is_ok(),
                SpamboxStore::Memory(store) => store.ingest(event).await,
            }
        }
    }

    struct BluetoothEventLog {
        path: PathBuf,
        state: Mutex<BluetoothEventLogState>,
    }

    struct BluetoothEventLogState {
        records: VecDeque<BluetoothReceivedEventRecord>,
        event_ids: HashSet<String>,
    }

    impl BluetoothEventLog {
        fn load(path: PathBuf) -> Self {
            let records = std::fs::read_to_string(&path)
                .ok()
                .map(|serialized| {
                    serialized
                        .lines()
                        .filter_map(|line| {
                            serde_json::from_str::<BluetoothReceivedEventRecord>(line).ok()
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let mut trimmed = VecDeque::with_capacity(BLUETOOTH_EVENT_LOG_CAPACITY);
            let start = records.len().saturating_sub(BLUETOOTH_EVENT_LOG_CAPACITY);
            for record in records.into_iter().skip(start) {
                trimmed.push_back(record);
            }
            let event_ids = trimmed
                .iter()
                .map(|record| record.event_id.clone())
                .collect::<HashSet<_>>();

            Self {
                path,
                state: Mutex::new(BluetoothEventLogState {
                    records: trimmed,
                    event_ids,
                }),
            }
        }

        async fn recent(&self, limit: usize) -> Vec<BluetoothReceivedEventRecord> {
            let state = self.state.lock().await;
            state
                .records
                .iter()
                .rev()
                .take(limit.max(1))
                .cloned()
                .collect()
        }

        async fn record(&self, event: &Event, peer_id: Option<String>) {
            let record = BluetoothReceivedEventRecord {
                event_id: event.id.to_hex(),
                pubkey: event.pubkey.to_hex(),
                kind: event.kind.as_u16() as u32,
                created_at: event.created_at.as_u64(),
                received_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|value| value.as_secs())
                    .unwrap_or(0),
                peer_id,
                cid_values: cid_values_from_event(event),
            };

            let serialized = {
                let mut state = self.state.lock().await;
                if state.event_ids.contains(&record.event_id) {
                    return;
                }

                state.event_ids.insert(record.event_id.clone());
                state.records.push_back(record);
                while state.records.len() > BLUETOOTH_EVENT_LOG_CAPACITY {
                    if let Some(removed) = state.records.pop_front() {
                        state.event_ids.remove(&removed.event_id);
                    }
                }

                state
                    .records
                    .iter()
                    .filter_map(|entry| serde_json::to_string(entry).ok())
                    .collect::<Vec<_>>()
                    .join("\n")
            };

            if let Some(parent) = self.path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&self.path, serialized);
        }
    }

    fn looks_like_cid_reference(value: &str) -> bool {
        Cid::parse(value).is_ok() || nhash_decode(value).is_ok()
    }

    fn cid_values_from_event(event: &Event) -> Vec<String> {
        let mut values = Vec::new();
        let mut seen = HashSet::new();

        for tag in &event.tags {
            let fields = tag.clone().to_vec();
            if fields.first().is_some_and(|name| name == "cid") {
                if let Some(value) = fields.get(1).filter(|value| !value.is_empty()) {
                    if seen.insert(value.clone()) {
                        values.push(value.clone());
                    }
                }
                continue;
            }

            for value in fields.into_iter().skip(1) {
                if looks_like_cid_reference(&value) && seen.insert(value.clone()) {
                    values.push(value);
                }
            }
        }

        values
    }

    pub struct NostrRelay {
        config: NostrRelayConfig,
        trusted: NostrStore,
        public_pubkeys: HashSet<String>,
        spambox: Option<SpamboxStore>,
        social_graph: Option<Arc<SocialGraphAccessControl>>,
        clients: Mutex<HashMap<u64, ClientState>>,
        subscriptions: Mutex<HashMap<u64, HashMap<SubscriptionId, Vec<NostrFilter>>>>,
        recent_events: Mutex<RecentEvents>,
        next_client_id: AtomicU64,
        bluetooth_event_log: Arc<BluetoothEventLog>,
    }

    impl NostrRelay {
        async fn collect_filter_events(
            &self,
            filter: &NostrFilter,
            limit: usize,
            seen: &mut HashSet<EventId>,
            events: &mut Vec<Event>,
        ) {
            if limit == 0 {
                return;
            }

            let mut added = 0usize;

            if !prefers_trusted_only(filter) {
                let recent = {
                    let cache = self.recent_events.lock().await;
                    cache.matching(filter)
                };
                for event in recent {
                    if seen.insert(event.id) {
                        events.push(event);
                        added += 1;
                        if added >= limit {
                            return;
                        }
                    }
                }
            }

            for event in self.trusted.query(filter.clone(), limit).await {
                if seen.insert(event.id) {
                    events.push(event);
                    added += 1;
                    if added >= limit {
                        return;
                    }
                }
            }
        }

        async fn collect_filter_count(
            &self,
            filter: &NostrFilter,
            limit: usize,
            seen: &mut HashSet<EventId>,
        ) {
            if limit == 0 {
                return;
            }

            let mut added = 0usize;

            if !prefers_trusted_only(filter) {
                let recent = {
                    let cache = self.recent_events.lock().await;
                    cache.matching(filter)
                };
                for event in recent {
                    if seen.insert(event.id) {
                        added += 1;
                        if added >= limit {
                            return;
                        }
                    }
                }
            }

            for event in self.trusted.query(filter.clone(), limit).await {
                if seen.insert(event.id) {
                    added += 1;
                    if added >= limit {
                        return;
                    }
                }
            }
        }

        pub fn new(
            trusted_store: Arc<dyn SocialGraphBackend>,
            data_dir: PathBuf,
            public_pubkeys: HashSet<String>,
            social_graph: Option<Arc<SocialGraphAccessControl>>,
            config: NostrRelayConfig,
        ) -> Result<Self> {
            let spambox = if config.spambox_db_max_bytes == 0 {
                Some(SpamboxStore::Memory(MemorySpambox::new(
                    config.max_query_limit * 2,
                )))
            } else {
                let spam_dir = data_dir.join("socialgraph_spambox");
                match socialgraph::open_social_graph_store_at_path(
                    &spam_dir,
                    Some(config.spambox_db_max_bytes),
                ) {
                    Ok(store) => Some(SpamboxStore::Persistent(NostrStore::new(store))),
                    Err(err) => {
                        warn!(
                            "Failed to open social graph spambox (falling back to memory): {}",
                            err
                        );
                        Some(SpamboxStore::Memory(MemorySpambox::new(
                            config.max_query_limit * 2,
                        )))
                    }
                }
            };

            let recent_size = config.max_query_limit.saturating_mul(2);
            let bluetooth_event_log = Arc::new(BluetoothEventLog::load(
                data_dir.join("bluetooth-events.jsonl"),
            ));

            Ok(Self {
                config,
                trusted: NostrStore::new(trusted_store),
                public_pubkeys,
                spambox,
                social_graph,
                clients: Mutex::new(HashMap::new()),
                subscriptions: Mutex::new(HashMap::new()),
                recent_events: Mutex::new(RecentEvents::new(recent_size)),
                next_client_id: AtomicU64::new(1),
                bluetooth_event_log,
            })
        }

        pub fn next_client_id(&self) -> u64 {
            self.next_client_id.fetch_add(1, Ordering::SeqCst)
        }

        pub async fn ingest_trusted_event(&self, event: Event) -> Result<()> {
            self.ingest_trusted_event_inner(event, true).await
        }

        pub async fn ingest_trusted_event_from_bluetooth(
            &self,
            event: Event,
            peer_id: Option<String>,
        ) -> Result<()> {
            self.ingest_trusted_event_inner(event.clone(), true).await?;
            self.bluetooth_event_log.record(&event, peer_id).await;
            Ok(())
        }

        pub async fn ingest_trusted_event_silent(&self, event: Event) -> Result<()> {
            self.ingest_trusted_event_inner(event, false).await
        }

        pub async fn bluetooth_received_events(
            &self,
            limit: usize,
        ) -> Vec<BluetoothReceivedEventRecord> {
            self.bluetooth_event_log.recent(limit).await
        }

        async fn ingest_trusted_event_inner(&self, event: Event, broadcast: bool) -> Result<()> {
            event
                .verify()
                .map_err(|e| anyhow::anyhow!("invalid signature: {}", e))?;

            let is_ephemeral = event.kind.is_ephemeral();
            {
                let mut recent = self.recent_events.lock().await;
                recent.insert(event.clone());
            }

            if !is_ephemeral {
                let storage_class = self.event_storage_class(&event);
                self.trusted
                    .ingest_with_storage_class(event.clone(), storage_class)
                    .await?;
            }

            if broadcast {
                self.broadcast_event(&event).await;
            }
            Ok(())
        }

        pub async fn query_events(&self, filter: &NostrFilter, limit: usize) -> Vec<Event> {
            let limit = limit.min(self.config.max_query_limit);
            if limit == 0 {
                return Vec::new();
            }

            let mut seen: HashSet<EventId> = HashSet::new();
            let mut events = Vec::new();

            if !prefers_trusted_only(filter) {
                let recent = {
                    let cache = self.recent_events.lock().await;
                    cache.matching(filter)
                };
                for event in recent {
                    if seen.insert(event.id) {
                        events.push(event);
                        if events.len() >= limit {
                            return events;
                        }
                    }
                }
            }

            for event in self.trusted.query(filter.clone(), limit).await {
                if seen.insert(event.id) {
                    events.push(event);
                    if events.len() >= limit {
                        break;
                    }
                }
            }

            events
        }

        pub async fn register_client(
            &self,
            client_id: u64,
            sender: mpsc::UnboundedSender<String>,
            pubkey: Option<String>,
        ) {
            let mut clients = self.clients.lock().await;
            clients.insert(
                client_id,
                ClientState {
                    sender,
                    pubkey,
                    quota: ClientQuota::new(),
                },
            );
        }

        pub async fn unregister_client(&self, client_id: u64) {
            let mut clients = self.clients.lock().await;
            clients.remove(&client_id);
            drop(clients);
            let mut subs = self.subscriptions.lock().await;
            subs.remove(&client_id);
        }

        pub async fn handle_client_message(&self, client_id: u64, msg: NostrClientMessage) {
            match msg {
                NostrClientMessage::Event(event) => {
                    self.handle_event(client_id, *event).await;
                }
                NostrClientMessage::Req {
                    subscription_id,
                    filters,
                } => {
                    self.handle_req(client_id, subscription_id, filters).await;
                }
                NostrClientMessage::Count {
                    subscription_id,
                    filters,
                } => {
                    self.handle_count(client_id, subscription_id, filters).await;
                }
                NostrClientMessage::Close(subscription_id) => {
                    self.handle_close(client_id, subscription_id).await;
                }
                NostrClientMessage::Auth(event) => {
                    self.handle_auth(client_id, *event).await;
                }
                NostrClientMessage::NegOpen { .. }
                | NostrClientMessage::NegMsg { .. }
                | NostrClientMessage::NegClose { .. } => {
                    self.send_to_client(
                        client_id,
                        NostrRelayMessage::notice("negentropy not supported"),
                    )
                    .await;
                }
            }
        }

        pub async fn register_subscription_query(
            &self,
            client_id: u64,
            subscription_id: SubscriptionId,
            mut filters: Vec<NostrFilter>,
        ) -> std::result::Result<Vec<Event>, &'static str> {
            if !self.allow_req(client_id).await {
                return Err("rate limited");
            }

            if filters.len() > self.config.max_filters_per_sub {
                filters.truncate(self.config.max_filters_per_sub);
            }

            {
                let mut subs = self.subscriptions.lock().await;
                let entry = subs.entry(client_id).or_default();
                if !entry.contains_key(&subscription_id)
                    && entry.len() >= self.config.max_subs_per_client
                {
                    return Err("too many subscriptions");
                }
                entry.insert(subscription_id.clone(), filters.clone());
            }

            let mut seen: HashSet<EventId> = HashSet::new();
            let mut events = Vec::new();
            let memory_before = process_memory_snapshot();
            let started = Instant::now();
            let filter_summary = nostr_filters_summary(&filters);
            for filter in &filters {
                let limit = filter
                    .limit
                    .unwrap_or(self.config.max_query_limit)
                    .min(self.config.max_query_limit);
                self.collect_filter_events(filter, limit, &mut seen, &mut events)
                    .await;
            }

            info!(
                target: "hashtree_cli::nostr_relay::query",
                client_id,
                subscription_id = %subscription_id,
                filters = filters.len(),
                events = events.len(),
                elapsed_ms = started.elapsed().as_millis() as u64,
                filter = %filter_summary,
                memory_before = ?memory_before,
                memory_after = ?process_memory_snapshot(),
                "nostr relay local subscription query completed",
            );
            Ok(events)
        }

        async fn handle_auth(&self, client_id: u64, event: Event) {
            let ok = event.verify().is_ok();
            let message = if ok { "" } else { "invalid auth" };
            self.send_to_client(client_id, NostrRelayMessage::ok(event.id, ok, message))
                .await;
        }

        async fn handle_close(&self, client_id: u64, subscription_id: SubscriptionId) {
            let mut subs = self.subscriptions.lock().await;
            if let Some(map) = subs.get_mut(&client_id) {
                map.remove(&subscription_id);
            }
        }

        async fn handle_event(&self, client_id: u64, event: Event) {
            let ok = event.verify().is_ok();
            if !ok {
                self.send_to_client(
                    client_id,
                    NostrRelayMessage::ok(event.id, false, "invalid: signature"),
                )
                .await;
                return;
            }

            let trusted = self.is_trusted_event(client_id, &event).await;
            if !trusted && !self.allow_spambox_event(client_id).await {
                self.send_to_client(
                    client_id,
                    NostrRelayMessage::ok(event.id, false, "rate limited"),
                )
                .await;
                return;
            }

            let is_ephemeral = event.kind.is_ephemeral();
            if trusted {
                let mut recent = self.recent_events.lock().await;
                recent.insert(event.clone());
            }
            if !is_ephemeral {
                let stored = if trusted {
                    let storage_class = self.event_storage_class(&event);
                    self.trusted
                        .ingest_with_storage_class(event.clone(), storage_class)
                        .await
                        .is_ok()
                } else {
                    match self.spambox.as_ref() {
                        Some(spambox) => spambox.ingest(&event).await,
                        None => false,
                    }
                };

                if !stored {
                    let message = if trusted {
                        "store failed"
                    } else {
                        "spambox full"
                    };
                    self.send_to_client(client_id, NostrRelayMessage::ok(event.id, false, message))
                        .await;
                    return;
                }
            }

            let message = if trusted { "" } else { "spambox" };
            self.send_to_client(client_id, NostrRelayMessage::ok(event.id, true, message))
                .await;

            if trusted {
                self.broadcast_event(&event).await;
            }
        }

        async fn handle_req(
            &self,
            client_id: u64,
            subscription_id: SubscriptionId,
            filters: Vec<NostrFilter>,
        ) {
            match self
                .register_subscription_query(client_id, subscription_id.clone(), filters)
                .await
            {
                Ok(events) => {
                    for event in events {
                        self.send_to_client(
                            client_id,
                            NostrRelayMessage::event(subscription_id.clone(), event),
                        )
                        .await;
                    }

                    self.send_to_client(client_id, NostrRelayMessage::eose(subscription_id))
                        .await;
                }
                Err(message) => {
                    self.send_to_client(
                        client_id,
                        NostrRelayMessage::closed(subscription_id, message),
                    )
                    .await;
                }
            }
        }

        async fn handle_count(
            &self,
            client_id: u64,
            subscription_id: SubscriptionId,
            filters: Vec<NostrFilter>,
        ) {
            if !self.allow_req(client_id).await {
                self.send_to_client(
                    client_id,
                    NostrRelayMessage::closed(subscription_id, "rate limited"),
                )
                .await;
                return;
            }

            let mut seen: HashSet<EventId> = HashSet::new();
            for filter in &filters {
                let limit = filter
                    .limit
                    .unwrap_or(self.config.max_query_limit)
                    .min(self.config.max_query_limit);
                self.collect_filter_count(filter, limit, &mut seen).await;
            }

            self.send_to_client(
                client_id,
                NostrRelayMessage::count(subscription_id, seen.len()),
            )
            .await;
        }

        async fn is_trusted_event(&self, client_id: u64, event: &Event) -> bool {
            let event_pubkey = event.pubkey.to_hex();
            let client_pubkey = {
                let clients = self.clients.lock().await;
                clients
                    .get(&client_id)
                    .and_then(|state| state.pubkey.clone())
            };
            if let Some(pubkey) = client_pubkey {
                return pubkey == event_pubkey
                    || self.social_graph.as_ref().is_some_and(|social_graph| {
                        social_graph.check_write_access(&event_pubkey)
                    });
            }
            if let Some(ref social_graph) = self.social_graph {
                return social_graph.check_write_access(&event_pubkey);
            }
            true
        }

        fn event_storage_class(&self, event: &Event) -> EventStorageClass {
            if self.public_pubkeys.contains(&event.pubkey.to_hex()) {
                EventStorageClass::Public
            } else {
                EventStorageClass::Ambient
            }
        }

        async fn allow_spambox_event(&self, client_id: u64) -> bool {
            let mut clients = self.clients.lock().await;
            let Some(state) = clients.get_mut(&client_id) else {
                return false;
            };
            state
                .quota
                .allow_spambox_event(self.config.spambox_max_events_per_min)
        }

        async fn allow_req(&self, client_id: u64) -> bool {
            let mut clients = self.clients.lock().await;
            let Some(state) = clients.get_mut(&client_id) else {
                return false;
            };
            state.quota.allow_req(self.config.spambox_max_reqs_per_min)
        }

        async fn broadcast_event(&self, event: &Event) {
            let subscriptions = self.subscriptions.lock().await;
            let mut deliveries: Vec<(u64, SubscriptionId)> = Vec::new();
            for (client_id, subs) in subscriptions.iter() {
                for (sub_id, filters) in subs.iter() {
                    if filters.iter().any(|f| f.match_event(event)) {
                        deliveries.push((*client_id, sub_id.clone()));
                    }
                }
            }
            drop(subscriptions);

            for (client_id, sub_id) in deliveries {
                self.send_to_client(client_id, NostrRelayMessage::event(sub_id, event.clone()))
                    .await;
            }
        }

        async fn send_to_client(&self, client_id: u64, msg: NostrRelayMessage) {
            let sender = {
                let clients = self.clients.lock().await;
                clients.get(&client_id).map(|state| state.sender.clone())
            };
            if let Some(tx) = sender {
                let _ = tx.send(msg.as_json());
            }
        }
    }
}

pub use imp::NostrRelay;

#[cfg(feature = "p2p")]
#[async_trait::async_trait]
impl MeshEventStore for NostrRelay {
    async fn ingest_trusted_event(&self, event: Event) -> anyhow::Result<()> {
        NostrRelay::ingest_trusted_event(self, event).await
    }

    async fn query_events(&self, filter: &NostrFilter, limit: usize) -> Vec<Event> {
        NostrRelay::query_events(self, filter, limit).await
    }
}

#[cfg(feature = "p2p")]
#[async_trait::async_trait]
impl MeshRelayClient for NostrRelay {
    fn next_client_id(&self) -> u64 {
        NostrRelay::next_client_id(self)
    }

    async fn register_client(
        &self,
        client_id: u64,
        sender: mpsc::UnboundedSender<String>,
        pubkey: Option<String>,
    ) {
        NostrRelay::register_client(self, client_id, sender, pubkey).await
    }

    async fn unregister_client(&self, client_id: u64) {
        NostrRelay::unregister_client(self, client_id).await
    }

    async fn handle_client_message(&self, client_id: u64, msg: NostrClientMessage) {
        NostrRelay::handle_client_message(self, client_id, msg).await
    }

    async fn register_subscription_query(
        &self,
        client_id: u64,
        subscription_id: SubscriptionId,
        filters: Vec<NostrFilter>,
    ) -> std::result::Result<Vec<Event>, &'static str> {
        NostrRelay::register_subscription_query(self, client_id, subscription_id, filters).await
    }

    async fn ingest_trusted_event_from_peer(
        &self,
        event: Event,
        peer_id: Option<String>,
    ) -> anyhow::Result<()> {
        NostrRelay::ingest_trusted_event_from_bluetooth(self, event, peer_id).await
    }
}

#[cfg(test)]
#[path = "nostr_relay/tests.rs"]
mod tests;
