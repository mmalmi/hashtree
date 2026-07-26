use std::cmp::Ordering;
use std::collections::{BTreeMap, VecDeque};
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use hashtree_core::{sha256, Cid, Store};
use hashtree_index::{BTree, BTreeOptions};
use hashtree_nostr::{
    compare_nostr_replaceable_events, nostr_event_index_entries, nostr_replaceable_slot,
    CrawlReport, NostrEventIndex, NostrEventStore, StoredNostrEvent,
};
use heed::types::Bytes;
use heed::{Database, Env, EnvOpenOptions};
use serde::{Deserialize, Serialize};

use super::{
    cid_to_nhash, load_stage_segment, load_stage_state, parse_root_text, persist_crawl_state,
    persist_json_atomic, validate_reachable_root, validate_stage_state, IndexedNostrCrawlPolicy,
    IndexedNostrCrawlState, ProjectionStores, SocialGraphIndexOptions, CRAWL_STATE_VERSION,
    INDEX_DIR,
};

const BULK_PROJECTION_VERSION: u32 = 2;
const BULK_PROJECTION_DIR: &str = "bulk-projection-v2";
const BULK_PROJECTION_STATE_FILE: &str = "state.json";
const BULK_PROJECTION_SPOOL_DIR: &str = "spool";
const REJECTED_SPOOL_EDGE_STATE_FILE: &str = "trusted-spool-edge-state-v1.json";
const BULK_PROJECTION_MAP_SIZE: usize = 256 * 1024 * 1024 * 1024;
const ENTRY_CHUNK_SIZE: usize = 400;
const ENTRY_PREFIX_SIZE: usize = 33;
const ENTRY_PAGE_SIZE: usize = 4_096;
const EDGE_HAS_CID: u8 = 1;
const EDGE_HAS_CHILDREN: u8 = 2;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct BulkProjectionState {
    version: u32,
    author_allowlist_source: Option<String>,
    policy: IndexedNostrCrawlPolicy,
    next_author: usize,
    segment_event_offset: usize,
    events_seen: usize,
    events_selected: usize,
    live_bytes_selected: u64,
    #[serde(default)]
    built_roots: BTreeMap<u8, String>,
    #[serde(default)]
    complete_root: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SpoolEventRecord {
    event: StoredNostrEvent,
    cid_hash: [u8; 32],
    cid_key: Option<[u8; 32]>,
}

#[derive(Debug, Default)]
struct SpoolApplyReport {
    retained_events: Vec<StoredNostrEvent>,
    inserted: usize,
    replaced: usize,
    skipped: usize,
    index_entries: usize,
    reused_records: usize,
    durable_reused_candidates: usize,
    stored_candidates: usize,
    reused_exact_batch: bool,
    spool_write_ms: u128,
    spool_sync_ms: u128,
}

#[derive(Debug)]
struct SpoolReplayPlan {
    events: Vec<(StoredNostrEvent, Option<Cid>)>,
    missing_positions: Vec<usize>,
    reused_records: usize,
}

impl SpoolReplayPlan {
    async fn reuse_durable_candidates<S: Store>(
        &mut self,
        target: &NostrEventStore<S>,
        staged_cids: &[Cid],
    ) -> Result<usize> {
        if self.events.len() != staged_cids.len() {
            anyhow::bail!(
                "bulk durable replay event/CID length mismatch: events={} cids={}",
                self.events.len(),
                staged_cids.len()
            );
        }

        let candidates = self
            .events
            .iter()
            .zip(staged_cids)
            .enumerate()
            .filter_map(|(position, ((_, durable_cid), staged_cid))| {
                durable_cid
                    .is_none()
                    .then(|| (position, staged_cid.clone()))
            })
            .collect::<Vec<_>>();
        let loaded = target
            .try_load_event_blobs(candidates.iter().map(|(_, cid)| cid.clone()))
            .await
            .context("probe durable target event blobs for bulk replay")?;
        if loaded.len() != candidates.len() {
            anyhow::bail!(
                "bulk durable replay probe length mismatch: candidates={} loaded={}",
                candidates.len(),
                loaded.len()
            );
        }

        let mut reused = 0usize;
        for ((position, cid), durable_event) in candidates.into_iter().zip(loaded) {
            let Some(durable_event) = durable_event else {
                continue;
            };
            let staged_event = &self.events[position].0;
            if durable_event != *staged_event {
                anyhow::bail!(
                    "bulk replay payload differs from durable target blob for event {}",
                    staged_event.id
                );
            }
            self.events[position].1 = Some(cid);
            reused = reused.saturating_add(1);
        }
        self.missing_positions = self
            .events
            .iter()
            .enumerate()
            .filter_map(|(position, (_, cid))| cid.is_none().then_some(position))
            .collect();
        Ok(reused)
    }
}

struct BulkProjectionSpool {
    env: Env,
    entries: Database<Bytes, Bytes>,
    events: Database<Bytes, Bytes>,
    slots: Database<Bytes, Bytes>,
}

#[derive(Debug)]
struct EntryEdge {
    chunk: Vec<u8>,
    value: Vec<u8>,
}

#[derive(Debug)]
struct EntryTrieFrame {
    parent: [u8; 32],
    logical_prefix_len: usize,
    after: Option<Vec<u8>>,
    edges: VecDeque<EntryEdge>,
    exhausted: bool,
}

struct EntryTrieCursor<'a> {
    spool: &'a BulkProjectionSpool,
    index: NostrEventIndex,
    logical_key: Vec<u8>,
    stack: Vec<EntryTrieFrame>,
}

impl BulkProjectionSpool {
    fn open(path: &Path) -> Result<Self> {
        std::fs::create_dir_all(path)
            .with_context(|| format!("create bulk projection spool {}", path.display()))?;
        let mut options = EnvOpenOptions::new();
        options
            .map_size(BULK_PROJECTION_MAP_SIZE)
            .max_dbs(3)
            .max_readers(32);
        // This command owns one spool environment for its lifetime. The path
        // is private to the bulk projection, so no same-process opener can use
        // incompatible LMDB options.
        let env = unsafe { options.open(path) }
            .with_context(|| format!("open bulk projection spool {}", path.display()))?;
        let mut wtxn = env.write_txn()?;
        let entries = env.create_database(&mut wtxn, Some("entries"))?;
        let events = env.create_database(&mut wtxn, Some("events"))?;
        let slots = env.create_database(&mut wtxn, Some("slots"))?;
        wtxn.commit()?;
        Ok(Self {
            env,
            entries,
            events,
            slots,
        })
    }

    fn apply(&self, events: Vec<(StoredNostrEvent, Cid)>) -> Result<SpoolApplyReport> {
        let mut report = SpoolApplyReport::default();
        let write_started = Instant::now();
        let mut wtxn = self.env.write_txn()?;
        for (event, cid) in events {
            if let Some(existing) = self.events.get(&wtxn, event.id.as_bytes())? {
                let existing: SpoolEventRecord =
                    rmp_serde::from_slice(existing).context("decode duplicate spool event")?;
                if existing.event != event {
                    anyhow::bail!(
                        "bulk replay payload differs from duplicate spool record for event {}",
                        event.id
                    );
                }
                if existing.cid_hash != cid.hash || existing.cid_key != cid.key {
                    anyhow::bail!(
                        "bulk replay CID differs from duplicate spool record for event {}",
                        event.id
                    );
                }
                report.skipped = report.skipped.saturating_add(1);
                // A crash can occur after the spool commit but before the
                // profile index and JSON cursor are durable. Returning the
                // retained event makes replay complete that idempotent work.
                report.retained_events.push(existing.event);
                continue;
            }

            let mut replaced = false;
            let slot = nostr_replaceable_slot(&event);
            if let Some((index, slot_key)) = slot.as_ref() {
                let encoded_slot = hashed_key(*index, slot_key);
                if let Some(previous_id) = self.slots.get(&wtxn, &encoded_slot)? {
                    let previous = self
                        .events
                        .get(&wtxn, previous_id)?
                        .context("replaceable spool slot referenced a missing event")?;
                    let previous: SpoolEventRecord = rmp_serde::from_slice(previous)
                        .context("decode replaceable spool event")?;
                    if compare_nostr_replaceable_events(&event, &previous.event)
                        != Ordering::Greater
                    {
                        report.skipped = report.skipped.saturating_add(1);
                        continue;
                    }
                    self.remove_event(&mut wtxn, &previous)?;
                    replaced = true;
                }
            }

            let record = SpoolEventRecord {
                event: event.clone(),
                cid_hash: cid.hash,
                cid_key: cid.key,
            };
            let encoded = rmp_serde::to_vec_named(&record).context("encode spool event")?;
            self.events.put(&mut wtxn, event.id.as_bytes(), &encoded)?;
            for entry in nostr_event_index_entries(&event, &cid) {
                self.put_entry(&mut wtxn, entry.index, &entry.key, &entry.cid)?;
                report.index_entries = report.index_entries.saturating_add(1);
            }
            if let Some((index, slot_key)) = slot {
                self.slots.put(
                    &mut wtxn,
                    &hashed_key(index, &slot_key),
                    event.id.as_bytes(),
                )?;
            }
            if replaced {
                report.replaced = report.replaced.saturating_add(1);
            } else {
                report.inserted = report.inserted.saturating_add(1);
            }
            report.retained_events.push(event);
        }
        wtxn.commit()?;
        report.spool_write_ms = write_started.elapsed().as_millis();
        let sync_started = Instant::now();
        self.env.force_sync()?;
        report.spool_sync_ms = sync_started.elapsed().as_millis();
        Ok(report)
    }

    fn plan_replay_batch(
        &self,
        events: Vec<StoredNostrEvent>,
        staged_cids: &[Cid],
    ) -> Result<SpoolReplayPlan> {
        // A spool record is committed only after the target event blob was
        // force-synced. During crash replay, a complete matching batch can
        // therefore reuse those durable target CIDs without revalidating and
        // rewriting every blob. Missing events still pass through the original
        // store/apply path, and apply receives the complete original sequence
        // so replaceable-event ordering remains unchanged.
        if events.len() != staged_cids.len() {
            anyhow::bail!(
                "bulk replay event/CID length mismatch: events={} cids={}",
                events.len(),
                staged_cids.len()
            );
        }

        let event_count = events.len();
        let mut lookup = events
            .into_iter()
            .zip(staged_cids.iter().cloned())
            .enumerate()
            .collect::<Vec<_>>();
        // The event spool is keyed lexicographically by event id. Staged
        // segments arrive in author/time order, which otherwise turns a large
        // replay page into tens of thousands of random LMDB page faults.
        // Sorting only the read schedule retains the original sequence below.
        lookup.sort_unstable_by(|(_, (left, _)), (_, (right, _))| left.id.cmp(&right.id));

        let rtxn = self.env.read_txn()?;
        let mut planned = (0..event_count).map(|_| None).collect::<Vec<_>>();
        let mut reused_records = 0usize;
        for (position, (event, staged_cid)) in lookup {
            let Some(encoded) = self.events.get(&rtxn, event.id.as_bytes())? else {
                planned[position] = Some((event, None));
                continue;
            };
            let existing: SpoolEventRecord =
                rmp_serde::from_slice(encoded).context("decode replayed spool event")?;
            if existing.event != event {
                anyhow::bail!(
                    "bulk replay payload differs from durable spool record for event {}",
                    event.id
                );
            }
            let existing_cid = Cid {
                hash: existing.cid_hash,
                key: existing.cid_key,
            };
            if existing_cid != staged_cid {
                anyhow::bail!(
                    "bulk replay CID differs from durable spool record for event {}",
                    event.id
                );
            }
            reused_records = reused_records.saturating_add(1);
            planned[position] = Some((event, Some(existing_cid)));
        }
        let planned = planned
            .into_iter()
            .map(|event| event.context("bulk replay read schedule omitted an event"))
            .collect::<Result<Vec<_>>>()?;
        let missing_positions = planned
            .iter()
            .enumerate()
            .filter_map(|(position, (_, cid))| cid.is_none().then_some(position))
            .collect();
        Ok(SpoolReplayPlan {
            events: planned,
            missing_positions,
            reused_records,
        })
    }

    fn remove_event(&self, wtxn: &mut heed::RwTxn<'_>, record: &SpoolEventRecord) -> Result<()> {
        let cid = Cid {
            hash: record.cid_hash,
            key: record.cid_key,
        };
        for entry in nostr_event_index_entries(&record.event, &cid) {
            self.remove_entry(wtxn, entry.index, &entry.key)?;
        }
        self.events.delete(wtxn, record.event.id.as_bytes())?;
        Ok(())
    }

    fn put_entry(
        &self,
        wtxn: &mut heed::RwTxn<'_>,
        index: NostrEventIndex,
        logical_key: &str,
        cid: &Cid,
    ) -> Result<()> {
        let bytes = logical_key.as_bytes();
        let mut parent = [0; 32];
        let mut offset: usize = 0;
        loop {
            let end = offset.saturating_add(ENTRY_CHUNK_SIZE).min(bytes.len());
            let chunk = &bytes[offset..end];
            let final_chunk = end == bytes.len();
            let physical_key = entry_edge_key(index, &parent, chunk);
            let mut edge = self
                .entries
                .get(wtxn, &physical_key)?
                .map(decode_edge_value)
                .transpose()?
                .unwrap_or_default();
            if final_chunk {
                edge.cid = Some(cid.clone());
            } else {
                edge.has_children = true;
            }
            self.entries
                .put(wtxn, &physical_key, &encode_edge_value(&edge))?;
            if final_chunk {
                return Ok(());
            }
            parent = sha256(&physical_key);
            offset = end;
        }
    }

    fn remove_entry(
        &self,
        wtxn: &mut heed::RwTxn<'_>,
        index: NostrEventIndex,
        logical_key: &str,
    ) -> Result<()> {
        let bytes = logical_key.as_bytes();
        let mut parent = [0; 32];
        let mut offset: usize = 0;
        loop {
            let end = offset.saturating_add(ENTRY_CHUNK_SIZE).min(bytes.len());
            let chunk = &bytes[offset..end];
            let final_chunk = end == bytes.len();
            let physical_key = entry_edge_key(index, &parent, chunk);
            if final_chunk {
                let Some(encoded) = self.entries.get(wtxn, &physical_key)? else {
                    return Ok(());
                };
                let mut edge = decode_edge_value(encoded)?;
                if edge.has_children {
                    edge.cid = None;
                    self.entries
                        .put(wtxn, &physical_key, &encode_edge_value(&edge))?;
                } else {
                    self.entries.delete(wtxn, &physical_key)?;
                }
                return Ok(());
            }
            parent = sha256(&physical_key);
            offset = end;
        }
    }

    fn load_edge_page(
        &self,
        index: NostrEventIndex,
        parent: &[u8; 32],
        after: Option<&[u8]>,
    ) -> Result<(VecDeque<EntryEdge>, bool)> {
        let prefix = entry_edge_prefix(index, parent);
        let start = match after {
            Some(key) => Bound::Excluded(key),
            None => Bound::Included(prefix.as_slice()),
        };
        let bounds = (start, Bound::Unbounded);
        let rtxn = self.env.read_txn()?;
        let mut edges = VecDeque::new();
        let mut exhausted = false;
        for item in self.entries.range(&rtxn, &bounds)? {
            let (key, value) = item?;
            if !key.starts_with(&prefix) {
                exhausted = true;
                break;
            }
            edges.push_back(EntryEdge {
                chunk: key[ENTRY_PREFIX_SIZE..].to_vec(),
                value: value.to_vec(),
            });
            if edges.len() == ENTRY_PAGE_SIZE {
                break;
            }
        }
        if edges.len() < ENTRY_PAGE_SIZE {
            exhausted = true;
        }
        Ok((edges, exhausted))
    }

    async fn build_index_root<S: Store>(
        &self,
        index: NostrEventIndex,
        store: Arc<S>,
        order: usize,
    ) -> Result<Option<Cid>> {
        let btree = BTree::new(store, BTreeOptions { order: Some(order) });
        let mut builder = btree.sorted_link_builder();
        let mut cursor = EntryTrieCursor::new(self, index);
        while let Some((key, cid)) = cursor.next_entry()? {
            builder.push(key, cid).await?;
        }
        Ok(builder.finish().await?)
    }
}

impl EntryTrieCursor<'_> {
    fn new(spool: &BulkProjectionSpool, index: NostrEventIndex) -> EntryTrieCursor<'_> {
        EntryTrieCursor {
            spool,
            index,
            logical_key: Vec::new(),
            stack: vec![EntryTrieFrame {
                parent: [0; 32],
                logical_prefix_len: 0,
                after: None,
                edges: VecDeque::new(),
                exhausted: false,
            }],
        }
    }

    fn next_entry(&mut self) -> Result<Option<(String, Cid)>> {
        loop {
            let Some(frame) = self.stack.last_mut() else {
                return Ok(None);
            };
            if frame.edges.is_empty() && !frame.exhausted {
                let (edges, exhausted) =
                    self.spool
                        .load_edge_page(self.index, &frame.parent, frame.after.as_deref())?;
                frame.edges = edges;
                frame.exhausted = exhausted;
            }
            let Some(edge) = frame.edges.pop_front() else {
                let prefix_len = frame.logical_prefix_len;
                self.stack.pop();
                self.logical_key.truncate(prefix_len);
                continue;
            };
            self.logical_key.truncate(frame.logical_prefix_len);
            self.logical_key.extend_from_slice(&edge.chunk);
            let physical_key = entry_edge_key(self.index, &frame.parent, &edge.chunk);
            frame.after = Some(physical_key.clone());
            let value = decode_edge_value(&edge.value)?;
            if value.has_children {
                self.stack.push(EntryTrieFrame {
                    parent: sha256(&physical_key),
                    logical_prefix_len: self.logical_key.len(),
                    after: None,
                    edges: VecDeque::new(),
                    exhausted: false,
                });
            }
            if let Some(cid) = value.cid {
                let key = String::from_utf8(self.logical_key.clone())
                    .context("bulk projection index key is not UTF-8")?;
                return Ok(Some((key, cid)));
            }
        }
    }
}

#[derive(Default)]
struct EdgeValue {
    cid: Option<Cid>,
    has_children: bool,
}

fn entry_edge_prefix(index: NostrEventIndex, parent: &[u8; 32]) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(ENTRY_PREFIX_SIZE);
    encoded.push(index.stable_id());
    encoded.extend_from_slice(parent);
    encoded
}

fn entry_edge_key(index: NostrEventIndex, parent: &[u8; 32], chunk: &[u8]) -> Vec<u8> {
    let mut encoded = entry_edge_prefix(index, parent);
    encoded.extend_from_slice(chunk);
    encoded
}

fn hashed_key(index: NostrEventIndex, key: &str) -> Vec<u8> {
    let mut logical = Vec::with_capacity(key.len() + 1);
    logical.push(index.stable_id());
    logical.extend_from_slice(key.as_bytes());
    let mut encoded = Vec::with_capacity(33);
    encoded.push(index.stable_id());
    encoded.extend_from_slice(&sha256(&logical));
    encoded
}

fn encode_edge_value(value: &EdgeValue) -> Vec<u8> {
    let mut flags = 0;
    if value.cid.is_some() {
        flags |= EDGE_HAS_CID;
    }
    if value.has_children {
        flags |= EDGE_HAS_CHILDREN;
    }
    let mut encoded = vec![flags];
    if let Some(cid) = value.cid.as_ref() {
        encoded.extend_from_slice(&encode_cid(cid));
    }
    encoded
}

fn decode_edge_value(encoded: &[u8]) -> Result<EdgeValue> {
    let (&flags, cid) = encoded
        .split_first()
        .context("bulk projection edge value is empty")?;
    if flags & !(EDGE_HAS_CID | EDGE_HAS_CHILDREN) != 0 {
        anyhow::bail!("invalid bulk projection edge flags {flags}");
    }
    let cid = if flags & EDGE_HAS_CID != 0 {
        Some(decode_cid(cid)?)
    } else {
        if !cid.is_empty() {
            anyhow::bail!("bulk projection edge without CID has trailing bytes");
        }
        None
    };
    Ok(EdgeValue {
        cid,
        has_children: flags & EDGE_HAS_CHILDREN != 0,
    })
}

fn encode_cid(cid: &Cid) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(if cid.key.is_some() { 65 } else { 33 });
    encoded.push(u8::from(cid.key.is_some()));
    encoded.extend_from_slice(&cid.hash);
    if let Some(key) = cid.key {
        encoded.extend_from_slice(&key);
    }
    encoded
}

fn decode_cid(encoded: &[u8]) -> Result<Cid> {
    let (encrypted, rest) = encoded
        .split_first()
        .context("bulk projection CID value is empty")?;
    let hash: [u8; 32] = rest
        .get(..32)
        .context("bulk projection CID hash is truncated")?
        .try_into()
        .expect("CID hash slice is exactly 32 bytes");
    let key = match *encrypted {
        0 if rest.len() == 32 => None,
        1 if rest.len() == 64 => Some(
            rest[32..]
                .try_into()
                .expect("CID key slice is exactly 32 bytes"),
        ),
        marker => anyhow::bail!(
            "invalid bulk projection CID encoding marker={marker} length={}",
            encoded.len()
        ),
    };
    Ok(Cid { hash, key })
}

fn bulk_paths(data_dir: &Path) -> (PathBuf, PathBuf) {
    let root = data_dir.join(INDEX_DIR).join(BULK_PROJECTION_DIR);
    (
        root.join(BULK_PROJECTION_STATE_FILE),
        root.join(BULK_PROJECTION_SPOOL_DIR),
    )
}

fn rejected_spool_edge_state_path(data_dir: &Path) -> PathBuf {
    data_dir
        .join(INDEX_DIR)
        .join(BULK_PROJECTION_DIR)
        .join(REJECTED_SPOOL_EDGE_STATE_FILE)
}

fn reject_spool_edge_state_marker(data_dir: &Path) -> Result<()> {
    let path = rejected_spool_edge_state_path(data_dir);
    match std::fs::symlink_metadata(&path) {
        Ok(_) => anyhow::bail!(
            "rejected bulk projection fast-forward marker is present at {}; \
             remove it and resume exact spool replay",
            path.display()
        ),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err)
            .with_context(|| format!("inspect rejected spool edge marker {}", path.display())),
    }
}

fn load_bulk_state(path: &Path) -> Result<Option<BulkProjectionState>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
            format!("parse bulk projection state {}", path.display())
        })?)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => {
            Err(err).with_context(|| format!("read bulk projection state {}", path.display()))
        }
    }
}

fn persist_bulk_state(path: &Path, state: &BulkProjectionState) -> Result<()> {
    persist_json_atomic(path, state, "bulk projection state")
}

pub(super) async fn project_staged_allowlist_bulk(
    stores: ProjectionStores<'_>,
    data_dir: &Path,
    staging_data_dir: &Path,
    options: &SocialGraphIndexOptions,
    authors: &[String],
    policy: &IndexedNostrCrawlPolicy,
    crawl_state: &mut IndexedNostrCrawlState,
) -> Result<CrawlReport> {
    if policy.base_root.is_some() {
        anyhow::bail!("bulk staged projection requires a fresh crawl policy without a base root");
    }

    let (state_path, spool_path) = bulk_paths(data_dir);
    reject_spool_edge_state_marker(data_dir)?;
    let mut state = load_bulk_state(&state_path)?.unwrap_or_else(|| BulkProjectionState {
        version: BULK_PROJECTION_VERSION,
        author_allowlist_source: options.author_allowlist_url.clone(),
        policy: policy.clone(),
        next_author: 0,
        segment_event_offset: 0,
        events_seen: 0,
        events_selected: 0,
        live_bytes_selected: 0,
        built_roots: BTreeMap::new(),
        complete_root: None,
    });
    if state.version != BULK_PROJECTION_VERSION || state.policy != *policy {
        anyhow::bail!("bulk projection state does not match the requested crawl policy");
    }
    let spool = BulkProjectionSpool::open(&spool_path)?;
    let target_event_store = NostrEventStore::with_options(
        stores.durable.store_arc(),
        super::nostr_event_store_options(options),
    );
    let staging_event_store = NostrEventStore::with_options(
        stores.staging.store_arc(),
        super::nostr_event_store_options(options),
    );

    if let Some(root) = state.complete_root.as_deref() {
        let root = parse_root_text(root).context("parse completed bulk projection root")?;
        validate_reachable_root(&target_event_store, Some(&root), "bulk projection root").await?;
        install_crawl_state(crawl_state, &state);
        persist_crawl_state(data_dir, crawl_state)?;
        return Ok(report_from_state(&state, authors.len(), Some(root)));
    }

    loop {
        let stage = load_stage_state(staging_data_dir)?.context(
            "no durable Nostr staging state exists; start --stage-only before projection",
        )?;
        validate_stage_state(&stage, policy, authors.len())?;
        if state.next_author >= stage.next_author {
            if state.next_author >= authors.len() || !options.projection_follow {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            continue;
        }
        if !state.built_roots.is_empty() {
            anyhow::bail!("bulk projection cannot append after final index construction started");
        }
        let started = Instant::now();
        let segment = load_stage_segment(staging_data_dir, state.next_author)?;
        if state.segment_event_offset > segment.event_cids.len() {
            anyhow::bail!("bulk projection offset exceeds staged segment length");
        }
        let chunk_size = options.index_commit_batch_size.max(1);
        let event_end = state
            .segment_event_offset
            .saturating_add(chunk_size)
            .min(segment.event_cids.len());
        let cids = segment.event_cids[state.segment_event_offset..event_end]
            .iter()
            .map(|root| parse_root_text(root))
            .collect::<Result<Vec<_>>>()?;
        let stage_load_started = Instant::now();
        let staged_blobs = staging_event_store
            .load_validated_event_blobs(cids.clone())
            .await
            .context("load staged events for bulk projection")?;
        let events = staged_blobs
            .iter()
            .map(|blob| blob.event().clone())
            .collect::<Vec<_>>();
        let stage_load_ms = stage_load_started.elapsed().as_millis();
        let replay_plan_started = Instant::now();
        let mut plan = spool.plan_replay_batch(events, &cids)?;
        let replay_plan_ms = replay_plan_started.elapsed().as_millis();
        let spool_missing_candidates = plan.missing_positions.len();

        // During crash catch-up, a batch can contain a mostly committed spool
        // page plus a few event blobs that were force-synced just before the
        // old process stopped. Prove those exceptional CIDs through the
        // durable target's full HashTree read path before reusing them. Do not
        // probe wholly new pages: the staging and durable pools are distinct,
        // and thousands of expected misses would only slow normal recovery.
        let durable_probe_started = Instant::now();
        let durable_reused_candidates =
            if plan.reused_records > 0 && !plan.missing_positions.is_empty() {
                plan.reuse_durable_candidates(&target_event_store, &cids)
                    .await?
            } else {
                0
            };
        let durable_probe_ms = durable_probe_started.elapsed().as_millis();
        let stored_candidates = plan.missing_positions.len();
        let mut target_store_ms = 0u128;
        let mut target_sync_ms = 0u128;
        let mut apply = if spool_missing_candidates == 0 {
            SpoolApplyReport {
                retained_events: plan.events.into_iter().map(|(event, _)| event).collect(),
                skipped: plan.reused_records,
                reused_records: plan.reused_records,
                reused_exact_batch: true,
                ..SpoolApplyReport::default()
            }
        } else {
            let target_cids = if stored_candidates == 0 {
                Vec::new()
            } else {
                let target_store_started = Instant::now();
                let target_cids = target_event_store
                    .store_validated_event_blobs(
                        plan.events
                            .iter()
                            .zip(&staged_blobs)
                            .filter_map(|((_, cid), blob)| cid.is_none().then_some(blob)),
                    )
                    .await
                    .context("copy bulk-projected event blobs")?;
                target_store_ms = target_store_started.elapsed().as_millis();
                let target_sync_started = Instant::now();
                stores
                    .durable
                    .force_sync()
                    .context("force-sync bulk-projected event blobs")?;
                target_sync_ms = target_sync_started.elapsed().as_millis();
                target_cids
            };
            let mut target_cids = target_cids.into_iter();
            let events = plan
                .events
                .into_iter()
                .map(|(event, existing_cid)| {
                    let cid = existing_cid
                        .or_else(|| target_cids.next())
                        .context("bulk replay omitted a stored event CID")?;
                    Ok((event, cid))
                })
                .collect::<Result<Vec<_>>>()?;
            if target_cids.next().is_some() {
                anyhow::bail!("bulk replay produced excess stored event CIDs");
            }
            spool.apply(events)?
        };
        apply.reused_records = plan.reused_records;
        apply.durable_reused_candidates = durable_reused_candidates;
        apply.stored_candidates = stored_candidates;
        let profile_sync_started = Instant::now();
        let profile_events = apply
            .retained_events
            .iter()
            .filter(|event| event.kind == 0)
            .map(|event| event.to_nostr_sdk_event().map_err(anyhow::Error::from))
            .collect::<Result<Vec<_>>>()?;
        if !profile_events.is_empty() {
            stores
                .graph
                .sync_profile_index_for_events(&profile_events)
                .context("sync bulk-projected profile events")?;
            stores.graph.force_sync()?;
        }
        let profile_sync_ms = profile_sync_started.elapsed().as_millis();

        let completed_segment = event_end == segment.event_cids.len();
        if completed_segment {
            state.next_author = segment.end_author;
            state.segment_event_offset = 0;
            state.events_seen = state.events_seen.saturating_add(segment.events_seen);
            state.events_selected = state
                .events_selected
                .saturating_add(segment.events_selected);
            state.live_bytes_selected = state
                .live_bytes_selected
                .saturating_add(segment.live_bytes_selected);
        } else {
            state.segment_event_offset = event_end;
        }
        let state_persist_started = Instant::now();
        persist_bulk_state(&state_path, &state)?;
        let state_persist_ms = state_persist_started.elapsed().as_millis();
        eprintln!(
            "Nostr bulk spool checkpoint: authors={}/{} staged_authors={} segment_event_offset={}/{} retained={} replaced={} skipped={} index_entries={} reused_records={} spool_missing_candidates={} durable_reused_candidates={} stored_candidates={} reused_exact_batch={} completed_segment={} stage_load_ms={} replay_plan_ms={} durable_probe_ms={} target_store_ms={} target_sync_ms={} spool_write_ms={} spool_sync_ms={} profile_sync_ms={} state_persist_ms={} batch_elapsed_ms={}",
            state.next_author,
            authors.len(),
            stage.next_author,
            state.segment_event_offset,
            segment.event_cids.len(),
            apply.inserted,
            apply.replaced,
            apply.skipped,
            apply.index_entries,
            apply.reused_records,
            spool_missing_candidates,
            apply.durable_reused_candidates,
            apply.stored_candidates,
            apply.reused_exact_batch,
            completed_segment,
            stage_load_ms,
            replay_plan_ms,
            durable_probe_ms,
            target_store_ms,
            target_sync_ms,
            apply.spool_write_ms,
            apply.spool_sync_ms,
            profile_sync_ms,
            state_persist_ms,
            started.elapsed().as_millis()
        );
    }

    for index in NostrEventIndex::ALL {
        if state.built_roots.contains_key(&index.stable_id()) {
            continue;
        }
        let started = Instant::now();
        let root = spool
            .build_index_root(index, stores.durable.store_arc(), options.btree_order)
            .await
            .with_context(|| format!("bulk-build {} index", index.name()))?;
        stores
            .durable
            .force_sync()
            .with_context(|| format!("force-sync {} bulk index", index.name()))?;
        state.built_roots.insert(
            index.stable_id(),
            root.as_ref()
                .map(cid_to_nhash)
                .transpose()?
                .unwrap_or_default(),
        );
        persist_bulk_state(&state_path, &state)?;
        eprintln!(
            "Nostr bulk index complete: index={} elapsed_ms={}",
            index.name(),
            started.elapsed().as_millis()
        );
    }

    let roots = NostrEventIndex::ALL
        .into_iter()
        .map(|index| {
            let encoded = state
                .built_roots
                .get(&index.stable_id())
                .context("bulk projection state omitted a built index")?;
            let root = (!encoded.is_empty())
                .then(|| parse_root_text(encoded))
                .transpose()?;
            Ok((index, root))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let root = target_event_store
        .write_bulk_index_manifest(&roots)
        .await
        .context("write bulk Nostr index manifest")?
        .context("bulk Nostr index manifest was empty")?;
    stores
        .durable
        .force_sync()
        .context("force-sync bulk Nostr index manifest")?;
    validate_reachable_root(&target_event_store, Some(&root), "bulk projection root").await?;
    state.complete_root = Some(cid_to_nhash(&root)?);
    persist_bulk_state(&state_path, &state)?;
    install_crawl_state(crawl_state, &state);
    persist_crawl_state(data_dir, crawl_state)?;
    Ok(report_from_state(&state, authors.len(), Some(root)))
}

fn install_crawl_state(target: &mut IndexedNostrCrawlState, source: &BulkProjectionState) {
    *target = IndexedNostrCrawlState {
        version: CRAWL_STATE_VERSION,
        author_allowlist_source: source.author_allowlist_source.clone(),
        policy: source.policy.clone(),
        next_author: source.next_author,
        staged_segment_event_offset: source.segment_event_offset,
        root: source.complete_root.clone(),
        events_seen: source.events_seen,
        events_selected: source.events_selected,
        live_bytes_selected: source.live_bytes_selected,
    };
}

fn report_from_state(
    state: &BulkProjectionState,
    author_count: usize,
    root: Option<Cid>,
) -> CrawlReport {
    CrawlReport {
        root,
        authors_considered: author_count,
        authors_processed: state.next_author,
        events_seen: state.events_seen,
        events_selected: state.events_selected,
        live_bytes_selected: state.live_bytes_selected,
        ..CrawlReport::default()
    }
}

#[cfg(test)]
mod tests;
