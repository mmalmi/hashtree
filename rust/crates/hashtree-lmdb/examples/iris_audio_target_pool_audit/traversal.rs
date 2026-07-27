use crate::model::{BlockLedgerRow, BlockRef, WorkItem, LEDGER_ROW_SCHEMA};
use crate::probe::HashProbe;
use hashtree_core::{
    decode_tree_node, decrypt_chk, nhash_decode, to_hex, Link, LinkType, TreeNode,
};
use std::collections::BTreeSet;

struct DiscoveredRoot {
    block: BlockRef,
}

pub struct ProcessedBlock {
    pub row: BlockLedgerRow,
    pub children: Vec<BlockRef>,
    pub traversal_failed: bool,
}

pub fn process_block(
    work_item_index: usize,
    item: &WorkItem,
    block: &BlockRef,
    probe: &HashProbe,
) -> ProcessedBlock {
    let mut children = Vec::new();
    let mut traversal_failed = false;
    let mut traversal = "unavailable".to_string();
    let mut error = None;
    let mut external_roots = Vec::new();

    if let Some(stored) = probe.data.as_ref() {
        let plaintext = match block.key {
            Some(key) => match decrypt_chk(stored, &key) {
                Ok(plaintext) => Some(plaintext),
                Err(decrypt_error) => {
                    traversal_failed = true;
                    traversal = "chk-auth-failed".into();
                    error = Some(decrypt_error.to_string());
                    None
                }
            },
            None => Some(stored.clone()),
        };
        if let Some(plaintext) = plaintext {
            if block.expected_link_type == Some(LinkType::Blob) {
                traversal = "blob".into();
                let (roots, invalid) = roots_from_json_bytes(&plaintext, &block.path);
                external_roots = roots;
                apply_invalid_references(
                    invalid,
                    &mut traversal_failed,
                    &mut traversal,
                    &mut error,
                );
            } else {
                match decode_tree_node(&plaintext) {
                    Ok(node) => {
                        traversal = node_type_label(node.node_type).into();
                        if block
                            .expected_link_type
                            .is_some_and(|expected| expected != node.node_type)
                        {
                            traversal_failed = true;
                            traversal = "link-type-mismatch".into();
                            error = Some(format!(
                                "expected {}, decoded {}",
                                expected_link_label(block.expected_link_type),
                                node_type_label(node.node_type)
                            ));
                        }
                        append_node_children(block, &node, &mut children);
                        let (roots, invalid) = roots_from_node_metadata(&node, &block.path);
                        external_roots = roots;
                        apply_invalid_references(
                            invalid,
                            &mut traversal_failed,
                            &mut traversal,
                            &mut error,
                        );
                    }
                    Err(decode_error) if block.expected_link_type.is_none() => {
                        traversal = "blob".into();
                        let (roots, invalid) = roots_from_json_bytes(&plaintext, &block.path);
                        external_roots = roots;
                        apply_invalid_references(
                            invalid,
                            &mut traversal_failed,
                            &mut traversal,
                            &mut error,
                        );
                        if item.kind == "inventory"
                            && block.path == "."
                            && serde_json::from_slice::<serde_json::Value>(&plaintext).is_err()
                        {
                            traversal_failed = true;
                            traversal = "song-root-decode-failed".into();
                            error = Some(format!(
                                "inventory root is neither a tree nor stored-song JSON: {decode_error}"
                            ));
                        }
                    }
                    Err(decode_error) => {
                        traversal_failed = true;
                        traversal = "tree-decode-failed".into();
                        error = Some(decode_error.to_string());
                    }
                }
            }
        }
    } else {
        traversal_failed = true;
        error = Some("no hash-valid body was available for transitive traversal".into());
    }

    let mut external_seen = BTreeSet::new();
    for discovered in external_roots {
        let identity = (
            discovered.block.hash,
            discovered.block.key,
            discovered.block.role.clone(),
            discovered.block.path.clone(),
        );
        if external_seen.insert(identity) {
            children.push(discovered.block);
        }
    }
    let discovered_external_roots = external_seen.len();
    ProcessedBlock {
        row: BlockLedgerRow {
            schema: LEDGER_ROW_SCHEMA,
            work_item_index,
            work_item_kind: item.kind,
            work_item_id: item.id.clone(),
            source_key: item.source_key.clone(),
            song_id: item.song_id.clone(),
            input_line: item.input_line,
            root_hash: to_hex(&item.hash),
            block_hash: to_hex(&block.hash),
            key: block.key.as_ref().map(to_hex),
            path: block.path.clone(),
            role: block.role.clone(),
            expected_link_type: expected_link_label(block.expected_link_type).into(),
            catalog_state: probe.catalog_state.clone(),
            catalog_candidates: probe.catalog_candidates.clone(),
            catalog_target_membership: probe.catalog_target_membership,
            catalog_error: probe.catalog_error.clone(),
            target_members: probe.target_members.clone(),
            fallback_tiers: probe.fallback_tiers.clone(),
            target_witness: probe.target_witness.clone(),
            fallback_witness: probe.fallback_witness.clone(),
            residency: probe.residency.into(),
            stored_bytes: probe.data.as_ref().map(Vec::len),
            traversal,
            discovered_external_roots,
            error,
        },
        children,
        traversal_failed,
    }
}

fn apply_invalid_references(
    invalid: Vec<String>,
    traversal_failed: &mut bool,
    traversal: &mut String,
    error: &mut Option<String>,
) {
    if !invalid.is_empty() {
        *traversal_failed = true;
        *traversal = "invalid-htree-reference".into();
        *error = Some(invalid.join("; "));
    }
}

fn append_node_children(parent: &BlockRef, node: &TreeNode, output: &mut Vec<BlockRef>) {
    for link in &node.links {
        output.push(BlockRef {
            hash: link.hash,
            key: link.key,
            path: child_path(&parent.path, link),
            role: child_role(&parent.role, link),
            expected_link_type: Some(link.link_type),
        });
    }
}

fn child_path(parent: &str, link: &Link) -> String {
    match link.name.as_deref() {
        Some(name) if parent == "." => name.into(),
        Some(name) => format!("{parent}/{name}"),
        None => parent.into(),
    }
}

fn child_role(parent_role: &str, link: &Link) -> String {
    if parent_role == "catalog" {
        return "catalog".into();
    }
    let name = link.name.as_deref().unwrap_or("").to_ascii_lowercase();
    if is_audio_name(&name) || link_meta_is_track(link) {
        "audio".into()
    } else if is_image_name(&name) {
        "image".into()
    } else {
        parent_role.into()
    }
}

fn link_meta_is_track(link: &Link) -> bool {
    link.meta
        .as_ref()
        .and_then(|meta| meta.get("schema"))
        .and_then(serde_json::Value::as_str)
        == Some("iris-audio-track-entry/v1")
}

fn is_audio_name(name: &str) -> bool {
    [
        ".mp3", ".ogg", ".oga", ".opus", ".flac", ".wav", ".m4a", ".aac",
    ]
    .iter()
    .any(|extension| name.ends_with(extension))
}

fn is_image_name(name: &str) -> bool {
    [".jpg", ".jpeg", ".png", ".webp", ".gif", ".avif"]
        .iter()
        .any(|extension| name.ends_with(extension))
}

fn node_type_label(link_type: LinkType) -> &'static str {
    match link_type {
        LinkType::Blob => "blob",
        LinkType::File => "file",
        LinkType::Dir => "dir",
        LinkType::Fanout => "fanout",
    }
}

fn expected_link_label(link_type: Option<LinkType>) -> &'static str {
    link_type.map(node_type_label).unwrap_or("root-auto")
}

fn roots_from_node_metadata(node: &TreeNode, path: &str) -> (Vec<DiscoveredRoot>, Vec<String>) {
    let mut roots = Vec::new();
    let mut invalid = Vec::new();
    for (link_index, link) in node.links.iter().enumerate() {
        let Some(meta) = link.meta.as_ref() else {
            continue;
        };
        let mut keys = meta.keys().collect::<Vec<_>>();
        keys.sort_unstable();
        for key in keys {
            collect_json_roots(
                &meta[key],
                &format!("{path}.links[{link_index}].meta.{key}"),
                key,
                &mut roots,
                &mut invalid,
            );
        }
    }
    (roots, invalid)
}

fn roots_from_json_bytes(bytes: &[u8], path: &str) -> (Vec<DiscoveredRoot>, Vec<String>) {
    let first = bytes
        .iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace());
    if !matches!(first, Some(b'{') | Some(b'[')) {
        return (Vec::new(), Vec::new());
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return (Vec::new(), Vec::new());
    };
    let mut roots = Vec::new();
    let mut invalid = Vec::new();
    collect_json_roots(
        &value,
        &format!("{path}.json"),
        "",
        &mut roots,
        &mut invalid,
    );
    (roots, invalid)
}

fn collect_json_roots(
    value: &serde_json::Value,
    path: &str,
    field: &str,
    roots: &mut Vec<DiscoveredRoot>,
    invalid: &mut Vec<String>,
) {
    match value {
        serde_json::Value::String(value) if is_htree_url(value) => {
            match parse_immutable_htree_root(value, field, path) {
                Ok(root) => roots.push(root),
                Err(error) => invalid.push(error),
            }
        }
        serde_json::Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                collect_json_roots(value, &format!("{path}[{index}]"), field, roots, invalid);
            }
        }
        serde_json::Value::Object(values) => {
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            for key in keys {
                collect_json_roots(&values[key], &format!("{path}.{key}"), key, roots, invalid);
            }
        }
        _ => {}
    }
}

fn parse_immutable_htree_root(
    value: &str,
    field: &str,
    path: &str,
) -> Result<DiscoveredRoot, String> {
    let normalized = value.trim();
    if !is_htree_url(normalized) {
        return Err(format!("{path}: unsupported htree URL scheme"));
    }
    let rest = &normalized["htree://".len()..];
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .filter(|authority| !authority.is_empty())
        .ok_or_else(|| format!("{path}: htree URL has no immutable root"))?;
    let decoded = nhash_decode(authority)
        .map_err(|error| format!("{path}: invalid immutable root: {error}"))?;
    Ok(DiscoveredRoot {
        block: BlockRef {
            hash: decoded.hash,
            key: decoded.decrypt_key,
            path: format!("@{path}"),
            role: external_role(field).into(),
            expected_link_type: None,
        },
    })
}

fn is_htree_url(value: &str) -> bool {
    value
        .trim()
        .get(.."htree://".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("htree://"))
}

fn external_role(field: &str) -> &'static str {
    let field = field.to_ascii_lowercase();
    if ["cover", "image", "photo", "logo", "thumbnail", "artwork"]
        .iter()
        .any(|needle| field.contains(needle))
    {
        "image"
    } else if field.contains("audio") {
        "audio"
    } else {
        "song"
    }
}
