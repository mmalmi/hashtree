use super::*;

async fn start_upstream(store: Arc<HashtreeStore>) -> (String, tokio::task::JoinHandle<()>) {
    let router = Router::new()
        .route("/:id", get(serve_blob_for_test))
        .with_state(store);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (format!("http://{address}"), server)
}

fn tiny_store(path: &std::path::Path) -> Arc<HashtreeStore> {
    Arc::new(HashtreeStore::new_with_backend(path, hashtree_config::StorageBackend::Fs, 1).unwrap())
}

#[tokio::test]
async fn valid_upstream_blob_serves_a_raw_range_when_disk_cache_rejects_it() {
    let temp = TempDir::new().unwrap();
    let source = Arc::new(HashtreeStore::new(temp.path().join("source")).unwrap());
    let local = tiny_store(&temp.path().join("local"));
    let bytes: Vec<u8> = (0..350_000).map(|offset| (offset % 251) as u8).collect();
    let hash = sha256(&bytes);
    source.put_blob(&bytes).unwrap();
    assert!(local.put_cached_blob(&bytes).is_err());
    let (upstream, server) = start_upstream(source).await;
    let state = test_app_state(local.clone(), vec![upstream]);

    let mut headers = HeaderMap::new();
    headers.insert(
        header::RANGE,
        header::HeaderValue::from_static("bytes=32-47"),
    );
    let response = serve_raw_blob_with_range(&state, hash, headers, true, true, false, true).await;

    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        response.headers()[header::CONTENT_RANGE],
        "bytes 32-47/350000"
    );
    let body = to_bytes(response.into_body(), 16).await.unwrap();
    assert_eq!(body.as_ref(), &bytes[32..48]);
    assert!(local.get_blob(&hash).unwrap().is_none());
    server.abort();
}

#[tokio::test]
async fn allowed_encrypted_tree_serves_cold_file_when_disk_cache_rejects_its_blobs() {
    let temp = TempDir::new().unwrap();
    let source = Arc::new(HashtreeStore::new(temp.path().join("source")).unwrap());
    let local = tiny_store(&temp.path().join("local"));
    let source_tree = HashTree::new(HashTreeConfig::new(source.store_arc()));
    let mut data = b"export default 'read directly from the verified upstream bytes';\n".to_vec();
    data.resize(400_000, b' ');
    let (entry, _) = source_tree.put(&data).await.unwrap();
    let root = source_tree
        .put_directory(vec![
            DirEntry::from_cid("script.js", &entry).with_link_type(LinkType::File)
        ])
        .await
        .unwrap();
    let root_blob = source.get_blob(&root.hash).unwrap().unwrap();
    let file_blob = source.get_blob(&entry.hash).unwrap().unwrap();
    assert!(local.put_cached_blob(&root_blob).is_err());
    let (upstream, server) = start_upstream(source).await;
    let mut state = test_app_state(local.clone(), vec![upstream]);
    state.require_random_untrusted_ingest = true;
    let npub = allow_plaintext_read_author(&mut state, &Keys::generate());
    put_cached_tree_root(
        &state,
        tree_root_cache_key(&npub, "site", None),
        root.clone(),
        "cache",
        None,
    );

    let response = htree_npub_impl(
        &state,
        MutableTreeRequest {
            npub,
            treename: "site".to_string(),
            path: Some("script.js".to_string()),
            params: HashMap::new(),
            headers: HeaderMap::new(),
            is_localhost: true,
            head_only: false,
        },
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), data.len()).await.unwrap();
    assert_eq!(body.as_ref(), data.as_slice());
    assert!(local.get_blob(&root.hash).unwrap().is_none());
    assert!(local.get_blob(&entry.hash).unwrap().is_none());

    let raw = serve_raw_blob_with_range(
        &state,
        entry.hash,
        HeaderMap::new(),
        true,
        false,
        false,
        true,
    )
    .await;
    assert_eq!(raw.status(), StatusCode::OK);
    let raw_body = to_bytes(raw.into_body(), file_blob.len()).await.unwrap();
    assert_eq!(raw_body.as_ref(), file_blob.as_slice());
    assert_ne!(raw_body.as_ref(), data.as_slice());

    let blocked = serve_tree_root_response(
        &state,
        TreeRootRequest {
            cid: root,
            path: Some("script.js".to_string()),
            headers: HeaderMap::new(),
            options: TreeServeOptions {
                is_immutable: true,
                is_localhost: false,
                allow_decryption_keys: false,
                head_only: false,
            },
        },
    )
    .await;
    assert_eq!(blocked.status(), StatusCode::FORBIDDEN);
    server.abort();
}

#[tokio::test]
async fn invalid_ingest_does_not_become_servable_when_disk_cache_rejects_blobs() {
    let temp = TempDir::new().unwrap();
    let source = Arc::new(HashtreeStore::new(temp.path().join("source")).unwrap());
    let local = tiny_store(&temp.path().join("local"));
    let bytes = vec![0; 300];
    let hash = sha256(&bytes);
    source.put_blob(&bytes).unwrap();
    let (upstream, server) = start_upstream(source).await;
    let mut state = test_app_state(local.clone(), vec![upstream]);
    state.require_random_untrusted_ingest = true;

    let response =
        serve_raw_blob_with_range(&state, hash, HeaderMap::new(), true, true, false, true).await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(local.get_blob(&hash).unwrap().is_none());
    server.abort();
}

#[tokio::test]
async fn paid_enabled_reads_still_require_disk_cache_admission() {
    let temp = TempDir::new().unwrap();
    let source = Arc::new(HashtreeStore::new(temp.path().join("source")).unwrap());
    let local = tiny_store(&temp.path().join("local"));
    let bytes: Vec<u8> = (0..350_000).map(|offset| (offset % 251) as u8).collect();
    let hash = sha256(&bytes);
    source.put_blob(&bytes).unwrap();
    let (upstream, server) = start_upstream(source).await;
    let mut state = test_app_state(local.clone(), vec![upstream]);
    state.paid_reads = Some(Arc::new(
        BlobRouter::new(
            vec![BlobRouteEntry::paid(
                "test-catalog",
                Arc::new(StoreBlobRoute::new(Arc::new(MemoryStore::new()))),
            )],
            None,
            BlobRouterConfig::default(),
        )
        .unwrap(),
    ));

    let response =
        serve_raw_blob_with_range(&state, hash, HeaderMap::new(), true, true, false, true).await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(local.get_blob(&hash).unwrap().is_none());
    assert!(state.blob_cache.get_body(&to_hex(&hash)).is_none());
    server.abort();
}
