use super::*;
use crate::fips_transport::DaemonFipsTransport;
use crate::server::blob_read::{
    acquire_blob_read, acquire_blob_write, blob_read_timeout, BLOB_READ_BUSY,
};
use crate::webrtc::WebRTCState;
use hashtree_core::{Link, TreeNode};
use serde::{Deserialize, Serialize};

const DEFAULT_COLD_READ_PREFETCH_CONCURRENCY: usize = 32;
const BLOB_BATCH_DOWNLOAD_MAGIC: &[u8; 8] = b"HTBDV1\0\0";
const BLOB_BATCH_DOWNLOAD_CONTENT_TYPE: &str =
    "application/vnd.hashtree.blob-batch.v1+octet-stream";
const DEFAULT_BLOB_BATCH_DOWNLOAD_MAX_HASHES: usize = 1024;
const DEFAULT_BLOB_BATCH_DOWNLOAD_MAX_BYTES: usize = 64 * 1024 * 1024;
const BLOB_BATCH_DOWNLOAD_MIN_BYTES: usize = 1024 * 1024;
const BLOB_BATCH_DOWNLOAD_MAX_BYTES: usize = 512 * 1024 * 1024;
const BLOB_BATCH_DOWNLOAD_MAX_HASHES_ENV: &str = "HTREE_BLOB_BATCH_DOWNLOAD_MAX_HASHES";
const BLOB_BATCH_DOWNLOAD_MAX_BYTES_ENV: &str = "HTREE_BLOB_BATCH_DOWNLOAD_MAX_BYTES";

pub(super) async fn fetch_and_cache_blob(state: &AppState, hash: &[u8]) -> bool {
    fetch_and_cache_blob_with_source(state, hash)
        .await
        .is_some()
}

pub(super) async fn fetch_and_cache_blob_with_source(
    state: &AppState,
    hash: &[u8],
) -> Option<BlobSource> {
    if !state.hash_get_enabled {
        return None;
    }

    let hash_hex = hex::encode(hash);
    tracing::info!(
        "[htree-fetch] Trying to fetch blob {} from upstream",
        &hash_hex[..16.min(hash_hex.len())]
    );

    enum FetchResult {
        WebRtc { data: Vec<u8>, peer_id: String },
        Fips { data: Vec<u8> },
        Upstream { data: Vec<u8>, server: String },
    }

    let mut fetches: Vec<BoxFuture<'static, Option<FetchResult>>> = Vec::new();

    if state.hash_get_enabled && state.http_webrtc_fetch {
        if let Some(ref webrtc_state) = state.webrtc_peers {
            tracing::info!(
                "[htree-fetch] Querying mesh peers for {}",
                &hash_hex[..16.min(hash_hex.len())]
            );
            let webrtc_state = webrtc_state.clone();
            let peer_hash_hex = hash_hex.clone();
            fetches.push(
                async move {
                    let query_hash_hex = peer_hash_hex.clone();
                    await_fetch_task("webrtc", &peer_hash_hex, async move {
                        query_webrtc_peers(&webrtc_state, &query_hash_hex).await
                    })
                    .await
                    .map(|(data, peer_id)| FetchResult::WebRtc { data, peer_id })
                }
                .boxed(),
            );
        }
    }

    if state.hash_get_enabled && state.fetch_from_fips_peers {
        if let Some(ref fips_transport) = state.fips_transport {
            tracing::info!(
                "[htree-fetch] Querying FIPS peers for {}",
                &hash_hex[..16.min(hash_hex.len())]
            );
            let fips_transport = fips_transport.clone();
            let fips_hash = hash.to_vec();
            let fips_hash_hex = hash_hex.clone();
            fetches.push(
                async move {
                    await_fetch_task("fips", &fips_hash_hex, async move {
                        query_fips_peers(&fips_transport, &fips_hash).await
                    })
                    .await
                    .map(|data| FetchResult::Fips { data })
                }
                .boxed(),
            );
        }
    }

    if !state.upstream_blossom.is_empty() {
        tracing::info!(
            "[htree-fetch] Querying {} Blossom servers for {}",
            state.upstream_blossom.len(),
            &hash_hex[..16.min(hash_hex.len())]
        );
        let upstream_blossom = state.upstream_blossom.clone();
        let upstream_http_client = state.upstream_http_client.clone();
        let upstream_hash_hex = hash_hex.clone();
        fetches.push(
            async move {
                let query_hash_hex = upstream_hash_hex.clone();
                await_fetch_task("upstream", &upstream_hash_hex, async move {
                    query_upstream_blossom(upstream_http_client, &upstream_blossom, &query_hash_hex)
                        .await
                })
                .await
                .map(|(data, server)| FetchResult::Upstream { data, server })
            }
            .boxed(),
        );
    } else {
        tracing::info!("[htree-fetch] No upstream Blossom servers configured");
    }

    if let Some(result) = first_available_fetch(fetches).await {
        match result {
            FetchResult::WebRtc { data, peer_id } => {
                tracing::info!(
                    "[htree-fetch] Got {} bytes from peer {} for {}",
                    data.len(),
                    peer_id,
                    &hash_hex[..16.min(hash_hex.len())]
                );
                let (_data, result) = put_cached_blob_without_blocking_runtime(state, data).await;
                if let Err(e) = result {
                    tracing::warn!("[htree-fetch] Failed to cache peer data: {}", e);
                    return None;
                }
                return Some(BlobSource::WebRtc(peer_id));
            }
            FetchResult::Fips { data } => {
                tracing::info!(
                    "[htree-fetch] Got {} bytes from FIPS peers for {}",
                    data.len(),
                    &hash_hex[..16.min(hash_hex.len())]
                );
                let (_data, result) = put_cached_blob_without_blocking_runtime(state, data).await;
                if let Err(e) = result {
                    tracing::warn!("[htree-fetch] Failed to cache FIPS peer data: {}", e);
                    return None;
                }
                return Some(BlobSource::Fips);
            }
            FetchResult::Upstream { data, server } => {
                tracing::info!(
                    "[htree-fetch] Got {} bytes from upstream {} for {}",
                    data.len(),
                    server,
                    &hash_hex[..16.min(hash_hex.len())]
                );
                let (_data, result) = put_cached_blob_without_blocking_runtime(state, data).await;
                if let Err(e) = result {
                    tracing::warn!("[htree-fetch] Failed to cache upstream data: {}", e);
                    return None;
                }
                return Some(BlobSource::Upstream(server));
            }
        }
    }

    if !state.upstream_blossom.is_empty() {
        tracing::info!(
            "[htree-fetch] No upstream had {}",
            &hash_hex[..16.min(hash_hex.len())]
        );
    }

    None
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(in crate::server) struct BlobBatchDownloadRequest {
    pub hashes: Vec<String>,
}

#[derive(Debug)]
pub(super) struct BlobBatchDownloadItem {
    pub hash: [u8; 32],
    pub data: Vec<u8>,
}

pub(in crate::server) async fn download_blob_batch(
    State(state): State<AppState>,
    Json(request): Json<BlobBatchDownloadRequest>,
) -> impl IntoResponse {
    let max_hashes = blob_batch_download_max_hashes();
    if request.hashes.len() > max_hashes {
        return Response::builder()
            .status(StatusCode::PAYLOAD_TOO_LARGE)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .body(Body::from(
                json!({
                    "error": "too many hashes",
                    "max_hashes": max_hashes,
                })
                .to_string(),
            ))
            .unwrap();
    }

    let mut hashes = Vec::with_capacity(request.hashes.len());
    let mut seen = HashSet::new();
    for hash_hex in request.hashes {
        let normalized = hash_hex.trim().to_ascii_lowercase();
        if normalized.len() != 64 || !normalized.chars().all(|ch| ch.is_ascii_hexdigit()) {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::from(
                    json!({
                        "error": "invalid hash",
                        "hash": hash_hex,
                    })
                    .to_string(),
                ))
                .unwrap();
        }

        let Ok(hash) = from_hex(&normalized) else {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::from(
                    json!({
                        "error": "invalid hash",
                        "hash": hash_hex,
                    })
                    .to_string(),
                ))
                .unwrap();
        };
        if seen.insert(hash) {
            hashes.push(hash);
        }
    }

    let max_bytes = blob_batch_download_max_bytes();
    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(BLOB_BATCH_DOWNLOAD_MAGIC);
    body.extend_from_slice(&0u32.to_be_bytes());

    let mut included = 0u32;
    let mut truncated = false;
    for hash in hashes {
        let data = match get_blob_without_blocking_runtime(&state, hash).await {
            Ok(Some(data)) => data,
            Ok(None) => continue,
            Err(error) => {
                return Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .header(header::CONTENT_TYPE, "text/plain")
                    .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                    .body(Body::from(error))
                    .unwrap();
            }
        };

        let entry_len = 32usize.saturating_add(8).saturating_add(data.len());
        if body.len().saturating_add(entry_len) > max_bytes {
            truncated = true;
            break;
        }

        body.extend_from_slice(&hash);
        body.extend_from_slice(&(data.len() as u64).to_be_bytes());
        body.extend_from_slice(&data);
        included = included.saturating_add(1);
    }

    body[8..12].copy_from_slice(&included.to_be_bytes());

    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, BLOB_BATCH_DOWNLOAD_CONTENT_TYPE)
        .header(header::CONTENT_LENGTH, body.len())
        .header(header::CACHE_CONTROL, IMMUTABLE_CACHE_CONTROL)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(CROSS_ORIGIN_RESOURCE_POLICY_HEADER, CORP_CROSS_ORIGIN);
    if truncated {
        builder = builder.header("X-Hashtree-Batch-Truncated", "true");
    }

    builder.body(Body::from(body)).unwrap()
}

pub(super) fn decode_blob_batch_download_response(
    bytes: &[u8],
) -> Result<Vec<BlobBatchDownloadItem>, String> {
    if bytes.len() < 12 {
        return Err("batch response too short".to_string());
    }
    if &bytes[..8] != BLOB_BATCH_DOWNLOAD_MAGIC {
        return Err("invalid batch response magic".to_string());
    }

    let count = u32::from_be_bytes(
        bytes[8..12]
            .try_into()
            .map_err(|_| "invalid batch response count".to_string())?,
    ) as usize;
    let mut offset = 12usize;
    let mut items = Vec::with_capacity(count);

    for _ in 0..count {
        if bytes.len().saturating_sub(offset) < 40 {
            return Err("truncated batch response entry header".to_string());
        }
        let hash: [u8; 32] = bytes[offset..offset + 32]
            .try_into()
            .map_err(|_| "invalid batch response hash".to_string())?;
        offset += 32;
        let len = u64::from_be_bytes(
            bytes[offset..offset + 8]
                .try_into()
                .map_err(|_| "invalid batch response entry length".to_string())?,
        );
        offset += 8;
        let len = usize::try_from(len)
            .map_err(|_| "batch response entry length too large".to_string())?;
        if bytes.len().saturating_sub(offset) < len {
            return Err("truncated batch response entry body".to_string());
        }
        items.push(BlobBatchDownloadItem {
            hash,
            data: bytes[offset..offset + len].to_vec(),
        });
        offset += len;
    }

    if offset != bytes.len() {
        return Err("batch response has trailing bytes".to_string());
    }

    Ok(items)
}

async fn fetch_and_cache_blobs_from_upstream_batch(state: &AppState, hashes: &[[u8; 32]]) -> usize {
    if hashes.len() < 2 || state.upstream_blossom.is_empty() || !state.hash_get_enabled {
        return 0;
    }

    let mut missing = Vec::new();
    for hash in hashes {
        match state.store.blob_exists(hash) {
            Ok(false) => missing.push(*hash),
            Ok(true) => {}
            Err(error) => {
                tracing::debug!(
                    "[htree-fetch] skipped batch candidate {}: {}",
                    &to_hex(hash)[..16],
                    error
                );
            }
        }
    }
    if missing.len() < 2 {
        return 0;
    }

    let Some((items, server)) = query_upstream_blossom_batch(
        state.upstream_http_client.clone(),
        &state.upstream_blossom,
        &missing,
    )
    .await
    else {
        return 0;
    };

    let mut cached = 0usize;
    let mut batch_items = Vec::with_capacity(items.len());
    for item in items {
        if let Err(rejection) = crate::server::ingest_filter::validate_untrusted_blob(
            &item.data,
            state.require_random_untrusted_ingest,
        ) {
            tracing::warn!(
                "[htree-fetch] rejected batch blob {} from {}: {}",
                &to_hex(&item.hash)[..16],
                server,
                rejection.reason
            );
            continue;
        }
        batch_items.push((item.hash, item.data));
    }

    match put_cached_blobs_without_blocking_runtime(state, batch_items).await {
        Ok(inserted_or_present) => cached = inserted_or_present,
        Err(error) => tracing::warn!(
            "[htree-fetch] failed to cache upstream batch from {}: {}",
            server,
            error
        ),
    }

    if cached > 0 {
        tracing::info!(
            "[htree-fetch] Got {} blobs from upstream batch {}",
            cached,
            server
        );
    }
    cached
}

async fn put_cached_blobs_without_blocking_runtime(
    state: &AppState,
    items: Vec<([u8; 32], Vec<u8>)>,
) -> Result<usize, String> {
    if items.is_empty() {
        return Ok(0);
    }

    let permit = acquire_blob_write().await.map_err(str::to_string)?;
    let store = state.store.clone();
    let blob_cache = state.blob_cache.clone();
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let result = store
            .put_cached_blobs_report(&items)
            .map_err(|e| e.to_string());
        if result.is_ok() {
            for (hash, data) in &items {
                let hash_hex = to_hex(hash);
                blob_cache.put_size(hash_hex.clone(), Some(data.len() as u64));
                blob_cache.put_body(hash_hex, data);
            }
        }
        result.map(|report| report.inserted_hashes.len())
    })
    .await
    {
        Ok(result) => result,
        Err(err) => Err(format!("cached blob batch write task failed: {}", err)),
    }
}

pub(super) async fn put_cached_blob_without_blocking_runtime(
    state: &AppState,
    data: Vec<u8>,
) -> (Vec<u8>, Result<String, String>) {
    if let Err(rejection) = crate::server::ingest_filter::validate_untrusted_blob(
        &data,
        state.require_random_untrusted_ingest,
    ) {
        return (data, Err(rejection.reason));
    }

    let permit = match acquire_blob_write().await {
        Ok(permit) => permit,
        Err(error) => return (data, Err(error.to_string())),
    };
    let store = state.store.clone();
    let blob_cache = state.blob_cache.clone();
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let result = store.put_cached_blob(&data).map_err(|e| e.to_string());
        if let Ok(hash_hex) = &result {
            blob_cache.put_size(hash_hex.clone(), Some(data.len() as u64));
            blob_cache.put_body(hash_hex.clone(), &data);
        }
        (data, result)
    })
    .await
    {
        Ok(result) => result,
        Err(err) => (
            Vec::new(),
            Err(format!("cached blob write task failed: {}", err)),
        ),
    }
}

pub(super) async fn get_blob_without_blocking_runtime(
    state: &AppState,
    hash: [u8; 32],
) -> Result<Option<Vec<u8>>, String> {
    let hash_hex = to_hex(&hash);
    if let Some(data) = state.blob_cache.get_body(&hash_hex) {
        return Ok(Some(data));
    }

    let read = {
        let mut inflight = state.inflight_blob_reads.lock().await;
        if let Some(existing) = inflight.get(&hash_hex) {
            existing.clone()
        } else {
            let state = state.clone();
            let hash_for_read = hash;
            let hash_hex_for_task = hash_hex.clone();
            let read = async move {
                let result = get_blob_once_without_blocking_runtime(&state, hash_for_read).await;
                state
                    .inflight_blob_reads
                    .lock()
                    .await
                    .remove(&hash_hex_for_task);
                result
            }
            .boxed()
            .shared();
            inflight.insert(hash_hex, read.clone());
            read
        }
    };

    read.await
}

pub(super) async fn get_blob_size_without_blocking_runtime(
    state: &AppState,
    hash: [u8; 32],
) -> Result<Option<u64>, String> {
    let hash_hex = to_hex(&hash);
    if let Some(size) = state.blob_cache.get_size(&hash_hex) {
        return Ok(size);
    }

    let permit = acquire_blob_read().await.map_err(str::to_string)?;
    let store = state.store.clone();
    let read =
        tokio::task::spawn_blocking(move || store.blob_size(&hash).map_err(|e| e.to_string()));
    let result = tokio::time::timeout(blob_read_timeout(), read).await;
    drop(permit);
    match result {
        Ok(Ok(result)) => {
            if let Ok(size) = &result {
                state.blob_cache.put_size(hash_hex, *size);
            }
            result
        }
        Ok(Err(err)) => Err(format!("blob size task failed: {}", err)),
        Err(_) => Err("blob size timed out".to_string()),
    }
}

pub(super) async fn get_blob_range_without_blocking_runtime(
    state: &AppState,
    hash: [u8; 32],
    start: u64,
    end_inclusive: u64,
) -> Result<Option<Vec<u8>>, String> {
    let permit = acquire_blob_read().await.map_err(str::to_string)?;
    let store = state.store.clone();
    let read = tokio::task::spawn_blocking(move || {
        store
            .get_blob_range(&hash, start, end_inclusive)
            .map_err(|e| e.to_string())
    });
    let result = tokio::time::timeout(blob_read_timeout(), read).await;
    drop(permit);
    match result {
        Ok(Ok(result)) => result,
        Ok(Err(err)) => Err(format!("blob range read task failed: {}", err)),
        Err(_) => Err("blob range read timed out".to_string()),
    }
}

async fn get_blob_once_without_blocking_runtime(
    state: &AppState,
    hash: [u8; 32],
) -> Result<Option<Vec<u8>>, String> {
    let permit = acquire_blob_read().await.map_err(str::to_string)?;
    let store = state.store.clone();
    let read =
        tokio::task::spawn_blocking(move || store.get_blob(&hash).map_err(|e| e.to_string()));
    let result = tokio::time::timeout(blob_read_timeout(), read).await;
    drop(permit);
    match result {
        Ok(Ok(result)) => {
            if let Ok(data) = &result {
                match data {
                    Some(data) => {
                        let hash_hex = to_hex(&hash);
                        state
                            .blob_cache
                            .put_size(hash_hex.clone(), Some(data.len() as u64));
                        state.blob_cache.put_body(hash_hex, data);
                    }
                    None => {
                        state.blob_cache.put_size(to_hex(&hash), None);
                    }
                }
            }
            result
        }
        Ok(Err(err)) => Err(format!("blob read task failed: {}", err)),
        Err(_) => Err("blob read timed out".to_string()),
    }
}

pub(super) fn blob_read_busy_error() -> &'static str {
    BLOB_READ_BUSY
}

pub(super) async fn await_fetch_task<F, T>(source: &str, hash_hex: &str, future: F) -> Option<T>
where
    F: std::future::Future<Output = Option<T>>,
{
    match std::panic::AssertUnwindSafe(future).catch_unwind().await {
        Ok(result) => result,
        Err(_) => {
            tracing::warn!(
                "[htree-fetch] {} fetch task panicked for {}",
                source,
                &hash_hex[..16.min(hash_hex.len())],
            );
            None
        }
    }
}

pub(super) async fn first_available_fetch<T>(
    futures: Vec<BoxFuture<'static, Option<T>>>,
) -> Option<T> {
    let mut pending = FuturesUnordered::new();
    for future in futures {
        pending.push(future);
    }

    while let Some(result) = pending.next().await {
        if let Some(value) = result {
            return Some(value);
        }
    }

    None
}

pub(super) async fn ensure_blob_available(
    state: &AppState,
    hash: &[u8; 32],
) -> Result<bool, String> {
    if state.store.blob_exists(hash).map_err(|e| e.to_string())? {
        return Ok(true);
    }

    let hash_hex = to_hex(hash);
    let fetch = {
        let mut inflight = state.inflight_blob_fetches.lock().await;
        if let Some(existing) = inflight.get(&hash_hex) {
            existing.clone()
        } else {
            let state = state.clone();
            let hash_bytes = *hash;
            let hash_hex_for_task = hash_hex.clone();
            let fetch = async move {
                let fetched = fetch_and_cache_blob(&state, &hash_bytes).await;
                state
                    .inflight_blob_fetches
                    .lock()
                    .await
                    .remove(&hash_hex_for_task);
                fetched
            }
            .boxed()
            .shared();
            inflight.insert(hash_hex.clone(), fetch.clone());
            fetch
        }
    };

    if fetch.await {
        return Ok(true);
    }

    state.store.blob_exists(hash).map_err(|e| e.to_string())
}

pub(super) async fn fetch_missing_chunk(
    state: &AppState,
    seen_missing: &mut HashSet<String>,
    missing: &str,
) -> Result<bool, String> {
    if !seen_missing.insert(missing.to_string()) {
        return Err(format!("Repeated missing chunk {}", missing));
    }

    let hash =
        from_hex(missing).map_err(|e| format!("Invalid missing chunk hash {}: {}", missing, e))?;
    ensure_blob_available(state, &hash).await
}

pub(super) async fn list_directory_with_fetch<S: Store>(
    state: &AppState,
    tree: &HashTree<S>,
    cid: &Cid,
) -> Result<Option<Vec<TreeEntry>>, String> {
    let cache_key = cid_cache_key(cid);
    if let Some(cached) = get_cached_lookup(&state.directory_listing_cache, &cache_key) {
        return Ok(cached);
    }

    let mut seen_missing = HashSet::new();

    loop {
        if !ensure_blob_available(state, &cid.hash).await? {
            put_cached_lookup(&state.directory_listing_cache, cache_key.clone(), None);
            return Ok(None);
        }

        match tree.list_directory(cid).await {
            Ok(entries) => {
                put_cached_lookup(
                    &state.directory_listing_cache,
                    cache_key.clone(),
                    Some(entries.clone()),
                );
                return Ok(Some(entries));
            }
            Err(HashTreeError::MissingChunk(missing)) => {
                if !fetch_missing_chunk(state, &mut seen_missing, &missing).await? {
                    put_cached_lookup(&state.directory_listing_cache, cache_key.clone(), None);
                    return Ok(None);
                }
            }
            Err(err) => return Err(err.to_string()),
        }
    }
}

pub(super) async fn resolve_path_with_fetch<S: Store>(
    state: &AppState,
    tree: &HashTree<S>,
    root_cid: &Cid,
    path: &str,
) -> Result<Option<ResolvedPathEntry>, String> {
    let cache_key = resolved_path_cache_key(root_cid, path);
    if let Some(cached) = get_cached_lookup(&state.resolved_path_cache, &cache_key) {
        return Ok(cached.map(|entry| ResolvedPathEntry {
            cid: entry.cid,
            link_type: entry.link_type,
        }));
    }

    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    if parts.is_empty() {
        let entry = ResolvedPathEntry {
            cid: root_cid.clone(),
            link_type: LinkType::Dir,
        };
        put_cached_lookup(
            &state.resolved_path_cache,
            cache_key,
            Some(cached_resolved_path_entry(&entry)),
        );
        return Ok(Some(entry));
    }

    let mut current_cid = root_cid.clone();
    let mut current_link_type = LinkType::Dir;

    for part in parts {
        let entries = match list_directory_with_fetch(state, tree, &current_cid).await? {
            Some(entries) => entries,
            None => {
                put_cached_lookup(&state.resolved_path_cache, cache_key, None);
                return Ok(None);
            }
        };

        let Some(entry) = entries.into_iter().find(|entry| entry.name == part) else {
            put_cached_lookup(&state.resolved_path_cache, cache_key, None);
            return Ok(None);
        };

        current_link_type = entry.link_type;
        current_cid = Cid {
            hash: entry.hash,
            key: entry.key,
        };
    }

    let entry = ResolvedPathEntry {
        cid: current_cid,
        link_type: current_link_type,
    };
    put_cached_lookup(
        &state.resolved_path_cache,
        cache_key,
        Some(cached_resolved_path_entry(&entry)),
    );
    Ok(Some(entry))
}

#[cfg(test)]
pub(super) async fn get_cid_with_fetch<S: Store>(
    state: &AppState,
    tree: &HashTree<S>,
    cid: &Cid,
) -> Result<Option<Vec<u8>>, String> {
    let mut seen_missing = HashSet::new();

    loop {
        if !ensure_blob_available(state, &cid.hash).await? {
            return Ok(None);
        }

        if let Err(err) = prefetch_file_range_chunks_with_fetch(state, tree, cid, 0, None).await {
            tracing::debug!("file chunk prefetch skipped: {}", err);
        }

        match tree.get(cid, None).await {
            Ok(data) => return Ok(data),
            Err(HashTreeError::MissingChunk(missing)) => {
                if !fetch_missing_chunk(state, &mut seen_missing, &missing).await? {
                    return Ok(None);
                }
            }
            Err(err) => return Err(err.to_string()),
        }
    }
}

pub(super) async fn read_file_range_cid_with_fetch<S: Store>(
    state: &AppState,
    tree: &HashTree<S>,
    cid: &Cid,
    start: u64,
    end: Option<u64>,
) -> Result<Option<Vec<u8>>, String> {
    let mut seen_missing = HashSet::new();

    loop {
        if !ensure_blob_available(state, &cid.hash).await? {
            return Ok(None);
        }

        if let Err(err) = prefetch_file_range_chunks_with_fetch(state, tree, cid, start, end).await
        {
            tracing::debug!("file range chunk prefetch skipped: {}", err);
        }

        match tree.read_file_range_cid(cid, start, end).await {
            Ok(data) => return Ok(data),
            Err(HashTreeError::MissingChunk(missing)) => {
                if !fetch_missing_chunk(state, &mut seen_missing, &missing).await? {
                    return Ok(None);
                }
            }
            Err(err) => return Err(err.to_string()),
        }
    }
}

async fn prefetch_file_range_chunks_with_fetch<S: Store>(
    state: &AppState,
    tree: &HashTree<S>,
    cid: &Cid,
    start: u64,
    end: Option<u64>,
) -> Result<(), String> {
    let Some(node) = tree.get_node(cid).await.map_err(|e| e.to_string())? else {
        return Ok(());
    };
    if node.node_type != LinkType::File {
        return Ok(());
    }

    let total_size: u64 = node.links.iter().map(|link| link.size).sum();
    let actual_end = end.unwrap_or(total_size).min(total_size);
    if start >= actual_end {
        return Ok(());
    }

    let mut hashes = Vec::new();
    let mut seen = HashSet::new();
    collect_range_leaf_hashes(
        state,
        tree,
        &node,
        start,
        actual_end,
        0,
        &mut hashes,
        &mut seen,
    )
    .await?;

    if hashes.is_empty() {
        return Ok(());
    }

    fetch_and_cache_blobs_from_upstream_batch(state, &hashes).await;

    let concurrency = cold_read_prefetch_concurrency();
    let mut fetches = stream::iter(
        hashes
            .into_iter()
            .map(|hash| async move { ensure_blob_available(state, &hash).await }),
    )
    .buffer_unordered(concurrency);

    while let Some(result) = fetches.next().await {
        result?;
    }

    Ok(())
}

fn cold_read_prefetch_concurrency() -> usize {
    std::env::var("HTREE_COLD_READ_PREFETCH_CONCURRENCY")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .map(|value| value.clamp(1, 256))
        .unwrap_or(DEFAULT_COLD_READ_PREFETCH_CONCURRENCY)
}

fn blob_batch_download_max_hashes() -> usize {
    std::env::var(BLOB_BATCH_DOWNLOAD_MAX_HASHES_ENV)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .map(|value| value.clamp(1, 16_384))
        .unwrap_or(DEFAULT_BLOB_BATCH_DOWNLOAD_MAX_HASHES)
}

fn blob_batch_download_max_bytes() -> usize {
    std::env::var(BLOB_BATCH_DOWNLOAD_MAX_BYTES_ENV)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .map(|value| value.clamp(BLOB_BATCH_DOWNLOAD_MIN_BYTES, BLOB_BATCH_DOWNLOAD_MAX_BYTES))
        .unwrap_or(DEFAULT_BLOB_BATCH_DOWNLOAD_MAX_BYTES)
}

async fn collect_range_leaf_hashes<S: Store>(
    state: &AppState,
    tree: &HashTree<S>,
    node: &TreeNode,
    start: u64,
    end: u64,
    base_offset: u64,
    hashes: &mut Vec<[u8; 32]>,
    seen: &mut HashSet<[u8; 32]>,
) -> Result<(), String> {
    let mut current_offset = base_offset;

    for link in &node.links {
        let child_start = current_offset;
        let child_end = child_start.saturating_add(link.size);
        current_offset = child_end;

        if child_end <= start {
            continue;
        }
        if child_start >= end {
            break;
        }

        if link.link_type == LinkType::File
            && state
                .store
                .blob_exists(&link.hash)
                .map_err(|err| err.to_string())?
        {
            let child_cid = link_to_cid(link);
            if let Some(child_node) = tree
                .get_node(&child_cid)
                .await
                .map_err(|err| err.to_string())?
            {
                if child_node.node_type == LinkType::File {
                    Box::pin(collect_range_leaf_hashes(
                        state,
                        tree,
                        &child_node,
                        start,
                        end,
                        child_start,
                        hashes,
                        seen,
                    ))
                    .await?;
                    continue;
                }
            }
        }

        if seen.insert(link.hash) {
            hashes.push(link.hash);
        }
    }

    Ok(())
}

fn link_to_cid(link: &Link) -> Cid {
    Cid {
        hash: link.hash,
        key: link.key,
    }
}

pub(super) fn stream_file_range_cid_with_fetch(
    state: AppState,
    cid: Cid,
    start: u64,
    end_inclusive: u64,
) -> impl futures::Stream<Item = Result<Bytes, std::io::Error>> {
    stream::unfold(
        (state, cid, start, end_inclusive, false),
        |(state, cid, offset, end_inclusive, finished)| async move {
            if finished || offset > end_inclusive {
                return None;
            }

            let chunk_end_inclusive = offset
                .saturating_add(CID_RANGE_STREAM_CHUNK_SIZE - 1)
                .min(end_inclusive);
            let chunk_end_exclusive = chunk_end_inclusive.saturating_add(1);
            let tree = HashTree::new(HashTreeConfig::new(state.store.store_arc()).public());

            match read_file_range_cid_with_fetch(
                &state,
                &tree,
                &cid,
                offset,
                Some(chunk_end_exclusive),
            )
            .await
            {
                Ok(Some(data)) if !data.is_empty() => Some((
                    Ok(Bytes::from(data)),
                    (
                        state,
                        cid,
                        chunk_end_inclusive.saturating_add(1),
                        end_inclusive,
                        false,
                    ),
                )),
                Ok(Some(_)) | Ok(None) => Some((
                    Err(std::io::Error::other("CID range returned no data")),
                    (state, cid, end_inclusive, end_inclusive, true),
                )),
                Err(err) => Some((
                    Err(std::io::Error::other(err)),
                    (state, cid, end_inclusive, end_inclusive, true),
                )),
            }
        },
    )
}

pub(super) async fn get_size_cid_with_fetch<S: Store>(
    state: &AppState,
    tree: &HashTree<S>,
    cid: &Cid,
) -> Result<Option<u64>, String> {
    let cache_key = cid_cache_key(cid);
    if let Some(cached) = get_cached_lookup(&state.cid_size_cache, &cache_key) {
        return Ok(cached);
    }

    let mut seen_missing = HashSet::new();

    loop {
        if !ensure_blob_available(state, &cid.hash).await? {
            put_cached_lookup(&state.cid_size_cache, cache_key.clone(), None);
            return Ok(None);
        }

        match tree.get_size_cid(cid).await {
            Ok(size) => {
                put_cached_lookup(&state.cid_size_cache, cache_key.clone(), Some(size));
                return Ok(Some(size));
            }
            Err(HashTreeError::MissingChunk(missing)) => {
                if !fetch_missing_chunk(state, &mut seen_missing, &missing).await? {
                    put_cached_lookup(&state.cid_size_cache, cache_key.clone(), None);
                    return Ok(None);
                }
            }
            Err(err) => return Err(err.to_string()),
        }
    }
}

pub(super) async fn root_is_directory_with_fetch<S: Store>(
    state: &AppState,
    tree: &HashTree<S>,
    cid: &Cid,
) -> Result<bool, String> {
    if !ensure_blob_available(state, &cid.hash).await? {
        return Ok(false);
    }

    match tree.get_node(cid).await.map_err(|e| e.to_string())? {
        Some(node) if node.node_type == LinkType::Dir => Ok(true),
        Some(node) if node.node_type == LinkType::File => {
            let mut seen_missing = HashSet::new();
            loop {
                match tree.is_dir(cid).await {
                    Ok(is_dir) => return Ok(is_dir),
                    Err(HashTreeError::MissingChunk(missing)) => {
                        if !fetch_missing_chunk(state, &mut seen_missing, &missing).await? {
                            return Ok(false);
                        }
                    }
                    Err(err) => return Err(err.to_string()),
                }
            }
        }
        Some(_) | None => Ok(false),
    }
}

#[cfg(test)]
pub(super) async fn await_webrtc_peer_response<F>(
    future: F,
    hash_hex: &str,
    timeout: Duration,
) -> Option<(Vec<u8>, String)>
where
    F: std::future::Future<Output = Option<(Vec<u8>, String)>>,
{
    match tokio::time::timeout(timeout, future).await {
        Ok(result) => result,
        Err(_) => {
            tracing::warn!(
                "[htree-fetch] Mesh peer query timed out for {}",
                &hash_hex[..16.min(hash_hex.len())]
            );
            None
        }
    }
}

pub(super) enum BlobSource {
    Local,
    WebRtc(String),
    Fips,
    Upstream(String),
}

impl BlobSource {
    fn to_header_value(&self) -> String {
        match self {
            BlobSource::Local => "local".to_string(),
            BlobSource::WebRtc(peer_id) => format!("webrtc:{peer_id}"),
            BlobSource::Fips => "fips".to_string(),
            BlobSource::Upstream(server) => format!("upstream:{server}"),
        }
    }
}

/// Build a blob response with optional X-Source header (only for localhost)
pub(super) fn build_blob_response(
    data: Vec<u8>,
    source: BlobSource,
    is_localhost: bool,
) -> Response<Body> {
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, data.len())
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CACHE_CONTROL, IMMUTABLE_CACHE_CONTROL)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(CROSS_ORIGIN_RESOURCE_POLICY_HEADER, CORP_CROSS_ORIGIN);

    if is_localhost {
        builder = builder.header("X-Source", source.to_header_value());
    }

    builder.body(Body::from(data)).unwrap()
}

pub(super) async fn query_fips_peers(
    fips_transport: &Arc<DaemonFipsTransport>,
    hash: &[u8],
) -> Option<Vec<u8>> {
    let hash: [u8; 32] = hash.try_into().ok()?;
    match fips_transport.get(&hash).await {
        Ok(Some(data)) => Some(data),
        Ok(None) => None,
        Err(err) => {
            tracing::warn!("FIPS peer fetch failed: {}", err);
            None
        }
    }
}

pub(super) async fn query_webrtc_peers(
    webrtc_state: &Arc<WebRTCState>,
    hash_hex: &str,
) -> Option<(Vec<u8>, String)> {
    if let Some((data, peer_id)) = webrtc_state.request_from_peers_with_source(hash_hex).await {
        tracing::info!(
            "Got {} bytes from peer {} for hash {}",
            data.len(),
            peer_id,
            &hash_hex[..16.min(hash_hex.len())]
        );
        return Some((data, peer_id));
    }

    tracing::debug!(
        "No connected mesh peer returned hash {}",
        &hash_hex[..16.min(hash_hex.len())]
    );

    None
}

/// Query upstream Blossom servers for content by hash
/// Returns the first successful response with server URL, or None if not found
pub(super) async fn query_upstream_blossom(
    client: reqwest::Client,
    servers: &[String],
    hash_hex: &str,
) -> Option<(Vec<u8>, String)> {
    use sha2::{Digest, Sha256};

    let mut pending = FuturesUnordered::new();
    for server in servers {
        let client = client.clone();
        let server = server.clone();
        let hash_hex = hash_hex.to_string();
        pending.push(async move {
            let url = format!("{}/{}.bin", server.trim_end_matches('/'), hash_hex);
            tracing::debug!("Trying upstream Blossom: {}", url);

            match client.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => match resp.bytes().await {
                    Ok(bytes) => {
                        let mut hasher = Sha256::new();
                        hasher.update(&bytes);
                        let computed = hex::encode(hasher.finalize());

                        if computed == hash_hex {
                            tracing::info!(
                                "Got {} bytes from upstream {} for hash {}",
                                bytes.len(),
                                server,
                                &hash_hex[..16.min(hash_hex.len())]
                            );
                            Some((bytes.to_vec(), server))
                        } else {
                            tracing::warn!(
                                "Hash mismatch from {}: expected {}, got {}",
                                server,
                                &hash_hex[..16.min(hash_hex.len())],
                                &computed[..16.min(computed.len())]
                            );
                            None
                        }
                    }
                    Err(err) => {
                        tracing::debug!("Upstream {} body read error: {}", server, err);
                        None
                    }
                },
                Ok(resp) => {
                    tracing::debug!("Upstream {} returned {}", server, resp.status());
                    None
                }
                Err(e) => {
                    tracing::debug!("Upstream {} error: {}", server, e);
                    None
                }
            }
        });
    }

    while let Some(result) = pending.next().await {
        if result.is_some() {
            return result;
        }
    }

    None
}

/// Query upstream hashtree/Blossom servers for multiple content hashes.
/// Returns verified blobs from the first server that supports the extension and
/// has at least one requested blob. Missing blobs are omitted and left for the
/// normal single-hash fallback.
pub(super) async fn query_upstream_blossom_batch(
    client: reqwest::Client,
    servers: &[String],
    hashes: &[[u8; 32]],
) -> Option<(Vec<BlobBatchDownloadItem>, String)> {
    if hashes.len() < 2 {
        return None;
    }

    let request_hashes: Vec<String> = hashes.iter().map(to_hex).collect();
    let requested_hashes: HashSet<[u8; 32]> = hashes.iter().copied().collect();
    let mut pending = FuturesUnordered::new();

    for server in servers {
        let client = client.clone();
        let server = server.trim_end_matches('/').to_string();
        let request = BlobBatchDownloadRequest {
            hashes: request_hashes.clone(),
        };
        let requested_hashes = requested_hashes.clone();
        pending.push(async move {
            let url = format!("{server}/blob/batch");
            match client.post(&url).json(&request).send().await {
                Ok(resp) if resp.status().is_success() => match resp.bytes().await {
                    Ok(bytes) => {
                        let items = match decode_blob_batch_download_response(&bytes) {
                            Ok(items) => items,
                            Err(error) => {
                                tracing::debug!(
                                    "Upstream {} batch response decode error: {}",
                                    server,
                                    error
                                );
                                return None;
                            }
                        };

                        let mut verified = Vec::with_capacity(items.len());
                        let mut seen = HashSet::new();
                        for item in items {
                            if !requested_hashes.contains(&item.hash) || !seen.insert(item.hash) {
                                continue;
                            }
                            let computed = sha256(&item.data);
                            if computed == item.hash {
                                verified.push(item);
                            } else {
                                tracing::warn!(
                                    "Hash mismatch from {} batch: expected {}, got {}",
                                    server,
                                    &to_hex(&item.hash)[..16],
                                    &to_hex(&computed)[..16]
                                );
                            }
                        }

                        if verified.is_empty() {
                            None
                        } else {
                            Some((verified, server))
                        }
                    }
                    Err(err) => {
                        tracing::debug!("Upstream {} batch body read error: {}", server, err);
                        None
                    }
                },
                Ok(resp) => {
                    tracing::debug!("Upstream {} batch returned {}", server, resp.status());
                    None
                }
                Err(error) => {
                    tracing::debug!("Upstream {} batch error: {}", server, error);
                    None
                }
            }
        });
    }

    while let Some(result) = pending.next().await {
        if result.is_some() {
            return result;
        }
    }

    None
}
