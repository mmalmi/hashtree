//! A bounded diagnostic through the unchanged append/cache/Pool implementations.
//! Root-read markers identify projection intervals without production hooks.
//! Interval wall time includes following flush/manifest work until the next root;
//! summed store latency can overlap during parallel tag updates. No global cache purge.
use super::*;
use hashtree_core::Cid;
use hashtree_index::{BTree, BTreeOptions};
use hashtree_lmdb::{PhysicalSpaceGuard, PoolMemberConfig, PoolStore, PoolStoreConfig};
use hashtree_nostr::{NostrEventManifest, StoredNostrEvent};
use nostr::secp256k1::rand::{rngs::StdRng, SeedableRng};
use nostr::{Tag, SECP256K1};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Instant;

#[cfg(target_os = "linux")]
mod cold_files;
mod cold_mappings;
#[cfg(target_os = "linux")]
mod cold_pool;
mod os_io;

#[derive(Clone, Default, Serialize)]
struct Counts {
    wall_us: u64,
    logical_gets: u64,
    backing_gets: u64,
    backing_get_bytes: u64,
    backing_get_us: u64,
    first_hash_reads: u64,
    repeated_hash_reads: u64,
    first_hash_read_us: u64,
    repeated_hash_read_us: u64,
    read_after_write: u64,
    has_calls: u64,
    has_us: u64,
    write_calls: u64,
    write_blobs: u64,
    write_bytes: u64,
    write_us: u64,
    read_thread_io: Option<os_io::Work>,
    write_thread_io: Option<os_io::Work>,
    process_io: Option<os_io::ProcessIo>,
}

struct Trace {
    enabled: bool,
    projections: bool,
    phase: String,
    started: Instant,
    roots: HashMap<Hash, &'static str>,
    phases: BTreeMap<String, Counts>,
    seen: HashSet<Hash>,
    written: HashSet<Hash>,
    process_sample: Option<os_io::ProcessIo>,
}
impl Trace {
    fn new(manifest: NostrEventManifest) -> Self {
        Self {
            enabled: false,
            projections: false,
            phase: "off".into(),
            started: Instant::now(),
            roots: roots(manifest)
                .into_iter()
                .filter_map(|(name, root)| root.map(|cid| (cid.hash, name)))
                .collect(),
            phases: BTreeMap::new(),
            seen: HashSet::new(),
            written: HashSet::new(),
            process_sample: None,
        }
    }
    fn phase(&mut self, next: &str) {
        let sample = os_io::ProcessIo::sample();
        if self.enabled {
            if let Some((after, before)) = sample.zip(self.process_sample) {
                os_io::ProcessIo::accumulate(
                    &mut self
                        .phases
                        .entry(self.phase.clone())
                        .or_default()
                        .process_io,
                    after.delta(before),
                );
            }
            self.phases.entry(self.phase.clone()).or_default().wall_us +=
                self.started.elapsed().as_micros() as u64;
        }
        self.process_sample = sample;
        self.phase = next.into();
        self.started = Instant::now();
    }
    fn add(&mut self, phase: &str, update: impl FnOnce(&mut Counts)) {
        if self.enabled {
            update(self.phases.entry(phase.into()).or_default());
        }
    }
    fn read(&mut self, hash: &Hash) -> String {
        if self.enabled && self.projections {
            if let Some(name) = self.roots.get(hash).copied() {
                if name != self.phase {
                    self.phase(name);
                }
            }
        }
        let phase = self.phase.clone();
        self.add(&phase, |row| row.logical_gets += 1);
        phase
    }
}
type SharedTrace = Arc<Mutex<Trace>>;

struct Measured<S: Store> {
    base: Arc<S>,
    trace: SharedTrace,
}
#[async_trait]
impl<S: Store> Store for Measured<S> {
    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        let phase = self.trace.lock().unwrap().phase.clone();
        let start = Instant::now();
        let resource = os_io::ThreadStart::start();
        let result = self.base.get(hash).await;
        let resource = resource.finish();
        let elapsed = start.elapsed().as_micros() as u64;
        let bytes = result
            .as_ref()
            .ok()
            .and_then(|v| v.as_ref())
            .map_or(0, |v| v.len()) as u64;
        let mut trace = self.trace.lock().unwrap();
        if trace.enabled {
            let first = trace.seen.insert(*hash);
            let written = trace.written.contains(hash);
            trace.add(&phase, |row| {
                os_io::Work::accumulate(&mut row.read_thread_io, resource);
                row.backing_gets += 1;
                row.backing_get_bytes += bytes;
                row.backing_get_us += elapsed;
                row.first_hash_reads += u64::from(first);
                row.repeated_hash_reads += u64::from(!first);
                if first {
                    row.first_hash_read_us += elapsed;
                } else {
                    row.repeated_hash_read_us += elapsed;
                }
                row.read_after_write += u64::from(written);
            });
        }
        result
    }
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        Ok(self.put_many_optimistic(vec![(hash, data)]).await? > 0)
    }
    async fn put_many(&self, items: Vec<(Hash, Vec<u8>)>) -> Result<usize, StoreError> {
        self.put_many_optimistic(items).await
    }
    async fn put_many_optimistic(&self, items: Vec<(Hash, Vec<u8>)>) -> Result<usize, StoreError> {
        let phase = self.trace.lock().unwrap().phase.clone();
        let hashes = items.iter().map(|(hash, _)| *hash).collect::<Vec<_>>();
        let bytes = items
            .iter()
            .map(|(_, bytes)| bytes.len() as u64)
            .sum::<u64>();
        let start = Instant::now();
        let resource = os_io::ThreadStart::start();
        let result = self.base.put_many_optimistic(items).await;
        let resource = resource.finish();
        let elapsed = start.elapsed().as_micros() as u64;
        let mut trace = self.trace.lock().unwrap();
        if trace.enabled && result.is_ok() {
            trace.written.extend(&hashes);
        }
        trace.add(&phase, |row| {
            os_io::Work::accumulate(&mut row.write_thread_io, resource);
            row.write_calls += 1;
            row.write_blobs += hashes.len() as u64;
            row.write_bytes += bytes;
            row.write_us += elapsed;
        });
        result
    }
    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        let phase = self.trace.lock().unwrap().phase.clone();
        let start = Instant::now();
        let result = self.base.has(hash).await;
        let elapsed = start.elapsed().as_micros() as u64;
        self.trace.lock().unwrap().add(&phase, |row| {
            row.has_calls += 1;
            row.has_us += elapsed;
        });
        result
    }
    async fn delete(&self, _: &Hash) -> Result<bool, StoreError> {
        panic!("diagnostic must preserve history")
    }
}

// Outside the actual cache, so a cache hit still marks the production root read.
struct Marked<S: Store> {
    cache: Arc<CatchupReadCache<Measured<S>>>,
    trace: SharedTrace,
}
#[async_trait]
impl<S: Store> Store for Marked<S> {
    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        self.trace.lock().unwrap().read(hash);
        self.cache.get(hash).await
    }
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        Ok(self.put_many_optimistic(vec![(hash, data)]).await? > 0)
    }
    async fn put_many(&self, items: Vec<(Hash, Vec<u8>)>) -> Result<usize, StoreError> {
        self.put_many_optimistic(items).await
    }
    async fn put_many_optimistic(&self, items: Vec<(Hash, Vec<u8>)>) -> Result<usize, StoreError> {
        let first = {
            let t = self.trace.lock().unwrap();
            t.enabled && t.phase == "existing-lookups"
        };
        if first {
            self.trace.lock().unwrap().phase("payload-flush");
        }
        let result = self.cache.put_many_optimistic(items).await;
        if first {
            let mut trace = self.trace.lock().unwrap();
            trace.projections = true;
            trace.phase("projection-setup");
        }
        result
    }
    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.cache.has(hash).await
    }
    async fn delete(&self, _: &Hash) -> Result<bool, StoreError> {
        panic!("diagnostic must preserve history")
    }
}

fn roots(m: NostrEventManifest) -> Vec<(&'static str, Option<Cid>)> {
    vec![
        ("by-id", m.by_id),
        ("by-author-time", m.by_author_time),
        ("by-author-kind-time", m.by_author_kind_time),
        ("by-kind-time", m.by_kind_time),
        ("by-kind-time-author", m.by_kind_time_author),
        ("by-time", m.by_time),
        ("by-tag", m.by_tag),
        ("replaceable", m.replaceable),
        ("parameterized", m.parameterized_replaceable),
    ]
}

fn event(key: &Keys, ordinal: usize, timestamp: u64, tags: usize) -> StoredNostrEvent {
    let tags = (0..tags).map(|n| {
        Tag::parse(vec![
            "e".to_string(),
            format!("{:064x}", (ordinal * 67 + n * 251) % 2048 + 1),
        ])
        .unwrap()
    });
    let event = EventBuilder::new(Kind::TextNote, format!("synthetic append {ordinal}"))
        .tags(tags)
        .custom_created_at(Timestamp::from_secs(timestamp))
        .build(key.public_key())
        .sign_with_ctx(
            SECP256K1,
            &mut StdRng::seed_from_u64(timestamp + ordinal as u64),
            key,
        )
        .unwrap();
    event.verify().unwrap();
    stored_event_from_nostr_sdk_event(&event)
}

async fn projection_rows<S: Store>(
    store: Arc<S>,
    root: &Cid,
) -> BTreeMap<String, Vec<(String, Cid)>> {
    let manifest = NostrEventStore::new(store.clone())
        .get_manifest(Some(root))
        .await
        .unwrap();
    let index = BTree::new(store, BTreeOptions::default());
    let mut rows = BTreeMap::new();
    for (name, root) in roots(manifest) {
        rows.insert(
            name.into(),
            index.links_entries(root.as_ref()).await.unwrap(),
        );
    }
    rows
}

async fn measure<S: Store>(
    base: Arc<S>,
    seed: &Arc<MemoryStore>,
    historical: &[StoredNostrEvent],
    root: &Cid,
    incoming: Vec<StoredNostrEvent>,
    backend: &str,
    tags: usize,
    prepare: impl FnOnce() -> serde_json::Value,
) -> (Cid, serde_json::Value) {
    let manifest = NostrEventStore::new(seed.clone())
        .get_manifest(Some(root))
        .await
        .unwrap();
    let before_rows = projection_rows(seed.clone(), root).await;
    let trace = Arc::new(Mutex::new(Trace::new(manifest)));
    let measured = Arc::new(Measured {
        base: base.clone(),
        trace: trace.clone(),
    });
    let cache = Arc::new(CatchupReadCache::new(measured, 64 * 1024 * 1024));
    let marked = Arc::new(Marked {
        cache: cache.clone(),
        trace: trace.clone(),
    });
    let writer = NostrEventStore::with_options(
        marked,
        NostrEventStoreOptions {
            index_commit_batch_size: Some(256),
            ..Default::default()
        },
    )
    .with_index_write_buffer_bytes(8 * 1024 * 1024);
    let preparation = prepare();
    {
        let mut t = trace.lock().unwrap();
        t.phase("existing-lookups");
        t.enabled = true;
    }
    let start = Instant::now();
    // Canonical catch-up directly appends. The separate deployed legacy
    // diagnostic includes repair reconciliation; do not import that runtime
    // path or compare its timing/counters as if these were identical builds.
    let next = writer
        .build(Some(root), incoming.clone())
        .await
        .unwrap()
        .unwrap();
    let elapsed = start.elapsed().as_micros() as u64;
    {
        let mut t = trace.lock().unwrap();
        t.phase("verification-excluded");
        t.enabled = false;
    }
    let phases = trace.lock().unwrap().phases.clone();
    let (hits, misses, bytes, entries) = cache.read_stats();
    assert_eq!(misses, phases.values().map(|v| v.backing_gets).sum::<u64>());
    assert_eq!(
        hits + misses,
        phases.values().map(|v| v.logical_gets).sum::<u64>()
    );
    assert!(bytes <= 64 * 1024 * 1024 && entries <= MAX_ENTRIES);
    writer.validate_index_root(Some(&next)).await.unwrap();
    assert_eq!(projection_rows(base.clone(), root).await, before_rows);
    let final_rows = projection_rows(base.clone(), &next).await;
    let rebuilt = Arc::new(MemoryStore::new());
    let reference = NostrEventStore::with_options(
        rebuilt.clone(),
        NostrEventStoreOptions {
            index_commit_batch_size: None,
            ..Default::default()
        },
    )
    .build(None, historical.iter().chain(incoming.iter()).cloned())
    .await
    .unwrap()
    .unwrap();
    assert_eq!(final_rows, projection_rows(rebuilt, &reference).await);
    let verified = NostrEventStore::new(base)
        .load_event_blobs(final_rows["by-id"].iter().map(|(_, cid)| cid.clone()))
        .await
        .unwrap();
    assert_eq!(verified.len(), historical.len() + incoming.len());
    for stored in verified {
        hashtree_nostr::VerifiedStoredNostrEvent::try_from(stored).unwrap();
    }
    let result = serde_json::json!({ "append_path": "canonical-direct-build", "backend": backend, "events": incoming.len(), "tags_per_event": tags,
        "historical_events": historical.len(), "append_us": elapsed, "hits": hits, "misses": misses,
        "root_hash": hex::encode(next.hash),
        "cache_bytes": bytes, "cache_entries": entries, "phases": phases, "preparation": preparation, "retained_history_verified": true });
    eprintln!("append-io-measurement {result}");
    (next, result)
}

#[tokio::test]
async fn append_io_attribution_49_events() {
    let key = Keys::parse(&format!("{:064x}", 9001)).unwrap();
    let historical = (0..8192)
        .map(|i| {
            let author = Keys::parse(&format!("{:064x}", i % 16 + 1)).unwrap();
            event(&author, i, 10000 + 2 * i as u64, 8)
        })
        .collect::<Vec<_>>();
    let seed = Arc::new(MemoryStore::new());
    let root = NostrEventStore::with_options(
        seed.clone(),
        NostrEventStoreOptions {
            index_commit_batch_size: None,
            ..Default::default()
        },
    )
    .build(None, historical.clone())
    .await
    .unwrap()
    .unwrap();
    let mut all = Vec::new();
    for tags in [0, 32] {
        let incoming = (0..49)
            .map(|i| event(&key, 90000 + i, 10001 + (i * 163) as u64, tags))
            .collect::<Vec<_>>();
        let memory = Arc::new(MemoryStore::new());
        let mut seed_blobs = seed
            .keys()
            .into_iter()
            .map(|hash| {
                (
                    hash,
                    futures::executor::block_on(seed.get(&hash))
                        .unwrap()
                        .unwrap(),
                )
            })
            .collect::<Vec<_>>();
        seed_blobs.sort_by_key(|(hash, _)| *hash);
        memory.put_many(seed_blobs.clone()).await.unwrap();
        let (memory_root, row) = measure(
            memory,
            &seed,
            &historical,
            &root,
            incoming.clone(),
            "memory",
            tags,
            || serde_json::Value::Null,
        )
        .await;
        all.push(row);
        let directory = tempfile::tempdir().unwrap();
        let mut config = PoolStoreConfig {
            catalog_map_size_bytes: 128 * 1024 * 1024,
            physical_space: Some(PhysicalSpaceGuard::new(10 * 1024 * 1024 * 1024).unwrap()),
            ..Default::default()
        };
        config.temperature.enabled = false;
        let pool = Arc::new(PoolStore::open(directory.path().join("catalog"), config).unwrap());
        pool.add_member(PoolMemberConfig::new(
            directory.path().join("member"),
            512 * 1024 * 1024,
        ))
        .unwrap();
        for chunk in seed_blobs.chunks(512) {
            pool.put_many_optimistic_sync(chunk).unwrap();
        }
        let (pool_root, row) = measure(
            pool.clone(),
            &seed,
            &historical,
            &root,
            incoming,
            "lmdb-pool",
            tags,
            || serde_json::Value::Null,
        )
        .await;
        assert_eq!(
            memory_root, pool_root,
            "backend must not alter immutable root"
        );
        all.push(row);
        drop(pool);
        drop(directory);
    }
    let no_tags = all[0]["misses"].as_u64().unwrap();
    let many_tags = all[2]["misses"].as_u64().unwrap();
    assert!(
        many_tags > no_tags,
        "tag fan-out must be visible in real backing reads"
    );
}
