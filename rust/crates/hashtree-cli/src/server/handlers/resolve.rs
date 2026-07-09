use super::super::auth::{AppState, CachedResolvedPathEntry, CachedTreeRootEntry, LookupResult};
use super::super::nostr_query::query_events_for_local_request;
use super::{list_directory_with_fetch, resolve_path_with_fetch};
use crate::webrtc::{build_root_filter, pick_latest_event, root_event_from_peer, PeerRootEvent};
use anyhow::Result;
use hashtree_core::{from_hex, to_hex, Cid, HashTree, LinkType, Store, TreeEntry};
use nostr_pubsub::EventSourceKind;
use std::collections::HashMap;
use std::time::{Duration, Instant};

const TREE_ROOT_CACHE_FRESH_TTL: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub(super) struct ResolvedRoot {
    pub(super) cid: Cid,
    pub(super) source: &'static str,
    pub(super) root_event: Option<PeerRootEvent>,
}

pub(super) struct NostrResolvedRootEvent {
    pub(super) source: &'static str,
    pub(super) root_event: PeerRootEvent,
}

pub(super) fn peer_root_to_cid(root: &PeerRootEvent) -> Option<Cid> {
    let mut cid = Cid::parse(&root.hash).ok()?;
    if cid.key.is_none() {
        cid.key = root
            .key
            .as_deref()
            .and_then(|key_hex| from_hex(key_hex).ok());
    }
    Some(cid)
}

pub(super) async fn resolve_root_from_nostr_relays(
    state: &AppState,
    pubkey: &str,
    treename: &str,
) -> Option<NostrResolvedRootEvent> {
    let filter = build_root_filter(pubkey, treename)?;
    let query = query_events_for_local_request(state, &filter, 50).await;
    let events = query.merged_events(50);
    let latest = pick_latest_event(events.iter())?;
    let source = match query.upstream_source(&latest.id) {
        Some(EventSourceKind::FipsEndpoint | EventSourceKind::Peer) => "fips-pubsub",
        Some(EventSourceKind::Relay) => "nostr-relay",
        Some(EventSourceKind::LocalIndex) | None => "local-relay",
    };
    Some(NostrResolvedRootEvent {
        source,
        root_event: root_event_from_peer(latest, source, treename)?,
    })
}

pub(super) async fn resolve_root_offline(
    state: &AppState,
    pubkey: &str,
    treename: &str,
    link_key: Option<[u8; 32]>,
) -> Option<ResolvedRoot> {
    let cache_key = tree_root_cache_key(pubkey, treename, link_key);
    if let Some(cached) = get_cached_tree_root(state, &cache_key) {
        return Some(resolved_cached_tree_root(cached, link_key, "cache"));
    }

    resolve_root_without_cache(state, pubkey, treename, link_key).await
}

pub(super) async fn resolve_root_for_mutable_request(
    state: &AppState,
    pubkey: &str,
    treename: &str,
    link_key: Option<[u8; 32]>,
) -> Option<ResolvedRoot> {
    let cache_key = tree_root_cache_key(pubkey, treename, link_key);
    if let Some(cached_root) = get_cached_tree_root(state, &cache_key) {
        if cached_tree_root_is_fresh(&cached_root) {
            return Some(resolved_cached_tree_root(cached_root, link_key, "cache"));
        }
        touch_cached_tree_root(state, &cache_key);
        refresh_tree_root_cache_in_background(
            state.clone(),
            pubkey.to_string(),
            treename.to_string(),
            link_key,
        );
        return Some(resolved_cached_tree_root(
            cached_root,
            link_key,
            "stale-cache",
        ));
    }

    resolve_root_without_cache(state, pubkey, treename, link_key).await
}

fn refresh_tree_root_cache_in_background(
    state: AppState,
    pubkey: String,
    treename: String,
    link_key: Option<[u8; 32]>,
) {
    tokio::spawn(async move {
        let _ = resolve_root_without_cache(&state, &pubkey, &treename, link_key).await;
    });
}

pub(super) async fn resolve_root_without_cache(
    state: &AppState,
    pubkey: &str,
    treename: &str,
    link_key: Option<[u8; 32]>,
) -> Option<ResolvedRoot> {
    let cache_key = tree_root_cache_key(pubkey, treename, link_key);
    if let Some(root) = resolve_root_from_nostr_relays(state, pubkey, treename).await {
        if let Some(mut cid) = peer_root_to_cid(&root.root_event) {
            if cid.key.is_none() {
                cid.key = link_key;
            }
            put_cached_tree_root(
                state,
                cache_key.clone(),
                cid.clone(),
                root.source,
                Some(root.root_event.clone()),
            );
            return Some(ResolvedRoot {
                cid,
                source: root.source,
                root_event: Some(root.root_event),
            });
        }
    }

    if state
        .nostr_provider
        .as_ref()
        .is_some_and(|provider| provider.mode() == nostr_pubsub::PubsubProviderMode::LocalOnly)
    {
        return None;
    }

    if let Some(ref webrtc_state) = state.webrtc_peers {
        if let Some((source, root)) = webrtc_state
            .resolve_root_from_local_buses_with_source(pubkey, treename, Duration::from_secs(2))
            .await
        {
            if let Some(mut cid) = peer_root_to_cid(&root) {
                if cid.key.is_none() {
                    cid.key = link_key;
                }
                put_cached_tree_root(
                    state,
                    cache_key.clone(),
                    cid.clone(),
                    source,
                    Some(root.clone()),
                );
                return Some(ResolvedRoot {
                    cid,
                    source,
                    root_event: Some(root),
                });
            }
        }

        if let Some(root) = webrtc_state
            .resolve_root_from_peers(pubkey, treename, Duration::from_secs(4))
            .await
        {
            if let Some(mut cid) = peer_root_to_cid(&root) {
                if cid.key.is_none() {
                    cid.key = link_key;
                }
                put_cached_tree_root(state, cache_key, cid.clone(), "webrtc", Some(root.clone()));
                return Some(ResolvedRoot {
                    cid,
                    source: "webrtc",
                    root_event: Some(root),
                });
            }
        }
    }

    None
}

pub(super) fn tree_root_cache_key(
    npub: &str,
    treename: &str,
    link_key: Option<[u8; 32]>,
) -> String {
    match link_key {
        Some(key) => format!("{}/{}?k={}", npub, treename, to_hex(&key)),
        None => format!("{}/{}", npub, treename),
    }
}

pub(super) fn query_flag(params: &HashMap<String, String>, name: &str) -> bool {
    params
        .get(name)
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

pub(super) fn cache_tree_root_key(
    npub: &str,
    treename: &str,
    visibility: Option<&str>,
    key: Option<[u8; 32]>,
) -> String {
    let visibility = visibility.unwrap_or("public");
    let link_key = if visibility == "public" { None } else { key };
    tree_root_cache_key(npub, treename, link_key)
}

pub(super) fn cid_cache_key(cid: &Cid) -> String {
    match cid.key {
        Some(key) => format!("{}?k={}", to_hex(&cid.hash), to_hex(&key)),
        None => to_hex(&cid.hash),
    }
}

pub(super) fn resolved_path_cache_key(root_cid: &Cid, path: &str) -> String {
    format!("{}|{}", cid_cache_key(root_cid), path)
}

pub(super) fn get_cached_lookup<T: Clone>(
    cache: &std::sync::Arc<
        std::sync::Mutex<super::super::auth::TimedLruCache<String, LookupResult<T>>>,
    >,
    key: &str,
) -> Option<Option<T>> {
    cache
        .lock()
        .ok()
        .and_then(|mut cache| cache.get_cloned(&key.to_string()))
        .map(LookupResult::into_option)
}

pub(super) fn put_cached_lookup<T: Clone>(
    cache: &std::sync::Arc<
        std::sync::Mutex<super::super::auth::TimedLruCache<String, LookupResult<T>>>,
    >,
    key: String,
    value: Option<T>,
) {
    if let Ok(mut cache) = cache.lock() {
        let cached = LookupResult::from_option(value);
        let ttl = cached.ttl();
        cache.put(key, cached, ttl);
    }
}

pub(super) fn cached_resolved_path_entry(entry: &ResolvedPathEntry) -> CachedResolvedPathEntry {
    CachedResolvedPathEntry {
        cid: entry.cid.clone(),
        link_type: entry.link_type,
    }
}

pub(super) fn get_cached_tree_root(
    state: &AppState,
    cache_key: &str,
) -> Option<CachedTreeRootEntry> {
    state
        .tree_root_cache
        .lock()
        .ok()
        .and_then(|cache| cache.get(cache_key).cloned())
}

fn cached_tree_root_is_fresh(entry: &CachedTreeRootEntry) -> bool {
    entry.cached_at.elapsed() < TREE_ROOT_CACHE_FRESH_TTL
}

fn resolved_cached_tree_root(
    mut cached: CachedTreeRootEntry,
    link_key: Option<[u8; 32]>,
    source: &'static str,
) -> ResolvedRoot {
    if cached.cid.key.is_none() {
        cached.cid.key = link_key;
    }
    ResolvedRoot {
        cid: cached.cid,
        source,
        root_event: cached.root_event,
    }
}

pub(super) fn put_cached_tree_root(
    state: &AppState,
    cache_key: String,
    cid: Cid,
    source: &'static str,
    root_event: Option<PeerRootEvent>,
) {
    if let Ok(mut cache) = state.tree_root_cache.lock() {
        cache.insert(
            cache_key,
            CachedTreeRootEntry {
                cid,
                source,
                root_event,
                cached_at: Instant::now(),
            },
        );
    }
}

fn touch_cached_tree_root(state: &AppState, cache_key: &str) {
    if let Ok(mut cache) = state.tree_root_cache.lock() {
        if let Some(entry) = cache.get_mut(cache_key) {
            entry.cached_at = Instant::now();
        }
    }
}

pub(super) fn remove_cached_tree_root(state: &AppState, cache_key: &str) -> bool {
    state
        .tree_root_cache
        .lock()
        .ok()
        .and_then(|mut cache| cache.remove(cache_key))
        .is_some()
}

const DEFAULT_DIRECTORY_INDEXES: [&str; 2] = ["index.html", "index.htm"];

pub(super) enum DirectoryTarget {
    File { cid: Cid, path: String },
    DirectoryListing { cid: Cid },
}

pub(super) struct ResolvedPathEntry {
    pub(super) cid: Cid,
    pub(super) link_type: LinkType,
}

async fn resolve_directory_index_path<S: Store>(
    state: &AppState,
    tree: &HashTree<S>,
    root_cid: &Cid,
    requested_path: Option<&str>,
) -> Result<Option<String>, String> {
    let base = requested_path
        .map(|path| path.trim_matches('/'))
        .filter(|path| !path.is_empty());

    for candidate in DEFAULT_DIRECTORY_INDEXES {
        let candidate_path = match base {
            Some(base) => format!("{}/{}", base, candidate),
            None => candidate.to_string(),
        };
        if resolve_path_with_fetch(state, tree, root_cid, &candidate_path)
            .await?
            .is_some()
        {
            return Ok(Some(candidate_path));
        }
    }

    Ok(None)
}

pub(super) async fn resolve_directory_target<S: Store>(
    state: &AppState,
    tree: &HashTree<S>,
    root_cid: &Cid,
    requested_path: Option<String>,
) -> Result<Option<DirectoryTarget>, String> {
    if let Some(path) = requested_path {
        let entry = match resolve_path_with_fetch(state, tree, root_cid, &path).await? {
            Some(entry) => entry,
            None => return Ok(None),
        };

        if entry.link_type == LinkType::Dir {
            if let Some(index_path) =
                resolve_directory_index_path(state, tree, root_cid, Some(&path)).await?
            {
                let index_entry = resolve_path_with_fetch(state, tree, root_cid, &index_path)
                    .await?
                    .ok_or_else(|| format!("Resolved default path missing: {}", index_path))?;
                return Ok(Some(DirectoryTarget::File {
                    cid: index_entry.cid,
                    path: index_path,
                }));
            }

            return Ok(Some(DirectoryTarget::DirectoryListing { cid: entry.cid }));
        }

        return Ok(Some(DirectoryTarget::File {
            cid: entry.cid,
            path,
        }));
    }

    if let Some(index_path) = resolve_directory_index_path(state, tree, root_cid, None).await? {
        let index_entry = resolve_path_with_fetch(state, tree, root_cid, &index_path)
            .await?
            .ok_or_else(|| format!("Resolved default path missing: {}", index_path))?;
        return Ok(Some(DirectoryTarget::File {
            cid: index_entry.cid,
            path: index_path,
        }));
    }

    Ok(Some(DirectoryTarget::DirectoryListing {
        cid: root_cid.clone(),
    }))
}

const THUMBNAIL_PATTERNS: &[&str] = &[
    "thumbnail.jpg",
    "thumbnail.webp",
    "thumbnail.png",
    "thumbnail.jpeg",
];

const VIDEO_EXTENSIONS: &[&str] = &[".mp4", ".webm", ".mkv", ".mov", ".avi", ".m4v"];

fn is_video_filename(name: &str) -> bool {
    name.starts_with("video.") || VIDEO_EXTENSIONS.iter().any(|ext| name.ends_with(ext))
}

fn is_metadata_filename(name: &str) -> bool {
    name.ends_with(".json") || name.ends_with(".txt")
}

fn is_image_filename(name: &str) -> bool {
    let normalized = name.trim().to_ascii_lowercase();
    normalized.ends_with(".jpg")
        || normalized.ends_with(".jpeg")
        || normalized.ends_with(".png")
        || normalized.ends_with(".webp")
}

fn find_thumbnail_entry_name(entries: &[TreeEntry]) -> Option<String> {
    for pattern in THUMBNAIL_PATTERNS {
        if entries.iter().any(|entry| entry.name == *pattern) {
            return Some((*pattern).to_string());
        }
    }

    entries
        .iter()
        .find(|entry| is_image_filename(&entry.name))
        .map(|entry| entry.name.clone())
}

pub(super) fn is_thumbnail_request(path: &str) -> bool {
    path == "thumbnail" || path.ends_with("/thumbnail")
}

pub(super) async fn resolve_thumbnail_path<S: Store>(
    state: &AppState,
    tree: &HashTree<S>,
    root: &Cid,
    path: &str,
) -> Result<Option<String>, String> {
    if !is_thumbnail_request(path) {
        return Ok(None);
    }

    let cache_key = resolved_path_cache_key(root, path);
    if let Some(cached) = get_cached_lookup(&state.thumbnail_path_cache, &cache_key) {
        return Ok(cached);
    }

    let dir_path = if path == "thumbnail" {
        ""
    } else {
        path.strip_suffix("/thumbnail").unwrap_or("")
    };

    let dir_entry = if dir_path.is_empty() {
        Some(root.clone())
    } else {
        resolve_path_with_fetch(state, tree, root, dir_path)
            .await?
            .map(|entry| entry.cid)
    };
    let Some(dir_entry) = dir_entry else {
        put_cached_lookup(&state.thumbnail_path_cache, cache_key, None);
        return Ok(None);
    };

    let Some(entries) = list_directory_with_fetch(state, tree, &dir_entry).await? else {
        put_cached_lookup(&state.thumbnail_path_cache, cache_key, None);
        return Ok(None);
    };

    if let Some(thumbnail_name) = find_thumbnail_entry_name(&entries) {
        let resolved = if dir_path.is_empty() {
            thumbnail_name
        } else {
            format!("{}/{}", dir_path, thumbnail_name)
        };
        put_cached_lookup(
            &state.thumbnail_path_cache,
            cache_key,
            Some(resolved.clone()),
        );
        return Ok(Some(resolved));
    }

    let has_video_file = entries.iter().any(|e| is_video_filename(&e.name));
    if !has_video_file && !entries.is_empty() {
        let mut sorted: Vec<_> = entries.iter().collect();
        sorted.sort_by(|a, b| a.name.cmp(&b.name));

        for entry in sorted.into_iter().take(3) {
            if is_metadata_filename(&entry.name) {
                continue;
            }

            let sub_cid = Cid {
                hash: entry.hash,
                key: entry.key,
            };
            let sub_entries = match list_directory_with_fetch(state, tree, &sub_cid).await? {
                Some(entries) => entries,
                None => continue,
            };

            if let Some(thumbnail_name) = find_thumbnail_entry_name(&sub_entries) {
                let prefix = if dir_path.is_empty() {
                    entry.name.clone()
                } else {
                    format!("{}/{}", dir_path, entry.name)
                };
                let resolved = format!("{}/{}", prefix, thumbnail_name);
                put_cached_lookup(
                    &state.thumbnail_path_cache,
                    cache_key,
                    Some(resolved.clone()),
                );
                return Ok(Some(resolved));
            }
        }
    }

    put_cached_lookup(&state.thumbnail_path_cache, cache_key, None);
    Ok(None)
}
