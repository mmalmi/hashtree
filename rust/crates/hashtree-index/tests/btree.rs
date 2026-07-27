use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use futures::executor::block_on;
use hashtree_core::{
    Cid, DirEntry, Hash, HashTree, HashTreeConfig, HashTreeError, LinkType, MemoryStore, Store,
    StoreError,
};
use hashtree_index::{escape_key, BTree, BTreeOptions};

fn cid_from_hex(hex: &str) -> Cid {
    let bytes = hex::decode(hex).unwrap();
    let hash: [u8; 32] = bytes.try_into().unwrap();
    Cid { hash, key: None }
}

#[derive(Default)]
struct CountingStore {
    inner: MemoryStore,
    gets: AtomicUsize,
}

impl CountingStore {
    fn reset_gets(&self) {
        self.gets.store(0, Ordering::Relaxed);
    }

    fn gets(&self) -> usize {
        self.gets.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl Store for CountingStore {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        self.inner.put(hash, data).await
    }

    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        self.gets.fetch_add(1, Ordering::Relaxed);
        self.inner.get(hash).await
    }

    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.inner.has(hash).await
    }

    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.inner.delete(hash).await
    }
}

#[derive(Default)]
struct ConcurrentReadStore {
    inner: MemoryStore,
    probe_enabled: AtomicBool,
    active_gets: AtomicUsize,
    max_active_gets: AtomicUsize,
}

impl ConcurrentReadStore {
    fn start_probe(&self) {
        self.active_gets.store(0, Ordering::SeqCst);
        self.max_active_gets.store(0, Ordering::SeqCst);
        self.probe_enabled.store(true, Ordering::SeqCst);
    }
}

#[async_trait]
impl Store for ConcurrentReadStore {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        self.inner.put(hash, data).await
    }

    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        if !self.probe_enabled.load(Ordering::SeqCst) {
            return self.inner.get(hash).await;
        }
        let active = self.active_gets.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active_gets.fetch_max(active, Ordering::SeqCst);
        // Hold the first probed synchronous read briefly. Real worker threads
        // can enter another read concurrently; future-only fan-out on one
        // executor thread cannot, which is exactly what this test distinguishes.
        if self.max_active_gets.load(Ordering::SeqCst) < 2 {
            for _ in 0..1_000 {
                if self.active_gets.load(Ordering::SeqCst) > 1 {
                    break;
                }
                std::thread::yield_now();
            }
        }
        let result = self.inner.get(hash).await;
        self.active_gets.fetch_sub(1, Ordering::SeqCst);
        result
    }

    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.inner.has(hash).await
    }

    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.inner.delete(hash).await
    }
}

fn cid_from_seed(seed: usize) -> Cid {
    let mut hash = [0_u8; 32];
    hash[..8].copy_from_slice(&(seed as u64).to_be_bytes());
    Cid { hash, key: None }
}

fn strip_internal_counts<'a, S: Store + 'a>(
    tree: &'a HashTree<S>,
    node: Cid,
) -> Pin<Box<dyn Future<Output = Result<Cid, HashTreeError>> + 'a>> {
    Box::pin(async move {
        let entries = tree.list_directory(&node).await?;
        if entries.iter().any(|entry| entry.link_type != LinkType::Dir) {
            return Ok(node);
        }

        let mut legacy_entries = Vec::with_capacity(entries.len());
        for entry in entries {
            let child = strip_internal_counts(
                tree,
                Cid {
                    hash: entry.hash,
                    key: entry.key,
                },
            )
            .await?;
            legacy_entries
                .push(DirEntry::from_cid(entry.name, &child).with_link_type(LinkType::Dir));
        }
        tree.put_directory(legacy_entries).await
    })
}

#[test]
fn string_values_support_get_and_range() {
    block_on(async {
        let store = Arc::new(MemoryStore::new());
        let btree = BTree::new(store, BTreeOptions { order: Some(4) });

        let mut root = None;
        for key in ["user:002", "user:001", "other:001", "user:003"] {
            root = Some(btree.insert(root.as_ref(), key, key).await.unwrap());
        }

        assert_eq!(
            btree.get(root.as_ref(), "user:001").await.unwrap(),
            Some("user:001".into())
        );
        assert_eq!(
            btree.prefix(root.as_ref().unwrap(), "user:").await.unwrap(),
            vec![
                ("user:001".to_string(), "user:001".to_string()),
                ("user:002".to_string(), "user:002".to_string()),
                ("user:003".to_string(), "user:003".to_string()),
            ]
        );
    });
}

#[test]
fn bulk_string_changes_match_map_semantics() {
    block_on(async {
        let store = Arc::new(MemoryStore::new());
        let btree = BTree::new(store, BTreeOptions { order: Some(4) });
        let initial = (0..40)
            .map(|index| (format!("key:{index:03}"), format!("value:{index:03}")))
            .collect::<BTreeMap<_, _>>();
        let root = btree
            .build(initial.clone())
            .await
            .unwrap()
            .expect("initial root");
        let changes = vec![
            ("key:003".to_string(), None),
            ("key:019".to_string(), Some("replacement".to_string())),
            ("key:041".to_string(), Some("added".to_string())),
        ];

        let updated = btree
            .update(Some(&root), changes.clone())
            .await
            .unwrap()
            .expect("updated root");
        let mut expected = initial;
        for (key, value) in changes {
            match value {
                Some(value) => {
                    expected.insert(key, value);
                }
                None => {
                    expected.remove(&key);
                }
            }
        }
        assert_eq!(
            btree.entries(Some(&updated)).await.unwrap(),
            expected.into_iter().collect::<Vec<_>>()
        );
    });
}

#[test]
fn sorted_link_build_streams_without_reading_existing_nodes() {
    block_on(async {
        let store = Arc::new(CountingStore::default());
        let btree = BTree::new(Arc::clone(&store), BTreeOptions { order: Some(8) });
        let entries = (0..10_000).flat_map(|index| {
            let key = format!("key:{index:05}");
            [
                (key.clone(), cid_from_seed(index)),
                (key, cid_from_seed(index + 20_000)),
            ]
        });

        let root = btree
            .build_sorted_links(entries)
            .await
            .expect("stream sorted links")
            .expect("sorted root");
        let expected = btree
            .build_links((0..10_000).flat_map(|index| {
                let key = format!("key:{index:05}");
                [
                    (key.clone(), cid_from_seed(index)),
                    (key, cid_from_seed(index + 20_000)),
                ]
            }))
            .await
            .expect("ordinary bulk links")
            .expect("ordinary root");

        assert_eq!(store.gets(), 0, "bulk construction must not read old nodes");
        assert_eq!(root, expected, "streaming and collected roots must match");
        assert_eq!(
            btree.count_stored_links(Some(&root)).await.unwrap(),
            Some(10_000)
        );
        assert_eq!(
            btree.get_link(Some(&root), "key:04200").await.unwrap(),
            Some(cid_from_seed(24_200)),
            "the last value for an adjacent duplicate key must win"
        );
    });
}

#[test]
fn link_btree_matches_typescript_fixture_root() {
    block_on(async {
        let store = Arc::new(MemoryStore::new());
        let btree = BTree::new(Arc::clone(&store), BTreeOptions { order: Some(4) });
        let _tree = HashTree::new(HashTreeConfig::new(store));

        let mut root = None;
        let fixtures = [
            (
                "author1:fffffffffffffff5:event-a",
                cid_from_hex("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"),
            ),
            (
                "author1:fffffffffffffff4:event-b",
                cid_from_hex("fffefdfcfbfaf9f8f7f6f5f4f3f2f1f0efeeedecebeae9e8e7e6e5e4e3e2e1e0"),
            ),
            (
                "author2:fffffffffffffff6:event-c",
                cid_from_hex("00070e151c232a31383f464d545b626970777e858c939aa1a8afb6bdc4cbd2d9"),
            ),
            (
                "author1:00000001:fffffffffffffff3:event-d",
                cid_from_hex("000d1a2734414e5b6875828f9ca9b6c3d0ddeaf704111e2b3845525f6c798693"),
            ),
        ];

        for (key, cid) in fixtures {
            root = Some(btree.insert_link(root.as_ref(), key, &cid).await.unwrap());
        }

        let root = root.expect("root");
        assert_eq!(
            hex::encode(root.hash),
            "2199cfc5fe036befe0932abf001df5cbe24a876e4caee661cba8a217be81f27c"
        );
        assert_eq!(
            root.key.map(hex::encode),
            Some("3df5002dd988d4c842309e2d79722300b885194229f43ad3a08e09d4285d4e30".to_string())
        );

        let prefix = btree.prefix_links(&root, "author1:").await.unwrap();
        assert_eq!(
            prefix
                .iter()
                .map(|(key, cid)| (key.clone(), hex::encode(cid.hash)))
                .collect::<Vec<_>>(),
            vec![
                (
                    "author1:00000001:fffffffffffffff3:event-d".to_string(),
                    "000d1a2734414e5b6875828f9ca9b6c3d0ddeaf704111e2b3845525f6c798693".to_string(),
                ),
                (
                    "author1:fffffffffffffff4:event-b".to_string(),
                    "fffefdfcfbfaf9f8f7f6f5f4f3f2f1f0efeeedecebeae9e8e7e6e5e4e3e2e1e0".to_string(),
                ),
                (
                    "author1:fffffffffffffff5:event-a".to_string(),
                    "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f".to_string(),
                ),
            ]
        );
    });
}

#[test]
fn bulk_link_build_matches_incremental_entries() {
    block_on(async {
        let store = Arc::new(MemoryStore::new());
        let btree = BTree::new(Arc::clone(&store), BTreeOptions { order: Some(4) });

        let fixtures = [
            (
                "author3:fffffffffffffff2:event-f",
                cid_from_hex("101112131415161718191a1b1c1d1e1f202122232425262728292a2b2c2d2e2f"),
            ),
            (
                "author1:fffffffffffffff5:event-a",
                cid_from_hex("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"),
            ),
            (
                "author1:fffffffffffffff4:event-b",
                cid_from_hex("fffefdfcfbfaf9f8f7f6f5f4f3f2f1f0efeeedecebeae9e8e7e6e5e4e3e2e1e0"),
            ),
            (
                "author2:fffffffffffffff6:event-c",
                cid_from_hex("00070e151c232a31383f464d545b626970777e858c939aa1a8afb6bdc4cbd2d9"),
            ),
            (
                "author1:00000001:fffffffffffffff3:event-d",
                cid_from_hex("000d1a2734414e5b6875828f9ca9b6c3d0ddeaf704111e2b3845525f6c798693"),
            ),
            (
                "author2:fffffffffffffff1:event-e",
                cid_from_hex("303132333435363738393a3b3c3d3e3f404142434445464748494a4b4c4d4e4f"),
            ),
            (
                "author2:fffffffffffffff0:event-g",
                cid_from_hex("505152535455565758595a5b5c5d5e5f606162636465666768696a6b6c6d6e6f"),
            ),
        ];

        let mut incremental_root = None;
        for (key, cid) in fixtures.iter() {
            incremental_root = Some(
                btree
                    .insert_link_unchecked(incremental_root.as_ref(), key, cid)
                    .await
                    .unwrap(),
            );
        }

        let bulk_root = btree
            .build_links(
                fixtures
                    .iter()
                    .map(|(key, cid)| ((*key).to_string(), cid.clone())),
            )
            .await
            .unwrap()
            .expect("bulk root");
        let incremental_root = incremental_root.expect("incremental root");

        assert_eq!(
            btree.links_entries(Some(&bulk_root)).await.unwrap(),
            btree.links_entries(Some(&incremental_root)).await.unwrap()
        );
        assert_eq!(
            btree.prefix_links(&bulk_root, "author1:").await.unwrap(),
            btree
                .prefix_links(&incremental_root, "author1:")
                .await
                .unwrap()
        );
        assert_eq!(
            btree
                .prefix_links_limited(&bulk_root, "author1:", 2)
                .await
                .unwrap(),
            btree
                .prefix_links(&bulk_root, "author1:")
                .await
                .unwrap()
                .into_iter()
                .take(2)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            btree
                .links_entries_limited(Some(&bulk_root), 3)
                .await
                .unwrap(),
            btree
                .links_entries(Some(&bulk_root))
                .await
                .unwrap()
                .into_iter()
                .take(3)
                .collect::<Vec<_>>()
        );
    });
}

#[test]
fn exclusive_link_pagination_preserves_nul_prefixed_successors() {
    block_on(async {
        let store = Arc::new(MemoryStore::new());
        let btree = BTree::new(Arc::clone(&store), BTreeOptions { order: Some(4) });
        let entries = ["a", "a\0x", "a\0y", "b"]
            .into_iter()
            .enumerate()
            .map(|(position, key)| (key.to_string(), Cid::public([position as u8 + 1; 32])))
            .collect::<Vec<_>>();
        let root = btree
            .build_links(entries.clone())
            .await
            .unwrap()
            .expect("generated link root");

        let after_a = btree
            .range_links_limited_after(&root, Some("a"), None, 2)
            .await
            .unwrap();
        assert_eq!(
            after_a
                .iter()
                .map(|(key, _)| key.as_str())
                .collect::<Vec<_>>(),
            vec!["a\0x", "a\0y"]
        );

        let mut after = None;
        let mut paged = Vec::new();
        for _ in 0..=entries.len() {
            let page = btree
                .range_links_limited_after(&root, after.as_deref(), None, 1)
                .await
                .unwrap();
            if page.is_empty() {
                break;
            }
            after = Some(page[0].0.clone());
            paged.extend(page);
        }
        assert_eq!(paged, entries);
    });
}

#[test]
fn bulk_link_changes_are_deterministic_and_match_map_semantics() {
    block_on(async {
        let store = Arc::new(MemoryStore::new());
        let btree = BTree::new(Arc::clone(&store), BTreeOptions { order: Some(4) });
        let initial = (0..40)
            .map(|index| (format!("key:{index:03}"), cid_from_seed(index)))
            .collect::<BTreeMap<_, _>>();
        let root = btree
            .build_links(initial.clone())
            .await
            .unwrap()
            .expect("initial root");

        let changes = vec![
            ("key:000".to_string(), None),
            ("key:007".to_string(), Some(cid_from_seed(700))),
            ("key:007".to_string(), Some(cid_from_seed(701))),
            ("key:020".to_string(), None),
            ("key:041".to_string(), Some(cid_from_seed(41))),
            ("key:099".to_string(), None),
            ("key:100".to_string(), Some(cid_from_seed(100))),
        ];
        let updated = btree
            .update_links(Some(&root), changes.clone())
            .await
            .unwrap()
            .expect("updated root");

        let mut expected = initial;
        for (key, value) in &changes {
            match value {
                Some(cid) => {
                    expected.insert(key.clone(), cid.clone());
                }
                None => {
                    expected.remove(key);
                }
            }
        }
        let repeated = btree
            .update_links(Some(&root), changes)
            .await
            .unwrap()
            .expect("repeated root");

        assert_eq!(updated, repeated);
        assert_eq!(
            btree.links_entries(Some(&updated)).await.unwrap(),
            expected.into_iter().collect::<Vec<_>>()
        );
    });
}

#[test]
fn bulk_link_changes_handle_noop_delete_all_and_delete_to_one() {
    block_on(async {
        let store = Arc::new(MemoryStore::new());
        let btree = BTree::new(Arc::clone(&store), BTreeOptions { order: Some(4) });
        let initial = (0..10)
            .map(|index| (format!("key:{index:02}"), cid_from_seed(index)))
            .collect::<Vec<_>>();
        let root = btree
            .build_links(initial.clone())
            .await
            .unwrap()
            .expect("initial root");

        let noop = btree
            .update_links(
                Some(&root),
                [
                    ("key:05".to_string(), Some(cid_from_seed(5))),
                    ("missing".to_string(), None),
                ],
            )
            .await
            .unwrap()
            .expect("noop root");
        assert_eq!(noop, root);

        let one = btree
            .update_links(
                Some(&root),
                initial
                    .iter()
                    .filter(|(key, _)| key != "key:05")
                    .map(|(key, _)| (key.clone(), None)),
            )
            .await
            .unwrap()
            .expect("single-entry root");
        assert_eq!(
            btree.links_entries(Some(&one)).await.unwrap(),
            vec![("key:05".to_string(), cid_from_seed(5))]
        );
        assert_eq!(btree.count_stored_links(Some(&one)).await.unwrap(), Some(1));

        assert_eq!(
            btree
                .update_links(Some(&root), initial.into_iter().map(|(key, _)| (key, None)),)
                .await
                .unwrap(),
            None
        );
    });
}

#[test]
fn sparse_link_changes_only_read_touched_paths() {
    block_on(async {
        let store = Arc::new(CountingStore::default());
        let btree = BTree::new(Arc::clone(&store), BTreeOptions::default());
        let initial = (0..31_000)
            .map(|index| (format!("key:{index:05}"), cid_from_seed(index)))
            .collect::<Vec<_>>();
        let root = btree
            .build_links(initial)
            .await
            .unwrap()
            .expect("initial root");
        let changes = vec![
            ("key:00001".to_string(), Some(cid_from_seed(40_001))),
            ("key:15500".to_string(), None),
            ("key:30998".to_string(), Some(cid_from_seed(40_002))),
        ];

        store.reset_gets();
        let updated = btree
            .update_links(Some(&root), changes)
            .await
            .unwrap()
            .expect("updated root");
        let update_gets = store.gets();

        assert_eq!(
            btree.get_link(Some(&updated), "key:00001").await.unwrap(),
            Some(cid_from_seed(40_001))
        );
        assert_eq!(
            btree.get_link(Some(&updated), "key:15500").await.unwrap(),
            None
        );
        assert!(
            update_gets < 64,
            "three sparse changes read {update_gets} blobs from a 31,000-entry tree"
        );
    });
}

#[test]
fn sparse_link_changes_do_not_scan_every_legacy_subtree_for_counts() {
    block_on(async {
        let store = Arc::new(CountingStore::default());
        let btree = BTree::new(Arc::clone(&store), BTreeOptions::default());
        let tree = HashTree::new(HashTreeConfig::new(Arc::clone(&store)));
        let root = btree
            .build_links((0..31_000).map(|index| (format!("key:{index:05}"), cid_from_seed(index))))
            .await
            .unwrap()
            .expect("initial root");
        let legacy_root = strip_internal_counts(&tree, root).await.unwrap();
        assert_eq!(
            btree.count_stored_links(Some(&legacy_root)).await.unwrap(),
            None
        );

        store.reset_gets();
        let updated = btree
            .update_links(
                Some(&legacy_root),
                [("key:00001".to_string(), Some(cid_from_seed(40_001)))],
            )
            .await
            .unwrap()
            .expect("updated root");
        let update_gets = store.gets();

        assert_eq!(
            btree.get_link(Some(&updated), "key:00001").await.unwrap(),
            Some(cid_from_seed(40_001))
        );
        assert_eq!(
            btree.count_stored_links(Some(&updated)).await.unwrap(),
            None
        );
        assert!(
            update_gets < 64,
            "one sparse legacy update read {update_gets} blobs from a 31,000-entry tree"
        );
        assert_eq!(btree.scan_links(Some(&updated)).await.unwrap(), 31_000);
    });
}

#[test]
fn dense_link_lookup_reads_each_existing_node_at_most_once() {
    block_on(async {
        let store = Arc::new(CountingStore::default());
        let btree = BTree::new(Arc::clone(&store), BTreeOptions::default());
        let entry_count = 3_100;
        let initial = (0..entry_count)
            .map(|index| (format!("key:{index:05}"), cid_from_seed(index)))
            .collect::<Vec<_>>();
        let root = btree
            .build_links(initial.clone())
            .await
            .unwrap()
            .expect("initial root");

        store.reset_gets();
        let found = btree
            .get_links(Some(&root), initial.into_iter().map(|(key, _)| key))
            .await
            .unwrap();
        let lookup_gets = store.gets();

        assert_eq!(found.len(), entry_count);
        assert!(
            lookup_gets < entry_count / 4,
            "bulk lookup made {lookup_gets} store reads for {entry_count} keys"
        );
    });
}

#[test]
fn dense_link_changes_read_each_existing_node_at_most_once() {
    block_on(async {
        let store = Arc::new(CountingStore::default());
        let btree = BTree::new(Arc::clone(&store), BTreeOptions::default());
        let entry_count = 3_100;
        let initial = (0..entry_count)
            .map(|index| (format!("key:{index:05}"), cid_from_seed(index)))
            .collect::<Vec<_>>();
        let root = btree
            .build_links(initial)
            .await
            .unwrap()
            .expect("initial root");
        let changes = (0..entry_count)
            .map(|index| {
                (
                    format!("key:{index:05}"),
                    Some(cid_from_seed(index + 10_000)),
                )
            })
            .collect::<Vec<_>>();

        store.reset_gets();
        let updated = btree
            .update_links(Some(&root), changes.clone())
            .await
            .unwrap()
            .expect("updated root");

        let update_gets = store.gets();
        assert_eq!(
            btree.count_stored_links(Some(&updated)).await.unwrap(),
            Some(entry_count as u64)
        );
        assert!(
            update_gets < changes.len() / 4,
            "bulk update made {} store reads for {} changes",
            update_gets,
            changes.len()
        );
    });
}

#[test]
fn link_update_reports_only_nodes_superseded_by_the_new_root() {
    block_on(async {
        let store = Arc::new(MemoryStore::new());
        let btree = BTree::new(Arc::clone(&store), BTreeOptions { order: Some(8) });
        let entry_count = 2_000;
        let root = btree
            .build_links(
                (0..entry_count).map(|index| (format!("key:{index:05}"), cid_from_seed(index))),
            )
            .await
            .unwrap()
            .expect("initial root");

        let report = btree
            .update_links_with_superseded(
                Some(&root),
                (800..1_200).map(|index| {
                    (
                        format!("key:{index:05}"),
                        Some(cid_from_seed(index + 10_000)),
                    )
                }),
            )
            .await
            .expect("update links");
        let updated = report.root.expect("updated root");

        assert!(!report.superseded_nodes.is_empty());
        assert!(!report.superseded_nodes.contains(&updated));
        for cid in &report.superseded_nodes {
            store
                .delete(&cid.hash)
                .await
                .expect("delete superseded node");
        }

        assert_eq!(
            btree.count_stored_links(Some(&updated)).await.unwrap(),
            Some(entry_count as u64)
        );
        assert_eq!(
            btree.get_link(Some(&updated), "key:00900").await.unwrap(),
            Some(cid_from_seed(10_900))
        );
    });
}

#[test]
fn dense_link_changes_read_independent_subtrees_concurrently() {
    block_on(async {
        let store = Arc::new(ConcurrentReadStore::default());
        let btree = BTree::new(Arc::clone(&store), BTreeOptions { order: Some(8) });
        let entry_count = 3_500;
        let root = btree
            .build_links(
                (0..entry_count).map(|index| (format!("key:{index:05}"), cid_from_seed(index))),
            )
            .await
            .unwrap()
            .expect("initial root");
        store.start_probe();

        btree
            .update_links(
                Some(&root),
                (0..entry_count).map(|index| {
                    (
                        format!("key:{index:05}"),
                        Some(cid_from_seed(index + 10_000)),
                    )
                }),
            )
            .await
            .expect("update links");

        let max_active_gets = store.max_active_gets.load(Ordering::SeqCst);
        assert!(
            max_active_gets > 1,
            "independent changed subtrees were read serially"
        );
        assert!(
            max_active_gets <= 4,
            "bounded subtree traversal issued {max_active_gets} concurrent reads"
        );
    });
}

#[test]
fn dense_link_changes_honor_single_worker_limit() {
    block_on(async {
        let store = Arc::new(ConcurrentReadStore::default());
        let btree = BTree::new(Arc::clone(&store), BTreeOptions { order: Some(8) })
            .with_update_concurrency(1);
        let entry_count = 3_500;
        let root = btree
            .build_links(
                (0..entry_count).map(|index| (format!("key:{index:05}"), cid_from_seed(index))),
            )
            .await
            .unwrap()
            .expect("initial root");
        store.start_probe();

        btree
            .update_links(
                Some(&root),
                (0..entry_count).map(|index| {
                    (
                        format!("key:{index:05}"),
                        Some(cid_from_seed(index + 10_000)),
                    )
                }),
            )
            .await
            .expect("update links");

        assert_eq!(
            store.max_active_gets.load(Ordering::SeqCst),
            1,
            "single-worker B-tree update issued concurrent reads"
        );
    });
}

#[test]
fn clustered_link_changes_find_deeper_parallel_subtrees() {
    block_on(async {
        let store = Arc::new(ConcurrentReadStore::default());
        let btree = BTree::new(Arc::clone(&store), BTreeOptions { order: Some(8) });
        let entry_count = 20_000;
        let root = btree
            .build_links(
                (0..entry_count).map(|index| (format!("key:{index:05}"), cid_from_seed(index))),
            )
            .await
            .unwrap()
            .expect("initial root");
        store.start_probe();

        btree
            .update_links(
                Some(&root),
                (8_000..9_000).map(|index| {
                    (
                        format!("key:{index:05}"),
                        Some(cid_from_seed(index + 30_000)),
                    )
                }),
            )
            .await
            .expect("update clustered links");

        let max_active_gets = store.max_active_gets.load(Ordering::SeqCst);
        assert!(
            max_active_gets > 1,
            "clustered changes did not parallelize below their shared root branch"
        );
        assert!(
            max_active_gets <= 4,
            "bounded clustered traversal issued {max_active_gets} concurrent reads"
        );
    });
}

#[test]
fn bulk_link_changes_preserve_unknown_counts_without_scanning_legacy_children() {
    block_on(async {
        let store = Arc::new(MemoryStore::new());
        let btree = BTree::new(Arc::clone(&store), BTreeOptions { order: Some(4) });
        let tree = HashTree::new(HashTreeConfig::new(Arc::clone(&store)));
        let root = btree
            .build_links((0..20).map(|index| (format!("key:{index:03}"), cid_from_seed(index))))
            .await
            .unwrap()
            .expect("initial root");
        let legacy_entries = tree
            .list_directory(&root)
            .await
            .unwrap()
            .into_iter()
            .map(|entry| {
                let cid = Cid {
                    hash: entry.hash,
                    key: entry.key,
                };
                DirEntry::from_cid(entry.name, &cid).with_link_type(LinkType::Dir)
            })
            .collect();
        let legacy_root = tree.put_directory(legacy_entries).await.unwrap();
        assert_eq!(
            btree.count_stored_links(Some(&legacy_root)).await.unwrap(),
            None
        );

        let updated = btree
            .update_links(
                Some(&legacy_root),
                [("key:001".to_string(), Some(cid_from_seed(101)))],
            )
            .await
            .unwrap()
            .expect("updated root");

        assert_eq!(
            btree.count_stored_links(Some(&updated)).await.unwrap(),
            None
        );
        assert_eq!(btree.scan_links(Some(&updated)).await.unwrap(), 20);
    });
}

#[test]
fn link_counts_distinguish_stored_counts_from_scans() {
    block_on(async {
        let store = Arc::new(MemoryStore::new());
        let btree = BTree::new(Arc::clone(&store), BTreeOptions { order: Some(4) });
        let tree = HashTree::new(HashTreeConfig::new(Arc::clone(&store)));

        let fixtures = [
            (
                "author1:fffffffffffffff5:event-a",
                cid_from_hex("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"),
            ),
            (
                "author1:fffffffffffffff4:event-b",
                cid_from_hex("fffefdfcfbfaf9f8f7f6f5f4f3f2f1f0efeeedecebeae9e8e7e6e5e4e3e2e1e0"),
            ),
            (
                "author2:fffffffffffffff6:event-c",
                cid_from_hex("00070e151c232a31383f464d545b626970777e858c939aa1a8afb6bdc4cbd2d9"),
            ),
            (
                "author1:00000001:fffffffffffffff3:event-d",
                cid_from_hex("000d1a2734414e5b6875828f9ca9b6c3d0ddeaf704111e2b3845525f6c798693"),
            ),
            (
                "author2:fffffffffffffff1:event-e",
                cid_from_hex("303132333435363738393a3b3c3d3e3f404142434445464748494a4b4c4d4e4f"),
            ),
            (
                "author3:fffffffffffffff2:event-f",
                cid_from_hex("101112131415161718191a1b1c1d1e1f202122232425262728292a2b2c2d2e2f"),
            ),
        ];

        let counted_root = btree
            .build_links(
                fixtures
                    .iter()
                    .map(|(key, cid)| ((*key).to_string(), cid.clone())),
            )
            .await
            .unwrap()
            .expect("counted root");

        assert_eq!(
            btree.count_stored_links(Some(&counted_root)).await.unwrap(),
            Some(fixtures.len() as u64)
        );
        assert_eq!(
            btree.scan_links(Some(&counted_root)).await.unwrap(),
            fixtures.len() as u64
        );
        assert_eq!(
            btree.count_links(Some(&counted_root)).await.unwrap(),
            fixtures.len() as u64
        );

        let left = btree
            .build_links(
                fixtures[..3]
                    .iter()
                    .map(|(key, cid)| ((*key).to_string(), cid.clone())),
            )
            .await
            .unwrap()
            .expect("left leaf");
        let right = btree
            .build_links(
                fixtures[3..]
                    .iter()
                    .map(|(key, cid)| ((*key).to_string(), cid.clone())),
            )
            .await
            .unwrap()
            .expect("right leaf");
        let legacy_root = tree
            .put_directory(vec![
                DirEntry::from_cid(escape_key(fixtures[0].0), &left).with_link_type(LinkType::Dir),
                DirEntry::from_cid(escape_key(fixtures[3].0), &right).with_link_type(LinkType::Dir),
            ])
            .await
            .unwrap();

        assert_eq!(
            btree.count_stored_links(Some(&legacy_root)).await.unwrap(),
            None
        );
        assert_eq!(
            btree.scan_links(Some(&legacy_root)).await.unwrap(),
            fixtures.len() as u64
        );
    });
}

#[test]
fn exhaustive_link_validation_reads_every_node_and_reports_leaf_samples() {
    block_on(async {
        let store = Arc::new(CountingStore::default());
        let btree = BTree::new(Arc::clone(&store), BTreeOptions { order: Some(8) });
        let entry_count = 2_000usize;
        let root = btree
            .build_links(
                (0..entry_count).map(|index| (format!("key:{index:05}"), cid_from_seed(index))),
            )
            .await
            .unwrap()
            .expect("built root");

        store.reset_gets();
        let validation = btree.validate_link_tree(Some(&root)).await.unwrap();
        assert_eq!(validation.links, entry_count as u64);
        assert!(validation.nodes > 1);
        assert_eq!(
            validation.first,
            Some(("key:00000".to_string(), cid_from_seed(0)))
        );
        assert_eq!(
            validation.last,
            Some((
                format!("key:{:05}", entry_count - 1),
                cid_from_seed(entry_count - 1)
            ))
        );
        assert_eq!(store.gets(), validation.nodes as usize);
    });
}

#[test]
fn exhaustive_link_validation_rejects_a_missing_descendant() {
    block_on(async {
        let store = Arc::new(MemoryStore::new());
        let btree = BTree::new(Arc::clone(&store), BTreeOptions { order: Some(4) });
        let tree = HashTree::new(HashTreeConfig::new(Arc::clone(&store)));
        let root = btree
            .build_links((0..100).map(|index| (format!("key:{index:03}"), cid_from_seed(index))))
            .await
            .unwrap()
            .expect("built root");
        let root_entries = tree.list_directory(&root).await.unwrap();
        let missing_child = root_entries
            .first()
            .filter(|entry| entry.link_type == LinkType::Dir)
            .expect("multi-level root");
        store.delete(&missing_child.hash).await.unwrap();

        let error = btree.validate_link_tree(Some(&root)).await.unwrap_err();
        assert!(
            error.to_string().contains("is empty"),
            "unexpected validation error: {error}"
        );
    });
}

#[test]
fn exhaustive_value_validation_reads_every_node_and_reports_key_bounds() {
    block_on(async {
        let store = Arc::new(CountingStore::default());
        let btree = BTree::new(Arc::clone(&store), BTreeOptions { order: Some(8) });
        let entry_count = 2_000usize;
        let root = btree
            .build(
                (0..entry_count)
                    .map(|index| (format!("key:{index:05}"), format!("value:{index:05}"))),
            )
            .await
            .unwrap()
            .expect("built root");

        store.reset_gets();
        let validation = btree.validate_value_tree(Some(&root)).await.unwrap();
        assert_eq!(validation.entries, entry_count as u64);
        assert!(validation.nodes > 1);
        assert_eq!(validation.first, Some("key:00000".to_string()));
        assert_eq!(validation.last, Some(format!("key:{:05}", entry_count - 1)));
        assert_eq!(store.gets(), validation.nodes as usize);
    });
}

#[test]
fn bulk_string_build_matches_incremental_entries() {
    block_on(async {
        let store = Arc::new(MemoryStore::new());
        let btree = BTree::new(Arc::clone(&store), BTreeOptions { order: Some(4) });

        let fixtures = [
            ("profile:petri:1", r#"{"name":"Petri","score":1}"#),
            ("profile:petri:2", r#"{"name":"Petri Lampinen","score":2}"#),
            ("profile:jack:1", r#"{"name":"jack","score":3}"#),
            ("profile:mil:1", r#"{"name":"Michael Miller","score":4}"#),
            ("profile:mil:2", r#"{"name":"Milad","score":5}"#),
            ("profile:sirius:1", r#"{"name":"Sirius","score":6}"#),
        ];

        let mut incremental_root = None;
        for (key, value) in fixtures.iter() {
            incremental_root = Some(
                btree
                    .insert(incremental_root.as_ref(), key, value)
                    .await
                    .unwrap(),
            );
        }

        let bulk_root = btree
            .build(
                fixtures
                    .iter()
                    .map(|(key, value)| ((*key).to_string(), (*value).to_string())),
            )
            .await
            .unwrap()
            .expect("bulk root");
        let incremental_root = incremental_root.expect("incremental root");

        assert_eq!(
            btree.entries(Some(&bulk_root)).await.unwrap(),
            btree.entries(Some(&incremental_root)).await.unwrap()
        );
        assert_eq!(
            btree.prefix(&bulk_root, "profile:mil:").await.unwrap(),
            btree
                .prefix(&incremental_root, "profile:mil:")
                .await
                .unwrap()
        );
    });
}

#[test]
fn escaping_matches_typescript() {
    assert_eq!(escape_key("a/b%c\0"), "a%2Fb%25c%00");
}
