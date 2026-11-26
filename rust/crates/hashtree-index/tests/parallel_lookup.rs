use async_trait::async_trait;
use futures::executor::block_on;
use hashtree_core::{
    Cid, Hash, HashTree, HashTreeConfig, LinkType, MemoryStore, Store, StoreError,
};
use hashtree_index::{BTree, BTreeOptions};
use std::collections::BTreeMap;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};

#[derive(Default)]
struct LocalStore {
    inner: MemoryStore,
    probing: AtomicBool,
    active: AtomicUsize,
    peak: AtomicUsize,
    reads: AtomicUsize,
}

#[async_trait]
impl Store for LocalStore {
    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        if !self.probing.load(Ordering::SeqCst) {
            return self.inner.get(hash).await;
        }
        let n = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(n, Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(2));
        let result = self.inner.get(hash).await;
        self.active.fetch_sub(1, Ordering::SeqCst);
        result
    }
    async fn put(&self, hash: Hash, bytes: Vec<u8>) -> Result<bool, StoreError> {
        self.inner.put(hash, bytes).await
    }
    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.inner.has(hash).await
    }
    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.inner.delete(hash).await
    }
}

fn target(n: usize) -> Cid {
    let mut hash = [0; 32];
    hash[..8].copy_from_slice(&(n as u64).to_be_bytes());
    Cid { hash, key: None }
}

#[test]
fn required_bulk_lookup_overlaps_blocking_reads_without_amplifying_them() {
    block_on(async {
        let store = Arc::new(LocalStore::default());
        let serial = BTree::new(store.clone(), BTreeOptions { order: Some(8) });
        let root = serial
            .build_links((0..2048).map(|n| (format!("k{n:05}"), target(n))))
            .await
            .unwrap()
            .unwrap();
        let keys = (0..2048)
            .step_by(7)
            .map(|n| format!("k{n:05}"))
            .chain(["missing".into()])
            .collect::<Vec<_>>();
        store.reads.store(0, Ordering::SeqCst);
        let expected = serial
            .get_links_required(&root, keys.clone())
            .await
            .unwrap();
        let serial_reads = store.reads.load(Ordering::SeqCst);
        for workers in [2, 4, 8, 16] {
            let parallel = BTree::new(store.clone(), BTreeOptions { order: Some(8) })
                .with_read_concurrency(workers);
            store.peak.store(0, Ordering::SeqCst);
            store.reads.store(0, Ordering::SeqCst);
            store.probing.store(true, Ordering::SeqCst);
            let found = parallel
                .get_links_required(&root, keys.clone())
                .await
                .unwrap();
            store.probing.store(false, Ordering::SeqCst);
            assert_eq!(found, expected);
            assert_eq!(
                store.reads.load(Ordering::SeqCst),
                serial_reads,
                "parallel reads must visit each selected node only once"
            );
            assert!(
                (2..=workers).contains(&store.peak.load(Ordering::SeqCst)),
                "independent blocking reads must overlap within the requested bound"
            );
            assert_eq!(store.active.load(Ordering::SeqCst), 0);
        }
    });
}

#[test]
fn optional_bulk_lookup_used_by_append_precheck_honors_read_workers() {
    block_on(async {
        let store = Arc::new(LocalStore::default());
        let serial = BTree::new(store.clone(), BTreeOptions { order: Some(8) });
        let root = serial
            .build_links((0..2048).map(|n| (format!("k{n:05}"), target(n))))
            .await
            .unwrap()
            .unwrap();
        let keys = (0..2048)
            .step_by(7)
            .map(|n| format!("k{n:05}"))
            .chain(["missing".into(), "k00000".into()])
            .collect::<Vec<_>>();
        store.reads.store(0, Ordering::SeqCst);
        let expected = serial.get_links(Some(&root), keys.clone()).await.unwrap();
        let reads = store.reads.load(Ordering::SeqCst);
        for workers in [1, 2, 4, 8, 16] {
            store.peak.store(0, Ordering::SeqCst);
            store.reads.store(0, Ordering::SeqCst);
            store.probing.store(true, Ordering::SeqCst);
            let started = std::time::Instant::now();
            let result = BTree::new(store.clone(), BTreeOptions { order: Some(8) })
                .with_read_concurrency(workers)
                .get_links(Some(&root), keys.clone())
                .await
                .unwrap();
            let peak = store.peak.load(Ordering::SeqCst);
            assert_eq!(result, expected);
            assert_eq!(
                store.reads.load(Ordering::SeqCst),
                reads,
                "no additional tree reads"
            );
            assert_eq!(store.active.load(Ordering::SeqCst), 0);
            assert!(
                peak <= workers && (workers == 1 || peak >= 2),
                "append precheck requested {workers} workers but reached {peak}"
            );
            println!(
                "optional lookup: workers={workers} reads={reads} peak={peak} elapsed_ms={}",
                started.elapsed().as_millis()
            );
        }
    });
}

#[test]
fn optional_parallel_lookup_preserves_absent_branch_and_empty_root_semantics() {
    block_on(async {
        let store = Arc::new(LocalStore::default());
        let serial = BTree::new(store.clone(), BTreeOptions { order: Some(8) });
        let root = serial
            .build_links((0..512).map(|n| (format!("k{n:05}"), target(n))))
            .await
            .unwrap()
            .unwrap();
        let tree = HashTree::new(HashTreeConfig::new(store.clone()));
        let first = tree.list_directory_required(&root).await.unwrap().remove(0);
        store.delete(&first.hash).await.unwrap();
        let keys = (0..512).map(|n| format!("k{n:05}")).collect::<Vec<_>>();
        let expected = serial.get_links(Some(&root), keys.clone()).await.unwrap();
        assert!(!expected.is_empty() && expected.len() < keys.len());
        let parallel =
            BTree::new(store.clone(), BTreeOptions { order: Some(8) }).with_read_concurrency(8);
        assert_eq!(
            parallel.get_links(Some(&root), keys.clone()).await.unwrap(),
            expected
        );
        assert!(parallel.get_links(None, keys).await.unwrap().is_empty());
        assert!(parallel
            .get_links(Some(&root), Vec::new())
            .await
            .unwrap()
            .is_empty());
        assert_eq!(store.active.load(Ordering::SeqCst), 0);
    });
}

#[test]
fn narrow_root_still_uses_independent_descendant_reads() {
    block_on(async {
        let store = Arc::new(LocalStore::default());
        let serial = BTree::new(store.clone(), BTreeOptions::default());
        let root = serial
            .build_links((0..32768).map(|n| (format!("k{n:05}"), target(n))))
            .await
            .unwrap()
            .unwrap();
        let keys = (0..32768)
            .step_by(127)
            .take(256)
            .map(|n| format!("k{n:05}"))
            .collect::<Vec<_>>();
        store.reads.store(0, Ordering::SeqCst);
        let expected = serial
            .get_links_required(&root, keys.clone())
            .await
            .unwrap();
        let serial_reads = store.reads.load(Ordering::SeqCst);
        store.reads.store(0, Ordering::SeqCst);
        store.probing.store(true, Ordering::SeqCst);
        let found = BTree::new(store.clone(), BTreeOptions::default())
            .with_read_concurrency(8)
            .get_links_required(&root, keys)
            .await
            .unwrap();
        assert_eq!(found, expected);
        assert_eq!(store.reads.load(Ordering::SeqCst), serial_reads);
        assert!(
            (4..=8).contains(&store.peak.load(Ordering::SeqCst)),
            "a two-child root must not cap independent descendant reads at two"
        );
        assert_eq!(store.active.load(Ordering::SeqCst), 0);
    });
}

#[test]
fn narrow_root_bulk_update_preserves_order_and_uses_available_workers() {
    block_on(async {
        let store = Arc::new(LocalStore::default());
        let serial = BTree::new(store.clone(), BTreeOptions::default()).with_update_concurrency(1);
        let root = serial
            .build_links((0..32768).map(|n| (format!("k{n:05}"), target(n))))
            .await
            .unwrap()
            .unwrap();
        let changes = (0..32768)
            .step_by(63)
            .take(512)
            .map(|n| (format!("k{n:05}"), (n % 5 != 0).then(|| target(n + 50000))))
            .collect::<Vec<_>>();
        store.reads.store(0, Ordering::SeqCst);
        let expected = serial
            .update_links_with_superseded(Some(&root), changes.clone())
            .await
            .unwrap();
        let expected_reads = store.reads.load(Ordering::SeqCst);
        for workers in [1, 2, 4, 8, 16] {
            store.reads.store(0, Ordering::SeqCst);
            store.peak.store(0, Ordering::SeqCst);
            store.probing.store(true, Ordering::SeqCst);
            let started = std::time::Instant::now();
            let result = BTree::new(store.clone(), BTreeOptions::default())
                .with_update_concurrency(workers)
                .update_links_with_superseded(Some(&root), changes.clone())
                .await
                .unwrap();
            store.probing.store(false, Ordering::SeqCst);
            assert_eq!(result.root, expected.root);
            assert_eq!(result.superseded_nodes, expected.superseded_nodes);
            assert_eq!(store.reads.load(Ordering::SeqCst), expected_reads);
            assert_eq!(store.active.load(Ordering::SeqCst), 0);
            let peak = store.peak.load(Ordering::SeqCst);
            assert!(
                (workers.min(4)..=workers).contains(&peak),
                "{workers} workers reached only {peak} despite independent descendants"
            );
            println!("update frontier: workers={workers} reads={expected_reads} peak={peak} elapsed_ms={}", started.elapsed().as_millis());
        }
    });
}

#[test]
fn parallel_update_frontier_preserves_splits_empty_and_unchanged_nodes() {
    block_on(async {
        let store = Arc::new(LocalStore::default());
        let serial =
            BTree::new(store.clone(), BTreeOptions { order: Some(8) }).with_update_concurrency(1);
        let root = serial
            .build_links((0..1024).map(|n| (format!("k{n:05}"), target(n))))
            .await
            .unwrap()
            .unwrap();
        let cases = [
            (0..1024)
                .map(|n| (format!("k{n:05}"), Some(target(n))))
                .collect::<Vec<_>>(),
            (0..1024).map(|n| (format!("k{n:05}"), None)).collect(),
            (0..1024)
                .map(|n| (format!("k{n:05}:new"), Some(target(n + 2000))))
                .collect(),
        ];
        for changes in cases {
            store.reads.store(0, Ordering::SeqCst);
            let expected = serial
                .update_links_with_superseded(Some(&root), changes.clone())
                .await
                .unwrap();
            let reads = store.reads.load(Ordering::SeqCst);
            for workers in [2, 4, 8, 16] {
                store.reads.store(0, Ordering::SeqCst);
                let actual = BTree::new(store.clone(), BTreeOptions { order: Some(8) })
                    .with_update_concurrency(workers)
                    .update_links_with_superseded(Some(&root), changes.clone())
                    .await
                    .unwrap();
                assert_eq!(actual.root, expected.root);
                assert_eq!(actual.superseded_nodes, expected.superseded_nodes);
                assert_eq!(store.reads.load(Ordering::SeqCst), reads);
                assert_eq!(store.active.load(Ordering::SeqCst), 0);
            }
        }
    });
}

#[test]
fn parallel_update_missing_descendant_joins_workers_and_preserves_old_root() {
    block_on(async {
        let store = Arc::new(LocalStore::default());
        let index = BTree::new(store.clone(), BTreeOptions { order: Some(8) });
        let root = index
            .build_links((0..4096).map(|n| (format!("k{n:05}"), target(n))))
            .await
            .unwrap()
            .unwrap();
        let tree = HashTree::new(HashTreeConfig::new(store.clone()));
        let original_root = store.get(&root.hash).await.unwrap();
        let mut leaf = root.clone();
        loop {
            let entries = tree.list_directory_required(&leaf).await.unwrap();
            if entries[0].link_type != LinkType::Dir {
                break;
            }
            let entry = &entries[entries.len() / 2];
            leaf = Cid {
                hash: entry.hash,
                key: entry.key,
            };
        }
        store.delete(&leaf.hash).await.unwrap();
        for workers in [2, 4, 8, 16] {
            store.probing.store(true, Ordering::SeqCst);
            assert!(BTree::new(store.clone(), BTreeOptions { order: Some(8) })
                .with_update_concurrency(workers)
                .update_links_with_superseded(
                    Some(&root),
                    (0..4096).map(|n| (format!("k{n:05}"), Some(target(n + 5000))))
                )
                .await
                .is_err());
            assert_eq!(
                store.active.load(Ordering::SeqCst),
                0,
                "error must join all workers"
            );
            store.probing.store(false, Ordering::SeqCst);
            assert_eq!(store.get(&root.hash).await.unwrap(), original_root);
        }
    });
}

#[test]
fn required_parallel_lookup_never_turns_a_missing_branch_into_partial_success() {
    block_on(async {
        let store = Arc::new(LocalStore::default());
        let index =
            BTree::new(store.clone(), BTreeOptions { order: Some(8) }).with_read_concurrency(4);
        let root = index
            .build_links((0..512).map(|n| (format!("k{n:05}"), target(n))))
            .await
            .unwrap()
            .unwrap();
        let tree = HashTree::new(HashTreeConfig::new(store.clone()));
        let mut leaf = root.clone();
        loop {
            let entries = tree.list_directory_required(&leaf).await.unwrap();
            if entries[0].link_type != LinkType::Dir {
                break;
            }
            leaf = Cid {
                hash: entries[0].hash,
                key: entries[0].key,
            };
        }
        store.delete(&leaf.hash).await.unwrap();
        store.probing.store(true, Ordering::SeqCst);
        assert!(index
            .get_links_required(&root, (0..512).map(|n| format!("k{n:05}")))
            .await
            .is_err());
        assert_eq!(
            store.active.load(Ordering::SeqCst),
            0,
            "failure must join every read worker"
        );
    });
}

#[test]
fn empty_and_duplicate_required_queries_preserve_exact_results() {
    block_on(async {
        let store = Arc::new(LocalStore::default());
        let index =
            BTree::new(store.clone(), BTreeOptions { order: Some(8) }).with_read_concurrency(4);
        let root = index
            .build_links([(String::from("a"), target(1))])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            index.get_links_required(&root, Vec::new()).await.unwrap(),
            BTreeMap::new()
        );
        assert_eq!(
            index
                .get_links_required(&root, ["a".into(), "a".into(), "absent".into()])
                .await
                .unwrap(),
            BTreeMap::from([("a".into(), target(1))])
        );
    });
}
