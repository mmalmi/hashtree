//! Bounded, deterministic comparison of the production append/index path.
//! Timings exclude signing, seed construction and verification. Byte counts are
//! retained content-addressed payloads, not LMDB allocation or filesystem I/O.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use futures::executor::block_on;
use hashtree_core::{Cid, Hash, MemoryStore, Store, StoreError};
use hashtree_index::{BTree, BTreeOptions};
use hashtree_nostr::{
    stored_event_from_nostr_sdk_event, NostrEventStore, NostrEventStoreOptions, StoredNostrEvent,
};
use nostr::secp256k1::rand::{rngs::StdRng, SeedableRng};
use nostr::{EventBuilder, Keys, Kind, Tag, Timestamp, SECP256K1};

#[derive(Default)]
struct Metrics {
    gets: AtomicU64,
    flushes: AtomicU64,
    attempted_blobs: AtomicU64,
    attempted_bytes: AtomicU64,
    inserted_blobs: AtomicU64,
    inserted_bytes: AtomicU64,
}

// Every run shares the same immutable historical seed and owns only its new
// blobs. This keeps the fixture bounded while retaining all intermediate roots,
// exactly as catch-up does. The actual index/B-tree algorithms are unmodified.
struct RecordingStore {
    historical: Arc<MemoryStore>,
    appended: MemoryStore,
    metrics: Metrics,
    fail_flush_on: AtomicU64,
}

impl RecordingStore {
    fn new(historical: Arc<MemoryStore>) -> Self {
        Self {
            historical,
            appended: MemoryStore::new(),
            metrics: Metrics::default(),
            fail_flush_on: AtomicU64::new(0),
        }
    }
}

#[async_trait]
impl Store for RecordingStore {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        self.metrics.attempted_blobs.fetch_add(1, Ordering::Relaxed);
        let bytes = data.len() as u64;
        self.metrics
            .attempted_bytes
            .fetch_add(bytes, Ordering::Relaxed);
        if self.historical.has(&hash).await? {
            return Ok(false);
        }
        let inserted = self.appended.put(hash, data).await?;
        if inserted {
            self.metrics.inserted_blobs.fetch_add(1, Ordering::Relaxed);
            self.metrics
                .inserted_bytes
                .fetch_add(bytes, Ordering::Relaxed);
        }
        Ok(inserted)
    }

    async fn put_many_optimistic(&self, items: Vec<(Hash, Vec<u8>)>) -> Result<usize, StoreError> {
        let ordinal = self.metrics.flushes.fetch_add(1, Ordering::Relaxed) + 1;
        if self.fail_flush_on.load(Ordering::Relaxed) == ordinal {
            return Err(StoreError::Other("bounded write refusal".into()));
        }
        self.put_many(items).await
    }

    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        self.metrics.gets.fetch_add(1, Ordering::Relaxed);
        match self.appended.get(hash).await? {
            Some(bytes) => Ok(Some(bytes)),
            None => self.historical.get(hash).await,
        }
    }

    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        Ok(self.appended.has(hash).await? || self.historical.has(hash).await?)
    }

    async fn delete(&self, _: &Hash) -> Result<bool, StoreError> {
        panic!("append benchmark must never delete retained history")
    }
}

fn signed_event(
    keys: &Keys,
    timestamp: u64,
    kind: u16,
    tags: Vec<Vec<String>>,
    content: String,
) -> StoredNostrEvent {
    // Public synthetic fixture keys and seeded signing randomness make every
    // event (including its valid signature) reproducible across executions.
    let signed = EventBuilder::new(Kind::from(kind), content)
        .tags(tags.into_iter().map(|tag| Tag::parse(tag).unwrap()))
        .custom_created_at(Timestamp::from_secs(timestamp))
        .build(keys.public_key())
        .sign_with_ctx(SECP256K1, &mut StdRng::seed_from_u64(timestamp), keys)
        .unwrap();
    signed.verify().unwrap();
    stored_event_from_nostr_sdk_event(&signed)
}

fn hot_tags(index: usize) -> Vec<Vec<String>> {
    vec![
        vec!["t".into(), "shared-topic".into()],
        vec!["t".into(), format!("topic-{}", index % 8)],
        vec!["e".into(), format!("{:064x}", index % 64 + 1)],
    ]
}

fn fixture() -> (Vec<StoredNostrEvent>, Vec<StoredNostrEvent>) {
    let keys = (1..=16)
        .map(|n| Keys::parse(&format!("{n:064x}")).unwrap())
        .collect::<Vec<_>>();
    let mut historical = (0..2048)
        .map(|i| {
            signed_event(
                &keys[i % 16],
                1000 + i as u64,
                1,
                hot_tags(i),
                format!("old {i}"),
            )
        })
        .collect::<Vec<_>>();
    historical.push(signed_event(
        &keys[0],
        4000,
        0,
        vec![],
        "old profile".into(),
    ));
    historical.push(signed_event(
        &keys[0],
        4001,
        3,
        (0..512)
            .map(|i| vec!["p".into(), format!("{:064x}", i + 1)])
            .collect(),
        "old contacts".into(),
    ));
    historical.push(signed_event(
        &keys[0],
        4002,
        30023,
        vec![vec!["d".into(), "article".into()]],
        "old article".into(),
    ));
    // One author, overlapping global/time/tag paths, plus wide replaceable tag
    // removal/addition. Interleaving historical timestamps exercises real merge
    // work rather than only appending to the edge of every chronological tree.
    let mut incoming = (0..1536)
        .map(|i| {
            signed_event(
                &keys[0],
                2000 + i as u64,
                1,
                hot_tags(i),
                format!("new {i}"),
            )
        })
        .collect::<Vec<_>>();
    incoming.push(signed_event(
        &keys[0],
        5000,
        0,
        vec![],
        "new profile".into(),
    ));
    incoming.push(signed_event(
        &keys[0],
        5001,
        3,
        (256..768)
            .map(|i| vec!["p".into(), format!("{:064x}", i + 1)])
            .collect(),
        "new contacts".into(),
    ));
    incoming.push(signed_event(
        &keys[0],
        5002,
        30023,
        vec![vec!["d".into(), "article".into()]],
        "new article".into(),
    ));
    incoming.push(signed_event(
        &keys[0],
        5003,
        5,
        vec![vec!["e".into(), historical[0].id.clone()]],
        "tombstone".into(),
    ));
    incoming.extend(historical.iter().take(32).cloned());
    incoming.push(incoming[0].clone());
    (historical, incoming)
}

async fn projections<S: Store>(
    store: Arc<S>,
    root: &Cid,
) -> BTreeMap<&'static str, Vec<(String, Cid)>> {
    let manifest = NostrEventStore::new(Arc::clone(&store))
        .get_manifest(Some(root))
        .await
        .unwrap();
    let tree = BTree::new(store, BTreeOptions::default());
    let mut result = BTreeMap::new();
    for (name, root) in [
        ("by-id", manifest.by_id),
        ("by-author-time", manifest.by_author_time),
        ("by-author-kind-time", manifest.by_author_kind_time),
        ("by-kind-time", manifest.by_kind_time),
        ("by-kind-time-author", manifest.by_kind_time_author),
        ("by-time", manifest.by_time),
        ("by-tag", manifest.by_tag),
        ("replaceable", manifest.replaceable),
        (
            "parameterized-replaceable",
            manifest.parameterized_replaceable,
        ),
    ] {
        result.insert(name, tree.links_entries(root.as_ref()).await.unwrap());
    }
    result
}

#[test]
fn catchup_append_fanout_and_commit_matrix_preserves_every_projection() {
    block_on(async {
        let (historical, incoming) = fixture();
        let backing = Arc::new(MemoryStore::new());
        let seed = NostrEventStore::with_options(
            Arc::clone(&backing),
            NostrEventStoreOptions {
                btree_order: Some(64),
                index_commit_batch_size: None,
                ..Default::default()
            },
        );
        let previous = seed.build(None, historical.clone()).await.unwrap().unwrap();
        let old_projections = projections(Arc::clone(&backing), &previous).await;
        let mut expected_old = historical.clone();
        expected_old.sort_by(|a, b| a.id.cmp(&b.id));
        // An independent rebuild establishes exact logical key/CID semantics,
        // including replaceable winners, duplicates, tombstones and tag removals.
        let reference_backing = Arc::new(MemoryStore::new());
        let reference = NostrEventStore::with_options(
            Arc::clone(&reference_backing),
            NostrEventStoreOptions {
                btree_order: Some(64),
                index_commit_batch_size: None,
                ..Default::default()
            },
        );
        let expected_root = reference
            .build(None, historical.iter().chain(&incoming).cloned())
            .await
            .unwrap()
            .unwrap();
        let expected = projections(reference_backing, &expected_root).await;

        for order in [32, 64, 128] {
            for commit in [256, 1024] {
                let measured = Arc::new(RecordingStore::new(Arc::clone(&backing)));
                let writer = NostrEventStore::with_options(
                    Arc::clone(&measured),
                    NostrEventStoreOptions {
                        btree_order: Some(order),
                        index_commit_batch_size: Some(commit),
                        ..Default::default()
                    },
                );
                let started = Instant::now();
                let report = writer
                    .build_with_superseded_nodes(Some(&previous), incoming.clone())
                    .await
                    .unwrap();
                let elapsed = started.elapsed();
                let next = report.root.unwrap();
                let m = &measured.metrics;
                let row = serde_json::json!({
                    "order": order, "commit": commit, "elapsed_ms": elapsed.as_millis(),
                    "historical_events": historical.len(), "incoming_events": incoming.len(),
                    "get_calls": m.gets.load(Ordering::Relaxed), "flushes": m.flushes.load(Ordering::Relaxed),
                    "attempted_blobs": m.attempted_blobs.load(Ordering::Relaxed),
                    "attempted_bytes": m.attempted_bytes.load(Ordering::Relaxed),
                    "retained_new_blobs": m.inserted_blobs.load(Ordering::Relaxed),
                    "retained_new_blob_bytes": m.inserted_bytes.load(Ordering::Relaxed),
                    "superseded_retained": report.superseded_nodes.len(),
                });
                assert_eq!(projections(Arc::clone(&measured), &next).await, expected);
                assert_eq!(
                    projections(Arc::clone(&measured), &previous).await,
                    old_projections
                );
                let old_bodies = writer
                    .load_event_blobs(old_projections["by-id"].iter().map(|(_, cid)| cid.clone()))
                    .await
                    .unwrap();
                assert_eq!(old_bodies, expected_old);
                for cid in &report.superseded_nodes {
                    assert!(measured.has(&cid.hash).await.unwrap());
                }
                let replay = writer
                    .build(Some(&next), incoming.clone())
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(replay, next, "duplicate replay must preserve exact root");
                assert_eq!(
                    m.inserted_bytes.load(Ordering::Relaxed),
                    row["retained_new_blob_bytes"].as_u64().unwrap(),
                    "duplicate replay must retain no new bytes"
                );
                println!("catchup-throughput {row}");
            }
        }
    });
}

#[test]
fn catchup_coalesces_projection_writes_without_changing_any_root_or_retained_blob() {
    block_on(async {
        let (historical, incoming) = fixture();
        let backing = Arc::new(MemoryStore::new());
        let previous = NostrEventStore::new(backing.clone())
            .build(None, historical.clone())
            .await
            .unwrap()
            .unwrap();
        let old_projections = projections(backing.clone(), &previous).await;
        let mut rows = Vec::new();
        let mut roots = Vec::new();
        for threshold in [0, 8 * 1024 * 1024] {
            let measured = Arc::new(RecordingStore::new(backing.clone()));
            let writer = NostrEventStore::with_options(
                measured.clone(),
                NostrEventStoreOptions {
                    index_commit_batch_size: Some(256),
                    ..Default::default()
                },
            )
            .with_index_write_buffer_bytes(threshold);
            let root = writer
                .build(Some(&previous), incoming.clone())
                .await
                .unwrap()
                .unwrap();
            let m = &measured.metrics;
            rows.push((
                m.flushes.load(Ordering::Relaxed),
                m.inserted_blobs.load(Ordering::Relaxed),
                m.inserted_bytes.load(Ordering::Relaxed),
            ));
            roots.push(root.clone());
            // Inspect the backing store directly, after the writer returns:
            // there must be no hidden unflushed bytes required by either root.
            let reader = NostrEventStore::new(measured.clone());
            reader.validate_index_root(Some(&root)).await.unwrap();
            assert_eq!(
                projections(measured.clone(), &previous).await,
                old_projections
            );
            let expected = projections(measured.clone(), &root).await;
            assert!(expected["by-id"].len() > historical.len());
            for event in &historical {
                assert!(reader
                    .get_by_id(Some(&previous), &event.id)
                    .await
                    .unwrap()
                    .is_some());
            }
        }
        assert_eq!(
            roots[0], roots[1],
            "buffering cannot alter any content-addressed projection"
        );
        assert_eq!(rows[0].1, rows[1].1, "same retained blob count");
        assert_eq!(rows[0].2, rows[1].2, "same retained payload bytes");
        assert_eq!(
            rows[1].0, 14,
            "seven commits must each flush payloads and their final indexes"
        );
        eprintln!(
            "catchup-write-coalescing baseline={:?} coalesced={:?}",
            rows[0], rows[1]
        );
        assert!(
            rows[1].0 * 2 < rows[0].0,
            "must eliminate over half of backing write batches"
        );
    });
}

#[test]
fn coalesced_append_flush_failures_do_not_return_an_uncommitted_root() {
    block_on(async {
        let (historical, incoming) = fixture();
        let backing = Arc::new(MemoryStore::new());
        let previous = NostrEventStore::new(backing.clone())
            .build(None, historical[..8].to_vec())
            .await
            .unwrap()
            .unwrap();
        let old_projections = projections(backing.clone(), &previous).await;
        for threshold in [1, 8 * 1024 * 1024] {
            let measured = Arc::new(RecordingStore::new(backing.clone()));
            // Payloads flush first. Refuse either the first projection's
            // threshold flush or the commit's unconditional final index flush.
            measured.fail_flush_on.store(2, Ordering::Relaxed);
            let writer = NostrEventStore::with_options(
                measured.clone(),
                NostrEventStoreOptions {
                    index_commit_batch_size: Some(256),
                    ..Default::default()
                },
            )
            .with_index_write_buffer_bytes(threshold);
            let error = writer
                .build(Some(&previous), incoming[..512].to_vec())
                .await
                .unwrap_err();
            assert!(error.to_string().contains("bounded write refusal"));
            assert_eq!(measured.metrics.flushes.load(Ordering::Relaxed), 2);
            assert_eq!(
                measured.metrics.inserted_blobs.load(Ordering::Relaxed),
                256,
                "refusal in the first commit must stop before the next event batch"
            );
            assert_eq!(
                projections(measured.clone(), &previous).await,
                old_projections
            );
            measured.fail_flush_on.store(0, Ordering::Relaxed);
            let next = writer
                .build(Some(&previous), incoming[..512].to_vec())
                .await
                .unwrap()
                .unwrap();
            NostrEventStore::new(measured.clone())
                .validate_index_root(Some(&next))
                .await
                .unwrap();
            assert_eq!(projections(measured, &previous).await, old_projections);
        }
    });
}
