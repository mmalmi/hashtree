//! Native htree HTTP server for Tauri
//!
//! Serves /htree/* routes just like the service worker does in the browser.
//! Runs a local HTTP server that the frontend can use.
//!
//! Routes:
//! - /htree/{npub}/{treeName}/{path} - Npub-based file access (mutable)
//! - /htree/{nhash}/{filename} - Direct nhash access (content-addressed)

use axum::{
    body::Body,
    extract::{OriginalUri, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{any, get, post},
    Json, Router,
};
use hashtree_blossom::{BlossomClient, BlossomStore};
use hashtree_core::{
    decode_tree_node, decrypt_chk, from_hex, is_tree_node, nhash_decode, to_hex, Cid, HashTree,
    HashTreeConfig, Store, StoreError,
};
use hashtree_fs::FsBlobStore;
use hashtree_resolver::{
    nostr::{NostrResolverConfig, NostrRootResolver},
    RootResolver,
};
use lru::LruCache;
use nostr_sdk::Keys;
use parking_lot::RwLock;
use serde::Deserialize;
use serde_json::json;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use thiserror::Error;
use tokio::net::TcpListener;
use tower_http::cors::{Any, CorsLayer};
use tracing::{debug, error, info, warn};

use crate::relay_proxy::{handle_relay_websocket, RelayProxyState};

/// Default Blossom servers for fetching blobs (matches web app defaults)
const DEFAULT_BLOSSOM_SERVERS: &[&str] = &[
    "https://cdn.iris.to",
];

/// Default Nostr relays for resolving tree roots
const DEFAULT_NOSTR_RELAYS: &[&str] = &[
    "wss://relay.damus.io",
    "wss://relay.primal.net",
    "wss://nos.lol",
    "wss://relay.nostr.band",
    "wss://relay.snort.social",
    "wss://temp.iris.to",
];

/// npub pattern: npub1 followed by 58 bech32 characters
fn is_npub(s: &str) -> bool {
    s.len() == 63
        && s.starts_with("npub1")
        && s.chars().skip(5).all(|c| c.is_ascii_alphanumeric())
}

#[derive(Error, Debug)]
pub enum HtreeError {
    #[error("Invalid path: {0}")]
    InvalidPath(String),
    #[error("Tree not found: {0}")]
    TreeNotFound(String),
    #[error("File not found: {0}")]
    FileNotFound(String),
    #[error("Resolver error: {0}")]
    Resolver(String),
    #[error("Store error: {0}")]
    Store(String),
    #[error("IO error: {0}")]
    Io(String),
}

impl IntoResponse for HtreeError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            HtreeError::FileNotFound(_) | HtreeError::TreeNotFound(_) => {
                warn!("htree not found: {}", self);
                (StatusCode::NOT_FOUND, self.to_string())
            }
            HtreeError::InvalidPath(_) => {
                warn!("htree bad request: {}", self);
                (StatusCode::BAD_REQUEST, self.to_string())
            }
            _ => {
                error!("htree error: {}", self);
                (StatusCode::INTERNAL_SERVER_ERROR, self.to_string())
            }
        };

        Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "text/plain")
            .body(Body::from(message))
            .unwrap()
    }
}

/// Guess MIME type from file path/extension
fn guess_mime_type(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        // Video
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "ogg" | "ogv" => "video/ogg",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "mkv" => "video/x-matroska",
        // Audio
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "m4a" | "aac" => "audio/mp4",
        "oga" => "audio/ogg",
        // Images
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        // Documents
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "md" => "text/markdown",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" => "application/javascript",
        "json" => "application/json",
        "xml" => "application/xml",
        // Archives
        "zip" => "application/zip",
        "tar" => "application/x-tar",
        "gz" => "application/gzip",
        // Code
        "ts" | "tsx" => "text/typescript",
        "jsx" => "text/javascript",
        "py" => "text/x-python",
        "rs" => "text/x-rust",
        "go" => "text/x-go",
        _ => "application/octet-stream",
    }
}

const THUMBNAIL_PATTERNS: &[&str] = &[
    "thumbnail.jpg",
    "thumbnail.webp",
    "thumbnail.png",
    "thumbnail.jpeg",
];

const VIDEO_EXTENSIONS: &[&str] = &[".mp4", ".webm", ".mkv", ".mov", ".avi", ".m4v"];

fn is_video_filename(name: &str) -> bool {
    name.starts_with("video.")
        || VIDEO_EXTENSIONS.iter().any(|ext| name.ends_with(ext))
}

fn is_metadata_filename(name: &str) -> bool {
    name.ends_with(".json") || name.ends_with(".txt")
}

fn is_thumbnail_request(path: &str) -> bool {
    path == "thumbnail" || path.ends_with("/thumbnail")
}

/// Tree visibility types (matches TypeScript TreeVisibility)
#[derive(Clone, Debug, PartialEq)]
pub enum TreeVisibility {
    Public,
    LinkVisible,
    Private,
}

impl TreeVisibility {
    fn from_str(s: &str) -> Self {
        match s {
            "link-visible" => TreeVisibility::LinkVisible,
            "private" => TreeVisibility::Private,
            _ => TreeVisibility::Public,
        }
    }
}

/// Cached tree root entry
#[derive(Clone)]
struct CachedRoot {
    cid: Cid,
    visibility: TreeVisibility,
    #[allow(dead_code)]
    timestamp: std::time::Instant,
}

/// Combined store that checks local filesystem first, then Blossom
/// This allows serving data that's stored locally but not yet synced to Blossom
pub struct CombinedStore {
    local: Arc<FsBlobStore>,
    blossom: Arc<BlossomStore>,
}

impl CombinedStore {
    pub fn new(local: Arc<FsBlobStore>, blossom: Arc<BlossomStore>) -> Self {
        Self { local, blossom }
    }
}

#[async_trait::async_trait]
impl Store for CombinedStore {
    async fn get(&self, hash: &[u8; 32]) -> Result<Option<Vec<u8>>, StoreError> {
        // Try local store first (FsBlobStore implements Store directly)
        if let Ok(Some(data)) = self.local.get(hash).await {
            debug!("Found blob {} in local store ({} bytes)", &to_hex(hash)[..8], data.len());
            return Ok(Some(data));
        }

        // Fall back to Blossom
        match self.blossom.get(hash).await {
            Ok(Some(data)) => {
                debug!("Found blob {} in Blossom ({} bytes)", &to_hex(hash)[..8], data.len());
                // Cache locally for future requests
                match self.local.put(*hash, data.clone()).await {
                    Ok(_) => debug!("Cached blob {} locally", &to_hex(hash)[..8]),
                    Err(e) => warn!("Failed to cache blob locally: {}", e),
                }
                Ok(Some(data))
            }
            Ok(None) => {
                debug!("Blob {} not found in local or Blossom", &to_hex(hash)[..8]);
                Ok(None)
            }
            Err(e) => {
                warn!("Blossom fetch error for {}: {}", &to_hex(hash)[..8], e);
                Err(StoreError::Other(e.to_string()))
            }
        }
    }

    async fn put(&self, hash: [u8; 32], data: Vec<u8>) -> Result<bool, StoreError> {
        // FsBlobStore implements Store directly
        self.local.put(hash, data).await
    }

    async fn has(&self, hash: &[u8; 32]) -> Result<bool, StoreError> {
        // Check local first
        if self.local.has(hash).await? {
            return Ok(true);
        }

        // Check Blossom
        self.blossom
            .has(hash)
            .await
            .map_err(|e| StoreError::Other(e.to_string()))
    }

    async fn delete(&self, hash: &[u8; 32]) -> Result<bool, StoreError> {
        // Only delete from local store
        self.local.delete(hash).await
    }
}

/// Shared state for the htree server
#[derive(Clone)]
pub struct HtreeState {
    resolver: Arc<RwLock<Option<Arc<NostrRootResolver>>>>,
    store: Arc<CombinedStore>,
    root_cache: Arc<RwLock<LruCache<String, CachedRoot>>>,
}

/// Default max storage: 1GB
const DEFAULT_MAX_BYTES: u64 = 1024 * 1024 * 1024;

impl HtreeState {
    /// Create a new HtreeState with local blob store at data_dir
    pub fn new(data_dir: PathBuf) -> Self {
        // Create local blob store using FsBlobStore from hashtree-fs
        let blobs_path = data_dir.join("blobs");
        let local_store = Arc::new(
            FsBlobStore::with_max_bytes(&blobs_path, DEFAULT_MAX_BYTES)
                .expect("Failed to create blob store"),
        );

        // Create Blossom client for fetching blobs
        let keys = Keys::generate();
        let blossom_client = BlossomClient::new_empty(keys)
            .with_read_servers(DEFAULT_BLOSSOM_SERVERS.iter().map(|s| s.to_string()).collect());
        let blossom_store = Arc::new(BlossomStore::new(blossom_client));

        // Combined store: local first, then Blossom
        let store = Arc::new(CombinedStore::new(local_store, blossom_store));

        Self {
            resolver: Arc::new(RwLock::new(None)),
            store,
            root_cache: Arc::new(RwLock::new(LruCache::new(
                NonZeroUsize::new(1000).unwrap(),
            ))),
        }
    }

    fn cache_root(&self, npub: &str, tree_name: &str, cid: Cid, visibility: TreeVisibility) {
        let cache_key = format!("{}/{}", npub, tree_name);
        let mut cache = self.root_cache.write();
        cache.put(
            cache_key,
            CachedRoot {
                cid,
                visibility,
                timestamp: std::time::Instant::now(),
            },
        );
    }

    /// Initialize the Nostr resolver (called lazily on first request)
    async fn ensure_resolver(&self) -> Result<(), HtreeError> {
        // Check if already initialized
        {
            let guard = self.resolver.read();
            if guard.is_some() {
                return Ok(());
            }
        }

        info!("Initializing Nostr resolver...");
        let config = NostrResolverConfig {
            relays: DEFAULT_NOSTR_RELAYS.iter().map(|s| s.to_string()).collect(),
            resolve_timeout: Duration::from_secs(5),
            secret_key: None,
        };

        let resolver = NostrRootResolver::new(config)
            .await
            .map_err(|e| HtreeError::Resolver(e.to_string()))?;

        let mut guard = self.resolver.write();
        *guard = Some(Arc::new(resolver));
        info!("Nostr resolver initialized");
        Ok(())
    }

    /// Resolve npub/treeName to Cid
    async fn resolve_tree(&self, npub: &str, tree_name: &str) -> Result<Cid, HtreeError> {
        let cache_key = format!("{}/{}", npub, tree_name);

        // Check cache first
        let cached = {
            let cache = self.root_cache.read();
            cache.peek(&cache_key).cloned()
        };
        if let Some(entry) = cached {
            if entry.cid.key.is_some() {
                debug!("Cache hit for {}", cache_key);
                return Ok(entry.cid);
            }

            if let Ok(Some(data)) = self.store.get(&entry.cid.hash).await {
                if is_tree_node(&data) {
                    debug!("Cache hit for {}", cache_key);
                    return Ok(entry.cid);
                }
            }

            debug!("Cache entry missing key for {}, refreshing", cache_key);
        }

        // Resolve from Nostr
        self.ensure_resolver().await?;

        let key = format!("{}/{}", npub, tree_name);
        debug!("Resolving tree: {}", key);

        // Clone the resolver to avoid holding the lock across await
        let resolver = {
            let resolver_guard = self.resolver.read();
            resolver_guard
                .as_ref()
                .ok_or_else(|| HtreeError::Resolver("Resolver not initialized".into()))?
                .clone()
        };

        let cid = tokio::time::timeout(Duration::from_secs(10), resolver.resolve(&key))
            .await
            .map_err(|_| HtreeError::Resolver("Timeout resolving tree".into()))?
            .map_err(|e| HtreeError::Resolver(e.to_string()))?
            .ok_or_else(|| HtreeError::TreeNotFound(key.clone()))?;

        // Cache the result (default to Public visibility when resolved from Nostr)
        {
            let mut cache = self.root_cache.write();
            cache.put(
                cache_key,
                CachedRoot {
                    cid: cid.clone(),
                    visibility: TreeVisibility::Public,
                    timestamp: std::time::Instant::now(),
                },
            );
        }

        Ok(cid)
    }

    /// Resolve a path within a tree to get the file's Cid
    async fn resolve_path(&self, root_cid: &Cid, path: &str) -> Result<Cid, HtreeError> {
        let tree = HashTree::new(HashTreeConfig::new(self.store.clone()));

        let cid = tree
            .resolve_path(root_cid, path)
            .await
            .map_err(|e| HtreeError::Store(e.to_string()))?
            .ok_or_else(|| HtreeError::FileNotFound(path.to_string()))?;

        Ok(cid)
    }

    async fn find_thumbnail_in_dir(
        &self,
        root_cid: &Cid,
        dir_path: &str,
    ) -> Result<Option<String>, HtreeError> {
        let tree = HashTree::new(HashTreeConfig::new(self.store.clone()));

        let dir_cid = if dir_path.is_empty() {
            root_cid.clone()
        } else {
            match self.resolve_path(root_cid, dir_path).await {
                Ok(cid) => cid,
                Err(_) => return Ok(None),
            }
        };

        let entries = tree
            .list_directory(&dir_cid)
            .await
            .map_err(|e| HtreeError::Store(e.to_string()))?;

        if entries.is_empty() {
            debug!("No entries found while searching thumbnail in '{}'", dir_path);
        }

        for pattern in THUMBNAIL_PATTERNS {
            if entries.iter().any(|e| e.name == *pattern) {
                let path = if dir_path.is_empty() {
                    (*pattern).to_string()
                } else {
                    format!("{}/{}", dir_path, pattern)
                };
                return Ok(Some(path));
            }
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
                    key: entry.key.clone(),
                };

                let sub_entries = match tree.list_directory(&sub_cid).await {
                    Ok(entries) => entries,
                    Err(_) => continue,
                };

                for pattern in THUMBNAIL_PATTERNS {
                    if sub_entries.iter().any(|e| e.name == *pattern) {
                        let prefix = if dir_path.is_empty() {
                            entry.name.clone()
                        } else {
                            format!("{}/{}", dir_path, entry.name)
                        };
                        return Ok(Some(format!("{}/{}", prefix, pattern)));
                    }
                }
            }
        }

        Ok(None)
    }

    /// Read file content from a Cid
    async fn read_file(&self, cid: &Cid) -> Result<Vec<u8>, HtreeError> {
        let tree = HashTree::new(HashTreeConfig::new(self.store.clone()));

        tree.get(cid)
            .await
            .map_err(|e| HtreeError::Store(e.to_string()))?
            .ok_or_else(|| HtreeError::FileNotFound(to_hex(&cid.hash)))
    }

    /// Read a byte range from a file (fetches only necessary chunks)
    /// This is more efficient than read_file() for partial reads of large files.
    async fn read_file_range(
        &self,
        cid: &Cid,
        start: u64,
        end: Option<u64>,
    ) -> Result<Vec<u8>, HtreeError> {
        let tree = HashTree::new(HashTreeConfig::new(self.store.clone()));

        tree.read_file_range(&cid.hash, start, end)
            .await
            .map_err(|e| HtreeError::Store(e.to_string()))?
            .ok_or_else(|| HtreeError::FileNotFound(to_hex(&cid.hash)))
    }

    /// Get the total size of a file without loading all its content
    /// Handles encrypted files by decrypting the root node to read the tree structure
    async fn get_file_size(&self, cid: &Cid) -> Result<u64, HtreeError> {
        // Get raw data from store
        let data = self
            .store
            .get(&cid.hash)
            .await
            .map_err(|e| HtreeError::Store(e.to_string()))?
            .ok_or_else(|| HtreeError::FileNotFound(to_hex(&cid.hash)))?;

        // Decrypt if key is present
        let data = if let Some(key) = &cid.key {
            decrypt_chk(&data, key).map_err(|e| HtreeError::Store(e.to_string()))?
        } else {
            data
        };

        // If not a tree node, return raw size
        if !is_tree_node(&data) {
            return Ok(data.len() as u64);
        }

        // Parse tree node and sum children's sizes
        let node = decode_tree_node(&data).map_err(|e| HtreeError::Store(e.to_string()))?;
        let total: u64 = node.links.iter().map(|link| link.size).sum();
        Ok(total)
    }

    /// Resolve nhash to Cid and mime type (without reading content)
    async fn resolve_nhash(
        &self,
        nhash: &str,
        filename: Option<&str>,
    ) -> Result<(Cid, String), HtreeError> {
        debug!("Resolving nhash: {}", nhash);

        let nhash_data =
            nhash_decode(nhash).map_err(|e| HtreeError::InvalidPath(e.to_string()))?;

        // Convert NHashData to Cid
        let cid = Cid {
            hash: nhash_data.hash,
            key: nhash_data.decrypt_key,
        };

        // If nhash has a path, resolve it
        let file_cid = if !nhash_data.path.is_empty() {
            let path = nhash_data.path.join("/");
            self.resolve_path(&cid, &path).await?
        } else {
            cid
        };

        let mime_type = guess_mime_type(filename.unwrap_or("file"));
        Ok((file_cid, mime_type.to_string()))
    }

    /// Resolve npub path to Cid and mime type (without reading content)
    async fn resolve_npub(
        &self,
        npub: &str,
        tree_name: &str,
        file_path: &str,
    ) -> Result<(Cid, String), HtreeError> {
        let mut tree_name = tree_name.to_string();
        let mut file_path = file_path.to_string();
        debug!(
            "Resolving npub path: npub={}, tree={}, path={}",
            &npub[..16.min(npub.len())],
            tree_name,
            file_path
        );

        // Resolve tree root
        let root_cid = match self.resolve_tree(npub, &tree_name).await {
            Ok(cid) => cid,
            Err(HtreeError::TreeNotFound(_)) if !file_path.is_empty() => {
                let mut parts = file_path.splitn(2, '/');
                let first = parts.next().unwrap_or("");
                let rest = parts.next().unwrap_or("");

                if first.is_empty() {
                    return Err(HtreeError::TreeNotFound(format!("{}/{}", npub, tree_name)));
                }

                let alt_tree_name = format!("{}/{}", tree_name, first);
                match self.resolve_tree(npub, &alt_tree_name).await {
                    Ok(cid) => {
                        tree_name = alt_tree_name;
                        file_path = rest.to_string();
                        cid
                    }
                    Err(HtreeError::TreeNotFound(_)) => {
                        return Err(HtreeError::TreeNotFound(format!("{}/{}", npub, tree_name)));
                    }
                    Err(e) => return Err(e),
                }
            }
            Err(e) => return Err(e),
        };
        debug!("Resolved tree root: {}", to_hex(&root_cid.hash));
        debug!(
            "Resolved tree key: {}",
            root_cid
                .key
                .as_ref()
                .map(|key| to_hex(key))
                .unwrap_or_else(|| "none".to_string())
        );

        let resolved_path = if is_thumbnail_request(&file_path) {
            let dir_path = file_path.strip_suffix("/thumbnail").unwrap_or("");
            self.find_thumbnail_in_dir(&root_cid, dir_path)
                .await?
                .unwrap_or_else(|| file_path.to_string())
        } else {
            file_path.to_string()
        };

        // Navigate to file if path is provided
        let file_cid = if resolved_path.is_empty() {
            root_cid
        } else {
            self.resolve_path(&root_cid, &resolved_path).await?
        };

        let mime_type = guess_mime_type(if resolved_path.is_empty() {
            &tree_name
        } else {
            &resolved_path
        });

        Ok((file_cid, mime_type.to_string()))
    }
}

// Remove Default impl - HtreeState now requires data_dir

/// Parse Range header value like "bytes=0-999" or "bytes=500-"
fn parse_range_header(range_header: &str, total_size: usize) -> Option<(usize, usize)> {
    if total_size == 0 {
        return None;
    }
    let range = range_header.strip_prefix("bytes=")?;
    let parts: Vec<&str> = range.split('-').collect();
    if parts.len() != 2 {
        return None;
    }

    let start: usize = if parts[0].is_empty() {
        // Suffix range like "-500" means last 500 bytes
        let suffix_len: usize = parts[1].parse().ok()?;
        total_size.saturating_sub(suffix_len)
    } else {
        parts[0].parse().ok()?
    };

    let end: usize = if parts[1].is_empty() {
        // Open-ended range like "500-" means from 500 to end
        total_size - 1
    } else {
        parts[1].parse().ok()?
    };

    // Validate range
    if start > end || start >= total_size {
        return None;
    }

    // Clamp end to file size
    let end = end.min(total_size - 1);

    Some((start, end))
}

async fn read_range_or_full(
    state: &HtreeState,
    file_cid: &Cid,
    range_header: Option<&str>,
) -> Result<(Vec<u8>, Option<(usize, usize, usize)>), HtreeError> {
    if let Some(range_str) = range_header {
        if file_cid.key.is_some() {
            let data = state.read_file(file_cid).await?;
            let total_size = data.len();
            if let Some((start, end)) = parse_range_header(range_str, total_size) {
                return Ok((data[start..end + 1].to_vec(), Some((start, end, total_size))));
            }
            return Ok((data, None));
        }

        let total_size = state.get_file_size(file_cid).await? as usize;
        if let Some((start, end)) = parse_range_header(range_str, total_size) {
            let data = state
                .read_file_range(file_cid, start as u64, Some((end + 1) as u64))
                .await?;
            return Ok((data, Some((start, end, total_size))));
        }
    }

    let data = state.read_file(file_cid).await?;
    Ok((data, None))
}

// Axum handler for /htree/*path - catches all htree requests
// Now supports efficient range requests that only fetch needed chunks
#[axum::debug_handler]
async fn handle_htree_request(
    State(state): State<HtreeState>,
    headers: HeaderMap,
    uri: OriginalUri,
) -> Response {
    // Get raw path from URI (preserves percent-encoding)
    let raw_path = uri.path();
    // Strip the /htree/ prefix
    let path = raw_path.strip_prefix("/htree/").unwrap_or(raw_path);
    debug!("htree request: raw_path={}, path={}", raw_path, path);

    // First resolve the path to get CID and mime type (without loading file content)
    let (file_cid, content_type) = match resolve_htree_inner(&state, &path).await {
        Ok(result) => result,
        Err(e) => return e.into_response(),
    };

    let range_header = headers.get(header::RANGE).and_then(|h| h.to_str().ok());
    let (data, range_info) = match read_range_or_full(&state, &file_cid, range_header).await {
        Ok(result) => result,
        Err(e) => return e.into_response(),
    };

    if let Some((start, end, total_size)) = range_info {
        let content_length = data.len();
        let content_range = format!("bytes {}-{}/{}", start, end, total_size);

        debug!(
            "htree range response: {} bytes (range {}-{}/{}), type={}",
            content_length, start, end, total_size, content_type
        );

        return Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header(header::CONTENT_TYPE, content_type)
            .header(header::CONTENT_LENGTH, content_length)
            .header(header::CONTENT_RANGE, content_range)
            .header(header::ACCEPT_RANGES, "bytes")
            .body(Body::from(data))
            .unwrap();
    }

    info!("htree response: {} bytes, type={}", data.len(), content_type);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, data.len())
        .header(header::ACCEPT_RANGES, "bytes")
        .body(Body::from(data))
        .unwrap()
}

/// URL-decode a string (percent-decode)
fn url_decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s)
        .decode_utf8_lossy()
        .into_owned()
}

/// Resolve path to Cid and mime type without loading file content.
/// This is used for efficient range requests where we need to know the file
/// before deciding how much to read.
async fn resolve_htree_inner(
    state: &HtreeState,
    path: &str,
) -> Result<(Cid, String), HtreeError> {
    let path = path.trim_start_matches('/');
    let parts: Vec<&str> = path.splitn(2, '/').collect();

    if parts.is_empty() || parts[0].is_empty() {
        return Err(HtreeError::InvalidPath("Empty path".into()));
    }

    let first = parts[0];
    let rest = parts.get(1).copied().unwrap_or("");

    if first.starts_with("nhash1") {
        let filename = if rest.is_empty() {
            None
        } else {
            Some(url_decode(rest))
        };
        state.resolve_nhash(first, filename.as_deref()).await
    } else if is_npub(first) {
        let rest_parts: Vec<&str> = rest.splitn(2, '/').collect();
        let tree_name_encoded = rest_parts.first().ok_or_else(|| {
            HtreeError::InvalidPath("Missing tree name in npub path".into())
        })?;
        if tree_name_encoded.is_empty() {
            return Err(HtreeError::InvalidPath("Empty tree name".into()));
        }
        let tree_name = url_decode(tree_name_encoded);
        let file_path = rest_parts
            .get(1)
            .map(|p| url_decode(p))
            .unwrap_or_default();

        state.resolve_npub(first, &tree_name, &file_path).await
    } else {
        Err(HtreeError::InvalidPath(format!(
            "Path must start with npub or nhash: {}",
            first
        )))
    }
}

/// Global server port - set when server starts
static SERVER_PORT: once_cell::sync::OnceCell<u16> = once_cell::sync::OnceCell::new();
static APP_HANDLE: once_cell::sync::OnceCell<AppHandle> = once_cell::sync::OnceCell::new();

pub fn set_app_handle(app: AppHandle) {
    let _ = APP_HANDLE.set(app);
}

/// Get the htree server port (if running)
pub fn get_server_port() -> Option<u16> {
    SERVER_PORT.get().copied()
}

/// Handle NIP-07 HTTP requests from webviews
async fn handle_nip07_request(
    headers: HeaderMap,
    Json(request): Json<crate::nip07::Nip07Request>,
) -> impl IntoResponse {
    info!(
        "[NIP-07 HTTP] Request: method={} origin={}",
        request.method, request.origin
    );

    // Get session token from header
    let session_token = match headers.get("x-session-token").and_then(|v| v.to_str().ok()) {
        Some(token) => token,
        None => {
            warn!("[NIP-07 HTTP] Missing session token");
            return (
                StatusCode::UNAUTHORIZED,
                Json(crate::nip07::Nip07Response {
                    result: None,
                    error: Some("Missing session token".to_string()),
                }),
            );
        }
    };

    // Get global state
    let nip07_state = match crate::nip07::get_nip07_state() {
        Some(state) => state,
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(crate::nip07::Nip07Response {
                    result: None,
                    error: Some("NIP-07 state not initialized".to_string()),
                }),
            );
        }
    };

    let worker_state = match crate::nip07::get_worker_state() {
        Some(state) => state,
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(crate::nip07::Nip07Response {
                    result: None,
                    error: Some("Worker state not initialized".to_string()),
                }),
            );
        }
    };

    // Validate session token for this origin
    if !nip07_state.validate_token(&request.origin, session_token) {
        return (
            StatusCode::FORBIDDEN,
            Json(crate::nip07::Nip07Response {
                result: None,
                error: Some("Invalid session token".to_string()),
            }),
        );
    }

    // Process the NIP-07 request
    let response = crate::nip07::handle_nip07_request(
        &worker_state,
        Some(&nip07_state.permissions),
        &request.method,
        &request.params,
        &request.origin,
    )
    .await;

    (StatusCode::OK, Json(response))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WebviewEventRequest {
    kind: String,
    label: String,
    origin: String,
    url: Option<String>,
    source: Option<String>,
    action: Option<String>,
}

async fn handle_webview_event(
    headers: HeaderMap,
    Json(request): Json<WebviewEventRequest>,
) -> impl IntoResponse {
    let session_token = match headers.get("x-session-token").and_then(|v| v.to_str().ok()) {
        Some(token) => token,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "Missing session token" })),
            );
        }
    };

    let nip07_state = match crate::nip07::get_nip07_state() {
        Some(state) => state,
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "NIP-07 state not initialized" })),
            );
        }
    };

    if !nip07_state.validate_token(&request.origin, session_token) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "Invalid session token" })),
        );
    }

    let app_handle = match APP_HANDLE.get() {
        Some(handle) => handle.clone(),
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "App handle not initialized" })),
            );
        }
    };

    match request.kind.as_str() {
        "location" => {
            let url = match request.url {
                Some(url) => url,
                None => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "error": "Missing url" })),
                    );
                }
            };
            let source = request.source.unwrap_or_else(|| "unknown".to_string());
            let _ = app_handle.emit(
                "child-webview-location",
                json!({
                    "label": request.label,
                    "url": url,
                    "source": source
                }),
            );
        }
        "navigate" => {
            let action = match request.action.as_deref() {
                Some("back") => "back",
                Some("forward") => "forward",
                _ => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "error": "Invalid action" })),
                    );
                }
            };
            let _ = app_handle.emit(
                "child-webview-navigate",
                json!({
                    "label": request.label,
                    "action": action
                }),
            );
        }
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "Invalid event kind" })),
            );
        }
    }

    (StatusCode::OK, Json(json!({ "ok": true })))
}

/// Start the htree HTTP server
/// Returns the port number the server is listening on
/// data_dir is the Tauri app data directory where blobs are stored
pub async fn start_server(data_dir: PathBuf) -> Result<u16, HtreeError> {
    // Bind to a fixed port on localhost for predictable URL
    let listener = TcpListener::bind(("127.0.0.1", 21417))
        .await
        .map_err(|e| HtreeError::Io(e.to_string()))?;
    start_server_with_listener(data_dir, listener).await
}

/// Start the htree HTTP server on a specific port (use 0 for ephemeral)
pub async fn start_server_on_port(data_dir: PathBuf, port: u16) -> Result<u16, HtreeError> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| HtreeError::Io(e.to_string()))?;
    start_server_with_listener(data_dir, listener).await
}

async fn start_server_with_listener(
    data_dir: PathBuf,
    listener: TcpListener,
) -> Result<u16, HtreeError> {
    let state = GLOBAL_HTREE_STATE
        .get_or_init(|| HtreeState::new(data_dir.clone()))
        .clone();

    // CORS configuration to allow requests from the Tauri app
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
        .expose_headers([
            header::ACCEPT_RANGES,
            header::CONTENT_RANGE,
            header::CONTENT_LENGTH,
            header::CONTENT_TYPE,
        ]);

    // Create relay proxy state
    let relay_state = RelayProxyState::new();

    // Build the combined app with htree, relay, and nip07 routes
    let htree_router = Router::new()
        .route("/htree/{*path}", get(handle_htree_request))
        .with_state(state);

    let relay_router = Router::new()
        .route("/relay", any(handle_relay_websocket))
        .with_state(relay_state);

    let nip07_router = Router::new().route("/nip07", post(handle_nip07_request));
    let webview_router = Router::new().route("/webview", post(handle_webview_event));

    let app = htree_router
        .merge(relay_router)
        .merge(nip07_router)
        .merge(webview_router)
        .layer(cors);

    let addr = listener
        .local_addr()
        .map_err(|e| HtreeError::Io(e.to_string()))?;

    let port = addr.port();
    SERVER_PORT.set(port).ok();

    info!("htree server listening on http://127.0.0.1:{}", port);

    // Spawn the server in the background
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            error!("htree server error: {}", e);
        }
    });

    Ok(port)
}

/// Tauri command to get the htree server URL
/// Returns the custom protocol URL for htree:// scheme
#[tauri::command]
pub fn get_htree_server_url() -> Option<String> {
    // Return fixed HTTP server URL - bind uses IPv4 loopback
    Some("http://127.0.0.1:21417".to_string())
}

/// Cache tree roots from the frontend for faster /thumbnail resolution.
#[tauri::command]
pub fn cache_tree_root(
    npub: String,
    tree_name: String,
    hash: String,
    key: Option<String>,
    visibility: Option<String>,
) -> Result<(), String> {
    let hash = from_hex(&hash).map_err(|_| "Invalid hash".to_string())?;
    let key = match key {
        Some(value) if !value.is_empty() => Some(from_hex(&value).map_err(|_| "Invalid key".to_string())?),
        _ => None,
    };
    let cid = Cid { hash, key };
    let visibility = TreeVisibility::from_str(visibility.as_deref().unwrap_or("public"));
    let state = GLOBAL_HTREE_STATE
        .get()
        .ok_or_else(|| "htree state not initialized".to_string())?;
    state.cache_root(&npub, &tree_name, cid, visibility);
    Ok(())
}

// Global state for URI scheme protocol handler
static GLOBAL_HTREE_STATE: once_cell::sync::OnceCell<HtreeState> = once_cell::sync::OnceCell::new();

/// Initialize the global htree state for the URI scheme protocol
pub fn init_htree_state(data_dir: PathBuf) {
    let _ = GLOBAL_HTREE_STATE.get_or_init(|| HtreeState::new(data_dir));
}

/// Handle htree:// URI scheme protocol requests
/// This is called by Tauri's register_uri_scheme_protocol
pub fn handle_htree_protocol<R: tauri::Runtime>(
    _ctx: tauri::UriSchemeContext<'_, R>,
    request: tauri::http::Request<Vec<u8>>,
) -> tauri::http::Response<Vec<u8>> {
    let uri = request.uri();
    let raw_path = uri.path();

    // Strip /htree/ prefix if present (frontend adds it for consistency with web)
    let path_with_query = raw_path
        .strip_prefix("/htree/")
        .or_else(|| raw_path.strip_prefix("/htree"))
        .unwrap_or(raw_path);

    // Strip query string if present (custom URI schemes may include it in path)
    // Query string might be URL-encoded as %3F
    let path = path_with_query
        .split('?')
        .next()
        .unwrap_or(path_with_query)
        .split("%3F")
        .next()
        .unwrap_or(path_with_query)
        .split("%3f")
        .next()
        .unwrap_or(path_with_query);

    let range_header = request.headers().get("range").and_then(|v| v.to_str().ok());

    info!("htree:// protocol request: raw_path={}, path={}", raw_path, path);

    // Get global state
    let state = match GLOBAL_HTREE_STATE.get() {
        Some(s) => s,
        None => {
            return tauri::http::Response::builder()
                .status(500)
                .body(b"htree state not initialized".to_vec())
                .unwrap();
        }
    };

    // Use tokio runtime to run async code with efficient range support
    let result = tauri::async_runtime::block_on(async {
        // First resolve the path to get CID and mime type (without loading file content)
        let (file_cid, content_type) = resolve_htree_inner(state, path).await?;

        let (data, range_info) = read_range_or_full(state, &file_cid, range_header).await?;
        Ok((content_type, data, range_info))
    });

    match result {
        Ok((content_type, data, range_info)) => {
            if let Some((start, end, total_size)) = range_info {
                let content_length = data.len();
                let content_range = format!("bytes {}-{}/{}", start, end, total_size);
                info!("htree:// protocol 206 response: range={}", content_range);

                return tauri::http::Response::builder()
                    .status(206)
                    .header("content-type", content_type)
                    .header("content-length", content_length.to_string())
                    .header("content-range", content_range)
                    .header("accept-ranges", "bytes")
                    .body(data)
                    .unwrap();
            }

            info!("htree:// protocol success: path={}, content_type={}, size={}", path, content_type, data.len());

            // Full response
            tauri::http::Response::builder()
                .status(200)
                .header("content-type", content_type)
                .header("content-length", data.len().to_string())
                .header("accept-ranges", "bytes")
                .body(data)
                .unwrap()
        }
        Err(e) => {
            error!("htree:// protocol error for {}: {}", path, e);
            let (status, message) = match &e {
                HtreeError::FileNotFound(msg) | HtreeError::TreeNotFound(msg) => (404, msg.clone()),
                HtreeError::InvalidPath(msg) => (400, msg.clone()),
                _ => (500, e.to_string()),
            };
            tauri::http::Response::builder()
                .status(status)
                .header("content-type", "text/plain")
                .body(message.into_bytes())
                .unwrap()
        }
    }
}
