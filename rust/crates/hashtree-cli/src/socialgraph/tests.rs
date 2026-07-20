use super::*;
use async_trait::async_trait;
use hashtree_config::StorageBackend;
use hashtree_core::{Hash, MemoryStore, Store, StoreError};
use hashtree_nostr::NostrEventStoreOptions;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use nostr::{EventBuilder, JsonUtil, Keys, Tag, Timestamp};
use tempfile::TempDir;

macro_rules! event_builder {
    ($kind:expr, $content:expr $(,)?) => {
        EventBuilder::new($kind, $content)
    };
    ($kind:expr, $content:expr, $tags:expr $(,)?) => {
        EventBuilder::new($kind, $content).tags($tags)
    };
}

const WELLORDER_FIXTURE_URL: &str =
    "https://wellorder.xyz/nostr/nostr-wellorder-early-500k-v1.jsonl.bz2";

#[derive(Debug, Clone, Default)]
struct ReadTraceSnapshot {
    get_calls: u64,
    total_bytes: u64,
    unique_blocks: usize,
    unique_bytes: u64,
    cache_hits: u64,
    remote_fetches: u64,
    remote_bytes: u64,
}

#[derive(Debug, Default)]
struct ReadTraceState {
    get_calls: u64,
    total_bytes: u64,
    unique_hashes: HashSet<Hash>,
    unique_bytes: u64,
    cache_hits: u64,
    remote_fetches: u64,
    remote_bytes: u64,
}

#[derive(Debug)]
struct CountingStore<S: Store> {
    base: Arc<S>,
    state: Mutex<ReadTraceState>,
}

impl<S: Store> CountingStore<S> {
    fn new(base: Arc<S>) -> Self {
        Self {
            base,
            state: Mutex::new(ReadTraceState::default()),
        }
    }

    fn reset(&self) {
        *self.state.lock().unwrap() = ReadTraceState::default();
    }

    fn snapshot(&self) -> ReadTraceSnapshot {
        let state = self.state.lock().unwrap();
        ReadTraceSnapshot {
            get_calls: state.get_calls,
            total_bytes: state.total_bytes,
            unique_blocks: state.unique_hashes.len(),
            unique_bytes: state.unique_bytes,
            cache_hits: state.cache_hits,
            remote_fetches: state.remote_fetches,
            remote_bytes: state.remote_bytes,
        }
    }

    fn record_read(&self, hash: &Hash, bytes: usize) {
        let mut state = self.state.lock().unwrap();
        state.get_calls += 1;
        state.total_bytes += bytes as u64;
        if state.unique_hashes.insert(*hash) {
            state.unique_bytes += bytes as u64;
        }
    }
}

#[async_trait]
impl<S: Store> Store for CountingStore<S> {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        self.base.put(hash, data).await
    }

    async fn put_many(&self, items: Vec<(Hash, Vec<u8>)>) -> Result<usize, StoreError> {
        self.base.put_many(items).await
    }

    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        let data = self.base.get(hash).await?;
        if let Some(bytes) = data.as_ref() {
            self.record_read(hash, bytes.len());
        }
        Ok(data)
    }

    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.base.has(hash).await
    }

    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.base.delete(hash).await
    }
}

#[derive(Debug)]
struct ReadThroughStore<R: Store> {
    cache: Arc<MemoryStore>,
    remote: Arc<R>,
    state: Mutex<ReadTraceState>,
}

impl<R: Store> ReadThroughStore<R> {
    fn new(cache: Arc<MemoryStore>, remote: Arc<R>) -> Self {
        Self {
            cache,
            remote,
            state: Mutex::new(ReadTraceState::default()),
        }
    }

    fn snapshot(&self) -> ReadTraceSnapshot {
        let state = self.state.lock().unwrap();
        ReadTraceSnapshot {
            get_calls: state.get_calls,
            total_bytes: state.total_bytes,
            unique_blocks: state.unique_hashes.len(),
            unique_bytes: state.unique_bytes,
            cache_hits: state.cache_hits,
            remote_fetches: state.remote_fetches,
            remote_bytes: state.remote_bytes,
        }
    }
}

#[async_trait]
impl<R: Store> Store for ReadThroughStore<R> {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        self.cache.put(hash, data).await
    }

    async fn put_many(&self, items: Vec<(Hash, Vec<u8>)>) -> Result<usize, StoreError> {
        self.cache.put_many(items).await
    }

    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        {
            let mut state = self.state.lock().unwrap();
            state.get_calls += 1;
        }

        if let Some(bytes) = self.cache.get(hash).await? {
            let mut state = self.state.lock().unwrap();
            state.cache_hits += 1;
            state.total_bytes += bytes.len() as u64;
            if state.unique_hashes.insert(*hash) {
                state.unique_bytes += bytes.len() as u64;
            }
            return Ok(Some(bytes));
        }

        let data = self.remote.get(hash).await?;
        if let Some(bytes) = data.as_ref() {
            let _ = self.cache.put(*hash, bytes.clone()).await?;
            let mut state = self.state.lock().unwrap();
            state.remote_fetches += 1;
            state.remote_bytes += bytes.len() as u64;
            state.total_bytes += bytes.len() as u64;
            if state.unique_hashes.insert(*hash) {
                state.unique_bytes += bytes.len() as u64;
            }
        }
        Ok(data)
    }

    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        if self.cache.has(hash).await? {
            return Ok(true);
        }
        self.remote.has(hash).await
    }

    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        let cache_deleted = self.cache.delete(hash).await?;
        let remote_deleted = self.remote.delete(hash).await?;
        Ok(cache_deleted || remote_deleted)
    }
}

#[derive(Debug, Clone)]
enum BenchmarkQueryCase {
    ById {
        id: String,
    },
    ByAuthor {
        pubkey: String,
        limit: usize,
    },
    ByAuthorKind {
        pubkey: String,
        kind: u32,
        limit: usize,
    },
    ByKind {
        kind: u32,
        limit: usize,
    },
    ByTag {
        tag_name: String,
        tag_value: String,
        limit: usize,
    },
    Recent {
        limit: usize,
    },
    Replaceable {
        pubkey: String,
        kind: u32,
    },
    ParameterizedReplaceable {
        pubkey: String,
        kind: u32,
        d_tag: String,
    },
}

impl BenchmarkQueryCase {
    fn name(&self) -> &'static str {
        match self {
            BenchmarkQueryCase::ById { .. } => "by_id",
            BenchmarkQueryCase::ByAuthor { .. } => "by_author",
            BenchmarkQueryCase::ByAuthorKind { .. } => "by_author_kind",
            BenchmarkQueryCase::ByKind { .. } => "by_kind",
            BenchmarkQueryCase::ByTag { .. } => "by_tag",
            BenchmarkQueryCase::Recent { .. } => "recent",
            BenchmarkQueryCase::Replaceable { .. } => "replaceable",
            BenchmarkQueryCase::ParameterizedReplaceable { .. } => "parameterized_replaceable",
        }
    }

    async fn execute<S: Store>(
        &self,
        store: &NostrEventStore<S>,
        root: &Cid,
    ) -> Result<usize, NostrEventStoreError> {
        match self {
            BenchmarkQueryCase::ById { id } => {
                Ok(store.get_by_id(Some(root), id).await?.into_iter().count())
            }
            BenchmarkQueryCase::ByAuthor { pubkey, limit } => Ok(store
                .list_by_author(
                    Some(root),
                    pubkey,
                    ListEventsOptions {
                        limit: Some(*limit),
                        ..Default::default()
                    },
                )
                .await?
                .len()),
            BenchmarkQueryCase::ByAuthorKind {
                pubkey,
                kind,
                limit,
            } => Ok(store
                .list_by_author_and_kind(
                    Some(root),
                    pubkey,
                    *kind,
                    ListEventsOptions {
                        limit: Some(*limit),
                        ..Default::default()
                    },
                )
                .await?
                .len()),
            BenchmarkQueryCase::ByKind { kind, limit } => Ok(store
                .list_by_kind(
                    Some(root),
                    *kind,
                    ListEventsOptions {
                        limit: Some(*limit),
                        ..Default::default()
                    },
                )
                .await?
                .len()),
            BenchmarkQueryCase::ByTag {
                tag_name,
                tag_value,
                limit,
            } => Ok(store
                .list_by_tag(
                    Some(root),
                    tag_name,
                    tag_value,
                    ListEventsOptions {
                        limit: Some(*limit),
                        ..Default::default()
                    },
                )
                .await?
                .len()),
            BenchmarkQueryCase::Recent { limit } => Ok(store
                .list_recent(
                    Some(root),
                    ListEventsOptions {
                        limit: Some(*limit),
                        ..Default::default()
                    },
                )
                .await?
                .len()),
            BenchmarkQueryCase::Replaceable { pubkey, kind } => Ok(store
                .get_replaceable(Some(root), pubkey, *kind)
                .await?
                .into_iter()
                .count()),
            BenchmarkQueryCase::ParameterizedReplaceable {
                pubkey,
                kind,
                d_tag,
            } => Ok(store
                .get_parameterized_replaceable(Some(root), pubkey, *kind, d_tag)
                .await?
                .into_iter()
                .count()),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct NetworkModel {
    name: &'static str,
    rtt_ms: f64,
    bandwidth_mib_per_s: f64,
}

#[derive(Debug, Clone)]
struct QueryBenchmarkResult {
    average_duration: Duration,
    p95_duration: Duration,
    reads: ReadTraceSnapshot,
}

const NETWORK_MODELS: [NetworkModel; 3] = [
    NetworkModel {
        name: "lan",
        rtt_ms: 2.0,
        bandwidth_mib_per_s: 100.0,
    },
    NetworkModel {
        name: "wan",
        rtt_ms: 40.0,
        bandwidth_mib_per_s: 20.0,
    },
    NetworkModel {
        name: "slow",
        rtt_ms: 120.0,
        bandwidth_mib_per_s: 5.0,
    },
];

#[test]
fn test_open_social_graph_store() {
    let _guard = test_lock_blocking();
    let tmp = TempDir::new().unwrap();
    let graph_store = open_test_social_graph_store(tmp.path()).unwrap();
    assert_eq!(Arc::strong_count(&graph_store), 1);
}

#[test]
fn test_set_root_and_get_follow_distance() {
    let _guard = test_lock_blocking();
    let tmp = TempDir::new().unwrap();
    let graph_store = open_test_social_graph_store(tmp.path()).unwrap();
    let root_pk = [1u8; 32];
    set_social_graph_root(&graph_store, &root_pk);
    assert_eq!(get_follow_distance(&graph_store, &root_pk), Some(0));
}

#[test]
fn test_ingest_event_updates_follows_and_mutes() {
    let _guard = test_lock_blocking();
    let tmp = TempDir::new().unwrap();
    let graph_store = open_test_social_graph_store(tmp.path()).unwrap();

    let root_keys = Keys::generate();
    let alice_keys = Keys::generate();
    let bob_keys = Keys::generate();

    let root_pk = root_keys.public_key().to_bytes();
    set_social_graph_root(&graph_store, &root_pk);

    let follow = event_builder!(
        Kind::ContactList,
        "",
        vec![Tag::public_key(alice_keys.public_key())],
    )
    .custom_created_at(Timestamp::from_secs(10))
    .sign_with_keys(&root_keys)
    .unwrap();
    ingest_event(&graph_store, "follow", &follow.as_json());

    let mute = event_builder!(
        Kind::MuteList,
        "",
        vec![Tag::public_key(bob_keys.public_key())],
    )
    .custom_created_at(Timestamp::from_secs(11))
    .sign_with_keys(&root_keys)
    .unwrap();
    ingest_event(&graph_store, "mute", &mute.as_json());

    assert_eq!(
        get_follow_distance(&graph_store, &alice_keys.public_key().to_bytes()),
        Some(1)
    );
    assert!(is_overmuted(
        &graph_store,
        &root_pk,
        &bob_keys.public_key().to_bytes(),
        1.0
    ));
}

#[test]
fn test_metadata_ingest_builds_profile_search_index_and_replaces_old_terms() {
    let _guard = test_lock_blocking();
    let tmp = TempDir::new().unwrap();
    let graph_store = open_test_social_graph_store(tmp.path()).unwrap();
    let keys = Keys::generate();

    let older = event_builder!(
        Kind::Metadata,
        serde_json::json!({
            "display_name": "sirius",
            "name": "Martti Malmi",
            "username": "mmalmi",
            "nip05": "siriusdev@iris.to",
        })
        .to_string(),
        [],
    )
    .custom_created_at(Timestamp::from_secs(5))
    .sign_with_keys(&keys)
    .unwrap();
    let newer = event_builder!(
        Kind::Metadata,
        serde_json::json!({
            "display_name": "bird",
            "nip05": "birdman@iris.to",
        })
        .to_string(),
        [],
    )
    .custom_created_at(Timestamp::from_secs(6))
    .sign_with_keys(&keys)
    .unwrap();

    ingest_parsed_event(&graph_store, &older).unwrap();

    let pubkey = keys.public_key().to_hex();
    let entries = graph_store
        .profile_search_entries_for_prefix("p:sirius")
        .unwrap();
    assert!(entries
        .iter()
        .any(|(key, entry)| key == &format!("p:sirius:{pubkey}") && entry.name == "sirius"));
    assert!(entries.iter().any(|(key, entry)| {
        key == &format!("p:siriusdev:{pubkey}")
            && entry.nip05.as_deref() == Some("siriusdev")
            && entry.aliases == vec!["Martti Malmi".to_string(), "mmalmi".to_string()]
            && entry.event_nhash.starts_with("nhash1")
    }));
    assert!(entries.iter().all(|(_, entry)| entry.pubkey == pubkey));
    assert_eq!(
        graph_store
            .latest_profile_event(&pubkey)
            .unwrap()
            .expect("latest mirrored profile")
            .id,
        older.id
    );

    ingest_parsed_event(&graph_store, &newer).unwrap();

    assert!(graph_store
        .profile_search_entries_for_prefix("p:sirius")
        .unwrap()
        .is_empty());
    let bird_entries = graph_store
        .profile_search_entries_for_prefix("p:bird")
        .unwrap();
    assert_eq!(bird_entries.len(), 2);
    assert!(bird_entries
        .iter()
        .any(|(key, entry)| key == &format!("p:bird:{pubkey}") && entry.name == "bird"));
    assert!(bird_entries.iter().any(|(key, entry)| {
        key == &format!("p:birdman:{pubkey}")
            && entry.nip05.as_deref() == Some("birdman")
            && entry.aliases.is_empty()
    }));
    assert_eq!(
        graph_store
            .latest_profile_event(&pubkey)
            .unwrap()
            .expect("latest mirrored profile")
            .id,
        newer.id
    );
}

#[test]
fn test_metadata_batch_updates_profile_search_terms_together() {
    let _guard = test_lock_blocking();
    let tmp = TempDir::new().unwrap();
    let graph_store = open_test_social_graph_store(tmp.path()).unwrap();
    let alice_keys = Keys::generate();
    let bob_keys = Keys::generate();

    let alice_old = event_builder!(
        Kind::Metadata,
        serde_json::json!({ "name": "aliceold" }).to_string(),
        [],
    )
    .custom_created_at(Timestamp::from_secs(5))
    .sign_with_keys(&alice_keys)
    .unwrap();
    let alice_new = event_builder!(
        Kind::Metadata,
        serde_json::json!({ "name": "alicenew" }).to_string(),
        [],
    )
    .custom_created_at(Timestamp::from_secs(6))
    .sign_with_keys(&alice_keys)
    .unwrap();
    let bob = event_builder!(
        Kind::Metadata,
        serde_json::json!({ "name": "bobnew" }).to_string(),
        [],
    )
    .custom_created_at(Timestamp::from_secs(6))
    .sign_with_keys(&bob_keys)
    .unwrap();
    graph_store
        .sync_profile_index_for_events(std::slice::from_ref(&alice_old))
        .unwrap();
    graph_store
        .sync_profile_index_for_events(&[alice_new.clone(), bob.clone()])
        .unwrap();

    assert!(graph_store
        .profile_search_entries_for_prefix("p:aliceold")
        .unwrap()
        .is_empty());
    assert_eq!(
        graph_store
            .profile_search_entries_for_prefix("p:alicenew")
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        graph_store
            .profile_search_entries_for_prefix("p:bobnew")
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn test_profile_search_entries_include_follow_distance() {
    let _guard = test_lock_blocking();
    let tmp = TempDir::new().unwrap();
    let graph_store = open_test_social_graph_store(tmp.path()).unwrap();
    let root_keys = Keys::generate();
    let alice_keys = Keys::generate();
    let stranger_keys = Keys::generate();

    set_social_graph_root(&graph_store, &root_keys.public_key().to_bytes());

    let follow_alice = event_builder!(
        Kind::ContactList,
        "",
        vec![Tag::public_key(alice_keys.public_key())],
    )
    .custom_created_at(Timestamp::from_secs(4))
    .sign_with_keys(&root_keys)
    .unwrap();

    let alice_profile = event_builder!(
        Kind::Metadata,
        serde_json::json!({
            "display_name": "Alice Search",
        })
        .to_string(),
        [],
    )
    .custom_created_at(Timestamp::from_secs(5))
    .sign_with_keys(&alice_keys)
    .unwrap();
    let stranger_profile = event_builder!(
        Kind::Metadata,
        serde_json::json!({
            "display_name": "Stranger Search",
        })
        .to_string(),
        [],
    )
    .custom_created_at(Timestamp::from_secs(5))
    .sign_with_keys(&stranger_keys)
    .unwrap();
    ingest_parsed_events(
        &graph_store,
        &[follow_alice, alice_profile, stranger_profile],
    )
    .unwrap();

    let alice_pubkey = alice_keys.public_key().to_hex();
    assert!(graph_store
        .profile_search_entries_for_prefix("p:alice")
        .unwrap()
        .iter()
        .any(|(key, entry)| {
            key == &format!("p:alice:{alice_pubkey}") && entry.follow_distance == Some(1)
        }));

    let stranger_pubkey = stranger_keys.public_key().to_hex();
    assert!(graph_store
        .profile_search_entries_for_prefix("p:stranger")
        .unwrap()
        .iter()
        .any(|(key, entry)| {
            key == &format!("p:stranger:{stranger_pubkey}") && entry.follow_distance.is_none()
        }));
}

#[test]
fn test_ambient_metadata_events_are_mirrored_into_public_profile_index() {
    let _guard = test_lock_blocking();
    let tmp = TempDir::new().unwrap();
    let graph_store = open_test_social_graph_store(tmp.path()).unwrap();
    let keys = Keys::generate();

    let profile = event_builder!(
        Kind::Metadata,
        serde_json::json!({
            "display_name": "ambient bird",
        })
        .to_string(),
        [],
    )
    .custom_created_at(Timestamp::from_secs(5))
    .sign_with_keys(&keys)
    .unwrap();

    ingest_parsed_event_with_storage_class(&graph_store, &profile, EventStorageClass::Ambient)
        .unwrap();

    let pubkey = keys.public_key().to_hex();
    let mirrored = graph_store
        .latest_profile_event(&pubkey)
        .unwrap()
        .expect("mirrored ambient profile");
    assert_eq!(mirrored.id, profile.id);
    assert_eq!(
        graph_store
            .profile_search_entries_for_prefix("p:ambient")
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn test_metadata_ingest_splits_compound_profile_terms_without_losing_whole_token() {
    let _guard = test_lock_blocking();
    let tmp = TempDir::new().unwrap();
    let graph_store = open_test_social_graph_store(tmp.path()).unwrap();
    let keys = Keys::generate();

    let profile = event_builder!(
        Kind::Metadata,
        serde_json::json!({
            "display_name": "SirLibre",
            "username": "XMLHttpRequest42",
        })
        .to_string(),
        [],
    )
    .custom_created_at(Timestamp::from_secs(5))
    .sign_with_keys(&keys)
    .unwrap();

    ingest_parsed_event(&graph_store, &profile).unwrap();

    let pubkey = keys.public_key().to_hex();
    assert!(graph_store
        .profile_search_entries_for_prefix("p:sirlibre")
        .unwrap()
        .iter()
        .any(|(key, entry)| key == &format!("p:sirlibre:{pubkey}") && entry.name == "SirLibre"));
    assert!(graph_store
        .profile_search_entries_for_prefix("p:libre")
        .unwrap()
        .iter()
        .any(|(key, entry)| key == &format!("p:libre:{pubkey}") && entry.name == "SirLibre"));
    assert!(graph_store
        .profile_search_entries_for_prefix("p:xml")
        .unwrap()
        .iter()
        .any(|(key, entry)| {
            key == &format!("p:xml:{pubkey}")
                && entry.aliases == vec!["XMLHttpRequest42".to_string()]
        }));
    assert!(graph_store
        .profile_search_entries_for_prefix("p:request")
        .unwrap()
        .iter()
        .any(|(key, entry)| {
            key == &format!("p:request:{pubkey}")
                && entry.aliases == vec!["XMLHttpRequest42".to_string()]
        }));
}

#[test]
fn test_profile_search_index_persists_across_reopen() {
    let _guard = test_lock_blocking();
    let tmp = TempDir::new().unwrap();
    let keys = Keys::generate();
    let pubkey = keys.public_key().to_hex();

    {
        let graph_store = open_test_social_graph_store(tmp.path()).unwrap();
        let profile = event_builder!(
            Kind::Metadata,
            serde_json::json!({
                "display_name": "reopen user",
            })
            .to_string(),
            [],
        )
        .custom_created_at(Timestamp::from_secs(5))
        .sign_with_keys(&keys)
        .unwrap();
        ingest_parsed_event(&graph_store, &profile).unwrap();
        assert!(graph_store.profile_search_root().unwrap().is_some());
    }

    let reopened = open_test_social_graph_store(tmp.path()).unwrap();
    assert!(reopened.profile_search_root().unwrap().is_some());
    assert_eq!(
        reopened
            .latest_profile_event(&pubkey)
            .unwrap()
            .expect("mirrored profile after reopen")
            .pubkey,
        keys.public_key()
    );
    let links = reopened
        .profile_search_entries_for_prefix("p:reopen")
        .unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].0, format!("p:reopen:{pubkey}"));
    assert_eq!(links[0].1.name, "reopen user");
}

#[test]
fn test_profile_search_index_with_shared_hashtree_storage() {
    let _guard = test_lock_blocking();
    let tmp = TempDir::new().unwrap();
    let store =
        crate::storage::HashtreeStore::with_embedded_options(tmp.path(), None, 1024 * 1024 * 1024)
            .unwrap();
    let graph_store =
        open_test_social_graph_store_with_storage(tmp.path(), store.store_arc(), None).unwrap();
    let keys = Keys::generate();
    let pubkey = keys.public_key().to_hex();

    let profile = event_builder!(
        Kind::Metadata,
        serde_json::json!({
            "display_name": "shared storage user",
            "nip05": "shareduser@example.com",
        })
        .to_string(),
        [],
    )
    .custom_created_at(Timestamp::from_secs(5))
    .sign_with_keys(&keys)
    .unwrap();

    graph_store
        .sync_profile_index_for_events(std::slice::from_ref(&profile))
        .unwrap();
    assert!(graph_store.profile_search_root().unwrap().is_some());
    assert!(graph_store.profile_search_root().unwrap().is_some());
    let links = graph_store
        .profile_search_entries_for_prefix("p:shared")
        .unwrap();
    assert_eq!(links.len(), 2);
    assert!(links
        .iter()
        .any(|(key, entry)| key == &format!("p:shared:{pubkey}")
            && entry.name == "shared storage user"));
    assert!(links
        .iter()
        .any(|(key, entry)| key == &format!("p:shareduser:{pubkey}")
            && entry.nip05.as_deref() == Some("shareduser")));
}

#[test]
fn test_social_graph_force_sync_flushes_managed_storage() {
    let _guard = test_lock_blocking();
    let tmp = TempDir::new().unwrap();
    let store =
        crate::storage::HashtreeStore::with_embedded_options(tmp.path(), None, 1024 * 1024 * 1024)
            .unwrap();
    let graph_store =
        open_test_social_graph_store_with_storage(tmp.path(), store.store_arc(), None).unwrap();
    let keys = Keys::generate();
    let profile = event_builder!(
        Kind::Metadata,
        serde_json::json!({ "display_name": "durable profile" }).to_string(),
        [],
    )
    .custom_created_at(Timestamp::from_secs(5))
    .sign_with_keys(&keys)
    .unwrap();

    graph_store
        .sync_profile_index_for_events(std::slice::from_ref(&profile))
        .unwrap();
    graph_store.force_sync().unwrap();
    store.force_sync().unwrap();
}

#[test]
fn test_rebuild_profile_index_from_stored_events_uses_ambient_and_public_metadata() {
    let _guard = test_lock_blocking();
    let tmp = TempDir::new().unwrap();
    let graph_store = open_test_social_graph_store(tmp.path()).unwrap();
    let public_keys = Keys::generate();
    let ambient_keys = Keys::generate();
    let public_pubkey = public_keys.public_key().to_hex();
    let ambient_pubkey = ambient_keys.public_key().to_hex();

    let older = event_builder!(
        Kind::Metadata,
        serde_json::json!({
            "display_name": "petri old",
        })
        .to_string(),
        [],
    )
    .custom_created_at(Timestamp::from_secs(5))
    .sign_with_keys(&public_keys)
    .unwrap();
    let newer = event_builder!(
        Kind::Metadata,
        serde_json::json!({
            "display_name": "petri",
            "name": "Petri Example",
            "nip05": "petri@example.com",
        })
        .to_string(),
        [],
    )
    .custom_created_at(Timestamp::from_secs(6))
    .sign_with_keys(&public_keys)
    .unwrap();
    let ambient = event_builder!(
        Kind::Metadata,
        serde_json::json!({
            "display_name": "ambient petri",
        })
        .to_string(),
        [],
    )
    .custom_created_at(Timestamp::from_secs(7))
    .sign_with_keys(&ambient_keys)
    .unwrap();

    ingest_parsed_event_with_storage_class(&graph_store, &older, EventStorageClass::Public)
        .unwrap();
    ingest_parsed_event_with_storage_class(&graph_store, &newer, EventStorageClass::Public)
        .unwrap();
    ingest_parsed_event_with_storage_class(&graph_store, &ambient, EventStorageClass::Ambient)
        .unwrap();

    graph_store
        .profile_index
        .write_by_pubkey_root(None)
        .unwrap();
    graph_store.profile_index.write_search_root(None).unwrap();

    let rebuilt = graph_store
        .rebuild_profile_index_from_stored_events()
        .unwrap();
    assert_eq!(rebuilt, 2);

    let entries = graph_store
        .profile_search_entries_for_prefix("p:petri")
        .unwrap();
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().any(|(key, entry)| {
        key == &format!("p:petri:{public_pubkey}")
            && entry.name == "petri"
            && entry.aliases == vec!["Petri Example".to_string()]
            && entry.nip05.is_none()
    }));
    assert!(entries.iter().any(|(key, entry)| {
        key == &format!("p:petri:{ambient_pubkey}")
            && entry.name == "ambient petri"
            && entry.aliases.is_empty()
            && entry.nip05.is_none()
    }));
}

#[test]
fn test_rebuild_profile_index_excludes_overmuted_users() {
    let _guard = test_lock_blocking();
    let tmp = TempDir::new().unwrap();
    let graph_store = open_test_social_graph_store(tmp.path()).unwrap();
    let root_keys = Keys::generate();
    let muted_keys = Keys::generate();
    let muted_pubkey = muted_keys.public_key().to_hex();

    set_social_graph_root(&graph_store, &root_keys.public_key().to_bytes());
    graph_store.set_profile_index_overmute_threshold(1.0);

    let profile = event_builder!(
        Kind::Metadata,
        serde_json::json!({
            "display_name": "muted petri",
        })
        .to_string(),
        [],
    )
    .custom_created_at(Timestamp::from_secs(5))
    .sign_with_keys(&muted_keys)
    .unwrap();
    ingest_parsed_event(&graph_store, &profile).unwrap();
    assert!(graph_store
        .latest_profile_event(&muted_pubkey)
        .unwrap()
        .is_some());

    let mute = event_builder!(
        Kind::MuteList,
        "",
        vec![Tag::public_key(muted_keys.public_key())],
    )
    .custom_created_at(Timestamp::from_secs(6))
    .sign_with_keys(&root_keys)
    .unwrap();
    ingest_parsed_event(&graph_store, &mute).unwrap();
    assert!(graph_store
        .is_overmuted_user(&muted_keys.public_key().to_bytes(), 1.0)
        .unwrap());

    let rebuilt = graph_store
        .rebuild_profile_index_from_stored_events()
        .unwrap();
    assert_eq!(rebuilt, 0);
    assert!(graph_store
        .latest_profile_event(&muted_pubkey)
        .unwrap()
        .is_none());
    assert!(graph_store
        .profile_search_entries_for_prefix("p:muted")
        .unwrap()
        .is_empty());
}

#[test]
fn test_query_events_by_author() {
    let _guard = test_lock_blocking();
    let tmp = TempDir::new().unwrap();
    let graph_store = open_test_social_graph_store(tmp.path()).unwrap();
    let keys = Keys::generate();

    let older = event_builder!(Kind::TextNote, "older")
        .custom_created_at(Timestamp::from_secs(5))
        .sign_with_keys(&keys)
        .unwrap();
    let newer = event_builder!(Kind::TextNote, "newer")
        .custom_created_at(Timestamp::from_secs(6))
        .sign_with_keys(&keys)
        .unwrap();

    ingest_parsed_event(&graph_store, &older).unwrap();
    ingest_parsed_event(&graph_store, &newer).unwrap();

    let filter = Filter::new().author(keys.public_key()).kind(Kind::TextNote);
    let events = query_events(&graph_store, &filter, 10);
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].id, newer.id);
    assert_eq!(events[1].id, older.id);
}

#[test]
fn test_query_events_by_multiple_authors_and_kinds() {
    let _guard = test_lock_blocking();
    let tmp = TempDir::new().unwrap();
    let graph_store = open_test_social_graph_store(tmp.path()).unwrap();
    let first_keys = Keys::generate();
    let second_keys = Keys::generate();
    let other_keys = Keys::generate();

    let first_note = event_builder!(Kind::TextNote, "first note")
        .custom_created_at(Timestamp::from_secs(5))
        .sign_with_keys(&first_keys)
        .unwrap();
    let first_profile = event_builder!(Kind::Metadata, "first profile")
        .custom_created_at(Timestamp::from_secs(6))
        .sign_with_keys(&first_keys)
        .unwrap();
    let second_note = event_builder!(Kind::TextNote, "second note")
        .custom_created_at(Timestamp::from_secs(7))
        .sign_with_keys(&second_keys)
        .unwrap();
    let other_note = event_builder!(Kind::TextNote, "other note")
        .custom_created_at(Timestamp::from_secs(8))
        .sign_with_keys(&other_keys)
        .unwrap();

    ingest_parsed_event(&graph_store, &first_note).unwrap();
    ingest_parsed_event(&graph_store, &first_profile).unwrap();
    ingest_parsed_event(&graph_store, &second_note).unwrap();
    ingest_parsed_event(&graph_store, &other_note).unwrap();

    let filter = Filter::new()
        .authors(vec![first_keys.public_key(), second_keys.public_key()])
        .kinds(vec![Kind::TextNote, Kind::Metadata])
        .limit(10);
    let events = query_events(&graph_store, &filter, 10);
    assert_eq!(
        events.iter().map(|event| event.id).collect::<Vec<_>>(),
        vec![second_note.id, first_profile.id, first_note.id]
    );
}

#[test]
fn test_query_events_by_kind() {
    let _guard = test_lock_blocking();
    let tmp = TempDir::new().unwrap();
    let graph_store = open_test_social_graph_store(tmp.path()).unwrap();
    let first_keys = Keys::generate();
    let second_keys = Keys::generate();

    let older = event_builder!(Kind::TextNote, "older")
        .custom_created_at(Timestamp::from_secs(5))
        .sign_with_keys(&first_keys)
        .unwrap();
    let newer = event_builder!(Kind::TextNote, "newer")
        .custom_created_at(Timestamp::from_secs(6))
        .sign_with_keys(&second_keys)
        .unwrap();
    let other_kind = event_builder!(Kind::Metadata, "profile")
        .custom_created_at(Timestamp::from_secs(7))
        .sign_with_keys(&second_keys)
        .unwrap();

    ingest_parsed_event(&graph_store, &older).unwrap();
    ingest_parsed_event(&graph_store, &newer).unwrap();
    ingest_parsed_event(&graph_store, &other_kind).unwrap();

    let filter = Filter::new().kind(Kind::TextNote);
    let events = query_events(&graph_store, &filter, 10);
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].id, newer.id);
    assert_eq!(events[1].id, older.id);
}

#[test]
fn test_query_events_by_id() {
    let _guard = test_lock_blocking();
    let tmp = TempDir::new().unwrap();
    let graph_store = open_test_social_graph_store(tmp.path()).unwrap();
    let keys = Keys::generate();

    let first = event_builder!(Kind::TextNote, "first")
        .custom_created_at(Timestamp::from_secs(5))
        .sign_with_keys(&keys)
        .unwrap();
    let target = event_builder!(Kind::TextNote, "target")
        .custom_created_at(Timestamp::from_secs(6))
        .sign_with_keys(&keys)
        .unwrap();

    ingest_parsed_event(&graph_store, &first).unwrap();
    ingest_parsed_event(&graph_store, &target).unwrap();

    let filter = Filter::new().id(target.id).kind(Kind::TextNote);
    let events = query_events(&graph_store, &filter, 10);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].id, target.id);
}

#[test]
fn test_query_events_search_is_case_insensitive() {
    let _guard = test_lock_blocking();
    let tmp = TempDir::new().unwrap();
    let graph_store = open_test_social_graph_store(tmp.path()).unwrap();
    let keys = Keys::generate();
    let other_keys = Keys::generate();

    let matching = event_builder!(Kind::TextNote, "Hello Nostr Search")
        .custom_created_at(Timestamp::from_secs(5))
        .sign_with_keys(&keys)
        .unwrap();
    let other = event_builder!(Kind::TextNote, "goodbye world")
        .custom_created_at(Timestamp::from_secs(6))
        .sign_with_keys(&other_keys)
        .unwrap();

    ingest_parsed_event(&graph_store, &matching).unwrap();
    ingest_parsed_event(&graph_store, &other).unwrap();

    let filter = Filter::new().kind(Kind::TextNote).search("nostr search");
    let events = query_events(&graph_store, &filter, 10);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].id, matching.id);
}

#[test]
fn test_query_events_since_until_are_inclusive() {
    let _guard = test_lock_blocking();
    let tmp = TempDir::new().unwrap();
    let graph_store = open_test_social_graph_store(tmp.path()).unwrap();
    let keys = Keys::generate();

    let before = event_builder!(Kind::TextNote, "before")
        .custom_created_at(Timestamp::from_secs(5))
        .sign_with_keys(&keys)
        .unwrap();
    let start = event_builder!(Kind::TextNote, "start")
        .custom_created_at(Timestamp::from_secs(6))
        .sign_with_keys(&keys)
        .unwrap();
    let end = event_builder!(Kind::TextNote, "end")
        .custom_created_at(Timestamp::from_secs(10))
        .sign_with_keys(&keys)
        .unwrap();
    let after = event_builder!(Kind::TextNote, "after")
        .custom_created_at(Timestamp::from_secs(11))
        .sign_with_keys(&keys)
        .unwrap();

    ingest_parsed_event(&graph_store, &before).unwrap();
    ingest_parsed_event(&graph_store, &start).unwrap();
    ingest_parsed_event(&graph_store, &end).unwrap();
    ingest_parsed_event(&graph_store, &after).unwrap();

    let filter = Filter::new()
        .kind(Kind::TextNote)
        .since(Timestamp::from_secs(6))
        .until(Timestamp::from_secs(10));
    let events = query_events(&graph_store, &filter, 10);
    let ids = events.into_iter().map(|event| event.id).collect::<Vec<_>>();
    assert_eq!(ids, vec![end.id, start.id]);
}

#[test]
fn test_query_events_replaceable_kind_returns_latest_winner() {
    let _guard = test_lock_blocking();
    let tmp = TempDir::new().unwrap();
    let graph_store = open_test_social_graph_store(tmp.path()).unwrap();
    let keys = Keys::generate();

    let older = event_builder!(Kind::Custom(10_000), "older mute list")
        .custom_created_at(Timestamp::from_secs(5))
        .sign_with_keys(&keys)
        .unwrap();
    let newer = event_builder!(Kind::Custom(10_000), "newer mute list")
        .custom_created_at(Timestamp::from_secs(6))
        .sign_with_keys(&keys)
        .unwrap();

    ingest_parsed_event(&graph_store, &older).unwrap();
    ingest_parsed_event(&graph_store, &newer).unwrap();

    let filter = Filter::new()
        .author(keys.public_key())
        .kind(Kind::Custom(10_000));
    let events = query_events(&graph_store, &filter, 10);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].id, newer.id);
}

#[test]
fn test_query_events_kind_41_replaceable_returns_latest_winner() {
    let _guard = test_lock_blocking();
    let tmp = TempDir::new().unwrap();
    let graph_store = open_test_social_graph_store(tmp.path()).unwrap();
    let keys = Keys::generate();

    let older = event_builder!(Kind::Custom(41), "older channel metadata")
        .custom_created_at(Timestamp::from_secs(5))
        .sign_with_keys(&keys)
        .unwrap();
    let newer = event_builder!(Kind::Custom(41), "newer channel metadata")
        .custom_created_at(Timestamp::from_secs(6))
        .sign_with_keys(&keys)
        .unwrap();

    ingest_parsed_event(&graph_store, &older).unwrap();
    ingest_parsed_event(&graph_store, &newer).unwrap();

    let filter = Filter::new()
        .author(keys.public_key())
        .kind(Kind::Custom(41));
    let events = query_events(&graph_store, &filter, 10);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].id, newer.id);
}

#[test]
fn test_public_and_ambient_indexes_stay_separate() {
    let _guard = test_lock_blocking();
    let tmp = TempDir::new().unwrap();
    let graph_store = open_test_social_graph_store(tmp.path()).unwrap();
    let public_keys = Keys::generate();
    let ambient_keys = Keys::generate();

    let public_event = event_builder!(Kind::TextNote, "public")
        .custom_created_at(Timestamp::from_secs(5))
        .sign_with_keys(&public_keys)
        .unwrap();
    let ambient_event = event_builder!(Kind::TextNote, "ambient")
        .custom_created_at(Timestamp::from_secs(6))
        .sign_with_keys(&ambient_keys)
        .unwrap();

    ingest_parsed_event_with_storage_class(&graph_store, &public_event, EventStorageClass::Public)
        .unwrap();
    ingest_parsed_event_with_storage_class(
        &graph_store,
        &ambient_event,
        EventStorageClass::Ambient,
    )
    .unwrap();

    let filter = Filter::new().kind(Kind::TextNote);
    let all_events = graph_store
        .query_events_in_scope(&filter, 10, EventQueryScope::All)
        .unwrap();
    assert_eq!(all_events.len(), 2);

    let public_events = graph_store
        .query_events_in_scope(&filter, 10, EventQueryScope::PublicOnly)
        .unwrap();
    assert_eq!(public_events.len(), 1);
    assert_eq!(public_events[0].id, public_event.id);

    let ambient_events = graph_store
        .query_events_in_scope(&filter, 10, EventQueryScope::AmbientOnly)
        .unwrap();
    assert_eq!(ambient_events.len(), 1);
    assert_eq!(ambient_events[0].id, ambient_event.id);
}

#[test]
fn test_default_ingest_classifies_root_author_as_public() {
    let _guard = test_lock_blocking();
    let tmp = TempDir::new().unwrap();
    let graph_store = open_test_social_graph_store(tmp.path()).unwrap();
    let root_keys = Keys::generate();
    let other_keys = Keys::generate();
    set_social_graph_root(&graph_store, &root_keys.public_key().to_bytes());

    let root_event = event_builder!(Kind::TextNote, "root")
        .custom_created_at(Timestamp::from_secs(5))
        .sign_with_keys(&root_keys)
        .unwrap();
    let other_event = event_builder!(Kind::TextNote, "other")
        .custom_created_at(Timestamp::from_secs(6))
        .sign_with_keys(&other_keys)
        .unwrap();

    ingest_parsed_event(&graph_store, &root_event).unwrap();
    ingest_parsed_event(&graph_store, &other_event).unwrap();

    let filter = Filter::new().kind(Kind::TextNote);
    let public_events = graph_store
        .query_events_in_scope(&filter, 10, EventQueryScope::PublicOnly)
        .unwrap();
    assert_eq!(public_events.len(), 1);
    assert_eq!(public_events[0].id, root_event.id);

    let ambient_events = graph_store
        .query_events_in_scope(&filter, 10, EventQueryScope::AmbientOnly)
        .unwrap();
    assert_eq!(ambient_events.len(), 1);
    assert_eq!(ambient_events[0].id, other_event.id);
}

#[test]
fn test_query_events_survives_reopen() {
    let _guard = test_lock_blocking();
    let tmp = TempDir::new().unwrap();
    let db_dir = tmp.path().join("socialgraph-store");
    let keys = Keys::generate();
    let other_keys = Keys::generate();

    {
        let graph_store = open_test_social_graph_store_at_path(&db_dir, None).unwrap();
        let older = event_builder!(Kind::TextNote, "older")
            .custom_created_at(Timestamp::from_secs(5))
            .sign_with_keys(&keys)
            .unwrap();
        let newer = event_builder!(Kind::TextNote, "newer")
            .custom_created_at(Timestamp::from_secs(6))
            .sign_with_keys(&keys)
            .unwrap();
        let latest = event_builder!(Kind::TextNote, "latest")
            .custom_created_at(Timestamp::from_secs(7))
            .sign_with_keys(&other_keys)
            .unwrap();

        ingest_parsed_event(&graph_store, &older).unwrap();
        ingest_parsed_event(&graph_store, &newer).unwrap();
        ingest_parsed_event(&graph_store, &latest).unwrap();
    }

    let reopened = open_test_social_graph_store_at_path(&db_dir, None).unwrap();

    let author_filter = Filter::new().author(keys.public_key()).kind(Kind::TextNote);
    let author_events = query_events(&reopened, &author_filter, 10);
    assert_eq!(author_events.len(), 2);
    assert_eq!(author_events[0].content, "newer");
    assert_eq!(author_events[1].content, "older");

    let recent_filter = Filter::new().kind(Kind::TextNote);
    let recent_events = query_events(&reopened, &recent_filter, 2);
    assert_eq!(recent_events.len(), 2);
    assert_eq!(recent_events[0].content, "latest");
    assert_eq!(recent_events[1].content, "newer");
}

#[test]
fn test_query_events_parameterized_replaceable_by_d_tag() {
    let _guard = test_lock_blocking();
    let tmp = TempDir::new().unwrap();
    let graph_store = open_test_social_graph_store(tmp.path()).unwrap();
    let keys = Keys::generate();

    let older = event_builder!(
        Kind::Custom(30078),
        "",
        vec![
            Tag::identifier("video"),
            Tag::parse(["l", "hashtree"]).unwrap(),
            Tag::parse(vec!["hash".to_string(), "11".repeat(32)]).unwrap(),
        ],
    )
    .custom_created_at(Timestamp::from_secs(5))
    .sign_with_keys(&keys)
    .unwrap();
    let newer = event_builder!(
        Kind::Custom(30078),
        "",
        vec![
            Tag::identifier("video"),
            Tag::parse(["l", "hashtree"]).unwrap(),
            Tag::parse(vec!["hash".to_string(), "22".repeat(32)]).unwrap(),
        ],
    )
    .custom_created_at(Timestamp::from_secs(6))
    .sign_with_keys(&keys)
    .unwrap();
    let other_tree = event_builder!(
        Kind::Custom(30078),
        "",
        vec![
            Tag::identifier("files"),
            Tag::parse(["l", "hashtree"]).unwrap(),
            Tag::parse(vec!["hash".to_string(), "33".repeat(32)]).unwrap(),
        ],
    )
    .custom_created_at(Timestamp::from_secs(7))
    .sign_with_keys(&keys)
    .unwrap();

    ingest_parsed_event(&graph_store, &older).unwrap();
    ingest_parsed_event(&graph_store, &newer).unwrap();
    ingest_parsed_event(&graph_store, &other_tree).unwrap();

    let filter = Filter::new()
        .author(keys.public_key())
        .kind(Kind::Custom(30078))
        .identifier("video");
    let events = query_events(&graph_store, &filter, 10);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].id, newer.id);
}

#[test]
fn test_query_events_by_hashtag_uses_tag_index() {
    let _guard = test_lock_blocking();
    let tmp = TempDir::new().unwrap();
    let graph_store = open_test_social_graph_store(tmp.path()).unwrap();
    let keys = Keys::generate();
    let other_keys = Keys::generate();

    let first = event_builder!(
        Kind::TextNote,
        "first",
        vec![Tag::parse(["t", "hashtree"]).unwrap()],
    )
    .custom_created_at(Timestamp::from_secs(5))
    .sign_with_keys(&keys)
    .unwrap();
    let second = event_builder!(
        Kind::TextNote,
        "second",
        vec![Tag::parse(["t", "hashtree"]).unwrap()],
    )
    .custom_created_at(Timestamp::from_secs(6))
    .sign_with_keys(&other_keys)
    .unwrap();
    let unrelated = event_builder!(
        Kind::TextNote,
        "third",
        vec![Tag::parse(["t", "other"]).unwrap()],
    )
    .custom_created_at(Timestamp::from_secs(7))
    .sign_with_keys(&other_keys)
    .unwrap();

    ingest_parsed_event(&graph_store, &first).unwrap();
    ingest_parsed_event(&graph_store, &second).unwrap();
    ingest_parsed_event(&graph_store, &unrelated).unwrap();

    let filter = Filter::new().hashtag("hashtree");
    let events = query_events(&graph_store, &filter, 10);
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].id, second.id);
    assert_eq!(events[1].id, first.id);
}

#[test]
fn test_query_events_combines_indexes_then_applies_search_filter() {
    let _guard = test_lock_blocking();
    let tmp = TempDir::new().unwrap();
    let graph_store = open_test_social_graph_store(tmp.path()).unwrap();
    let keys = Keys::generate();
    let other_keys = Keys::generate();

    let matching = event_builder!(
        Kind::TextNote,
        "hashtree video release",
        vec![Tag::parse(["t", "hashtree"]).unwrap()],
    )
    .custom_created_at(Timestamp::from_secs(5))
    .sign_with_keys(&keys)
    .unwrap();
    let non_matching = event_builder!(
        Kind::TextNote,
        "plain text note",
        vec![Tag::parse(["t", "hashtree"]).unwrap()],
    )
    .custom_created_at(Timestamp::from_secs(6))
    .sign_with_keys(&other_keys)
    .unwrap();

    ingest_parsed_event(&graph_store, &matching).unwrap();
    ingest_parsed_event(&graph_store, &non_matching).unwrap();

    let filter = Filter::new().hashtag("hashtree").search("video");
    let events = query_events(&graph_store, &filter, 10);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].id, matching.id);
}

fn benchmark_dataset_path() -> Option<PathBuf> {
    std::env::var_os("HASHTREE_BENCH_DATASET_PATH").map(PathBuf::from)
}

fn benchmark_dataset_url() -> String {
    std::env::var("HASHTREE_BENCH_DATASET_URL")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| WELLORDER_FIXTURE_URL.to_string())
}

fn benchmark_stream_warmup_events(measured_events: usize) -> usize {
    std::env::var("HASHTREE_BENCH_WARMUP_EVENTS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or_else(|| measured_events.clamp(1, 200))
}

fn ensure_benchmark_dataset(path: &Path, url: &str) -> Result<()> {
    if path.exists() {
        return Ok(());
    }

    let parent = path
        .parent()
        .context("benchmark dataset path has no parent directory")?;
    fs::create_dir_all(parent).context("create benchmark dataset directory")?;

    let tmp = path.with_extension("tmp");
    let mut response = reqwest::blocking::get(url)
        .context("download benchmark dataset")?
        .error_for_status()
        .context("benchmark dataset request failed")?;
    let mut file = File::create(&tmp).context("create temporary benchmark dataset file")?;
    std::io::copy(&mut response, &mut file).context("write benchmark dataset")?;
    fs::rename(&tmp, path).context("move benchmark dataset into place")?;

    Ok(())
}

fn load_benchmark_dataset(path: &Path, max_events: usize) -> Result<Vec<Event>> {
    if max_events == 0 {
        return Ok(Vec::new());
    }

    let mut child = Command::new("bzip2")
        .args(["-dc", &path.to_string_lossy()])
        .stdout(Stdio::piped())
        .spawn()
        .context("spawn bzip2 for benchmark dataset")?;
    let stdout = child
        .stdout
        .take()
        .context("benchmark dataset stdout missing")?;
    let mut events = Vec::with_capacity(max_events);

    {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            if events.len() >= max_events {
                break;
            }
            let line = line.context("read benchmark dataset line")?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(event) = Event::from_json(trimmed) {
                events.push(event);
            }
        }
    }

    if events.len() < max_events {
        let status = child.wait().context("wait for benchmark dataset reader")?;
        anyhow::ensure!(
            status.success(),
            "benchmark dataset reader exited with status {status}"
        );
    } else {
        let _ = child.kill();
        let _ = child.wait();
    }

    Ok(events)
}

fn build_synthetic_benchmark_events(event_count: usize, author_count: usize) -> Vec<Event> {
    let authors = (0..author_count)
        .map(|_| Keys::generate())
        .collect::<Vec<_>>();
    let mut events = Vec::with_capacity(event_count);
    for i in 0..event_count {
        let kind = if i % 8 < 5 {
            Kind::TextNote
        } else {
            Kind::Custom(30_023)
        };
        let mut tags = Vec::new();
        if kind == Kind::TextNote && i % 16 == 0 {
            tags.push(Tag::parse(["t", "hashtree"]).unwrap());
        }
        let content = if kind == Kind::TextNote && i % 32 == 0 {
            format!("benchmark target event {i}")
        } else {
            format!("benchmark event {i}")
        };
        let event = event_builder!(kind, content, tags)
            .custom_created_at(Timestamp::from_secs(1_700_000_000 + i as u64))
            .sign_with_keys(&authors[i % author_count])
            .unwrap();
        events.push(event);
    }
    events
}

fn load_benchmark_events(event_count: usize, author_count: usize) -> Result<(String, Vec<Event>)> {
    if let Some(path) = benchmark_dataset_path() {
        let url = benchmark_dataset_url();
        ensure_benchmark_dataset(&path, &url)?;
        let events = load_benchmark_dataset(&path, event_count)?;
        return Ok((format!("dataset:{}", path.display()), events));
    }

    Ok((
        format!("synthetic:{author_count}-authors"),
        build_synthetic_benchmark_events(event_count, author_count),
    ))
}

fn first_tag_filter(event: &Event) -> Option<Filter> {
    event.tags.iter().find_map(|tag| match tag.as_slice() {
        [name, value, ..]
            if name.len() == 1 && !value.is_empty() && name.as_bytes()[0].is_ascii_lowercase() =>
        {
            let letter = SingleLetterTag::from_char(name.chars().next()?).ok()?;
            Some(Filter::new().custom_tag(letter, value.to_string()))
        }
        _ => None,
    })
}

fn first_search_term(event: &Event) -> Option<String> {
    event
        .content
        .split(|ch: char| !ch.is_alphanumeric())
        .find(|token| token.len() >= 4)
        .map(|token| token.to_ascii_lowercase())
}

fn benchmark_match_count(events: &[Event], filter: &Filter, limit: usize) -> usize {
    events
        .iter()
        .filter(|event| filter.match_event(event, Default::default()))
        .count()
        .min(limit)
}

fn benchmark_btree_orders() -> Vec<usize> {
    std::env::var("HASHTREE_BTREE_ORDERS")
        .ok()
        .map(|value| {
            value
                .split(',')
                .filter_map(|part| part.trim().parse::<usize>().ok())
                .filter(|order| *order >= 2)
                .collect::<Vec<_>>()
        })
        .filter(|orders| !orders.is_empty())
        .unwrap_or_else(|| vec![16, 24, 32, 48, 64])
}

fn benchmark_read_iterations() -> usize {
    std::env::var("HASHTREE_BENCH_READ_ITERATIONS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(5)
        .max(1)
}

fn average_duration(samples: &[Duration]) -> Duration {
    if samples.is_empty() {
        return Duration::ZERO;
    }

    Duration::from_secs_f64(
        samples.iter().map(Duration::as_secs_f64).sum::<f64>() / samples.len() as f64,
    )
}

fn average_read_trace(samples: &[ReadTraceSnapshot]) -> ReadTraceSnapshot {
    if samples.is_empty() {
        return ReadTraceSnapshot::default();
    }

    let len = samples.len() as u64;
    ReadTraceSnapshot {
        get_calls: samples.iter().map(|sample| sample.get_calls).sum::<u64>() / len,
        total_bytes: samples.iter().map(|sample| sample.total_bytes).sum::<u64>() / len,
        unique_blocks: (samples
            .iter()
            .map(|sample| sample.unique_blocks as u64)
            .sum::<u64>()
            / len) as usize,
        unique_bytes: samples
            .iter()
            .map(|sample| sample.unique_bytes)
            .sum::<u64>()
            / len,
        cache_hits: samples.iter().map(|sample| sample.cache_hits).sum::<u64>() / len,
        remote_fetches: samples
            .iter()
            .map(|sample| sample.remote_fetches)
            .sum::<u64>()
            / len,
        remote_bytes: samples
            .iter()
            .map(|sample| sample.remote_bytes)
            .sum::<u64>()
            / len,
    }
}

fn estimate_serialized_remote_ms(snapshot: &ReadTraceSnapshot, model: NetworkModel) -> f64 {
    let transfer_ms = if model.bandwidth_mib_per_s <= 0.0 {
        0.0
    } else {
        (snapshot.remote_bytes as f64 / (model.bandwidth_mib_per_s * 1024.0 * 1024.0)) * 1000.0
    };
    snapshot.remote_fetches as f64 * model.rtt_ms + transfer_ms
}

#[derive(Debug, Clone)]
struct IndexBenchmarkDataset {
    source: String,
    events: Vec<Event>,
    guaranteed_tag_name: String,
    guaranteed_tag_value: String,
    replaceable_pubkey: String,
    replaceable_kind: u32,
    parameterized_pubkey: String,
    parameterized_kind: u32,
    parameterized_d_tag: String,
}

fn load_index_benchmark_dataset(
    event_count: usize,
    author_count: usize,
) -> Result<IndexBenchmarkDataset> {
    let (source, mut events) = load_benchmark_events(event_count, author_count)?;
    let base_timestamp = events
        .iter()
        .map(|event| event.created_at.as_secs())
        .max()
        .unwrap_or(1_700_000_000)
        + 1;

    let replaceable_keys = Keys::generate();
    let parameterized_keys = Keys::generate();
    let tagged_keys = Keys::generate();
    let guaranteed_tag_name = "t".to_string();
    let guaranteed_tag_value = "btreebench".to_string();
    let replaceable_kind = 10_000u32;
    let parameterized_kind = 30_023u32;
    let parameterized_d_tag = "btree-bench".to_string();

    let tagged = event_builder!(
        Kind::TextNote,
        "btree benchmark tagged note",
        vec![Tag::parse(vec!["t".to_string(), guaranteed_tag_value.clone(),]).unwrap()],
    )
    .custom_created_at(Timestamp::from_secs(base_timestamp))
    .sign_with_keys(&tagged_keys)
    .unwrap();
    let replaceable_old = event_builder!(
        Kind::Custom(replaceable_kind.try_into().unwrap()),
        "replaceable old",
        [],
    )
    .custom_created_at(Timestamp::from_secs(base_timestamp + 1))
    .sign_with_keys(&replaceable_keys)
    .unwrap();
    let replaceable_new = event_builder!(
        Kind::Custom(replaceable_kind.try_into().unwrap()),
        "replaceable new",
        [],
    )
    .custom_created_at(Timestamp::from_secs(base_timestamp + 2))
    .sign_with_keys(&replaceable_keys)
    .unwrap();
    let parameterized_old = event_builder!(
        Kind::Custom(parameterized_kind.try_into().unwrap()),
        "",
        vec![Tag::identifier(&parameterized_d_tag)],
    )
    .custom_created_at(Timestamp::from_secs(base_timestamp + 3))
    .sign_with_keys(&parameterized_keys)
    .unwrap();
    let parameterized_new = event_builder!(
        Kind::Custom(parameterized_kind.try_into().unwrap()),
        "",
        vec![Tag::identifier(&parameterized_d_tag)],
    )
    .custom_created_at(Timestamp::from_secs(base_timestamp + 4))
    .sign_with_keys(&parameterized_keys)
    .unwrap();

    events.extend([
        tagged,
        replaceable_old,
        replaceable_new,
        parameterized_old,
        parameterized_new,
    ]);

    Ok(IndexBenchmarkDataset {
        source,
        events,
        guaranteed_tag_name,
        guaranteed_tag_value,
        replaceable_pubkey: replaceable_keys.public_key().to_hex(),
        replaceable_kind,
        parameterized_pubkey: parameterized_keys.public_key().to_hex(),
        parameterized_kind,
        parameterized_d_tag,
    })
}

fn build_btree_query_cases(dataset: &IndexBenchmarkDataset) -> Vec<BenchmarkQueryCase> {
    let primary_kind = dataset
        .events
        .iter()
        .find(|event| event.kind == Kind::TextNote)
        .map(|event| event.kind)
        .or_else(|| dataset.events.first().map(|event| event.kind))
        .expect("benchmark requires at least one event");
    let primary_kind_u32 = primary_kind.as_u16() as u32;

    let author_pubkey = dataset
        .events
        .iter()
        .filter(|event| event.kind == primary_kind)
        .fold(HashMap::<String, usize>::new(), |mut counts, event| {
            *counts.entry(event.pubkey.to_hex()).or_default() += 1;
            counts
        })
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(pubkey, _)| pubkey)
        .expect("benchmark requires an author for the selected kind");

    let by_id_id = dataset.events[dataset.events.len() / 2].id.to_hex();

    vec![
        BenchmarkQueryCase::ById { id: by_id_id },
        BenchmarkQueryCase::ByAuthor {
            pubkey: author_pubkey.clone(),
            limit: 50,
        },
        BenchmarkQueryCase::ByAuthorKind {
            pubkey: author_pubkey,
            kind: primary_kind_u32,
            limit: 50,
        },
        BenchmarkQueryCase::ByKind {
            kind: primary_kind_u32,
            limit: 200,
        },
        BenchmarkQueryCase::ByTag {
            tag_name: dataset.guaranteed_tag_name.clone(),
            tag_value: dataset.guaranteed_tag_value.clone(),
            limit: 100,
        },
        BenchmarkQueryCase::Recent { limit: 100 },
        BenchmarkQueryCase::Replaceable {
            pubkey: dataset.replaceable_pubkey.clone(),
            kind: dataset.replaceable_kind,
        },
        BenchmarkQueryCase::ParameterizedReplaceable {
            pubkey: dataset.parameterized_pubkey.clone(),
            kind: dataset.parameterized_kind,
            d_tag: dataset.parameterized_d_tag.clone(),
        },
    ]
}

fn benchmark_warm_query_case<S: Store + 'static>(
    base: Arc<S>,
    root: &Cid,
    order: usize,
    case: &BenchmarkQueryCase,
    iterations: usize,
) -> QueryBenchmarkResult {
    let trace_store = Arc::new(CountingStore::new(base));
    let event_store = NostrEventStore::with_options(
        Arc::clone(&trace_store),
        NostrEventStoreOptions {
            btree_order: Some(order),
            ..NostrEventStoreOptions::default()
        },
    );
    let mut durations = Vec::with_capacity(iterations);
    let mut traces = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        trace_store.reset();
        let started = Instant::now();
        let matches = block_on(case.execute(&event_store, root)).unwrap();
        durations.push(started.elapsed());
        traces.push(trace_store.snapshot());
        assert!(
            matches > 0,
            "benchmark query {} returned no matches",
            case.name()
        );
    }
    let mut sorted = durations.clone();
    sorted.sort_unstable();
    QueryBenchmarkResult {
        average_duration: average_duration(&durations),
        p95_duration: duration_percentile(&sorted, 95, 100),
        reads: average_read_trace(&traces),
    }
}

fn benchmark_cold_query_case<S: Store + 'static>(
    remote: Arc<S>,
    root: &Cid,
    order: usize,
    case: &BenchmarkQueryCase,
    iterations: usize,
) -> QueryBenchmarkResult {
    let mut durations = Vec::with_capacity(iterations);
    let mut traces = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let cache = Arc::new(MemoryStore::new());
        let trace_store = Arc::new(ReadThroughStore::new(cache, Arc::clone(&remote)));
        let event_store = NostrEventStore::with_options(
            Arc::clone(&trace_store),
            NostrEventStoreOptions {
                btree_order: Some(order),
                ..NostrEventStoreOptions::default()
            },
        );
        let started = Instant::now();
        let matches = block_on(case.execute(&event_store, root)).unwrap();
        durations.push(started.elapsed());
        traces.push(trace_store.snapshot());
        assert!(
            matches > 0,
            "benchmark query {} returned no matches",
            case.name()
        );
    }
    let mut sorted = durations.clone();
    sorted.sort_unstable();
    QueryBenchmarkResult {
        average_duration: average_duration(&durations),
        p95_duration: duration_percentile(&sorted, 95, 100),
        reads: average_read_trace(&traces),
    }
}

fn duration_percentile(
    sorted: &[std::time::Duration],
    numerator: usize,
    denominator: usize,
) -> std::time::Duration {
    if sorted.is_empty() {
        return std::time::Duration::ZERO;
    }
    let index = ((sorted.len() - 1) * numerator) / denominator;
    sorted[index]
}

#[test]
#[ignore = "benchmark"]
fn benchmark_query_events_large_dataset() {
    let _guard = test_lock_blocking();
    let tmp = TempDir::new().unwrap();
    let graph_store =
        open_test_social_graph_store_with_mapsize(tmp.path(), Some(512 * 1024 * 1024)).unwrap();
    set_nostr_profile_enabled(true);
    reset_nostr_profile();

    let author_count = 64usize;
    let measured_event_count = std::env::var("HASHTREE_BENCH_EVENTS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(600usize);
    let warmup_event_count = benchmark_stream_warmup_events(measured_event_count);
    let total_event_count = warmup_event_count + measured_event_count;
    let (source, events) = load_benchmark_events(total_event_count, author_count).unwrap();
    let loaded_event_count = events.len();
    let warmup_event_count = warmup_event_count.min(loaded_event_count.saturating_sub(1));
    let (warmup_events, measured_events) = events.split_at(warmup_event_count);

    println!(
            "starting steady-state dataset benchmark with {} warmup events and {} measured stream events from {}",
            warmup_events.len(),
            measured_events.len(),
            source
        );
    if !warmup_events.is_empty() {
        ingest_parsed_events(&graph_store, warmup_events).unwrap();
    }

    let stream_start = Instant::now();
    let mut per_event_latencies = Vec::with_capacity(measured_events.len());
    for event in measured_events {
        let event_start = Instant::now();
        ingest_parsed_event(&graph_store, event).unwrap();
        per_event_latencies.push(event_start.elapsed());
    }
    let ingest_duration = stream_start.elapsed();

    let mut sorted_latencies = per_event_latencies.clone();
    sorted_latencies.sort_unstable();
    let average_latency = if per_event_latencies.is_empty() {
        std::time::Duration::ZERO
    } else {
        std::time::Duration::from_secs_f64(
            per_event_latencies
                .iter()
                .map(std::time::Duration::as_secs_f64)
                .sum::<f64>()
                / per_event_latencies.len() as f64,
        )
    };
    let ingest_capacity_eps = if ingest_duration.is_zero() {
        f64::INFINITY
    } else {
        measured_events.len() as f64 / ingest_duration.as_secs_f64()
    };
    println!(
            "benchmark steady-state ingest complete in {:?} (avg={:?} p50={:?} p95={:?} p99={:?} capacity={:.2} events/s)",
            ingest_duration,
            average_latency,
            duration_percentile(&sorted_latencies, 50, 100),
            duration_percentile(&sorted_latencies, 95, 100),
            duration_percentile(&sorted_latencies, 99, 100),
            ingest_capacity_eps
        );
    let mut profile = take_nostr_profile();
    profile.sort_by_key(|stat| std::cmp::Reverse(stat.total));
    for stat in profile {
        let pct = if ingest_duration.is_zero() {
            0.0
        } else {
            (stat.total.as_secs_f64() / ingest_duration.as_secs_f64()) * 100.0
        };
        let average = if stat.count == 0 {
            std::time::Duration::ZERO
        } else {
            std::time::Duration::from_secs_f64(stat.total.as_secs_f64() / stat.count as f64)
        };
        println!(
            "ingest profile: label={} total={:?} pct={:.1}% count={} avg={:?} max={:?}",
            stat.label, stat.total, pct, stat.count, average, stat.max
        );
    }
    set_nostr_profile_enabled(false);

    let kind = events
        .iter()
        .find(|event| event.kind == Kind::TextNote)
        .map(|event| event.kind)
        .or_else(|| events.first().map(|event| event.kind))
        .expect("benchmark requires at least one event");
    let kind_filter = Filter::new().kind(kind);
    let kind_start = Instant::now();
    let kind_events = query_events(&graph_store, &kind_filter, 200);
    let kind_duration = kind_start.elapsed();
    assert_eq!(
        kind_events.len(),
        benchmark_match_count(&events, &kind_filter, 200)
    );
    assert!(kind_events
        .windows(2)
        .all(|window| window[0].created_at >= window[1].created_at));

    let author_pubkey = events
        .iter()
        .find(|event| event.kind == kind)
        .map(|event| event.pubkey)
        .expect("benchmark requires an author for the selected kind");
    let author_filter = Filter::new().author(author_pubkey).kind(kind);
    let author_start = Instant::now();
    let author_events = query_events(&graph_store, &author_filter, 50);
    let author_duration = author_start.elapsed();
    assert_eq!(
        author_events.len(),
        benchmark_match_count(&events, &author_filter, 50)
    );

    let tag_filter = events
        .iter()
        .find_map(first_tag_filter)
        .expect("benchmark requires at least one tagged event");
    let tag_start = Instant::now();
    let tag_events = query_events(&graph_store, &tag_filter, 100);
    let tag_duration = tag_start.elapsed();
    assert_eq!(
        tag_events.len(),
        benchmark_match_count(&events, &tag_filter, 100)
    );

    let search_source = events
        .iter()
        .find_map(|event| first_search_term(event).map(|term| (event.kind, term)))
        .expect("benchmark requires at least one searchable event");
    let search_filter = Filter::new().kind(search_source.0).search(search_source.1);
    let search_start = Instant::now();
    let search_events = query_events(&graph_store, &search_filter, 100);
    let search_duration = search_start.elapsed();
    assert_eq!(
        search_events.len(),
        benchmark_match_count(&events, &search_filter, 100)
    );

    println!(
            "steady-state benchmark: source={} warmup_events={} stream_events={} ingest={:?} avg={:?} p50={:?} p95={:?} p99={:?} capacity_eps={:.2} kind={:?} author={:?} tag={:?} search={:?}",
            source,
            warmup_events.len(),
            measured_events.len(),
            ingest_duration,
            average_latency,
            duration_percentile(&sorted_latencies, 50, 100),
            duration_percentile(&sorted_latencies, 95, 100),
            duration_percentile(&sorted_latencies, 99, 100),
            ingest_capacity_eps,
            kind_duration,
            author_duration,
            tag_duration,
            search_duration
        );
}

#[test]
#[ignore = "benchmark"]
fn benchmark_nostr_btree_query_tradeoffs() {
    let _guard = test_lock_blocking();
    let event_count = std::env::var("HASHTREE_BENCH_EVENTS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(2_000usize);
    let iterations = benchmark_read_iterations();
    let orders = benchmark_btree_orders();
    let dataset = load_index_benchmark_dataset(event_count, 64).unwrap();
    let cases = build_btree_query_cases(&dataset);
    let stored_events = dataset
        .events
        .iter()
        .map(stored_event_from_nostr_sdk_event)
        .collect::<Vec<_>>();

    println!(
        "btree-order benchmark: source={} events={} iterations={} orders={:?}",
        dataset.source,
        stored_events.len(),
        iterations,
        orders
    );
    println!(
        "network models are serialized fetch estimates: {}",
        NETWORK_MODELS
            .iter()
            .map(|model| format!(
                "{}={}ms_rtt/{}MiBps",
                model.name, model.rtt_ms, model.bandwidth_mib_per_s
            ))
            .collect::<Vec<_>>()
            .join(", ")
    );

    for order in orders {
        let tmp = TempDir::new().unwrap();
        let local_store =
            Arc::new(LocalStore::new(tmp.path().join("blobs"), &StorageBackend::Lmdb).unwrap());
        let event_store = NostrEventStore::with_options(
            Arc::clone(&local_store),
            NostrEventStoreOptions {
                btree_order: Some(order),
                ..NostrEventStoreOptions::default()
            },
        );
        let root = block_on(event_store.build(None, stored_events.clone()))
            .unwrap()
            .expect("benchmark build root");

        println!("btree-order={} root={}", order, hex::encode(root.hash));
        let mut warm_total_ms = 0.0f64;
        let mut model_totals = NETWORK_MODELS
            .iter()
            .map(|model| (model.name, 0.0f64))
            .collect::<HashMap<_, _>>();

        for case in &cases {
            let warm =
                benchmark_warm_query_case(Arc::clone(&local_store), &root, order, case, iterations);
            let cold =
                benchmark_cold_query_case(Arc::clone(&local_store), &root, order, case, iterations);
            warm_total_ms += warm.average_duration.as_secs_f64() * 1000.0;

            let model_estimates = NETWORK_MODELS
                .iter()
                .map(|model| {
                    let estimate = estimate_serialized_remote_ms(&cold.reads, *model);
                    *model_totals.get_mut(model.name).unwrap() += estimate;
                    format!("{}={:.2}ms", model.name, estimate)
                })
                .collect::<Vec<_>>()
                .join(" ");

            println!(
                    "btree-order={} query={} warm_avg={:?} warm_p95={:?} warm_blocks={} warm_unique_bytes={} cold_fetches={} cold_bytes={} cold_local_avg={:?} {}",
                    order,
                    case.name(),
                    warm.average_duration,
                    warm.p95_duration,
                    warm.reads.unique_blocks,
                    warm.reads.unique_bytes,
                    cold.reads.remote_fetches,
                    cold.reads.remote_bytes,
                    cold.average_duration,
                    model_estimates
                );
        }

        println!(
            "btree-order={} summary unweighted_warm_avg_ms={:.3} {}",
            order,
            warm_total_ms / cases.len() as f64,
            NETWORK_MODELS
                .iter()
                .map(|model| format!(
                    "{}={:.2}ms",
                    model.name,
                    model_totals[model.name] / cases.len() as f64
                ))
                .collect::<Vec<_>>()
                .join(" ")
        );
    }
}

#[test]
fn test_ensure_social_graph_mapsize_rounds_and_applies() {
    let _guard = test_lock_blocking();
    let tmp = TempDir::new().unwrap();
    let requested = 70 * 1024 * 1024;
    ensure_social_graph_mapsize(tmp.path(), requested).unwrap();
    let map_size = social_graph_map_size(Some(requested)).unwrap();
    let env = unsafe {
        heed::EnvOpenOptions::new()
            .map_size(map_size)
            .max_dbs(SOCIALGRAPH_MAX_DBS)
            .open(tmp.path())
    }
    .unwrap();
    assert!(env.info().map_size >= requested as usize);
    assert_eq!(env.info().map_size % page_size_bytes(), 0);
    let _closing = env.prepare_for_closing();
}

#[test]
fn test_social_graph_mapsize_honors_explicit_smaller_limit() {
    let requested = 128 * 1024 * 1024;
    let map_size = social_graph_map_size(Some(requested)).unwrap();
    assert!(map_size >= requested as usize);
    assert!(map_size < DEFAULT_SOCIALGRAPH_MAP_SIZE_BYTES as usize);
    assert_eq!(map_size % page_size_bytes(), 0);
}

#[test]
fn test_ingest_events_batches_graph_updates() {
    let _guard = test_lock_blocking();
    let tmp = TempDir::new().unwrap();
    let graph_store = open_test_social_graph_store(tmp.path()).unwrap();

    let root_keys = Keys::generate();
    let alice_keys = Keys::generate();
    let bob_keys = Keys::generate();

    let root_pk = root_keys.public_key().to_bytes();
    set_social_graph_root(&graph_store, &root_pk);

    let root_follows_alice = event_builder!(
        Kind::ContactList,
        "",
        vec![Tag::public_key(alice_keys.public_key())],
    )
    .custom_created_at(Timestamp::from_secs(10))
    .sign_with_keys(&root_keys)
    .unwrap();
    let alice_follows_bob = event_builder!(
        Kind::ContactList,
        "",
        vec![Tag::public_key(bob_keys.public_key())],
    )
    .custom_created_at(Timestamp::from_secs(11))
    .sign_with_keys(&alice_keys)
    .unwrap();

    ingest_parsed_events(
        &graph_store,
        &[root_follows_alice.clone(), alice_follows_bob.clone()],
    )
    .unwrap();

    assert_eq!(
        get_follow_distance(&graph_store, &alice_keys.public_key().to_bytes()),
        Some(1)
    );
    assert_eq!(
        get_follow_distance(&graph_store, &bob_keys.public_key().to_bytes()),
        Some(2)
    );

    let filter = Filter::new().kind(Kind::ContactList);
    let stored = query_events(&graph_store, &filter, 10);
    let ids = stored.into_iter().map(|event| event.id).collect::<Vec<_>>();
    assert!(ids.contains(&root_follows_alice.id));
    assert!(ids.contains(&alice_follows_bob.id));
}

#[test]
fn test_ingest_graph_events_updates_graph_without_indexing_events() {
    let _guard = test_lock_blocking();
    let tmp = TempDir::new().unwrap();
    let graph_store = open_test_social_graph_store(tmp.path()).unwrap();

    let root_keys = Keys::generate();
    let alice_keys = Keys::generate();

    let root_pk = root_keys.public_key().to_bytes();
    set_social_graph_root(&graph_store, &root_pk);

    let root_follows_alice = event_builder!(
        Kind::ContactList,
        "",
        vec![Tag::public_key(alice_keys.public_key())],
    )
    .custom_created_at(Timestamp::from_secs(10))
    .sign_with_keys(&root_keys)
    .unwrap();

    ingest_graph_parsed_events(&graph_store, std::slice::from_ref(&root_follows_alice)).unwrap();

    assert_eq!(
        get_follow_distance(&graph_store, &alice_keys.public_key().to_bytes()),
        Some(1)
    );
    let filter = Filter::new().kind(Kind::ContactList);
    assert!(query_events(&graph_store, &filter, 10).is_empty());
}
