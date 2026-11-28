use super::*;
use crate::server::HashtreeServer;
use axum::http::StatusCode;
use axum::routing::{head, post, put};
use hashtree_core::{sha256, DirEntry, HashTree, HashTreeConfig};
use nostr::Keys;
use tempfile::TempDir;

fn test_store() -> (TempDir, Arc<HashtreeStore>) {
    let dir = tempfile::tempdir().unwrap();
    let store = HashtreeStore::with_options(dir.path(), None, 64 * 1024 * 1024).unwrap();
    (dir, Arc::new(store))
}

async fn start_server(
    store: Arc<HashtreeStore>,
    keys: &Keys,
    max_blob_bytes: usize,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = HashtreeServer::new(store, addr.to_string())
        .with_allowed_pubkeys(HashSet::from([keys.public_key().to_hex()]))
        .with_public_writes(false)
        .with_max_upload_bytes(max_blob_bytes);
    let task = tokio::spawn(async move {
        server.run_with_listener(listener).await.unwrap();
    });
    (format!("http://{addr}"), task)
}

#[tokio::test]
#[ignore = "run explicitly for a quiet loopback transfer measurement"]
async fn measure_regular_push_wire_cost() {
    use std::net::SocketAddr;
    use std::time::Instant;
    use tokio::net::{TcpListener, TcpStream};
    use tokio::task::JoinSet;

    let (_source_dir, source) = test_store();
    let (_remote_dir, remote) = test_store();
    let tree = HashTree::new(HashTreeConfig::new(source.store_arc()).public());
    let mut entries = Vec::new();
    let mut payload_bytes = 0;
    for i in 0..32u8 {
        let len = if i < 30 { 8 * 1024 } else { 512 * 1024 };
        let data = vec![i + 1; len];
        let (file, size) = tree.put(&data).await.unwrap();
        entries.push(DirEntry::from_cid(format!("leaf-{i}"), &file).with_size(size));
        payload_bytes += len;
    }
    let root = tree.put_directory(entries).await.unwrap();
    let cids = collect_cids_for_push(&source, root.clone(), None)
        .await
        .unwrap();
    let blocks = cids.len();
    let keys = Keys::generate();
    let (url, server) = start_server(remote.clone(), &keys, 2 * 1024 * 1024).await;
    let target: SocketAddr = url.strip_prefix("http://").unwrap().parse().unwrap();
    for pass in ["new", "repeat"] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = format!("http://{}", listener.local_addr().unwrap());
        let (stop, mut stopped) = tokio::sync::oneshot::channel();
        let meter = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (mut downstream, _) = accepted.unwrap();
                        connections.spawn(async move {
                            let mut upstream = TcpStream::connect(target).await.unwrap();
                            tokio::io::copy_bidirectional(&mut downstream, &mut upstream).await.unwrap()
                        });
                    }
                    _ = &mut stopped => break,
                }
            }
            let mut sent = 0u64;
            let mut received = 0u64;
            while let Some(connection) = connections.join_next().await {
                let (up, down) = connection.unwrap();
                sent += up;
                received += down;
            }
            (sent, received)
        });
        let client = BlossomClient::new_empty(keys.clone()).with_write_servers(vec![proxy]);
        let started = Instant::now();
        let (uploaded, existing) =
            upload_cids_with_client(source.clone(), None, client, cids.clone(), false)
                .await
                .unwrap();
        let elapsed_us = started.elapsed().as_micros();
        stop.send(()).unwrap();
        let (sent, received) = tokio::time::timeout(Duration::from_secs(10), meter)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(uploaded + existing, blocks);
        assert!(sent > 0 && received > 0);
        eprintln!("regular_push pass={pass} payload_bytes={payload_bytes} blocks={blocks} uploaded={uploaded} existing={existing} request_wire_bytes={sent} response_wire_bytes={received} elapsed_us={elapsed_us}");
    }
    let remote_tree = HashTree::new(HashTreeConfig::new(remote.store_arc()).public());
    let file = remote_tree
        .resolve_path(&root, "leaf-31")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        remote_tree.get(&file, None).await.unwrap().unwrap(),
        vec![32; 512 * 1024]
    );
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn regular_push_claims_existing_leaf_and_preserves_rooted_read() {
    let (_source_dir, source) = test_store();
    let (_remote_dir, remote) = test_store();
    let tree = HashTree::new(HashTreeConfig::new(source.store_arc()).public());
    let bytes = b"previously uploaded by a different author";
    let (leaf, size) = tree.put(bytes).await.unwrap();
    let root = tree
        .put_directory(vec![DirEntry::from_cid("note", &leaf).with_size(size)])
        .await
        .unwrap();
    let cids = collect_cids_for_push(&source, root.clone(), None)
        .await
        .unwrap();
    let old_owner = [6; 32];
    remote.put_owned_blob(bytes, &old_owner).unwrap();

    let keys = Keys::generate();
    let (url, server) = start_server(remote.clone(), &keys, 1024 * 1024).await;
    let reader = BlossomClient::new_empty(Keys::generate()).with_read_servers(vec![url.clone()]);
    let client = BlossomClient::new_empty(keys).with_write_servers(vec![url]);
    let counts = upload_cids_with_client(source, None, client, cids, false)
        .await
        .unwrap();
    assert_eq!(counts, (1, 1));
    assert!(
        !remote.delete_blossom_blob(&leaf.hash, &old_owner).unwrap(),
        "the second author's signed push must protect the leaf"
    );
    let root_bytes = reader.download(&to_hex(&root.hash)).await.unwrap();
    let directory = decode_tree_node_by_cid(&root, root_bytes).unwrap().unwrap();
    let note = directory
        .links
        .iter()
        .find(|link| link.name.as_deref() == Some("note"))
        .unwrap()
        .hash;
    assert_eq!(
        reader.download(&to_hex(&note)).await.unwrap(),
        bytes.as_slice()
    );
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn regular_single_large_push_claims_existing_bytes() {
    let (_source_dir, source) = test_store();
    let (_remote_dir, remote) = test_store();
    let bytes = vec![7; 256 * 1024];
    let cid = Cid::parse(&source.put_blob(&bytes).unwrap()).unwrap();
    let old_owner = [3; 32];
    remote.put_owned_blob(&bytes, &old_owner).unwrap();

    let keys = Keys::generate();
    let (url, server) = start_server(remote.clone(), &keys, 1024 * 1024).await;
    let client = BlossomClient::new_empty(keys).with_write_servers(vec![url]);
    let counts = upload_cids_with_client(source, None, client, vec![cid.clone()], false)
        .await
        .unwrap();
    assert_eq!(counts, (0, 1));
    assert!(!remote.delete_blossom_blob(&cid.hash, &old_owner).unwrap());
    assert_eq!(remote.get_blob(&cid.hash).unwrap().unwrap(), bytes);
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn regular_push_tries_another_batch_server_after_a_later_batch_fails() {
    let (_source_dir, source) = test_store();
    let (_first_dir, first) = test_store();
    let (_second_dir, second) = test_store();
    let small = vec![1; 4 * 1024 * 1024];
    let large = vec![2; 5 * 1024 * 1024];
    let small = Cid::parse(&source.put_blob(&small).unwrap()).unwrap();
    let large = Cid::parse(&source.put_blob(&large).unwrap()).unwrap();
    let keys = Keys::generate();
    let (first_url, first_server) = start_server(first.clone(), &keys, 4 * 1024 * 1024).await;
    let (second_url, second_server) = start_server(second.clone(), &keys, 5 * 1024 * 1024).await;
    let client = BlossomClient::new_empty(keys).with_write_servers(vec![first_url, second_url]);
    let counts = upload_cids_with_client(
        source,
        None,
        client,
        vec![small.clone(), large.clone()],
        false,
    )
    .await
    .expect("both ranges must reach a configured server");
    assert_eq!(counts, (2, 0));
    assert!(first.blob_exists(&small.hash).unwrap());
    assert!(second.blob_exists(&large.hash).unwrap());
    first_server.abort();
    second_server.abort();
    let _ = tokio::join!(first_server, second_server);
}

#[tokio::test]
async fn regular_push_uses_signed_plain_upload_after_batch_refusal_even_when_head_is_present() {
    let (_dir, store) = test_store();
    let bodies = [vec![13; 256 * 1024], vec![14; 32]];
    let cids = bodies
        .iter()
        .map(|body| Cid::parse(&store.put_blob(body).unwrap()).unwrap())
        .collect();
    let keys = Keys::generate();
    let author = keys.public_key().to_hex();
    let rejected = axum::Router::new()
        .route(
            "/upload/check",
            post(
                |axum::Json(req): axum::Json<serde_json::Value>| async move {
                    let count = req["hashes"].as_array().unwrap().len();
                    axum::Json(serde_json::json!({"count": count, "present": "AA=="}))
                },
            ),
        )
        .route(
            "/upload/batch-binary",
            post(|| async { StatusCode::BAD_REQUEST }),
        )
        .route(
            "/upload",
            put(|body: bytes::Bytes| async move {
                if body.len() < 256 * 1024 {
                    StatusCode::ACCEPTED
                } else {
                    StatusCode::CONFLICT
                }
            }),
        )
        .route("/:hash", head(|| async { StatusCode::OK }));
    let received = Arc::new(std::sync::Mutex::new(HashSet::new()));
    let sink = Arc::clone(&received);
    let plain = axum::Router::new()
        .route("/:hash", head(|| async { StatusCode::OK }))
        .route(
            "/upload",
            put(move |headers: axum::http::HeaderMap, body: bytes::Bytes| {
                let sink = Arc::clone(&sink);
                let author = author.clone();
                async move {
                    let hash = sha256(&body);
                    let auth = crate::server::blossom::verify_blossom_auth(
                        &headers,
                        "upload",
                        Some(&to_hex(&hash)),
                    )
                    .unwrap();
                    assert_eq!(auth.pubkey, author);
                    sink.lock().unwrap().insert(hash);
                    StatusCode::CREATED
                }
            }),
        );
    let mut urls = Vec::new();
    let mut servers = Vec::new();
    for app in [rejected, plain] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        urls.push(format!("http://{}", listener.local_addr().unwrap()));
        servers.push(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap()
        }));
    }
    let client = BlossomClient::new_empty(keys).with_write_servers(urls);
    let counts = upload_cids_with_client(store, None, client, cids, false)
        .await
        .unwrap();
    assert_eq!(counts, (2, 0));
    assert_eq!(
        *received.lock().unwrap(),
        bodies.iter().map(|body| sha256(body)).collect()
    );
    for server in servers {
        server.abort();
        let _ = server.await;
    }
}
