use std::sync::Arc;

use futures::executor::block_on;
use hashtree_core::{Cid, DirEntry, HashTree, HashTreeConfig, LinkType, MemoryStore, Store};
use hashtree_index::{BTree, BTreeOptions, LinkComparisonLimits, LinkTreeVerifier};

fn value(n: usize) -> Cid {
    Cid {
        hash: [(n % 251) as u8; 32],
        key: None,
    }
}
async fn fixture(count: usize) -> (Arc<MemoryStore>, Cid, Cid) {
    let store = Arc::new(MemoryStore::new());
    let bulk = BTree::new(store.clone(), BTreeOptions { order: Some(64) });
    let old = bulk
        .build_links((0..count).map(|n| (format!("{n:06}"), value(n))))
        .await
        .unwrap()
        .unwrap();
    let writer = BTree::new(store.clone(), BTreeOptions::default());
    let new = writer
        .update_links(Some(&old), [("000017".into(), Some(value(17)))])
        .await
        .unwrap()
        .unwrap();
    assert_ne!(
        old, new,
        "order64 fixture must actually rewrite non-ID paths"
    );
    (store, old, new)
}
async fn check(store: Arc<MemoryStore>, old: &Cid, new: &Cid) -> bool {
    LinkTreeVerifier::new(store, LinkComparisonLimits::default())
        .compare(old, new, None)
        .await
        .is_ok()
}
async fn root_with(store: Arc<MemoryStore>, entries: Vec<DirEntry>) -> Cid {
    HashTree::new(HashTreeConfig::new(store))
        .put_directory(entries)
        .await
        .unwrap()
}
async fn altered_root(
    store: Arc<MemoryStore>,
    cid: &Cid,
    edit: impl FnOnce(&mut Vec<DirEntry>),
) -> Cid {
    let tree = HashTree::new(HashTreeConfig::new(store));
    let mut entries = tree
        .list_directory_required(cid)
        .await
        .unwrap()
        .into_iter()
        .map(|entry| {
            DirEntry::from_cid(
                entry.name,
                &Cid {
                    hash: entry.hash,
                    key: entry.key,
                },
            )
            .with_size(entry.size)
            .with_link_type(entry.link_type)
        })
        .collect();
    edit(&mut entries);
    tree.put_directory(entries).await.unwrap()
}

#[test]
fn equivalent_order64_to32_rewrites_preserve_all_mappings_and_prune_shared_subtrees() {
    block_on(async {
        let (store, old, new) = fixture(4000).await;
        let report = LinkTreeVerifier::new(store, LinkComparisonLimits::default())
            .compare(&old, &new, None)
            .await
            .unwrap();
        assert_eq!(report.before_count, 4000);
        assert_eq!(report.after_count, 4000);
        assert!(report.opaque_subtrees > 0);
    });
}
#[test]
fn by_id_requires_exact_one_insertion_with_original_full_cid() {
    block_on(async {
        let (store, old, _) = fixture(100).await;
        let index = BTree::new(store.clone(), BTreeOptions::default());
        let missing = index
            .update_links(Some(&old), [("000017".into(), None)])
            .await
            .unwrap()
            .unwrap();
        let repaired = index
            .insert_link(Some(&missing), "000017", &value(17))
            .await
            .unwrap();
        let mut verifier = LinkTreeVerifier::new(store.clone(), LinkComparisonLimits::default());
        assert_eq!(
            verifier
                .compare(&missing, &repaired, Some(("000017", &value(17))))
                .await
                .unwrap()
                .after_count,
            100
        );
        assert!(verifier
            .compare(&missing, &repaired, Some(("000017", &value(18))))
            .await
            .is_err());
        assert!(verifier
            .compare(&old, &repaired, Some(("000017", &value(17))))
            .await
            .is_err());
    });
}
#[test]
fn changed_deleted_and_extra_mapping_are_rejected() {
    block_on(async {
        let (store, old, _) = fixture(100).await;
        let index = BTree::new(store.clone(), BTreeOptions::default());
        for (key, change) in [
            ("000017", Some(value(18))),
            ("000017", None),
            ("000100", Some(value(100))),
        ] {
            let new = index
                .update_links(Some(&old), [(key.into(), change)])
                .await
                .unwrap()
                .unwrap();
            assert!(!check(store.clone(), &old, &new).await);
        }
        let missing = index
            .update_links(Some(&old), [("000017".into(), None)])
            .await
            .unwrap()
            .unwrap();
        let extra = index
            .update_links(Some(&old), [("000100".into(), Some(value(100)))])
            .await
            .unwrap()
            .unwrap();
        assert!(
            LinkTreeVerifier::new(store, LinkComparisonLimits::default())
                .compare(&missing, &extra, Some(("000017", &value(17))))
                .await
                .is_err()
        );
    });
}
#[test]
fn changed_full_cid_key_is_not_pruned_or_accepted() {
    block_on(async {
        let (store, old, _) = fixture(100).await;
        let mut wrong = value(17);
        wrong.key = Some([8; 32].into());
        let index = BTree::new(store.clone(), BTreeOptions::default());
        let new = index
            .update_links(Some(&old), [("000017".into(), Some(wrong))])
            .await
            .unwrap()
            .unwrap();
        assert!(!check(store, &old, &new).await);
    });
}
#[test]
fn forged_nonzero_child_count_is_rejected_even_for_identical_child_cid() {
    block_on(async {
        let (store, old, new) = fixture(100).await;
        let bad = altered_root(store.clone(), &new, |entries| entries[0].size += 1).await;
        assert!(!check(store, &old, &bad).await);
    });
}
#[test]
fn legacy_unknown_counts_expand_and_are_derived() {
    block_on(async {
        let (store, old, new) = fixture(100).await;
        let legacy = altered_root(store.clone(), &old, |entries| {
            for entry in entries {
                entry.size = 0;
            }
        })
        .await;
        let report = LinkTreeVerifier::new(store, LinkComparisonLimits::default())
            .compare(&legacy, &new, None)
            .await
            .unwrap();
        assert_eq!(report.before_count, 100);
        assert_eq!(report.after_count, 100);
    });
}
#[test]
fn forged_child_separator_and_range_are_rejected() {
    block_on(async {
        let (store, old, new) = fixture(100).await;
        for ordinal in [0, 1] {
            let bad = altered_root(store.clone(), &new, |entries| {
                entries[ordinal].name.push('x');
            })
            .await;
            assert!(!check(store.clone(), &old, &bad).await);
        }
    });
}
#[test]
fn mixed_unnamed_duplicate_noncanonical_escaped_and_metadata_links_reject() {
    block_on(async {
        let store = Arc::new(MemoryStore::new());
        let old = root_with(
            store.clone(),
            vec![DirEntry::from_cid("a", &value(1)).with_link_type(LinkType::File)],
        )
        .await;
        let file = || DirEntry::from_cid("a", &value(1)).with_link_type(LinkType::File);
        for entries in [
            vec![
                file(),
                DirEntry::from_cid("b", &value(2)).with_link_type(LinkType::Dir),
            ],
            vec![file(), file()],
            vec![DirEntry::from_cid("", &value(1)).with_link_type(LinkType::File)],
            vec![DirEntry::from_cid("x%2f", &value(1)).with_link_type(LinkType::File)],
            vec![file().with_meta(Default::default())],
            vec![file().with_size(1)],
        ] {
            let bad = root_with(store.clone(), entries).await;
            assert!(!check(store.clone(), &old, &bad).await);
        }
        let node = hashtree_core::TreeNode {
            node_type: LinkType::Dir,
            links: vec![hashtree_core::Link::new(value(1).hash).with_link_type(LinkType::File)],
        };
        let bytes = hashtree_core::encode_tree_node(&node).unwrap();
        let hash = hashtree_core::sha256(&bytes);
        store.put(hash, bytes).await.unwrap();
        assert!(!check(store, &old, &Cid { hash, key: None }).await);
    });
}
#[test]
fn missing_changed_nodes_and_bad_hashes_reject() {
    block_on(async {
        let (store, old, new) = fixture(100).await;
        let bytes = store.get(&new.hash).await.unwrap().unwrap();
        store.delete(&new.hash).await.unwrap();
        assert!(!check(store.clone(), &old, &new).await);
        store.put(new.hash, vec![1, 2, 3]).await.unwrap();
        assert!(!check(store.clone(), &old, &new).await);
        store.delete(&new.hash).await.unwrap();
        store.put(new.hash, bytes).await.unwrap();
        assert!(check(store, &old, &new).await);
    });
}
#[test]
fn each_comparison_budget_is_fatal_and_shared_across_projections() {
    block_on(async {
        let (store, old, new) = fixture(100).await;
        let base = LinkComparisonLimits::default();
        for limits in [
            LinkComparisonLimits { nodes: 1, ..base },
            LinkComparisonLimits { bytes: 1, ..base },
            LinkComparisonLimits {
                block_bytes: 1,
                ..base
            },
            LinkComparisonLimits { depth: 0, ..base },
            LinkComparisonLimits {
                links_per_node: 1,
                ..base
            },
            LinkComparisonLimits {
                expanded_links: 1,
                ..base
            },
        ] {
            assert!(LinkTreeVerifier::new(store.clone(), limits)
                .compare(&old, &new, None)
                .await
                .is_err());
        }
        // Identical positive-count child spans must not evade the depth cap.
        assert!(
            LinkTreeVerifier::new(store.clone(), LinkComparisonLimits { depth: 0, ..base })
                .compare(&old, &old, None)
                .await
                .is_err()
        );
        let mut verifier = LinkTreeVerifier::new(
            store,
            LinkComparisonLimits {
                expanded_links: 200,
                ..base
            },
        );
        verifier.compare(&old, &new, None).await.unwrap();
        assert!(verifier.compare(&old, &new, None).await.is_err());
    });
}
#[test]
fn empty_root_insertion_and_empty_child_rejection() {
    block_on(async {
        let store = Arc::new(MemoryStore::new());
        let empty = root_with(store.clone(), vec![]).await;
        let full = root_with(
            store.clone(),
            vec![DirEntry::from_cid("a", &value(1)).with_link_type(LinkType::File)],
        )
        .await;
        assert!(
            LinkTreeVerifier::new(store.clone(), LinkComparisonLimits::default())
                .compare(&empty, &full, Some(("a", &value(1))))
                .await
                .is_ok()
        );
        let bad = root_with(
            store.clone(),
            vec![DirEntry::from_cid("a", &empty).with_link_type(LinkType::Dir)],
        )
        .await;
        assert!(!check(store.clone(), &empty, &bad).await);
        let wrapped = root_with(
            store.clone(),
            vec![DirEntry::from_cid("a", &full)
                .with_size(1)
                .with_link_type(LinkType::Dir)],
        )
        .await;
        assert!(check(store.clone(), &full, &wrapped).await);
        assert!(LinkTreeVerifier::new(
            store.clone(),
            LinkComparisonLimits {
                depth: 0,
                ..Default::default()
            }
        )
        .compare(&wrapped, &wrapped, None)
        .await
        .is_err());
        let twice = root_with(
            store.clone(),
            vec![DirEntry::from_cid("a", &wrapped)
                .with_size(1)
                .with_link_type(LinkType::Dir)],
        )
        .await;
        assert!(LinkTreeVerifier::new(
            store,
            LinkComparisonLimits {
                depth: 1,
                ..Default::default()
            }
        )
        .compare(&wrapped, &twice, None)
        .await
        .is_err());
    });
}
#[test]
fn decoded_utf8_keys_and_escaped_percent_slash_round_trip() {
    block_on(async {
        let store = Arc::new(MemoryStore::new());
        let keys = ["a/b", "a%b", "a\0b", "\u{e000}", "\u{10000}"];
        let index = BTree::new(store.clone(), BTreeOptions { order: Some(3) });
        let old = index
            .build_links(
                keys.iter()
                    .enumerate()
                    .map(|(i, key)| (key.to_string(), value(i))),
            )
            .await
            .unwrap()
            .unwrap();
        let new = BTree::new(store.clone(), BTreeOptions::default())
            .update_links(Some(&old), [("a/b".to_string(), Some(value(0)))])
            .await
            .unwrap()
            .unwrap();
        assert!(check(store, &old, &new).await);
    });
}

#[test]
fn noncanonical_raw_encoding_and_plaintext_with_a_forged_chk_key_reject() {
    block_on(async {
        let store = Arc::new(MemoryStore::new());
        let node = hashtree_core::TreeNode {
            node_type: LinkType::Dir,
            links: vec![hashtree_core::Link::new(value(1).hash)
                .with_name("a")
                .with_link_type(LinkType::File)],
        };
        let bytes = hashtree_core::encode_tree_node(&node).unwrap();
        let hash = hashtree_core::sha256(&bytes);
        store.put(hash, bytes.clone()).await.unwrap();
        let valid = Cid { hash, key: None };
        let mut extra = bytes;
        extra.push(0);
        let extra_hash = hashtree_core::sha256(&extra);
        store.put(extra_hash, extra).await.unwrap();
        assert!(
            !check(
                store.clone(),
                &valid,
                &Cid {
                    hash: extra_hash,
                    key: None
                }
            )
            .await
        );
        // The general reader permits historical plaintext with a key; the
        // repair proof deliberately requires authenticated, canonical CHK.
        assert!(
            !check(
                store,
                &valid,
                &Cid {
                    hash,
                    key: Some(hash.into())
                }
            )
            .await
        );
    });
}
