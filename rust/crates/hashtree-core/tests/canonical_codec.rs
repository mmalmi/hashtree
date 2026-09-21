use hashtree_core::{decode_tree_node, encode_tree_node, sha256, Link, TreeNode};
use serde_json::{json, Value};

fn directory(metadata: Value) -> TreeNode {
    TreeNode::dir(vec![Link::new([0xab; 32]).with_name("x").with_meta(
        metadata
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    )])
}

#[test]
fn shared_directory_bytes_and_hash() {
    let fixture: Value = serde_json::from_str(include_str!("canonical-directory.json")).unwrap();
    let encoded = encode_tree_node(&directory(fixture["metadata"].clone())).unwrap();
    assert_eq!(hex::encode(&encoded), fixture["msgpack"].as_str().unwrap());
    assert_eq!(
        hex::encode(sha256(&encoded)),
        fixture["sha256"].as_str().unwrap()
    );
}

#[test]
fn integral_floats_and_negative_zero_have_one_encoding() {
    for (float, integer) in [
        (1.0, 1_i64),
        (-0.0, 0),
        (4294967296.0, 4294967296),
        (9007199254740992.0, 9007199254740992),
        (-9223372036854775808.0, i64::MIN),
    ] {
        let left = directory(json!({"nested": [{"n": float}]}));
        let right = directory(json!({"nested": [{"n": integer}]}));
        assert_eq!(
            encode_tree_node(&left).unwrap(),
            encode_tree_node(&right).unwrap()
        );
    }
}

#[test]
fn full_width_integers_remain_exact() {
    let node = directory(json!({"max": u64::MAX, "min": i64::MIN}));
    let encoded = encode_tree_node(&node).unwrap();
    assert_eq!(decode_tree_node(&encoded).unwrap(), node);
}
