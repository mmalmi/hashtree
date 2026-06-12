use std::sync::{Arc, OnceLock};

use futures::io::AllowStdIo;
use futures::StreamExt;
use hashtree_blossom::{BlossomClient, BlossomStore};
use hashtree_core::{
    nhash_decode, nhash_encode_full, Cid, HashTree, HashTreeConfig, MemoryStore, NHashData, Store,
};
use nostr::Keys;
use tokio::io::AsyncWriteExt;

/// FFI-friendly hashtree error type.
#[derive(Debug, Clone, thiserror::Error, uniffi::Error)]
pub enum HashtreeError {
    #[error("{0}")]
    Message(String),
}

fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to initialize hashtree_ffi runtime")
    })
}

#[uniffi::export]
pub fn hashtree_nhash_from_file(file_path: String) -> Result<String, HashtreeError> {
    runtime().block_on(nhash_from_file_impl(&file_path))
}

#[uniffi::export]
pub fn hashtree_upload_file(
    privkey_hex: String,
    file_path: String,
    read_servers: Vec<String>,
    write_servers: Vec<String>,
) -> Result<String, HashtreeError> {
    runtime().block_on(upload_file_impl(
        &privkey_hex,
        &file_path,
        read_servers,
        write_servers,
    ))
}

#[uniffi::export]
pub fn hashtree_download_bytes(
    nhash: String,
    read_servers: Vec<String>,
) -> Result<Vec<u8>, HashtreeError> {
    runtime().block_on(download_bytes_impl(&nhash, read_servers))
}

#[uniffi::export]
pub fn hashtree_download_to_file(
    nhash: String,
    output_path: String,
    read_servers: Vec<String>,
) -> Result<(), HashtreeError> {
    runtime().block_on(download_to_file_impl(&nhash, &output_path, read_servers))
}

async fn nhash_from_file_impl(_file_path: &str) -> Result<String, HashtreeError> {
    let (cid, _store) = put_file_to_memory(_file_path).await?;
    encode_nhash_from_cid(&cid)
}

async fn upload_file_impl(
    privkey_hex: &str,
    file_path: &str,
    read_servers: Vec<String>,
    write_servers: Vec<String>,
) -> Result<String, HashtreeError> {
    let (cid, local_store) = put_file_to_memory(file_path).await?;
    let client = build_upload_client(privkey_hex, read_servers, write_servers)?;

    let mut hashes = local_store.keys();
    hashes.sort();

    for hash in hashes {
        let data = local_store
            .get(&hash)
            .await
            .map_err(|e| HashtreeError::Message(format!("failed to read local blob: {}", e)))?
            .ok_or_else(|| {
                HashtreeError::Message("blob missing from local hashtree upload store".to_string())
            })?;

        client
            .upload_if_missing(&data)
            .await
            .map_err(|e| HashtreeError::Message(format!("blossom upload failed: {}", e)))?;
    }

    encode_nhash_from_cid(&cid)
}

async fn download_bytes_impl(
    nhash: &str,
    read_servers: Vec<String>,
) -> Result<Vec<u8>, HashtreeError> {
    let cid = parse_nhash_to_cid(nhash)?;
    let store = build_download_store(read_servers)?;
    let tree = HashTree::new(HashTreeConfig::new(store));

    tree.get(&cid, None)
        .await
        .map_err(|e| HashtreeError::Message(format!("failed to download attachment: {}", e)))?
        .ok_or_else(|| HashtreeError::Message("attachment not found".to_string()))
}

async fn download_to_file_impl(
    nhash: &str,
    output_path: &str,
    read_servers: Vec<String>,
) -> Result<(), HashtreeError> {
    let cid = parse_nhash_to_cid(nhash)?;
    let store = build_download_store(read_servers)?;
    let tree = HashTree::new(HashTreeConfig::new(store));

    let output = std::path::Path::new(output_path);
    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| {
                HashtreeError::Message(format!(
                    "failed to create output directory {}: {}",
                    parent.display(),
                    e
                ))
            })?;
        }
    }

    let mut file = tokio::fs::File::create(output)
        .await
        .map_err(|e| HashtreeError::Message(format!("failed to create output file: {}", e)))?;

    let mut wrote_any_chunk = false;
    let mut stream = tree.get_stream(&cid);
    while let Some(chunk) = stream.next().await {
        wrote_any_chunk = true;
        let chunk = chunk
            .map_err(|e| HashtreeError::Message(format!("failed to stream attachment: {}", e)))?;
        file.write_all(&chunk)
            .await
            .map_err(|e| HashtreeError::Message(format!("failed to write output file: {}", e)))?;
    }

    if !wrote_any_chunk {
        return Err(HashtreeError::Message("attachment not found".to_string()));
    }

    file.flush()
        .await
        .map_err(|e| HashtreeError::Message(format!("failed to flush output file: {}", e)))?;

    Ok(())
}

fn parse_nhash_to_cid(nhash: &str) -> Result<Cid, HashtreeError> {
    let decoded = nhash_decode(nhash)
        .map_err(|e| HashtreeError::Message(format!("invalid nhash attachment id: {}", e)))?;
    Ok(Cid {
        hash: decoded.hash,
        key: decoded.decrypt_key,
    })
}

fn encode_nhash_from_cid(cid: &Cid) -> Result<String, HashtreeError> {
    nhash_encode_full(&NHashData {
        hash: cid.hash,
        decrypt_key: cid.key,
    })
    .map_err(|e| HashtreeError::Message(format!("failed to encode nhash: {}", e)))
}

fn normalize_servers(servers: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    for server in servers {
        let trimmed = server.trim();
        if trimmed.is_empty() {
            continue;
        }
        let normalized = trimmed.trim_end_matches('/').to_string();
        if !out.iter().any(|existing| existing == &normalized) {
            out.push(normalized);
        }
    }
    out
}

fn build_upload_client(
    privkey_hex: &str,
    read_servers: Vec<String>,
    write_servers: Vec<String>,
) -> Result<BlossomClient, HashtreeError> {
    let keys = Keys::parse(privkey_hex).map_err(|e| {
        HashtreeError::Message(format!(
            "invalid blossom signing key (hex or nsec expected): {}",
            e
        ))
    })?;

    let read = normalize_servers(read_servers);
    let mut write = normalize_servers(write_servers);
    if write.is_empty() && !read.is_empty() {
        write = read.clone();
    }

    let mut client = BlossomClient::new_empty(keys);
    if !read.is_empty() {
        client = client.with_read_servers(read);
    }
    if !write.is_empty() {
        client = client.with_write_servers(write);
    }

    if client.write_servers().is_empty() {
        return Err(HashtreeError::Message(
            "no write servers provided for blossom upload".to_string(),
        ));
    }

    Ok(client)
}

fn build_download_store(read_servers: Vec<String>) -> Result<Arc<BlossomStore>, HashtreeError> {
    let read = normalize_servers(read_servers);
    if read.is_empty() {
        return Err(HashtreeError::Message(
            "no read servers provided for attachment download".to_string(),
        ));
    }

    let client = BlossomClient::new_empty(Keys::generate()).with_read_servers(read);
    Ok(Arc::new(BlossomStore::new(client)))
}

async fn put_file_to_memory(file_path: &str) -> Result<(Cid, MemoryStore), HashtreeError> {
    let file = std::fs::File::open(file_path)
        .map_err(|e| HashtreeError::Message(format!("failed to open {}: {}", file_path, e)))?;

    let store = MemoryStore::new();
    let tree = HashTree::new(HashTreeConfig::new(Arc::new(store.clone())));
    let (cid, _size) = tree
        .put_stream(AllowStdIo::new(file))
        .await
        .map_err(|e| HashtreeError::Message(format!("failed to hash/encrypt file: {}", e)))?;

    Ok((cid, store))
}

uniffi::setup_scaffolding!();

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Bytes,
        extract::{DefaultBodyLimit, Path as AxumPath, State},
        http::{header, HeaderMap, StatusCode},
        response::IntoResponse,
        routing::{get, put},
        Router,
    };
    use hashtree_blossom::compute_sha256;
    use nostr::Keys;
    use std::{
        collections::HashMap,
        sync::{Arc, RwLock},
    };
    use tempfile::TempDir;

    #[derive(Clone, Default)]
    struct TestServerState {
        blobs: Arc<RwLock<HashMap<String, Vec<u8>>>>,
    }

    impl TestServerState {
        fn blob_count(&self) -> usize {
            self.blobs.read().unwrap().len()
        }
    }

    async fn upload_blob(State(state): State<TestServerState>, body: Bytes) -> impl IntoResponse {
        let hash = compute_sha256(&body);
        let mut blobs = state.blobs.write().unwrap();
        if blobs.contains_key(&hash) {
            return StatusCode::OK;
        }
        blobs.insert(hash, body.to_vec());
        StatusCode::CREATED
    }

    async fn get_blob(
        AxumPath(blob): AxumPath<String>,
        State(state): State<TestServerState>,
    ) -> impl IntoResponse {
        let hash = blob.strip_suffix(".bin").unwrap_or(blob.as_str());
        let blobs = state.blobs.read().unwrap();
        if let Some(data) = blobs.get(hash) {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/octet-stream"),
            );
            headers.insert(
                header::CONTENT_LENGTH,
                header::HeaderValue::from_str(&data.len().to_string()).unwrap(),
            );
            return (StatusCode::OK, headers, data.clone()).into_response();
        }

        StatusCode::NOT_FOUND.into_response()
    }

    async fn head_blob(
        AxumPath(blob): AxumPath<String>,
        State(state): State<TestServerState>,
    ) -> impl IntoResponse {
        let hash = blob.strip_suffix(".bin").unwrap_or(blob.as_str());
        let blobs = state.blobs.read().unwrap();
        if let Some(data) = blobs.get(hash) {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/octet-stream"),
            );
            headers.insert(
                header::CONTENT_LENGTH,
                header::HeaderValue::from_str(&data.len().to_string()).unwrap(),
            );
            return (StatusCode::OK, headers).into_response();
        }

        StatusCode::NOT_FOUND.into_response()
    }

    async fn start_test_server() -> (String, TestServerState) {
        let state = TestServerState::default();
        let app = Router::new()
            .route("/upload", put(upload_blob))
            .route("/:blob", get(get_blob).head(head_blob))
            .layer(DefaultBodyLimit::disable())
            .with_state(state.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        (format!("http://{}", addr), state)
    }

    fn write_fixture_file(temp_dir: &TempDir) -> (String, Vec<u8>) {
        let file_path = temp_dir.path().join("fixture.bin");
        let mut data = vec![0u8; 5_100_123];
        for (i, b) in data.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        std::fs::write(&file_path, &data).unwrap();
        (file_path.to_string_lossy().to_string(), data)
    }

    #[tokio::test]
    async fn nhash_from_file_is_deterministic() {
        let temp_dir = tempfile::tempdir().unwrap();
        let (file_path, _) = write_fixture_file(&temp_dir);

        let first = nhash_from_file_impl(&file_path).await.unwrap();
        let second = nhash_from_file_impl(&file_path).await.unwrap();
        assert_eq!(first, second);
        assert!(first.starts_with("nhash1"));
    }

    #[tokio::test]
    async fn upload_and_download_chunked_file_roundtrips() {
        let (server_url, state) = start_test_server().await;
        let temp_dir = tempfile::tempdir().unwrap();
        let (file_path, data) = write_fixture_file(&temp_dir);

        let keys = Keys::generate();
        let nhash = upload_file_impl(
            &keys.secret_key().to_secret_hex(),
            &file_path,
            vec![server_url.clone()],
            vec![server_url.clone()],
        )
        .await
        .unwrap();

        let downloaded = download_bytes_impl(&nhash, vec![server_url]).await.unwrap();
        assert_eq!(downloaded, data);
        assert!(
            state.blob_count() > 1,
            "expected chunked upload to produce multiple blobs"
        );
    }

    #[tokio::test]
    async fn download_to_file_writes_exact_bytes() {
        let (server_url, _state) = start_test_server().await;
        let temp_dir = tempfile::tempdir().unwrap();
        let (file_path, data) = write_fixture_file(&temp_dir);

        let keys = Keys::generate();
        let nhash = upload_file_impl(
            &keys.secret_key().to_secret_hex(),
            &file_path,
            vec![server_url.clone()],
            vec![server_url.clone()],
        )
        .await
        .unwrap();

        let out_path = temp_dir.path().join("downloaded.bin");
        download_to_file_impl(&nhash, &out_path.to_string_lossy(), vec![server_url])
            .await
            .unwrap();

        let written = std::fs::read(out_path).unwrap();
        assert_eq!(written, data);
    }
}
