use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use hashtree_core::{Cid, Store};
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

const BULK_PROJECTION_VERSION: u32 = 1;
const BULK_PROJECTION_DIR: &str = "bulk-projection-v1";
const BULK_PROJECTION_STATE_FILE: &str = "state.json";
const BULK_PROJECTION_SPOOL_DIR: &str = "spool";
const BULK_PROJECTION_MAP_SIZE: usize = 256 * 1024 * 1024 * 1024;

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
}

struct BulkProjectionSpool {
    env: Env,
    entries: Database<Bytes, Bytes>,
    events: Database<Bytes, Bytes>,
    slots: Database<Bytes, Bytes>,
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
        let mut wtxn = self.env.write_txn()?;
        for (event, cid) in events {
            if let Some(existing) = self.events.get(&wtxn, event.id.as_bytes())? {
                let existing: SpoolEventRecord =
                    rmp_serde::from_slice(existing).context("decode duplicate spool event")?;
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
                let encoded_slot = prefixed_key(*index, slot_key);
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
                self.entries.put(
                    &mut wtxn,
                    &prefixed_key(entry.index, &entry.key),
                    &encode_cid(&entry.cid),
                )?;
                report.index_entries = report.index_entries.saturating_add(1);
            }
            if let Some((index, slot_key)) = slot {
                self.slots.put(
                    &mut wtxn,
                    &prefixed_key(index, &slot_key),
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
        self.env.force_sync()?;
        Ok(report)
    }

    fn remove_event(&self, wtxn: &mut heed::RwTxn<'_>, record: &SpoolEventRecord) -> Result<()> {
        let cid = Cid {
            hash: record.cid_hash,
            key: record.cid_key,
        };
        for entry in nostr_event_index_entries(&record.event, &cid) {
            self.entries
                .delete(wtxn, &prefixed_key(entry.index, &entry.key))?;
        }
        self.events.delete(wtxn, record.event.id.as_bytes())?;
        Ok(())
    }

    async fn build_index_root<S: Store>(
        &self,
        index: NostrEventIndex,
        store: Arc<S>,
        order: usize,
    ) -> Result<Option<Cid>> {
        let btree = BTree::new(store, BTreeOptions { order: Some(order) });
        let mut builder = btree.sorted_link_builder();
        let rtxn = self.env.read_txn()?;
        let prefix = [index.stable_id()];
        for item in self.entries.prefix_iter(&rtxn, &prefix)? {
            let (key, value) = item?;
            let key = std::str::from_utf8(&key[1..])
                .context("bulk projection index key is not UTF-8")?
                .to_owned();
            builder.push(key, decode_cid(value)?).await?;
        }
        Ok(builder.finish().await?)
    }
}

fn prefixed_key(index: NostrEventIndex, key: &str) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(key.len() + 1);
    encoded.push(index.stable_id());
    encoded.extend_from_slice(key.as_bytes());
    encoded
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
        let events = staging_event_store
            .load_event_blobs(cids)
            .await
            .context("load staged events for bulk projection")?;
        let target_cids = target_event_store
            .store_event_blobs(events.clone())
            .await
            .context("store bulk-projected event blobs")?;
        stores
            .durable
            .force_sync()
            .context("force-sync bulk-projected event blobs")?;
        let apply = spool.apply(events.into_iter().zip(target_cids).collect())?;
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
        persist_bulk_state(&state_path, &state)?;
        eprintln!(
            "Nostr bulk spool checkpoint: authors={}/{} staged_authors={} segment_event_offset={}/{} retained={} replaced={} skipped={} index_entries={} completed_segment={} batch_elapsed_ms={}",
            state.next_author,
            authors.len(),
            stage.next_author,
            state.segment_event_offset,
            segment.event_cids.len(),
            apply.inserted,
            apply.replaced,
            apply.skipped,
            apply.index_entries,
            completed_segment,
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
