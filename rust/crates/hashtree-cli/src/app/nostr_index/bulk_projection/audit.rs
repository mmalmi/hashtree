use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use hashtree_core::{Cid, HashTree, HashTreeConfig, LinkType, Store};
use hashtree_index::{BTree, BTreeOptions};
use hashtree_lmdb::{ReadOnlyPoolStore, SHARED_BLOB_POOL_DIR_NAME};
use hashtree_nostr::{
    nostr_event_index_entries, ListEventsOptions, NostrEventIndex, NostrEventStore,
    StoredNostrEvent,
};
use heed::types::Bytes;
use heed::{Database, EnvFlags, EnvOpenOptions};
use nostr::{Event, JsonUtil};
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::super::{
    cid_to_nhash, parse_root_text, persist_json_atomic, StagedNostrCrawlState, STAGE_DIR,
    STAGE_FORMAT_VERSION, STAGE_STATE_FILE,
};
use super::{
    bulk_paths, encode_cid, validate_terminal_stage_state, BulkProjectionSpool,
    BulkProjectionState, EntryTrieCursor, SpoolEventRecord, BULK_PROJECTION_VERSION,
};

const COLLECTION_MANIFEST_METADATA_FILE: &str = ".collection-manifest.json";

#[derive(Debug, Clone)]
pub(crate) struct BulkProjectionAuditOptions {
    pub(crate) staging_data_dir: PathBuf,
    pub(crate) expected_state_sha256: Option<String>,
    pub(crate) expected_stage_state_sha256: Option<String>,
    pub(crate) btree_order: usize,
    pub(crate) page_size: usize,
    pub(crate) query_limit: usize,
    pub(crate) out: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct BulkProjectionIndexAudit {
    index: String,
    root: Option<String>,
    nodes: u64,
    links: u64,
    durable_values_validated: u64,
    entries_sha256: String,
    retained_set_sha256: String,
    first_key: Option<String>,
    last_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct BulkProjectionQueryAudit {
    query: String,
    parameters: serde_json::Value,
    event_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct BulkProjectionBlockEvidence {
    role: String,
    nhash: String,
    sha256: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct BulkProjectionProfileAudit {
    by_pubkey_root: String,
    by_pubkey_root_file_sha256: String,
    by_pubkey_nodes: u64,
    by_pubkey_links: u64,
    by_pubkey_entries_sha256: String,
    search_root: String,
    search_root_file_sha256: String,
    search_nodes: u64,
    search_entries: u64,
    search_entries_sha256: String,
    sample_pubkey: String,
    sample_event_id: String,
    sample_name: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct BulkProjectionAuditOutput {
    version: u32,
    candidate_root: String,
    state_sha256: String,
    stage_state_sha256: String,
    pool_catalog_sha256: String,
    pool_stored_locations: u64,
    authors_processed: usize,
    authors_total: usize,
    recovery_tranche_only: bool,
    indexes: Vec<BulkProjectionIndexAudit>,
    profile: BulkProjectionProfileAudit,
    queries: Vec<BulkProjectionQueryAudit>,
    representative_blocks: Vec<BulkProjectionBlockEvidence>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct EntrySetProof {
    count: u64,
    xor: [u8; 32],
}

impl EntrySetProof {
    fn insert(&mut self, index: NostrEventIndex, key: &str, cid: &Cid) -> Result<()> {
        let mut digest = Sha256::new();
        digest.update(b"hashtree-nostr-retained-index-entry-v1\0");
        digest.update([index.stable_id()]);
        digest.update((key.len() as u64).to_be_bytes());
        digest.update(key.as_bytes());
        let encoded_cid = encode_cid(cid);
        digest.update((encoded_cid.len() as u64).to_be_bytes());
        digest.update(encoded_cid);
        let entry: [u8; 32] = digest.finalize().into();
        for (target, byte) in self.xor.iter_mut().zip(entry) {
            *target ^= byte;
        }
        self.count = self
            .count
            .checked_add(1)
            .context("retained index entry count overflow")?;
        Ok(())
    }

    fn evidence_sha256(self) -> String {
        let mut digest = Sha256::new();
        digest.update(b"hashtree-nostr-retained-index-set-v1\0");
        digest.update(self.count.to_be_bytes());
        digest.update(self.xor);
        hex::encode(digest.finalize())
    }
}

impl BulkProjectionSpool {
    fn open_read_only(path: &Path) -> Result<Self> {
        if !path.join("data.mdb").is_file() {
            anyhow::bail!("bulk projection spool is missing at {}", path.display());
        }
        let mut options = EnvOpenOptions::new();
        options.max_dbs(3).max_readers(32);
        unsafe {
            options.flags(EnvFlags::READ_ONLY | EnvFlags::NO_READ_AHEAD);
        }
        let env = unsafe { options.open(path) }
            .with_context(|| format!("open bulk projection spool {} read-only", path.display()))?;
        let rtxn = env.read_txn()?;
        let open_database = |name| -> Result<Database<Bytes, Bytes>> {
            env.open_database(&rtxn, Some(name))?
                .with_context(|| format!("bulk projection spool omitted {name} database"))
        };
        let events = open_database("events")?;
        let slots = open_database("slots")?;
        let entries = open_database("entries")?;
        // Publishing DBI handles commits only the reader transaction. The
        // environment itself is READ_ONLY; LMDB may update reader slots in
        // lock.mdb, but this cannot mutate data.mdb.
        rtxn.commit()?;
        Ok(Self {
            env,
            entries,
            events,
            slots,
        })
    }

    fn event_record(&self, event_id: &str) -> Result<Option<SpoolEventRecord>> {
        let rtxn = self.env.read_txn()?;
        self.events
            .get(&rtxn, event_id.as_bytes())?
            .map(|encoded| rmp_serde::from_slice(encoded).context("decode bulk spool event record"))
            .transpose()
    }

    fn event_record_count(&self) -> Result<u64> {
        let rtxn = self.env.read_txn()?;
        let mut count = 0u64;
        for item in self.events.iter(&rtxn)? {
            item?;
            count = count
                .checked_add(1)
                .context("bulk spool event record count overflow")?;
        }
        Ok(count)
    }

    fn retained_profile_records(&self) -> Result<BTreeMap<String, SpoolEventRecord>> {
        let rtxn = self.env.read_txn()?;
        let mut profiles = BTreeMap::new();
        for item in self.events.iter(&rtxn)? {
            let (event_id, encoded) = item?;
            let event_id =
                std::str::from_utf8(event_id).context("bulk spool event key is not UTF-8")?;
            let record: SpoolEventRecord =
                rmp_serde::from_slice(encoded).context("decode retained profile spool event")?;
            if record.event.id != event_id {
                anyhow::bail!(
                    "bulk spool event key `{event_id}` differs from record id `{}`",
                    record.event.id
                );
            }
            if record.event.kind != 0 {
                continue;
            }
            if profiles
                .insert(record.event.pubkey.clone(), record)
                .is_some()
            {
                anyhow::bail!("bulk spool retained multiple kind-0 winners for one pubkey");
            }
        }
        Ok(profiles)
    }
}

fn bytes_sha256(bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(bytes);
    hex::encode(digest.finalize())
}

fn require_expected_sha256(label: &str, actual: &str, expected: Option<&str>) -> Result<()> {
    if let Some(expected) = expected {
        if expected != actual {
            anyhow::bail!("{label} SHA-256 mismatch: expected {expected}, found {actual}");
        }
    }
    Ok(())
}

fn manifest_root_for_index(
    manifest: &hashtree_nostr::NostrEventManifest,
    index: NostrEventIndex,
) -> Option<&Cid> {
    match index {
        NostrEventIndex::ById => manifest.by_id.as_ref(),
        NostrEventIndex::ByAuthorTime => manifest.by_author_time.as_ref(),
        NostrEventIndex::ByAuthorKindTime => manifest.by_author_kind_time.as_ref(),
        NostrEventIndex::ByKindTime => manifest.by_kind_time.as_ref(),
        NostrEventIndex::ByKindTimeAuthor => manifest.by_kind_time_author.as_ref(),
        NostrEventIndex::ByTime => manifest.by_time.as_ref(),
        NostrEventIndex::ByTag => manifest.by_tag.as_ref(),
        NostrEventIndex::Replaceable => manifest.replaceable.as_ref(),
        NostrEventIndex::ParameterizedReplaceable => manifest.parameterized_replaceable.as_ref(),
    }
}

async fn validate_exact_manifest_directory(
    store: Arc<ReadOnlyPoolStore>,
    target: &NostrEventStore<ReadOnlyPoolStore>,
    candidate_root: &Cid,
    manifest: &hashtree_nostr::NostrEventManifest,
) -> Result<Cid> {
    let tree = HashTree::new(HashTreeConfig::new(Arc::clone(&store)));
    let entries = tree
        .list_directory_required(candidate_root)
        .await
        .context("list exact bulk manifest directory")?;
    let mut expected = NostrEventIndex::ALL
        .into_iter()
        .map(|index| {
            manifest_root_for_index(manifest, index)
                .cloned()
                .with_context(|| {
                    format!(
                        "bulk manifest omitted required canonical `{}` root",
                        index.name()
                    )
                })
                .map(|cid| (index.name().to_string(), cid))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let mut metadata = None;
    for entry in entries {
        if entry.name == COLLECTION_MANIFEST_METADATA_FILE {
            if metadata.is_some() {
                anyhow::bail!("bulk manifest repeats collection metadata entry");
            }
            if entry.link_type != LinkType::File {
                anyhow::bail!("bulk manifest collection metadata is not a File link");
            }
            metadata = Some(Cid {
                hash: entry.hash,
                key: entry.key,
            });
            continue;
        }
        let expected_cid = expected.remove(&entry.name).with_context(|| {
            format!("bulk manifest has unexpected or duplicate `{}`", entry.name)
        })?;
        if entry.link_type != LinkType::Dir {
            anyhow::bail!("bulk manifest index `{}` is not a Dir link", entry.name);
        }
        if (Cid {
            hash: entry.hash,
            key: entry.key,
        }) != expected_cid
        {
            anyhow::bail!("bulk manifest index `{}` has the wrong CID", entry.name);
        }
    }
    if !expected.is_empty() {
        anyhow::bail!(
            "bulk manifest directory omitted canonical indexes: {:?}",
            expected.keys().collect::<Vec<_>>()
        );
    }
    let metadata = metadata.context("bulk manifest omitted collection metadata")?;
    target
        .validate_index_root(Some(candidate_root))
        .await
        .context("validate bulk collection manifest metadata and required indexes")?;
    Ok(metadata)
}

async fn audit_index_root(
    spool: &BulkProjectionSpool,
    target: &NostrEventStore<ReadOnlyPoolStore>,
    btree: &BTree<ReadOnlyPoolStore>,
    index: NostrEventIndex,
    root: Option<&Cid>,
    page_size: usize,
    expected_entries: &mut [EntrySetProof; 9],
) -> Result<(BulkProjectionIndexAudit, EntrySetProof)> {
    let mut digest = Sha256::new();
    digest.update(b"hashtree-nostr-bulk-index-parity-v1\0");
    digest.update(index.name().as_bytes());
    digest.update([0]);
    let mut retained_set = EntrySetProof::default();
    let mut cursor = EntryTrieCursor::new(spool, index);
    let Some(root) = root else {
        if let Some((key, _)) = cursor.next_entry()? {
            anyhow::bail!(
                "manifest {} root is empty but spool starts with `{key}`",
                index.name()
            );
        }
        return Ok((
            BulkProjectionIndexAudit {
                index: index.name().to_string(),
                root: None,
                nodes: 0,
                links: 0,
                durable_values_validated: 0,
                entries_sha256: hex::encode(digest.finalize()),
                retained_set_sha256: retained_set.evidence_sha256(),
                first_key: None,
                last_key: None,
            },
            retained_set,
        ));
    };

    let structural = btree
        .validate_link_tree(Some(root))
        .await
        .with_context(|| format!("exhaustively validate {} link tree", index.name()))?;
    let mut start = None::<String>;
    let mut links = 0u64;
    let mut durable_values_validated = 0u64;
    let mut first_key = None;
    let mut last_key = None;
    loop {
        let page = btree
            .range_links_limited(root, start.as_deref(), None, page_size)
            .await
            .with_context(|| format!("read {} root parity page", index.name()))?;
        if page.is_empty() {
            break;
        }
        for (key, cid) in &page {
            let Some((spool_key, spool_cid)) = cursor.next_entry()? else {
                anyhow::bail!(
                    "{} root has an extra key `{key}` at row {links}",
                    index.name()
                );
            };
            if spool_key != *key {
                anyhow::bail!(
                    "{} key mismatch at row {links}: spool=`{spool_key}` root=`{key}`",
                    index.name()
                );
            }
            if spool_cid != *cid {
                anyhow::bail!("{} CID mismatch at row {links}, key=`{key}`", index.name());
            }
            retained_set.insert(index, key, cid)?;
            if index == NostrEventIndex::ById {
                let record = spool
                    .event_record(key)?
                    .with_context(|| format!("by-id spool key `{key}` has no event record"))?;
                let record_cid = Cid {
                    hash: record.cid_hash,
                    key: record.cid_key,
                };
                if record_cid != *cid {
                    anyhow::bail!("by-id event record CID differs at key `{key}`");
                }
                let durable = target
                    .load_event_blob(cid)
                    .await
                    .with_context(|| format!("exhaustively load durable by-id event `{key}`"))?;
                if durable.id != *key || durable != record.event {
                    anyhow::bail!(
                        "durable by-id event `{key}` differs from its exact spool record"
                    );
                }
                durable_values_validated = durable_values_validated
                    .checked_add(1)
                    .context("bulk index durable value validation count overflow")?;
                let entries = nostr_event_index_entries(&record.event, cid);
                for (position, entry) in entries.iter().enumerate() {
                    if entries[..position]
                        .iter()
                        .any(|seen| seen.index == entry.index && seen.key == entry.key)
                    {
                        continue;
                    }
                    expected_entries[entry.index.stable_id() as usize].insert(
                        entry.index,
                        &entry.key,
                        &entry.cid,
                    )?;
                }
            }
            digest.update((key.len() as u64).to_be_bytes());
            digest.update(key.as_bytes());
            let encoded_cid = encode_cid(cid);
            digest.update((encoded_cid.len() as u64).to_be_bytes());
            digest.update(encoded_cid);
            if first_key.is_none() {
                first_key = Some(key.clone());
            }
            last_key = Some(key.clone());
            links = links
                .checked_add(1)
                .context("bulk index parity link count overflow")?;
        }
        start = Some(format!(
            "{}\0",
            page.last().expect("non-empty index parity page").0
        ));
        if page.len() < page_size {
            break;
        }
    }
    if let Some((key, _)) = cursor.next_entry()? {
        anyhow::bail!(
            "{} root ended at row {links} before spool key `{key}`",
            index.name()
        );
    }
    if links != structural.links {
        anyhow::bail!(
            "{} structural link count {} differs from exact parity count {links}",
            index.name(),
            structural.links
        );
    }
    Ok((
        BulkProjectionIndexAudit {
            index: index.name().to_string(),
            root: Some(cid_to_nhash(root)?),
            nodes: structural.nodes,
            links,
            durable_values_validated,
            entries_sha256: hex::encode(digest.finalize()),
            retained_set_sha256: retained_set.evidence_sha256(),
            first_key,
            last_key,
        },
        retained_set,
    ))
}

async fn load_spool_prefix_events(
    spool: &BulkProjectionSpool,
    target: &NostrEventStore<ReadOnlyPoolStore>,
    index: NostrEventIndex,
    prefix: &str,
    limit: usize,
) -> Result<Vec<StoredNostrEvent>> {
    let mut cursor = EntryTrieCursor::new(spool, index);
    let mut events = Vec::new();
    while let Some((key, cid)) = cursor.next_entry()? {
        if !key.starts_with(prefix) {
            if !events.is_empty() {
                break;
            }
            continue;
        }
        events.push(
            target
                .load_event_blob(&cid)
                .await
                .with_context(|| format!("load {} query parity event `{key}`", index.name()))?,
        );
        if events.len() == limit {
            break;
        }
    }
    Ok(events)
}

fn event_ids(events: &[StoredNostrEvent]) -> Vec<String> {
    events.iter().map(|event| event.id.clone()).collect()
}

fn checked_query(
    query: &str,
    parameters: serde_json::Value,
    expected: &[StoredNostrEvent],
    actual: &[StoredNostrEvent],
) -> Result<BulkProjectionQueryAudit> {
    let expected_ids = event_ids(expected);
    let actual_ids = event_ids(actual);
    if actual_ids != expected_ids {
        anyhow::bail!(
            "{query} query differs from deterministic spool truth: expected={expected_ids:?} actual={actual_ids:?}"
        );
    }
    Ok(BulkProjectionQueryAudit {
        query: query.to_string(),
        parameters,
        event_ids: actual_ids,
    })
}

fn first_spool_entry(spool: &BulkProjectionSpool, index: NostrEventIndex) -> Result<(String, Cid)> {
    EntryTrieCursor::new(spool, index)
        .next_entry()?
        .with_context(|| format!("{} spool has no real query candidate", index.name()))
}

async fn audit_real_queries(
    spool: &BulkProjectionSpool,
    target: &NostrEventStore<ReadOnlyPoolStore>,
    root: &Cid,
    limit: usize,
) -> Result<(Vec<BulkProjectionQueryAudit>, Cid)> {
    let list_options = || ListEventsOptions {
        limit: Some(limit),
        since: None,
        until: None,
    };
    let mut queries = Vec::new();

    let (by_id_key, representative_event_cid) = first_spool_entry(spool, NostrEventIndex::ById)?;
    let expected_by_id =
        load_spool_prefix_events(spool, target, NostrEventIndex::ById, &by_id_key, 1).await?;
    let actual_by_id = target
        .get_by_id(Some(root), &by_id_key)
        .await
        .context("query by-id terminal candidate")?
        .into_iter()
        .collect::<Vec<_>>();
    queries.push(checked_query(
        "by-id",
        serde_json::json!({"id": by_id_key}),
        &expected_by_id,
        &actual_by_id,
    )?);

    let (_, author_cid) = first_spool_entry(spool, NostrEventIndex::ByAuthorTime)?;
    let author_event = target.load_event_blob(&author_cid).await?;
    let author_prefix = format!("{}:", author_event.pubkey);
    let expected_author = load_spool_prefix_events(
        spool,
        target,
        NostrEventIndex::ByAuthorTime,
        &author_prefix,
        limit,
    )
    .await?;
    let actual_author = target
        .list_by_author(Some(root), &author_event.pubkey, list_options())
        .await?;
    queries.push(checked_query(
        "by-author",
        serde_json::json!({"author": author_event.pubkey, "limit": limit}),
        &expected_author,
        &actual_author,
    )?);

    let (_, author_kind_cid) = first_spool_entry(spool, NostrEventIndex::ByAuthorKindTime)?;
    let author_kind_event = target.load_event_blob(&author_kind_cid).await?;
    let author_kind_prefix = format!(
        "{}:{:08x}:",
        author_kind_event.pubkey, author_kind_event.kind
    );
    let expected_author_kind = load_spool_prefix_events(
        spool,
        target,
        NostrEventIndex::ByAuthorKindTime,
        &author_kind_prefix,
        limit,
    )
    .await?;
    let actual_author_kind = target
        .list_by_author_and_kind(
            Some(root),
            &author_kind_event.pubkey,
            author_kind_event.kind,
            list_options(),
        )
        .await?;
    queries.push(checked_query(
        "by-author-kind",
        serde_json::json!({
            "author": author_kind_event.pubkey,
            "kind": author_kind_event.kind,
            "limit": limit
        }),
        &expected_author_kind,
        &actual_author_kind,
    )?);

    let (_, kind_cid) = first_spool_entry(spool, NostrEventIndex::ByKindTime)?;
    let kind_event = target.load_event_blob(&kind_cid).await?;
    let kind_prefix = format!("{:08x}:", kind_event.kind);
    let expected_kind = load_spool_prefix_events(
        spool,
        target,
        NostrEventIndex::ByKindTime,
        &kind_prefix,
        limit,
    )
    .await?;
    let actual_kind = target
        .list_by_kind(Some(root), kind_event.kind, list_options())
        .await?;
    queries.push(checked_query(
        "by-kind",
        serde_json::json!({"kind": kind_event.kind, "limit": limit}),
        &expected_kind,
        &actual_kind,
    )?);

    let expected_recent =
        load_spool_prefix_events(spool, target, NostrEventIndex::ByTime, "", limit).await?;
    let actual_recent = target.list_recent(Some(root), list_options()).await?;
    queries.push(checked_query(
        "recent",
        serde_json::json!({"limit": limit}),
        &expected_recent,
        &actual_recent,
    )?);

    let (tag_key, _) = first_spool_entry(spool, NostrEventIndex::ByTag)?;
    let tag_prefix = tag_key
        .rsplit_once(':')
        .and_then(|(without_id, _)| without_id.rsplit_once(':').map(|(prefix, _)| prefix))
        .context("first by-tag spool key has no timestamp/event suffix")?;
    let (tag_name, tag_value) = tag_prefix
        .split_once(':')
        .context("first by-tag spool key has no name/value prefix")?;
    let expected_tag = load_spool_prefix_events(
        spool,
        target,
        NostrEventIndex::ByTag,
        &format!("{tag_prefix}:"),
        limit,
    )
    .await?;
    let actual_tag = target
        .list_by_tag(Some(root), tag_name, tag_value, list_options())
        .await?;
    queries.push(checked_query(
        "by-tag",
        serde_json::json!({
            "tag": tag_name,
            "value": tag_value,
            "limit": limit
        }),
        &expected_tag,
        &actual_tag,
    )?);

    let (_, replaceable_cid) = first_spool_entry(spool, NostrEventIndex::Replaceable)?;
    let replaceable_event = target.load_event_blob(&replaceable_cid).await?;
    let expected_replaceable = vec![replaceable_event.clone()];
    let actual_replaceable = target
        .get_replaceable(
            Some(root),
            &replaceable_event.pubkey,
            replaceable_event.kind,
        )
        .await?
        .into_iter()
        .collect::<Vec<_>>();
    queries.push(checked_query(
        "replaceable",
        serde_json::json!({
            "author": replaceable_event.pubkey,
            "kind": replaceable_event.kind
        }),
        &expected_replaceable,
        &actual_replaceable,
    )?);

    let (_, parameterized_cid) =
        first_spool_entry(spool, NostrEventIndex::ParameterizedReplaceable)?;
    let parameterized_event = target.load_event_blob(&parameterized_cid).await?;
    let d_tag = parameterized_event
        .tags
        .iter()
        .find_map(|tag| match tag.as_slice() {
            [name, value, ..] if name == "d" => Some(value.as_str()),
            _ => None,
        })
        .unwrap_or_default();
    let expected_parameterized = vec![parameterized_event.clone()];
    let actual_parameterized = target
        .get_parameterized_replaceable(
            Some(root),
            &parameterized_event.pubkey,
            parameterized_event.kind,
            d_tag,
        )
        .await?
        .into_iter()
        .collect::<Vec<_>>();
    queries.push(checked_query(
        "parameterized-replaceable",
        serde_json::json!({
            "author": parameterized_event.pubkey,
            "kind": parameterized_event.kind,
            "d": d_tag
        }),
        &expected_parameterized,
        &actual_parameterized,
    )?);

    Ok((queries, representative_event_cid))
}

async fn block_evidence(
    role: impl Into<String>,
    cid: &Cid,
    store: &ReadOnlyPoolStore,
) -> Result<BulkProjectionBlockEvidence> {
    store
        .get(&cid.hash)
        .await
        .with_context(|| format!("read representative block {}", hex::encode(cid.hash)))?
        .with_context(|| format!("representative block {} is missing", hex::encode(cid.hash)))?;
    Ok(BulkProjectionBlockEvidence {
        role: role.into(),
        nhash: cid_to_nhash(cid)?,
        sha256: hex::encode(cid.hash),
    })
}

async fn audit_profile_indexes(
    data_dir: &Path,
    spool: &BulkProjectionSpool,
    store: Arc<ReadOnlyPoolStore>,
) -> Result<(
    BulkProjectionProfileAudit,
    Vec<Cid>,
    hashtree_cli::socialgraph::ProfileIndexRoots,
)> {
    #[derive(Debug)]
    struct ExpectedSearchEntry {
        pubkey: String,
        created_at: u64,
        event_nhash: String,
        event_id: String,
    }

    let roots_before = hashtree_cli::socialgraph::read_profile_index_roots(data_dir)?;
    let by_pubkey_root = roots_before
        .by_pubkey
        .clone()
        .context("profile-by-pubkey root is missing")?;
    let by_pubkey_root_file_sha256 = roots_before
        .by_pubkey_file_sha256
        .clone()
        .context("profile-by-pubkey root file hash is missing")?;
    let search_root = roots_before
        .search
        .clone()
        .context("profile-search root is missing")?;
    let search_root_file_sha256 = roots_before
        .search_file_sha256
        .clone()
        .context("profile-search root file hash is missing")?;
    let btree = BTree::new(Arc::clone(&store), BTreeOptions { order: Some(64) });
    let by_pubkey = btree
        .validate_link_tree(Some(&by_pubkey_root))
        .await
        .context("exhaustively validate profile-by-pubkey root")?;
    let by_pubkey_entries = btree
        .links_entries(Some(&by_pubkey_root))
        .await
        .context("exhaustively traverse profile-by-pubkey root")?;
    if by_pubkey_entries.len() as u64 != by_pubkey.links {
        anyhow::bail!(
            "profile-by-pubkey structural count {} differs from traversal count {}",
            by_pubkey.links,
            by_pubkey_entries.len()
        );
    }
    let mut retained_profiles = spool.retained_profile_records()?;
    if retained_profiles.is_empty() {
        anyhow::bail!("bulk spool contains no retained kind-0 profiles");
    }
    let tree = HashTree::new(HashTreeConfig::new(Arc::clone(&store)));
    let mut by_pubkey_digest = Sha256::new();
    by_pubkey_digest.update(b"hashtree-profile-by-pubkey-audit-v1\0");
    let mut expected_search = BTreeMap::<String, ExpectedSearchEntry>::new();
    let mut representative_profile_cid = None;
    for (pubkey, mirrored_cid) in &by_pubkey_entries {
        let record = retained_profiles
            .remove(pubkey)
            .with_context(|| format!("profile-by-pubkey contains unexpected pubkey `{pubkey}`"))?;
        let expected_event = record
            .event
            .to_nostr_sdk_event()
            .with_context(|| format!("decode retained profile event {}", record.event.id))?;
        let mirrored_bytes = tree
            .get(mirrored_cid, None)
            .await
            .with_context(|| format!("read mirrored profile event for {pubkey}"))?
            .with_context(|| format!("mirrored profile event for {pubkey} is missing"))?;
        let mirrored = Event::from_json(
            String::from_utf8(mirrored_bytes)
                .with_context(|| format!("decode mirrored profile event for {pubkey} as UTF-8"))?,
        )
        .with_context(|| format!("decode mirrored profile event JSON for {pubkey}"))?;
        if mirrored != expected_event {
            anyhow::bail!(
                "profile-by-pubkey mirrored event for `{pubkey}` differs from retained event {}",
                record.event.id
            );
        }
        let event_nhash = cid_to_nhash(mirrored_cid)?;
        for key in hashtree_cli::socialgraph::profile_search_keys_for_event(&mirrored) {
            let expected = ExpectedSearchEntry {
                pubkey: pubkey.clone(),
                created_at: mirrored.created_at.as_secs(),
                event_nhash: event_nhash.clone(),
                event_id: mirrored.id.to_hex(),
            };
            if expected_search.insert(key.clone(), expected).is_some() {
                anyhow::bail!("retained profiles produced duplicate search key `{key}`");
            }
        }
        by_pubkey_digest.update((pubkey.len() as u64).to_be_bytes());
        by_pubkey_digest.update(pubkey.as_bytes());
        let encoded_cid = encode_cid(mirrored_cid);
        by_pubkey_digest.update((encoded_cid.len() as u64).to_be_bytes());
        by_pubkey_digest.update(encoded_cid);
        representative_profile_cid.get_or_insert_with(|| mirrored_cid.clone());
    }
    if !retained_profiles.is_empty() {
        anyhow::bail!(
            "profile-by-pubkey omitted {} retained profiles; first missing pubkey `{}`",
            retained_profiles.len(),
            retained_profiles
                .first_key_value()
                .expect("non-empty retained profiles")
                .0
        );
    }

    let search_structural = btree
        .validate_value_tree(Some(&search_root))
        .await
        .context("exhaustively validate profile-search root structure")?;
    let search_entries = btree
        .entries(Some(&search_root))
        .await
        .context("exhaustively traverse profile-search root")?;
    if search_entries.len() as u64 != search_structural.entries {
        anyhow::bail!(
            "profile-search structural count {} differs from traversal count {}",
            search_structural.entries,
            search_entries.len()
        );
    }
    let mut search_digest = Sha256::new();
    search_digest.update(b"hashtree-profile-search-audit-v1\0");
    let mut sample = None;
    for (key, value) in &search_entries {
        let expected = expected_search
            .remove(key)
            .with_context(|| format!("profile-search contains unexpected key `{key}`"))?;
        let entry: hashtree_cli::socialgraph::StoredProfileSearchEntry =
            serde_json::from_str(value)
                .with_context(|| format!("decode profile-search entry `{key}`"))?;
        if entry.pubkey != expected.pubkey
            || entry.created_at != expected.created_at
            || entry.event_nhash != expected.event_nhash
        {
            anyhow::bail!(
                "profile-search entry `{key}` does not match its retained mirrored profile"
            );
        }
        sample.get_or_insert_with(|| {
            (
                expected.pubkey.clone(),
                expected.event_id,
                entry.name.clone(),
            )
        });
        search_digest.update((key.len() as u64).to_be_bytes());
        search_digest.update(key.as_bytes());
        search_digest.update((value.len() as u64).to_be_bytes());
        search_digest.update(value.as_bytes());
    }
    if !expected_search.is_empty() {
        anyhow::bail!(
            "profile-search omitted {} expected keys; first missing key `{}`",
            expected_search.len(),
            expected_search
                .first_key_value()
                .expect("non-empty expected search entries")
                .0
        );
    }
    let (sample_pubkey, sample_event_id, sample_name) =
        sample.context("profile-search root contains no entries")?;
    let roots_after = hashtree_cli::socialgraph::read_profile_index_roots(data_dir)?;
    if roots_after != roots_before {
        anyhow::bail!("profile index root files changed during read-only audit");
    }
    let representative_profile_cid =
        representative_profile_cid.context("profile-by-pubkey root contains no entries")?;
    Ok((
        BulkProjectionProfileAudit {
            by_pubkey_root: cid_to_nhash(&by_pubkey_root)?,
            by_pubkey_root_file_sha256,
            by_pubkey_nodes: by_pubkey.nodes,
            by_pubkey_links: by_pubkey.links,
            by_pubkey_entries_sha256: hex::encode(by_pubkey_digest.finalize()),
            search_root: cid_to_nhash(&search_root)?,
            search_root_file_sha256,
            search_nodes: search_structural.nodes,
            search_entries: search_entries.len() as u64,
            search_entries_sha256: hex::encode(search_digest.finalize()),
            sample_pubkey,
            sample_event_id,
            sample_name,
        },
        vec![by_pubkey_root, search_root, representative_profile_cid],
        roots_before,
    ))
}

fn write_audit_output(output: &BulkProjectionAuditOutput, out: Option<&Path>) -> Result<()> {
    match out {
        None => {
            println!("{}", serde_json::to_string_pretty(output)?);
            Ok(())
        }
        Some(path) if path == Path::new("-") => {
            println!("{}", serde_json::to_string_pretty(output)?);
            Ok(())
        }
        Some(path) => persist_json_atomic(path, output, "bulk projection audit evidence"),
    }
}

pub(crate) async fn audit_bulk_projection(
    data_dir: &Path,
    options: BulkProjectionAuditOptions,
) -> Result<BulkProjectionAuditOutput> {
    if options.btree_order < 2 || options.page_size == 0 || options.query_limit == 0 {
        anyhow::bail!(
            "audit B-tree order must be at least 2 and page/query limits must be non-zero"
        );
    }
    let (state_path, spool_path) = bulk_paths(data_dir);
    let stage_state_path = options
        .staging_data_dir
        .join(STAGE_DIR)
        .join(STAGE_STATE_FILE);
    let state_bytes = std::fs::read(&state_path)
        .with_context(|| format!("read bulk projection state {}", state_path.display()))?;
    let stage_bytes = std::fs::read(&stage_state_path)
        .with_context(|| format!("read staging state {}", stage_state_path.display()))?;
    let state_sha256 = bytes_sha256(&state_bytes);
    let stage_state_sha256 = bytes_sha256(&stage_bytes);
    require_expected_sha256(
        "bulk projection state",
        &state_sha256,
        options.expected_state_sha256.as_deref(),
    )?;
    require_expected_sha256(
        "staging state",
        &stage_state_sha256,
        options.expected_stage_state_sha256.as_deref(),
    )?;
    let state: BulkProjectionState =
        serde_json::from_slice(&state_bytes).context("parse bulk projection state")?;
    let stage: StagedNostrCrawlState =
        serde_json::from_slice(&stage_bytes).context("parse staging state")?;
    if state.version != BULK_PROJECTION_VERSION {
        anyhow::bail!(
            "unsupported bulk projection state version {}",
            state.version
        );
    }
    if stage.version != STAGE_FORMAT_VERSION {
        anyhow::bail!("unsupported frozen staging state version {}", stage.version);
    }
    if state.author_allowlist_source != stage.author_allowlist_source {
        anyhow::bail!(
            "terminal bulk projection allowlist source differs from frozen staging source"
        );
    }
    if state.next_author > state.policy.author_count {
        anyhow::bail!(
            "terminal bulk projection author watermark {} exceeds policy author count {}",
            state.next_author,
            state.policy.author_count
        );
    }
    validate_terminal_stage_state(&state, &stage)?;
    if state.built_roots.len() != NostrEventIndex::ALL.len()
        || NostrEventIndex::ALL
            .iter()
            .any(|index| !state.built_roots.contains_key(&index.stable_id()))
    {
        anyhow::bail!("bulk projection state must contain exactly all nine index roots");
    }
    let candidate_root = state
        .complete_root
        .as_deref()
        .context("bulk projection has no complete candidate root")
        .and_then(parse_root_text)?;

    let spool = BulkProjectionSpool::open_read_only(&spool_path)?;
    let pool_path = data_dir.join(SHARED_BLOB_POOL_DIR_NAME);
    let store = Arc::new(
        ReadOnlyPoolStore::open(&pool_path)
            .with_context(|| format!("open exact native PoolStore {}", pool_path.display()))?,
    );
    let catalog_before = store
        .validate_committed_catalog()
        .context("validate fully committed PoolStore catalog")?;
    let target = NostrEventStore::new(Arc::clone(&store));
    let manifest = target
        .get_manifest(Some(&candidate_root))
        .await
        .context("read complete bulk projection manifest")?;
    let manifest_metadata =
        validate_exact_manifest_directory(Arc::clone(&store), &target, &candidate_root, &manifest)
            .await?;
    let btree = BTree::new(
        Arc::clone(&store),
        BTreeOptions {
            order: Some(options.btree_order),
        },
    );

    let mut indexes = Vec::with_capacity(NostrEventIndex::ALL.len());
    let mut expected_entries = [EntrySetProof::default(); 9];
    let mut representative_cids = vec![candidate_root.clone(), manifest_metadata];
    for index in NostrEventIndex::ALL {
        let encoded = state
            .built_roots
            .get(&index.stable_id())
            .expect("all nine state roots checked");
        if encoded.is_empty() {
            anyhow::bail!(
                "bulk projection state omitted required canonical `{}` root",
                index.name()
            );
        }
        let state_root = Some(
            parse_root_text(encoded)
                .with_context(|| format!("parse state {} root", index.name()))?,
        );
        let manifest_root = manifest_root_for_index(&manifest, index);
        if manifest_root != state_root.as_ref() {
            anyhow::bail!(
                "manifest {} root differs from exact projection state",
                index.name()
            );
        }
        let (audit, retained_set) = audit_index_root(
            &spool,
            &target,
            &btree,
            index,
            manifest_root,
            options.page_size,
            &mut expected_entries,
        )
        .await?;
        let expected = expected_entries[index.stable_id() as usize];
        if retained_set != expected {
            anyhow::bail!(
                "{} root/spool entries do not exactly match the retained by-id event set: \
                 actual_count={} expected_count={} actual_digest={} expected_digest={}",
                index.name(),
                retained_set.count,
                expected.count,
                retained_set.evidence_sha256(),
                expected.evidence_sha256()
            );
        }
        indexes.push(audit);
        if let Some(root) = manifest_root {
            representative_cids.push(root.clone());
        }
    }
    let event_records = spool.event_record_count()?;
    if event_records != indexes[0].durable_values_validated {
        anyhow::bail!(
            "bulk spool contains {event_records} event records but by-id validated {}",
            indexes[0].durable_values_validated
        );
    }

    let (profile, profile_cids, profile_roots_before) =
        audit_profile_indexes(data_dir, &spool, Arc::clone(&store)).await?;
    representative_cids.extend(profile_cids);
    let (queries, representative_event_cid) =
        audit_real_queries(&spool, &target, &candidate_root, options.query_limit).await?;
    representative_cids.push(representative_event_cid);

    let mut representative_blocks = Vec::new();
    let mut seen_blocks = HashSet::new();
    for (position, cid) in representative_cids.iter().enumerate() {
        if seen_blocks.insert(cid.hash) {
            let role = match position {
                0 => "manifest".to_string(),
                1 => "manifest-metadata".to_string(),
                2..=10 => format!("index-root-{}", position - 2),
                _ => format!("representative-{position}"),
            };
            representative_blocks.push(block_evidence(role, cid, &store).await?);
        }
    }

    let final_state_bytes = std::fs::read(&state_path)
        .with_context(|| format!("re-read bulk projection state {}", state_path.display()))?;
    let final_stage_bytes = std::fs::read(&stage_state_path)
        .with_context(|| format!("re-read staging state {}", stage_state_path.display()))?;
    if bytes_sha256(&final_state_bytes) != state_sha256
        || bytes_sha256(&final_stage_bytes) != stage_state_sha256
    {
        anyhow::bail!("projection or staging state changed during read-only audit");
    }
    let catalog_after = store
        .validate_committed_catalog()
        .context("revalidate PoolStore catalog after audit")?;
    if catalog_after != catalog_before {
        anyhow::bail!("PoolStore catalog changed during read-only audit");
    }
    let profile_roots_after = hashtree_cli::socialgraph::read_profile_index_roots(data_dir)?;
    if profile_roots_after != profile_roots_before {
        anyhow::bail!("profile index root files changed during read-only audit");
    }

    let output = BulkProjectionAuditOutput {
        version: 1,
        candidate_root: cid_to_nhash(&candidate_root)?,
        state_sha256,
        stage_state_sha256,
        pool_catalog_sha256: catalog_before.sha256,
        pool_stored_locations: catalog_before.stored_locations,
        authors_processed: state.next_author,
        authors_total: state.policy.author_count,
        recovery_tranche_only: state.next_author < state.policy.author_count,
        indexes,
        profile,
        queries,
        representative_blocks,
    };
    write_audit_output(&output, options.out.as_deref())?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashtree_config::StorageBackend;
    use hashtree_nostr::{stored_event_from_nostr_sdk_event, NostrEventStoreOptions};
    use nostr::{EventBuilder, Keys, Kind, Tag, Timestamp};

    use super::super::super::{
        persist_stage_state, run_nostr_bulk_projection_audit, CrawlStateLock,
        IndexedNostrCrawlPolicy, StagedNostrCrawlState, STAGE_FORMAT_VERSION,
    };

    fn policy(author_count: usize) -> IndexedNostrCrawlPolicy {
        IndexedNostrCrawlPolicy {
            base_root: None,
            author_allowlist_sha256: "ab".repeat(32),
            author_count,
            relays: vec!["wss://relay.example".to_string()],
            require_all_relays: false,
            max_events_seen: None,
            max_authors: author_count,
            max_follow_distance: Some(0),
            max_live_bytes: 1_000_000,
            author_batch_size: 1,
            checkpoint_authors: 1,
            per_author_event_limit: 256,
            per_author_kind_event_limit: None,
            per_author_live_bytes: Some(64 * 1024 * 1024),
            fetch_timeout_millis: 30_000,
            relay_event_max_bytes: Some(1024 * 1024),
            global_relay_scan: false,
            full_author_history: true,
            negentropy_only: false,
            relay_page_size: 1_000,
            max_relay_pages: 67,
            kinds: Some(vec![0, 1, 30_000]),
        }
    }

    #[tokio::test]
    async fn audits_real_pool_spool_manifest_profiles_and_queries_read_only() {
        let temp = tempfile::tempdir().unwrap();
        let data_dir = temp.path().join("projection");
        let staging_data_dir = temp.path().join("staging");
        let evidence_path = temp.path().join("audit-evidence.json");
        let store = hashtree_cli::HashtreeStore::with_options_and_backend(
            &data_dir,
            None,
            0,
            false,
            &StorageBackend::Lmdb,
        )
        .unwrap();
        let graph = hashtree_cli::socialgraph::open_social_graph_store_with_storage(
            &data_dir,
            store.store_arc(),
            Some(128 * 1024 * 1024),
        )
        .unwrap();

        let keys = Keys::generate();
        let profile = EventBuilder::new(Kind::Metadata, r#"{"name":"Audit Alice"}"#)
            .custom_created_at(Timestamp::from_secs(10))
            .sign_with_keys(&keys)
            .unwrap();
        let note = EventBuilder::new(Kind::TextNote, "real audit note")
            .tags([Tag::parse(["t", "hashtree"]).unwrap()])
            .custom_created_at(Timestamp::from_secs(20))
            .sign_with_keys(&keys)
            .unwrap();
        let parameterized = EventBuilder::new(Kind::Custom(30_000), "real parameterized event")
            .tags([Tag::identifier("audit-article")])
            .custom_created_at(Timestamp::from_secs(30))
            .sign_with_keys(&keys)
            .unwrap();
        let stored = [&profile, &note, &parameterized]
            .into_iter()
            .map(stored_event_from_nostr_sdk_event)
            .collect::<Vec<_>>();
        let event_store = NostrEventStore::with_options(
            store.store_arc(),
            NostrEventStoreOptions {
                btree_order: Some(8),
                ..NostrEventStoreOptions::default()
            },
        );
        let event_cids = event_store.store_event_blobs(stored.clone()).await.unwrap();
        store.force_sync().unwrap();

        let (state_path, spool_path) = bulk_paths(&data_dir);
        let spool = BulkProjectionSpool::open(&spool_path).unwrap();
        spool
            .apply(stored.clone().into_iter().zip(event_cids).collect())
            .unwrap();
        graph
            .sync_profile_index_for_events(std::slice::from_ref(&profile))
            .unwrap();
        graph.force_sync().unwrap();

        let mut roots = BTreeMap::new();
        for index in NostrEventIndex::ALL {
            roots.insert(
                index,
                spool
                    .build_index_root(index, store.store_arc(), 8)
                    .await
                    .unwrap(),
            );
        }
        let candidate_root = event_store
            .write_bulk_index_manifest(&roots)
            .await
            .unwrap()
            .unwrap();
        store.force_sync().unwrap();

        let policy = policy(10);
        let state = BulkProjectionState {
            version: BULK_PROJECTION_VERSION,
            author_allowlist_source: Some("file:///audit-authors".to_string()),
            policy: policy.clone(),
            next_author: 3,
            segment_event_offset: 0,
            events_seen: 3,
            events_selected: 3,
            live_bytes_selected: 123,
            built_roots: roots
                .iter()
                .map(|(index, root)| {
                    (
                        index.stable_id(),
                        root.as_ref()
                            .map(cid_to_nhash)
                            .transpose()
                            .unwrap()
                            .unwrap_or_default(),
                    )
                })
                .collect(),
            complete_root: Some(cid_to_nhash(&candidate_root).unwrap()),
        };
        super::super::persist_bulk_state(&state_path, &state).unwrap();
        let stage = StagedNostrCrawlState {
            version: STAGE_FORMAT_VERSION,
            author_allowlist_source: Some("file:///audit-authors".to_string()),
            policy,
            next_author: state.next_author,
            events_seen: state.events_seen,
            events_selected: state.events_selected,
            live_bytes_selected: state.live_bytes_selected,
        };
        persist_stage_state(&staging_data_dir, &stage).unwrap();
        drop(CrawlStateLock::acquire(&data_dir).unwrap());
        drop(CrawlStateLock::acquire_stage(&staging_data_dir).unwrap());

        drop(event_store);
        drop(graph);
        // Heed caches opened environments process-wide. Explicitly remove the
        // writer from that cache so this same-process integration test can
        // exercise the auditor's deliberately incompatible READ_ONLY open.
        let spool_closing = spool.env.clone().prepare_for_closing();
        drop(spool);
        spool_closing.wait();
        drop(store);

        let state_sha256 = bytes_sha256(&std::fs::read(&state_path).unwrap());
        let stage_path = staging_data_dir.join(STAGE_DIR).join(STAGE_STATE_FILE);
        let stage_sha256 = bytes_sha256(&std::fs::read(stage_path).unwrap());
        run_nostr_bulk_projection_audit(
            data_dir,
            BulkProjectionAuditOptions {
                staging_data_dir,
                expected_state_sha256: Some(state_sha256),
                expected_stage_state_sha256: Some(stage_sha256),
                btree_order: 8,
                page_size: 2,
                query_limit: 2,
                out: Some(evidence_path.clone()),
            },
        )
        .await
        .unwrap();

        let output: serde_json::Value =
            serde_json::from_slice(&std::fs::read(evidence_path).unwrap()).unwrap();
        assert_eq!(
            output["candidate_root"],
            cid_to_nhash(&candidate_root).unwrap()
        );
        assert_eq!(output["recovery_tranche_only"], true);
        assert_eq!(output["indexes"].as_array().unwrap().len(), 9);
        assert_eq!(output["indexes"][0]["durable_values_validated"], 3);
        assert_eq!(output["profile"]["by_pubkey_links"], 1);
        assert!(output["profile"]["search_entries"].as_u64().unwrap() >= 1);
        assert_eq!(
            output["profile"]["by_pubkey_root_file_sha256"]
                .as_str()
                .unwrap()
                .len(),
            64
        );
        assert_eq!(
            output["profile"]["search_root_file_sha256"]
                .as_str()
                .unwrap()
                .len(),
            64
        );
        let query_names = output["queries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|query| query["query"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert!(query_names.contains(&"replaceable"));
        assert!(query_names.contains(&"parameterized-replaceable"));
        assert!(!output["representative_blocks"]
            .as_array()
            .unwrap()
            .is_empty());
    }
}
