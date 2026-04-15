//! Hashtree-native Nostr event indexes.

pub mod crawl;
pub mod tree_event_snapshots;
pub use crawl::{
    CrawlConfig, CrawlError, CrawlReport, EventSelectionPolicy, KindPriorityPolicy, NostrBridge,
    RelayFetchMode,
};
pub use tree_event_snapshots::{
    compare_tree_event_snapshots, is_newer_tree_event_snapshot,
    parse_tree_event_snapshot_permalink, read_tree_event_snapshot, resolve_snapshot_root_cid,
    serialize_tree_event_snapshot_permalink, snapshot_matches_root_cid, store_tree_event_snapshot,
    TreeEventSnapshotInfo, TreeEventSnapshotPermalink,
};

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use std::cell::RefCell;

use hashtree_collection::{
    load_collection_state, CollectionDefinition, CollectionOptions, CollectionPublishedSchema,
    CollectionSource, CollectionState, CollectionWriter,
};
use hashtree_core::{
    sha256, BufferedStore, Cid, HashTree, HashTreeConfig, HashTreeError, Store, TreeVisibility,
};
use hashtree_index::{BTree, BTreeError, BTreeOptions};
use serde::{Deserialize, Serialize};

pub const NOSTR_EVENT_ENVELOPE_VERSION: u8 = 1;
const MANIFEST_BY_AUTHOR_TIME: &str = "by-author-time";
const MANIFEST_BY_AUTHOR_KIND_TIME: &str = "by-author-kind-time";
const MANIFEST_BY_KIND_TIME: &str = "by-kind-time";
const MANIFEST_BY_KIND_TIME_AUTHOR: &str = "by-kind-time-author";
const MANIFEST_BY_TIME: &str = "by-time";
const MANIFEST_BY_TAG: &str = "by-tag";
const MANIFEST_REPLACEABLE: &str = "replaceable";
const MANIFEST_PARAMETERIZED_REPLACEABLE: &str = "parameterized-replaceable";
const NOSTR_EVENT_ITEM_FORMAT: &str = "nostr/event@1";
const NOSTR_EVENT_PROJECTION_FORMAT: &str = "hashtree/nostr-event-index@1";
const MAX_SNAPSHOT_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredNostrEvent {
    pub id: String,
    pub pubkey: String,
    pub created_at: u64,
    pub kind: u32,
    pub tags: Vec<Vec<String>>,
    pub content: String,
    pub sig: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedHashtreeRootEvent {
    pub event: StoredNostrEvent,
    pub tree_name: String,
    pub root_cid: Cid,
    pub visibility: TreeVisibility,
    pub labels: Vec<String>,
    pub encrypted_key: Option<String>,
    pub key_id: Option<String>,
    pub self_encrypted_key: Option<String>,
    pub self_encrypted_link_key: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct NostrEventManifest {
    pub by_id: Option<Cid>,
    pub by_author_time: Option<Cid>,
    pub by_author_kind_time: Option<Cid>,
    pub by_kind_time: Option<Cid>,
    pub by_kind_time_author: Option<Cid>,
    pub by_time: Option<Cid>,
    pub by_tag: Option<Cid>,
    pub replaceable: Option<Cid>,
    pub parameterized_replaceable: Option<Cid>,
}

#[derive(Debug, Clone, Default)]
pub struct ListEventsOptions {
    pub limit: Option<usize>,
    pub since: Option<u64>,
    pub until: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct ProfileStat {
    pub label: &'static str,
    pub count: u64,
    pub total: Duration,
    pub max: Duration,
}

#[derive(Debug, Default)]
struct ProfileAccumulator {
    count: u64,
    total: Duration,
    max: Duration,
}

static PROFILE_ENABLED: AtomicBool = AtomicBool::new(false);

thread_local! {
    static PROFILE_STATE: RefCell<BTreeMap<&'static str, ProfileAccumulator>> =
        const { RefCell::new(BTreeMap::new()) };
}

pub fn set_profile_enabled(enabled: bool) {
    PROFILE_ENABLED.store(enabled, Ordering::Relaxed);
}

pub fn reset_profile() {
    PROFILE_STATE.with(|state| state.borrow_mut().clear());
}

pub fn take_profile() -> Vec<ProfileStat> {
    PROFILE_STATE.with(|state| {
        state
            .borrow()
            .iter()
            .map(|(&label, acc)| ProfileStat {
                label,
                count: acc.count,
                total: acc.total,
                max: acc.max,
            })
            .collect()
    })
}

pub struct ProfileGuard {
    label: &'static str,
    started: Option<Instant>,
}

impl ProfileGuard {
    pub fn new(label: &'static str) -> Self {
        let started = PROFILE_ENABLED.load(Ordering::Relaxed).then(Instant::now);
        Self { label, started }
    }
}

impl Drop for ProfileGuard {
    fn drop(&mut self) {
        let Some(started) = self.started else {
            return;
        };
        let elapsed = started.elapsed();
        PROFILE_STATE.with(|state| {
            let mut state = state.borrow_mut();
            let entry = state.entry(self.label).or_default();
            entry.count += 1;
            entry.total += elapsed;
            entry.max = entry.max.max(elapsed);
        });
    }
}

#[derive(Debug, thiserror::Error)]
pub enum NostrEventStoreError {
    #[error("hash tree error: {0}")]
    HashTree(#[from] HashTreeError),
    #[error("index error: {0}")]
    Index(#[from] BTreeError),
    #[error("collection error: {0}")]
    Collection(#[from] hashtree_collection::CollectionError),
    #[error("encode error: {0}")]
    Encode(#[from] rmp_serde::encode::Error),
    #[error("decode error: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Validation(String),
}

const MISSING_STORED_EVENT_BLOB_ERROR: &str = "stored nostr event blob is missing";

fn is_missing_stored_event_error(err: &NostrEventStoreError) -> bool {
    matches!(
        err,
        NostrEventStoreError::Validation(message) if message == MISSING_STORED_EVENT_BLOB_ERROR
    )
}

pub struct NostrEventStore<S: Store> {
    store: Arc<S>,
    tree: HashTree<S>,
    index: BTree<S>,
    options: NostrEventStoreOptions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ReplaceableSlot {
    Replaceable,
    Parameterized,
}

#[derive(Debug, Clone)]
struct ExistingReplaceableEvent {
    event: StoredNostrEvent,
    cid: Cid,
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
enum ReplaceableDecision {
    Accept {
        replaced: Option<ExistingReplaceableEvent>,
        slot: Option<(ReplaceableSlot, String)>,
    },
    Reject,
}

pub fn encode_signed_event_json(event: &StoredNostrEvent) -> Result<Vec<u8>, NostrEventStoreError> {
    let normalized = normalize_signed_event(event.clone())?;
    Ok(serde_json::to_vec(&normalized)?)
}

pub fn decode_signed_event_json(data: &[u8]) -> Result<StoredNostrEvent, NostrEventStoreError> {
    let decoded: StoredNostrEvent = serde_json::from_slice(data)?;
    normalize_signed_event(decoded)
}

pub fn encode_stored_event_msgpack(
    event: &StoredNostrEvent,
) -> Result<Vec<u8>, NostrEventStoreError> {
    let normalized = normalize_signed_event(event.clone())?;
    Ok(rmp_serde::to_vec(&(
        NOSTR_EVENT_ENVELOPE_VERSION,
        normalized.id,
        normalized.pubkey,
        normalized.created_at,
        normalized.kind,
        normalized.tags,
        normalized.content,
        normalized.sig,
    ))?)
}

pub fn decode_stored_event_msgpack(data: &[u8]) -> Result<StoredNostrEvent, NostrEventStoreError> {
    let (version, id, pubkey, created_at, kind, tags, content, sig): (
        u8,
        String,
        String,
        u64,
        u32,
        Vec<Vec<String>>,
        String,
        String,
    ) = rmp_serde::from_slice(data)?;

    if version != NOSTR_EVENT_ENVELOPE_VERSION {
        return Err(NostrEventStoreError::Validation(format!(
            "unsupported event envelope version: {version}"
        )));
    }

    normalize_signed_event(StoredNostrEvent {
        id,
        pubkey,
        created_at,
        kind,
        tags,
        content,
        sig,
    })
}

pub async fn store_signed_event_snapshot<S: Store>(
    store: Arc<S>,
    event: &StoredNostrEvent,
) -> Result<Cid, NostrEventStoreError> {
    let bytes = encode_signed_event_json(event)?;
    let tree = HashTree::new(HashTreeConfig::new(store).public());
    let (cid, _) = tree.put(&bytes).await?;
    Ok(cid)
}

pub async fn read_signed_event_snapshot<S: Store>(
    store: Arc<S>,
    snapshot_cid: &Cid,
    max_bytes: Option<usize>,
) -> Result<StoredNostrEvent, NostrEventStoreError> {
    let max_bytes = max_bytes.unwrap_or(MAX_SNAPSHOT_BYTES);
    let tree = HashTree::new(HashTreeConfig::new(store).public());
    let data = tree
        .get(snapshot_cid, Some((max_bytes + 1) as u64))
        .await?
        .ok_or_else(|| {
            NostrEventStoreError::Validation("signed Nostr event snapshot is missing".to_string())
        })?;
    if data.len() > max_bytes {
        return Err(NostrEventStoreError::Validation(format!(
            "signed Nostr event snapshot exceeds {max_bytes} bytes"
        )));
    }
    decode_signed_event_json(&data)
}

pub fn parse_hashtree_root_event(
    event: &StoredNostrEvent,
) -> Result<Option<ParsedHashtreeRootEvent>, NostrEventStoreError> {
    let normalized = normalize_signed_event(event.clone())?;
    if normalized.kind != 30078 {
        return Ok(None);
    }

    let tree_name = normalized
        .tags
        .iter()
        .find(|tag| tag.first().map(String::as_str) == Some("d"))
        .and_then(|tag| tag.get(1))
        .cloned();
    let hash_hex = normalized
        .tags
        .iter()
        .find(|tag| tag.first().map(String::as_str) == Some("hash"))
        .and_then(|tag| tag.get(1))
        .cloned();
    let (Some(tree_name), Some(hash_hex)) = (tree_name, hash_hex) else {
        return Ok(None);
    };

    if has_any_label(&normalized) && !has_label(&normalized, "hashtree") {
        return Ok(None);
    }

    let labels = unique_labels(
        normalized
            .tags
            .iter()
            .filter(|tag| tag.first().map(String::as_str) == Some("l"))
            .filter_map(|tag| tag.get(1).cloned())
            .collect(),
    );
    let key_hex = normalized
        .tags
        .iter()
        .find(|tag| tag.first().map(String::as_str) == Some("key"))
        .and_then(|tag| tag.get(1))
        .cloned();
    let encrypted_key = normalized
        .tags
        .iter()
        .find(|tag| tag.first().map(String::as_str) == Some("encryptedKey"))
        .and_then(|tag| tag.get(1))
        .cloned();
    let key_id = normalized
        .tags
        .iter()
        .find(|tag| tag.first().map(String::as_str) == Some("keyId"))
        .and_then(|tag| tag.get(1))
        .cloned();
    let self_encrypted_key = normalized
        .tags
        .iter()
        .find(|tag| tag.first().map(String::as_str) == Some("selfEncryptedKey"))
        .and_then(|tag| tag.get(1))
        .cloned();
    let self_encrypted_link_key = normalized
        .tags
        .iter()
        .find(|tag| tag.first().map(String::as_str) == Some("selfEncryptedLinkKey"))
        .and_then(|tag| tag.get(1))
        .cloned();

    let visibility = if encrypted_key.is_some() {
        TreeVisibility::LinkVisible
    } else if self_encrypted_key.is_some() {
        TreeVisibility::Private
    } else {
        TreeVisibility::Public
    };

    let hash = hashtree_core::from_hex(&hash_hex).map_err(|_| {
        NostrEventStoreError::Validation(
            "root hash must be a lowercase 64-character hex string".to_string(),
        )
    })?;
    let key = match (visibility, key_hex) {
        (TreeVisibility::Public, Some(key_hex)) => {
            Some(hashtree_core::from_hex(&key_hex).map_err(|_| {
                NostrEventStoreError::Validation(
                    "root key must be a lowercase 64-character hex string".to_string(),
                )
            })?)
        }
        _ => None,
    };

    Ok(Some(ParsedHashtreeRootEvent {
        event: normalized,
        tree_name,
        root_cid: Cid { hash, key },
        visibility,
        labels,
        encrypted_key,
        key_id,
        self_encrypted_key,
        self_encrypted_link_key,
    }))
}

#[derive(Debug, Clone, Default)]
pub struct NostrEventStoreOptions {
    pub btree_order: Option<usize>,
}

fn nostr_collection_definition() -> CollectionDefinition<StoredNostrEvent> {
    CollectionDefinition::new(|event: &StoredNostrEvent| event.id.clone())
        .with_published_schema(
            CollectionPublishedSchema::new()
                .with_item_format(NOSTR_EVENT_ITEM_FORMAT)
                .with_projection_format(NOSTR_EVENT_PROJECTION_FORMAT),
        )
        .with_key_index(MANIFEST_BY_AUTHOR_TIME, |event| {
            vec![author_time_key(event)]
        })
        .with_key_index(MANIFEST_BY_AUTHOR_KIND_TIME, |event| {
            vec![author_kind_time_key(event)]
        })
        .with_key_index(MANIFEST_BY_KIND_TIME, |event| vec![kind_time_key(event)])
        .with_key_index(MANIFEST_BY_KIND_TIME_AUTHOR, |event| {
            vec![kind_time_author_key(event)]
        })
        .with_key_index(MANIFEST_BY_TIME, |event| vec![time_key(event)])
        .with_key_index(MANIFEST_BY_TAG, |event| tag_keys(event))
        .with_key_index(MANIFEST_REPLACEABLE, |event| {
            if is_replaceable_kind(event.kind) {
                vec![replaceable_key(&event.pubkey, event.kind)]
            } else {
                Vec::new()
            }
        })
        .with_key_index(MANIFEST_PARAMETERIZED_REPLACEABLE, |event| {
            if is_parameterized_replaceable_kind(event.kind) {
                vec![parameterized_replaceable_key(
                    &event.pubkey,
                    event.kind,
                    &parameterized_replaceable_d_tag(event),
                )]
            } else {
                Vec::new()
            }
        })
}

fn nostr_collection_options(options: &NostrEventStoreOptions) -> CollectionOptions {
    CollectionOptions {
        btree_order: options.btree_order,
    }
}

fn collection_state_from_nostr_manifest(manifest: &NostrEventManifest) -> CollectionState {
    let mut key_roots = BTreeMap::new();
    key_roots.insert(
        MANIFEST_BY_AUTHOR_TIME.to_string(),
        manifest.by_author_time.clone(),
    );
    key_roots.insert(
        MANIFEST_BY_AUTHOR_KIND_TIME.to_string(),
        manifest.by_author_kind_time.clone(),
    );
    key_roots.insert(
        MANIFEST_BY_KIND_TIME.to_string(),
        manifest.by_kind_time.clone(),
    );
    key_roots.insert(
        MANIFEST_BY_KIND_TIME_AUTHOR.to_string(),
        manifest.by_kind_time_author.clone(),
    );
    key_roots.insert(MANIFEST_BY_TIME.to_string(), manifest.by_time.clone());
    key_roots.insert(MANIFEST_BY_TAG.to_string(), manifest.by_tag.clone());
    key_roots.insert(
        MANIFEST_REPLACEABLE.to_string(),
        manifest.replaceable.clone(),
    );
    key_roots.insert(
        MANIFEST_PARAMETERIZED_REPLACEABLE.to_string(),
        manifest.parameterized_replaceable.clone(),
    );
    CollectionState {
        by_id_root: manifest.by_id.clone(),
        key_roots,
        search_roots: BTreeMap::new(),
    }
}

fn nostr_manifest_from_collection_state(state: &CollectionState) -> NostrEventManifest {
    NostrEventManifest {
        by_id: state.by_id_root.clone(),
        by_author_time: state.key_root(MANIFEST_BY_AUTHOR_TIME).cloned(),
        by_author_kind_time: state.key_root(MANIFEST_BY_AUTHOR_KIND_TIME).cloned(),
        by_kind_time: state.key_root(MANIFEST_BY_KIND_TIME).cloned(),
        by_kind_time_author: state.key_root(MANIFEST_BY_KIND_TIME_AUTHOR).cloned(),
        by_time: state.key_root(MANIFEST_BY_TIME).cloned(),
        by_tag: state.key_root(MANIFEST_BY_TAG).cloned(),
        replaceable: state.key_root(MANIFEST_REPLACEABLE).cloned(),
        parameterized_replaceable: state.key_root(MANIFEST_PARAMETERIZED_REPLACEABLE).cloned(),
    }
}

impl<S: Store> NostrEventStore<S> {
    pub fn new(store: Arc<S>) -> Self {
        Self::with_options(store, NostrEventStoreOptions::default())
    }

    pub fn with_options(store: Arc<S>, options: NostrEventStoreOptions) -> Self {
        Self {
            store: Arc::clone(&store),
            tree: HashTree::new(HashTreeConfig::new(Arc::clone(&store))),
            index: BTree::new(
                store,
                BTreeOptions {
                    order: options.btree_order,
                },
            ),
            options,
        }
    }

    pub fn encode_event(&self, event: &StoredNostrEvent) -> Result<Vec<u8>, NostrEventStoreError> {
        encode_stored_event_msgpack(&self.validate_event_shape(event.clone())?)
    }

    pub fn decode_event(&self, data: &[u8]) -> Result<StoredNostrEvent, NostrEventStoreError> {
        self.validate_event_shape(decode_stored_event_msgpack(data)?)
    }

    pub async fn add(
        &self,
        root: Option<&Cid>,
        event: StoredNostrEvent,
    ) -> Result<Cid, NostrEventStoreError> {
        let buffered_store = Arc::new(BufferedStore::new_optimistic(Arc::clone(&self.store)));
        let buffered_writer =
            NostrEventStore::with_options(Arc::clone(&buffered_store), self.options.clone());
        let normalized = {
            let _profile = ProfileGuard::new("nostr.add.validate_event");
            buffered_writer.validate_event(event).await?
        };
        let mut manifest = {
            let _profile = ProfileGuard::new("nostr.add.get_manifest");
            buffered_writer.get_manifest(root).await?
        };
        let mut obsolete_event_cids = Vec::new();
        buffered_writer
            .insert_into_manifest(&mut manifest, normalized, &mut obsolete_event_cids)
            .await?;

        let Some(root) = ({
            let _profile = ProfileGuard::new("nostr.add.write_manifest");
            buffered_writer.write_manifest(&manifest).await?
        }) else {
            return Err(NostrEventStoreError::Validation(
                "failed to write event manifest".to_string(),
            ));
        };
        {
            let _profile = ProfileGuard::new("nostr.add.flush");
            buffered_store.flush().await.map_err(|err| {
                NostrEventStoreError::HashTree(HashTreeError::Store(err.to_string()))
            })?;
        }
        self.delete_obsolete_event_blobs(&obsolete_event_cids)
            .await?;
        Ok(root)
    }

    pub async fn build<I>(
        &self,
        root: Option<&Cid>,
        events: I,
    ) -> Result<Option<Cid>, NostrEventStoreError>
    where
        I: IntoIterator<Item = StoredNostrEvent>,
    {
        let mut events: Vec<StoredNostrEvent> = events.into_iter().collect();
        if events.is_empty() {
            return Ok(root.cloned());
        }
        events = retain_latest_replaceable_events(events);
        events.sort_by(|left, right| match compare_events(left, right) {
            x if x < 0 => std::cmp::Ordering::Less,
            x if x > 0 => std::cmp::Ordering::Greater,
            _ => std::cmp::Ordering::Equal,
        });

        let buffered_store = Arc::new(BufferedStore::new_optimistic(Arc::clone(&self.store)));
        let buffered_writer =
            NostrEventStore::with_options(Arc::clone(&buffered_store), self.options.clone());
        let mut obsolete_event_cids = Vec::new();
        let next_root = if root.is_none() {
            buffered_writer.build_manifest_from_events(events).await?
        } else {
            let mut manifest = buffered_writer.get_manifest(root).await?;
            for event in events {
                let normalized = buffered_writer.validate_event(event).await?;
                buffered_writer
                    .insert_into_manifest(&mut manifest, normalized, &mut obsolete_event_cids)
                    .await?;
            }
            buffered_writer.write_manifest(&manifest).await?
        };
        buffered_store
            .flush()
            .await
            .map_err(|err| NostrEventStoreError::HashTree(HashTreeError::Store(err.to_string())))?;
        self.delete_obsolete_event_blobs(&obsolete_event_cids)
            .await?;
        Ok(next_root)
    }

    pub async fn upgrade_manifest_indexes(
        &self,
        root: Option<&Cid>,
    ) -> Result<Option<Cid>, NostrEventStoreError> {
        let Some(root) = root else {
            return Ok(None);
        };

        let manifest = self.get_manifest(Some(root)).await?;
        let buffered_store = Arc::new(BufferedStore::new_optimistic(Arc::clone(&self.store)));
        let buffered_writer =
            NostrEventStore::with_options(Arc::clone(&buffered_store), self.options.clone());

        let Some(next_manifest) = buffered_writer
            .upgrade_manifest_with_missing_indexes(&manifest)
            .await?
        else {
            return Ok(Some(root.clone()));
        };

        let next_root = buffered_writer.write_manifest(&next_manifest).await?;
        buffered_store
            .flush()
            .await
            .map_err(|err| NostrEventStoreError::HashTree(HashTreeError::Store(err.to_string())))?;
        Ok(next_root)
    }

    pub async fn get_by_id(
        &self,
        root: Option<&Cid>,
        event_id: &str,
    ) -> Result<Option<StoredNostrEvent>, NostrEventStoreError> {
        validate_lower_hex(event_id, 64, "event id")?;
        let manifest = self.get_manifest(root).await?;
        let source = self.collection_source_from_manifest(&manifest);
        let Some(event_cid) = source.get(event_id).await? else {
            return Ok(None);
        };
        match self.read_stored_event(&event_cid).await {
            Ok(event) => Ok(Some(event)),
            Err(err) if is_missing_stored_event_error(&err) => Ok(None),
            Err(err) => Err(err),
        }
    }

    pub async fn list_by_author(
        &self,
        root: Option<&Cid>,
        pubkey: &str,
        options: ListEventsOptions,
    ) -> Result<Vec<StoredNostrEvent>, NostrEventStoreError> {
        let manifest = self.get_manifest(root).await?;
        let Some(by_author_time) = manifest.by_author_time.as_ref() else {
            return Ok(Vec::new());
        };
        self.collect_events(
            by_author_time,
            &format!("{}:", validate_lower_hex(pubkey, 64, "pubkey")?),
            &options,
        )
        .await
    }

    pub async fn list_by_author_and_kind(
        &self,
        root: Option<&Cid>,
        pubkey: &str,
        kind: u32,
        options: ListEventsOptions,
    ) -> Result<Vec<StoredNostrEvent>, NostrEventStoreError> {
        let manifest = self.get_manifest(root).await?;
        let Some(by_author_kind_time) = manifest.by_author_kind_time.as_ref() else {
            return Ok(Vec::new());
        };

        self.collect_events(
            by_author_kind_time,
            &format!(
                "{}:{}:",
                validate_lower_hex(pubkey, 64, "pubkey")?,
                pad_kind(kind)
            ),
            &options,
        )
        .await
    }

    pub async fn list_by_kind(
        &self,
        root: Option<&Cid>,
        kind: u32,
        options: ListEventsOptions,
    ) -> Result<Vec<StoredNostrEvent>, NostrEventStoreError> {
        let manifest = self.get_manifest(root).await?;
        let Some(by_kind_time) = manifest.by_kind_time.as_ref() else {
            return Ok(Vec::new());
        };

        self.collect_events(by_kind_time, &format!("{}:", pad_kind(kind)), &options)
            .await
    }

    pub async fn list_by_kind_lossy(
        &self,
        root: Option<&Cid>,
        kind: u32,
        options: ListEventsOptions,
    ) -> Result<Vec<StoredNostrEvent>, NostrEventStoreError> {
        let manifest = self.get_manifest(root).await?;
        let Some(by_kind_time) = manifest.by_kind_time.as_ref() else {
            return Ok(Vec::new());
        };

        self.collect_events_lossy(by_kind_time, &format!("{}:", pad_kind(kind)), &options)
            .await
    }

    pub async fn get_replaceable(
        &self,
        root: Option<&Cid>,
        pubkey: &str,
        kind: u32,
    ) -> Result<Option<StoredNostrEvent>, NostrEventStoreError> {
        let manifest = self.get_manifest(root).await?;
        let key = replaceable_key(&validate_lower_hex(pubkey, 64, "pubkey")?, kind);
        let source = self.collection_source_from_manifest(&manifest);
        let Some(cid) = source.get_index_link(MANIFEST_REPLACEABLE, &key).await? else {
            return Ok(None);
        };
        match self.read_stored_event(&cid).await {
            Ok(event) => Ok(Some(event)),
            Err(err) if is_missing_stored_event_error(&err) => Ok(None),
            Err(err) => Err(err),
        }
    }

    pub async fn list_recent(
        &self,
        root: Option<&Cid>,
        options: ListEventsOptions,
    ) -> Result<Vec<StoredNostrEvent>, NostrEventStoreError> {
        let manifest = self.get_manifest(root).await?;
        let Some(by_time) = manifest.by_time.as_ref() else {
            return Ok(Vec::new());
        };
        self.collect_events(by_time, "", &options).await
    }

    pub async fn list_recent_lossy(
        &self,
        root: Option<&Cid>,
        options: ListEventsOptions,
    ) -> Result<Vec<StoredNostrEvent>, NostrEventStoreError> {
        let manifest = self.get_manifest(root).await?;
        let Some(by_time) = manifest.by_time.as_ref() else {
            return Ok(Vec::new());
        };
        self.collect_events_lossy(by_time, "", &options).await
    }

    pub async fn list_by_tag(
        &self,
        root: Option<&Cid>,
        tag_name: &str,
        tag_value: &str,
        options: ListEventsOptions,
    ) -> Result<Vec<StoredNostrEvent>, NostrEventStoreError> {
        let manifest = self.get_manifest(root).await?;
        let Some(by_tag) = manifest.by_tag.as_ref() else {
            return Ok(Vec::new());
        };
        let prefix = tag_prefix(tag_name, tag_value)?;
        self.collect_events(by_tag, &prefix, &options).await
    }

    pub async fn get_parameterized_replaceable(
        &self,
        root: Option<&Cid>,
        pubkey: &str,
        kind: u32,
        d_tag: &str,
    ) -> Result<Option<StoredNostrEvent>, NostrEventStoreError> {
        let manifest = self.get_manifest(root).await?;
        let key =
            parameterized_replaceable_key(&validate_lower_hex(pubkey, 64, "pubkey")?, kind, d_tag);
        let source = self.collection_source_from_manifest(&manifest);
        let Some(cid) = source
            .get_index_link(MANIFEST_PARAMETERIZED_REPLACEABLE, &key)
            .await?
        else {
            return Ok(None);
        };
        match self.read_stored_event(&cid).await {
            Ok(event) => Ok(Some(event)),
            Err(err) if is_missing_stored_event_error(&err) => Ok(None),
            Err(err) => Err(err),
        }
    }

    pub async fn get_manifest(
        &self,
        root: Option<&Cid>,
    ) -> Result<NostrEventManifest, NostrEventStoreError> {
        let definition = nostr_collection_definition();
        let state = load_collection_state(Arc::clone(&self.store), &definition, root).await?;
        Ok(nostr_manifest_from_collection_state(&state))
    }

    async fn collect_events(
        &self,
        root: &Cid,
        prefix: &str,
        options: &ListEventsOptions,
    ) -> Result<Vec<StoredNostrEvent>, NostrEventStoreError> {
        let mut events = Vec::new();
        let entries = if prefix.is_empty() {
            self.index.links_entries(Some(root)).await?
        } else {
            self.index.prefix_links(root, prefix).await?
        };
        for (key, cid) in entries {
            let created_at = created_at_from_index_key(&key)?;
            if options.until.is_some_and(|until| created_at > until) {
                continue;
            }
            if options.since.is_some_and(|since| created_at < since) {
                break;
            }
            events.push(self.read_stored_event(&cid).await?);
            if options.limit.is_some_and(|limit| events.len() >= limit) {
                break;
            }
        }
        Ok(events)
    }

    async fn collect_events_lossy(
        &self,
        root: &Cid,
        prefix: &str,
        options: &ListEventsOptions,
    ) -> Result<Vec<StoredNostrEvent>, NostrEventStoreError> {
        let mut events = Vec::new();
        let entries = if prefix.is_empty() {
            self.index.links_entries(Some(root)).await?
        } else {
            self.index.prefix_links(root, prefix).await?
        };
        for (key, cid) in entries {
            let created_at = created_at_from_index_key(&key)?;
            if options.until.is_some_and(|until| created_at > until) {
                continue;
            }
            if options.since.is_some_and(|since| created_at < since) {
                break;
            }
            match self.read_stored_event(&cid).await {
                Ok(event) => events.push(event),
                Err(NostrEventStoreError::Validation(message))
                    if message == MISSING_STORED_EVENT_BLOB_ERROR => {}
                Err(err) => return Err(err),
            }
            if options.limit.is_some_and(|limit| events.len() >= limit) {
                break;
            }
        }
        Ok(events)
    }

    async fn read_stored_event(&self, cid: &Cid) -> Result<StoredNostrEvent, NostrEventStoreError> {
        let Some(data) = self.tree.get(cid, None).await? else {
            return Err(NostrEventStoreError::Validation(
                MISSING_STORED_EVENT_BLOB_ERROR.to_string(),
            ));
        };
        self.decode_event(&data)
    }

    async fn insert_into_manifest(
        &self,
        manifest: &mut NostrEventManifest,
        normalized: StoredNostrEvent,
        obsolete_event_cids: &mut Vec<Cid>,
    ) -> Result<(), NostrEventStoreError> {
        let replaceable = self
            .resolve_replaceable_decision(manifest, &normalized)
            .await?;
        let (_replaceable_slot, replaced_existing) = match replaceable {
            ReplaceableDecision::Reject => return Ok(()),
            ReplaceableDecision::Accept { replaced, slot } => (slot, replaced),
        };

        let event_bytes = {
            let _profile = ProfileGuard::new("nostr.add.encode_event");
            self.encode_validated_event(&normalized)?
        };
        let (event_cid, _size) = {
            let _profile = ProfileGuard::new("nostr.add.put_file");
            self.tree.put_file(&event_bytes).await?
        };

        let mut collection = self.collection_writer_from_manifest(manifest);
        {
            let _profile = ProfileGuard::new("nostr.add.index.collection");
            collection
                .put(
                    &normalized,
                    &event_cid,
                    replaced_existing.as_ref().map(|existing| &existing.event),
                )
                .await?;
        }
        *manifest = nostr_manifest_from_collection_state(collection.state());

        if let Some(existing) = replaced_existing.as_ref() {
            obsolete_event_cids.push(existing.cid.clone());
        }

        Ok(())
    }

    async fn build_manifest_from_events(
        &self,
        events: Vec<StoredNostrEvent>,
    ) -> Result<Option<Cid>, NostrEventStoreError> {
        let mut indexed_events = Vec::with_capacity(events.len());

        for event in events {
            let normalized = self.validate_event(event).await?;
            let event_bytes = self.encode_validated_event(&normalized)?;
            let (event_cid, _size) = self.tree.put_file(&event_bytes).await?;
            indexed_events.push((normalized, event_cid));
        }

        let mut collection = CollectionWriter::with_options(
            Arc::clone(&self.store),
            nostr_collection_definition(),
            nostr_collection_options(&self.options),
        );
        collection.rebuild(indexed_events).await?;
        collection.write_root().await.map_err(Into::into)
    }

    async fn upgrade_manifest_with_missing_indexes(
        &self,
        manifest: &NostrEventManifest,
    ) -> Result<Option<NostrEventManifest>, NostrEventStoreError> {
        let mut next_manifest = manifest.clone();
        let mut changed = false;

        if next_manifest.by_kind_time_author.is_none() {
            if let Some(by_author_kind_time_root) = manifest.by_author_kind_time.as_ref() {
                let entries = self
                    .index
                    .links_entries(Some(by_author_kind_time_root))
                    .await?;
                let mut derived = BTreeMap::new();
                for (key, cid) in entries {
                    derived.insert(kind_time_author_key_from_author_kind_time_key(&key)?, cid);
                }
                next_manifest.by_kind_time_author = self.index.build_links(derived).await?;
                changed = true;
            }
        }

        if changed {
            Ok(Some(next_manifest))
        } else {
            Ok(None)
        }
    }

    async fn resolve_replaceable_decision(
        &self,
        manifest: &NostrEventManifest,
        event: &StoredNostrEvent,
    ) -> Result<ReplaceableDecision, NostrEventStoreError> {
        let slot = if is_replaceable_kind(event.kind) {
            Some((
                ReplaceableSlot::Replaceable,
                replaceable_key(&event.pubkey, event.kind),
            ))
        } else if is_parameterized_replaceable_kind(event.kind) {
            Some((
                ReplaceableSlot::Parameterized,
                parameterized_replaceable_key(
                    &event.pubkey,
                    event.kind,
                    &parameterized_replaceable_d_tag(event),
                ),
            ))
        } else {
            None
        };

        let Some((slot_kind, key)) = slot else {
            return Ok(ReplaceableDecision::Accept {
                replaced: None,
                slot: None,
            });
        };

        let source = self.collection_source_from_manifest(manifest);
        let existing_cid = match slot_kind {
            ReplaceableSlot::Replaceable => {
                source.get_index_link(MANIFEST_REPLACEABLE, &key).await?
            }
            ReplaceableSlot::Parameterized => {
                source
                    .get_index_link(MANIFEST_PARAMETERIZED_REPLACEABLE, &key)
                    .await?
            }
        };

        let Some(existing_cid) = existing_cid else {
            return Ok(ReplaceableDecision::Accept {
                replaced: None,
                slot: Some((slot_kind, key)),
            });
        };

        let existing = match self.read_stored_event(&existing_cid).await {
            Ok(existing) => existing,
            Err(err) if is_missing_stored_event_error(&err) => {
                return Ok(ReplaceableDecision::Accept {
                    replaced: None,
                    slot: Some((slot_kind, key)),
                });
            }
            Err(err) => return Err(err),
        };
        if compare_events(event, &existing) > 0 {
            return Ok(ReplaceableDecision::Accept {
                replaced: Some(ExistingReplaceableEvent {
                    event: existing,
                    cid: existing_cid,
                }),
                slot: Some((slot_kind, key)),
            });
        }

        Ok(ReplaceableDecision::Reject)
    }

    async fn delete_obsolete_event_blobs(
        &self,
        obsolete_event_cids: &[Cid],
    ) -> Result<(), NostrEventStoreError> {
        for cid in obsolete_event_cids {
            self.store.delete(&cid.hash).await.map_err(|err| {
                NostrEventStoreError::HashTree(HashTreeError::Store(err.to_string()))
            })?;
        }
        Ok(())
    }

    async fn write_manifest(
        &self,
        manifest: &NostrEventManifest,
    ) -> Result<Option<Cid>, NostrEventStoreError> {
        let collection = self.collection_writer_from_manifest(manifest);
        Ok(collection.write_root().await?)
    }

    fn collection_source_from_manifest(
        &self,
        manifest: &NostrEventManifest,
    ) -> CollectionSource<S> {
        CollectionSource::new(
            Arc::clone(&self.store),
            collection_state_from_nostr_manifest(manifest),
        )
    }

    fn collection_writer_from_manifest(
        &self,
        manifest: &NostrEventManifest,
    ) -> CollectionWriter<S, StoredNostrEvent> {
        CollectionWriter::with_state_and_options(
            Arc::clone(&self.store),
            nostr_collection_definition(),
            collection_state_from_nostr_manifest(manifest),
            nostr_collection_options(&self.options),
        )
    }

    async fn validate_event(
        &self,
        event: StoredNostrEvent,
    ) -> Result<StoredNostrEvent, NostrEventStoreError> {
        let normalized = self.validate_event_shape(event)?;
        let payload = serde_json::to_string(&(
            0u8,
            normalized.pubkey.clone(),
            normalized.created_at,
            normalized.kind,
            normalized.tags.clone(),
            normalized.content.clone(),
        ))?;
        let computed = hex::encode(sha256(payload.as_bytes()));
        if computed != normalized.id {
            return Err(NostrEventStoreError::Validation(
                "event id does not match canonical nostr payload".to_string(),
            ));
        }
        Ok(normalized)
    }

    fn encode_validated_event(
        &self,
        event: &StoredNostrEvent,
    ) -> Result<Vec<u8>, NostrEventStoreError> {
        encode_stored_event_msgpack(event)
    }

    fn validate_event_shape(
        &self,
        event: StoredNostrEvent,
    ) -> Result<StoredNostrEvent, NostrEventStoreError> {
        normalize_signed_event(event)
    }
}

fn normalize_signed_event(
    event: StoredNostrEvent,
) -> Result<StoredNostrEvent, NostrEventStoreError> {
    Ok(StoredNostrEvent {
        id: validate_lower_hex(&event.id, 64, "event id")?,
        pubkey: validate_lower_hex(&event.pubkey, 64, "pubkey")?,
        created_at: event.created_at,
        kind: event.kind,
        tags: event.tags,
        content: event.content,
        sig: validate_lower_hex(&event.sig, 128, "signature")?,
    })
}

fn has_label(event: &StoredNostrEvent, label: &str) -> bool {
    event.tags.iter().any(|tag| {
        tag.first().map(String::as_str) == Some("l")
            && tag.get(1).map(String::as_str) == Some(label)
    })
}

fn has_any_label(event: &StoredNostrEvent) -> bool {
    event
        .tags
        .iter()
        .any(|tag| tag.first().map(String::as_str) == Some("l"))
}

fn unique_labels(labels: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut result = Vec::new();
    for label in labels {
        if label.is_empty() || !seen.insert(label.clone()) {
            continue;
        }
        result.push(label);
    }
    result
}

fn validate_lower_hex(
    value: &str,
    expected_len: usize,
    label: &str,
) -> Result<String, NostrEventStoreError> {
    let valid = value.len() == expected_len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if valid {
        Ok(value.to_string())
    } else {
        Err(NostrEventStoreError::Validation(format!(
            "{label} must be a lowercase {expected_len}-character hex string"
        )))
    }
}

fn pad_kind(kind: u32) -> String {
    format!("{kind:08x}")
}

fn reverse_timestamp(created_at: u64) -> String {
    format!("{:016x}", u64::MAX - created_at)
}

fn author_time_key(event: &StoredNostrEvent) -> String {
    format!(
        "{}:{}:{}",
        event.pubkey,
        reverse_timestamp(event.created_at),
        event.id
    )
}

fn author_kind_time_key(event: &StoredNostrEvent) -> String {
    format!(
        "{}:{}:{}:{}",
        event.pubkey,
        pad_kind(event.kind),
        reverse_timestamp(event.created_at),
        event.id
    )
}

fn kind_time_key(event: &StoredNostrEvent) -> String {
    format!(
        "{}:{}:{}",
        pad_kind(event.kind),
        reverse_timestamp(event.created_at),
        event.id
    )
}

fn kind_time_author_key(event: &StoredNostrEvent) -> String {
    format!(
        "{}:{}:{}:{}",
        pad_kind(event.kind),
        reverse_timestamp(event.created_at),
        event.pubkey,
        event.id
    )
}

fn kind_time_author_key_from_author_kind_time_key(
    key: &str,
) -> Result<String, NostrEventStoreError> {
    let mut parts = key.split(':');
    let Some(pubkey) = parts.next() else {
        return Err(NostrEventStoreError::Validation(format!(
            "invalid author-kind-time key: {key}"
        )));
    };
    let Some(kind) = parts.next() else {
        return Err(NostrEventStoreError::Validation(format!(
            "invalid author-kind-time key: {key}"
        )));
    };
    let Some(reversed_timestamp) = parts.next() else {
        return Err(NostrEventStoreError::Validation(format!(
            "invalid author-kind-time key: {key}"
        )));
    };
    let Some(event_id) = parts.next() else {
        return Err(NostrEventStoreError::Validation(format!(
            "invalid author-kind-time key: {key}"
        )));
    };
    if parts.next().is_some() {
        return Err(NostrEventStoreError::Validation(format!(
            "invalid author-kind-time key: {key}"
        )));
    }

    Ok(format!(
        "{}:{}:{}:{}",
        kind, reversed_timestamp, pubkey, event_id
    ))
}

fn time_key(event: &StoredNostrEvent) -> String {
    format!("{}:{}", reverse_timestamp(event.created_at), event.id)
}

fn created_at_from_index_key(key: &str) -> Result<u64, NostrEventStoreError> {
    let mut parts = key.rsplitn(3, ':');
    let _event_id = parts.next();
    let Some(reversed) = parts.next() else {
        return Err(NostrEventStoreError::Validation(format!(
            "invalid nostr index key: {key}"
        )));
    };
    let reversed = u64::from_str_radix(reversed, 16).map_err(|err| {
        NostrEventStoreError::Validation(format!(
            "invalid reversed timestamp in nostr index key {key}: {err}"
        ))
    })?;
    Ok(u64::MAX - reversed)
}

fn tag_keys(event: &StoredNostrEvent) -> Vec<String> {
    event
        .tags
        .iter()
        .filter_map(|tag| match tag.as_slice() {
            [name, value, ..] if !name.is_empty() && !value.is_empty() => {
                let normalized_name = name.to_lowercase();
                let normalized_value = normalize_tag_value(&normalized_name, value);
                Some(format!(
                    "{}:{}:{}:{}",
                    normalized_name,
                    normalized_value,
                    reverse_timestamp(event.created_at),
                    event.id
                ))
            }
            _ => None,
        })
        .collect()
}

fn tag_prefix(tag_name: &str, tag_value: &str) -> Result<String, NostrEventStoreError> {
    let normalized_name = normalize_tag_name(tag_name)?;
    let normalized_value = normalize_tag_value(&normalized_name, tag_value);
    Ok(format!("{normalized_name}:{normalized_value}:"))
}

fn normalize_tag_name(tag_name: &str) -> Result<String, NostrEventStoreError> {
    if tag_name.is_empty() {
        return Err(NostrEventStoreError::Validation(
            "tag name must be non-empty".to_string(),
        ));
    }
    Ok(tag_name.to_lowercase())
}

fn normalize_tag_value(tag_name: &str, tag_value: &str) -> String {
    if tag_name == "t" {
        tag_value.to_lowercase()
    } else {
        tag_value.to_string()
    }
}

fn replaceable_key(pubkey: &str, kind: u32) -> String {
    format!("{}:{}", pubkey, pad_kind(kind))
}

fn parameterized_replaceable_key(pubkey: &str, kind: u32, d_tag: &str) -> String {
    format!("{}:{}:{}", pubkey, pad_kind(kind), d_tag)
}

pub fn is_replaceable_kind(kind: u32) -> bool {
    kind == 0 || kind == 3 || kind == 41 || (10_000..20_000).contains(&kind)
}

pub fn is_parameterized_replaceable_kind(kind: u32) -> bool {
    (30_000..40_000).contains(&kind)
}

fn get_d_tag(event: &StoredNostrEvent) -> Option<String> {
    event.tags.iter().find_map(|tag| match tag.as_slice() {
        [name, value, ..] if name == "d" && !value.is_empty() => Some(value.clone()),
        _ => None,
    })
}

fn parameterized_replaceable_d_tag(event: &StoredNostrEvent) -> String {
    get_d_tag(event).unwrap_or_default()
}

fn compare_events(left: &StoredNostrEvent, right: &StoredNostrEvent) -> i8 {
    match left.created_at.cmp(&right.created_at) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Greater => 1,
        std::cmp::Ordering::Equal => match left.id.cmp(&right.id) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Greater => 1,
            std::cmp::Ordering::Equal => 0,
        },
    }
}

fn retain_latest_replaceable_events(events: Vec<StoredNostrEvent>) -> Vec<StoredNostrEvent> {
    let mut winners = BTreeMap::<(ReplaceableSlot, String), StoredNostrEvent>::new();
    let mut plain = Vec::new();

    for event in events {
        let slot = if is_replaceable_kind(event.kind) {
            Some((
                ReplaceableSlot::Replaceable,
                replaceable_key(&event.pubkey, event.kind),
            ))
        } else if is_parameterized_replaceable_kind(event.kind) {
            Some((
                ReplaceableSlot::Parameterized,
                parameterized_replaceable_key(
                    &event.pubkey,
                    event.kind,
                    &parameterized_replaceable_d_tag(&event),
                ),
            ))
        } else {
            None
        };

        if let Some(slot) = slot {
            match winners.get(&slot) {
                Some(current) if compare_events(&event, current) <= 0 => {}
                _ => {
                    winners.insert(slot, event);
                }
            }
        } else {
            plain.push(event);
        }
    }

    plain.extend(winners.into_values());
    plain
}
