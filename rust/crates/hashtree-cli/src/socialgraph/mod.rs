pub mod access;
pub mod crawler;
pub mod local_lists;
pub mod snapshot;

pub use access::SocialGraphAccessControl;
pub use crawler::SocialGraphCrawler;
pub use local_lists::{
    read_local_list_file_state, sync_local_list_files_force, sync_local_list_files_if_changed,
    LocalListFileState, LocalListSyncOutcome,
};

mod index_buckets;

use index_buckets::{
    dedupe_events, latest_metadata_events_by_pubkey, EventIndexBucket, ProfileIndexBucket,
};

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use anyhow::{Context, Result};
use bytes::Bytes;
use futures::executor::block_on;
use hashtree_core::{nhash_encode_full, Cid, HashTree, HashTreeConfig, NHashData};
use hashtree_index::BTree;
use hashtree_nostr::{
    is_parameterized_replaceable_kind, is_replaceable_kind, ListEventsOptions, NostrEventStore,
    NostrEventStoreError, ProfileGuard as NostrProfileGuard, StoredNostrEvent,
};
#[cfg(test)]
use hashtree_nostr::{
    reset_profile as reset_nostr_profile, set_profile_enabled as set_nostr_profile_enabled,
    take_profile as take_nostr_profile,
};
use heed::EnvFlags;
use nostr::{Event, Filter, JsonUtil, Kind, SingleLetterTag};
use nostr_social_graph::{
    BinaryBudget, GraphStats, NostrEvent as GraphEvent, SocialGraph,
    SocialGraphBackend as NostrSocialGraphBackend,
};
use nostr_social_graph_heed::HeedSocialGraph;

use crate::storage::{LocalStore, StorageRouter};

#[cfg(test)]
use std::sync::{Mutex, MutexGuard, OnceLock};
#[cfg(test)]
use std::time::Instant;

pub type UserSet = BTreeSet<[u8; 32]>;

const DEFAULT_ROOT_HEX: &str = "0000000000000000000000000000000000000000000000000000000000000000";
const EVENTS_ROOT_FILE: &str = "events-root.msgpack";
const AMBIENT_EVENTS_ROOT_FILE: &str = "events-root-ambient.msgpack";
const AMBIENT_EVENTS_BLOB_DIR: &str = "ambient-blobs";
const PROFILE_SEARCH_ROOT_FILE: &str = "profile-search-root.msgpack";
const PROFILES_BY_PUBKEY_ROOT_FILE: &str = "profiles-by-pubkey-root.msgpack";
const UNKNOWN_FOLLOW_DISTANCE: u32 = 1000;
const DEFAULT_SOCIALGRAPH_MAP_SIZE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const SOCIALGRAPH_MAX_DBS: u32 = 16;
const PROFILE_SEARCH_INDEX_ORDER: usize = 64;
const PROFILE_SEARCH_PREFIX: &str = "p:";
const PROFILE_NAME_MAX_LENGTH: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventStorageClass {
    Public,
    Ambient,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EventQueryScope {
    PublicOnly,
    AmbientOnly,
    All,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct StoredCid {
    hash: [u8; 32],
    key: Option<[u8; 32]>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StoredProfileSearchEntry {
    pub pubkey: String,
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub nip05: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_distance: Option<u32>,
    pub created_at: u64,
    pub event_nhash: String,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SocialGraphStats {
    pub total_users: usize,
    pub root: Option<String>,
    pub total_follows: usize,
    pub max_depth: u32,
    pub size_by_distance: BTreeMap<u32, usize>,
    pub enabled: bool,
}

#[derive(Debug, Clone)]
struct DistanceCache {
    stats: SocialGraphStats,
    users_by_distance: BTreeMap<u32, Vec<[u8; 32]>>,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct UpstreamGraphBackendError(String);

pub struct SocialGraphStore {
    graph: StdMutex<HeedSocialGraph>,
    distance_cache: StdMutex<Option<DistanceCache>>,
    public_events: EventIndexBucket,
    ambient_events: EventIndexBucket,
    profile_index: ProfileIndexBucket,
    profile_index_overmute_threshold: StdMutex<f64>,
}

pub trait SocialGraphBackend: Send + Sync {
    fn stats(&self) -> Result<SocialGraphStats>;
    fn users_by_follow_distance(&self, distance: u32) -> Result<Vec<[u8; 32]>>;
    fn follow_distance(&self, pk_bytes: &[u8; 32]) -> Result<Option<u32>>;
    fn follow_list_created_at(&self, owner: &[u8; 32]) -> Result<Option<u64>>;
    fn followed_targets(&self, owner: &[u8; 32]) -> Result<UserSet>;
    fn is_overmuted_user(&self, user_pk: &[u8; 32], threshold: f64) -> Result<bool>;
    fn profile_search_root(&self) -> Result<Option<Cid>> {
        Ok(None)
    }
    fn snapshot_chunks(&self, root: &[u8; 32], options: &BinaryBudget) -> Result<Vec<Bytes>>;
    fn ingest_event(&self, event: &Event) -> Result<()>;
    fn ingest_event_with_storage_class(
        &self,
        event: &Event,
        storage_class: EventStorageClass,
    ) -> Result<()> {
        let _ = storage_class;
        self.ingest_event(event)
    }
    fn ingest_events(&self, events: &[Event]) -> Result<()> {
        for event in events {
            self.ingest_event(event)?;
        }
        Ok(())
    }
    fn ingest_events_with_storage_class(
        &self,
        events: &[Event],
        storage_class: EventStorageClass,
    ) -> Result<()> {
        for event in events {
            self.ingest_event_with_storage_class(event, storage_class)?;
        }
        Ok(())
    }
    fn ingest_graph_events(&self, events: &[Event]) -> Result<()> {
        self.ingest_events(events)
    }
    fn query_events(&self, filter: &Filter, limit: usize) -> Result<Vec<Event>>;
}

#[cfg(test)]
pub type TestLockGuard = MutexGuard<'static, ()>;

#[cfg(test)]
static NDB_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[cfg(test)]
pub fn test_lock() -> TestLockGuard {
    NDB_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|err| err.into_inner())
}

pub fn open_social_graph_store(data_dir: &Path) -> Result<Arc<SocialGraphStore>> {
    open_social_graph_store_with_mapsize(data_dir, None)
}

pub fn open_social_graph_store_with_mapsize(
    data_dir: &Path,
    mapsize_bytes: Option<u64>,
) -> Result<Arc<SocialGraphStore>> {
    let db_dir = data_dir.join("socialgraph");
    open_social_graph_store_at_path(&db_dir, mapsize_bytes)
}

pub fn open_social_graph_store_with_storage(
    data_dir: &Path,
    store: Arc<StorageRouter>,
    mapsize_bytes: Option<u64>,
) -> Result<Arc<SocialGraphStore>> {
    let db_dir = data_dir.join("socialgraph");
    open_social_graph_store_at_path_with_storage(&db_dir, store, mapsize_bytes)
}

pub fn open_embedded_social_graph_store_with_storage(
    data_dir: &Path,
    store: Arc<StorageRouter>,
    mapsize_bytes: Option<u64>,
) -> Result<Arc<SocialGraphStore>> {
    let db_dir = data_dir.join("socialgraph");
    open_social_graph_store_at_path_with_storage_and_env_flags(
        &db_dir,
        store,
        mapsize_bytes,
        EnvFlags::NO_LOCK,
    )
}

pub fn open_social_graph_store_at_path(
    db_dir: &Path,
    mapsize_bytes: Option<u64>,
) -> Result<Arc<SocialGraphStore>> {
    let config = hashtree_config::Config::load_or_default();
    let backend = &config.storage.backend;
    let local_store = Arc::new(
        LocalStore::new_with_lmdb_map_size(db_dir.join("blobs"), backend, mapsize_bytes)
            .map_err(|err| anyhow::anyhow!("Failed to create social graph blob store: {err}"))?,
    );
    let store = Arc::new(StorageRouter::new(local_store));
    open_social_graph_store_at_path_with_storage(db_dir, store, mapsize_bytes)
}

pub fn open_embedded_social_graph_store_at_path(
    db_dir: &Path,
    mapsize_bytes: Option<u64>,
) -> Result<Arc<SocialGraphStore>> {
    let local_store = Arc::new(
        LocalStore::new_with_lmdb_map_size(
            db_dir.join("blobs"),
            &hashtree_config::StorageBackend::Fs,
            mapsize_bytes,
        )
        .map_err(|err| anyhow::anyhow!("Failed to create social graph blob store: {err}"))?,
    );
    let store = Arc::new(StorageRouter::new(local_store));
    open_social_graph_store_at_path_with_storage_and_env_flags(
        db_dir,
        store,
        mapsize_bytes,
        EnvFlags::NO_LOCK,
    )
}

pub fn open_social_graph_store_at_path_with_storage(
    db_dir: &Path,
    store: Arc<StorageRouter>,
    mapsize_bytes: Option<u64>,
) -> Result<Arc<SocialGraphStore>> {
    open_social_graph_store_at_path_with_storage_and_env_flags(
        db_dir,
        store,
        mapsize_bytes,
        EnvFlags::empty(),
    )
}

fn open_social_graph_store_at_path_with_storage_and_env_flags(
    db_dir: &Path,
    store: Arc<StorageRouter>,
    mapsize_bytes: Option<u64>,
    env_flags: EnvFlags,
) -> Result<Arc<SocialGraphStore>> {
    let ambient_backend = store.local_store().backend();
    let ambient_local = Arc::new(
        LocalStore::new_with_lmdb_map_size(
            db_dir.join(AMBIENT_EVENTS_BLOB_DIR),
            &ambient_backend,
            mapsize_bytes,
        )
        .map_err(|err| {
            anyhow::anyhow!("Failed to create social graph ambient blob store: {err}")
        })?,
    );
    let ambient_store = Arc::new(StorageRouter::new(ambient_local));
    open_social_graph_store_at_path_with_storage_split_and_env_flags(
        db_dir,
        store,
        ambient_store,
        mapsize_bytes,
        env_flags,
    )
}

pub fn open_social_graph_store_at_path_with_storage_split(
    db_dir: &Path,
    public_store: Arc<StorageRouter>,
    ambient_store: Arc<StorageRouter>,
    mapsize_bytes: Option<u64>,
) -> Result<Arc<SocialGraphStore>> {
    open_social_graph_store_at_path_with_storage_split_and_env_flags(
        db_dir,
        public_store,
        ambient_store,
        mapsize_bytes,
        EnvFlags::empty(),
    )
}

fn open_social_graph_store_at_path_with_storage_split_and_env_flags(
    db_dir: &Path,
    public_store: Arc<StorageRouter>,
    ambient_store: Arc<StorageRouter>,
    mapsize_bytes: Option<u64>,
    env_flags: EnvFlags,
) -> Result<Arc<SocialGraphStore>> {
    std::fs::create_dir_all(db_dir)?;
    if let Some(size) = mapsize_bytes {
        ensure_social_graph_mapsize_with_env_flags(db_dir, size, env_flags)?;
    }
    let graph_map_size = social_graph_map_size(mapsize_bytes)?;
    let graph = unsafe {
        HeedSocialGraph::open_with_env_flags_and_map_size(
            db_dir,
            DEFAULT_ROOT_HEX,
            env_flags,
            graph_map_size,
        )
    }
    .context("open nostr-social-graph heed backend")?;

    Ok(Arc::new(SocialGraphStore {
        graph: StdMutex::new(graph),
        distance_cache: StdMutex::new(None),
        public_events: EventIndexBucket {
            event_store: NostrEventStore::new(Arc::clone(&public_store)),
            root_path: db_dir.join(EVENTS_ROOT_FILE),
        },
        ambient_events: EventIndexBucket {
            event_store: NostrEventStore::new(ambient_store),
            root_path: db_dir.join(AMBIENT_EVENTS_ROOT_FILE),
        },
        profile_index: ProfileIndexBucket {
            tree: HashTree::new(HashTreeConfig::new(Arc::clone(&public_store))),
            index: BTree::new(
                public_store,
                hashtree_index::BTreeOptions {
                    order: Some(PROFILE_SEARCH_INDEX_ORDER),
                },
            ),
            by_pubkey_root_path: db_dir.join(PROFILES_BY_PUBKEY_ROOT_FILE),
            search_root_path: db_dir.join(PROFILE_SEARCH_ROOT_FILE),
        },
        profile_index_overmute_threshold: StdMutex::new(1.0),
    }))
}

pub fn set_social_graph_root(store: &SocialGraphStore, pk_bytes: &[u8; 32]) {
    if let Err(err) = store.set_root(pk_bytes) {
        tracing::warn!("Failed to set social graph root: {err}");
    }
}

pub fn get_follow_distance(
    backend: &(impl SocialGraphBackend + ?Sized),
    pk_bytes: &[u8; 32],
) -> Option<u32> {
    backend.follow_distance(pk_bytes).ok().flatten()
}

pub fn get_follows(
    backend: &(impl SocialGraphBackend + ?Sized),
    pk_bytes: &[u8; 32],
) -> Vec<[u8; 32]> {
    match backend.followed_targets(pk_bytes) {
        Ok(set) => set.into_iter().collect(),
        Err(_) => Vec::new(),
    }
}

pub fn is_overmuted(
    backend: &(impl SocialGraphBackend + ?Sized),
    _root_pk: &[u8; 32],
    user_pk: &[u8; 32],
    threshold: f64,
) -> bool {
    backend
        .is_overmuted_user(user_pk, threshold)
        .unwrap_or(false)
}

pub fn ingest_event(backend: &(impl SocialGraphBackend + ?Sized), _sub_id: &str, event_json: &str) {
    let event = match Event::from_json(event_json) {
        Ok(event) => event,
        Err(_) => return,
    };

    if let Err(err) = backend.ingest_event(&event) {
        tracing::warn!("Failed to ingest social graph event: {err}");
    }
}

pub fn ingest_parsed_event(
    backend: &(impl SocialGraphBackend + ?Sized),
    event: &Event,
) -> Result<()> {
    backend.ingest_event(event)
}

pub fn ingest_parsed_event_with_storage_class(
    backend: &(impl SocialGraphBackend + ?Sized),
    event: &Event,
    storage_class: EventStorageClass,
) -> Result<()> {
    backend.ingest_event_with_storage_class(event, storage_class)
}

pub fn ingest_parsed_events(
    backend: &(impl SocialGraphBackend + ?Sized),
    events: &[Event],
) -> Result<()> {
    backend.ingest_events(events)
}

pub fn ingest_parsed_events_with_storage_class(
    backend: &(impl SocialGraphBackend + ?Sized),
    events: &[Event],
    storage_class: EventStorageClass,
) -> Result<()> {
    backend.ingest_events_with_storage_class(events, storage_class)
}

pub fn ingest_graph_parsed_events(
    backend: &(impl SocialGraphBackend + ?Sized),
    events: &[Event],
) -> Result<()> {
    backend.ingest_graph_events(events)
}

pub fn query_events(
    backend: &(impl SocialGraphBackend + ?Sized),
    filter: &Filter,
    limit: usize,
) -> Vec<Event> {
    backend.query_events(filter, limit).unwrap_or_default()
}

impl SocialGraphStore {
    pub fn set_profile_index_overmute_threshold(&self, threshold: f64) {
        *self
            .profile_index_overmute_threshold
            .lock()
            .expect("profile index overmute threshold") = threshold;
    }

    fn profile_index_overmute_threshold(&self) -> f64 {
        *self
            .profile_index_overmute_threshold
            .lock()
            .expect("profile index overmute threshold")
    }

    fn invalidate_distance_cache(&self) {
        *self.distance_cache.lock().unwrap() = None;
    }

    fn build_distance_cache(state: nostr_social_graph::SocialGraphState) -> Result<DistanceCache> {
        let unique_ids = state
            .unique_ids
            .into_iter()
            .map(|(pubkey, id)| decode_pubkey(&pubkey).map(|decoded| (id, decoded)))
            .collect::<Result<HashMap<_, _>>>()?;

        let mut users_by_distance = BTreeMap::new();
        let mut size_by_distance = BTreeMap::new();
        for (distance, users) in state.users_by_follow_distance {
            let decoded = users
                .into_iter()
                .filter_map(|id| unique_ids.get(&id).copied())
                .collect::<Vec<_>>();
            size_by_distance.insert(distance, decoded.len());
            users_by_distance.insert(distance, decoded);
        }

        let total_follows = state
            .followed_by_user
            .iter()
            .map(|(_, targets)| targets.len())
            .sum::<usize>();
        let total_users = size_by_distance.values().copied().sum();
        let max_depth = size_by_distance.keys().copied().max().unwrap_or_default();

        Ok(DistanceCache {
            stats: SocialGraphStats {
                total_users,
                root: Some(state.root),
                total_follows,
                max_depth,
                size_by_distance,
                enabled: true,
            },
            users_by_distance,
        })
    }

    fn load_distance_cache(&self) -> Result<DistanceCache> {
        if let Some(cache) = self.distance_cache.lock().unwrap().clone() {
            return Ok(cache);
        }

        let state = {
            let graph = self.graph.lock().unwrap();
            graph.export_state().context("export social graph state")?
        };
        let cache = Self::build_distance_cache(state)?;
        *self.distance_cache.lock().unwrap() = Some(cache.clone());
        Ok(cache)
    }

    fn set_root(&self, root: &[u8; 32]) -> Result<()> {
        let root_hex = hex::encode(root);
        {
            let mut graph = self.graph.lock().unwrap();
            if should_replace_placeholder_root(&graph)? {
                let fresh = SocialGraph::new(&root_hex);
                graph
                    .replace_state(&fresh.export_state())
                    .context("replace placeholder social graph root")?;
            } else {
                graph
                    .set_root(&root_hex)
                    .context("set nostr-social-graph root")?;
            }
        }
        self.invalidate_distance_cache();
        Ok(())
    }

    fn stats(&self) -> Result<SocialGraphStats> {
        Ok(self.load_distance_cache()?.stats)
    }

    fn follow_distance(&self, pk_bytes: &[u8; 32]) -> Result<Option<u32>> {
        let graph = self.graph.lock().unwrap();
        let distance = graph
            .get_follow_distance(&hex::encode(pk_bytes))
            .context("read social graph follow distance")?;
        Ok((distance != UNKNOWN_FOLLOW_DISTANCE).then_some(distance))
    }

    fn users_by_follow_distance(&self, distance: u32) -> Result<Vec<[u8; 32]>> {
        Ok(self
            .load_distance_cache()?
            .users_by_distance
            .get(&distance)
            .cloned()
            .unwrap_or_default())
    }

    fn follow_list_created_at(&self, owner: &[u8; 32]) -> Result<Option<u64>> {
        let graph = self.graph.lock().unwrap();
        graph
            .get_follow_list_created_at(&hex::encode(owner))
            .context("read social graph follow list timestamp")
    }

    fn followed_targets(&self, owner: &[u8; 32]) -> Result<UserSet> {
        let graph = self.graph.lock().unwrap();
        decode_pubkey_set(
            graph
                .get_followed_by_user(&hex::encode(owner))
                .context("read followed targets")?,
        )
    }

    fn is_overmuted_user(&self, user_pk: &[u8; 32], threshold: f64) -> Result<bool> {
        if threshold <= 0.0 {
            return Ok(false);
        }
        let graph = self.graph.lock().unwrap();
        graph
            .is_overmuted(&hex::encode(user_pk), threshold)
            .context("check social graph overmute")
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn profile_search_root(&self) -> Result<Option<Cid>> {
        self.profile_index.search_root()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn profiles_by_pubkey_root(&self) -> Result<Option<Cid>> {
        self.profile_index.by_pubkey_root()
    }

    pub fn public_events_root(&self) -> Result<Option<Cid>> {
        self.public_events.events_root()
    }

    #[cfg_attr(test, allow(dead_code))]
    pub(crate) fn public_events_root_for_write(&self) -> Result<Option<Cid>> {
        self.public_events.events_root_for_write()
    }

    pub(crate) fn write_public_events_root(&self, root: Option<&Cid>) -> Result<()> {
        self.public_events.write_events_root(root)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn latest_profile_event(&self, pubkey_hex: &str) -> Result<Option<Event>> {
        self.profile_index.profile_event_for_pubkey(pubkey_hex)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn profile_search_entries_for_prefix(
        &self,
        prefix: &str,
    ) -> Result<Vec<(String, StoredProfileSearchEntry)>> {
        self.profile_index.search_entries_for_prefix(prefix)
    }

    pub fn sync_profile_index_for_events(&self, events: &[Event]) -> Result<()> {
        self.update_profile_index_for_events(events)
    }

    pub(crate) fn rebuild_profile_index_for_events(&self, events: &[Event]) -> Result<()> {
        let latest_by_pubkey = self.filtered_latest_metadata_events_by_pubkey(events)?;
        let (by_pubkey_root, search_root) = self
            .profile_index
            .rebuild_profile_events_with_distances(latest_by_pubkey.into_values(), |event| {
                self.follow_distance(&event.pubkey.to_bytes())
            })?;
        self.profile_index
            .write_by_pubkey_root(by_pubkey_root.as_ref())?;
        self.profile_index.write_search_root(search_root.as_ref())?;
        Ok(())
    }

    pub(crate) async fn rebuild_profile_index_for_events_async(
        &self,
        events: &[Event],
    ) -> Result<()> {
        let latest_by_pubkey = self.filtered_latest_metadata_events_by_pubkey(events)?;
        let (by_pubkey_root, search_root) = self
            .profile_index
            .rebuild_profile_events_async_with_distances(latest_by_pubkey.into_values(), |event| {
                self.follow_distance(&event.pubkey.to_bytes())
            })
            .await?;
        self.profile_index
            .write_by_pubkey_root(by_pubkey_root.as_ref())?;
        self.profile_index.write_search_root(search_root.as_ref())?;
        Ok(())
    }

    pub fn rebuild_profile_index_from_stored_events(&self) -> Result<usize> {
        let public_events_root = self.public_events.events_root()?;
        let ambient_events_root = self.ambient_events.events_root()?;
        if public_events_root.is_none() && ambient_events_root.is_none() {
            self.profile_index.write_by_pubkey_root(None)?;
            self.profile_index.write_search_root(None)?;
            return Ok(0);
        }

        let mut events = Vec::new();
        for (bucket, root) in [
            (&self.public_events, public_events_root),
            (&self.ambient_events, ambient_events_root),
        ] {
            let Some(root) = root else {
                continue;
            };
            let stored = block_on(bucket.event_store.list_by_kind_lossy(
                Some(&root),
                Kind::Metadata.as_u16() as u32,
                ListEventsOptions::default(),
            ))
            .map_err(map_event_store_error)?;
            events.extend(
                stored
                    .into_iter()
                    .map(nostr_event_from_stored)
                    .collect::<Result<Vec<_>>>()?,
            );
        }

        let latest_count = self
            .filtered_latest_metadata_events_by_pubkey(&events)?
            .len();
        self.rebuild_profile_index_for_events(&events)?;
        Ok(latest_count)
    }

    pub async fn rebuild_profile_index_from_stored_events_async(&self) -> Result<usize> {
        let public_events_root = self.public_events.events_root()?;
        let ambient_events_root = self.ambient_events.events_root()?;
        if public_events_root.is_none() && ambient_events_root.is_none() {
            self.profile_index.write_by_pubkey_root(None)?;
            self.profile_index.write_search_root(None)?;
            return Ok(0);
        }

        let mut events = Vec::new();
        for (bucket, root) in [
            (&self.public_events, public_events_root),
            (&self.ambient_events, ambient_events_root),
        ] {
            let Some(root) = root else {
                continue;
            };
            let stored = bucket
                .event_store
                .list_by_kind_lossy(
                    Some(&root),
                    Kind::Metadata.as_u16() as u32,
                    ListEventsOptions::default(),
                )
                .await
                .map_err(map_event_store_error)?;
            events.extend(
                stored
                    .into_iter()
                    .map(nostr_event_from_stored)
                    .collect::<Result<Vec<_>>>()?,
            );
        }

        let latest_count = self
            .filtered_latest_metadata_events_by_pubkey(&events)?
            .len();
        self.rebuild_profile_index_for_events_async(&events).await?;
        Ok(latest_count)
    }

    pub fn rebuild_event_indexes_from_stored_events(&self) -> Result<(usize, usize)> {
        let public_count =
            self.rebuild_event_index_bucket_from_stored_events(&self.public_events)?;
        let ambient_count =
            self.rebuild_event_index_bucket_from_stored_events(&self.ambient_events)?;
        self.rebuild_profile_index_from_stored_events()?;
        Ok((public_count, ambient_count))
    }

    pub async fn rebuild_event_indexes_from_stored_events_async(&self) -> Result<(usize, usize)> {
        let public_count = self
            .rebuild_event_index_bucket_from_stored_events_async(&self.public_events)
            .await?;
        let ambient_count = self
            .rebuild_event_index_bucket_from_stored_events_async(&self.ambient_events)
            .await?;
        self.rebuild_profile_index_from_stored_events_async()
            .await?;
        Ok((public_count, ambient_count))
    }

    fn rebuild_event_index_bucket_from_stored_events(
        &self,
        bucket: &EventIndexBucket,
    ) -> Result<usize> {
        let Some(root) = bucket.events_root()? else {
            bucket.write_events_root(None)?;
            return Ok(0);
        };

        let manifest = match block_on(bucket.event_store.get_manifest(Some(&root))) {
            Ok(manifest) => manifest,
            Err(err) => {
                tracing::warn!(
                    "Clearing invalid social graph event index root {} before rebuild: {}",
                    hex::encode(root.hash),
                    err
                );
                bucket.write_events_root(None)?;
                return Ok(0);
            }
        };
        if manifest.by_kind_time_author.is_none() {
            let next_root = block_on(bucket.event_store.upgrade_manifest_indexes(Some(&root)))
                .map_err(map_event_store_error)?;
            if next_root.as_ref() != Some(&root) {
                bucket.write_events_root(next_root.as_ref())?;
                return Ok(0);
            }
        }

        let stored = block_on(
            bucket
                .event_store
                .list_recent_lossy(Some(&root), ListEventsOptions::default()),
        )
        .map_err(map_event_store_error)?;
        let count = stored.len();
        let next_root =
            block_on(bucket.event_store.build(None, stored)).map_err(map_event_store_error)?;
        bucket.write_events_root(next_root.as_ref())?;
        Ok(count)
    }

    async fn rebuild_event_index_bucket_from_stored_events_async(
        &self,
        bucket: &EventIndexBucket,
    ) -> Result<usize> {
        let Some(root) = bucket.events_root()? else {
            bucket.write_events_root(None)?;
            return Ok(0);
        };

        let manifest = match bucket.event_store.get_manifest(Some(&root)).await {
            Ok(manifest) => manifest,
            Err(err) => {
                tracing::warn!(
                    "Clearing invalid social graph event index root {} before rebuild: {}",
                    hex::encode(root.hash),
                    err
                );
                bucket.write_events_root(None)?;
                return Ok(0);
            }
        };
        if manifest.by_kind_time_author.is_none() {
            let next_root = bucket
                .event_store
                .upgrade_manifest_indexes(Some(&root))
                .await
                .map_err(map_event_store_error)?;
            if next_root.as_ref() != Some(&root) {
                bucket.write_events_root(next_root.as_ref())?;
                return Ok(0);
            }
        }

        let stored = bucket
            .event_store
            .list_recent_lossy(Some(&root), ListEventsOptions::default())
            .await
            .map_err(map_event_store_error)?;
        let count = stored.len();
        let next_root = bucket
            .event_store
            .build(None, stored)
            .await
            .map_err(map_event_store_error)?;
        bucket.write_events_root(next_root.as_ref())?;
        Ok(count)
    }

    fn update_profile_index_for_events(&self, events: &[Event]) -> Result<()> {
        let latest_by_pubkey = latest_metadata_events_by_pubkey(events);
        let threshold = self.profile_index_overmute_threshold();

        if latest_by_pubkey.is_empty() {
            return Ok(());
        }

        let mut by_pubkey_root = self.profile_index.by_pubkey_root()?;
        let mut search_root = self.profile_index.search_root()?;
        let mut changed = false;

        for event in latest_by_pubkey.into_values() {
            let overmuted = self.is_overmuted_user(&event.pubkey.to_bytes(), threshold)?;
            let (next_by_pubkey_root, next_search_root, updated) = if overmuted {
                self.profile_index.remove_profile_event(
                    by_pubkey_root.as_ref(),
                    search_root.as_ref(),
                    &event.pubkey.to_hex(),
                )?
            } else {
                self.profile_index.update_profile_event(
                    by_pubkey_root.as_ref(),
                    search_root.as_ref(),
                    event,
                    self.follow_distance(&event.pubkey.to_bytes())?,
                )?
            };
            if updated {
                by_pubkey_root = next_by_pubkey_root;
                search_root = next_search_root;
                changed = true;
            }
        }

        if changed {
            self.profile_index
                .write_by_pubkey_root(by_pubkey_root.as_ref())?;
            self.profile_index.write_search_root(search_root.as_ref())?;
        }

        Ok(())
    }

    fn filtered_latest_metadata_events_by_pubkey<'a>(
        &self,
        events: &'a [Event],
    ) -> Result<BTreeMap<String, &'a Event>> {
        let threshold = self.profile_index_overmute_threshold();
        let mut latest_by_pubkey = BTreeMap::<String, &Event>::new();
        for event in events.iter().filter(|event| event.kind == Kind::Metadata) {
            if self.is_overmuted_user(&event.pubkey.to_bytes(), threshold)? {
                continue;
            }
            let pubkey = event.pubkey.to_hex();
            match latest_by_pubkey.get(&pubkey) {
                Some(current) if compare_nostr_events(event, current).is_le() => {}
                _ => {
                    latest_by_pubkey.insert(pubkey, event);
                }
            }
        }
        Ok(latest_by_pubkey)
    }

    fn snapshot_chunks(&self, root: &[u8; 32], options: &BinaryBudget) -> Result<Vec<Bytes>> {
        let state = {
            let graph = self.graph.lock().unwrap();
            graph.export_state().context("export social graph state")?
        };
        let mut graph = SocialGraph::from_state(state).context("rebuild social graph state")?;
        let root_hex = hex::encode(root);
        if graph.get_root() != root_hex {
            graph
                .set_root(&root_hex)
                .context("set snapshot social graph root")?;
        }
        let chunks = graph
            .to_binary_chunks_with_budget(*options)
            .context("encode social graph snapshot")?;
        Ok(chunks.into_iter().map(Bytes::from).collect())
    }

    fn ingest_event(&self, event: &Event) -> Result<()> {
        self.ingest_event_with_storage_class(event, self.default_storage_class_for(event)?)
    }

    fn ingest_events(&self, events: &[Event]) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }

        let mut public = Vec::new();
        let mut ambient = Vec::new();
        for event in events {
            match self.default_storage_class_for(event)? {
                EventStorageClass::Public => public.push(event.clone()),
                EventStorageClass::Ambient => ambient.push(event.clone()),
            }
        }

        if !public.is_empty() {
            self.ingest_events_with_storage_class(&public, EventStorageClass::Public)?;
        }
        if !ambient.is_empty() {
            self.ingest_events_with_storage_class(&ambient, EventStorageClass::Ambient)?;
        }

        Ok(())
    }

    fn apply_graph_events_only(&self, events: &[Event]) -> Result<()> {
        let graph_events = events
            .iter()
            .filter(|event| is_social_graph_event(event.kind))
            .collect::<Vec<_>>();
        if graph_events.is_empty() {
            return Ok(());
        }

        {
            let mut graph = self.graph.lock().unwrap();
            let mut snapshot = SocialGraph::from_state(
                graph
                    .export_state()
                    .context("export social graph state for graph-only ingest")?,
            )
            .context("rebuild social graph state for graph-only ingest")?;
            for event in graph_events {
                snapshot.handle_event(&graph_event_from_nostr(event), true, 0.0);
            }
            graph
                .replace_state(&snapshot.export_state())
                .context("replace graph-only social graph state")?;
        }
        self.invalidate_distance_cache();
        Ok(())
    }

    fn query_events(&self, filter: &Filter, limit: usize) -> Result<Vec<Event>> {
        self.query_events_in_scope(filter, limit, EventQueryScope::All)
    }

    fn default_storage_class_for(&self, event: &Event) -> Result<EventStorageClass> {
        let graph = self.graph.lock().unwrap();
        let root_hex = graph.get_root().context("read social graph root")?;
        if root_hex != DEFAULT_ROOT_HEX && root_hex == event.pubkey.to_hex() {
            return Ok(EventStorageClass::Public);
        }
        Ok(EventStorageClass::Ambient)
    }

    fn bucket(&self, storage_class: EventStorageClass) -> &EventIndexBucket {
        match storage_class {
            EventStorageClass::Public => &self.public_events,
            EventStorageClass::Ambient => &self.ambient_events,
        }
    }

    fn ingest_event_with_storage_class(
        &self,
        event: &Event,
        storage_class: EventStorageClass,
    ) -> Result<()> {
        let current_root = self.bucket(storage_class).events_root_for_write()?;
        let next_root = self
            .bucket(storage_class)
            .store_event(current_root.as_ref(), event)?;
        self.bucket(storage_class)
            .write_events_root(Some(&next_root))?;

        if is_social_graph_event(event.kind) {
            {
                let mut graph = self.graph.lock().unwrap();
                graph
                    .handle_event(&graph_event_from_nostr(event), true, 0.0)
                    .context("ingest social graph event into nostr-social-graph")?;
            }
            self.invalidate_distance_cache();
        }

        self.update_profile_index_for_events(std::slice::from_ref(event))?;

        Ok(())
    }

    fn ingest_events_with_storage_class(
        &self,
        events: &[Event],
        storage_class: EventStorageClass,
    ) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }

        let bucket = self.bucket(storage_class);
        let current_root = bucket.events_root_for_write()?;
        let stored_events = events
            .iter()
            .map(stored_event_from_nostr)
            .collect::<Vec<_>>();
        let next_root = block_on(
            bucket
                .event_store
                .build(current_root.as_ref(), stored_events),
        )
        .map_err(map_event_store_error)?;
        bucket.write_events_root(next_root.as_ref())?;

        let graph_events = events
            .iter()
            .filter(|event| is_social_graph_event(event.kind))
            .collect::<Vec<_>>();
        if !graph_events.is_empty() {
            let mut graph = self.graph.lock().unwrap();
            let mut snapshot = SocialGraph::from_state(
                graph
                    .export_state()
                    .context("export social graph state for batch ingest")?,
            )
            .context("rebuild social graph state for batch ingest")?;
            for event in graph_events {
                snapshot.handle_event(&graph_event_from_nostr(event), true, 0.0);
            }
            graph
                .replace_state(&snapshot.export_state())
                .context("replace batched social graph state")?;
            self.invalidate_distance_cache();
        }

        self.update_profile_index_for_events(events)?;

        Ok(())
    }

    pub(crate) fn query_events_in_scope(
        &self,
        filter: &Filter,
        limit: usize,
        scope: EventQueryScope,
    ) -> Result<Vec<Event>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let buckets: &[&EventIndexBucket] = match scope {
            EventQueryScope::PublicOnly => &[&self.public_events],
            EventQueryScope::AmbientOnly => &[&self.ambient_events],
            EventQueryScope::All => &[&self.public_events, &self.ambient_events],
        };

        let mut candidates = Vec::new();
        for bucket in buckets {
            candidates.extend(bucket.query_events(filter, limit)?);
        }

        let mut deduped = dedupe_events(candidates);
        deduped.retain(|event| filter.match_event(event, Default::default()));
        deduped.truncate(limit);
        Ok(deduped)
    }
}

impl SocialGraphBackend for SocialGraphStore {
    fn stats(&self) -> Result<SocialGraphStats> {
        SocialGraphStore::stats(self)
    }

    fn users_by_follow_distance(&self, distance: u32) -> Result<Vec<[u8; 32]>> {
        SocialGraphStore::users_by_follow_distance(self, distance)
    }

    fn follow_distance(&self, pk_bytes: &[u8; 32]) -> Result<Option<u32>> {
        SocialGraphStore::follow_distance(self, pk_bytes)
    }

    fn follow_list_created_at(&self, owner: &[u8; 32]) -> Result<Option<u64>> {
        SocialGraphStore::follow_list_created_at(self, owner)
    }

    fn followed_targets(&self, owner: &[u8; 32]) -> Result<UserSet> {
        SocialGraphStore::followed_targets(self, owner)
    }

    fn is_overmuted_user(&self, user_pk: &[u8; 32], threshold: f64) -> Result<bool> {
        SocialGraphStore::is_overmuted_user(self, user_pk, threshold)
    }

    fn profile_search_root(&self) -> Result<Option<Cid>> {
        SocialGraphStore::profile_search_root(self)
    }

    fn snapshot_chunks(&self, root: &[u8; 32], options: &BinaryBudget) -> Result<Vec<Bytes>> {
        SocialGraphStore::snapshot_chunks(self, root, options)
    }

    fn ingest_event(&self, event: &Event) -> Result<()> {
        SocialGraphStore::ingest_event(self, event)
    }

    fn ingest_event_with_storage_class(
        &self,
        event: &Event,
        storage_class: EventStorageClass,
    ) -> Result<()> {
        SocialGraphStore::ingest_event_with_storage_class(self, event, storage_class)
    }

    fn ingest_events(&self, events: &[Event]) -> Result<()> {
        SocialGraphStore::ingest_events(self, events)
    }

    fn ingest_events_with_storage_class(
        &self,
        events: &[Event],
        storage_class: EventStorageClass,
    ) -> Result<()> {
        SocialGraphStore::ingest_events_with_storage_class(self, events, storage_class)
    }

    fn ingest_graph_events(&self, events: &[Event]) -> Result<()> {
        SocialGraphStore::apply_graph_events_only(self, events)
    }

    fn query_events(&self, filter: &Filter, limit: usize) -> Result<Vec<Event>> {
        SocialGraphStore::query_events(self, filter, limit)
    }
}

impl NostrSocialGraphBackend for SocialGraphStore {
    type Error = UpstreamGraphBackendError;

    fn get_root(&self) -> std::result::Result<String, Self::Error> {
        let graph = self.graph.lock().unwrap();
        graph
            .get_root()
            .context("read social graph root")
            .map_err(|err| UpstreamGraphBackendError(err.to_string()))
    }

    fn set_root(&mut self, root: &str) -> std::result::Result<(), Self::Error> {
        let root_bytes =
            decode_pubkey(root).map_err(|err| UpstreamGraphBackendError(err.to_string()))?;
        SocialGraphStore::set_root(self, &root_bytes)
            .map_err(|err| UpstreamGraphBackendError(err.to_string()))
    }

    fn handle_event(
        &mut self,
        event: &GraphEvent,
        allow_unknown_authors: bool,
        overmute_threshold: f64,
    ) -> std::result::Result<(), Self::Error> {
        {
            let mut graph = self.graph.lock().unwrap();
            graph
                .handle_event(event, allow_unknown_authors, overmute_threshold)
                .context("ingest social graph event into heed backend")
                .map_err(|err| UpstreamGraphBackendError(err.to_string()))?;
        }
        self.invalidate_distance_cache();
        Ok(())
    }

    fn get_follow_distance(&self, user: &str) -> std::result::Result<u32, Self::Error> {
        let graph = self.graph.lock().unwrap();
        graph
            .get_follow_distance(user)
            .context("read social graph follow distance")
            .map_err(|err| UpstreamGraphBackendError(err.to_string()))
    }

    fn is_following(
        &self,
        follower: &str,
        followed_user: &str,
    ) -> std::result::Result<bool, Self::Error> {
        let graph = self.graph.lock().unwrap();
        graph
            .is_following(follower, followed_user)
            .context("read social graph following edge")
            .map_err(|err| UpstreamGraphBackendError(err.to_string()))
    }

    fn get_followed_by_user(&self, user: &str) -> std::result::Result<Vec<String>, Self::Error> {
        let graph = self.graph.lock().unwrap();
        graph
            .get_followed_by_user(user)
            .context("read followed-by-user list")
            .map_err(|err| UpstreamGraphBackendError(err.to_string()))
    }

    fn get_followers_by_user(&self, user: &str) -> std::result::Result<Vec<String>, Self::Error> {
        let graph = self.graph.lock().unwrap();
        graph
            .get_followers_by_user(user)
            .context("read followers-by-user list")
            .map_err(|err| UpstreamGraphBackendError(err.to_string()))
    }

    fn get_muted_by_user(&self, user: &str) -> std::result::Result<Vec<String>, Self::Error> {
        let graph = self.graph.lock().unwrap();
        graph
            .get_muted_by_user(user)
            .context("read muted-by-user list")
            .map_err(|err| UpstreamGraphBackendError(err.to_string()))
    }

    fn get_user_muted_by(&self, user: &str) -> std::result::Result<Vec<String>, Self::Error> {
        let graph = self.graph.lock().unwrap();
        graph
            .get_user_muted_by(user)
            .context("read user-muted-by list")
            .map_err(|err| UpstreamGraphBackendError(err.to_string()))
    }

    fn get_follow_list_created_at(
        &self,
        user: &str,
    ) -> std::result::Result<Option<u64>, Self::Error> {
        let graph = self.graph.lock().unwrap();
        graph
            .get_follow_list_created_at(user)
            .context("read social graph follow list timestamp")
            .map_err(|err| UpstreamGraphBackendError(err.to_string()))
    }

    fn get_mute_list_created_at(
        &self,
        user: &str,
    ) -> std::result::Result<Option<u64>, Self::Error> {
        let graph = self.graph.lock().unwrap();
        graph
            .get_mute_list_created_at(user)
            .context("read social graph mute list timestamp")
            .map_err(|err| UpstreamGraphBackendError(err.to_string()))
    }

    fn is_overmuted(&self, user: &str, threshold: f64) -> std::result::Result<bool, Self::Error> {
        let graph = self.graph.lock().unwrap();
        graph
            .is_overmuted(user, threshold)
            .context("check social graph overmute")
            .map_err(|err| UpstreamGraphBackendError(err.to_string()))
    }
}

impl<T> SocialGraphBackend for Arc<T>
where
    T: SocialGraphBackend + ?Sized,
{
    fn stats(&self) -> Result<SocialGraphStats> {
        self.as_ref().stats()
    }

    fn users_by_follow_distance(&self, distance: u32) -> Result<Vec<[u8; 32]>> {
        self.as_ref().users_by_follow_distance(distance)
    }

    fn follow_distance(&self, pk_bytes: &[u8; 32]) -> Result<Option<u32>> {
        self.as_ref().follow_distance(pk_bytes)
    }

    fn follow_list_created_at(&self, owner: &[u8; 32]) -> Result<Option<u64>> {
        self.as_ref().follow_list_created_at(owner)
    }

    fn followed_targets(&self, owner: &[u8; 32]) -> Result<UserSet> {
        self.as_ref().followed_targets(owner)
    }

    fn is_overmuted_user(&self, user_pk: &[u8; 32], threshold: f64) -> Result<bool> {
        self.as_ref().is_overmuted_user(user_pk, threshold)
    }

    fn profile_search_root(&self) -> Result<Option<Cid>> {
        self.as_ref().profile_search_root()
    }

    fn snapshot_chunks(&self, root: &[u8; 32], options: &BinaryBudget) -> Result<Vec<Bytes>> {
        self.as_ref().snapshot_chunks(root, options)
    }

    fn ingest_event(&self, event: &Event) -> Result<()> {
        self.as_ref().ingest_event(event)
    }

    fn ingest_event_with_storage_class(
        &self,
        event: &Event,
        storage_class: EventStorageClass,
    ) -> Result<()> {
        self.as_ref()
            .ingest_event_with_storage_class(event, storage_class)
    }

    fn ingest_events(&self, events: &[Event]) -> Result<()> {
        self.as_ref().ingest_events(events)
    }

    fn ingest_events_with_storage_class(
        &self,
        events: &[Event],
        storage_class: EventStorageClass,
    ) -> Result<()> {
        self.as_ref()
            .ingest_events_with_storage_class(events, storage_class)
    }

    fn ingest_graph_events(&self, events: &[Event]) -> Result<()> {
        self.as_ref().ingest_graph_events(events)
    }

    fn query_events(&self, filter: &Filter, limit: usize) -> Result<Vec<Event>> {
        self.as_ref().query_events(filter, limit)
    }
}

fn should_replace_placeholder_root(graph: &HeedSocialGraph) -> Result<bool> {
    if graph.get_root().context("read current social graph root")? != DEFAULT_ROOT_HEX {
        return Ok(false);
    }

    let GraphStats {
        users,
        follows,
        mutes,
        ..
    } = graph.size().context("size social graph")?;
    Ok(users <= 1 && follows == 0 && mutes == 0)
}

fn decode_pubkey_set(values: Vec<String>) -> Result<UserSet> {
    let mut set = UserSet::new();
    for value in values {
        set.insert(decode_pubkey(&value)?);
    }
    Ok(set)
}

fn decode_pubkey(value: &str) -> Result<[u8; 32]> {
    let mut bytes = [0u8; 32];
    hex::decode_to_slice(value, &mut bytes)
        .with_context(|| format!("decode social graph pubkey {value}"))?;
    Ok(bytes)
}

fn is_social_graph_event(kind: Kind) -> bool {
    kind == Kind::ContactList || kind == Kind::MuteList
}

fn graph_event_from_nostr(event: &Event) -> GraphEvent {
    GraphEvent {
        created_at: event.created_at.as_secs(),
        content: event.content.clone(),
        tags: event
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
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
            .map(|tag| tag.as_slice().to_vec())
            .collect(),
        content: event.content.clone(),
        sig: event.sig.to_string(),
    }
}

fn nostr_event_from_stored(event: StoredNostrEvent) -> Result<Event> {
    let value = serde_json::json!({
        "id": event.id,
        "pubkey": event.pubkey,
        "created_at": event.created_at,
        "kind": event.kind,
        "tags": event.tags,
        "content": event.content,
        "sig": event.sig,
    });
    Event::from_json(value.to_string()).context("decode stored nostr event")
}

pub(crate) fn stored_event_to_nostr_event(event: StoredNostrEvent) -> Result<Event> {
    nostr_event_from_stored(event)
}

fn encode_cid(cid: &Cid) -> Result<Vec<u8>> {
    rmp_serde::to_vec_named(&StoredCid {
        hash: cid.hash,
        key: cid.key,
    })
    .context("encode social graph events root")
}

fn decode_cid(bytes: &[u8]) -> Result<Option<Cid>> {
    let stored: StoredCid =
        rmp_serde::from_slice(bytes).context("decode social graph events root")?;
    Ok(Some(Cid {
        hash: stored.hash,
        key: stored.key,
    }))
}

fn read_root_file(path: &Path) -> Result<Option<Cid>> {
    let Ok(bytes) = std::fs::read(path) else {
        return Ok(None);
    };
    decode_cid(&bytes)
}

fn write_root_file(path: &Path, root: Option<&Cid>) -> Result<()> {
    let Some(root) = root else {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        return Ok(());
    };

    let encoded = encode_cid(root)?;
    let tmp_path = path.with_extension("tmp");
    std::fs::write(&tmp_path, encoded)?;
    std::fs::rename(tmp_path, path)?;
    Ok(())
}

fn normalize_profile_name(value: &serde_json::Value) -> Option<String> {
    let raw = value.as_str()?;
    let trimmed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(PROFILE_NAME_MAX_LENGTH).collect())
}

fn extract_profile_names(profile: &serde_json::Map<String, serde_json::Value>) -> Vec<String> {
    let mut names = Vec::new();
    let mut seen = HashSet::new();

    for key in ["display_name", "displayName", "name", "username"] {
        let Some(value) = profile.get(key).and_then(normalize_profile_name) else {
            continue;
        };
        let lowered = value.to_lowercase();
        if seen.insert(lowered) {
            names.push(value);
        }
    }

    names
}

fn should_reject_profile_nip05(local_part: &str, primary_name: &str) -> bool {
    if local_part.len() == 1 || local_part.starts_with("npub1") {
        return true;
    }

    primary_name
        .to_lowercase()
        .split_whitespace()
        .collect::<String>()
        .contains(local_part)
}

fn normalize_profile_nip05(
    profile: &serde_json::Map<String, serde_json::Value>,
    primary_name: Option<&str>,
) -> Option<String> {
    let raw = profile.get("nip05")?.as_str()?;
    let local_part = raw.split('@').next()?.trim().to_lowercase();
    if local_part.is_empty() {
        return None;
    }
    let truncated: String = local_part.chars().take(PROFILE_NAME_MAX_LENGTH).collect();
    if truncated.is_empty() {
        return None;
    }
    if primary_name.is_some_and(|name| should_reject_profile_nip05(&truncated, name)) {
        return None;
    }
    Some(truncated)
}

fn is_search_stop_word(word: &str) -> bool {
    matches!(
        word,
        "a" | "an"
            | "the"
            | "and"
            | "or"
            | "but"
            | "in"
            | "on"
            | "at"
            | "to"
            | "for"
            | "of"
            | "with"
            | "by"
            | "from"
            | "is"
            | "it"
            | "as"
            | "be"
            | "was"
            | "are"
            | "this"
            | "that"
            | "these"
            | "those"
            | "i"
            | "you"
            | "he"
            | "she"
            | "we"
            | "they"
            | "my"
            | "your"
            | "his"
            | "her"
            | "its"
            | "our"
            | "their"
            | "what"
            | "which"
            | "who"
            | "whom"
            | "how"
            | "when"
            | "where"
            | "why"
            | "will"
            | "would"
            | "could"
            | "should"
            | "can"
            | "may"
            | "might"
            | "must"
            | "have"
            | "has"
            | "had"
            | "do"
            | "does"
            | "did"
            | "been"
            | "being"
            | "get"
            | "got"
            | "just"
            | "now"
            | "then"
            | "so"
            | "if"
            | "not"
            | "no"
            | "yes"
            | "all"
            | "any"
            | "some"
            | "more"
            | "most"
            | "other"
            | "into"
            | "over"
            | "after"
            | "before"
            | "about"
            | "up"
            | "down"
            | "out"
            | "off"
            | "through"
            | "during"
            | "under"
            | "again"
            | "further"
            | "once"
    )
}

fn is_pure_search_number(word: &str) -> bool {
    if !word.chars().all(|ch| ch.is_ascii_digit()) {
        return false;
    }
    !(word.len() == 4
        && word
            .parse::<u16>()
            .is_ok_and(|year| (1900..=2099).contains(&year)))
}

fn split_compound_search_word(word: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = word.chars().collect();

    for (index, ch) in chars.iter().copied().enumerate() {
        let split_before = current.chars().last().is_some_and(|prev| {
            (prev.is_lowercase() && ch.is_uppercase())
                || (prev.is_ascii_digit() && ch.is_alphabetic())
                || (prev.is_alphabetic() && ch.is_ascii_digit())
                || (prev.is_uppercase()
                    && ch.is_uppercase()
                    && chars.get(index + 1).is_some_and(|next| next.is_lowercase()))
        });

        if split_before && !current.is_empty() {
            parts.push(std::mem::take(&mut current));
        }

        current.push(ch);
    }

    if !current.is_empty() {
        parts.push(current);
    }

    parts
}

fn parse_search_keywords(text: &str) -> Vec<String> {
    let mut keywords = Vec::new();
    let mut seen = HashSet::new();

    for word in text
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| !word.is_empty())
    {
        let mut variants = Vec::with_capacity(1 + word.len() / 4);
        variants.push(word.to_lowercase());
        variants.extend(
            split_compound_search_word(word)
                .into_iter()
                .map(|part| part.to_lowercase()),
        );

        for lowered in variants {
            if lowered.chars().count() < 2
                || is_search_stop_word(&lowered)
                || is_pure_search_number(&lowered)
            {
                continue;
            }
            if seen.insert(lowered.clone()) {
                keywords.push(lowered);
            }
        }
    }

    keywords
}

fn profile_search_terms_for_event(event: &Event) -> Vec<String> {
    let profile = match serde_json::from_str::<serde_json::Value>(&event.content) {
        Ok(serde_json::Value::Object(profile)) => profile,
        _ => serde_json::Map::new(),
    };
    let names = extract_profile_names(&profile);
    let primary_name = names.first().map(String::as_str);
    let mut parts = Vec::new();
    if let Some(name) = primary_name {
        parts.push(name.to_string());
    }
    if let Some(nip05) = normalize_profile_nip05(&profile, primary_name) {
        parts.push(nip05);
    }
    parts.push(event.pubkey.to_hex());
    if names.len() > 1 {
        parts.extend(names.into_iter().skip(1));
    }
    parse_search_keywords(&parts.join(" "))
}

fn compare_nostr_events(left: &Event, right: &Event) -> std::cmp::Ordering {
    left.created_at
        .as_secs()
        .cmp(&right.created_at.as_secs())
        .then_with(|| left.id.to_hex().cmp(&right.id.to_hex()))
}

fn map_event_store_error(err: NostrEventStoreError) -> anyhow::Error {
    anyhow::anyhow!("nostr event store error: {err}")
}

#[cfg(test)]
fn ensure_social_graph_mapsize(db_dir: &Path, requested_bytes: u64) -> Result<()> {
    ensure_social_graph_mapsize_with_env_flags(db_dir, requested_bytes, EnvFlags::empty())
}

fn ensure_social_graph_mapsize_with_env_flags(
    db_dir: &Path,
    requested_bytes: u64,
    env_flags: EnvFlags,
) -> Result<()> {
    let map_size = social_graph_map_size(Some(requested_bytes))?;

    let mut options = heed::EnvOpenOptions::new();
    options.map_size(map_size).max_dbs(SOCIALGRAPH_MAX_DBS);
    unsafe {
        options.flags(env_flags);
    }
    let env = unsafe { options.open(db_dir) }.context("open social graph LMDB env for resize")?;
    if env.info().map_size < map_size {
        unsafe { env.resize(map_size) }.context("resize social graph LMDB env")?;
    }

    Ok(())
}

fn social_graph_map_size(requested_bytes: Option<u64>) -> Result<usize> {
    let requested = requested_bytes
        .unwrap_or(DEFAULT_SOCIALGRAPH_MAP_SIZE_BYTES)
        .max(DEFAULT_SOCIALGRAPH_MAP_SIZE_BYTES);
    let page_size = page_size_bytes() as u64;
    let rounded = requested
        .checked_add(page_size.saturating_sub(1))
        .map(|size| size / page_size * page_size)
        .unwrap_or(requested);
    usize::try_from(rounded).context("social graph mapsize exceeds usize")
}

fn page_size_bytes() -> usize {
    page_size::get_granularity()
}

#[cfg(test)]
mod tests;
