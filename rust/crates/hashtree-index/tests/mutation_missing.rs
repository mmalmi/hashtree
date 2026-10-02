use async_trait::async_trait;
use futures::executor::block_on;
use hashtree_core::{
    Cid, DirEntry, Hash, HashTree, HashTreeConfig, HashTreeError, LinkType, MemoryStore, Store,
    StoreError,
};
use hashtree_index::{BTree, BTreeError, BTreeOptions};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

fn target(n: u8) -> Cid {
    Cid {
        hash: [n; 32],
        key: None,
    }
}
fn missing<T: std::fmt::Debug>(result: Result<T, BTreeError>, cid: &Cid) {
    assert!(
        matches!(result, Err(BTreeError::HashTree(HashTreeError::MissingChunk(ref hash))) if hash == &hex::encode(cid.hash)),
        "expected missing existing content: {result:?}"
    );
}
async fn first_leaf(tree: &HashTree<MemoryStore>, root: &Cid) -> Cid {
    let mut node = root.clone();
    loop {
        let entries = tree.list_directory(&node).await.unwrap();
        let child = entries.first().unwrap();
        if child.link_type != LinkType::Dir {
            return node;
        }
        node = Cid {
            hash: child.hash,
            key: child.key,
        };
    }
}

#[test]
fn bulk_link_update_rejects_missing_existing_root_instead_of_replacing_it() {
    block_on(async {
        let store = Arc::new(MemoryStore::new());
        let index = BTree::new(store.clone(), BTreeOptions { order: Some(4) });
        let root = index
            .build_links([("a".into(), target(1)), ("b".into(), target(2))])
            .await
            .unwrap()
            .unwrap();
        let bytes = store.get(&root.hash).await.unwrap().unwrap();
        store.delete(&root.hash).await.unwrap();
        missing(
            index
                .update_links_with_superseded(Some(&root), [("c".into(), Some(target(3)))])
                .await,
            &root,
        );
        store.put(root.hash, bytes).await.unwrap();
        let next = index
            .update_links(Some(&root), [("c".into(), Some(target(3)))])
            .await
            .unwrap()
            .unwrap();
        for (key, n) in [("a", 1), ("b", 2), ("c", 3)] {
            assert_eq!(
                index.get_link(Some(&next), key).await.unwrap(),
                Some(target(n))
            );
        }
        assert_eq!(index.count_links(Some(&root)).await.unwrap(), 2);
    });
}

#[test]
fn bulk_link_update_rejects_missing_child_on_a_split_path() {
    block_on(async {
        let store = Arc::new(MemoryStore::new());
        let index = BTree::new(store.clone(), BTreeOptions { order: Some(4) });
        let tree = HashTree::new(HashTreeConfig::new(store.clone()));
        let root = index
            .build_links((0..24).map(|i| (format!("k{i:03}"), target(i))))
            .await
            .unwrap()
            .unwrap();
        let leaf = first_leaf(&tree, &root).await;
        assert_ne!(leaf, root);
        let bytes = store.get(&leaf.hash).await.unwrap().unwrap();
        store.delete(&leaf.hash).await.unwrap();
        let changes = (0..12)
            .map(|i| (format!("a{i:03}"), Some(target(i + 40))))
            .collect::<Vec<_>>();
        missing(
            index.update_links(Some(&root), changes.clone()).await,
            &leaf,
        );
        store.put(leaf.hash, bytes).await.unwrap();
        let next = index
            .update_links(Some(&root), changes)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(index.count_links(Some(&next)).await.unwrap(), 36);
        for i in 0..24 {
            assert_eq!(
                index
                    .get_link(Some(&next), &format!("k{i:03}"))
                    .await
                    .unwrap(),
                Some(target(i))
            );
        }
        assert_eq!(index.count_links(Some(&root)).await.unwrap(), 24);
    });
}

#[test]
fn bulk_string_update_rejects_missing_root_and_descendant() {
    block_on(async {
        for remove_root in [true, false] {
            let store = Arc::new(MemoryStore::new());
            let index = BTree::new(store.clone(), BTreeOptions { order: Some(4) });
            let tree = HashTree::new(HashTreeConfig::new(store.clone()));
            let root = index
                .build((0..12).map(|i| (format!("k{i:03}"), format!("value{i}"))))
                .await
                .unwrap()
                .unwrap();
            let removed = if remove_root {
                root.clone()
            } else {
                first_leaf(&tree, &root).await
            };
            store.delete(&removed.hash).await.unwrap();
            missing(
                index
                    .update(Some(&root), [("a".into(), Some("new".into()))])
                    .await,
                &removed,
            );
        }
    });
}

#[test]
fn bulk_string_update_never_drops_an_unavailable_existing_value() {
    block_on(async {
        let store = Arc::new(MemoryStore::new());
        let index = BTree::new(store.clone(), BTreeOptions { order: Some(4) });
        let tree = HashTree::new(HashTreeConfig::new(store.clone()));
        let root = index
            .build([("a".into(), "old".into()), ("b".into(), "retained".into())])
            .await
            .unwrap()
            .unwrap();
        let entry = tree
            .list_directory(&root)
            .await
            .unwrap()
            .into_iter()
            .find(|e| e.name == "a")
            .unwrap();
        let value = Cid {
            hash: entry.hash,
            key: entry.key,
        };
        store.delete(&value.hash).await.unwrap();
        missing(
            index
                .update(Some(&root), [("c".into(), Some("new".into()))])
                .await,
            &value,
        );
        assert_eq!(
            index.get(Some(&root), "b").await.unwrap().as_deref(),
            Some("retained")
        );
    });
}

#[test]
fn single_entry_mutations_reject_missing_existing_nodes() {
    block_on(async {
        for remove_root in [true, false] {
            let store = Arc::new(MemoryStore::new());
            let index = BTree::new(store.clone(), BTreeOptions { order: Some(4) });
            let tree = HashTree::new(HashTreeConfig::new(store.clone()));
            let root = index
                .build_links((0..12).map(|i| (format!("k{i:03}"), target(i))))
                .await
                .unwrap()
                .unwrap();
            let removed = if remove_root {
                root.clone()
            } else {
                first_leaf(&tree, &root).await
            };
            store.delete(&removed.hash).await.unwrap();
            missing(
                index.insert_link(Some(&root), "a", &target(60)).await,
                &removed,
            );
            missing(
                index
                    .insert_link_unchecked(Some(&root), "a", &target(60))
                    .await,
                &removed,
            );
            missing(index.insert(Some(&root), "a", "value").await, &removed);
            missing(index.delete(&root, "k000").await, &removed);
        }
    });
}

#[derive(Default)]
struct MissingReadStore {
    inner: MemoryStore,
    target: Mutex<Option<Hash>>,
    reads: AtomicUsize,
    omit_at: AtomicUsize,
}
#[async_trait]
impl Store for MissingReadStore {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        self.inner.put(hash, data).await
    }
    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        let targeted = self.target.lock().unwrap().as_ref() == Some(hash);
        if targeted
            && self.reads.fetch_add(1, Ordering::SeqCst) + 1 == self.omit_at.load(Ordering::SeqCst)
        {
            return Ok(None);
        }
        self.inner.get(hash).await
    }
    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.inner.has(hash).await
    }
    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.inner.delete(hash).await
    }
}

#[test]
fn single_entry_mutation_reload_is_strict_after_successful_descent() {
    block_on(async {
        for deleting in [false, true] {
            let store = Arc::new(MissingReadStore::default());
            let index = BTree::new(store.clone(), BTreeOptions { order: Some(4) });
            let root = index
                .build_links([("a".into(), target(1)), ("b".into(), target(2))])
                .await
                .unwrap()
                .unwrap();
            *store.target.lock().unwrap() = Some(root.hash);
            // Insert: get_link, descent, mutation reload. Delete: descent, reload.
            store
                .omit_at
                .store(if deleting { 2 } else { 3 }, Ordering::SeqCst);
            if deleting {
                missing(index.delete(&root, "a").await, &root);
            } else {
                missing(index.insert_link(Some(&root), "c", &target(3)).await, &root);
            }
            assert_eq!(
                index.get_link(Some(&root), "a").await.unwrap(),
                Some(target(1))
            );
            assert_eq!(
                index.get_link(Some(&root), "b").await.unwrap(),
                Some(target(2))
            );
        }
    });
}

#[test]
fn mutation_count_fallback_rejects_an_unavailable_uncounted_sibling() {
    block_on(async {
        let store = Arc::new(MemoryStore::new());
        let index = BTree::new(store.clone(), BTreeOptions { order: Some(4) });
        let tree = HashTree::new(HashTreeConfig::new(store.clone()));
        let left = index
            .build_links([("a".into(), target(1))])
            .await
            .unwrap()
            .unwrap();
        let right = index
            .build_links([("z".into(), target(2))])
            .await
            .unwrap()
            .unwrap();
        let root = tree
            .put_directory(vec![
                DirEntry::from_cid("a", &left).with_link_type(LinkType::Dir),
                DirEntry::from_cid("z", &right).with_link_type(LinkType::Dir),
            ])
            .await
            .unwrap();
        store.delete(&right.hash).await.unwrap();
        missing(
            index.insert_link(Some(&root), "b", &target(3)).await,
            &right,
        );
        // Ordinary read behavior remains outside this mutation-only change.
        assert_eq!(index.count_links(Some(&root)).await.unwrap(), 1);
    });
}

#[test]
fn legitimate_empty_trees_splits_and_unchanged_values_remain_supported() {
    block_on(async {
        let store = Arc::new(MemoryStore::new());
        let index = BTree::new(store.clone(), BTreeOptions { order: Some(4) });
        assert_eq!(
            index
                .update_links(None, Vec::<(String, Option<Cid>)>::new())
                .await
                .unwrap(),
            None
        );
        let tree = HashTree::new(HashTreeConfig::new(store));
        let empty = tree.put_directory(vec![]).await.unwrap();
        let mut root = index
            .insert_link(Some(&empty), "k000", &target(0))
            .await
            .unwrap();
        assert_eq!(
            index
                .insert_link(Some(&root), "k000", &target(0))
                .await
                .unwrap(),
            root
        );
        for i in 1..24 {
            root = index
                .insert_link(Some(&root), &format!("k{i:03}"), &target(i))
                .await
                .unwrap();
        }
        for i in 0..24 {
            assert_eq!(
                index
                    .get_link(Some(&root), &format!("k{i:03}"))
                    .await
                    .unwrap(),
                Some(target(i))
            );
        }
        for i in 0..23 {
            root = index
                .delete(&root, &format!("k{i:03}"))
                .await
                .unwrap()
                .unwrap();
        }
        assert_eq!(index.delete(&root, "k023").await.unwrap(), None);
        let strings = index
            .update(None, [("a".into(), Some("value".into()))])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            index.insert(Some(&strings), "a", "value").await.unwrap(),
            strings
        );
        assert_eq!(
            index
                .update(Some(&strings), [("a".into(), None)])
                .await
                .unwrap(),
            None
        );
    });
}
