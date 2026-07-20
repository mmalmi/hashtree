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

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use std::cell::RefCell;

use futures::{stream, StreamExt, TryStreamExt};
use hashtree_collection::{
    load_collection_manifest_metadata, load_collection_state, CollectionDefinition,
    CollectionOptions, CollectionPublishedSchema, CollectionSource, CollectionState,
    CollectionWriter,
};
use hashtree_core::{
    sha256, BufferedStore, Cid, HashTree, HashTreeConfig, HashTreeError, Store, TreeVisibility,
};
use hashtree_index::{BTree, BTreeError, BTreeOptions};
use nostr_sdk::nips::nip44::{self, Version as Nip44Version};
use nostr_sdk::{
    Alphabet, Event, EventBuilder, Filter as NostrFilter, Keys, Kind, PublicKey, SingleLetterTag,
    Tag, TagKind,
};
use serde::{Deserialize, Serialize};

pub const NOSTR_EVENT_ENVELOPE_VERSION: u8 = 1;
pub const HASHTREE_ROOT_KIND: u32 = 30064;
pub const HASHTREE_LEGACY_ROOT_KIND: u32 = 30078;
pub const HASHTREE_LABEL: &str = "hashtree";
pub const TAG_HASH: &str = "hash";
pub const TAG_KEY: &str = "key";
pub const TAG_ENCRYPTED_KEY: &str = "encryptedKey";
pub const TAG_KEY_ID: &str = "keyId";
pub const TAG_SELF_ENCRYPTED_KEY: &str = "selfEncryptedKey";
pub const TAG_SELF_ENCRYPTED_LINK_KEY: &str = "selfEncryptedLinkKey";
const MANIFEST_BY_AUTHOR_TIME: &str = "by-author-time";
const MANIFEST_BY_AUTHOR_KIND_TIME: &str = "by-author-kind-time";
const MANIFEST_BY_KIND_TIME: &str = "by-kind-time";
const MANIFEST_BY_KIND_TIME_AUTHOR: &str = "by-kind-time-author";
const MANIFEST_BY_TIME: &str = "by-time";
const MANIFEST_BY_TAG: &str = "by-tag";
const MANIFEST_REPLACEABLE: &str = "replaceable";
const MANIFEST_PARAMETERIZED_REPLACEABLE: &str = "parameterized-replaceable";
const EVENT_BLOB_WRITE_CONCURRENCY: usize = 64;
const NOSTR_EVENT_ITEM_FORMAT: &str = "nostr/event@1";
const NOSTR_EVENT_PROJECTION_FORMAT: &str = "hashtree/nostr-event-index@1";
const MAX_SNAPSHOT_BYTES: usize = 256 * 1024;

pub fn is_hashtree_root_kind(kind: u32) -> bool {
    kind == HASHTREE_ROOT_KIND || kind == HASHTREE_LEGACY_ROOT_KIND
}

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

impl StoredNostrEvent {
    pub fn to_nostr_sdk_event(&self) -> Result<Event, NostrEventStoreError> {
        let bytes = serde_json::to_vec(self)?;
        Ok(serde_json::from_slice(&bytes)?)
    }
}

/// A Nostr SDK event whose id and Schnorr signature have been checked locally.
#[derive(Debug, Clone)]
pub struct VerifiedEvent(Event);

impl VerifiedEvent {
    pub fn as_event(&self) -> &Event {
        &self.0
    }

    pub fn into_event(self) -> Event {
        self.0
    }

    pub fn to_stored_event(&self) -> VerifiedStoredNostrEvent {
        VerifiedStoredNostrEvent {
            event: stored_event_from_nostr_sdk_event(&self.0),
        }
    }
}

impl TryFrom<Event> for VerifiedEvent {
    type Error = NostrEventStoreError;

    fn try_from(event: Event) -> Result<Self, Self::Error> {
        verify_nostr_sdk_event(&event)?;
        Ok(Self(event))
    }
}

impl AsRef<Event> for VerifiedEvent {
    fn as_ref(&self) -> &Event {
        self.as_event()
    }
}

/// A stored Nostr event whose serialized event has been locally verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedStoredNostrEvent {
    event: StoredNostrEvent,
}

impl VerifiedStoredNostrEvent {
    pub fn as_stored(&self) -> &StoredNostrEvent {
        &self.event
    }

    pub fn into_stored(self) -> StoredNostrEvent {
        self.event
    }

    pub fn to_nostr_sdk_event(&self) -> Result<VerifiedEvent, NostrEventStoreError> {
        VerifiedEvent::try_from(self.event.to_nostr_sdk_event()?)
    }
}

impl TryFrom<StoredNostrEvent> for VerifiedStoredNostrEvent {
    type Error = NostrEventStoreError;

    fn try_from(event: StoredNostrEvent) -> Result<Self, Self::Error> {
        let event = normalize_signed_event(event)?;
        let sdk_event = event.to_nostr_sdk_event()?;
        verify_nostr_sdk_event(&sdk_event)?;
        Ok(Self { event })
    }
}

impl AsRef<StoredNostrEvent> for VerifiedStoredNostrEvent {
    fn as_ref(&self) -> &StoredNostrEvent {
        self.as_stored()
    }
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

/// Result of an incremental event-index build.
///
/// Superseded nodes remain readable until the caller deletes them. The caller
/// must first durably publish `root`; deleting them earlier would make crash
/// recovery unsafe.
#[derive(Debug, Clone, Default)]
pub struct NostrEventBuildReport {
    pub root: Option<Cid>,
    pub superseded_nodes: Vec<Cid>,
}

#[derive(Debug, Clone)]
pub struct NostrReplaceableRepairReport {
    pub root: Cid,
    pub entries_scanned: u64,
    pub replaceable_entries: usize,
    pub parameterized_replaceable_entries: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MissingNostrEventLink {
    pub key: String,
    pub cid: Cid,
}

struct ReplaceableSourceScan {
    entries_scanned: u64,
    replaceable: Vec<(String, Cid)>,
    parameterized_replaceable: BTreeMap<String, Cid>,
    missing_parameterized: Vec<MissingNostrEventLink>,
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

struct PreparedEventBlob {
    sequence: usize,
    event: StoredNostrEvent,
    bytes: Vec<u8>,
    previous: Option<StoredNostrEvent>,
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

pub fn stored_event_from_nostr_sdk_event(event: &Event) -> StoredNostrEvent {
    StoredNostrEvent {
        id: event.id.to_hex(),
        pubkey: event.pubkey.to_hex(),
        created_at: event.created_at.as_secs(),
        kind: u32::from(event.kind.as_u16()),
        tags: event
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect(),
        content: event.content.clone(),
        sig: event.sig.to_string(),
    }
}

fn verify_nostr_sdk_event(event: &Event) -> Result<(), NostrEventStoreError> {
    event.verify().map_err(|err| {
        NostrEventStoreError::Validation(format!("signature verification failed: {err}"))
    })
}

pub fn parse_hashtree_root_event(
    event: &StoredNostrEvent,
) -> Result<Option<ParsedHashtreeRootEvent>, NostrEventStoreError> {
    let normalized = normalize_signed_event(event.clone())?;
    if !is_hashtree_root_kind(normalized.kind) {
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
        .find(|tag| tag.first().map(String::as_str) == Some(TAG_HASH))
        .and_then(|tag| tag.get(1))
        .cloned();
    let (Some(tree_name), Some(hash_hex)) = (tree_name, hash_hex) else {
        return Ok(None);
    };

    if has_any_label(&normalized) && !has_label(&normalized, HASHTREE_LABEL) {
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
        .find(|tag| tag.first().map(String::as_str) == Some(TAG_KEY))
        .and_then(|tag| tag.get(1))
        .cloned();
    let encrypted_key = normalized
        .tags
        .iter()
        .find(|tag| tag.first().map(String::as_str) == Some(TAG_ENCRYPTED_KEY))
        .and_then(|tag| tag.get(1))
        .cloned();
    let key_id = normalized
        .tags
        .iter()
        .find(|tag| tag.first().map(String::as_str) == Some(TAG_KEY_ID))
        .and_then(|tag| tag.get(1))
        .cloned();
    let self_encrypted_key = normalized
        .tags
        .iter()
        .find(|tag| tag.first().map(String::as_str) == Some(TAG_SELF_ENCRYPTED_KEY))
        .and_then(|tag| tag.get(1))
        .cloned();
    let self_encrypted_link_key = normalized
        .tags
        .iter()
        .find(|tag| tag.first().map(String::as_str) == Some(TAG_SELF_ENCRYPTED_LINK_KEY))
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

pub fn parse_verified_hashtree_root_event(
    event: &Event,
) -> Result<Option<ParsedHashtreeRootEvent>, NostrEventStoreError> {
    let verified = VerifiedEvent::try_from(event.clone())?;
    parse_hashtree_root_event(verified.to_stored_event().as_stored())
}

pub fn resolve_self_encrypted_root_cid(
    parsed: &ParsedHashtreeRootEvent,
    owner_keys: &Keys,
) -> Result<Cid, NostrEventStoreError> {
    if parsed.root_cid.key.is_some() {
        return Ok(parsed.root_cid.clone());
    }

    let ciphertext = parsed.self_encrypted_key.as_ref().ok_or_else(|| {
        NostrEventStoreError::Validation("hashtree root key is unavailable".to_string())
    })?;
    let author = PublicKey::from_hex(&parsed.event.pubkey).map_err(|err| {
        NostrEventStoreError::Validation(format!("invalid root event pubkey: {err}"))
    })?;
    if owner_keys.public_key() != author {
        return Err(NostrEventStoreError::Validation(format!(
            "owner key {} does not match root event author {}",
            owner_keys.public_key().to_hex(),
            parsed.event.pubkey
        )));
    }
    let key_hex = nip44::decrypt(owner_keys.secret_key(), &author, ciphertext).map_err(|_| {
        NostrEventStoreError::Validation("hashtree root key is unavailable".to_string())
    })?;
    let key = hashtree_core::from_hex(&key_hex).map_err(|_| {
        NostrEventStoreError::Validation(
            "root key must be a lowercase 64-character hex string".to_string(),
        )
    })?;
    Ok(Cid {
        hash: parsed.root_cid.hash,
        key: Some(key),
    })
}

pub fn build_private_hashtree_root_event(
    owner_keys: &Keys,
    tree_name: &str,
    root_cid: &Cid,
    created_at: Option<u64>,
) -> Result<Event, NostrEventStoreError> {
    let Some(root_key) = root_cid.key else {
        return Err(NostrEventStoreError::Validation(
            "private hashtree root requires an encrypted CID".to_string(),
        ));
    };
    let root_key_hex = hex::encode(root_key);
    let self_encrypted_key = nip44::encrypt(
        owner_keys.secret_key(),
        &owner_keys.public_key(),
        root_key_hex,
        Nip44Version::V2,
    )
    .map_err(|err| {
        NostrEventStoreError::Validation(format!("self-encrypted root key failed: {err}"))
    })?;
    let created_at_ms = created_at
        .and_then(|created_at| created_at.checked_mul(1000))
        .unwrap_or_else(unix_now_millis);
    let builder = EventBuilder::new(Kind::from(HASHTREE_ROOT_KIND as u16), "").tags([
        Tag::identifier(tree_name.to_string()),
        Tag::custom(
            TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::L)),
            vec![HASHTREE_LABEL],
        ),
        Tag::custom(
            TagKind::Custom(TAG_HASH.into()),
            vec![hex::encode(root_cid.hash)],
        ),
        Tag::custom(
            TagKind::Custom(TAG_SELF_ENCRYPTED_KEY.into()),
            vec![self_encrypted_key],
        ),
        Tag::custom(
            TagKind::Custom("ms".into()),
            vec![created_at_ms.to_string()],
        ),
    ]);
    let builder = if let Some(created_at) = created_at {
        builder.custom_created_at(nostr_sdk::Timestamp::from(created_at))
    } else {
        builder
    };
    builder.sign_with_keys(owner_keys).map_err(|err| {
        NostrEventStoreError::Validation(format!("build hashtree root event failed: {err}"))
    })
}

fn unix_now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

const DEFAULT_INDEX_COMMIT_BATCH_SIZE: usize = 8_192;

#[derive(Debug, Clone)]
pub struct NostrEventStoreOptions {
    pub btree_order: Option<usize>,
    /// Maximum number of events applied in one collection update. Event blobs
    /// and each independent index projection are flushed within the update so
    /// memory is bounded without repeatedly walking every index.
    /// None preserves the legacy single-commit behavior.
    pub index_commit_batch_size: Option<usize>,
}

impl Default for NostrEventStoreOptions {
    fn default() -> Self {
        Self {
            btree_order: None,
            index_commit_batch_size: Some(DEFAULT_INDEX_COMMIT_BATCH_SIZE),
        }
    }
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
        .with_key_index(MANIFEST_BY_TAG, tag_keys)
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

    pub fn decode_verified_event(
        &self,
        data: &[u8],
    ) -> Result<VerifiedStoredNostrEvent, NostrEventStoreError> {
        VerifiedStoredNostrEvent::try_from(self.decode_event(data)?)
    }

    /// Persist signed events as independent content-addressed Hashtree blobs
    /// without mutating any query index.
    pub async fn store_event_blobs<I>(&self, events: I) -> Result<Vec<Cid>, NostrEventStoreError>
    where
        I: IntoIterator<Item = StoredNostrEvent>,
    {
        let mut prepared = Vec::new();
        for (sequence, event) in events.into_iter().enumerate() {
            let normalized = self.validate_event(event).await?;
            prepared.push(self.prepare_event_blob(sequence, normalized, None)?);
        }
        Ok(self
            .put_prepared_event_blobs_parallel(prepared)
            .await?
            .into_iter()
            .map(|(_sequence, _event, cid, _previous)| cid)
            .collect())
    }

    /// Read one independently persisted event blob.
    pub async fn load_event_blob(
        &self,
        cid: &Cid,
    ) -> Result<StoredNostrEvent, NostrEventStoreError> {
        self.read_stored_event(cid).await
    }

    /// Read independent event blobs concurrently while preserving input order.
    pub async fn load_event_blobs<I>(
        &self,
        cids: I,
    ) -> Result<Vec<StoredNostrEvent>, NostrEventStoreError>
    where
        I: IntoIterator<Item = Cid>,
    {
        let mut events = stream::iter(cids.into_iter().enumerate().map(
            |(sequence, cid)| async move {
                let cid_text = cid.to_string();
                self.read_stored_event(&cid)
                    .await
                    .map(|event| (sequence, event))
                    .map_err(|err| {
                        NostrEventStoreError::Validation(format!(
                            "load event blob {cid_text} at sequence {sequence}: {err}"
                        ))
                    })
            },
        ))
        .buffer_unordered(EVENT_BLOB_WRITE_CONCURRENCY)
        .try_collect::<Vec<_>>()
        .await?;
        events.sort_by_key(|(sequence, _)| *sequence);
        Ok(events.into_iter().map(|(_, event)| event).collect())
    }

    pub async fn validate_index_root(
        &self,
        root: Option<&Cid>,
    ) -> Result<(), NostrEventStoreError> {
        let Some(root) = root else {
            return Ok(());
        };

        let manifest = self.get_manifest(Some(root)).await?;
        let mut missing = Vec::new();
        if manifest.by_id.is_none() {
            missing.push("by-id");
        }
        if manifest.by_author_time.is_none() {
            missing.push(MANIFEST_BY_AUTHOR_TIME);
        }
        if manifest.by_author_kind_time.is_none() {
            missing.push(MANIFEST_BY_AUTHOR_KIND_TIME);
        }
        if manifest.by_kind_time.is_none() {
            missing.push(MANIFEST_BY_KIND_TIME);
        }
        if manifest.by_time.is_none() {
            missing.push(MANIFEST_BY_TIME);
        }
        if !missing.is_empty() {
            return Err(NostrEventStoreError::Validation(format!(
                "nostr event index root missing required manifest entries: {}",
                missing.join(", ")
            )));
        }

        if let Some(metadata) =
            load_collection_manifest_metadata(Arc::clone(&self.store), Some(root)).await?
        {
            let schema = metadata.published_schema();
            if let Some(item_format) = schema.and_then(|schema| schema.item_format()) {
                if item_format != NOSTR_EVENT_ITEM_FORMAT {
                    return Err(NostrEventStoreError::Validation(format!(
                        "nostr event index item format mismatch: expected {NOSTR_EVENT_ITEM_FORMAT}, got {item_format}"
                    )));
                }
            }
            if let Some(projection_format) = schema.and_then(|schema| schema.projection_format()) {
                if projection_format != NOSTR_EVENT_PROJECTION_FORMAT {
                    return Err(NostrEventStoreError::Validation(format!(
                        "nostr event index projection format mismatch: expected {NOSTR_EVENT_PROJECTION_FORMAT}, got {projection_format}"
                    )));
                }
            }
        }

        Ok(())
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
        Ok(self.build_with_superseded_nodes(root, events).await?.root)
    }

    /// Reclaim nodes returned by [`NostrEventStore::build_with_superseded_nodes`].
    /// The new root must already be durably published before calling this.
    pub async fn delete_superseded_nodes(
        &self,
        nodes: &[Cid],
    ) -> Result<usize, NostrEventStoreError> {
        const DELETE_BATCH_SIZE: usize = 16_384;
        let mut deleted = 0usize;
        for batch in nodes.chunks(DELETE_BATCH_SIZE) {
            deleted = deleted.saturating_add(
                self.store
                    .delete_many(batch.iter().map(|cid| cid.hash).collect())
                    .await
                    .map_err(|err| {
                        NostrEventStoreError::HashTree(HashTreeError::Store(err.to_string()))
                    })?,
            );
        }
        Ok(deleted)
    }

    /// Build event indexes while retaining the exact old copy-on-write nodes
    /// that the new root supersedes.
    pub async fn build_with_superseded_nodes<I>(
        &self,
        root: Option<&Cid>,
        events: I,
    ) -> Result<NostrEventBuildReport, NostrEventStoreError>
    where
        I: IntoIterator<Item = StoredNostrEvent>,
    {
        let mut events: Vec<StoredNostrEvent> = events.into_iter().collect();
        if events.is_empty() {
            return Ok(NostrEventBuildReport {
                root: root.cloned(),
                superseded_nodes: Vec::new(),
            });
        }
        events = retain_unique_latest_events(events);
        events.sort_by(|left, right| match compare_events(left, right) {
            x if x < 0 => std::cmp::Ordering::Less,
            x if x > 0 => std::cmp::Ordering::Greater,
            _ => std::cmp::Ordering::Equal,
        });

        let batch_size = self
            .options
            .index_commit_batch_size
            .unwrap_or(events.len())
            .max(1);
        let mut current_root = root.cloned();
        let mut superseded_nodes = Vec::new();
        let mut pending = events.into_iter();
        loop {
            let batch = pending.by_ref().take(batch_size).collect::<Vec<_>>();
            if batch.is_empty() {
                break;
            }
            let report = self
                .build_index_commit(current_root.as_ref(), batch)
                .await?;
            current_root = report.root;
            superseded_nodes.extend(report.superseded_nodes);
            // Each commit drops its large per-index change maps before
            // returning. glibc can otherwise retain those freed arenas across
            // the remaining commits in a large author, defeating the bounded
            // commit size and eventually exhausting a crawler cgroup.
            trim_index_commit_allocations();
        }
        let mut seen_superseded = HashSet::new();
        superseded_nodes.retain(|candidate| seen_superseded.insert(candidate.hash));
        if let Some(root) = current_root.as_ref() {
            superseded_nodes.retain(|candidate| candidate != root);
        }
        Ok(NostrEventBuildReport {
            root: current_root,
            superseded_nodes,
        })
    }

    /// Restore event blobs that are referenced by an existing collection but
    /// missing from physical storage. The recovered logical event is supplied
    /// as both the previous and replacement item so every derived index is
    /// updated through the collection's explicit replace path.
    ///
    /// This is a closed-writer repair operation. The returned root must be
    /// force-synced and published durably before deleting superseded nodes.
    pub async fn repair_missing_event_blobs_with_superseded_nodes<I>(
        &self,
        root: &Cid,
        events: I,
    ) -> Result<NostrEventBuildReport, NostrEventStoreError>
    where
        I: IntoIterator<Item = StoredNostrEvent>,
    {
        let buffered_store = Arc::new(BufferedStore::new_optimistic(Arc::clone(&self.store)));
        let buffered_writer =
            NostrEventStore::with_options(Arc::clone(&buffered_store), self.options.clone());
        let mut manifest = buffered_writer.get_manifest(Some(root)).await?;
        let mut prepared_events = Vec::new();
        for (sequence, event) in events.into_iter().enumerate() {
            let normalized = buffered_writer.validate_event(event).await?;
            prepared_events.push(buffered_writer.prepare_event_blob(
                sequence,
                normalized.clone(),
                Some(normalized),
            )?);
        }
        if prepared_events.is_empty() {
            return Ok(NostrEventBuildReport {
                root: Some(root.clone()),
                superseded_nodes: Vec::new(),
            });
        }

        let indexed_events = buffered_writer
            .put_prepared_event_blobs_parallel(prepared_events)
            .await?;
        buffered_store.flush().await.map_err(|error| {
            NostrEventStoreError::HashTree(HashTreeError::Store(error.to_string()))
        })?;
        let mut collection = buffered_writer.collection_writer_from_manifest(&manifest);
        collection
            .put_batch(
                indexed_events
                    .into_iter()
                    .map(|(_sequence, event, event_cid, previous)| (event, event_cid, previous)),
            )
            .await?;
        let mut superseded_nodes = collection.take_superseded_index_nodes();
        manifest = nostr_manifest_from_collection_state(collection.state());
        let next_root = buffered_writer.write_manifest(&manifest).await?;
        buffered_store.flush().await.map_err(|error| {
            NostrEventStoreError::HashTree(HashTreeError::Store(error.to_string()))
        })?;
        if next_root.as_ref().is_some_and(|next| next != root) {
            superseded_nodes.push(root.clone());
        }
        let mut seen_superseded = HashSet::new();
        superseded_nodes.retain(|candidate| seen_superseded.insert(candidate.hash));
        if let Some(next_root) = next_root.as_ref() {
            superseded_nodes.retain(|candidate| candidate != next_root);
        }
        Ok(NostrEventBuildReport {
            root: next_root,
            superseded_nodes,
        })
    }

    async fn build_index_commit(
        &self,
        root: Option<&Cid>,
        events: Vec<StoredNostrEvent>,
    ) -> Result<NostrEventBuildReport, NostrEventStoreError> {
        let buffered_store = Arc::new(BufferedStore::new_optimistic(Arc::clone(&self.store)));
        let buffered_writer =
            NostrEventStore::with_options(Arc::clone(&buffered_store), self.options.clone());
        let mut obsolete_event_cids = Vec::new();
        let mut superseded_nodes = Vec::new();
        let next_root = if root.is_none() {
            buffered_writer.build_manifest_from_events(events).await?
        } else {
            let mut manifest = buffered_writer.get_manifest(root).await?;
            let mut normalized_events = Vec::with_capacity(events.len());
            for (sequence, event) in events.into_iter().enumerate() {
                let normalized = buffered_writer.validate_event(event).await?;
                normalized_events.push((sequence, normalized));
            }

            let existing_by_id = buffered_writer
                .index
                .get_links(
                    manifest.by_id.as_ref(),
                    normalized_events
                        .iter()
                        .filter(|(_, event)| {
                            !is_replaceable_kind(event.kind)
                                && !is_parameterized_replaceable_kind(event.kind)
                        })
                        .map(|(_, event)| event.id.clone()),
                )
                .await?;
            let existing_replaceable = buffered_writer
                .index
                .get_links(
                    manifest.replaceable.as_ref(),
                    normalized_events
                        .iter()
                        .filter(|(_, event)| is_replaceable_kind(event.kind))
                        .map(|(_, event)| replaceable_key(&event.pubkey, event.kind)),
                )
                .await?;
            let existing_parameterized = buffered_writer
                .index
                .get_links(
                    manifest.parameterized_replaceable.as_ref(),
                    normalized_events
                        .iter()
                        .filter(|(_, event)| is_parameterized_replaceable_kind(event.kind))
                        .map(|(_, event)| {
                            parameterized_replaceable_key(
                                &event.pubkey,
                                event.kind,
                                &parameterized_replaceable_d_tag(event),
                            )
                        }),
                )
                .await?;
            let known_absent_by_id = normalized_events
                .iter()
                .filter(|(_, event)| {
                    !is_replaceable_kind(event.kind)
                        && !is_parameterized_replaceable_kind(event.kind)
                        && !existing_by_id.contains_key(&event.id)
                })
                .map(|(_, event)| event.id.clone())
                .collect::<BTreeSet<_>>();

            let mut prepared_events = Vec::with_capacity(normalized_events.len());
            for (sequence, normalized) in normalized_events {
                let existing_cid = if is_replaceable_kind(normalized.kind) {
                    existing_replaceable.get(&replaceable_key(&normalized.pubkey, normalized.kind))
                } else if is_parameterized_replaceable_kind(normalized.kind) {
                    existing_parameterized.get(&parameterized_replaceable_key(
                        &normalized.pubkey,
                        normalized.kind,
                        &parameterized_replaceable_d_tag(&normalized),
                    ))
                } else {
                    existing_by_id.get(&normalized.id)
                };

                let previous = match existing_cid {
                    Some(existing_cid) => {
                        match buffered_writer.read_stored_event(existing_cid).await {
                            Ok(existing)
                                if is_replaceable_kind(normalized.kind)
                                    || is_parameterized_replaceable_kind(normalized.kind) =>
                            {
                                if compare_replaceable_events(&normalized, &existing) <= 0 {
                                    continue;
                                }
                                obsolete_event_cids.push(existing_cid.clone());
                                Some(existing)
                            }
                            Ok(_) => continue,
                            Err(err) if is_missing_stored_event_error(&err) => {
                                (!is_replaceable_kind(normalized.kind)
                                    && !is_parameterized_replaceable_kind(normalized.kind))
                                .then(|| normalized.clone())
                            }
                            Err(err) => return Err(err),
                        }
                    }
                    None => None,
                };
                prepared_events
                    .push(buffered_writer.prepare_event_blob(sequence, normalized, previous)?);
            }

            let indexed_events = buffered_writer
                .put_prepared_event_blobs_parallel(prepared_events)
                .await?;
            buffered_store.flush().await.map_err(|err| {
                NostrEventStoreError::HashTree(HashTreeError::Store(err.to_string()))
            })?;
            if !indexed_events.is_empty() {
                let mut collection = buffered_writer.collection_writer_from_manifest(&manifest);
                collection
                    .put_batch_with_known_absent_ids(
                        indexed_events.into_iter().map(
                            |(_sequence, event, event_cid, previous)| (event, event_cid, previous),
                        ),
                        known_absent_by_id,
                    )
                    .await?;
                superseded_nodes.extend(collection.take_superseded_index_nodes());
                manifest = nostr_manifest_from_collection_state(collection.state());
            }
            buffered_writer.write_manifest(&manifest).await?
        };
        buffered_store
            .flush()
            .await
            .map_err(|err| NostrEventStoreError::HashTree(HashTreeError::Store(err.to_string())))?;
        superseded_nodes.extend(obsolete_event_cids);
        if let (Some(old_root), Some(new_root)) = (root, next_root.as_ref()) {
            if old_root != new_root {
                superseded_nodes.push(old_root.clone());
            }
        }
        let mut seen_superseded = HashSet::new();
        superseded_nodes.retain(|candidate| seen_superseded.insert(candidate.hash));
        if let Some(root) = next_root.as_ref() {
            superseded_nodes.retain(|candidate| candidate != root);
        }
        Ok(NostrEventBuildReport {
            root: next_root,
            superseded_nodes,
        })
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

    async fn scan_replaceable_sources(
        &self,
        root: &Cid,
        page_size: usize,
    ) -> Result<ReplaceableSourceScan, NostrEventStoreError> {
        if page_size == 0 {
            return Err(NostrEventStoreError::Validation(
                "replaceable repair page size must be non-zero".to_string(),
            ));
        }
        let manifest = self.get_manifest(Some(root)).await?;
        let author_kind_time = manifest.by_author_kind_time.as_ref().ok_or_else(|| {
            NostrEventStoreError::Validation(
                "Nostr manifest has no by-author-kind-time index".to_string(),
            )
        })?;

        let mut start = None;
        let mut entries_scanned = 0u64;
        let mut last_slot = None;
        let mut replaceable = Vec::new();
        let mut parameterized_replaceable = BTreeMap::new();
        let mut missing_parameterized = Vec::new();
        loop {
            let page = self
                .index
                .range_links_limited(author_kind_time, start.as_deref(), None, page_size)
                .await?;
            if page.is_empty() {
                break;
            }
            entries_scanned = entries_scanned.saturating_add(page.len() as u64);
            for (key, cid) in &page {
                let mut parts = key.split(':');
                let pubkey = parts.next().unwrap_or_default();
                let kind_hex = parts.next().unwrap_or_default();
                let kind = u32::from_str_radix(kind_hex, 16).map_err(|error| {
                    NostrEventStoreError::Validation(format!(
                        "invalid kind in author-kind-time key {key}: {error}"
                    ))
                })?;
                if pubkey.len() != 64 {
                    continue;
                }
                if is_replaceable_kind(kind) {
                    let slot = replaceable_key(pubkey, kind);
                    if last_slot.as_ref() == Some(&slot) {
                        continue;
                    }
                    last_slot = Some(slot.clone());
                    replaceable.push((slot, cid.clone()));
                } else if is_parameterized_replaceable_kind(kind) {
                    let event = match self.read_stored_event(cid).await {
                        Ok(event) => event,
                        Err(error) if is_missing_stored_event_error(&error) => {
                            missing_parameterized.push(MissingNostrEventLink {
                                key: key.clone(),
                                cid: cid.clone(),
                            });
                            continue;
                        }
                        Err(error) => return Err(error),
                    };
                    let slot = parameterized_replaceable_key(
                        pubkey,
                        kind,
                        &parameterized_replaceable_d_tag(&event),
                    );
                    parameterized_replaceable
                        .entry(slot)
                        .or_insert_with(|| cid.clone());
                }
            }

            let last_key = page.last().map(|(key, _)| key).expect("non-empty page");
            start = Some(format!("{last_key}\0"));
            if page.len() < page_size {
                break;
            }
        }

        Ok(ReplaceableSourceScan {
            entries_scanned,
            replaceable,
            parameterized_replaceable,
            missing_parameterized,
        })
    }

    /// Find parameterized replaceable events whose current index value blob is
    /// missing. The returned author/kind/time key includes the event id.
    pub async fn missing_parameterized_event_links(
        &self,
        root: &Cid,
        page_size: usize,
    ) -> Result<Vec<MissingNostrEventLink>, NostrEventStoreError> {
        Ok(self
            .scan_replaceable_sources(root, page_size)
            .await?
            .missing_parameterized)
    }

    /// Write a temporary manifest that omits both derived replaceable indexes.
    /// Other authoritative indexes remain unchanged. This lets a closed-writer
    /// repair reinsert missing event blobs before rebuilding both catalogs.
    pub async fn clear_replaceable_indexes(&self, root: &Cid) -> Result<Cid, NostrEventStoreError> {
        let mut manifest = self.get_manifest(Some(root)).await?;
        manifest.replaceable = None;
        manifest.parameterized_replaceable = None;
        self.write_manifest(&manifest).await?.ok_or_else(|| {
            NostrEventStoreError::Validation("temporary Nostr repair manifest was empty".into())
        })
    }

    /// Rebuild the regular and parameterized replaceable-event indexes from
    /// the authoritative author/kind/time index while leaving every other
    /// manifest index intact.
    ///
    /// This is a bounded-memory closed-writer repair path for damaged derived
    /// replaceable indexes. The caller must force-sync the returned root and
    /// publish it durably before deleting anything reachable only from the
    /// previous root.
    pub async fn rebuild_replaceable_index(
        &self,
        root: &Cid,
        page_size: usize,
    ) -> Result<NostrReplaceableRepairReport, NostrEventStoreError> {
        let mut manifest = self.get_manifest(Some(root)).await?;
        let scan = self.scan_replaceable_sources(root, page_size).await?;

        if let Some(missing) = scan.missing_parameterized.first() {
            return Err(NostrEventStoreError::Validation(format!(
                "{} parameterized event blobs are missing; first key={key} cid={cid}",
                scan.missing_parameterized.len(),
                key = missing.key,
                cid = missing.cid,
            )));
        }

        manifest.replaceable = self.index.build_links(scan.replaceable.clone()).await?;
        manifest.parameterized_replaceable = self
            .index
            .build_links(scan.parameterized_replaceable.clone())
            .await?;
        let repaired_root = self.write_manifest(&manifest).await?.ok_or_else(|| {
            NostrEventStoreError::Validation("repaired Nostr manifest was empty".to_string())
        })?;
        Ok(NostrReplaceableRepairReport {
            root: repaired_root,
            entries_scanned: scan.entries_scanned,
            replaceable_entries: scan.replaceable.len(),
            parameterized_replaceable_entries: scan.parameterized_replaceable.len(),
        })
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

    pub async fn get_verified_by_id(
        &self,
        root: Option<&Cid>,
        event_id: &str,
    ) -> Result<Option<VerifiedStoredNostrEvent>, NostrEventStoreError> {
        self.get_by_id(root, event_id)
            .await?
            .map(VerifiedStoredNostrEvent::try_from)
            .transpose()
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

    pub async fn list_by_author_lossy(
        &self,
        root: Option<&Cid>,
        pubkey: &str,
        options: ListEventsOptions,
    ) -> Result<Vec<StoredNostrEvent>, NostrEventStoreError> {
        let manifest = self.get_manifest(root).await?;
        let Some(by_author_time) = manifest.by_author_time.as_ref() else {
            return Ok(Vec::new());
        };
        self.collect_events_lossy(
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

    pub async fn list_by_author_and_kind_lossy(
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

        self.collect_events_lossy(
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

    pub async fn query_events(
        &self,
        root: Option<&Cid>,
        filter: &NostrFilter,
        limit: usize,
    ) -> Result<Vec<StoredNostrEvent>, NostrEventStoreError> {
        let limit = normalized_query_limit(filter, limit);
        if limit == 0 {
            return Ok(Vec::new());
        }

        let options = ListEventsOptions {
            limit: None,
            since: filter.since.map(|timestamp| timestamp.as_secs()),
            until: filter.until.map(|timestamp| timestamp.as_secs()),
        };
        let authors = filter
            .authors
            .as_ref()
            .filter(|authors| !authors.is_empty());
        let kinds = filter.kinds.as_ref().filter(|kinds| !kinds.is_empty());
        let mut candidates = Vec::new();

        if let Some(ids) = filter.ids.as_ref().filter(|ids| !ids.is_empty()) {
            for id in ids {
                if let Some(event) = self.get_by_id(root, &id.to_hex()).await? {
                    candidates.push(event);
                }
            }
        } else if let (Some(authors), Some(kinds)) = (authors, kinds) {
            for author in authors {
                let author = author.to_hex();
                for kind in kinds {
                    candidates.extend(
                        self.list_by_author_and_kind(
                            root,
                            &author,
                            u32::from(kind.as_u16()),
                            options.clone(),
                        )
                        .await?,
                    );
                }
            }
        } else if let Some((tag_name, tag_values)) = first_indexed_filter_tag(filter) {
            for tag_value in tag_values {
                candidates.extend(
                    self.list_by_tag(root, tag_name.as_str(), tag_value, options.clone())
                        .await?,
                );
            }
        } else if let Some(authors) = authors {
            for author in authors {
                candidates.extend(
                    self.list_by_author(root, &author.to_hex(), options.clone())
                        .await?,
                );
            }
        } else if let Some(kinds) = kinds {
            for kind in kinds {
                candidates.extend(
                    self.list_by_kind(root, u32::from(kind.as_u16()), options.clone())
                        .await?,
                );
            }
        } else {
            let recent_options = if filter.search.is_some() {
                options
            } else {
                ListEventsOptions {
                    limit: Some(limit),
                    ..options
                }
            };
            candidates = self.list_recent(root, recent_options).await?;
        }

        Ok(filter_query_candidates(filter, candidates, limit))
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
            match options.limit {
                Some(limit) => self.index.links_entries_limited(Some(root), limit).await?,
                None => self.index.links_entries(Some(root)).await?,
            }
        } else {
            match options.limit {
                Some(limit) => self.index.prefix_links_limited(root, prefix, limit).await?,
                None => self.index.prefix_links(root, prefix).await?,
            }
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
            match options.limit {
                Some(limit) => self.index.links_entries_limited(Some(root), limit).await?,
                None => self.index.links_entries(Some(root)).await?,
            }
        } else {
            match options.limit {
                Some(limit) => self.index.prefix_links_limited(root, prefix, limit).await?,
                None => self.index.prefix_links(root, prefix).await?,
            }
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

    fn prepare_event_blob(
        &self,
        sequence: usize,
        event: StoredNostrEvent,
        previous: Option<StoredNostrEvent>,
    ) -> Result<PreparedEventBlob, NostrEventStoreError> {
        let bytes = self.encode_validated_event(&event)?;
        Ok(PreparedEventBlob {
            sequence,
            event,
            bytes,
            previous,
        })
    }

    async fn put_prepared_event_blobs_parallel(
        &self,
        prepared_events: Vec<PreparedEventBlob>,
    ) -> Result<Vec<(usize, StoredNostrEvent, Cid, Option<StoredNostrEvent>)>, NostrEventStoreError>
    {
        let tree = &self.tree;
        let mut indexed_events =
            stream::iter(prepared_events.into_iter().map(|prepared| async move {
                let (event_cid, _size) = tree.put_file(&prepared.bytes).await?;
                Ok::<_, NostrEventStoreError>((
                    prepared.sequence,
                    prepared.event,
                    event_cid,
                    prepared.previous,
                ))
            }))
            .buffer_unordered(EVENT_BLOB_WRITE_CONCURRENCY)
            .try_collect::<Vec<_>>()
            .await?;

        indexed_events.sort_by_key(|(sequence, _, _, _)| *sequence);
        Ok(indexed_events)
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
        let mut prepared_events = Vec::with_capacity(events.len());

        for (sequence, event) in events.into_iter().enumerate() {
            let normalized = self.validate_event(event).await?;
            prepared_events.push(self.prepare_event_blob(sequence, normalized, None)?);
        }
        let indexed_events = self
            .put_prepared_event_blobs_parallel(prepared_events)
            .await?
            .into_iter()
            .map(|(_sequence, event, event_cid, _previous)| (event, event_cid))
            .collect::<Vec<_>>();

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
        if compare_replaceable_events(event, &existing) > 0 {
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

fn normalized_query_limit(filter: &NostrFilter, limit: usize) -> usize {
    filter
        .limit
        .map_or(limit, |filter_limit| filter_limit.min(limit))
}

fn first_indexed_filter_tag(filter: &NostrFilter) -> Option<(&SingleLetterTag, &BTreeSet<String>)> {
    filter
        .generic_tags
        .iter()
        .find(|(_, values)| !values.is_empty())
}

fn filter_query_candidates(
    filter: &NostrFilter,
    candidates: Vec<StoredNostrEvent>,
    limit: usize,
) -> Vec<StoredNostrEvent> {
    let mut seen = HashSet::new();
    let mut events = Vec::new();
    for event in candidates {
        if seen.insert(event.id.clone()) && stored_event_matches_filter(filter, &event) {
            events.push(event);
        }
    }
    events.sort_by(|left, right| {
        right
            .created_at
            .cmp(&left.created_at)
            .then_with(|| right.id.cmp(&left.id))
    });
    events.truncate(limit);
    events
}

fn stored_event_matches_filter(filter: &NostrFilter, event: &StoredNostrEvent) -> bool {
    filter_ids_match(filter, event)
        && filter_authors_match(filter, event)
        && filter_kinds_match(filter, event)
        && filter
            .since
            .is_none_or(|since| event.created_at >= since.as_secs())
        && filter
            .until
            .is_none_or(|until| event.created_at <= until.as_secs())
        && filter_tags_match(filter, event)
        && filter_search_matches(filter, event)
}

fn filter_ids_match(filter: &NostrFilter, event: &StoredNostrEvent) -> bool {
    filter_values_match(filter.ids.as_ref(), |id| id.to_hex() == event.id)
}

fn filter_authors_match(filter: &NostrFilter, event: &StoredNostrEvent) -> bool {
    filter_values_match(filter.authors.as_ref(), |author| {
        author.to_hex() == event.pubkey
    })
}

fn filter_kinds_match(filter: &NostrFilter, event: &StoredNostrEvent) -> bool {
    filter_values_match(filter.kinds.as_ref(), |kind| {
        u32::from(kind.as_u16()) == event.kind
    })
}

fn filter_values_match<T>(values: Option<&BTreeSet<T>>, matches: impl Fn(&T) -> bool) -> bool {
    values.is_none_or(|values| values.is_empty() || values.iter().any(matches))
}

fn filter_tags_match(filter: &NostrFilter, event: &StoredNostrEvent) -> bool {
    if filter.generic_tags.is_empty() {
        return true;
    }
    if event.tags.is_empty() {
        return false;
    }

    filter.generic_tags.iter().all(|(tag_name, values)| {
        !values.is_empty()
            && event.tags.iter().any(|tag| {
                tag.first().map(String::as_str) == Some(tag_name.as_str())
                    && tag.get(1).is_some_and(|value| values.contains(value))
            })
    })
}

fn filter_search_matches(filter: &NostrFilter, event: &StoredNostrEvent) -> bool {
    let Some(query) = filter.search.as_ref() else {
        return true;
    };
    let query = query.as_bytes();
    query.is_empty()
        || event
            .content
            .as_bytes()
            .windows(query.len())
            .any(|window| window.eq_ignore_ascii_case(query))
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

fn compare_replaceable_events(left: &StoredNostrEvent, right: &StoredNostrEvent) -> i8 {
    match left.created_at.cmp(&right.created_at) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Greater => 1,
        std::cmp::Ordering::Equal => match right.id.cmp(&left.id) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Greater => 1,
            std::cmp::Ordering::Equal => 0,
        },
    }
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn trim_index_commit_allocations() {
    // SAFETY: malloc_trim asks glibc to return free heap pages to the OS after
    // the commit-local values that owned them have been dropped.
    unsafe {
        libc::malloc_trim(0);
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn trim_index_commit_allocations() {}

fn retain_unique_latest_events(events: Vec<StoredNostrEvent>) -> Vec<StoredNostrEvent> {
    let mut winners = BTreeMap::<(ReplaceableSlot, String), StoredNostrEvent>::new();
    let mut seen_ids = HashSet::new();
    let mut plain = Vec::new();

    for event in events {
        if !seen_ids.insert(event.id.clone()) {
            continue;
        }
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
                Some(current) if compare_replaceable_events(&event, current) <= 0 => {}
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
