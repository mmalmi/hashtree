use std::fmt::Write as _;
use std::sync::Arc;

use hashtree_core::{from_hex, nhash_decode, nhash_encode, xor_keys, Cid, Store, TreeVisibility};
use nostr_sdk::{PublicKey, ToBech32};

use crate::{
    parse_hashtree_root_event, read_signed_event_snapshot, store_signed_event_snapshot,
    NostrEventStoreError, ParsedHashtreeRootEvent, StoredNostrEvent,
};

#[derive(Debug, Clone, PartialEq)]
pub struct TreeEventSnapshotInfo {
    pub event: StoredNostrEvent,
    pub tree_name: String,
    pub root_cid: Cid,
    pub visibility: TreeVisibility,
    pub labels: Vec<String>,
    pub encrypted_key: Option<String>,
    pub key_id: Option<String>,
    pub self_encrypted_key: Option<String>,
    pub self_encrypted_link_key: Option<String>,
    pub snapshot_cid: Cid,
    pub snapshot_nhash: String,
    pub npub: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEventSnapshotPermalink {
    pub snapshot_nhash: String,
    pub path: Vec<String>,
    pub link_key: Option<String>,
}

fn validation_error(message: impl Into<String>) -> NostrEventStoreError {
    NostrEventStoreError::Validation(message.into())
}

fn normalize_snapshot_nhash(value: &str) -> Result<String, NostrEventStoreError> {
    let trimmed = value.trim();
    if !trimmed.starts_with("nhash1") {
        return Err(validation_error(format!(
            "tree snapshot permalink must start with nhash1, got: {trimmed}"
        )));
    }

    let decoded = nhash_decode(trimmed)
        .map_err(|error| validation_error(format!("invalid tree snapshot nhash: {error}")))?;
    if decoded.decrypt_key.is_some() {
        return Err(validation_error(
            "tree snapshot nhash must point to a public signed event snapshot".to_string(),
        ));
    }

    Ok(trimmed.to_string())
}

fn normalize_link_key(value: Option<&str>) -> Result<Option<String>, NostrEventStoreError> {
    let Some(trimmed) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };

    let lower = trimmed.to_ascii_lowercase();
    from_hex(&lower)
        .map_err(|_| validation_error(format!("invalid tree snapshot link key: {trimmed}")))?;
    Ok(Some(lower))
}

fn snapshot_info_from_parsed(
    parsed: ParsedHashtreeRootEvent,
    snapshot_cid: Cid,
) -> Result<TreeEventSnapshotInfo, NostrEventStoreError> {
    let snapshot_nhash = nhash_encode(&snapshot_cid.hash).map_err(|error| {
        validation_error(format!("failed to encode tree snapshot nhash: {error}"))
    })?;
    let npub = PublicKey::from_hex(&parsed.event.pubkey)
        .map_err(|_| validation_error("Nostr event pubkey must be valid lowercase hex"))?
        .to_bech32()
        .map_err(|error| validation_error(format!("failed to encode npub: {error}")))?;

    Ok(TreeEventSnapshotInfo {
        event: parsed.event,
        tree_name: parsed.tree_name,
        root_cid: parsed.root_cid,
        visibility: parsed.visibility,
        labels: parsed.labels,
        encrypted_key: parsed.encrypted_key,
        key_id: parsed.key_id,
        self_encrypted_key: parsed.self_encrypted_key,
        self_encrypted_link_key: parsed.self_encrypted_link_key,
        snapshot_cid,
        snapshot_nhash,
        npub,
    })
}

fn compare_event_order(
    candidate: &StoredNostrEvent,
    current: &StoredNostrEvent,
) -> std::cmp::Ordering {
    candidate
        .created_at
        .cmp(&current.created_at)
        .then_with(|| candidate.id.cmp(&current.id))
}

fn encode_segment(segment: &str) -> String {
    let mut encoded = String::new();
    for byte in segment.as_bytes() {
        if matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~') {
            encoded.push(*byte as char);
        } else {
            let _ = write!(&mut encoded, "%{:02X}", byte);
        }
    }
    encoded
}

fn decode_segment(segment: &str) -> Option<String> {
    let bytes = segment.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return None;
            }
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok()?;
            let value = u8::from_str_radix(hex, 16).ok()?;
            decoded.push(value);
            index += 3;
            continue;
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(decoded).ok()
}

fn query_param<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    for entry in query.split('&').filter(|value| !value.is_empty()) {
        let (key, value) = entry.split_once('=').unwrap_or((entry, ""));
        if key == name {
            return Some(value);
        }
    }
    None
}

fn extract_permalink_candidate(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    if let Some(index) = trimmed.find("#/") {
        return trimmed[index + 1..].to_string();
    }

    if let Some(scheme_index) = trimmed.find("://") {
        let rest = &trimmed[scheme_index + 3..];
        let host_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let host = &rest[..host_end];
        if host.is_empty() {
            return String::new();
        }
        let tail = &rest[host_end..];
        return format!("/{host}{tail}");
    }

    if let Some(stripped) = trimmed.strip_prefix("#/") {
        return format!("/{stripped}");
    }

    trimmed.to_string()
}

pub fn compare_tree_event_snapshots(
    candidate: &TreeEventSnapshotInfo,
    current: &TreeEventSnapshotInfo,
) -> std::cmp::Ordering {
    compare_event_order(&candidate.event, &current.event)
}

pub fn is_newer_tree_event_snapshot(
    candidate: &TreeEventSnapshotInfo,
    current: &TreeEventSnapshotInfo,
) -> bool {
    compare_tree_event_snapshots(candidate, current).is_gt()
}

pub fn snapshot_matches_root_cid(
    snapshot: Option<&TreeEventSnapshotInfo>,
    root_cid: Option<&Cid>,
) -> bool {
    let (Some(snapshot), Some(root_cid)) = (snapshot, root_cid) else {
        return false;
    };

    if snapshot.root_cid.hash != root_cid.hash {
        return false;
    }

    if snapshot.visibility != TreeVisibility::Public {
        return true;
    }

    match (snapshot.root_cid.key, root_cid.key) {
        (Some(left), Some(right)) => left == right,
        _ => true,
    }
}

pub fn resolve_snapshot_root_cid(
    snapshot: &TreeEventSnapshotInfo,
    link_key: Option<&str>,
) -> Result<Option<Cid>, NostrEventStoreError> {
    match snapshot.visibility {
        TreeVisibility::Public => Ok(Some(snapshot.root_cid.clone())),
        TreeVisibility::LinkVisible => {
            let Some(encrypted_key_hex) = snapshot.encrypted_key.as_deref() else {
                return Ok(None);
            };
            let Some(link_key_hex) = normalize_link_key(link_key)? else {
                return Ok(None);
            };
            let encrypted_key = from_hex(encrypted_key_hex)
                .map_err(|_| validation_error("invalid encrypted tree key in snapshot event"))?;
            let link_key = from_hex(&link_key_hex)
                .map_err(|_| validation_error("invalid tree snapshot link key"))?;
            let decrypted_key = xor_keys(&encrypted_key, &link_key);
            Ok(Some(Cid {
                hash: snapshot.root_cid.hash,
                key: Some(decrypted_key),
            }))
        }
        TreeVisibility::Private => Ok(None),
    }
}

pub async fn store_tree_event_snapshot<S: Store>(
    store: Arc<S>,
    event: &StoredNostrEvent,
) -> Result<Option<TreeEventSnapshotInfo>, NostrEventStoreError> {
    let Some(parsed) = parse_hashtree_root_event(event)? else {
        return Ok(None);
    };
    let snapshot_cid = store_signed_event_snapshot(store, &parsed.event).await?;
    snapshot_info_from_parsed(parsed, snapshot_cid).map(Some)
}

pub async fn read_tree_event_snapshot<S: Store>(
    store: Arc<S>,
    snapshot_cid: &Cid,
    max_bytes: Option<usize>,
) -> Result<Option<TreeEventSnapshotInfo>, NostrEventStoreError> {
    let event = read_signed_event_snapshot(store, snapshot_cid, max_bytes).await?;
    let Some(parsed) = parse_hashtree_root_event(&event)? else {
        return Ok(None);
    };
    snapshot_info_from_parsed(parsed, snapshot_cid.clone()).map(Some)
}

pub fn serialize_tree_event_snapshot_permalink(
    permalink: &TreeEventSnapshotPermalink,
) -> Result<String, NostrEventStoreError> {
    let snapshot_nhash = normalize_snapshot_nhash(&permalink.snapshot_nhash)?;
    let link_key = normalize_link_key(permalink.link_key.as_deref())?;
    let encoded_path = permalink
        .path
        .iter()
        .filter(|segment| !segment.is_empty())
        .map(|segment| encode_segment(segment))
        .collect::<Vec<_>>()
        .join("/");

    let mut serialized = snapshot_nhash;
    if !encoded_path.is_empty() {
        serialized.push('/');
        serialized.push_str(&encoded_path);
    }
    serialized.push_str("?snapshot=1");
    if let Some(link_key) = link_key {
        serialized.push_str("&k=");
        serialized.push_str(&link_key);
    }
    Ok(serialized)
}

pub fn parse_tree_event_snapshot_permalink(input: &str) -> Option<TreeEventSnapshotPermalink> {
    let candidate = extract_permalink_candidate(input);
    if candidate.is_empty() {
        return None;
    }

    let (path_part, query_part) = candidate
        .split_once('?')
        .unwrap_or((candidate.as_str(), ""));
    let mut parts = path_part
        .trim_start_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(decode_segment)
        .collect::<Option<Vec<_>>>()?;

    if parts.first().map(String::as_str) == Some("nhash") {
        parts.remove(0);
    }

    let snapshot_nhash = parts.first()?.clone();
    if normalize_snapshot_nhash(&snapshot_nhash).is_err() {
        return None;
    }
    parts.remove(0);

    if query_param(query_part, "snapshot") != Some("1") {
        return None;
    }

    let raw_link_key = query_param(query_part, "k");
    let link_key = normalize_link_key(raw_link_key).ok()?;
    if raw_link_key.is_some() && link_key.is_none() {
        return None;
    }

    Some(TreeEventSnapshotPermalink {
        snapshot_nhash,
        path: parts,
        link_key,
    })
}
