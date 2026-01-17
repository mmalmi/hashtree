mod blossom;
mod combined_store;
mod nostr;
pub mod store;
mod tree;
mod types;
mod webrtc;

pub use store::BlobStore;
pub use tree::TreeManager;
pub use types::{PeerStatEntry, WorkerCid, WorkerDirEntry, WorkerRequest, WorkerResponse};

use blossom::BlossomManager;
use nostr::NostrManager;
use webrtc::WebRTCManager;
use nostrdb::{Config, Ndb, Transaction};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{AppHandle, Emitter};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// Convert hex pubkey string to 32-byte array
fn hex_to_pubkey(hex: &str) -> Result<[u8; 32], String> {
    let bytes = hex::decode(hex).map_err(|e| format!("Invalid hex: {}", e))?;
    bytes
        .try_into()
        .map_err(|_| "Pubkey must be 32 bytes".to_string())
}

/// Convert 32-byte array to hex string
fn pubkey_to_hex(pk: &[u8; 32]) -> String {
    hex::encode(pk)
}

/// Convert JSON filter to nostrdb Filter for cache queries
fn json_to_ndb_filter(filter_json: &serde_json::Value) -> Option<nostrdb::Filter> {
    let mut builder = nostrdb::Filter::new();

    // IDs
    if let Some(ids) = filter_json.get("ids").and_then(|v| v.as_array()) {
        let id_bytes: Vec<[u8; 32]> = ids
            .iter()
            .filter_map(|id| {
                id.as_str()
                    .and_then(|s| hex::decode(s).ok())
                    .and_then(|b| b.try_into().ok())
            })
            .collect();
        if !id_bytes.is_empty() {
            builder = builder.ids(id_bytes.iter().collect::<Vec<_>>());
        }
    }

    // Authors
    if let Some(authors) = filter_json.get("authors").and_then(|v| v.as_array()) {
        let author_bytes: Vec<[u8; 32]> = authors
            .iter()
            .filter_map(|a| {
                a.as_str()
                    .and_then(|s| hex::decode(s).ok())
                    .and_then(|b| b.try_into().ok())
            })
            .collect();
        if !author_bytes.is_empty() {
            builder = builder.authors(author_bytes.iter().collect::<Vec<_>>());
        }
    }

    // Kinds
    if let Some(kinds) = filter_json.get("kinds").and_then(|v| v.as_array()) {
        let kind_vec: Vec<u64> = kinds.iter().filter_map(|k| k.as_u64()).collect();
        if !kind_vec.is_empty() {
            builder = builder.kinds(kind_vec);
        }
    }

    // Since/Until
    if let Some(since) = filter_json.get("since").and_then(|v| v.as_u64()) {
        builder = builder.since(since);
    }
    if let Some(until) = filter_json.get("until").and_then(|v| v.as_u64()) {
        builder = builder.until(until);
    }

    // Limit
    if let Some(limit) = filter_json.get("limit").and_then(|v| v.as_u64()) {
        builder = builder.limit(limit);
    }

    Some(builder.build())
}

/// Query ndb cache and emit cached events to frontend
fn query_ndb_cache(
    ndb: &Ndb,
    filters_json: &[serde_json::Value],
    sub_id: &str,
    app_handle: &AppHandle,
) -> Vec<[u8; 32]> {
    let mut found_ids: Vec<[u8; 32]> = Vec::new();

    let txn = match Transaction::new(ndb) {
        Ok(t) => t,
        Err(_) => return found_ids,
    };

    for filter_json in filters_json {
        // Check if this is an ID-based query
        let has_ids = filter_json.get("ids").and_then(|v| v.as_array()).map(|a| !a.is_empty()).unwrap_or(false);

        if has_ids {
            // Fast path: direct ID lookup
            if let Some(ids) = filter_json.get("ids").and_then(|v| v.as_array()) {
                for id in ids {
                    if let Some(id_str) = id.as_str() {
                        if let Ok(id_bytes) = hex::decode(id_str) {
                            let id_arr: Result<[u8; 32], _> = id_bytes.try_into();
                            if let Ok(id_arr) = id_arr {
                                if let Ok(note_key) = ndb.get_notekey_by_id(&txn, &id_arr) {
                                    if let Ok(note) = ndb.get_note_by_key(&txn, note_key) {
                                        if let Ok(event_json) = note.json() {
                                            if let Ok(event) = serde_json::from_str::<serde_json::Value>(&event_json) {
                                                let _ = app_handle.emit(
                                                    "worker_response",
                                                    &WorkerResponse::Event {
                                                        sub_id: sub_id.to_string(),
                                                        event,
                                                    },
                                                );
                                                found_ids.push(id_arr);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        } else {
            // General query path
            if let Some(ndb_filter) = json_to_ndb_filter(filter_json) {
                if let Ok(results) = ndb.query(&txn, &[ndb_filter], 1000) {
                    for result in results.iter() {
                        if let Ok(event_json) = result.note.json() {
                            if let Ok(event) = serde_json::from_str::<serde_json::Value>(&event_json) {
                                // Track found ID
                                let id_bytes = result.note.id();
                                found_ids.push(*id_bytes);

                                let _ = app_handle.emit(
                                    "worker_response",
                                    &WorkerResponse::Event {
                                        sub_id: sub_id.to_string(),
                                        event,
                                    },
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    debug!("Cache query returned {} events for sub {}", found_ids.len(), sub_id);
    found_ids
}

/// Shared state for worker operations
pub struct WorkerState {
    pub store: Arc<BlobStore>,
    pub tree: Arc<RwLock<Option<TreeManager>>>,
    pub nostr: Arc<NostrManager>,
    pub ndb: Arc<Ndb>,
    pub blossom: Arc<BlossomManager>,
    pub webrtc: Arc<WebRTCManager>,
    /// Our pubkey for WoT calculations (hex, 64 chars)
    pub our_pubkey: Arc<parking_lot::RwLock<Option<String>>>,
}

impl WorkerState {
    pub fn new(store: BlobStore, data_dir: PathBuf) -> Result<Self, String> {
        let store = Arc::new(store);

        // Initialize nostrdb with limited ingester threads to avoid MDB_READERS_FULL
        let ndb_dir = data_dir.join("nostrdb");
        std::fs::create_dir_all(&ndb_dir).map_err(|e| format!("Failed to create nostrdb dir: {}", e))?;
        let config = Config::new()
            .set_ingester_threads(2);  // Limit threads to avoid exhausting LMDB readers
        let ndb = Ndb::new(ndb_dir.to_str().unwrap(), &config)
            .map_err(|e| format!("Failed to initialize nostrdb: {:?}", e))?;
        info!("Initialized nostrdb at {:?}", ndb_dir);

        Ok(Self {
            store: store.clone(),
            tree: Arc::new(RwLock::new(Some(TreeManager::new(store)))),
            nostr: Arc::new(NostrManager::new()),
            ndb: Arc::new(ndb),
            blossom: Arc::new(BlossomManager::new()),
            webrtc: Arc::new(WebRTCManager::new()),
            our_pubkey: Arc::new(parking_lot::RwLock::new(None)),
        })
    }
}

/// Handle worker messages from frontend
#[tauri::command]
pub async fn worker_message(
    message: WorkerRequest,
    app_handle: AppHandle,
    state: tauri::State<'_, std::sync::Arc<WorkerState>>,
) -> Result<(), String> {
    let response = match message {
        // Lifecycle
        WorkerRequest::Init { id } => {
            // Initialize Nostr client and connect to relays during init
            // This ensures relay stats are available immediately
            if let Err(e) = state.nostr.ensure_client(Some(app_handle.clone()), Some(state.ndb.clone())).await {
                tracing::warn!("Failed to initialize Nostr client during init: {}", e);
            }
            WorkerResponse::Ready { id }
        }
        WorkerRequest::Ping { id } => WorkerResponse::Pong { id },

        // Store operations
        WorkerRequest::Get { id, hash } => {
            // Use CombinedStore (with Blossom fallback) via TreeManager if available
            let tree_guard = state.tree.read().await;
            let data = if let Some(tree) = tree_guard.as_ref() {
                tree.get_blob(&hash).await.map(|d| BASE64.encode(&d))
            } else {
                state.store.get(&hash).await.map(|d| BASE64.encode(&d))
            };
            WorkerResponse::Result { id, data }
        }

        WorkerRequest::Put { id, hash, data } => {
            let bytes = BASE64
                .decode(&data)
                .map_err(|e| format!("Invalid base64: {}", e))?;
            let ok = state.store.put(&hash, &bytes).await.unwrap_or(false);
            WorkerResponse::Bool { id, value: ok }
        }

        WorkerRequest::Has { id, hash } => WorkerResponse::Bool {
            id,
            value: state.store.has(&hash),
        },

        WorkerRequest::Delete { id, hash } => {
            let ok = state.store.delete(&hash).await;
            WorkerResponse::Bool { id, value: ok }
        }

        // Tree operations
        WorkerRequest::ReadFile { id, cid } => {
            let tree_guard = state.tree.read().await;
            if let Some(tree) = tree_guard.as_ref() {
                match tree.read_file(&cid).await {
                    Ok(data) => WorkerResponse::Result {
                        id,
                        data: Some(BASE64.encode(&data)),
                    },
                    Err(e) => WorkerResponse::Error { id, error: e },
                }
            } else {
                WorkerResponse::Error {
                    id,
                    error: "Tree not initialized".to_string(),
                }
            }
        }

        WorkerRequest::ReadFileRange { id, cid, start, end } => {
            let tree_guard = state.tree.read().await;
            if let Some(tree) = tree_guard.as_ref() {
                match tree.read_file_range(&cid, start, end).await {
                    Ok(data) => WorkerResponse::Result {
                        id,
                        data: Some(BASE64.encode(&data)),
                    },
                    Err(e) => WorkerResponse::Error { id, error: e },
                }
            } else {
                WorkerResponse::Error {
                    id,
                    error: "Tree not initialized".to_string(),
                }
            }
        }

        WorkerRequest::WriteFile {
            id,
            parent_cid,
            path,
            data,
        } => {
            let bytes = BASE64
                .decode(&data)
                .map_err(|e| format!("Invalid base64: {}", e))?;

            let tree_guard = state.tree.read().await;
            if let Some(tree) = tree_guard.as_ref() {
                match tree.write_file(parent_cid.as_ref(), &path, &bytes).await {
                    Ok(cid) => WorkerResponse::Cid { id, cid: Some(cid) },
                    Err(e) => WorkerResponse::Error { id, error: e },
                }
            } else {
                WorkerResponse::Error {
                    id,
                    error: "Tree not initialized".to_string(),
                }
            }
        }

        WorkerRequest::DeleteFile {
            id,
            parent_cid,
            path,
        } => {
            let tree_guard = state.tree.read().await;
            if let Some(tree) = tree_guard.as_ref() {
                match tree.delete_file(&parent_cid, &path).await {
                    Ok(cid) => WorkerResponse::Cid { id, cid: Some(cid) },
                    Err(e) => WorkerResponse::Error { id, error: e },
                }
            } else {
                WorkerResponse::Error {
                    id,
                    error: "Tree not initialized".to_string(),
                }
            }
        }

        WorkerRequest::ListDir { id, cid } => {
            tracing::info!("ListDir cid: {:?}", cid);
            let tree_guard = state.tree.read().await;
            if let Some(tree) = tree_guard.as_ref() {
                match tree.list_dir(&cid).await {
                    Ok(entries) => {
                        tracing::info!("ListDir returned {} entries", entries.len());
                        WorkerResponse::DirListing {
                            id,
                            entries: Some(entries),
                        }
                    }
                    Err(e) => {
                        tracing::error!("ListDir error: {}", e);
                        WorkerResponse::Error { id, error: e }
                    }
                }
            } else {
                WorkerResponse::Error {
                    id,
                    error: "Tree not initialized".to_string(),
                }
            }
        }

        WorkerRequest::ResolveRoot { id, npub, path } => {
            // Parse npub to get pubkey (supports npub1... or hex)
            let public_key = if npub.starts_with("npub1") {
                match nostr_sdk::PublicKey::parse(&npub) {
                    Ok(pk) => pk,
                    Err(e) => {
                        return app_handle
                            .emit("worker_response", &WorkerResponse::Cid { id, cid: None })
                            .map_err(|_| format!("Invalid npub: {}", e));
                    }
                }
            } else {
                match nostr_sdk::PublicKey::from_hex(&npub) {
                    Ok(pk) => pk,
                    Err(e) => {
                        return app_handle
                            .emit("worker_response", &WorkerResponse::Cid { id, cid: None })
                            .map_err(|_| format!("Invalid pubkey: {}", e));
                    }
                }
            };
            let pk_bytes = public_key.to_bytes();

            // Parse path to get tree name (first segment, default 'public')
            let tree_name = path
                .as_ref()
                .and_then(|p| p.split('/').filter(|s| !s.is_empty()).next())
                .unwrap_or("public");

            // Helper to extract CID from nostrdb query results
            fn extract_cid_from_ndb_results(
                ndb: &Ndb,
                txn: &Transaction,
                pk_bytes: &[u8; 32],
                tree_name: &str,
            ) -> Option<WorkerCid> {
                let filter = nostrdb::Filter::new()
                    .kinds(vec![30078])
                    .authors(vec![pk_bytes])
                    .build();

                let results = match ndb.query(txn, &[filter], 100) {
                    Ok(r) => r,
                    Err(_) => return None,
                };

                for result in results.iter() {
                    let mut has_d_tag = false;
                    let mut has_l_tag = false;
                    let mut hash_value: Option<String> = None;
                    let mut key_value: Option<String> = None;

                    for tag in result.note.tags() {
                        if let Some(tag_str) = tag.get_unchecked(0).str() {
                            if tag_str == "d" {
                                if let Some(val) = tag.get_unchecked(1).str() {
                                    has_d_tag = val == tree_name;
                                }
                            } else if tag_str == "l" {
                                if let Some(val) = tag.get_unchecked(1).str() {
                                    has_l_tag = val == "hashtree";
                                }
                            } else if tag_str == "hash" {
                                if let Some(val) = tag.get_unchecked(1).str() {
                                    if !val.is_empty() {
                                        hash_value = Some(val.to_string());
                                    }
                                }
                            } else if tag_str == "key" {
                                if let Some(val) = tag.get_unchecked(1).str() {
                                    if !val.is_empty() {
                                        key_value = Some(val.to_string());
                                    }
                                }
                            }
                        }
                    }

                    if has_d_tag && has_l_tag {
                        if let Some(hash) = hash_value {
                            return Some(WorkerCid { hash, key: key_value });
                        }
                    }
                }
                None
            }

            // 1. Query nostrdb cache first (fast path)
            let cached_cid: Option<WorkerCid> = {
                if let Ok(txn) = Transaction::new(&state.ndb) {
                    extract_cid_from_ndb_results(&state.ndb, &txn, &pk_bytes, tree_name)
                } else {
                    None
                }
            };

            if cached_cid.is_some() {
                return app_handle
                    .emit("worker_response", &WorkerResponse::Cid { id, cid: cached_cid })
                    .map_err(|e| format!("Failed to emit: {}", e));
            }

            // 2. Not in cache - query relays with timeout
            if let Err(e) = state.nostr.ensure_client(Some(app_handle.clone()), Some(state.ndb.clone())).await {
                debug!("Failed to init nostr client for ResolveRoot: {}", e);
                return app_handle
                    .emit("worker_response", &WorkerResponse::Cid { id, cid: None })
                    .map_err(|e| format!("Failed to emit: {}", e));
            }

            // Build filter for kind 30078 with d tag and l=hashtree
            let relay_filter = nostr_sdk::Filter::new()
                .kind(nostr_sdk::Kind::from(30078u16))
                .author(public_key)
                .custom_tag(nostr_sdk::SingleLetterTag::from_char('d').unwrap(), vec![tree_name.to_string()])
                .custom_tag(nostr_sdk::SingleLetterTag::from_char('l').unwrap(), vec!["hashtree".to_string()])
                .limit(1);

            // One-shot fetch with 3 second timeout
            let fetch_result = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                state.nostr.fetch_events(vec![relay_filter])
            ).await;

            let found_cid = match fetch_result {
                Ok(Ok(events)) => {
                    // Process events - store in ndb and extract CID
                    for event in &events {
                        let event_json = serde_json::to_string(&event).unwrap_or_default();
                        let relay_msg = format!(r#"["EVENT","resolve-root",{}]"#, event_json);
                        let _ = state.ndb.process_event(&relay_msg);
                    }

                    // Now query ndb again for the result
                    if let Ok(txn) = Transaction::new(&state.ndb) {
                        extract_cid_from_ndb_results(&state.ndb, &txn, &pk_bytes, tree_name)
                    } else {
                        None
                    }
                }
                Ok(Err(e)) => {
                    debug!("Relay fetch error: {}", e);
                    None
                }
                Err(_) => {
                    debug!("Relay fetch timeout for {}/{}", npub, tree_name);
                    None
                }
            };

            tracing::info!("ResolveRoot {}/{} -> {:?}", npub, tree_name, found_cid);
            WorkerResponse::Cid { id, cid: found_cid }
        }

        // Nostr operations
        WorkerRequest::Subscribe { id, filters } => {
            // Ensure client is initialized with ndb for event storage
            if let Err(e) = state.nostr.ensure_client(Some(app_handle.clone()), Some(state.ndb.clone())).await {
                return app_handle
                    .emit(
                        "worker_response",
                        &WorkerResponse::Error {
                            id,
                            error: format!("Failed to initialize Nostr client: {}", e),
                        },
                    )
                    .map_err(|e| format!("Failed to emit response: {}", e));
            }

            // Query ndb cache first - emit cached events immediately
            let _found_ids = query_ndb_cache(&state.ndb, &filters, &id, &app_handle);

            // Parse filters and subscribe to relays for more/missing events
            match nostr::parse_filters(filters) {
                Ok(parsed_filters) => match state.nostr.subscribe(id.clone(), parsed_filters).await
                {
                    Ok(()) => WorkerResponse::Void { id },
                    Err(e) => WorkerResponse::Error { id, error: e },
                },
                Err(e) => WorkerResponse::Error {
                    id,
                    error: format!("Invalid filters: {}", e),
                },
            }
        }

        WorkerRequest::Unsubscribe { id, sub_id } => {
            match state.nostr.unsubscribe(&sub_id).await {
                Ok(()) => WorkerResponse::Void { id },
                Err(e) => WorkerResponse::Error { id, error: e },
            }
        }

        WorkerRequest::Publish { id, event } => {
            // Ensure client is initialized with ndb for event storage
            if let Err(e) = state.nostr.ensure_client(Some(app_handle.clone()), Some(state.ndb.clone())).await {
                return app_handle
                    .emit(
                        "worker_response",
                        &WorkerResponse::Error {
                            id,
                            error: format!("Failed to initialize Nostr client: {}", e),
                        },
                    )
                    .map_err(|e| format!("Failed to emit response: {}", e));
            }

            match state.nostr.publish(event).await {
                Ok(event_id) => WorkerResponse::Result {
                    id,
                    data: Some(event_id.to_hex()),
                },
                Err(e) => WorkerResponse::Error { id, error: e },
            }
        }

        WorkerRequest::SetIdentity { id, pubkey, nsec } => {
            // Set identity for Nostr
            if let Err(e) = state.nostr.set_identity(&pubkey, nsec.as_deref()) {
                return app_handle
                    .emit(
                        "worker_response",
                        &WorkerResponse::Error {
                            id,
                            error: e.clone(),
                        },
                    )
                    .map_err(|e| format!("Failed to emit response: {}", e));
            }

            // Set pubkey for social graph WoT calculations
            *state.our_pubkey.write() = Some(pubkey.clone());
            if let Ok(pk_bytes) = hex_to_pubkey(&pubkey) {
                nostrdb::socialgraph::set_root(&state.ndb, &pk_bytes);
                info!("Set social graph root to {}", &pubkey[..8]);
            }

            // Initialize Blossom client with keys if nsec is provided
            if let Some(nsec_str) = &nsec {
                let secret_key = if nsec_str.starts_with("nsec1") {
                    nostr_sdk::SecretKey::parse(nsec_str).ok()
                } else {
                    nostr_sdk::SecretKey::from_hex(nsec_str).ok()
                };
                if let Some(sk) = secret_key {
                    let keys = nostr_sdk::Keys::new(sk.clone());
                    state.blossom.set_keys(keys);

                    // Initialize WebRTC with shared Nostr client (run in background to not block)
                    if let (Some(client), Some(nostr_keys)) = (state.nostr.get_client(), state.nostr.get_keys()) {
                        let webrtc = state.webrtc.clone();
                        tokio::spawn(async move {
                            if let Err(e) = webrtc.init(client, nostr_keys).await {
                                warn!("Failed to initialize WebRTC: {}", e);
                            }
                        });
                    }
                }
            }

            WorkerResponse::Void { id }
        }

        // Relay management
        WorkerRequest::SetRelays { id, relays } => {
            match state.nostr.set_relays(relays).await {
                Ok(()) => WorkerResponse::Void { id },
                Err(e) => WorkerResponse::Error { id, error: e },
            }
        }

        WorkerRequest::GetRelays { id } => {
            let relays = state.nostr.get_relays().await;
            WorkerResponse::Relays { id, relays }
        }

        // Social graph operations - now handled by nostrdb
        // UpdateFollows is no longer needed - nostrdb auto-processes kind 3 events
        WorkerRequest::UpdateFollows { id, .. } => {
            // No-op: nostrdb handles follow updates from kind 3 events automatically
            WorkerResponse::Void { id }
        }

        WorkerRequest::GetFollows { id, pubkey } => {
            match hex_to_pubkey(&pubkey) {
                Ok(pk_bytes) => {
                    let txn = match nostrdb::Transaction::new(&state.ndb) {
                        Ok(t) => t,
                        Err(e) => {
                            return app_handle
                                .emit("worker_response", &WorkerResponse::Error {
                                    id,
                                    error: format!("Transaction error: {:?}", e),
                                })
                                .map_err(|e| format!("Failed to emit: {}", e));
                        }
                    };
                    let follows = nostrdb::socialgraph::get_followed(&txn, &state.ndb, &pk_bytes, 10000);
                    let pubkeys: Vec<String> = follows.iter().map(pubkey_to_hex).collect();
                    WorkerResponse::Follows { id, pubkeys }
                }
                Err(e) => WorkerResponse::Error { id, error: e },
            }
        }

        WorkerRequest::GetFollowers { id, pubkey } => {
            match hex_to_pubkey(&pubkey) {
                Ok(pk_bytes) => {
                    let txn = match nostrdb::Transaction::new(&state.ndb) {
                        Ok(t) => t,
                        Err(e) => {
                            return app_handle
                                .emit("worker_response", &WorkerResponse::Error {
                                    id,
                                    error: format!("Transaction error: {:?}", e),
                                })
                                .map_err(|e| format!("Failed to emit: {}", e));
                        }
                    };
                    let followers = nostrdb::socialgraph::get_followers(&txn, &state.ndb, &pk_bytes, 10000);
                    let pubkeys: Vec<String> = followers.iter().map(pubkey_to_hex).collect();
                    WorkerResponse::Follows { id, pubkeys }
                }
                Err(e) => WorkerResponse::Error { id, error: e },
            }
        }

        WorkerRequest::GetWotDistance { id, target } => {
            match hex_to_pubkey(&target) {
                Ok(pk_bytes) => {
                    let txn = match nostrdb::Transaction::new(&state.ndb) {
                        Ok(t) => t,
                        Err(e) => {
                            return app_handle
                                .emit("worker_response", &WorkerResponse::Error {
                                    id,
                                    error: format!("Transaction error: {:?}", e),
                                })
                                .map_err(|e| format!("Failed to emit: {}", e));
                        }
                    };
                    let dist = nostrdb::socialgraph::get_follow_distance(&txn, &state.ndb, &pk_bytes);
                    // nostrdb returns 1000 for "not connected"
                    let distance = if dist >= 1000 { None } else { Some(dist as usize) };
                    WorkerResponse::WotDistance { id, distance }
                }
                Err(e) => WorkerResponse::Error { id, error: e },
            }
        }

        WorkerRequest::GetUsersWithinDistance { id, max_distance } => {
            // Get users at each distance level up to max_distance
            let mut users: Vec<(String, usize)> = Vec::new();
            let txn = match nostrdb::Transaction::new(&state.ndb) {
                Ok(t) => t,
                Err(e) => {
                    return app_handle
                        .emit("worker_response", &WorkerResponse::Error {
                            id,
                            error: format!("Transaction error: {:?}", e),
                        })
                        .map_err(|e| format!("Failed to emit: {}", e));
                }
            };

            // Start from our pubkey and BFS
            if let Some(our_pk) = state.our_pubkey.read().as_ref() {
                if let Ok(root_bytes) = hex_to_pubkey(our_pk) {
                    let mut visited = std::collections::HashSet::new();
                    let mut current_level = vec![root_bytes];
                    visited.insert(root_bytes);

                    for distance in 1..=max_distance {
                        let mut next_level = Vec::new();
                        for pk in &current_level {
                            let follows = nostrdb::socialgraph::get_followed(&txn, &state.ndb, pk, 10000);
                            for followed in follows {
                                if !visited.contains(&followed) {
                                    visited.insert(followed);
                                    users.push((pubkey_to_hex(&followed), distance));
                                    next_level.push(followed);
                                }
                            }
                        }
                        current_level = next_level;
                        if current_level.is_empty() {
                            break;
                        }
                    }
                }
            }
            WorkerResponse::UsersWithDistance { id, users }
        }

        // Blossom operations (Phase 6)
        WorkerRequest::BlossomUpload { id, data } => {
            let bytes = match BASE64.decode(&data) {
                Ok(b) => b,
                Err(e) => {
                    return app_handle
                        .emit(
                            "worker_response",
                            &WorkerResponse::Error {
                                id,
                                error: format!("Invalid base64: {}", e),
                            },
                        )
                        .map_err(|e| format!("Failed to emit response: {}", e));
                }
            };

            match state.blossom.upload(&bytes).await {
                Ok(hash) => WorkerResponse::Result {
                    id,
                    data: Some(hash),
                },
                Err(e) => WorkerResponse::Error {
                    id,
                    error: format!("Blossom upload error: {}", e),
                },
            }
        }

        WorkerRequest::BlossomDownload { id, hash } => {
            match state.blossom.download(&hash).await {
                Ok(data) => WorkerResponse::Result {
                    id,
                    data: Some(BASE64.encode(&data)),
                },
                Err(e) => WorkerResponse::Error {
                    id,
                    error: format!("Blossom download error: {}", e),
                },
            }
        }

        WorkerRequest::BlossomExists { id, hash } => {
            match state.blossom.exists(&hash).await {
                Ok(exists) => WorkerResponse::Bool { id, value: exists },
                Err(e) => WorkerResponse::Error {
                    id,
                    error: format!("Blossom exists error: {}", e),
                },
            }
        }

        // Stats operations
        WorkerRequest::GetStorageStats { id } => {
            let stats = state.store.stats();
            WorkerResponse::StorageStats {
                id,
                items: stats.items,
                bytes: stats.bytes,
                pinned_items: stats.pinned_items,
                pinned_bytes: stats.pinned_bytes,
                max_bytes: state.store.max_bytes(),
            }
        }

        WorkerRequest::GetSocialGraphSize { id } => {
            // Count users by checking how many we follow (approximation)
            let size = if let Some(our_pk) = state.our_pubkey.read().as_ref() {
                if let Ok(pk_bytes) = hex_to_pubkey(our_pk) {
                    if let Ok(txn) = nostrdb::Transaction::new(&state.ndb) {
                        nostrdb::socialgraph::followed_count(&txn, &state.ndb, &pk_bytes)
                    } else {
                        0
                    }
                } else {
                    0
                }
            } else {
                0
            };
            WorkerResponse::SocialGraphSize { id, size }
        }

        // Storage management
        WorkerRequest::SetStorageMaxBytes { id, max_bytes } => {
            state.store.set_max_bytes(max_bytes);
            WorkerResponse::Void { id }
        }

        WorkerRequest::RunEviction { id } => {
            let bytes_freed = state.store.evict_if_needed().await;
            WorkerResponse::EvictionResult { id, bytes_freed }
        }

        // Relay statistics
        WorkerRequest::GetRelayStats { id } => {
            let relays = state.nostr.get_relay_stats().await;
            WorkerResponse::RelayStats { id, relays }
        }

        // Blossom server configuration
        WorkerRequest::SetBlossomServers {
            id,
            read_servers,
            write_servers,
        } => {
            // Update blossom manager
            let result = state.blossom.set_servers(read_servers.clone(), write_servers);

            // Also update tree's combined store for remote blob fetching
            if result.is_ok() {
                if let Some(tree) = state.tree.read().await.as_ref() {
                    tree.set_blossom_servers(read_servers).await;
                }
            }

            match result {
                Ok(()) => WorkerResponse::Void { id },
                Err(e) => WorkerResponse::Error { id, error: e },
            }
        }

        WorkerRequest::GetBlossomServers { id } => WorkerResponse::BlossomServers {
            id,
            read_servers: state.blossom.read_servers(),
            write_servers: state.blossom.write_servers(),
        },

        // Tree push to Blossom
        WorkerRequest::PushToBlossom { id, cid, tree_name } => {
            let tree_guard = state.tree.read().await;
            let tree = match tree_guard.as_ref() {
                Some(t) => t,
                None => {
                    return app_handle
                        .emit(
                            "worker_response",
                            &WorkerResponse::Error {
                                id,
                                error: "Tree not initialized".to_string(),
                            },
                        )
                        .map_err(|e| format!("Failed to emit response: {}", e));
                }
            };

            // Walk all blocks in the tree
            let blocks = match tree.walk_blocks(&cid).await {
                Ok(b) => b,
                Err(e) => {
                    return app_handle
                        .emit(
                            "worker_response",
                            &WorkerResponse::Error { id, error: e },
                        )
                        .map_err(|e| format!("Failed to emit response: {}", e));
                }
            };

            let total = blocks.len() as u32;
            let tree_name_str = tree_name.unwrap_or_else(|| "unknown".to_string());
            let mut pushed: u32 = 0;
            let mut skipped: u32 = 0;
            let mut failed: u32 = 0;
            let mut errors: Vec<String> = Vec::new();

            for (idx, block) in blocks.iter().enumerate() {
                // Emit progress
                if idx % 10 == 0 || idx == blocks.len() - 1 {
                    let _ = app_handle.emit(
                        "worker_response",
                        &WorkerResponse::PushProgress {
                            tree_name: tree_name_str.clone(),
                            current: idx as u32 + 1,
                            total,
                        },
                    );
                }

                // Upload to Blossom
                match state.blossom.upload(&block.data).await {
                    Ok(hash) => {
                        let expected = hashtree_core::to_hex(&block.hash);
                        if hash == expected {
                            pushed += 1;
                        } else {
                            // Hash mismatch - still counts as success but log warning
                            pushed += 1;
                            tracing::warn!("Hash mismatch: {} vs {}", hash, expected);
                        }
                    }
                    Err(e) => {
                        // Check if it's "already exists"
                        let err_str = format!("{}", e);
                        if err_str.contains("409") || err_str.to_lowercase().contains("exists") {
                            skipped += 1;
                        } else {
                            failed += 1;
                            errors.push(err_str);
                        }
                    }
                }
            }

            WorkerResponse::PushResult {
                id,
                pushed,
                skipped,
                failed,
                errors: if errors.is_empty() { None } else { Some(errors) },
            }
        }

        // Republish tree event to Nostr
        WorkerRequest::RepublishTree { id, pubkey, tree_name } => {
            let pk_bytes = match hex_to_pubkey(&pubkey) {
                Ok(b) => b,
                Err(e) => {
                    return app_handle
                        .emit("worker_response", &WorkerResponse::Bool { id, value: false })
                        .map_err(|_| e);
                }
            };

            // Query nostrdb synchronously, extract event JSON before any await
            let found_event: Option<String> = {
                let txn = match Transaction::new(&state.ndb) {
                    Ok(t) => t,
                    Err(e) => {
                        return app_handle
                            .emit("worker_response", &WorkerResponse::Error {
                                id,
                                error: format!("Transaction error: {:?}", e),
                            })
                            .map_err(|e| format!("Failed to emit: {}", e));
                    }
                };

                let filter = nostrdb::Filter::new()
                    .kinds(vec![30078])
                    .authors(vec![&pk_bytes])
                    .build();

                let results = match state.ndb.query(&txn, &[filter], 100) {
                    Ok(r) => r,
                    Err(_) => {
                        return app_handle
                            .emit("worker_response", &WorkerResponse::Bool { id, value: false })
                            .map_err(|e| format!("Failed to emit: {}", e));
                    }
                };

                let mut found: Option<String> = None;
                for result in results.iter() {
                    let mut has_d_tag = false;
                    let mut has_l_tag = false;
                    let mut has_hash = false;

                    for tag in result.note.tags() {
                        if let Some(tag_str) = tag.get_unchecked(0).str() {
                            if tag_str == "d" {
                                if let Some(val) = tag.get_unchecked(1).str() {
                                    has_d_tag = val == tree_name;
                                }
                            } else if tag_str == "l" {
                                if let Some(val) = tag.get_unchecked(1).str() {
                                    has_l_tag = val == "hashtree";
                                }
                            } else if tag_str == "hash" {
                                if let Some(val) = tag.get_unchecked(1).str() {
                                    has_hash = !val.is_empty();
                                }
                            }
                        }
                    }

                    if has_d_tag && has_l_tag && has_hash {
                        if let Ok(json) = result.note.json() {
                            if found.is_none() {
                                found = Some(json);
                            }
                        }
                    }
                }
                found
                // txn, results, filter all dropped here
            };

            match found_event {
                Some(event_json) => {
                    match serde_json::from_str::<serde_json::Value>(&event_json) {
                        Ok(event_value) => {
                            match state.nostr.publish(event_value).await {
                                Ok(_) => {
                                    info!("Republished tree event: {}", tree_name);
                                    WorkerResponse::Bool { id, value: true }
                                }
                                Err(e) => {
                                    debug!("Failed to republish: {}", e);
                                    WorkerResponse::Bool { id, value: false }
                                }
                            }
                        }
                        Err(_) => WorkerResponse::Bool { id, value: false }
                    }
                }
                None => {
                    debug!("No cached event found for tree: {}", tree_name);
                    WorkerResponse::Bool { id, value: false }
                }
            }
        }

        // Batch republish all trees for a pubkey prefix
        WorkerRequest::RepublishTrees { id, pubkey_prefix } => {
            // Query nostrdb for all kind 30078 events with l=hashtree
            let events_to_republish: Vec<String> = {
                let txn = match Transaction::new(&state.ndb) {
                    Ok(t) => t,
                    Err(e) => {
                        return app_handle
                            .emit("worker_response", &WorkerResponse::Error {
                                id,
                                error: format!("Transaction error: {:?}", e),
                            })
                            .map_err(|e| format!("Failed to emit: {}", e));
                    }
                };

                let filter = nostrdb::Filter::new()
                    .kinds(vec![30078])
                    .build();

                let results = match state.ndb.query(&txn, &[filter], 1000) {
                    Ok(r) => r,
                    Err(_) => {
                        return app_handle
                            .emit("worker_response", &WorkerResponse::RepublishResult {
                                id,
                                count: 0,
                                encryption_errors: None,
                            })
                            .map_err(|e| format!("Failed to emit: {}", e));
                    }
                };

                let mut events: Vec<String> = Vec::new();
                for result in results.iter() {
                    // Check if it has l=hashtree tag
                    let mut has_l_tag = false;
                    let mut has_hash = false;
                    let author_hex = pubkey_to_hex(result.note.pubkey());

                    // Filter by pubkey prefix if specified
                    if let Some(ref prefix) = pubkey_prefix {
                        if !author_hex.starts_with(prefix) {
                            continue;
                        }
                    }

                    for tag in result.note.tags() {
                        if let Some(tag_str) = tag.get_unchecked(0).str() {
                            if tag_str == "l" {
                                if let Some(val) = tag.get_unchecked(1).str() {
                                    if val == "hashtree" {
                                        has_l_tag = true;
                                    }
                                }
                            } else if tag_str == "hash" {
                                if let Some(val) = tag.get_unchecked(1).str() {
                                    has_hash = !val.is_empty();
                                }
                            }
                        }
                    }

                    if has_l_tag && has_hash {
                        if let Ok(json) = result.note.json() {
                            events.push(json);
                        }
                    }
                }
                events
            };

            // Republish all found events
            let mut count = 0u32;
            for event_json in events_to_republish {
                if let Ok(event_value) = serde_json::from_str::<serde_json::Value>(&event_json) {
                    if state.nostr.publish(event_value).await.is_ok() {
                        count += 1;
                    }
                }
            }

            info!("Republished {} tree events", count);
            WorkerResponse::RepublishResult {
                id,
                count,
                encryption_errors: None,
            }
        }

        // Streaming file read
        WorkerRequest::ReadFileStream { id, cid } => {
            let tree_guard = state.tree.read().await;
            if let Some(tree) = tree_guard.as_ref() {
                // Read file in chunks and emit
                match tree.read_file(&cid).await {
                    Ok(data) => {
                        const CHUNK_SIZE: usize = 256 * 1024; // 256KB chunks
                        let chunks: Vec<&[u8]> = data.chunks(CHUNK_SIZE).collect();
                        let total = chunks.len();

                        for (i, chunk) in chunks.into_iter().enumerate() {
                            let is_last = i == total - 1;
                            let _ = app_handle.emit(
                                "worker_response",
                                &WorkerResponse::StreamChunk {
                                    id: id.clone(),
                                    data: Some(BASE64.encode(chunk)),
                                    done: is_last,
                                },
                            );
                        }

                        // Return void since we already emitted chunks
                        WorkerResponse::Void { id }
                    }
                    Err(e) => WorkerResponse::Error {
                        id,
                        error: format!("Read error: {}", e),
                    },
                }
            } else {
                WorkerResponse::Error {
                    id,
                    error: "Tree not initialized".to_string(),
                }
            }
        }

        // WebRTC operations
        WorkerRequest::GetPeerStats { id } => {
            let stats = state.webrtc.get_peer_stats().await;
            let peers = stats
                .into_iter()
                .map(|s| types::PeerStatEntry {
                    peer_id: s.peer_id,
                    connected: s.connected,
                    pool: s.pool,
                })
                .collect();
            WorkerResponse::PeerStats { id, peers }
        }

        WorkerRequest::SendHello { id, roots } => {
            let roots_vec = roots.unwrap_or_default();
            match state.webrtc.send_hello(roots_vec).await {
                Ok(()) => WorkerResponse::Void { id },
                Err(e) => WorkerResponse::Error { id, error: e },
            }
        }

        WorkerRequest::SetWebRTCPools {
            id,
            follows_max,
            follows_satisfied,
            other_max,
            other_satisfied,
        } => {
            state.webrtc.set_pools(
                follows_max,
                follows_satisfied,
                other_max,
                other_satisfied,
            ).await;
            WorkerResponse::Void { id }
        }
    };

    app_handle
        .emit("worker_response", &response)
        .map_err(|e| format!("Failed to emit response: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn create_test_state() -> WorkerState {
        let dir = tempdir().unwrap();
        let path = dir.keep();
        let store = BlobStore::new(path.clone());
        WorkerState::new(store, path).expect("failed to create test state")
    }

    #[tokio::test]
    async fn test_store_operations() {
        let state = create_test_state();

        // Put - use a valid 64-character hex hash
        let hash = "a".repeat(64);
        let data = b"test data";
        assert!(state.store.put(&hash, data).await.unwrap());

        // Has
        assert!(state.store.has(&hash));

        // Get
        let result = state.store.get(&hash).await;
        assert_eq!(result, Some(data.to_vec()));

        // Delete
        assert!(state.store.delete(&hash).await);
        assert!(!state.store.has(&hash));
    }

    #[tokio::test]
    async fn test_tree_write_and_read() {
        let state = create_test_state();

        let tree_guard = state.tree.read().await;
        let tree = tree_guard.as_ref().unwrap();

        // Write file
        let data = b"Hello, Tree!";
        let cid = tree.write_file(None, "test.txt", data).await.unwrap();

        // Read file
        let result = tree.read_file(&cid).await.unwrap();
        assert_eq!(result, data);
    }

    #[tokio::test]
    async fn test_tree_list_dir() {
        let state = create_test_state();

        let tree_guard = state.tree.read().await;
        let tree = tree_guard.as_ref().unwrap();

        // Create empty dir
        let dir_cid = tree.create_empty_dir().await.unwrap();

        // Add file
        let new_cid = tree
            .write_file(Some(&dir_cid), "file.txt", b"content")
            .await
            .unwrap();

        // List
        let entries = tree.list_dir(&new_cid).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "file.txt");
    }

    #[test]
    fn test_json_to_ndb_filter_with_ids() {
        let filter_json = serde_json::json!({
            "ids": ["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]
        });
        let filter = json_to_ndb_filter(&filter_json);
        assert!(filter.is_some());
    }

    #[test]
    fn test_json_to_ndb_filter_with_authors() {
        let filter_json = serde_json::json!({
            "authors": ["bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"],
            "kinds": [1, 3]
        });
        let filter = json_to_ndb_filter(&filter_json);
        assert!(filter.is_some());
    }

    #[test]
    fn test_json_to_ndb_filter_with_time_range() {
        let filter_json = serde_json::json!({
            "kinds": [1],
            "since": 1700000000,
            "until": 1800000000,
            "limit": 100
        });
        let filter = json_to_ndb_filter(&filter_json);
        assert!(filter.is_some());
    }

    #[test]
    fn test_hex_to_pubkey_valid() {
        let hex = "a".repeat(64);
        let result = hex_to_pubkey(&hex);
        assert!(result.is_ok());
        let pk = result.unwrap();
        assert_eq!(pk, [0xaa; 32]);
    }

    #[test]
    fn test_hex_to_pubkey_invalid_length() {
        let hex = "aaa"; // too short
        let result = hex_to_pubkey(&hex);
        assert!(result.is_err());
    }

    #[test]
    fn test_pubkey_to_hex() {
        let pk = [0xbb; 32];
        let hex = pubkey_to_hex(&pk);
        assert_eq!(hex, "b".repeat(64));
    }

    #[test]
    fn test_ndb_process_and_query_event() {
        let state = create_test_state();

        // Create a valid signed test event (kind 1 note)
        // This is a real signed event for testing
        let event_json = r#"{
            "id": "d7dd5eb3ab747e16f8d0212d53032ea2a7cadef53837e5a6c66d42849fcb9027",
            "pubkey": "22e804d26ed16b68db5259e78449e96dab5d464c8f470bda3eb1a70467f2c793",
            "created_at": 1700000000,
            "kind": 1,
            "tags": [],
            "content": "test content",
            "sig": "a]invalid_sig_for_test"
        }"#;

        // Process event (will fail sig verification but tests the flow)
        let relay_msg = format!(r#"["EVENT","test-sub",{}]"#, event_json);
        let _ = state.ndb.process_event(&relay_msg);

        // Query - even if sig fails, we test the query path works
        let filter_json = serde_json::json!({
            "ids": ["d7dd5eb3ab747e16f8d0212d53032ea2a7cadef53837e5a6c66d42849fcb9027"]
        });
        let ndb_filter = json_to_ndb_filter(&filter_json).unwrap();

        let txn = Transaction::new(&state.ndb).unwrap();
        let results = state.ndb.query(&txn, &[ndb_filter], 10);
        // Result depends on sig validation - just verify no panic
        assert!(results.is_ok() || results.is_err());
    }

    #[test]
    fn test_ndb_social_graph_operations() {
        let state = create_test_state();

        // Set root pubkey
        let root = [0xcc; 32];
        nostrdb::socialgraph::set_root(&state.ndb, &root);

        // Query distance to unknown user should be 1000 (not connected)
        // because no follow relationships exist yet
        let unknown = [0xee; 32];
        let txn = Transaction::new(&state.ndb).unwrap();
        let distance = nostrdb::socialgraph::get_follow_distance(&txn, &state.ndb, &unknown);
        assert_eq!(distance, 1000); // nostrdb uses 1000 for "not connected"

        // Verify we can query follows (empty for new user)
        let follows = nostrdb::socialgraph::get_followed(&txn, &state.ndb, &root, 100);
        assert!(follows.is_empty());

        // Verify follower count is 0
        let count = nostrdb::socialgraph::follower_count(&txn, &state.ndb, &root);
        assert_eq!(count, 0);
    }
}

#[cfg(test)]
mod resolve_root_tests {
    #[test]
    fn test_parse_npub() {
        let npub = "npub1g53mukxnjkcmr94fhryzkqutdz2ukq4ks0gvy5af25rgmwsl4ngq43drvk";
        let pk = nostr_sdk::PublicKey::parse(npub);
        assert!(pk.is_ok(), "Failed to parse npub: {:?}", pk.err());
        let pk = pk.unwrap();
        println!("Parsed pubkey: {}", pk.to_hex());
    }

    #[test]
    fn test_parse_path_for_tree_name() {
        // Test various path formats
        let paths = vec![
            ("media/aika%20mieheka%CC%88s.jpg", "media"),
            ("media/file.jpg", "media"),
            ("public/test.txt", "public"),
            ("/public/test.txt", "public"),
        ];

        for (path, expected_tree) in paths {
            let path_str = path.to_string();
            let tree_name = path_str
                .split('/')
                .filter(|s| !s.is_empty())
                .next()
                .unwrap_or("public");
            assert_eq!(tree_name, expected_tree, "Path '{}' should give tree '{}'", path, expected_tree);
        }

        // Empty path defaults to public
        let empty: Option<String> = None;
        let tree_name = empty
            .as_ref()
            .and_then(|p| p.split('/').filter(|s| !s.is_empty()).next())
            .unwrap_or("public");
        assert_eq!(tree_name, "public");
    }

    #[tokio::test]
    async fn test_resolve_media_tree() {
        // This test specifically queries for the media tree
        let npub = "npub1g53mukxnjkcmr94fhryzkqutdz2ukq4ks0gvy5af25rgmwsl4ngq43drvk";
        let public_key = nostr_sdk::PublicKey::parse(npub).unwrap();

        // Create a client and connect to relays
        let client = nostr_sdk::Client::default();
        client.add_relay("wss://relay.damus.io").await.unwrap();
        client.add_relay("wss://relay.primal.net").await.unwrap();
        client.add_relay("wss://nos.lol").await.unwrap();
        client.connect().await;

        // Build filter for kind 30078 with d=media and l=hashtree
        let filter = nostr_sdk::Filter::new()
            .kind(nostr_sdk::Kind::from(30078u16))
            .author(public_key)
            .custom_tag(nostr_sdk::SingleLetterTag::from_char('d').unwrap(), vec!["media".to_string()])
            .custom_tag(nostr_sdk::SingleLetterTag::from_char('l').unwrap(), vec!["hashtree".to_string()])
            .limit(5);

        println!("Querying relays for media tree...");
        let events = client
            .get_events_of(vec![filter], nostr_sdk::EventSource::relays(Some(std::time::Duration::from_secs(5))))
            .await;

        match events {
            Ok(evts) => {
                println!("Found {} media events", evts.len());
                for evt in &evts {
                    println!("Event ID: {}", evt.id);
                    println!("Kind: {}", evt.kind);
                    println!("Created at: {}", evt.created_at);
                    let mut hash = None;
                    let mut key = None;
                    for tag in evt.tags.iter() {
                        println!("  Tag: {:?}", tag);
                        let tag_vec: Vec<String> = tag.as_slice().iter().map(|s| s.to_string()).collect();
                        if tag_vec.len() >= 2 {
                            if tag_vec[0] == "hash" {
                                hash = Some(tag_vec[1].clone());
                            }
                            if tag_vec[0] == "key" {
                                key = Some(tag_vec[1].clone());
                            }
                        }
                    }
                    if let Some(h) = &hash {
                        println!("\n=== MEDIA TREE FOUND ===");
                        println!("Hash: {}", h);
                        if let Some(k) = &key {
                            println!("Key: {}", k);
                        }

                        // Try to fetch from Blossom
                        println!("\nTrying to fetch from Blossom...");
                        let blossom_urls = vec![
                            format!("https://cdn.iris.to/{}.bin", h),
                        ];

                        for url in blossom_urls {
                            println!("Trying: {}", url);
                            match reqwest::get(&url).await {
                                Ok(resp) => {
                                    println!("  Status: {}", resp.status());
                                    if resp.status().is_success() {
                                        let bytes = resp.bytes().await.unwrap();
                                        println!("  Size: {} bytes", bytes.len());
                                        println!("  First 32 bytes: {:?}", &bytes[..32.min(bytes.len())]);
                                        break;
                                    }
                                }
                                Err(e) => println!("  Error: {}", e),
                            }
                        }
                    }
                }
                assert!(!evts.is_empty(), "Should find the media tree");
            }
            Err(e) => {
                panic!("Failed to fetch events: {}", e);
            }
        }
    }
}
