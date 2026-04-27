use std::sync::Arc;

use futures::executor::block_on;
use hashtree_core::{Cid, HashTree, HashTreeConfig, MemoryStore};
use hashtree_index::{escape_key, BTree, BTreeOptions};

fn cid_from_hex(hex: &str) -> Cid {
    let bytes = hex::decode(hex).unwrap();
    let hash: [u8; 32] = bytes.try_into().unwrap();
    Cid { hash, key: None }
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
            "3107fabdefe0b5e58650caf14c891af6f6c7c08ebebb2549dafc4c7c83965407"
        );
        assert_eq!(
            root.key.map(hex::encode),
            Some("7dcc2db7539c3d2f29952d60fe57b875ccb40e1da55f7d5decb7566c95e5c248".to_string())
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
