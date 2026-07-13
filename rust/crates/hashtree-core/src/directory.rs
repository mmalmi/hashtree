use std::collections::HashMap;

use crate::types::{Link, LinkType, TreeNode};

#[derive(Clone)]
pub(crate) struct DirectoryFanoutSpan {
    pub link: Link,
    pub count: usize,
    pub first: String,
    pub last: String,
}

pub(crate) fn directory_fanout_meta(
    count: usize,
    first: &str,
    last: &str,
) -> HashMap<String, serde_json::Value> {
    let mut meta = HashMap::new();
    meta.insert("count".to_string(), serde_json::json!(count as u64));
    meta.insert("first".to_string(), serde_json::json!(first));
    meta.insert("last".to_string(), serde_json::json!(last));
    meta
}

fn internal_chunk_start(name: &str) -> Option<usize> {
    let suffix = name.strip_prefix("_chunk_")?;
    if suffix.is_empty() || !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    suffix.parse().ok()
}

fn node_uses_legacy_directory_fanout(node: &TreeNode) -> bool {
    node.node_type == LinkType::Dir
        && !node.links.is_empty()
        && node.links.iter().all(|link| {
            let Some(name) = link.name.as_deref() else {
                return false;
            };
            internal_chunk_start(name).is_some() && link.link_type == LinkType::Dir
        })
}

pub(crate) fn is_internal_directory_link(node: &TreeNode, link: &Link) -> bool {
    if node.node_type == LinkType::Fanout {
        return matches!(link.link_type, LinkType::Dir | LinkType::Fanout);
    }

    if !node_uses_legacy_directory_fanout(node) || link.link_type != LinkType::Dir {
        return false;
    }

    link.name
        .as_deref()
        .and_then(internal_chunk_start)
        .is_some()
}
