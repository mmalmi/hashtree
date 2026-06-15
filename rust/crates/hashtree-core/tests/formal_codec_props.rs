use std::collections::HashMap;

use hashtree_core::{decode_tree_node, encode_tree_node, Link, LinkType, TreeNode};
use proptest::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value};

fn arb_hash() -> impl Strategy<Value = [u8; 32]> {
    any::<[u8; 32]>()
}

fn arb_json_value() -> impl Strategy<Value = Value> {
    prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(|n| Value::Number(Number::from(n))),
        "[a-z0-9_\\-]{0,16}".prop_map(Value::String),
    ]
}

fn arb_meta() -> impl Strategy<Value = Option<HashMap<String, Value>>> {
    prop::option::of(
        prop::collection::vec(
            ("[a-z]{1,6}".prop_map(|s| s.to_string()), arb_json_value()),
            0..6,
        )
        .prop_map(|entries| entries.into_iter().collect::<HashMap<_, _>>()),
    )
}

fn arb_link_type() -> impl Strategy<Value = LinkType> {
    prop_oneof![
        Just(LinkType::Blob),
        Just(LinkType::File),
        Just(LinkType::Dir),
    ]
}

fn arb_link() -> impl Strategy<Value = Link> {
    (
        arb_hash(),
        prop::option::of("[a-z0-9_\\-.]{1,16}"),
        any::<u16>().prop_map(u64::from),
        prop::option::of(any::<[u8; 32]>()),
        arb_link_type(),
        arb_meta(),
    )
        .prop_map(|(hash, name, size, key, link_type, meta)| Link {
            hash,
            name,
            size,
            key,
            link_type,
            meta,
        })
}

fn arb_node() -> impl Strategy<Value = TreeNode> {
    (
        prop_oneof![Just(LinkType::File), Just(LinkType::Dir)],
        prop::collection::vec(arb_link(), 0..8),
    )
        .prop_map(|(node_type, links)| TreeNode { node_type, links })
}

fn normalize_for_encoding(mut node: TreeNode) -> TreeNode {
    if node.node_type == LinkType::Dir {
        node.links
            .sort_by(|left, right| dir_link_sort_name(left).cmp(dir_link_sort_name(right)));
    }
    node
}

fn dir_link_sort_name(link: &Link) -> &str {
    link.name.as_deref().unwrap_or("")
}

proptest! {
    #[test]
    fn prop_decode_encode_roundtrip(node in arb_node()) {
        let encoded = encode_tree_node(&node).unwrap();
        let decoded = decode_tree_node(&encoded).unwrap();
        prop_assert_eq!(decoded, normalize_for_encoding(node));
    }

    #[test]
    fn prop_encode_deterministic(node in arb_node()) {
        let encoded1 = encode_tree_node(&node).unwrap();
        let encoded2 = encode_tree_node(&node).unwrap();
        let encoded3 = encode_tree_node(&node).unwrap();
        prop_assert_eq!(encoded1, encoded2.clone());
        prop_assert_eq!(encoded2, encoded3);
    }
}

#[test]
fn test_metadata_insertion_order_does_not_change_encoding() {
    let mut meta1 = HashMap::new();
    meta1.insert("a".to_string(), Value::String("1".to_string()));
    meta1.insert("b".to_string(), Value::String("2".to_string()));
    meta1.insert("c".to_string(), Value::String("3".to_string()));

    let mut meta2 = HashMap::new();
    meta2.insert("c".to_string(), Value::String("3".to_string()));
    meta2.insert("a".to_string(), Value::String("1".to_string()));
    meta2.insert("b".to_string(), Value::String("2".to_string()));

    let link1 = Link {
        hash: [7u8; 32],
        name: Some("x".to_string()),
        size: 1,
        key: None,
        link_type: LinkType::Blob,
        meta: Some(meta1),
    };
    let link2 = Link {
        hash: [7u8; 32],
        name: Some("x".to_string()),
        size: 1,
        key: None,
        link_type: LinkType::Blob,
        meta: Some(meta2),
    };

    let node1 = TreeNode::dir(vec![link1]);
    let node2 = TreeNode::dir(vec![link2]);

    let enc1 = encode_tree_node(&node1).unwrap();
    let enc2 = encode_tree_node(&node2).unwrap();
    assert_eq!(enc1, enc2);
}

#[test]
fn test_directory_link_order_does_not_change_encoding() {
    let node1 = TreeNode::dir(vec![
        Link::new([2u8; 32]).with_name("zebra").with_size(2),
        Link::new([1u8; 32]).with_name("apple").with_size(1),
    ]);
    let node2 = TreeNode::dir(vec![
        Link::new([1u8; 32]).with_name("apple").with_size(1),
        Link::new([2u8; 32]).with_name("zebra").with_size(2),
    ]);

    let enc1 = encode_tree_node(&node1).unwrap();
    let enc2 = encode_tree_node(&node2).unwrap();
    assert_eq!(enc1, enc2);
}

#[test]
fn test_file_link_order_remains_semantic() {
    let node1 = TreeNode::file(vec![
        Link::new([1u8; 32]).with_size(1),
        Link::new([2u8; 32]).with_size(1),
    ]);
    let node2 = TreeNode::file(vec![
        Link::new([2u8; 32]).with_size(1),
        Link::new([1u8; 32]).with_size(1),
    ]);

    let enc1 = encode_tree_node(&node1).unwrap();
    let enc2 = encode_tree_node(&node2).unwrap();
    assert_ne!(enc1, enc2);
}

#[derive(Serialize, Deserialize)]
struct RawWireLink {
    #[serde(with = "serde_bytes")]
    h: Vec<u8>,
    s: u64,
    t: u8,
}

#[derive(Serialize, Deserialize)]
struct RawWireTreeNode {
    l: Vec<RawWireLink>,
    t: u8,
}

#[test]
fn test_invalid_node_type_rejected() {
    let raw = RawWireTreeNode {
        l: vec![RawWireLink {
            h: vec![0u8; 32],
            s: 1,
            t: 0,
        }],
        t: 9,
    };

    let bytes = rmp_serde::to_vec_named(&raw).unwrap();
    let err = decode_tree_node(&bytes).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("Invalid node type"),
        "unexpected error message: {msg}"
    );
}

#[test]
fn test_unknown_link_type_defaults_to_blob() {
    let raw = RawWireTreeNode {
        l: vec![RawWireLink {
            h: vec![1u8; 32],
            s: 42,
            t: 99,
        }],
        t: 2,
    };

    let bytes = rmp_serde::to_vec_named(&raw).unwrap();
    let decoded = decode_tree_node(&bytes).unwrap();
    assert_eq!(decoded.node_type, LinkType::Dir);
    assert_eq!(decoded.links.len(), 1);
    assert_eq!(decoded.links[0].link_type, LinkType::Blob);
}
