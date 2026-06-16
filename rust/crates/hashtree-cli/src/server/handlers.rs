use super::auth::AppState;
use super::mime::get_mime_type;
use super::nostr_query::query_events_for_local_request;
pub(super) use super::peer_status::{daemon_status, webrtc_peers};
use super::request_paths::{
    parse_api_resolve_request_path, parse_bare_npub_request_path, parse_mutable_htree_request_path,
    parse_resolve_request_path, parse_virtual_tree_root, request_virtual_tree_root,
    should_fallback_to_virtual_host_index, VirtualTreeRoot,
};
use super::ui::root_page;
use crate::socialgraph;
use axum::{
    body::Body,
    extract::{ConnectInfo, Multipart, OriginalUri, Path, Query, State},
    http::{header, HeaderMap, Method, Response, StatusCode},
    response::{IntoResponse, Json},
};
use bytes::Bytes;
use futures::future::BoxFuture;
use futures::stream::{self, FuturesUnordered, StreamExt};
use futures::FutureExt;
use hashtree_core::{
    from_hex, nhash_decode, sha256, to_hex, Cid, HashTree, HashTreeConfig, HashTreeError, LinkType,
    Store, TreeEntry,
};
use hashtree_resolver::{
    nostr::{NostrResolverConfig, NostrRootResolver},
    RootResolver,
};
use nostr::{Filter as NostrFilter, FromBech32, Kind, PublicKey};
use serde::Deserialize;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

mod content;
mod resolve;

pub(in crate::server) use content::download_blob_batch;
use content::*;
use resolve::*;

const CID_RANGE_STREAM_CHUNK_SIZE: u64 = 256 * 1024;
const NOT_FOUND_CACHE_CONTROL: &str = "no-store";
const IMMUTABLE_NOT_FOUND_CACHE_CONTROL: &str = "public, max-age=0, s-maxage=5";

fn not_found_response(body: impl Into<Body>) -> Response<Body> {
    not_found_response_with_cache_control(body, NOT_FOUND_CACHE_CONTROL)
}

fn immutable_not_found_response(body: impl Into<Body>) -> Response<Body> {
    not_found_response_with_cache_control(body, IMMUTABLE_NOT_FOUND_CACHE_CONTROL)
}

fn not_found_response_with_cache_control(
    body: impl Into<Body>,
    cache_control: &'static str,
) -> Response<Body> {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(header::CACHE_CONTROL, cache_control)
        .body(body.into())
        .unwrap()
}

pub async fn serve_root() -> impl IntoResponse {
    root_page()
}

pub async fn serve_root_or_virtual_host(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
    method: Method,
    headers: axum::http::HeaderMap,
    connect_info: ConnectInfo<std::net::SocketAddr>,
) -> impl IntoResponse {
    let Some(virtual_root) = request_virtual_tree_root(&headers) else {
        return serve_root().await.into_response();
    };

    serve_virtual_tree_host_request(
        &state,
        &virtual_root,
        None,
        params,
        headers,
        connect_info,
        method == Method::HEAD,
    )
    .await
}

pub async fn htree_test() -> impl IntoResponse {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/plain")
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .body(Body::from("ok"))
        .unwrap()
}

fn invalid_native_store_hash() -> Response<Body> {
    Response::builder()
        .status(StatusCode::BAD_REQUEST)
        .header(header::CONTENT_TYPE, "text/plain")
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .body(Body::from("invalid store hash"))
        .unwrap()
}

fn parse_native_store_hash(hash_hex: &str) -> Result<[u8; 32], Response<Body>> {
    if hash_hex.len() != 64 || !hash_hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(invalid_native_store_hash());
    }
    from_hex(hash_hex).map_err(|_| invalid_native_store_hash())
}

pub async fn iris_store_get(
    State(state): State<AppState>,
    Path(hash_hex): Path<String>,
) -> impl IntoResponse {
    let hash = match parse_native_store_hash(&hash_hex) {
        Ok(hash) => hash,
        Err(response) => return response,
    };

    match get_blob_without_blocking_runtime(&state, hash).await {
        Ok(Some(data)) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .header(header::CONTENT_LENGTH, data.len())
            .header(header::CACHE_CONTROL, "no-store")
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .body(Body::from(data))
            .unwrap(),
        Ok(None) => not_found_response("not found"),
        Err(error) => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header(header::CONTENT_TYPE, "text/plain")
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .body(Body::from(error))
            .unwrap(),
    }
}

pub async fn iris_store_head(
    State(state): State<AppState>,
    Path(hash_hex): Path<String>,
) -> impl IntoResponse {
    let hash = match parse_native_store_hash(&hash_hex) {
        Ok(hash) => hash,
        Err(response) => return response,
    };

    match get_blob_size_without_blocking_runtime(&state, hash).await {
        Ok(Some(size)) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .header(header::CONTENT_LENGTH, size)
            .header(header::CACHE_CONTROL, "no-store")
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .body(Body::empty())
            .unwrap(),
        Ok(None) => not_found_response(Body::empty()),
        Err(error) => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header(header::CONTENT_TYPE, "text/plain")
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .body(Body::from(error))
            .unwrap(),
    }
}

pub async fn iris_store_put(
    State(state): State<AppState>,
    Path(hash_hex): Path<String>,
    body: Bytes,
) -> impl IntoResponse {
    let expected_hash = match parse_native_store_hash(&hash_hex) {
        Ok(hash) => hash,
        Err(response) => return response,
    };
    let actual_hash = sha256(&body);
    if actual_hash != expected_hash {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header(header::CONTENT_TYPE, "text/plain")
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .body(Body::from("content hash mismatch"))
            .unwrap();
    }

    let existed = state.store.blob_exists(&expected_hash).unwrap_or(false);
    let store = state.store.clone();
    let blob_cache = state.blob_cache.clone();
    let data = body.to_vec();
    let size = data.len() as u64;
    match tokio::task::spawn_blocking(move || {
        let result = store.put_cached_blob(&data).map_err(|e| e.to_string());
        if let Ok(hash_hex) = &result {
            blob_cache.put_size(hash_hex.clone(), Some(size));
            blob_cache.put_body(hash_hex.clone(), &data);
        }
        result
    })
    .await
    {
        Ok(Ok(_)) => Response::builder()
            .status(if existed {
                StatusCode::OK
            } else {
                StatusCode::CREATED
            })
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .body(Body::from(format!(
                r#"{{"hash":"{}","stored":true}}"#,
                hash_hex.to_ascii_lowercase()
            )))
            .unwrap(),
        Ok(Err(error)) => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header(header::CONTENT_TYPE, "text/plain")
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .body(Body::from(error))
            .unwrap(),
        Err(error) => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header(header::CONTENT_TYPE, "text/plain")
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .body(Body::from(format!("store task failed: {error}")))
            .unwrap(),
    }
}

pub async fn iris_store_delete(
    State(state): State<AppState>,
    Path(hash_hex): Path<String>,
) -> impl IntoResponse {
    let hash = match parse_native_store_hash(&hash_hex) {
        Ok(hash) => hash,
        Err(response) => return response,
    };

    let store = state.store.clone();
    let blob_cache = state.blob_cache.clone();
    let normalized_hash_hex = hash_hex.to_ascii_lowercase();
    match tokio::task::spawn_blocking(move || store.router().delete_sync(&hash)).await {
        Ok(Ok(deleted)) => {
            if deleted {
                blob_cache.remove(&normalized_hash_hex);
            }
            Response::builder()
                .status(if deleted {
                    StatusCode::OK
                } else {
                    StatusCode::NOT_FOUND
                })
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::empty())
                .unwrap()
        }
        Ok(Err(error)) => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header(header::CONTENT_TYPE, "text/plain")
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .body(Body::from(error.to_string()))
            .unwrap(),
        Err(error) => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header(header::CONTENT_TYPE, "text/plain")
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .body(Body::from(format!("delete task failed: {error}")))
            .unwrap(),
    }
}

pub async fn p2p_signal(State(state): State<AppState>, body: Bytes) -> impl IntoResponse {
    #[cfg(feature = "p2p")]
    {
        let Some(webrtc_state) = state.webrtc_peers.as_ref() else {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        let Ok(event) = serde_json::from_slice::<nostr_sdk::nostr::Event>(&body) else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        if webrtc_state
            .submit_direct_signaling_event("webrtc-signal".to_string(), event)
            .await
        {
            StatusCode::ACCEPTED.into_response()
        } else {
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }

    #[cfg(not(feature = "p2p"))]
    {
        let _ = (state, body);
        StatusCode::NOT_FOUND.into_response()
    }
}

async fn serve_virtual_tree_host_request(
    state: &AppState,
    virtual_root: &str,
    requested_path: Option<String>,
    params: HashMap<String, String>,
    headers: axum::http::HeaderMap,
    connect_info: ConnectInfo<std::net::SocketAddr>,
    head_only: bool,
) -> Response<Body> {
    let Some(root) = parse_virtual_tree_root(virtual_root) else {
        return not_found_response("Not found");
    };

    let initial_response = match &root {
        VirtualTreeRoot::Immutable { nhash } => {
            htree_nhash_impl(
                State(state.clone()),
                nhash.clone(),
                requested_path.clone().filter(|path| !path.is_empty()),
                Query(params.clone()),
                headers.clone(),
                ConnectInfo(connect_info.0),
                head_only,
            )
            .await
        }
        VirtualTreeRoot::Mutable { npub, treename } => {
            htree_npub_impl(
                State(state.clone()),
                npub.clone(),
                treename.clone(),
                requested_path.clone().filter(|path| !path.is_empty()),
                Query(params.clone()),
                headers.clone(),
                ConnectInfo(connect_info.0),
                head_only,
            )
            .await
        }
    };

    if initial_response.status() != StatusCode::NOT_FOUND
        || !should_fallback_to_virtual_host_index(requested_path.as_deref(), &headers)
    {
        return initial_response;
    }

    match root {
        VirtualTreeRoot::Immutable { nhash } => {
            htree_nhash_impl(
                State(state.clone()),
                nhash,
                None,
                Query(params),
                headers,
                connect_info,
                head_only,
            )
            .await
        }
        VirtualTreeRoot::Mutable { npub, treename } => {
            htree_npub_impl(
                State(state.clone()),
                npub,
                treename,
                None,
                Query(params),
                headers,
                connect_info,
                head_only,
            )
            .await
        }
    }
}

pub async fn serve_virtual_host_fallback(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    Query(params): Query<HashMap<String, String>>,
    method: Method,
    headers: axum::http::HeaderMap,
    connect_info: ConnectInfo<std::net::SocketAddr>,
) -> impl IntoResponse {
    let Some(virtual_root) = request_virtual_tree_root(&headers) else {
        return not_found_response("Not found");
    };

    serve_virtual_tree_host_request(
        &state,
        &virtual_root,
        Some(uri.path().trim_start_matches('/').to_string()).filter(|path| !path.is_empty()),
        params,
        headers,
        connect_info,
        method == Method::HEAD,
    )
    .await
}

#[derive(Deserialize)]
pub struct CacheTreeRootRequest {
    #[serde(rename = "npub")]
    pub npub: String,
    #[serde(rename = "treeName")]
    pub tree_name: String,
    pub hash: String,
    pub key: Option<String>,
    pub visibility: Option<String>,
}

#[derive(Deserialize)]
pub struct ClearTreeRootCacheRequest {
    #[serde(rename = "npub")]
    pub npub: String,
    #[serde(rename = "treeName")]
    pub tree_name: String,
    pub key: Option<String>,
    pub visibility: Option<String>,
}

pub async fn cache_tree_root(
    State(state): State<AppState>,
    Json(request): Json<CacheTreeRootRequest>,
) -> impl IntoResponse {
    let hash = match from_hex(&request.hash) {
        Ok(value) => value,
        Err(error) => {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::from(format!("Invalid hash: {}", error)))
                .unwrap();
        }
    };

    let key = match request.key {
        Some(value) => match from_hex(&value) {
            Ok(decoded) => Some(decoded),
            Err(error) => {
                return Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                    .body(Body::from(format!("Invalid key: {}", error)))
                    .unwrap();
            }
        },
        None => None,
    };

    let cid = Cid { hash, key };
    let cache_key = cache_tree_root_key(
        &request.npub,
        &request.tree_name,
        request.visibility.as_deref(),
        cid.key,
    );
    put_cached_tree_root(&state, cache_key, cid, "cache", None);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .body(Body::from("ok"))
        .unwrap()
}

pub async fn clear_tree_root_cache(
    State(state): State<AppState>,
    Json(request): Json<ClearTreeRootCacheRequest>,
) -> impl IntoResponse {
    let key = match request.key {
        Some(value) => match from_hex(&value) {
            Ok(decoded) => Some(decoded),
            Err(error) => {
                return Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                    .body(Body::from(format!("Invalid key: {}", error)))
                    .unwrap();
            }
        },
        None => None,
    };

    let cache_key = cache_tree_root_key(
        &request.npub,
        &request.tree_name,
        request.visibility.as_deref(),
        key,
    );
    remove_cached_tree_root(&state, &cache_key);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .body(Body::from("ok"))
        .unwrap()
}

fn parse_nostr_pubkey(input: &str) -> Result<PublicKey, &'static str> {
    PublicKey::from_hex(input)
        .or_else(|_| PublicKey::from_bech32(input))
        .map_err(|_| "Invalid pubkey")
}

fn is_allowed_plaintext_read_author(state: &AppState, pubkey: &str) -> bool {
    if state.public_plaintext_reads {
        return true;
    }

    let Ok(author) = parse_nostr_pubkey(pubkey) else {
        return false;
    };
    let author_hex = author.to_hex();
    if state.allowed_pubkeys.contains(&author_hex) {
        return true;
    }

    state
        .social_graph
        .as_ref()
        .map(|sg| sg.check_write_access(&author_hex))
        .unwrap_or(false)
}

fn plaintext_read_forbidden_response(pubkey: &str) -> Response<Body> {
    Response::builder()
        .status(StatusCode::FORBIDDEN)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .body(Body::from(
            json!({
                "error": "Plaintext read access denied. Pubkey is not approved.",
                "pubkey": pubkey,
            })
            .to_string(),
        ))
        .unwrap()
}

pub async fn nostr_profile(
    State(state): State<AppState>,
    Path(pubkey): Path<String>,
) -> impl IntoResponse {
    let author = match parse_nostr_pubkey(&pubkey) {
        Ok(author) => author,
        Err(error) => {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::from(json!({ "error": error }).to_string()))
                .unwrap();
        }
    };

    let latest = query_events_for_local_request(
        &state,
        &NostrFilter::new()
            .author(author)
            .kind(Kind::Metadata)
            .limit(50),
        50,
    )
    .await
    .merged_events(50)
    .into_iter()
    .max_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.id.cmp(&right.id))
    });

    let Some(event) = latest else {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header(header::CACHE_CONTROL, NOT_FOUND_CACHE_CONTROL)
            .body(Body::from(
                json!({
                    "error": "Profile not found",
                    "pubkey": author.to_hex(),
                })
                .to_string(),
            ))
            .unwrap();
    };

    let profile = match serde_json::from_str::<serde_json::Value>(&event.content) {
        Ok(profile) => profile,
        Err(error) => {
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::from(
                    json!({
                        "error": format!("Invalid metadata content: {error}"),
                        "pubkey": event.pubkey.to_hex(),
                    })
                    .to_string(),
                ))
                .unwrap();
        }
    };

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .body(Body::from(
            json!({
                "pubkey": event.pubkey.to_hex(),
                "event_id": event.id.to_hex(),
                "created_at": event.created_at.as_secs(),
                "profile": profile,
            })
            .to_string(),
        ))
        .unwrap()
}

async fn list_directory_json(
    state: &AppState,
    cid: &Cid,
    is_immutable: bool,
    is_localhost: bool,
) -> Response<Body> {
    let store = state.store.store_arc();
    let tree = HashTree::new(HashTreeConfig::new(store).public());
    let entries = match list_directory_with_fetch(state, &tree, cid).await {
        Ok(Some(list)) => list,
        Ok(None) => {
            return not_found_response("Directory not found");
        }
        Err(e) => {
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::from(format!("Error: {}", e)))
                .unwrap();
        }
    };

    let payload = json!({
        "entries": entries.into_iter().map(|entry| {
            json!({
                "name": entry.name,
                "hash": to_hex(&entry.hash),
                "key": entry.key.map(|key| to_hex(&key)),
                "size": entry.size,
                "type": match entry.link_type {
                    LinkType::Blob => "blob",
                    LinkType::File => "file",
                    LinkType::Dir => "dir",
                },
            })
        }).collect::<Vec<_>>(),
    });

    build_json_response(payload, is_immutable, is_localhost)
}

/// Try to fetch a blob from WebRTC peers and upstream Blossom servers, caching locally.
/// Returns true if the blob was fetched and cached, false otherwise.
async fn htree_nhash_impl(
    State(state): State<AppState>,
    nhash: String,
    path: Option<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: axum::http::HeaderMap,
    connect_info: axum::extract::ConnectInfo<std::net::SocketAddr>,
    head_only: bool,
) -> Response<Body> {
    let is_localhost = connect_info.0.ip().is_loopback();

    let nhash_data = match nhash_decode(&nhash) {
        Ok(data) => data,
        Err(e) => {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::from(format!("Invalid nhash: {}", e)))
                .unwrap();
        }
    };

    let mut cid = Cid {
        hash: nhash_data.hash,
        key: nhash_data.decrypt_key,
    };

    if cid.key.is_none() {
        if let Some(k) = parse_hex_key(params.get("k")) {
            cid.key = Some(k);
        }
    }

    serve_tree_root_response(&state, cid, path, headers, true, is_localhost, head_only).await
}

pub async fn htree_nhash(
    State(state): State<AppState>,
    Path(nhash): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    method: Method,
    headers: axum::http::HeaderMap,
    connect_info: axum::extract::ConnectInfo<std::net::SocketAddr>,
) -> impl IntoResponse {
    let full = format!("nhash1{}", nhash);
    htree_nhash_impl(
        State(state),
        full,
        None,
        Query(params),
        headers,
        connect_info,
        method == Method::HEAD,
    )
    .await
}

pub async fn htree_nhash_path(
    State(state): State<AppState>,
    Path((nhash, path)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
    method: Method,
    headers: axum::http::HeaderMap,
    connect_info: axum::extract::ConnectInfo<std::net::SocketAddr>,
) -> impl IntoResponse {
    let full = format!("nhash1{}", nhash);
    htree_nhash_impl(
        State(state),
        full,
        Some(path),
        Query(params),
        headers,
        connect_info,
        method == Method::HEAD,
    )
    .await
}

async fn serve_tree_root_response(
    state: &AppState,
    cid: Cid,
    path: Option<String>,
    headers: axum::http::HeaderMap,
    is_immutable: bool,
    is_localhost: bool,
    head_only: bool,
) -> Response<Body> {
    let store = state.store.store_arc();
    let tree = HashTree::new(HashTreeConfig::new(store).public());
    let mut effective_path = path.filter(|value| !value.is_empty());

    if let Some(requested_path) = effective_path.clone() {
        if is_thumbnail_request(&requested_path) {
            match resolve_thumbnail_path(state, &tree, &cid, &requested_path).await {
                Ok(Some(resolved_path)) => {
                    effective_path = Some(resolved_path);
                }
                Ok(None) => {}
                Err(e) => {
                    return Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                        .body(Body::from(format!("Error: {}", e)))
                        .unwrap();
                }
            }
        }
    }

    let is_dir = match root_is_directory_with_fetch(state, &tree, &cid).await {
        Ok(value) => value,
        Err(e) => {
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::from(format!("Error: {}", e)))
                .unwrap();
        }
    };

    if is_dir {
        match resolve_directory_target(state, &tree, &cid, effective_path.clone()).await {
            Ok(Some(DirectoryTarget::File { cid: entry, path })) => {
                return serve_cid_with_range(
                    state,
                    &entry,
                    headers,
                    is_immutable,
                    is_localhost,
                    Some(&path),
                    head_only,
                )
                .await;
            }
            Ok(Some(DirectoryTarget::DirectoryListing { cid: listing_cid })) => {
                return list_directory_json(state, &listing_cid, is_immutable, is_localhost).await;
            }
            Ok(None) => {
                return not_found_response("File not found");
            }
            Err(e) => {
                return Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                    .body(Body::from(format!("Error: {}", e)))
                    .unwrap();
            }
        }
    }

    if is_immutable
        && effective_path
            .as_deref()
            .map(|requested_path| requested_path.contains('/'))
            .unwrap_or(false)
    {
        return not_found_response("Not found");
    }

    serve_cid_with_range(
        state,
        &cid,
        headers,
        is_immutable,
        is_localhost,
        effective_path.as_deref(),
        head_only,
    )
    .await
}

async fn htree_npub_impl(
    State(state): State<AppState>,
    npub: String,
    treename: String,
    path: Option<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: axum::http::HeaderMap,
    connect_info: axum::extract::ConnectInfo<std::net::SocketAddr>,
    head_only: bool,
) -> Response<Body> {
    let is_localhost = connect_info.0.ip().is_loopback();
    let key = format!("{}/{}", npub, treename);
    if !is_allowed_plaintext_read_author(&state, &npub) {
        return plaintext_read_forbidden_response(&npub);
    }

    let link_key = parse_hex_key(params.get("k"));
    let resolved = if let Some(resolved) =
        resolve_root_for_mutable_request(&state, &npub, &treename, link_key).await
    {
        resolved.cid
    } else {
        let resolver = match NostrRootResolver::new(resolver_config(&state)).await {
            Ok(r) => r,
            Err(e) => {
                return Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                    .body(Body::from(format!("Failed to create resolver: {}", e)))
                    .unwrap();
            }
        };

        let cid = match tokio::time::timeout(
            HTTP_RESOLVER_TIMEOUT,
            resolve_npub_root(&key, &resolver, link_key),
        )
        .await
        {
            Ok(Ok(cid)) => cid,
            Ok(Err(e)) => {
                let _ = resolver.stop().await;
                return Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                    .body(Body::from(format!("Resolution failed: {}", e)))
                    .unwrap();
            }
            Err(_) => {
                let _ = resolver.stop().await;
                return Response::builder()
                    .status(StatusCode::GATEWAY_TIMEOUT)
                    .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                    .body(Body::from("Resolution timeout"))
                    .unwrap();
            }
        };
        let _ = resolver.stop().await;
        put_cached_tree_root(
            &state,
            tree_root_cache_key(&npub, &treename, link_key),
            cid.clone(),
            "nostr",
            None,
        );
        cid
    };

    let mut cid = resolved;
    if cid.key.is_none() {
        if let Some(k) = link_key {
            cid.key = Some(k);
        }
    }

    serve_tree_root_response(&state, cid, path, headers, false, is_localhost, head_only).await
}

pub async fn htree_npub(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    Path((npub, treename)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
    method: Method,
    headers: axum::http::HeaderMap,
    connect_info: axum::extract::ConnectInfo<std::net::SocketAddr>,
) -> impl IntoResponse {
    let parsed = parse_mutable_htree_request_path(uri.path());
    let full = parsed
        .as_ref()
        .map(|entry| entry.npub.clone())
        .unwrap_or_else(|| format!("npub1{}", npub));
    let resolved_treename = parsed
        .as_ref()
        .map(|entry| entry.treename.clone())
        .unwrap_or(treename);
    htree_npub_impl(
        State(state),
        full,
        resolved_treename,
        None,
        Query(params),
        headers,
        connect_info,
        method == Method::HEAD,
    )
    .await
}

pub async fn htree_npub_path(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    Path((npub, treename, path)): Path<(String, String, String)>,
    Query(params): Query<HashMap<String, String>>,
    method: Method,
    headers: axum::http::HeaderMap,
    connect_info: axum::extract::ConnectInfo<std::net::SocketAddr>,
) -> impl IntoResponse {
    let parsed = parse_mutable_htree_request_path(uri.path());
    let full = parsed
        .as_ref()
        .map(|entry| entry.npub.clone())
        .unwrap_or_else(|| format!("npub1{}", npub));
    let resolved_treename = parsed
        .as_ref()
        .map(|entry| entry.treename.clone())
        .unwrap_or(treename);
    let resolved_path = parsed.and_then(|entry| entry.path).or(Some(path));
    htree_npub_impl(
        State(state),
        full,
        resolved_treename,
        resolved_path,
        Query(params),
        headers,
        connect_info,
        method == Method::HEAD,
    )
    .await
}

/// Cache-Control header for immutable content-addressed data (1 year)
const IMMUTABLE_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";
const CORP_CROSS_ORIGIN: &str = "cross-origin";
const CROSS_ORIGIN_RESOURCE_POLICY_HEADER: &str = "cross-origin-resource-policy";
const DEFAULT_CDN_HOST: &str = "cdn.iris.to";

fn parse_hex_key(value: Option<&String>) -> Option<[u8; 32]> {
    let hex = value?;
    if hex.len() != 64 {
        return None;
    }
    from_hex(hex).ok()
}

fn content_type_for_path(path: Option<&str>) -> &'static str {
    let filename = path.and_then(|p| p.rsplit('/').next()).unwrap_or("");
    if filename.is_empty() {
        return "application/octet-stream";
    }
    get_mime_type(filename)
}

fn request_host(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.split('/').next().unwrap_or(value))
        .map(|value| {
            value
                .rsplit_once(':')
                .map(|(host, _)| host)
                .unwrap_or(value)
        })
}

fn should_redirect_extensionless_cdn_blob(headers: &HeaderMap, ext: Option<&str>) -> bool {
    ext.is_none()
        && request_host(headers)
            .map(|host| host.eq_ignore_ascii_case(DEFAULT_CDN_HOST))
            .unwrap_or(false)
}

fn extensionful_blob_redirect(hash_hex: &str) -> Response<Body> {
    Response::builder()
        .status(StatusCode::PERMANENT_REDIRECT)
        .header(header::LOCATION, format!("/{hash_hex}.bin"))
        .header(header::CACHE_CONTROL, IMMUTABLE_CACHE_CONTROL)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(CROSS_ORIGIN_RESOURCE_POLICY_HEADER, CORP_CROSS_ORIGIN)
        .body(Body::empty())
        .unwrap()
}

fn build_json_response(
    payload: serde_json::Value,
    is_immutable: bool,
    is_localhost: bool,
) -> Response<Body> {
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(CROSS_ORIGIN_RESOURCE_POLICY_HEADER, CORP_CROSS_ORIGIN);
    if is_immutable {
        builder = builder.header(header::CACHE_CONTROL, IMMUTABLE_CACHE_CONTROL);
    }
    if is_localhost {
        builder = builder.header("X-Source", "local");
    }
    builder.body(Body::from(payload.to_string())).unwrap()
}

enum ParsedByteRange {
    Satisfiable { start: u64, end_inclusive: u64 },
    Unsatisfiable,
}

fn parse_byte_range(range_header: &str, total_size: u64) -> Option<ParsedByteRange> {
    let bytes_range = range_header.strip_prefix("bytes=")?;
    if bytes_range.contains(',') {
        return None;
    }

    let (start_part, end_part) = bytes_range.split_once('-')?;
    if total_size == 0 {
        return Some(ParsedByteRange::Unsatisfiable);
    }

    if start_part.is_empty() {
        let suffix_len = end_part.parse::<u64>().ok()?;
        if suffix_len == 0 {
            return Some(ParsedByteRange::Unsatisfiable);
        }
        let clamped_suffix_len = suffix_len.min(total_size);
        return Some(ParsedByteRange::Satisfiable {
            start: total_size - clamped_suffix_len,
            end_inclusive: total_size - 1,
        });
    }

    let start = start_part.parse::<u64>().ok()?;
    if start >= total_size {
        return Some(ParsedByteRange::Unsatisfiable);
    }

    let end_inclusive = if end_part.is_empty() {
        total_size - 1
    } else {
        let parsed_end = end_part.parse::<u64>().ok()?;
        if parsed_end < start {
            return Some(ParsedByteRange::Unsatisfiable);
        }
        parsed_end.min(total_size - 1)
    };

    Some(ParsedByteRange::Satisfiable {
        start,
        end_inclusive,
    })
}

async fn serve_cid_with_range(
    state: &AppState,
    cid: &Cid,
    headers: axum::http::HeaderMap,
    is_immutable: bool,
    is_localhost: bool,
    filename_hint: Option<&str>,
    head_only: bool,
) -> Response<Body> {
    let store = state.store.store_arc();
    let tree = HashTree::new(HashTreeConfig::new(store).public());
    let content_type = content_type_for_path(filename_hint);
    let total_size = match get_size_cid_with_fetch(state, &tree, cid).await {
        Ok(Some(size)) => size,
        Ok(None) => {
            return not_found_response("Not found");
        }
        Err(e) => {
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::from(format!("Error: {}", e)))
                .unwrap();
        }
    };

    let range_header = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    if let Some(range_str) = range_header {
        if let Some(parsed_range) = parse_byte_range(range_str, total_size) {
            let (start, end_inclusive) = match parsed_range {
                ParsedByteRange::Satisfiable {
                    start,
                    end_inclusive,
                } => (start, end_inclusive),
                ParsedByteRange::Unsatisfiable => {
                    return Response::builder()
                        .status(StatusCode::RANGE_NOT_SATISFIABLE)
                        .header(header::CONTENT_TYPE, "text/plain")
                        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                        .body(Body::from("Range not satisfiable"))
                        .unwrap();
                }
            };

            let end_exclusive = end_inclusive.saturating_add(1);
            let content_length = end_exclusive.saturating_sub(start) as usize;
            let content_range = format!("bytes {}-{}/{}", start, end_inclusive, total_size);
            let body = if head_only {
                Body::empty()
            } else {
                Body::from_stream(stream_file_range_cid_with_fetch(
                    state.clone(),
                    cid.clone(),
                    start,
                    end_inclusive,
                ))
            };

            let mut builder = Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_TYPE, content_type)
                .header(header::CONTENT_LENGTH, content_length)
                .header(header::CONTENT_RANGE, content_range)
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .header(CROSS_ORIGIN_RESOURCE_POLICY_HEADER, CORP_CROSS_ORIGIN);
            if is_immutable {
                builder = builder.header(header::CACHE_CONTROL, IMMUTABLE_CACHE_CONTROL);
            }
            if is_localhost {
                builder = builder.header("X-Source", "local");
            }
            return builder.body(body).unwrap();
        }
    }

    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, total_size)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(CROSS_ORIGIN_RESOURCE_POLICY_HEADER, CORP_CROSS_ORIGIN);
    if is_immutable {
        builder = builder.header(header::CACHE_CONTROL, IMMUTABLE_CACHE_CONTROL);
    }
    if is_localhost {
        builder = builder.header("X-Source", "local");
    }

    let body = if head_only || total_size == 0 {
        Body::empty()
    } else {
        Body::from_stream(stream_file_range_cid_with_fetch(
            state.clone(),
            cid.clone(),
            0,
            total_size.saturating_sub(1),
        ))
    };
    builder.body(body).unwrap()
}

/// Internal content serving (shared by CID and blossom routes)
///
/// `is_immutable`: if true, adds Cache-Control: immutable header.
/// Use true for content-addressed routes (hash, nhash, blossom SHA256).
/// Use false for mutable routes (npub/ref_name) where the reference can change.
/// `is_localhost`: if true, adds X-Source header for debugging.
async fn serve_content_internal(
    state: &AppState,
    hash: &[u8; 32],
    headers: axum::http::HeaderMap,
    is_immutable: bool,
    is_localhost: bool,
) -> Response<Body> {
    let store = &state.store;

    // Always return raw bytes - no conversion to JSON/HTML
    // This is required for Blossom protocol compatibility

    // Try as file
    // Check for Range header
    let range_header = headers.get(header::RANGE).and_then(|v| v.to_str().ok());

    if let Some(range_str) = range_header {
        // Content type - hashtree doesn't store filenames, so default to octet-stream
        let content_type = "application/octet-stream";

        match store.get_file_chunk_metadata(hash) {
            Ok(Some(metadata)) => {
                let total_size = metadata.total_size;
                if let Some(parsed_range) = parse_byte_range(range_str, total_size) {
                    let (start, end_actual) = match parsed_range {
                        ParsedByteRange::Satisfiable {
                            start,
                            end_inclusive,
                        } => (start, end_inclusive),
                        ParsedByteRange::Unsatisfiable => {
                            return Response::builder()
                                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                                .header(header::CONTENT_TYPE, "text/plain")
                                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                                .body(Body::from("Range not satisfiable"))
                                .unwrap()
                                .into_response();
                        }
                    };

                    let content_length = end_actual - start + 1;
                    let content_range = format!("bytes {}-{}/{}", start, end_actual, total_size);

                    if metadata.is_chunked {
                        match state
                            .store
                            .clone()
                            .stream_file_range_chunks_owned(hash, start, end_actual)
                        {
                            Ok(Some(chunks_iter)) => {
                                let stream =
                                    stream::iter(chunks_iter).map(|result| result.map(Bytes::from));

                                let mut builder = Response::builder()
                                    .status(StatusCode::PARTIAL_CONTENT)
                                    .header(header::CONTENT_TYPE, content_type)
                                    .header(header::CONTENT_LENGTH, content_length)
                                    .header(header::CONTENT_RANGE, content_range)
                                    .header(header::ACCEPT_RANGES, "bytes")
                                    .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*");
                                if is_immutable {
                                    builder = builder
                                        .header(header::CACHE_CONTROL, IMMUTABLE_CACHE_CONTROL);
                                }
                                if is_localhost {
                                    builder = builder.header("X-Source", "local");
                                }
                                return builder
                                    .body(Body::from_stream(stream))
                                    .unwrap()
                                    .into_response();
                            }
                            Ok(None) => {
                                let response = if is_immutable {
                                    immutable_not_found_response("File not found")
                                } else {
                                    not_found_response("File not found")
                                };
                                return response.into_response();
                            }
                            Err(e) => {
                                return Response::builder()
                                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                                    .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                                    .body(Body::from(format!("Error: {}", e)))
                                    .unwrap()
                                    .into_response();
                            }
                        }
                    } else {
                        match store.get_file_range(hash, start, Some(end_actual)) {
                            Ok(Some((range_content, _))) => {
                                let mut builder = Response::builder()
                                    .status(StatusCode::PARTIAL_CONTENT)
                                    .header(header::CONTENT_TYPE, content_type)
                                    .header(header::CONTENT_LENGTH, range_content.len())
                                    .header(header::CONTENT_RANGE, content_range)
                                    .header(header::ACCEPT_RANGES, "bytes")
                                    .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*");
                                if is_immutable {
                                    builder = builder
                                        .header(header::CACHE_CONTROL, IMMUTABLE_CACHE_CONTROL);
                                }
                                if is_localhost {
                                    builder = builder.header("X-Source", "local");
                                }
                                return builder
                                    .body(Body::from(range_content))
                                    .unwrap()
                                    .into_response();
                            }
                            Ok(None) => {
                                let response = if is_immutable {
                                    immutable_not_found_response("File not found")
                                } else {
                                    not_found_response("File not found")
                                };
                                return response.into_response();
                            }
                            Err(e) => {
                                return Response::builder()
                                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                                    .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                                    .body(Body::from(format!("Error: {}", e)))
                                    .unwrap()
                                    .into_response();
                            }
                        }
                    }
                }
            }
            Ok(None) => {
                let response = if is_immutable {
                    immutable_not_found_response("File not found")
                } else {
                    not_found_response("File not found")
                };
                return response.into_response();
            }
            Err(e) => {
                return Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                    .body(Body::from(format!("Error: {}", e)))
                    .unwrap()
                    .into_response();
            }
        }
    }

    // Fall back to full file
    match store.get_file(hash) {
        Ok(Some(content)) => {
            // Content type - hashtree doesn't store filenames, so default to octet-stream
            let content_type = "application/octet-stream";

            let mut builder = Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, content_type)
                .header(header::CONTENT_LENGTH, content.len())
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*");
            if is_immutable {
                builder = builder.header(header::CACHE_CONTROL, IMMUTABLE_CACHE_CONTROL);
            }
            if is_localhost {
                builder = builder.header("X-Source", "local");
            }
            builder.body(Body::from(content)).unwrap().into_response()
        }
        Ok(None) => {
            let response = if is_immutable {
                immutable_not_found_response("Not found")
            } else {
                not_found_response("Not found")
            };
            response.into_response()
        }
        Err(e) => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .body(Body::from(format!("Error: {}", e)))
            .unwrap()
            .into_response(),
    }
}

/// Serve content by CID or blossom SHA256 hash
/// Tries CID first, then falls back to blossom lookup if input looks like SHA256
/// If not found locally, queries connected WebSocket/WebRTC peers
pub async fn serve_content_or_blob(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    method: Method,
    headers: axum::http::HeaderMap,
    connect_info: axum::extract::ConnectInfo<std::net::SocketAddr>,
) -> impl IntoResponse {
    if let Some(virtual_root) = request_virtual_tree_root(&headers) {
        return serve_virtual_tree_host_request(
            &state,
            &virtual_root,
            Some(id.clone()),
            params,
            headers,
            connect_info,
            method == Method::HEAD,
        )
        .await
        .into_response();
    }

    let is_localhost = connect_info.0.ip().is_loopback();
    let _client_ip = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| connect_info.0.ip().to_string());
    // Parse potential extension for blossom
    let (hash_part, ext) = if let Some(dot_pos) = id.rfind('.') {
        (&id[..dot_pos], Some(&id[dot_pos..]))
    } else {
        (id.as_str(), None)
    };

    // Check if it looks like a SHA256 hash (64 hex chars)
    let is_sha256 = hash_part.len() == 64 && hash_part.chars().all(|c| c.is_ascii_hexdigit());

    // Try raw blob lookup first (for hashtree chunks / git objects)
    // This takes priority over file tree serving to avoid returning reassembled
    // file content when the caller expects raw chunk data
    if is_sha256 && state.hash_get_enabled {
        let hash_hex = hash_part.to_lowercase();
        if should_redirect_extensionless_cdn_blob(&headers, ext) {
            return extensionful_blob_redirect(&hash_hex).into_response();
        }
        if let Ok(hash_bytes) = from_hex(&hash_hex) {
            if let Some(range_header) = headers.get(header::RANGE).and_then(|v| v.to_str().ok()) {
                match get_blob_size_without_blocking_runtime(&state, hash_bytes).await {
                    Ok(Some(total_size)) => match parse_byte_range(range_header, total_size) {
                        Some(ParsedByteRange::Satisfiable {
                            start,
                            end_inclusive,
                        }) => {
                            match get_blob_range_without_blocking_runtime(
                                &state,
                                hash_bytes,
                                start,
                                end_inclusive,
                            )
                            .await
                            {
                                Ok(Some(data)) => {
                                    let content_range =
                                        format!("bytes {}-{}/{}", start, end_inclusive, total_size);
                                    let mut builder = Response::builder()
                                        .status(StatusCode::PARTIAL_CONTENT)
                                        .header(header::CONTENT_TYPE, "application/octet-stream")
                                        .header(header::CONTENT_LENGTH, data.len())
                                        .header(header::CONTENT_RANGE, content_range)
                                        .header(header::ACCEPT_RANGES, "bytes")
                                        .header(header::CACHE_CONTROL, IMMUTABLE_CACHE_CONTROL)
                                        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                                        .header(
                                            CROSS_ORIGIN_RESOURCE_POLICY_HEADER,
                                            CORP_CROSS_ORIGIN,
                                        );
                                    if is_localhost {
                                        builder = builder.header("X-Source", "local");
                                    }
                                    return builder.body(Body::from(data)).unwrap().into_response();
                                }
                                Ok(None) => {}
                                Err(error) if error == blob_read_busy_error() => {
                                    return Response::builder()
                                        .status(StatusCode::SERVICE_UNAVAILABLE)
                                        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                                        .header(header::CACHE_CONTROL, NOT_FOUND_CACHE_CONTROL)
                                        .header("Retry-After", "1")
                                        .body(Body::from("Blob read queue is full"))
                                        .unwrap()
                                        .into_response();
                                }
                                Err(error) => {
                                    tracing::warn!(
                                        "Failed to read local blob range {}: {}",
                                        hash_hex,
                                        error
                                    );
                                    return Response::builder()
                                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                                        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                                        .body(Body::from("Blob range read failed"))
                                        .unwrap()
                                        .into_response();
                                }
                            }
                        }
                        Some(ParsedByteRange::Unsatisfiable) => {
                            return Response::builder()
                                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                                .header(header::CONTENT_TYPE, "text/plain")
                                .header(header::CONTENT_RANGE, format!("bytes */{}", total_size))
                                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                                .body(Body::from("Range not satisfiable"))
                                .unwrap()
                                .into_response();
                        }
                        None => {}
                    },
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!("Failed to read local blob size {}: {}", hash_hex, error);
                    }
                }
            }

            match get_blob_without_blocking_runtime(&state, hash_bytes).await {
                Ok(Some(data)) => {
                    return build_blob_response(data, BlobSource::Local, is_localhost)
                        .into_response();
                }
                Ok(None) => {}
                Err(error) if error == blob_read_busy_error() => {
                    return Response::builder()
                        .status(StatusCode::SERVICE_UNAVAILABLE)
                        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                        .header(header::CACHE_CONTROL, NOT_FOUND_CACHE_CONTROL)
                        .header("Retry-After", "1")
                        .body(Body::from("Blob read queue is full"))
                        .unwrap()
                        .into_response();
                }
                Err(error) => {
                    tracing::warn!("Failed to read local blob {}: {}", hash_hex, error);
                }
            }
        }
    }

    // Try file tree lookup (serves reassembled file content)
    // (hashtree hashes are 64 hex chars, same as blossom SHA256)
    if let Ok(hash) = from_hex(&id) {
        if state
            .store
            .get_file_chunk_metadata(&hash)
            .ok()
            .flatten()
            .is_some()
        {
            return serve_content_internal(&state, &hash, headers, true, is_localhost).await;
        }
    }

    // Not found locally - try the shared daemon fetch path. This includes
    // FIPS, legacy mesh peers, and configured Blossom fallback sources.
    if is_sha256 && state.hash_get_enabled {
        let hash_hex = hash_part.to_lowercase();
        if let Ok(hash_bytes) = from_hex(&hash_hex) {
            if let Some(source) = fetch_and_cache_blob_with_source(&state, &hash_bytes).await {
                match get_blob_without_blocking_runtime(&state, hash_bytes).await {
                    Ok(Some(data)) => {
                        return build_blob_response(data, source, is_localhost).into_response();
                    }
                    Ok(None) => {}
                    Err(error) if error == blob_read_busy_error() => {
                        return Response::builder()
                            .status(StatusCode::SERVICE_UNAVAILABLE)
                            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                            .header(header::CACHE_CONTROL, NOT_FOUND_CACHE_CONTROL)
                            .header("Retry-After", "1")
                            .body(Body::from("Blob read queue is full"))
                            .unwrap()
                            .into_response();
                    }
                    Err(error) => {
                        tracing::warn!("Failed to read fetched blob {}: {}", hash_hex, error);
                        return Response::builder()
                            .status(StatusCode::INTERNAL_SERVER_ERROR)
                            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                            .body(Body::from("Blob read failed"))
                            .unwrap()
                            .into_response();
                    }
                }
            }
        }
    }

    // Not found anywhere
    if is_sha256 {
        immutable_not_found_response("Not found").into_response()
    } else {
        not_found_response("Not found").into_response()
    }
}

/// Serve content by npub/ref_name (Nostr resolver)
/// Route: /npub1... (the "npub1" prefix is matched by the route, :rest captures pubkey remainder + /ref)
pub async fn serve_npub(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    Path(params): Path<HashMap<String, String>>,
    Query(query): Query<HashMap<String, String>>,
    method: Method,
    headers: axum::http::HeaderMap,
    connect_info: axum::extract::ConnectInfo<std::net::SocketAddr>,
) -> impl IntoResponse {
    // Reconstruct full key: "npub1" + rest (e.g., "abc.../mydata")
    let parsed = parse_bare_npub_request_path(uri.path());
    let (npub, treename, path) = if let Some(entry) = parsed {
        (entry.npub, entry.treename, entry.path)
    } else {
        let rest = params.get("rest").cloned().unwrap_or_default();
        let key = format!("npub1{}", rest);
        let Some((npub, treename)) = key.split_once('/') else {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::from("Missing ref name: use /npub1.../ref_name"))
                .unwrap()
                .into_response();
        };
        (npub.to_string(), treename.to_string(), None)
    };

    // Validate format: must have a / for ref name
    if treename.is_empty() {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .body(Body::from("Missing ref name: use /npub1.../ref_name"))
            .unwrap()
            .into_response();
    }

    htree_npub_impl(
        State(state),
        npub,
        treename,
        path,
        Query(query),
        headers,
        connect_info,
        method == Method::HEAD,
    )
    .await
    .into_response()
}

pub async fn upload_file(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let store = &state.store;
    let mut temp_file_path: Option<std::path::PathBuf> = None;
    let mut file_name_final: Option<String> = None;
    let temp_dir = tempfile::tempdir().unwrap();

    while let Some(mut field) = multipart.next_field().await.unwrap_or(None) {
        let name = field.name().unwrap_or("").to_string();

        if name == "file" {
            let file_name = field.file_name().unwrap_or("upload").to_string();
            let temp_file = temp_dir.path().join(&file_name);

            // Stream directly to disk instead of loading into memory
            let mut file = tokio::fs::File::create(&temp_file).await.unwrap();

            while let Some(chunk) = field.next().await {
                if let Ok(data) = chunk {
                    file.write_all(&data).await.unwrap();
                }
            }

            file.flush().await.unwrap();
            temp_file_path = Some(temp_file);
            file_name_final = Some(file_name);
            break;
        }
    }

    let (temp_file, file_name) = match (temp_file_path, file_name_final) {
        (Some(path), Some(name)) => (path, name),
        _ => {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from("No file provided"))
                .unwrap();
        }
    };

    // Use streaming upload for files > 10MB
    let file_size = std::fs::metadata(&temp_file)
        .ok()
        .map(|m| m.len())
        .unwrap_or(0);
    let use_streaming = file_size > 10 * 1024 * 1024;

    let cid_result = if use_streaming {
        // Streaming upload with progress callbacks
        let file = std::fs::File::open(&temp_file).unwrap();
        store.upload_file_stream(file, file_name, |_intermediate_cid| {
            // Could log progress here or publish to websocket
        })
    } else {
        // Regular upload for small files
        store.upload_file(&temp_file)
    };

    // Upload and get CID
    match cid_result {
        Ok(cid) => {
            let json = json!({
                "success": true,
                "cid": cid
            });
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(json.to_string()))
                .unwrap()
        }
        Err(e) => {
            let json = json!({
                "success": false,
                "error": e.to_string()
            });
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(json.to_string()))
                .unwrap()
        }
    }
}

pub async fn list_pins(State(state): State<AppState>) -> impl IntoResponse {
    let store = &state.store;
    match store.list_pins_with_names() {
        Ok(pins) => Json(json!({
            "pins": pins.iter().map(|p| json!({
                "cid": p.cid,
                "name": p.name,
                "is_directory": p.is_directory,
                "size_bytes": p.size_bytes
            })).collect::<Vec<_>>()
        })),
        Err(e) => Json(json!({
            "error": e.to_string()
        })),
    }
}

pub async fn pin_cid(State(state): State<AppState>, Path(cid): Path<String>) -> impl IntoResponse {
    let hash = match from_hex(&cid) {
        Ok(h) => h,
        Err(e) => {
            return Json(json!({
                "success": false,
                "error": format!("Invalid CID format: {}", e)
            }));
        }
    };
    let store = &state.store;
    match store.pin(&hash) {
        Ok(_) => Json(json!({
            "success": true,
            "cid": cid
        })),
        Err(e) => Json(json!({
            "success": false,
            "error": e.to_string()
        })),
    }
}

pub async fn unpin_cid(
    State(state): State<AppState>,
    Path(cid): Path<String>,
) -> impl IntoResponse {
    let hash = match from_hex(&cid) {
        Ok(h) => h,
        Err(e) => {
            return Json(json!({
                "success": false,
                "error": format!("Invalid CID format: {}", e)
            }));
        }
    };
    let store = &state.store;
    match store.unpin(&hash) {
        Ok(_) => Json(json!({
            "success": true,
            "cid": cid
        })),
        Err(e) => Json(json!({
            "success": false,
            "error": e.to_string()
        })),
    }
}

pub async fn storage_stats(State(state): State<AppState>) -> impl IntoResponse {
    let store = &state.store;
    match store.get_storage_stats() {
        Ok(stats) => Json(json!({
            "total_dags": stats.total_dags,
            "pinned_dags": stats.pinned_dags,
            "total_bytes": stats.total_bytes,
        })),
        Err(e) => Json(json!({
            "error": e.to_string()
        })),
    }
}

/// Health check endpoint - minimal overhead, just returns ok
pub async fn health_check() -> impl IntoResponse {
    // Minimal health check - if we can respond, we're alive
    // Storage checks would be heavier and DDoS-able
    Response::builder()
        .status(StatusCode::OK)
        .body(Body::from("ok"))
        .unwrap()
}

pub async fn garbage_collect(State(state): State<AppState>) -> impl IntoResponse {
    let store = &state.store;
    match store.gc() {
        Ok(gc_stats) => Json(json!({
            "deleted_dags": gc_stats.deleted_dags,
            "freed_bytes": gc_stats.freed_bytes
        })),
        Err(e) => Json(json!({
            "error": e.to_string()
        })),
    }
}

pub async fn socialgraph_stats(State(state): State<AppState>) -> impl IntoResponse {
    match &state.social_graph {
        Some(sg) => {
            let stats = sg.stats();
            Json(json!(stats))
        }
        None => Json(json!({
            "enabled": false,
            "message": "Social graph not active"
        })),
    }
}

#[derive(Debug, Deserialize)]
pub struct SocialGraphSnapshotQuery {
    #[serde(rename = "maxNodes")]
    pub max_nodes: Option<usize>,
    #[serde(rename = "maxEdges")]
    pub max_edges: Option<usize>,
    #[serde(rename = "maxDistance")]
    pub max_distance: Option<u32>,
    #[serde(rename = "maxEdgesPerNode")]
    pub max_edges_per_node: Option<usize>,
}

pub async fn socialgraph_snapshot(
    State(state): State<AppState>,
    Query(params): Query<SocialGraphSnapshotQuery>,
    connect_info: axum::extract::ConnectInfo<std::net::SocketAddr>,
) -> impl IntoResponse {
    let ip = connect_info.0.ip();
    if !state.socialgraph_snapshot_public && !ip.is_loopback() {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "localhost only"})),
        )
            .into_response();
    }

    let add_public_cors = |builder: axum::http::response::Builder| {
        if state.socialgraph_snapshot_public {
            builder.header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        } else {
            builder
        }
    };

    let social_graph_store = match &state.social_graph_store {
        Some(store) => Arc::clone(store),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "social graph not initialized"})),
            )
                .into_response();
        }
    };
    let root = match state.social_graph_root {
        Some(root) => root,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "social graph root missing"})),
            )
                .into_response();
        }
    };

    let options = socialgraph::snapshot::SnapshotOptions {
        max_nodes: params.max_nodes,
        max_edges: params.max_edges,
        max_distance: params.max_distance,
        max_edges_per_node: params.max_edges_per_node,
    };

    let chunks = match tokio::task::spawn_blocking(move || {
        socialgraph::snapshot::build_snapshot_chunks(social_graph_store.as_ref(), &root, &options)
    })
    .await
    {
        Ok(Ok(chunks)) => chunks,
        Ok(Err(err)) => {
            return add_public_cors(Response::builder())
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from(format!("Error generating snapshot: {err}")))
                .unwrap();
        }
        Err(err) => {
            return add_public_cors(Response::builder())
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from(format!("Error generating snapshot: {err}")))
                .unwrap();
        }
    };

    let stream = stream::iter(chunks.into_iter().map(Ok::<Bytes, std::io::Error>));
    add_public_cors(Response::builder())
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(
            header::CONTENT_DISPOSITION,
            "attachment; filename=\"social-graph.bin\"",
        )
        .header(
            header::CACHE_CONTROL,
            "public, max-age=60, stale-while-revalidate=60",
        )
        .body(Body::from_stream(stream))
        .unwrap()
}

pub async fn follow_distance(
    State(state): State<AppState>,
    Path(pubkey): Path<String>,
) -> impl IntoResponse {
    if pubkey.len() != 64 || !pubkey.chars().all(|c| c.is_ascii_hexdigit()) {
        return Json(json!({
            "error": "Invalid pubkey format (expected 64 hex chars)"
        }));
    }

    match &state.social_graph {
        Some(sg) => {
            let allowed = sg.check_write_access(&pubkey);
            Json(json!({
                "pubkey": pubkey,
                "write_access": allowed,
            }))
        }
        None => Json(json!({
            "pubkey": pubkey,
            "error": "Social graph not active",
        })),
    }
}

/// Timeout for HTTP resolver requests
const HTTP_RESOLVER_TIMEOUT: Duration = Duration::from_secs(10);

/// Create resolver config with HTTP timeout
fn resolver_config(state: &AppState) -> NostrResolverConfig {
    let mut config = NostrResolverConfig {
        resolve_timeout: HTTP_RESOLVER_TIMEOUT,
        ..Default::default()
    };
    if !state.nostr_relay_urls.is_empty() {
        config.relays = state.nostr_relay_urls.clone();
    }
    config
}

/// Resolve npub/treename to hash and serve content
/// Route: /n/:pubkey/:treename or /n/:pubkey/:treename/*path
pub async fn resolve_and_serve(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    Path(params): Path<(String, String)>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let (fallback_pubkey, fallback_treename) = params;
    let parsed = parse_resolve_request_path(uri.path());
    let pubkey = parsed
        .as_ref()
        .map(|entry| entry.pubkey.clone())
        .unwrap_or(fallback_pubkey);
    let treename = parsed
        .as_ref()
        .map(|entry| entry.treename.clone())
        .unwrap_or(fallback_treename);
    let key = format!("{}/{}", pubkey, treename);
    if !is_allowed_plaintext_read_author(&state, &pubkey) {
        return plaintext_read_forbidden_response(&pubkey).into_response();
    }

    if let Some(resolved) = resolve_root_for_mutable_request(&state, &pubkey, &treename, None).await
    {
        return serve_content_internal(&state, &resolved.cid.hash, headers, false, false).await;
    }

    let resolver = match NostrRootResolver::new(resolver_config(&state)).await {
        Ok(r) => r,
        Err(e) => {
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::from(
                    json!({
                        "error": format!("Failed to create resolver: {}", e),
                        "key": key
                    })
                    .to_string(),
                ))
                .unwrap()
                .into_response();
        }
    };

    // Use resolve_wait with timeout - waits for key to appear
    // This is a mutable route (npub/treename can change over time)
    match tokio::time::timeout(HTTP_RESOLVER_TIMEOUT, resolver.resolve_wait(&key)).await {
        Ok(Ok(cid)) => {
            cache_public_tree_root(&state, &pubkey, &treename, &cid);
            let _ = resolver.stop().await;
            serve_content_internal(&state, &cid.hash, headers, false, false).await
        }
        Ok(Err(e)) => {
            let _ = resolver.stop().await;
            Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::from(
                    json!({
                        "error": e.to_string(),
                        "key": key
                    })
                    .to_string(),
                ))
                .unwrap()
                .into_response()
        }
        Err(_) => {
            let _ = resolver.stop().await;
            Response::builder()
                .status(StatusCode::GATEWAY_TIMEOUT)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::from(
                    json!({
                        "error": "Resolution timeout",
                        "key": key
                    })
                    .to_string(),
                ))
                .unwrap()
                .into_response()
        }
    }
}

/// API endpoint to resolve npub/treename to hash (returns JSON)
/// Tries relays first, then WebRTC peers if available.
pub async fn resolve_to_hash(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    Path(params): Path<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let (fallback_pubkey, fallback_treename) = params;
    let parsed = parse_api_resolve_request_path(uri.path());
    let pubkey = parsed
        .as_ref()
        .map(|entry| entry.pubkey.clone())
        .unwrap_or(fallback_pubkey);
    let treename = parsed
        .as_ref()
        .map(|entry| entry.treename.clone())
        .unwrap_or(fallback_treename);
    let key = format!("{}/{}", pubkey, treename);
    let refresh = query_flag(&query, "refresh")
        || query_flag(&query, "force")
        || query_flag(&query, "skipCache");

    if !refresh {
        if let Some(resolved) = resolve_root_offline(&state, &pubkey, &treename, None).await {
            let mut payload = json!({
                "key": key,
                "hash": to_hex(&resolved.cid.hash),
                "cid": resolved.cid.to_string(),
                "source": resolved.source,
            });
            if let Some(root) = resolved.root_event {
                payload["peer"] = json!(root.peer_id);
                payload["event_id"] = json!(root.event_id);
                payload["created_at"] = json!(root.created_at);
                payload["key_tag"] = json!(root.key);
                payload["encryptedKey"] = json!(root.encrypted_key);
                payload["selfEncryptedKey"] = json!(root.self_encrypted_key);
            }
            return Json(payload);
        }
    }

    if let Some(resolved) = resolve_root_without_cache(&state, &pubkey, &treename, None).await {
        let mut payload = json!({
            "key": key,
            "hash": to_hex(&resolved.cid.hash),
            "cid": resolved.cid.to_string(),
            "source": resolved.source,
        });
        if let Some(root) = resolved.root_event {
            payload["peer"] = json!(root.peer_id);
            payload["event_id"] = json!(root.event_id);
            payload["created_at"] = json!(root.created_at);
            payload["key_tag"] = json!(root.key);
            payload["encryptedKey"] = json!(root.encrypted_key);
            payload["selfEncryptedKey"] = json!(root.self_encrypted_key);
        }
        return Json(payload);
    }

    let resolver = match NostrRootResolver::new(resolver_config(&state)).await {
        Ok(r) => r,
        Err(e) => {
            return Json(json!({
                "error": format!("Failed to create resolver: {}", e),
                "key": key
            }));
        }
    };

    let relay_result =
        match tokio::time::timeout(HTTP_RESOLVER_TIMEOUT, resolver.resolve_wait(&key)).await {
            Ok(Ok(cid)) => {
                cache_public_tree_root(&state, &pubkey, &treename, &cid);
                Some(Json(json!({
                    "key": key,
                    "hash": to_hex(&cid.hash),
                    "cid": cid.to_string(),
                    "source": "nostr",
                })))
            }
            Ok(Err(_)) | Err(_) => None,
        };

    let _ = resolver.stop().await;
    if let Some(result) = relay_result {
        return result;
    }

    Json(json!({
        "error": "Resolution failed via relays, multicast, and peers",
        "key": key
    }))
}

/// List all trees for a pubkey
pub async fn list_trees(
    State(state): State<AppState>,
    Path(pubkey): Path<String>,
) -> impl IntoResponse {
    let resolver = match NostrRootResolver::new(resolver_config(&state)).await {
        Ok(r) => r,
        Err(e) => {
            return Json(json!({
                "error": format!("Failed to create resolver: {}", e),
                "pubkey": pubkey
            }));
        }
    };

    // list() uses the configured timeout internally
    let result = match resolver.list(&pubkey).await {
        Ok(entries) => Json(json!({
            "pubkey": pubkey,
            "trees": entries.iter().map(|e| json!({
                "name": e.key.split('/').next_back().unwrap_or(&e.key),
                "hash": to_hex(&e.cid.hash),
                "cid": e.cid.to_string()
            })).collect::<Vec<_>>()
        })),
        Err(e) => Json(json!({
            "error": e.to_string(),
            "pubkey": pubkey
        })),
    };

    let _ = resolver.stop().await;
    result
}

#[cfg(test)]
mod tests;
