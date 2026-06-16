use super::*;
use crate::nostr_relay::{NostrRelay, NostrRelayConfig};
use crate::socialgraph;
use crate::storage::HashtreeStore;
use crate::webrtc::{
    ConnectionState, PeerDirection, PeerEntry, PeerPool, PeerRootEvent, PeerSignalPath,
    PeerTransport, WebRTCState,
};
use async_trait::async_trait;
use axum::{
    body::{to_bytes, Body},
    extract::{Path as AxumPath, State as AxumState},
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use futures::{SinkExt, StreamExt};
use hashtree_core::{DirEntry, MemoryStore, Store};
use hashtree_fips_transport::{
    FipsEndpointIo, FipsEndpointPacket, FipsTransportError, HashtreeFipsTransport,
};
use http_body_util::BodyExt;
use nostr::{
    nips::nip19::ToBech32, Alphabet, ClientMessage as NostrClientMessage, EventBuilder,
    JsonUtil as NostrJsonUtil, Keys, Kind, RelayMessage as NostrRelayMessage, SingleLetterTag, Tag,
    TagKind, Timestamp,
};
use sha2::Digest;
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    net::SocketAddr,
    time::{Duration, Instant},
};
use tempfile::TempDir;
use tokio::time::timeout;
use tokio_tungstenite::{accept_async, tungstenite::Message as TungsteniteMessage};
use tower::ServiceExt;

macro_rules! event_builder {
    ($kind:expr, $content:expr $(,)?) => {
        EventBuilder::new($kind, $content)
    };
    ($kind:expr, $content:expr, $tags:expr $(,)?) => {
        EventBuilder::new($kind, $content).tags($tags)
    };
}

#[derive(Clone)]
struct UpstreamBlobTestState {
    store: Arc<HashtreeStore>,
    requested_ids: Arc<std::sync::Mutex<Vec<String>>>,
}

#[derive(Clone)]
struct UpstreamBlobBatchTestState {
    store: Arc<HashtreeStore>,
    requested_ids: Arc<std::sync::Mutex<Vec<String>>>,
    batch_requests: Arc<std::sync::atomic::AtomicUsize>,
    batch_requested_hashes: Arc<std::sync::Mutex<Vec<Vec<String>>>>,
}

#[derive(Clone)]
struct DelayedUpstreamBlobTestState {
    store: Arc<HashtreeStore>,
    requested_ids: Arc<std::sync::Mutex<Vec<String>>>,
    in_flight: Arc<std::sync::atomic::AtomicUsize>,
    max_in_flight: Arc<std::sync::atomic::AtomicUsize>,
}

struct InFlightRequest {
    in_flight: Arc<std::sync::atomic::AtomicUsize>,
}

impl Drop for InFlightRequest {
    fn drop(&mut self) {
        self.in_flight
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

async fn serve_blob_for_test(
    AxumState(store): AxumState<Arc<HashtreeStore>>,
    AxumPath(id): AxumPath<String>,
) -> Response<Body> {
    let id = id.strip_suffix(".bin").unwrap_or(&id).to_string();
    let Ok(hash) = from_hex(&id) else {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::from("invalid hash"))
            .unwrap();
    };

    match store.get_blob(&hash) {
        Ok(Some(data)) => Response::builder()
            .status(StatusCode::OK)
            .body(Body::from(data))
            .unwrap(),
        Ok(None) => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("missing"))
            .unwrap(),
        Err(err) => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(Body::from(err.to_string()))
            .unwrap(),
    }
}

async fn serve_delayed_blob_with_request_log_for_test(
    AxumState(state): AxumState<DelayedUpstreamBlobTestState>,
    AxumPath(id): AxumPath<String>,
) -> Response<Body> {
    let active = state
        .in_flight
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        + 1;
    let _guard = InFlightRequest {
        in_flight: state.in_flight.clone(),
    };

    let mut observed = state
        .max_in_flight
        .load(std::sync::atomic::Ordering::SeqCst);
    while active > observed {
        match state.max_in_flight.compare_exchange(
            observed,
            active,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        ) {
            Ok(_) => break,
            Err(next) => observed = next,
        }
    }

    state.requested_ids.lock().unwrap().push(id.clone());
    tokio::time::sleep(Duration::from_millis(75)).await;

    let id = id.strip_suffix(".bin").unwrap_or(&id).to_string();
    let Ok(hash) = from_hex(&id) else {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::from("invalid hash"))
            .unwrap();
    };

    match state.store.get_blob(&hash) {
        Ok(Some(data)) => Response::builder()
            .status(StatusCode::OK)
            .body(Body::from(data))
            .unwrap(),
        Ok(None) => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("missing"))
            .unwrap(),
        Err(err) => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(Body::from(err.to_string()))
            .unwrap(),
    }
}

async fn serve_blob_with_request_log_for_test(
    AxumState(state): AxumState<UpstreamBlobTestState>,
    AxumPath(id): AxumPath<String>,
) -> Response<Body> {
    state.requested_ids.lock().unwrap().push(id.clone());
    let id = id.strip_suffix(".bin").unwrap_or(&id).to_string();
    let Ok(hash) = from_hex(&id) else {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::from("invalid hash"))
            .unwrap();
    };

    match state.store.get_blob(&hash) {
        Ok(Some(data)) => Response::builder()
            .status(StatusCode::OK)
            .body(Body::from(data))
            .unwrap(),
        Ok(None) => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("missing"))
            .unwrap(),
        Err(err) => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(Body::from(err.to_string()))
            .unwrap(),
    }
}

async fn serve_blob_with_batch_state_for_test(
    AxumState(state): AxumState<UpstreamBlobBatchTestState>,
    AxumPath(id): AxumPath<String>,
) -> Response<Body> {
    state.requested_ids.lock().unwrap().push(id.clone());
    let id = id.strip_suffix(".bin").unwrap_or(&id).to_string();
    let Ok(hash) = from_hex(&id) else {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::from("invalid hash"))
            .unwrap();
    };

    match state.store.get_blob(&hash) {
        Ok(Some(data)) => Response::builder()
            .status(StatusCode::OK)
            .body(Body::from(data))
            .unwrap(),
        Ok(None) => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("missing"))
            .unwrap(),
        Err(err) => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(Body::from(err.to_string()))
            .unwrap(),
    }
}

async fn serve_blob_batch_with_request_log_for_test(
    AxumState(state): AxumState<UpstreamBlobBatchTestState>,
    Json(request): Json<BlobBatchDownloadRequest>,
) -> Response<Body> {
    state
        .batch_requests
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    state
        .batch_requested_hashes
        .lock()
        .unwrap()
        .push(request.hashes.clone());

    download_blob_batch(
        AxumState(test_app_state(state.store.clone(), Vec::new())),
        Json(request),
    )
    .await
    .into_response()
}

fn test_app_state(store: Arc<HashtreeStore>, upstream_blossom: Vec<String>) -> AppState {
    AppState {
        store,
        auth: None,
        daemon_started_at: 1_700_000_000,
        peer_mode: crate::config::ServerMode::Normal,
        hash_get_enabled: true,
        http_webrtc_fetch: true,
        webrtc_peers: None,
        fips_transport: None,
        fetch_from_fips_peers: true,
        ws_relay: Arc::new(crate::server::auth::WsRelayState::new()),
        max_upload_bytes: 5 * 1024 * 1024,
        public_writes: true,
        public_plaintext_reads: true,
        require_random_untrusted_ingest: false,
        optimistic_blossom_uploads: false,
        optimistic_upload_queue_bytes: 512 * 1024 * 1024,
        optimistic_upload_queue: Arc::new(tokio::sync::Semaphore::new(512 * 1024 * 1024)),
        allowed_pubkeys: HashSet::new(),
        upstream_blossom,
        upstream_http_client: crate::server::new_upstream_http_client(),
        blossom_upload_replicas: Vec::new(),
        blossom_upload_replica_queue_bytes: 512 * 1024 * 1024,
        blossom_upload_replica_queue: Arc::new(tokio::sync::Semaphore::new(512 * 1024 * 1024)),
        blossom_upload_replica_keys: None,
        blossom_upload_replica_scheduler: Arc::new(
            crate::server::blossom::BlossomUploadReplicaScheduler::new(),
        ),
        social_graph: None,
        social_graph_store: None,
        social_graph_root: None,
        socialgraph_snapshot_public: false,
        nostr_relay: None,
        nostr_relay_urls: Vec::new(),
        tree_root_cache: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        inflight_blob_fetches: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        inflight_blob_reads: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        blob_cache: Arc::new(crate::blob_cache::BlobCache::for_tests()),
        directory_listing_cache: Arc::new(std::sync::Mutex::new(crate::server::new_lookup_cache())),
        resolved_path_cache: Arc::new(std::sync::Mutex::new(crate::server::new_lookup_cache())),
        thumbnail_path_cache: Arc::new(std::sync::Mutex::new(crate::server::new_lookup_cache())),
        cid_size_cache: Arc::new(std::sync::Mutex::new(crate::server::new_lookup_cache())),
    }
}

#[tokio::test]
async fn native_store_endpoint_round_trips_raw_blob() {
    let temp = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp.path().join("store")).unwrap());
    let state = test_app_state(store.clone(), vec![]);
    let body = Bytes::from_static(b"tiny");
    let hash = hashtree_core::sha256(&body);
    let hash_hex = to_hex(&hash);

    let put = iris_store_put(
        AxumState(state.clone()),
        AxumPath(hash_hex.clone()),
        body.clone(),
    )
    .await
    .into_response();
    assert_eq!(put.status(), StatusCode::CREATED);

    let head = iris_store_head(AxumState(state.clone()), AxumPath(hash_hex.clone()))
        .await
        .into_response();
    assert_eq!(head.status(), StatusCode::OK);
    assert_eq!(head.headers()["content-length"], body.len().to_string());

    let get = iris_store_get(AxumState(state.clone()), AxumPath(hash_hex.clone()))
        .await
        .into_response();
    assert_eq!(get.status(), StatusCode::OK);
    let bytes = to_bytes(get.into_body(), usize::MAX).await.unwrap();
    assert_eq!(bytes, body);

    let delete = iris_store_delete(AxumState(state.clone()), AxumPath(hash_hex.clone()))
        .await
        .into_response();
    assert_eq!(delete.status(), StatusCode::OK);
    assert_eq!(store.get_blob(&hash).unwrap(), None);

    let missing = iris_store_get(AxumState(state), AxumPath(hash_hex))
        .await
        .into_response();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn native_store_endpoint_rejects_hash_mismatch() {
    let temp = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp.path().join("store")).unwrap());
    let state = test_app_state(store, vec![]);
    let wrong_hash = to_hex(&[1u8; 32]);

    let response = iris_store_put(
        AxumState(state),
        AxumPath(wrong_hash),
        Bytes::from_static(b"tiny"),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn blob_batch_download_serves_present_hashes_in_binary_frame() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("store")).unwrap());
    let state = test_app_state(store.clone(), vec![]);

    let first = b"batch-first".to_vec();
    let second = b"batch-second".to_vec();
    let first_hash = hashtree_core::sha256(&first);
    let second_hash = hashtree_core::sha256(&second);
    store.put_blob(&first).unwrap();
    store.put_blob(&second).unwrap();

    let missing_hash = [9u8; 32];
    let response = download_blob_batch(
        AxumState(state),
        Json(BlobBatchDownloadRequest {
            hashes: vec![
                to_hex(&first_hash),
                to_hex(&missing_hash),
                to_hex(&second_hash),
            ],
        }),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/vnd.hashtree.blob-batch.v1+octet-stream")
    );

    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let entries = decode_blob_batch_download_response(&body).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].hash, first_hash);
    assert_eq!(entries[0].data, first);
    assert_eq!(entries[1].hash, second_hash);
    assert_eq!(entries[1].data, second);
}

fn allow_plaintext_read_author(state: &mut AppState, keys: &Keys) -> String {
    let npub = keys.public_key().to_bech32().unwrap();
    state.allowed_pubkeys.insert(keys.public_key().to_hex());
    npub
}

struct FakeFipsEndpoint {
    id: String,
    network: Arc<
        tokio::sync::Mutex<HashMap<String, tokio::sync::mpsc::UnboundedSender<FipsEndpointPacket>>>,
    >,
    rx: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<FipsEndpointPacket>>,
}

impl FakeFipsEndpoint {
    async fn new(
        id: &str,
        network: Arc<
            tokio::sync::Mutex<
                HashMap<String, tokio::sync::mpsc::UnboundedSender<FipsEndpointPacket>>,
            >,
        >,
    ) -> Arc<Self> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        network.lock().await.insert(id.to_string(), tx);
        Arc::new(Self {
            id: id.to_string(),
            network,
            rx: tokio::sync::Mutex::new(rx),
        })
    }
}

#[async_trait]
impl FipsEndpointIo for FakeFipsEndpoint {
    async fn send(&self, peer_id: &str, data: Vec<u8>) -> Result<(), FipsTransportError> {
        let tx = self
            .network
            .lock()
            .await
            .get(peer_id)
            .cloned()
            .ok_or_else(|| FipsTransportError::Send(format!("unknown peer {peer_id}")))?;
        tx.send(FipsEndpointPacket {
            peer_id: self.id.clone(),
            data,
        })
        .map_err(|_| FipsTransportError::Send("receiver closed".to_string()))
    }

    async fn recv(&self) -> Option<FipsEndpointPacket> {
        self.rx.lock().await.recv().await
    }

    async fn peer_ids(&self) -> Vec<String> {
        self.network
            .lock()
            .await
            .keys()
            .filter(|id| *id != &self.id)
            .cloned()
            .collect()
    }

    fn local_peer_id(&self) -> Option<String> {
        Some(self.id.clone())
    }
}

async fn sample_webrtc_state() -> Arc<WebRTCState> {
    let state = Arc::new(WebRTCState::new());
    let peer_id = crate::webrtc::PeerId::new(
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string(),
    );
    let peer_key = peer_id.to_string();
    let signal_paths = BTreeSet::from([PeerSignalPath::Relay, PeerSignalPath::Multicast]);
    state.peers.write().await.insert(
        peer_key.clone(),
        PeerEntry {
            peer_id,
            direction: PeerDirection::Outbound,
            state: ConnectionState::Connected,
            last_seen: Instant::now(),
            peer: None,
            pool: PeerPool::Follows,
            transport: PeerTransport::WebRtc,
            signal_paths,
            bytes_sent: 64,
            bytes_received: 128,
        },
    );
    state.record_sent(&peer_key, 16).await;
    state.record_received(&peer_key, 32).await;
    state
}

async fn test_nostr_relay(dir: &TempDir, allowed_pubkey: String) -> Arc<NostrRelay> {
    let graph_store =
        socialgraph::open_social_graph_store_with_mapsize(dir.path(), Some(128 * 1024 * 1024))
            .unwrap();
    let backend: Arc<dyn socialgraph::SocialGraphBackend> = graph_store.clone();
    let mut allowed = HashSet::new();
    allowed.insert(allowed_pubkey.clone());
    let access = Arc::new(socialgraph::SocialGraphAccessControl::new(
        Arc::clone(&backend),
        0,
        allowed,
    ));

    Arc::new(
        NostrRelay::new(
            backend,
            dir.path().join("relay"),
            HashSet::from([allowed_pubkey]),
            Some(access),
            NostrRelayConfig {
                spambox_db_max_bytes: 0,
                ..Default::default()
            },
        )
        .unwrap(),
    )
}

async fn spawn_mock_upstream_relay(events: Vec<nostr::Event>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind relay");
    let addr = listener.local_addr().expect("relay addr");
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept relay");
        let ws = accept_async(stream).await.expect("accept websocket");
        let (mut write, mut read) = ws.split();

        while let Some(Ok(message)) = read.next().await {
            let TungsteniteMessage::Text(text) = message else {
                continue;
            };
            let Ok(parsed) = NostrClientMessage::from_json(text.as_bytes()) else {
                continue;
            };
            if let NostrClientMessage::Req {
                subscription_id,
                filters,
            } = parsed
            {
                let subscription_id = subscription_id.into_owned();
                let filters = filters
                    .into_iter()
                    .map(|filter| filter.into_owned())
                    .collect::<Vec<_>>();
                for event in events.iter().filter(|event| {
                    filters
                        .iter()
                        .any(|filter| filter.match_event(event, Default::default()))
                }) {
                    let _ = write
                        .send(TungsteniteMessage::Text(
                            NostrRelayMessage::event(subscription_id.clone(), event.clone())
                                .as_json()
                                .into(),
                        ))
                        .await;
                }
                let _ = write
                    .send(TungsteniteMessage::Text(
                        NostrRelayMessage::eose(subscription_id).as_json().into(),
                    ))
                    .await;
            }
        }
    });
    format!("ws://{}", addr)
}

#[tokio::test]
async fn test_query_upstream_blossom_no_servers() {
    let servers: Vec<String> = vec![];
    let result = query_upstream_blossom(
        crate::server::new_upstream_http_client(),
        &servers,
        "abc123",
    )
    .await;
    assert!(result.is_none());
}

#[tokio::test]
async fn await_webrtc_peer_response_returns_success() {
    let result = await_webrtc_peer_response(
        async { Some((b"ok".to_vec(), "peer-a".to_string())) },
        "abcd1234",
        Duration::from_millis(10),
    )
    .await;

    assert_eq!(result, Some((b"ok".to_vec(), "peer-a".to_string())));
}

#[tokio::test]
async fn webrtc_peers_reports_transport_and_signal_paths() {
    let temp = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp.path()).unwrap());
    let mut state = test_app_state(store, vec![]);
    state.webrtc_peers = Some(sample_webrtc_state().await);

    let response = webrtc_peers(AxumState(state)).await.into_response();
    assert_eq!(response.status(), StatusCode::OK);

    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(json["enabled"], true);
    assert_eq!(json["transport_counts"]["webrtc"], 1);
    assert_eq!(json["transport_counts"]["bluetooth"], 0);
    assert_eq!(json["bytes_sent"], 16);
    assert_eq!(json["bytes_received"], 32);
    assert_eq!(json["peers"][0]["transport"], "webrtc");
    assert_eq!(json["peers"][0]["bytes_sent"], 80);
    assert_eq!(json["peers"][0]["bytes_received"], 160);
    assert_eq!(
        json["peers"][0]["signal_paths"],
        json!(["relay", "multicast"])
    );
}

#[tokio::test]
async fn daemon_status_exposes_mesh_alias_with_transport_metadata() {
    let temp = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp.path()).unwrap());
    let mut state = test_app_state(store, vec![]);
    state.webrtc_peers = Some(sample_webrtc_state().await);
    state.nostr_relay_urls = vec![
        "wss://relay.damus.io".to_string(),
        "wss://nos.lol".to_string(),
    ];
    state.ws_relay.note_upstream_relay_send(512);
    state.ws_relay.note_upstream_relay_receive(1024);
    crate::server::status_metrics::record_http_status_for_test(StatusCode::SWITCHING_PROTOCOLS);
    crate::server::status_metrics::record_http_status_for_test(StatusCode::OK);
    crate::server::status_metrics::record_http_status_for_test(StatusCode::NOT_FOUND);
    crate::server::status_metrics::record_http_status_for_test(StatusCode::SERVICE_UNAVAILABLE);

    let response = daemon_status(
        AxumState(state),
        axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 21417))),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::OK);

    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(json["mesh"]["enabled"], true);
    assert_eq!(json["mesh"]["transport_counts"]["webrtc"], 1);
    assert_eq!(json["mesh"]["bytes_sent"], 16);
    assert_eq!(json["mesh"]["bytes_received"], 32);
    assert_eq!(json["mesh"]["peers"][0]["transport"], "webrtc");
    assert_eq!(json["mesh"]["peers"][0]["capabilities"]["hash_get"], true);
    assert_eq!(json["webrtc"], json["mesh"]);
    assert_eq!(json["relay"]["enabled"], true);
    assert_eq!(json["relay"]["bytes_sent"], 512);
    assert_eq!(json["relay"]["bytes_received"], 1024);
    assert_eq!(json["upstream"]["nostr_relays"], 2);
    assert_eq!(json["mode"], "normal");
    assert_eq!(json["capabilities"]["hash_get"], true);
    assert_eq!(json["capabilities"]["http_webrtc_fetch"], true);
    assert_eq!(json["daemon_started_at"], 1_700_000_000u64);
    assert!(json["uptime_seconds"].as_u64().unwrap() > 0);
    assert!(json["queues"]["blob_reads"]["limit"].as_u64().unwrap() > 0);
    assert!(json["queues"]["blob_writes"]["limit"].as_u64().unwrap() > 0);
    assert!(
        json["queues"]["blob_writes"]["queue_timeout_ms"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_eq!(
        json["queues"]["optimistic_uploads"]["max_bytes"],
        512 * 1024 * 1024u64
    );
    assert!(
        json["http"]["status_classes"]["recent"]["1xx"]
            .as_u64()
            .unwrap()
            >= 1
    );
    assert!(
        json["http"]["status_classes"]["recent"]["2xx"]
            .as_u64()
            .unwrap()
            >= 1
    );
    assert!(
        json["http"]["status_classes"]["recent"]["4xx"]
            .as_u64()
            .unwrap()
            >= 1
    );
    assert!(
        json["http"]["status_classes"]["recent"]["5xx"]
            .as_u64()
            .unwrap()
            >= 1
    );
}

#[tokio::test]
async fn daemon_status_reports_assist_mode_and_disabled_hash_get() {
    let temp = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp.path()).unwrap());
    let mut state = test_app_state(store, vec![]);
    state.peer_mode = crate::config::ServerMode::Assist;
    state.hash_get_enabled = false;

    let response = daemon_status(
        AxumState(state),
        axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 21417))),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::OK);

    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(json["mode"], "assist");
    assert_eq!(json["capabilities"]["hash_get"], false);
}

#[tokio::test]
async fn await_webrtc_peer_response_times_out() {
    let result = await_webrtc_peer_response(
        std::future::pending::<Option<(Vec<u8>, String)>>(),
        "abcd1234",
        Duration::from_millis(10),
    )
    .await;

    assert!(result.is_none());
}

#[tokio::test]
async fn first_available_fetch_prefers_fast_success() {
    let result = first_available_fetch(vec![
        async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            Some("slow")
        }
        .boxed(),
        async {
            tokio::time::sleep(Duration::from_millis(5)).await;
            Some("fast")
        }
        .boxed(),
    ])
    .await;

    assert_eq!(result, Some("fast"));
}

#[tokio::test]
async fn first_available_fetch_skips_empty_results() {
    let result = first_available_fetch(vec![
        async { None::<&'static str> }.boxed(),
        async {
            tokio::time::sleep(Duration::from_millis(5)).await;
            Some("available")
        }
        .boxed(),
    ])
    .await;

    assert_eq!(result, Some("available"));
}

#[tokio::test]
async fn await_fetch_task_returns_result() {
    let result = await_fetch_task("test", "abc123", async { Some(7usize) }).await;
    assert_eq!(result, Some(7));
}

#[tokio::test]
async fn await_fetch_task_recovers_from_panic() {
    let result: Option<usize> = await_fetch_task("test", "abc123", async move {
        panic!("boom");
    })
    .await;

    assert!(result.is_none());
}

#[tokio::test]
async fn fetch_and_cache_blob_uses_fips_transport() {
    let network = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let source_endpoint = FakeFipsEndpoint::new("source", network.clone()).await;
    let target_endpoint = FakeFipsEndpoint::new("target", network).await;
    let source_store = Arc::new(MemoryStore::new());
    let data = b"hashtree daemon over fips".to_vec();
    let digest = sha2::Sha256::digest(&data);
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&digest);
    source_store.put(hash, data.clone()).await.unwrap();

    let source_transport = Arc::new(HashtreeFipsTransport::new(source_endpoint, source_store));
    let source_task = source_transport.start();

    let temp_dir = TempDir::new().unwrap();
    let local_store = Arc::new(HashtreeStore::new(temp_dir.path().join("store")).unwrap());
    let target_transport = Arc::new(
        HashtreeFipsTransport::new(target_endpoint, local_store.store_arc())
            .with_request_timeout(Duration::from_millis(100))
            .with_cache_responses(false),
    );
    target_transport.set_peers(vec!["source".to_string()]).await;
    let target_task = target_transport.start();

    let mut state = test_app_state(local_store.clone(), Vec::new());
    state.fips_transport = Some(target_transport);

    assert!(fetch_and_cache_blob(&state, &hash).await);
    assert_eq!(local_store.get_blob(&hash).unwrap(), Some(data));

    source_task.abort();
    target_task.abort();
}

#[tokio::test]
async fn serve_content_or_blob_fetches_raw_blob_over_fips() {
    let network = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let source_endpoint = FakeFipsEndpoint::new("source", network.clone()).await;
    let target_endpoint = FakeFipsEndpoint::new("target", network).await;
    let source_store = Arc::new(MemoryStore::new());
    let data = b"raw hashtree/fips route fetch".to_vec();
    let digest = sha2::Sha256::digest(&data);
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&digest);
    let hash_hex = hex::encode(hash);
    source_store.put(hash, data.clone()).await.unwrap();

    let source_transport = Arc::new(HashtreeFipsTransport::new(source_endpoint, source_store));
    let source_task = source_transport.start();

    let temp_dir = TempDir::new().unwrap();
    let local_store = Arc::new(HashtreeStore::new(temp_dir.path().join("store")).unwrap());
    let target_transport = Arc::new(
        HashtreeFipsTransport::new(target_endpoint, local_store.store_arc())
            .with_request_timeout(Duration::from_millis(100))
            .with_cache_responses(false),
    );
    target_transport.set_peers(vec!["source".to_string()]).await;
    let target_task = target_transport.start();

    let mut state = test_app_state(local_store.clone(), Vec::new());
    state.fips_transport = Some(target_transport);

    let response = serve_content_or_blob(
        State(state),
        Path(format!("{hash_hex}.bin")),
        Query(HashMap::new()),
        axum::http::Method::GET,
        axum::http::HeaderMap::new(),
        axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 43123))),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(body.as_ref(), data.as_slice());
    assert_eq!(local_store.get_blob(&hash).unwrap(), Some(data));

    source_task.abort();
    target_task.abort();
}

#[tokio::test]
async fn test_query_upstream_blossom_invalid_server() {
    let servers = vec!["http://localhost:99999".to_string()];
    let result = query_upstream_blossom(
        crate::server::new_upstream_http_client(),
        &servers,
        "abc123",
    )
    .await;
    assert!(result.is_none());
}

#[tokio::test]
async fn test_query_upstream_blossom_hash_format() {
    // Test with valid SHA256 hash format but non-existent server
    let servers = vec!["http://localhost:99999".to_string()];
    let hash = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    let result =
        query_upstream_blossom(crate::server::new_upstream_http_client(), &servers, hash).await;
    assert!(result.is_none());
}

#[tokio::test]
async fn test_query_upstream_blossom_uses_bin_suffix() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("store")).unwrap());
    let requested_ids = Arc::new(std::sync::Mutex::new(Vec::new()));

    let data = b"hello blossom";
    store.put_blob(data).unwrap();
    let hash_hex = hex::encode(sha2::Sha256::digest(data));

    let upstream_router = Router::new()
        .route("/:id", get(serve_blob_with_request_log_for_test))
        .with_state(UpstreamBlobTestState {
            store: store.clone(),
            requested_ids: requested_ids.clone(),
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = listener.local_addr().unwrap();
    let upstream_server =
        tokio::spawn(async move { axum::serve(listener, upstream_router).await.unwrap() });

    let result = query_upstream_blossom(
        crate::server::new_upstream_http_client(),
        &[format!("http://{}", upstream_addr)],
        &hash_hex,
    )
    .await
    .expect("fetch blob");
    assert_eq!(result.0, data);
    assert_eq!(result.1, format!("http://{}", upstream_addr));
    assert_eq!(
        requested_ids.lock().unwrap().as_slice(),
        &[format!("{}.bin", hash_hex)]
    );

    upstream_server.abort();
}

#[tokio::test]
async fn query_upstream_blossom_uses_first_server_that_responds() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("store")).unwrap());
    let requested_ids = Arc::new(std::sync::Mutex::new(Vec::new()));

    let data = b"parallel blossom";
    store.put_blob(data).unwrap();
    let hash_hex = hex::encode(sha2::Sha256::digest(data));

    let slow_router = Router::new().route(
        "/:id",
        get(|| async {
            tokio::time::sleep(Duration::from_secs(11)).await;
            StatusCode::GATEWAY_TIMEOUT
        }),
    );
    let slow_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let slow_addr = slow_listener.local_addr().unwrap();
    let slow_server =
        tokio::spawn(async move { axum::serve(slow_listener, slow_router).await.unwrap() });

    let fast_router = Router::new()
        .route("/:id", get(serve_blob_with_request_log_for_test))
        .with_state(UpstreamBlobTestState {
            store: store.clone(),
            requested_ids: requested_ids.clone(),
        });
    let fast_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fast_addr = fast_listener.local_addr().unwrap();
    let fast_server =
        tokio::spawn(async move { axum::serve(fast_listener, fast_router).await.unwrap() });

    let result = timeout(
        Duration::from_secs(3),
        query_upstream_blossom(
            crate::server::new_upstream_http_client(),
            &[
                format!("http://{}", slow_addr),
                format!("http://{}", fast_addr),
            ],
            &hash_hex,
        ),
    )
    .await
    .expect("parallel upstream query completed")
    .expect("fetch blob");

    assert_eq!(result.0, data);
    assert_eq!(result.1, format!("http://{}", fast_addr));
    assert_eq!(
        requested_ids.lock().unwrap().as_slice(),
        &[format!("{}.bin", hash_hex)]
    );

    slow_server.abort();
    fast_server.abort();
}

#[tokio::test]
async fn ensure_blob_available_coalesces_concurrent_upstream_fetches() {
    let source_dir = TempDir::new().unwrap();
    let source_store = Arc::new(HashtreeStore::new(source_dir.path().join("source-db")).unwrap());
    let requested_ids = Arc::new(std::sync::Mutex::new(Vec::new()));
    let data = b"shared-upstream-blob";
    source_store.put_blob(data).unwrap();
    let hash = from_hex(&hex::encode(sha2::Sha256::digest(data))).unwrap();

    let upstream_router = Router::new()
        .route("/:id", get(serve_blob_with_request_log_for_test))
        .with_state(UpstreamBlobTestState {
            store: source_store.clone(),
            requested_ids: requested_ids.clone(),
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = listener.local_addr().unwrap();
    let upstream_server =
        tokio::spawn(async move { axum::serve(listener, upstream_router).await.unwrap() });

    let local_dir = TempDir::new().unwrap();
    let local_store = Arc::new(HashtreeStore::new(local_dir.path().join("local-db")).unwrap());
    let state = test_app_state(
        local_store.clone(),
        vec![format!("http://{}", upstream_addr)],
    );

    let (first, second, third) = tokio::join!(
        ensure_blob_available(&state, &hash),
        ensure_blob_available(&state, &hash),
        ensure_blob_available(&state, &hash),
    );

    assert_eq!(first.unwrap(), true);
    assert_eq!(second.unwrap(), true);
    assert_eq!(third.unwrap(), true);
    assert_eq!(
        requested_ids.lock().unwrap().as_slice(),
        &[format!("{}.bin", hex::encode(hash))]
    );
    assert!(local_store.get_blob(&hash).unwrap().is_some());

    upstream_server.abort();
}

#[tokio::test]
async fn fetch_missing_chunk_coalesces_concurrent_upstream_fetches() {
    let source_dir = TempDir::new().unwrap();
    let source_store = Arc::new(HashtreeStore::new(source_dir.path().join("source-db")).unwrap());
    let requested_ids = Arc::new(std::sync::Mutex::new(Vec::new()));
    let data = b"shared-missing-child-chunk";
    source_store.put_blob(data).unwrap();
    let hash_hex = hex::encode(sha2::Sha256::digest(data));

    let upstream_router = Router::new()
        .route("/:id", get(serve_blob_with_request_log_for_test))
        .with_state(UpstreamBlobTestState {
            store: source_store.clone(),
            requested_ids: requested_ids.clone(),
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = listener.local_addr().unwrap();
    let upstream_server =
        tokio::spawn(async move { axum::serve(listener, upstream_router).await.unwrap() });

    let local_dir = TempDir::new().unwrap();
    let local_store = Arc::new(HashtreeStore::new(local_dir.path().join("local-db")).unwrap());
    let state = test_app_state(
        local_store.clone(),
        vec![format!("http://{}", upstream_addr)],
    );

    let (first, second, third) = tokio::join!(
        async {
            let mut seen = HashSet::new();
            fetch_missing_chunk(&state, &mut seen, &hash_hex).await
        },
        async {
            let mut seen = HashSet::new();
            fetch_missing_chunk(&state, &mut seen, &hash_hex).await
        },
        async {
            let mut seen = HashSet::new();
            fetch_missing_chunk(&state, &mut seen, &hash_hex).await
        },
    );

    assert_eq!(first.unwrap(), true);
    assert_eq!(second.unwrap(), true);
    assert_eq!(third.unwrap(), true);
    assert_eq!(
        requested_ids.lock().unwrap().as_slice(),
        &[format!("{}.bin", hash_hex)]
    );
    assert!(local_store
        .get_blob(&from_hex(&hash_hex).unwrap())
        .unwrap()
        .is_some());

    upstream_server.abort();
}

#[tokio::test]
async fn get_cid_with_fetch_prefetches_missing_file_chunks_concurrently() {
    let source_dir = TempDir::new().unwrap();
    let source_store = Arc::new(HashtreeStore::new(source_dir.path().join("source-db")).unwrap());
    let source_tree =
        HashTree::new(HashTreeConfig::new(source_store.store_arc()).with_chunk_size(64 * 1024));
    let data: Vec<u8> = (0..(512 * 1024 + 17)).map(|i| (i % 251) as u8).collect();
    let (cid, _) = source_tree.put(&data).await.unwrap();

    let requested_ids = Arc::new(std::sync::Mutex::new(Vec::new()));
    let in_flight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let max_in_flight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let upstream_router = Router::new()
        .route("/:id", get(serve_delayed_blob_with_request_log_for_test))
        .with_state(DelayedUpstreamBlobTestState {
            store: source_store.clone(),
            requested_ids: requested_ids.clone(),
            in_flight: in_flight.clone(),
            max_in_flight: max_in_flight.clone(),
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = listener.local_addr().unwrap();
    let upstream_server =
        tokio::spawn(async move { axum::serve(listener, upstream_router).await.unwrap() });

    let local_dir = TempDir::new().unwrap();
    let local_store = Arc::new(HashtreeStore::new(local_dir.path().join("local-db")).unwrap());
    let encrypted_file_node = source_store
        .get_blob(&cid.hash)
        .unwrap()
        .expect("source file node");
    local_store.put_blob(&encrypted_file_node).unwrap();

    let state = test_app_state(
        local_store.clone(),
        vec![format!("http://{}", upstream_addr)],
    );
    let local_tree = HashTree::new(HashTreeConfig::new(local_store.store_arc()).public());

    let fetched = get_cid_with_fetch(&state, &local_tree, &cid)
        .await
        .unwrap()
        .expect("fetched file");

    assert_eq!(fetched, data);
    assert!(
        requested_ids.lock().unwrap().len() > 1,
        "expected multiple missing leaf chunks to be fetched"
    );
    assert!(
        max_in_flight.load(std::sync::atomic::Ordering::SeqCst) > 1,
        "missing file chunks should be fetched concurrently"
    );

    upstream_server.abort();
}

#[tokio::test]
async fn get_cid_with_fetch_uses_upstream_blob_batch_for_missing_file_chunks() {
    let source_dir = TempDir::new().unwrap();
    let source_store = Arc::new(HashtreeStore::new(source_dir.path().join("source-db")).unwrap());
    let source_tree =
        HashTree::new(HashTreeConfig::new(source_store.store_arc()).with_chunk_size(64 * 1024));
    let data: Vec<u8> = (0..(512 * 1024 + 17)).map(|i| (i % 251) as u8).collect();
    let (cid, _) = source_tree.put(&data).await.unwrap();

    let requested_ids = Arc::new(std::sync::Mutex::new(Vec::new()));
    let batch_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let batch_requested_hashes = Arc::new(std::sync::Mutex::new(Vec::new()));
    let upstream_router = Router::new()
        .route(
            "/blob/batch",
            post(serve_blob_batch_with_request_log_for_test),
        )
        .route("/:id", get(serve_blob_with_batch_state_for_test))
        .with_state(UpstreamBlobBatchTestState {
            store: source_store.clone(),
            requested_ids: requested_ids.clone(),
            batch_requests: batch_requests.clone(),
            batch_requested_hashes: batch_requested_hashes.clone(),
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = listener.local_addr().unwrap();
    let upstream_server =
        tokio::spawn(async move { axum::serve(listener, upstream_router).await.unwrap() });

    let local_dir = TempDir::new().unwrap();
    let local_store = Arc::new(HashtreeStore::new(local_dir.path().join("local-db")).unwrap());
    let encrypted_file_node = source_store
        .get_blob(&cid.hash)
        .unwrap()
        .expect("source file node");
    local_store.put_blob(&encrypted_file_node).unwrap();

    let state = test_app_state(
        local_store.clone(),
        vec![format!("http://{}", upstream_addr)],
    );
    let local_tree = HashTree::new(HashTreeConfig::new(local_store.store_arc()).public());

    let fetched = get_cid_with_fetch(&state, &local_tree, &cid)
        .await
        .unwrap()
        .expect("fetched file");

    assert_eq!(fetched, data);
    assert!(
        batch_requests.load(std::sync::atomic::Ordering::SeqCst) > 0,
        "expected read-through to use the blob batch endpoint"
    );
    assert!(
        requested_ids.lock().unwrap().is_empty(),
        "batch-capable upstream should not need per-blob GET fallback"
    );
    assert!(
        batch_requested_hashes
            .lock()
            .unwrap()
            .iter()
            .any(|hashes| hashes.len() > 1),
        "expected at least one multi-blob batch request"
    );

    upstream_server.abort();
}

#[tokio::test]
async fn resolve_thumbnail_path_prefers_root_thumbnail() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("store")).unwrap());
    let tree = HashTree::new(HashTreeConfig::new(store.store_arc()));
    let state = test_app_state(store.clone(), Vec::new());

    let (thumb_cid, _size) = tree.put(b"thumb").await.unwrap();
    let root_cid = tree
        .put_directory(vec![
            DirEntry::from_cid("thumbnail.jpg", &thumb_cid).with_link_type(LinkType::File)
        ])
        .await
        .unwrap();

    let resolved = resolve_thumbnail_path(&state, &tree, &root_cid, "thumbnail")
        .await
        .unwrap();
    assert_eq!(resolved.as_deref(), Some("thumbnail.jpg"));
}

#[tokio::test]
async fn resolve_thumbnail_path_accepts_generic_image_names() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("store")).unwrap());
    let tree = HashTree::new(HashTreeConfig::new(store.store_arc()));
    let state = test_app_state(store.clone(), Vec::new());

    let (thumb_cid, _size) = tree.put(b"thumb").await.unwrap();
    let root_cid = tree
        .put_directory(vec![
            DirEntry::from_cid("cover.jpeg", &thumb_cid).with_link_type(LinkType::File)
        ])
        .await
        .unwrap();

    let resolved = resolve_thumbnail_path(&state, &tree, &root_cid, "thumbnail")
        .await
        .unwrap();
    assert_eq!(resolved.as_deref(), Some("cover.jpeg"));
}

#[tokio::test]
async fn resolve_thumbnail_path_falls_back_to_subdir() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("store")).unwrap());
    let tree = HashTree::new(HashTreeConfig::new(store.store_arc()));
    let state = test_app_state(store.clone(), Vec::new());

    let (thumb_cid, _size) = tree.put(b"thumb").await.unwrap();
    let subdir_cid = tree
        .put_directory(vec![
            DirEntry::from_cid("thumbnail.png", &thumb_cid).with_link_type(LinkType::File)
        ])
        .await
        .unwrap();

    let (meta_cid, _size) = tree.put(b"{}").await.unwrap();
    let root_cid = tree
        .put_directory(vec![
            DirEntry::from_cid("clip", &subdir_cid).with_link_type(LinkType::Dir),
            DirEntry::from_cid("meta.json", &meta_cid).with_link_type(LinkType::File),
        ])
        .await
        .unwrap();

    let resolved = resolve_thumbnail_path(&state, &tree, &root_cid, "thumbnail")
        .await
        .unwrap();
    assert_eq!(resolved.as_deref(), Some("clip/thumbnail.png"));
}

#[tokio::test]
async fn resolve_thumbnail_path_fetches_missing_subdir_from_upstream() {
    let source_dir = TempDir::new().unwrap();
    let source_store = Arc::new(HashtreeStore::new(source_dir.path().join("source-db")).unwrap());
    let source_tree = HashTree::new(HashTreeConfig::new(source_store.store_arc()));

    let (thumb_cid, _size) = source_tree.put(b"thumb").await.unwrap();
    let subdir_cid = source_tree
        .put_directory(vec![
            DirEntry::from_cid("thumbnail.jpg", &thumb_cid).with_link_type(LinkType::File)
        ])
        .await
        .unwrap();
    let root_cid = source_tree
        .put_directory(vec![
            DirEntry::from_cid("clip", &subdir_cid).with_link_type(LinkType::Dir)
        ])
        .await
        .unwrap();

    let upstream_router = Router::new()
        .route("/:id", get(serve_blob_for_test))
        .with_state(source_store.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = listener.local_addr().unwrap();
    let upstream_server =
        tokio::spawn(async move { axum::serve(listener, upstream_router).await.unwrap() });

    let local_dir = TempDir::new().unwrap();
    let local_store = Arc::new(HashtreeStore::new(local_dir.path().join("local-db")).unwrap());
    let state = test_app_state(
        local_store.clone(),
        vec![format!("http://{}", upstream_addr)],
    );
    let local_tree = HashTree::new(HashTreeConfig::new(local_store.store_arc()));

    let resolved = resolve_thumbnail_path(&state, &local_tree, &root_cid, "thumbnail")
        .await
        .unwrap();
    assert_eq!(resolved.as_deref(), Some("clip/thumbnail.jpg"));

    upstream_server.abort();
}

#[tokio::test]
async fn resolve_directory_target_prefers_root_index() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("store")).unwrap());
    let tree = HashTree::new(HashTreeConfig::new(store.store_arc()));
    let state = test_app_state(store.clone(), Vec::new());

    let (index_cid, _size) = tree.put(b"<html>ok</html>").await.unwrap();
    let root_cid = tree
        .put_directory(vec![
            DirEntry::from_cid("index.html", &index_cid).with_link_type(LinkType::File)
        ])
        .await
        .unwrap();

    let target = resolve_directory_target(&state, &tree, &root_cid, None)
        .await
        .expect("resolve")
        .expect("target");

    match target {
        DirectoryTarget::File { cid, path } => {
            assert_eq!(cid, index_cid);
            assert_eq!(path, "index.html");
        }
        DirectoryTarget::DirectoryListing { .. } => panic!("expected file target"),
    }
}

#[tokio::test]
async fn resolve_directory_target_prefers_subdir_index() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("store")).unwrap());
    let tree = HashTree::new(HashTreeConfig::new(store.store_arc()));
    let state = test_app_state(store.clone(), Vec::new());

    let (index_cid, _size) = tree.put(b"<html>nested</html>").await.unwrap();
    let subdir_cid = tree
        .put_directory(vec![
            DirEntry::from_cid("index.html", &index_cid).with_link_type(LinkType::File)
        ])
        .await
        .unwrap();
    let root_cid = tree
        .put_directory(vec![
            DirEntry::from_cid("video", &subdir_cid).with_link_type(LinkType::Dir)
        ])
        .await
        .unwrap();

    let target = resolve_directory_target(&state, &tree, &root_cid, Some("video".to_string()))
        .await
        .expect("resolve")
        .expect("target");

    match target {
        DirectoryTarget::File { cid, path } => {
            assert_eq!(cid, index_cid);
            assert_eq!(path, "video/index.html");
        }
        DirectoryTarget::DirectoryListing { .. } => panic!("expected file target"),
    }
}

#[tokio::test]
async fn resolve_directory_target_lists_directory_without_index() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("store")).unwrap());
    let tree = HashTree::new(HashTreeConfig::new(store.store_arc()));
    let state = test_app_state(store.clone(), Vec::new());

    let (file_cid, _size) = tree.put(b"asset").await.unwrap();
    let root_cid = tree
        .put_directory(vec![
            DirEntry::from_cid("asset.txt", &file_cid).with_link_type(LinkType::File)
        ])
        .await
        .unwrap();

    let target = resolve_directory_target(&state, &tree, &root_cid, None)
        .await
        .expect("resolve")
        .expect("target");

    match target {
        DirectoryTarget::DirectoryListing { cid } => assert_eq!(cid, root_cid),
        DirectoryTarget::File { .. } => panic!("expected directory listing"),
    }
}

#[test]
fn content_type_for_path_uses_extension() {
    assert_eq!(content_type_for_path(Some("dir/video.mp4")), "video/mp4");
    assert_eq!(content_type_for_path(Some("image.jpeg")), "image/jpeg");
    assert_eq!(content_type_for_path(None), "application/octet-stream");
}

#[tokio::test]
async fn htree_nhash_path_fetches_nested_assets_from_upstream_tree() {
    let source_dir = TempDir::new().unwrap();
    let source_store = Arc::new(HashtreeStore::new(source_dir.path().join("source-db")).unwrap());

    let site_dir = source_dir.path().join("site");
    let assets_dir = site_dir.join("assets");
    std::fs::create_dir_all(&assets_dir).unwrap();

    let index_html = r#"
<!doctype html>
<html>
  <head><script type="module" src="./assets/main.js"></script></head>
  <body>ok</body>
</html>
"#;
    let main_js = "export const big = '".to_string() + &"x".repeat(2_500_000) + "';\n";

    std::fs::write(site_dir.join("index.html"), index_html).unwrap();
    std::fs::write(assets_dir.join("main.js"), &main_js).unwrap();

    let root_hash = source_store
        .upload_dir_with_options(&site_dir, true)
        .expect("upload site");
    let root_hash_bytes = from_hex(&root_hash).expect("hex root hash");
    let nhash = hashtree_core::nhash_encode(&root_hash_bytes).expect("encode nhash");
    let route_nhash = nhash.strip_prefix("nhash1").expect("nhash prefix");

    let upstream_router = Router::new()
        .route("/:id", get(serve_blob_for_test))
        .with_state(source_store.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = listener.local_addr().unwrap();
    let _server = tokio::spawn(async move {
        axum::serve(listener, upstream_router).await.unwrap();
    });

    let target_dir = TempDir::new().unwrap();
    let target_store = Arc::new(HashtreeStore::new(target_dir.path().join("target-db")).unwrap());
    let state = test_app_state(target_store, vec![format!("http://{}", upstream_addr)]);

    let response = htree_nhash_path(
        State(state),
        Path((route_nhash.to_string(), "assets/main.js".to_string())),
        Query(HashMap::new()),
        axum::http::Method::GET,
        axum::http::HeaderMap::new(),
        axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 43123))),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(CROSS_ORIGIN_RESOURCE_POLICY_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some(CORP_CROSS_ORIGIN)
    );
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(body.as_ref(), main_js.as_bytes());
}

#[tokio::test]
async fn htree_nhash_path_resolves_thumbnail_alias() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db")).unwrap());
    let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());

    let thumb_bytes = vec![0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, 0x4a, 0x46];
    let (thumb_cid, _) = tree.put(&thumb_bytes).await.unwrap();
    let root_cid = tree
        .put_directory(vec![
            DirEntry::from_cid("thumbnail.jpg", &thumb_cid).with_link_type(LinkType::File)
        ])
        .await
        .unwrap();

    let nhash = hashtree_core::nhash_encode(&root_cid.hash).expect("encode nhash");
    let route_nhash = nhash.strip_prefix("nhash1").expect("nhash prefix");

    let response = htree_nhash_path(
        State(test_app_state(store, Vec::new())),
        Path((route_nhash.to_string(), "thumbnail".to_string())),
        Query(HashMap::new()),
        axum::http::Method::GET,
        axum::http::HeaderMap::new(),
        axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 43123))),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(body.as_ref(), thumb_bytes.as_slice());
}

#[test]
fn parse_byte_range_supports_suffix_requests() {
    match parse_byte_range("bytes=-500", 1000) {
        Some(ParsedByteRange::Satisfiable {
            start,
            end_inclusive,
        }) => {
            assert_eq!(start, 500);
            assert_eq!(end_inclusive, 999);
        }
        _ => panic!("expected satisfiable suffix range"),
    }
}

#[test]
fn parse_byte_range_clamps_large_suffix_requests() {
    match parse_byte_range("bytes=-5000", 1000) {
        Some(ParsedByteRange::Satisfiable {
            start,
            end_inclusive,
        }) => {
            assert_eq!(start, 0);
            assert_eq!(end_inclusive, 999);
        }
        _ => panic!("expected satisfiable suffix range"),
    }
}

#[tokio::test]
async fn serve_cid_with_range_honors_suffix_ranges() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("store")).unwrap());
    let state = test_app_state(store.clone(), Vec::new());
    let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());
    let data = b"0123456789";
    let (cid, _) = tree.put(data).await.unwrap();

    let mut headers = axum::http::HeaderMap::new();
    headers.insert(header::RANGE, header::HeaderValue::from_static("bytes=-4"));

    let response =
        serve_cid_with_range(&state, &cid, headers, false, false, Some("clip.mp4"), false).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok()),
        Some("bytes 6-9/10")
    );
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(body.as_ref(), b"6789");
}

#[tokio::test]
async fn serve_cid_with_range_streams_large_explicit_ranges() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("store")).unwrap());
    let state = test_app_state(store.clone(), Vec::new());
    let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());
    let data: Vec<u8> = (0..(5 * 1024 * 1024 + 17))
        .map(|i| (i % 251) as u8)
        .collect();
    let (cid, _) = tree.put(&data).await.unwrap();

    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        header::RANGE,
        header::HeaderValue::from_str(&format!("bytes=0-{}", data.len() - 1)).unwrap(),
    );

    let response =
        serve_cid_with_range(&state, &cid, headers, true, false, Some("clip.mp4"), false).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);

    let mut body = response.into_body();
    let first_frame = timeout(Duration::from_secs(1), body.frame())
        .await
        .expect("first body frame should arrive quickly")
        .expect("body should yield a frame")
        .expect("body frame should be ok");
    let first_chunk = first_frame
        .into_data()
        .expect("first frame should contain bytes");
    assert_eq!(first_chunk.len(), CID_RANGE_STREAM_CHUNK_SIZE as usize);
}

#[tokio::test]
async fn serve_cid_with_range_head_returns_metadata_without_body() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("store")).unwrap());
    let state = test_app_state(store.clone(), Vec::new());
    let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());
    let data: Vec<u8> = (0..(5 * 1024 * 1024 + 17))
        .map(|i| (i % 251) as u8)
        .collect();
    let (cid, _) = tree.put(&data).await.unwrap();

    let response = serve_cid_with_range(
        &state,
        &cid,
        axum::http::HeaderMap::new(),
        true,
        false,
        Some("release.tar.gz"),
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let expected_len = data.len().to_string();
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok()),
        Some(expected_len.as_str())
    );
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert!(body.is_empty());
}

#[tokio::test]
async fn serve_cid_with_range_streams_large_full_gets() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("store")).unwrap());
    let state = test_app_state(store.clone(), Vec::new());
    let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());
    let data: Vec<u8> = (0..(5 * 1024 * 1024 + 17))
        .map(|i| (i % 251) as u8)
        .collect();
    let (cid, _) = tree.put(&data).await.unwrap();

    let response = serve_cid_with_range(
        &state,
        &cid,
        axum::http::HeaderMap::new(),
        true,
        false,
        Some("release.tar.gz"),
        false,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let expected_len = data.len().to_string();
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok()),
        Some(expected_len.as_str())
    );

    let mut body = response.into_body();
    let first_frame = timeout(Duration::from_secs(1), body.frame())
        .await
        .expect("first body frame should arrive quickly")
        .expect("body should yield a frame")
        .expect("body frame should be ok");
    let first_chunk = first_frame
        .into_data()
        .expect("first frame should contain bytes");
    assert!(!first_chunk.is_empty());
    assert!(
        first_chunk.len() < data.len(),
        "full GET should stream chunks instead of one huge frame"
    );
    assert_eq!(first_chunk.as_ref(), &data[..first_chunk.len()]);
}

#[tokio::test]
async fn serve_cid_with_range_streams_keyed_full_gets() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("store")).unwrap());
    let state = test_app_state(store.clone(), Vec::new());
    let tree = HashTree::new(HashTreeConfig::new(store.store_arc()));
    let data: Vec<u8> = (0..(5 * 1024 * 1024 + 17))
        .map(|i| (i % 251) as u8)
        .collect();
    let (cid, _) = tree.put(&data).await.unwrap();
    assert!(cid.key.is_some(), "test must cover keyed/encrypted CIDs");

    let response = serve_cid_with_range(
        &state,
        &cid,
        axum::http::HeaderMap::new(),
        true,
        false,
        Some("release.tar.gz"),
        false,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let expected_len = data.len().to_string();
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok()),
        Some(expected_len.as_str())
    );
    let mut body = response.into_body();
    let first_frame = timeout(Duration::from_secs(1), body.frame())
        .await
        .expect("first keyed body frame should arrive quickly")
        .expect("keyed body should yield a frame")
        .expect("keyed body frame should be ok");
    let first_chunk = first_frame
        .into_data()
        .expect("first keyed frame should contain bytes");
    assert!(!first_chunk.is_empty());
    assert!(
        first_chunk.len() < data.len(),
        "keyed full GET should stream chunks instead of one huge frame"
    );
    assert_eq!(first_chunk.as_ref(), &data[..first_chunk.len()]);
}

#[tokio::test]
async fn serve_cid_with_range_serves_keyed_ranges() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("store")).unwrap());
    let state = test_app_state(store.clone(), Vec::new());
    let tree = HashTree::new(HashTreeConfig::new(store.store_arc()));
    let data: Vec<u8> = (0..(5 * 1024 * 1024 + 17))
        .map(|i| (i % 251) as u8)
        .collect();
    let (cid, _) = tree.put(&data).await.unwrap();
    assert!(cid.key.is_some(), "test must cover keyed/encrypted CIDs");

    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        header::RANGE,
        header::HeaderValue::from_static("bytes=1024-4095"),
    );

    let response = serve_cid_with_range(
        &state,
        &cid,
        headers,
        true,
        false,
        Some("release.tar.gz"),
        false,
    )
    .await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    let expected_range = format!("bytes 1024-4095/{}", data.len());
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok()),
        Some(expected_range.as_str())
    );
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(body.as_ref(), &data[1024..4096]);
}

fn copy_blob_between_stores(
    source_store: &Arc<HashtreeStore>,
    target_store: &Arc<HashtreeStore>,
    hash: &[u8; 32],
) {
    let data = source_store
        .get_blob(hash)
        .unwrap()
        .unwrap_or_else(|| panic!("missing blob {}", to_hex(hash)));
    target_store.put_blob(&data).unwrap();
}

#[tokio::test]
async fn htree_npub_path_range_fetches_missing_nested_file_from_upstream() {
    let source_dir = TempDir::new().unwrap();
    let source_store = Arc::new(HashtreeStore::new(source_dir.path().join("source-db")).unwrap());
    let source_tree = HashTree::new(HashTreeConfig::new(source_store.store_arc()));

    let video_data: Vec<u8> = (0..(3 * 1024 * 1024 + 137))
        .map(|i| (i % 251) as u8)
        .collect();
    let (video_cid, _) = source_tree.put(&video_data).await.unwrap();
    let child_dir_cid = source_tree
        .put_directory(vec![
            DirEntry::from_cid("video.mp4", &video_cid).with_link_type(LinkType::File)
        ])
        .await
        .unwrap();
    let root_cid = source_tree
        .put_directory(vec![DirEntry::from_cid(
            "video_1767136282070",
            &child_dir_cid,
        )
        .with_link_type(LinkType::Dir)])
        .await
        .unwrap();

    let upstream_router = Router::new()
        .route("/:id", get(serve_blob_for_test))
        .with_state(source_store.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = listener.local_addr().unwrap();
    let upstream_server =
        tokio::spawn(async move { axum::serve(listener, upstream_router).await.unwrap() });

    let local_dir = TempDir::new().unwrap();
    let local_store = Arc::new(HashtreeStore::new(local_dir.path().join("local-db")).unwrap());

    // Simulate a warm playlist lookup: directory nodes are local, the media file is not.
    copy_blob_between_stores(&source_store, &local_store, &root_cid.hash);
    copy_blob_between_stores(&source_store, &local_store, &child_dir_cid.hash);

    let keys = Keys::generate();
    let mut state = test_app_state(
        local_store.clone(),
        vec![format!("http://{}", upstream_addr)],
    );
    let npub = allow_plaintext_read_author(&mut state, &keys);
    put_cached_tree_root(
        &state,
        tree_root_cache_key(&npub, "videos/Music", None),
        root_cid.clone(),
        "cache",
        None,
    );

    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        header::RANGE,
        header::HeaderValue::from_static("bytes=0-1023"),
    );

    let response = htree_npub_impl(
        State(state),
        npub,
        "videos/Music".to_string(),
        Some("video_1767136282070/video.mp4".to_string()),
        Query(HashMap::new()),
        headers,
        axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 43123))),
        false,
    )
    .await;

    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(body.as_ref(), &video_data[..1024]);

    upstream_server.abort();
}

#[tokio::test]
async fn htree_npub_path_range_fetches_missing_nested_file_chunks_from_upstream() {
    let source_dir = TempDir::new().unwrap();
    let source_store = Arc::new(HashtreeStore::new(source_dir.path().join("source-db")).unwrap());
    let source_tree = HashTree::new(HashTreeConfig::new(source_store.store_arc()));

    let video_data: Vec<u8> = (0..(5 * 1024 * 1024 + 17))
        .map(|i| 255 - (i % 251) as u8)
        .collect();
    let (video_cid, _) = source_tree.put(&video_data).await.unwrap();
    let child_dir_cid = source_tree
        .put_directory(vec![
            DirEntry::from_cid("video.mp4", &video_cid).with_link_type(LinkType::File)
        ])
        .await
        .unwrap();
    let root_cid = source_tree
        .put_directory(vec![DirEntry::from_cid(
            "video_1767136255334",
            &child_dir_cid,
        )
        .with_link_type(LinkType::Dir)])
        .await
        .unwrap();

    let upstream_router = Router::new()
        .route("/:id", get(serve_blob_for_test))
        .with_state(source_store.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = listener.local_addr().unwrap();
    let upstream_server =
        tokio::spawn(async move { axum::serve(listener, upstream_router).await.unwrap() });

    let local_dir = TempDir::new().unwrap();
    let local_store = Arc::new(HashtreeStore::new(local_dir.path().join("local-db")).unwrap());

    // Simulate a warmer cache: the file tree is local, but its encrypted chunks are not.
    copy_blob_between_stores(&source_store, &local_store, &root_cid.hash);
    copy_blob_between_stores(&source_store, &local_store, &child_dir_cid.hash);
    copy_blob_between_stores(&source_store, &local_store, &video_cid.hash);

    let keys = Keys::generate();
    let mut state = test_app_state(
        local_store.clone(),
        vec![format!("http://{}", upstream_addr)],
    );
    let npub = allow_plaintext_read_author(&mut state, &keys);
    put_cached_tree_root(
        &state,
        tree_root_cache_key(&npub, "videos/Music", None),
        root_cid.clone(),
        "cache",
        None,
    );

    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        header::RANGE,
        header::HeaderValue::from_static("bytes=0-1023"),
    );

    let response = htree_npub_impl(
        State(state),
        npub,
        "videos/Music".to_string(),
        Some("video_1767136255334/video.mp4".to_string()),
        Query(HashMap::new()),
        headers,
        axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 43123))),
        false,
    )
    .await;

    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(body.as_ref(), &video_data[..1024]);

    upstream_server.abort();
}

#[tokio::test]
async fn htree_npub_path_uses_original_uri_for_encoded_tree_names() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db")).unwrap());
    let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());

    let asset_bytes = b"nostr-vpn-macos-zip".to_vec();
    let (asset_cid, _) = tree.put(&asset_bytes).await.unwrap();
    let assets_dir = tree
        .put_directory(vec![DirEntry::from_cid(
            "nostr-vpn-v0.3.0-macos-arm64.zip",
            &asset_cid,
        )
        .with_link_type(LinkType::File)])
        .await
        .unwrap();
    let version_dir = tree
        .put_directory(vec![
            DirEntry::from_cid("assets", &assets_dir).with_link_type(LinkType::Dir)
        ])
        .await
        .unwrap();
    let root_cid = tree
        .put_directory(vec![
            DirEntry::from_cid("v0.3.0", &version_dir).with_link_type(LinkType::Dir)
        ])
        .await
        .unwrap();

    let keys = Keys::generate();
    let mut state = test_app_state(store, Vec::new());
    state.public_plaintext_reads = false;
    let npub = allow_plaintext_read_author(&mut state, &keys);
    put_cached_tree_root(
        &state,
        tree_root_cache_key(&npub, "releases/nostr-vpn", None),
        root_cid.clone(),
        "cache",
        None,
    );

    let response = htree_npub_path(
        State(state),
        OriginalUri(
            format!(
                "/htree/{npub}/releases%2Fnostr-vpn/v0.3.0/assets/nostr-vpn-v0.3.0-macos-arm64.zip"
            )
            .parse()
            .unwrap(),
        ),
        Path((
            npub.strip_prefix("npub1").unwrap_or(&npub).to_string(),
            "releases%2Fnostr-vpn".to_string(),
            "v0.3.0/assets/nostr-vpn-v0.3.0-macos-arm64.zip".to_string(),
        )),
        Query(HashMap::new()),
        axum::http::Method::GET,
        axum::http::HeaderMap::new(),
        axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 43123))),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(body.as_ref(), asset_bytes.as_slice());
}

#[tokio::test]
async fn bare_npub_route_serves_encoded_tree_name_suffix_paths() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db")).unwrap());
    let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());

    let install_bytes = b"#!/bin/sh\necho install\n".to_vec();
    let (install_cid, _) = tree.put(&install_bytes).await.unwrap();
    let latest_dir = tree
        .put_directory(vec![
            DirEntry::from_cid("install.sh", &install_cid).with_link_type(LinkType::File)
        ])
        .await
        .unwrap();
    let root_cid = tree
        .put_directory(vec![
            DirEntry::from_cid("latest", &latest_dir).with_link_type(LinkType::Dir)
        ])
        .await
        .unwrap();

    let keys = Keys::generate();
    let mut state = test_app_state(store, Vec::new());
    state.public_plaintext_reads = false;
    let npub = allow_plaintext_read_author(&mut state, &keys);
    put_cached_tree_root(
        &state,
        tree_root_cache_key(&npub, "releases/hashtree", None),
        root_cid,
        "cache",
        None,
    );

    let app = Router::new()
        .route("/npub1:rest", get(serve_npub))
        .route("/npub1:rest/*path", get(serve_npub))
        .with_state(state);

    let mut request = axum::http::Request::builder()
        .uri(format!("/{npub}/releases%2Fhashtree/latest/install.sh"))
        .body(Body::empty())
        .unwrap();
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(SocketAddr::from((
            [127, 0, 0, 1],
            43123,
        ))));

    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(body.as_ref(), install_bytes.as_slice());
}

#[tokio::test]
async fn htree_npub_rejects_unapproved_plaintext_reads_when_public_reads_disabled() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db")).unwrap());
    let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());

    let secret_bytes = b"private-ish plaintext".to_vec();
    let (secret_cid, _) = tree.put(&secret_bytes).await.unwrap();
    let root_cid = tree
        .put_directory(vec![
            DirEntry::from_cid("secret.txt", &secret_cid).with_link_type(LinkType::File)
        ])
        .await
        .unwrap();

    let keys = Keys::generate();
    let npub = keys.public_key().to_bech32().unwrap();
    let mut state = test_app_state(store, Vec::new());
    state.public_plaintext_reads = false;
    put_cached_tree_root(
        &state,
        tree_root_cache_key(&npub, "shared", None),
        root_cid,
        "cache",
        None,
    );

    let response = htree_npub_impl(
        State(state),
        npub,
        "shared".to_string(),
        Some("secret.txt".to_string()),
        Query(HashMap::new()),
        axum::http::HeaderMap::new(),
        axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 43123))),
        false,
    )
    .await;

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert!(!body
        .windows(secret_bytes.len())
        .any(|window| window == secret_bytes));
}

#[tokio::test]
async fn resolve_and_serve_rejects_unapproved_plaintext_reads_when_public_reads_disabled() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db")).unwrap());
    let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());

    let secret_bytes = b"cached plaintext via n route".to_vec();
    let (secret_cid, _) = tree.put(&secret_bytes).await.unwrap();

    let keys = Keys::generate();
    let npub = keys.public_key().to_bech32().unwrap();
    let mut state = test_app_state(store, Vec::new());
    state.public_plaintext_reads = false;
    put_cached_tree_root(
        &state,
        tree_root_cache_key(&npub, "shared", None),
        secret_cid,
        "cache",
        None,
    );

    let response = resolve_and_serve(
        State(state),
        OriginalUri(format!("/n/{npub}/shared").parse().unwrap()),
        Path((npub, "shared".to_string())),
        axum::http::HeaderMap::new(),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert!(!body
        .windows(secret_bytes.len())
        .any(|window| window == secret_bytes));
}

#[tokio::test]
async fn serve_content_internal_honors_suffix_ranges() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("store")).unwrap());
    let state = test_app_state(store.clone(), Vec::new());
    let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());
    let data = b"abcdefghij";
    let (cid, _) = tree.put(data).await.unwrap();

    let mut headers = axum::http::HeaderMap::new();
    headers.insert(header::RANGE, header::HeaderValue::from_static("bytes=-3"));

    let response = serve_content_internal(&state, &cid.hash, headers, true, false).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok()),
        Some("bytes 7-9/10")
    );
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(body.as_ref(), b"hij");
}

#[tokio::test]
async fn serve_content_or_blob_honors_raw_blob_ranges() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("store")).unwrap());
    let state = test_app_state(store.clone(), Vec::new());
    let data = b"raw-blob-range";
    let hash_hex = store.put_blob(data).unwrap();

    let mut headers = axum::http::HeaderMap::new();
    headers.insert(header::RANGE, header::HeaderValue::from_static("bytes=4-7"));

    let response = serve_content_or_blob(
        State(state),
        Path(format!("{hash_hex}.bin")),
        Query(HashMap::new()),
        axum::http::Method::GET,
        headers,
        axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 43123))),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok()),
        Some("bytes 4-7/14")
    );
    assert_eq!(
        response
            .headers()
            .get(header::ACCEPT_RANGES)
            .and_then(|value| value.to_str().ok()),
        Some("bytes")
    );
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(body.as_ref(), b"blob");
}

#[tokio::test]
async fn serve_content_or_blob_redirects_extensionless_cdn_hash_to_bin() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("store")).unwrap());
    let state = test_app_state(store.clone(), Vec::new());
    let hash_hex = store.put_blob(b"cdn-cacheable-blob").unwrap();

    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "x-forwarded-host",
        header::HeaderValue::from_static("cdn.iris.to"),
    );

    let response = serve_content_or_blob(
        State(state),
        Path(hash_hex.clone()),
        Query(HashMap::new()),
        axum::http::Method::GET,
        headers,
        axum::extract::ConnectInfo(SocketAddr::from(([203, 0, 113, 1], 43123))),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
    assert_eq!(
        response
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok()),
        Some(format!("/{hash_hex}.bin").as_str())
    );
    assert_eq!(
        response
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|value| value.to_str().ok()),
        Some(IMMUTABLE_CACHE_CONTROL)
    );
}

#[tokio::test]
async fn serve_content_or_blob_keeps_extensionless_upload_hash_compatible() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("store")).unwrap());
    let state = test_app_state(store.clone(), Vec::new());
    let data = b"upload-blossom-compatible";
    let hash_hex = store.put_blob(data).unwrap();

    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "x-forwarded-host",
        header::HeaderValue::from_static("upload.iris.to"),
    );

    let response = serve_content_or_blob(
        State(state),
        Path(hash_hex),
        Query(HashMap::new()),
        axum::http::Method::GET,
        headers,
        axum::extract::ConnectInfo(SocketAddr::from(([203, 0, 113, 1], 43123))),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(body.as_ref(), data);
}

#[tokio::test]
async fn hot_blob_cache_serves_repeated_raw_blob_reads() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("store")).unwrap());
    let state = test_app_state(store.clone(), Vec::new());
    let data = b"hot-cache-blob";
    let hash_hex = store.put_blob(data).unwrap();
    let hash = from_hex(&hash_hex).unwrap();

    assert_eq!(
        get_blob_size_without_blocking_runtime(&state, hash)
            .await
            .unwrap(),
        Some(data.len() as u64)
    );
    assert_eq!(
        get_blob_without_blocking_runtime(&state, hash)
            .await
            .unwrap()
            .as_deref(),
        Some(data.as_slice())
    );

    assert!(store.router().delete_sync(&hash).unwrap());

    assert_eq!(
        get_blob_size_without_blocking_runtime(&state, hash)
            .await
            .unwrap(),
        Some(data.len() as u64)
    );
    assert_eq!(
        get_blob_without_blocking_runtime(&state, hash)
            .await
            .unwrap()
            .as_deref(),
        Some(data.as_slice())
    );
}

#[tokio::test]
async fn raw_blob_miss_allows_short_edge_negative_cache() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("store")).unwrap());
    let state = test_app_state(store, Vec::new());
    let missing_hash = "0000000000000000000000000000000000000000000000000000000000000000";

    let response = serve_content_or_blob(
        State(state),
        Path(format!("{missing_hash}.bin")),
        Query(HashMap::new()),
        axum::http::Method::GET,
        axum::http::HeaderMap::new(),
        axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 43123))),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|value| value.to_str().ok()),
        Some(IMMUTABLE_NOT_FOUND_CACHE_CONTROL)
    );
}

#[tokio::test]
async fn generic_not_found_stays_uncacheable() {
    let response = not_found_response("missing");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|value| value.to_str().ok()),
        Some(NOT_FOUND_CACHE_CONTROL)
    );
}

#[tokio::test]
async fn cache_tree_root_seeds_mutable_root_cache() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db")).unwrap());
    let state = test_app_state(store, Vec::new());

    let response = cache_tree_root(
        State(state.clone()),
        Json(CacheTreeRootRequest {
            npub: "npub1example".to_string(),
            tree_name: "video".to_string(),
            hash: "988db3f24dc222715f1c1e1fa5876690d3147122243d72d85fd44283867cd61a".to_string(),
            key: None,
            visibility: Some("public".to_string()),
        }),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::OK);
    let cached = get_cached_tree_root(&state, "npub1example/video").expect("cached cid");
    assert_eq!(
        to_hex(&cached.cid.hash),
        "988db3f24dc222715f1c1e1fa5876690d3147122243d72d85fd44283867cd61a"
    );
    assert!(cached.cid.key.is_none());
}

#[tokio::test]
async fn resolve_root_offline_accepts_npub_owner_for_local_relay_events() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db")).unwrap());
    let keys = Keys::generate();
    let relay = test_nostr_relay(&temp_dir, keys.public_key().to_hex()).await;
    let state = AppState {
        nostr_relay: Some(relay.clone()),
        ..test_app_state(store, Vec::new())
    };
    let hash_hex = "ab".repeat(32);
    let tree_name = "offline-tree";
    let event = event_builder!(
        Kind::Custom(30078),
        "",
        [
            Tag::identifier(tree_name.to_string()),
            Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::L)),
                vec!["hashtree".to_string()],
            ),
            Tag::custom(TagKind::Custom("hash".into()), vec![hash_hex.clone()]),
        ],
    )
    .sign_with_keys(&keys)
    .unwrap();
    relay.ingest_trusted_event(event.clone()).await.unwrap();

    let resolved = resolve_root_offline(
        &state,
        &keys.public_key().to_bech32().unwrap(),
        tree_name,
        None,
    )
    .await
    .expect("offline root should resolve from local relay with npub");

    assert_eq!(resolved.source, "local-relay");
    assert_eq!(to_hex(&resolved.cid.hash), hash_hex);
    assert_eq!(
        resolved
            .root_event
            .as_ref()
            .map(|root| root.event_id.as_str()),
        Some(event.id.to_hex().as_str())
    );
}

#[tokio::test]
async fn nostr_profile_queries_upstream_relays_after_local_miss() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db")).unwrap());
    let keys = Keys::generate();
    let relay = test_nostr_relay(&temp_dir, keys.public_key().to_hex()).await;
    let event = event_builder!(
        Kind::Metadata,
        serde_json::json!({
            "display_name": "Sirius Business",
            "picture": "https://example.com/avatar.png",
        })
        .to_string(),
        [],
    )
    .custom_created_at(Timestamp::from_secs(42))
    .sign_with_keys(&keys)
    .unwrap();
    let upstream_url = spawn_mock_upstream_relay(vec![event.clone()]).await;
    let mut state = test_app_state(store, Vec::new());
    state.nostr_relay = Some(relay.clone());
    state.nostr_relay_urls = vec![upstream_url];

    let response = nostr_profile(AxumState(state), AxumPath(keys.public_key().to_hex()))
        .await
        .into_response();

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        payload["profile"]["display_name"].as_str(),
        Some("Sirius Business")
    );
    assert_eq!(payload["created_at"].as_u64(), Some(42));

    let cached = relay
        .query_events(
            &nostr::Filter::new()
                .author(keys.public_key())
                .kind(Kind::Metadata)
                .limit(10),
            10,
        )
        .await;
    assert_eq!(cached.len(), 1);
    assert_eq!(cached[0].id, event.id);
}

#[test]
fn resolver_config_prefers_state_relay_urls() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db")).unwrap());
    let mut state = test_app_state(store, Vec::new());
    state.nostr_relay_urls = vec![
        "wss://temp.iris.to".to_string(),
        "wss://upload.iris.to/nostr".to_string(),
    ];

    let config = resolver_config(&state);

    assert_eq!(config.relays, state.nostr_relay_urls);
    assert_eq!(config.resolve_timeout, HTTP_RESOLVER_TIMEOUT);
}

#[tokio::test]
async fn resolve_to_hash_refresh_skips_cached_root() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db")).unwrap());
    let state = test_app_state(store, Vec::new());
    let hash_hex = "11".repeat(32);
    let cid = Cid::parse(&hash_hex).expect("valid cid");
    put_cached_tree_root(
        &state,
        tree_root_cache_key("npub1example", "video", None),
        cid,
        "cache",
        None,
    );

    let cached = resolve_to_hash(
        State(state.clone()),
        OriginalUri("/api/resolve/npub1example/video".parse().unwrap()),
        Path(("npub1example".to_string(), "video".to_string())),
        Query(HashMap::new()),
    )
    .await
    .into_response();
    let cached_body = to_bytes(cached.into_body(), usize::MAX).await.unwrap();
    let cached_json: serde_json::Value = serde_json::from_slice(&cached_body).unwrap();
    assert_eq!(cached_json["hash"], hash_hex);
    assert_eq!(cached_json["source"], "cache");

    let refresh = resolve_to_hash(
        State(state),
        OriginalUri("/api/resolve/npub1example/video".parse().unwrap()),
        Path(("npub1example".to_string(), "video".to_string())),
        Query(HashMap::from([("refresh".to_string(), "1".to_string())])),
    )
    .await
    .into_response();
    let refresh_body = to_bytes(refresh.into_body(), usize::MAX).await.unwrap();
    let refresh_json: serde_json::Value = serde_json::from_slice(&refresh_body).unwrap();
    assert!(refresh_json.get("error").is_some());
    assert_eq!(refresh_json["key"], "npub1example/video");
}

#[tokio::test]
async fn resolve_to_hash_refresh_uses_local_relay_before_relays() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db")).unwrap());
    let keys = Keys::generate();
    let relay = test_nostr_relay(&temp_dir, keys.public_key().to_hex()).await;
    let tree_name = "video";
    let cached_hash = "11".repeat(32);
    let refreshed_hash = "22".repeat(32);

    let event = event_builder!(
        Kind::Custom(30078),
        "",
        [
            Tag::identifier(tree_name.to_string()),
            Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::L)),
                vec!["hashtree".to_string()],
            ),
            Tag::custom(TagKind::Custom("hash".into()), vec![refreshed_hash.clone()]),
        ],
    )
    .sign_with_keys(&keys)
    .unwrap();
    relay.ingest_trusted_event(event.clone()).await.unwrap();

    let state = AppState {
        nostr_relay: Some(relay),
        ..test_app_state(store, Vec::new())
    };
    put_cached_tree_root(
        &state,
        tree_root_cache_key(&keys.public_key().to_bech32().unwrap(), tree_name, None),
        Cid::parse(&cached_hash).expect("valid cached cid"),
        "cache",
        None,
    );

    let refresh = resolve_to_hash(
        State(state),
        OriginalUri(
            format!(
                "/api/resolve/{}/{}",
                keys.public_key().to_bech32().unwrap(),
                tree_name
            )
            .parse()
            .unwrap(),
        ),
        Path((
            keys.public_key().to_bech32().unwrap(),
            tree_name.to_string(),
        )),
        Query(HashMap::from([("refresh".to_string(), "1".to_string())])),
    )
    .await
    .into_response();
    let refresh_body = to_bytes(refresh.into_body(), usize::MAX).await.unwrap();
    let refresh_json: serde_json::Value = serde_json::from_slice(&refresh_body).unwrap();
    assert_eq!(refresh_json["hash"], refreshed_hash);
    assert_eq!(refresh_json["source"], "local-relay");
    assert_eq!(refresh_json["event_id"], event.id.to_hex());
}

#[tokio::test]
async fn mutable_root_request_refreshes_stale_cached_root_from_local_relay() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db")).unwrap());
    let keys = Keys::generate();
    let relay = test_nostr_relay(&temp_dir, keys.public_key().to_hex()).await;
    let tree_name = "video";
    let cached_hash = "11".repeat(32);
    let refreshed_hash = "22".repeat(32);

    let event = event_builder!(
        Kind::Custom(30078),
        "",
        [
            Tag::identifier(tree_name.to_string()),
            Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::L)),
                vec!["hashtree".to_string()],
            ),
            Tag::custom(TagKind::Custom("hash".into()), vec![refreshed_hash.clone()]),
        ],
    )
    .sign_with_keys(&keys)
    .unwrap();
    relay.ingest_trusted_event(event.clone()).await.unwrap();

    let state = AppState {
        nostr_relay: Some(relay),
        ..test_app_state(store, Vec::new())
    };
    let npub = keys.public_key().to_bech32().unwrap();
    let cache_key = tree_root_cache_key(&npub, tree_name, None);
    put_cached_tree_root(
        &state,
        cache_key.clone(),
        Cid::parse(&cached_hash).expect("valid cached cid"),
        "cache",
        None,
    );
    state
        .tree_root_cache
        .lock()
        .unwrap()
        .get_mut(&cache_key)
        .unwrap()
        .cached_at = Instant::now() - Duration::from_secs(120);

    let resolved = resolve_root_for_mutable_request(&state, &npub, tree_name, None)
        .await
        .expect("stale root should refresh from local relay");

    assert_eq!(to_hex(&resolved.cid.hash), refreshed_hash);
    assert_eq!(resolved.source, "local-relay");
    let event_id = event.id.to_hex();
    assert_eq!(
        resolved
            .root_event
            .as_ref()
            .map(|root| root.event_id.as_str()),
        Some(event_id.as_str())
    );
}

#[tokio::test]
async fn mutable_root_request_keeps_fresh_cached_root_without_refresh() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db")).unwrap());
    let keys = Keys::generate();
    let relay = test_nostr_relay(&temp_dir, keys.public_key().to_hex()).await;
    let tree_name = "video";
    let cached_hash = "11".repeat(32);
    let refreshed_hash = "22".repeat(32);

    let event = event_builder!(
        Kind::Custom(30078),
        "",
        [
            Tag::identifier(tree_name.to_string()),
            Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::L)),
                vec!["hashtree".to_string()],
            ),
            Tag::custom(TagKind::Custom("hash".into()), vec![refreshed_hash]),
        ],
    )
    .sign_with_keys(&keys)
    .unwrap();
    relay.ingest_trusted_event(event).await.unwrap();

    let state = AppState {
        nostr_relay: Some(relay),
        ..test_app_state(store, Vec::new())
    };
    let npub = keys.public_key().to_bech32().unwrap();
    put_cached_tree_root(
        &state,
        tree_root_cache_key(&npub, tree_name, None),
        Cid::parse(&cached_hash).expect("valid cached cid"),
        "cache",
        None,
    );

    let resolved = resolve_root_for_mutable_request(&state, &npub, tree_name, None)
        .await
        .expect("fresh cache should resolve");

    assert_eq!(to_hex(&resolved.cid.hash), cached_hash);
    assert_eq!(resolved.source, "cache");
}

#[tokio::test]
async fn mutable_root_request_serves_stale_cached_root_when_refresh_misses() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db")).unwrap());
    let state = test_app_state(store, Vec::new());
    let tree_name = "video";
    let cached_hash = "11".repeat(32);
    let cache_key = tree_root_cache_key("npub1example", tree_name, None);
    put_cached_tree_root(
        &state,
        cache_key.clone(),
        Cid::parse(&cached_hash).expect("valid cached cid"),
        "cache",
        None,
    );
    state
        .tree_root_cache
        .lock()
        .unwrap()
        .get_mut(&cache_key)
        .unwrap()
        .cached_at = Instant::now() - Duration::from_secs(120);

    let resolved = resolve_root_for_mutable_request(&state, "npub1example", tree_name, None)
        .await
        .expect("stale cache should be served when refresh misses");

    assert_eq!(to_hex(&resolved.cid.hash), cached_hash);
    assert_eq!(resolved.source, "stale-cache");
}

#[tokio::test]
async fn htree_npub_path_refreshes_stale_cached_root_before_serving_file() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db")).unwrap());
    let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());
    let keys = Keys::generate();
    let relay = test_nostr_relay(&temp_dir, keys.public_key().to_hex()).await;
    let tree_name = "releases/hashtree";

    let (old_release_cid, _) = tree.put(br#"{"version":"old"}"#).await.unwrap();
    let old_latest = tree
        .put_directory(vec![
            DirEntry::from_cid("release.json", &old_release_cid).with_link_type(LinkType::File)
        ])
        .await
        .unwrap();
    let old_root = tree
        .put_directory(vec![
            DirEntry::from_cid("latest", &old_latest).with_link_type(LinkType::Dir)
        ])
        .await
        .unwrap();

    let new_release = br#"{"version":"new"}"#;
    let (new_release_cid, _) = tree.put(new_release).await.unwrap();
    let new_latest = tree
        .put_directory(vec![
            DirEntry::from_cid("release.json", &new_release_cid).with_link_type(LinkType::File)
        ])
        .await
        .unwrap();
    let new_root = tree
        .put_directory(vec![
            DirEntry::from_cid("latest", &new_latest).with_link_type(LinkType::Dir)
        ])
        .await
        .unwrap();

    let event = event_builder!(
        Kind::Custom(30078),
        "",
        [
            Tag::identifier(tree_name.to_string()),
            Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::L)),
                vec!["hashtree".to_string()],
            ),
            Tag::custom(TagKind::Custom("hash".into()), vec![to_hex(&new_root.hash)],),
        ],
    )
    .sign_with_keys(&keys)
    .unwrap();
    relay.ingest_trusted_event(event).await.unwrap();

    let state = AppState {
        nostr_relay: Some(relay),
        ..test_app_state(store, Vec::new())
    };
    let npub = keys.public_key().to_bech32().unwrap();
    let cache_key = tree_root_cache_key(&npub, tree_name, None);
    put_cached_tree_root(&state, cache_key.clone(), old_root, "cache", None);
    state
        .tree_root_cache
        .lock()
        .unwrap()
        .get_mut(&cache_key)
        .unwrap()
        .cached_at = Instant::now() - Duration::from_secs(120);

    let response = htree_npub_impl(
        State(state.clone()),
        npub,
        tree_name.to_string(),
        Some("latest/release.json".to_string()),
        Query(HashMap::new()),
        axum::http::HeaderMap::new(),
        axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 43123))),
        false,
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(body.as_ref(), new_release);
    let cached = get_cached_tree_root(&state, &cache_key).expect("refreshed root should be cached");
    assert_eq!(cached.cid.hash, new_root.hash);
}

#[tokio::test]
async fn resolve_to_hash_refresh_uses_upstream_relays_after_local_miss() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db")).unwrap());
    let keys = Keys::generate();
    let relay = test_nostr_relay(&temp_dir, keys.public_key().to_hex()).await;
    let tree_name = "video";
    let refreshed_hash = "33".repeat(32);

    let event = event_builder!(
        Kind::Custom(30078),
        "",
        [
            Tag::identifier(tree_name.to_string()),
            Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::L)),
                vec!["hashtree".to_string()],
            ),
            Tag::custom(TagKind::Custom("hash".into()), vec![refreshed_hash.clone()]),
        ],
    )
    .sign_with_keys(&keys)
    .unwrap();
    let upstream_url = spawn_mock_upstream_relay(vec![event.clone()]).await;

    let mut state = test_app_state(store, Vec::new());
    state.nostr_relay = Some(relay.clone());
    state.nostr_relay_urls = vec![upstream_url];

    let refresh = resolve_to_hash(
        State(state),
        OriginalUri(
            format!(
                "/api/resolve/{}/{}",
                keys.public_key().to_bech32().unwrap(),
                tree_name
            )
            .parse()
            .unwrap(),
        ),
        Path((
            keys.public_key().to_bech32().unwrap(),
            tree_name.to_string(),
        )),
        Query(HashMap::from([("refresh".to_string(), "1".to_string())])),
    )
    .await
    .into_response();
    let refresh_body = to_bytes(refresh.into_body(), usize::MAX).await.unwrap();
    let refresh_json: serde_json::Value = serde_json::from_slice(&refresh_body).unwrap();
    assert_eq!(refresh_json["hash"], refreshed_hash);
    assert_eq!(refresh_json["source"], "nostr-relay");
    assert_eq!(refresh_json["event_id"], event.id.to_hex());

    let cached = relay
        .query_events(
            &nostr::Filter::new()
                .author(keys.public_key())
                .kind(Kind::Custom(30078))
                .limit(10),
            10,
        )
        .await;
    assert_eq!(cached.len(), 1);
    assert_eq!(cached[0].id, event.id);
}

#[tokio::test]
async fn htree_npub_path_thumbnail_does_not_fall_back_to_historical_root() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db")).unwrap());
    let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());
    let keys = Keys::generate();
    let relay = test_nostr_relay(&temp_dir, keys.public_key().to_hex()).await;

    let thumb_bytes = vec![0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, 0x4a, 0x46];
    let (thumb_cid, _) = tree.put(&thumb_bytes).await.unwrap();
    let historical_root = tree
        .put_directory(vec![
            DirEntry::from_cid("thumbnail.jpg", &thumb_cid).with_link_type(LinkType::File)
        ])
        .await
        .unwrap();
    let (video_cid, _) = tree.put(b"video-data").await.unwrap();
    let current_root = tree
        .put_directory(vec![
            DirEntry::from_cid("video.mp4", &video_cid).with_link_type(LinkType::File)
        ])
        .await
        .unwrap();

    let tree_name = "videos/Mine Bombers in-game music";
    let historical_event = event_builder!(
        Kind::Custom(30078),
        "",
        [
            Tag::identifier(tree_name.to_string()),
            Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::L)),
                vec!["hashtree".to_string()],
            ),
            Tag::custom(
                TagKind::Custom("hash".into()),
                vec![to_hex(&historical_root.hash)],
            ),
        ],
    )
    .custom_created_at(Timestamp::from(10))
    .sign_with_keys(&keys)
    .unwrap();
    relay
        .ingest_trusted_event(historical_event.clone())
        .await
        .unwrap();

    let current_event = event_builder!(
        Kind::Custom(30078),
        "",
        [
            Tag::identifier(tree_name.to_string()),
            Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::L)),
                vec!["hashtree".to_string()],
            ),
            Tag::custom(
                TagKind::Custom("hash".into()),
                vec![to_hex(&current_root.hash)],
            ),
        ],
    )
    .custom_created_at(Timestamp::from(20))
    .sign_with_keys(&keys)
    .unwrap();
    relay.ingest_trusted_event(current_event).await.unwrap();

    let state = AppState {
        nostr_relay: Some(relay),
        ..test_app_state(store, Vec::new())
    };
    let npub = keys.public_key().to_bech32().unwrap();
    put_cached_tree_root(
        &state,
        tree_root_cache_key(&npub, tree_name, None),
        current_root.clone(),
        "cache",
        None,
    );

    let response = htree_npub_impl(
        State(state),
        npub,
        tree_name.to_string(),
        Some("thumbnail".to_string()),
        Query(HashMap::new()),
        axum::http::HeaderMap::new(),
        axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 43123))),
        false,
    )
    .await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn cache_tree_root_public_chk_uses_plain_mutable_cache_key() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db")).unwrap());
    let state = test_app_state(store, Vec::new());

    let response = cache_tree_root(
        State(state.clone()),
        Json(CacheTreeRootRequest {
            npub: "npub1example".to_string(),
            tree_name: "video".to_string(),
            hash: "be8f5da537f62d02d3ff113d213a7058116f790a8d0e158c2766543deda10e35".to_string(),
            key: Some(
                "34e24fadaddc60da2e761501aae44c1c2b6b8706b73dff736eb0fc7d803133bb".to_string(),
            ),
            visibility: Some("public".to_string()),
        }),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::OK);
    let cached = get_cached_tree_root(&state, "npub1example/video").expect("cached cid");
    assert_eq!(
        to_hex(&cached.cid.hash),
        "be8f5da537f62d02d3ff113d213a7058116f790a8d0e158c2766543deda10e35"
    );
    assert_eq!(
        cached.cid.key.map(|key| to_hex(&key)).as_deref(),
        Some("34e24fadaddc60da2e761501aae44c1c2b6b8706b73dff736eb0fc7d803133bb")
    );
    assert!(get_cached_tree_root(
        &state,
        "npub1example/video?k=34e24fadaddc60da2e761501aae44c1c2b6b8706b73dff736eb0fc7d803133bb"
    )
    .is_none());
}

#[tokio::test]
async fn clear_tree_root_cache_removes_seeded_mutable_root_cache() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db")).unwrap());
    let state = test_app_state(store, Vec::new());

    let seed_response = cache_tree_root(
        State(state.clone()),
        Json(CacheTreeRootRequest {
            npub: "npub1example".to_string(),
            tree_name: "video".to_string(),
            hash: "988db3f24dc222715f1c1e1fa5876690d3147122243d72d85fd44283867cd61a".to_string(),
            key: None,
            visibility: Some("public".to_string()),
        }),
    )
    .await
    .into_response();
    assert_eq!(seed_response.status(), StatusCode::OK);
    assert!(get_cached_tree_root(&state, "npub1example/video").is_some());

    let clear_response = clear_tree_root_cache(
        State(state.clone()),
        Json(ClearTreeRootCacheRequest {
            npub: "npub1example".to_string(),
            tree_name: "video".to_string(),
            key: None,
            visibility: Some("public".to_string()),
        }),
    )
    .await
    .into_response();

    assert_eq!(clear_response.status(), StatusCode::OK);
    assert!(get_cached_tree_root(&state, "npub1example/video").is_none());
}

#[tokio::test]
async fn cached_root_preserves_encrypted_key_metadata_for_followup_resolves() {
    let temp_dir = TempDir::new().unwrap();
    let store = Arc::new(HashtreeStore::new(temp_dir.path().join("db")).unwrap());
    let state = test_app_state(store, Vec::new());
    let hash_hex = "cd".repeat(32);
    let encrypted_key = "ef".repeat(32);
    let cid = Cid::parse(&hash_hex).expect("valid cid");
    let root_event = PeerRootEvent {
        hash: hash_hex.clone(),
        key: None,
        encrypted_key: Some(encrypted_key.clone()),
        self_encrypted_key: None,
        event_id: "event-1".to_string(),
        created_at: 1,
        peer_id: "peer-a".to_string(),
    };

    put_cached_tree_root(
        &state,
        tree_root_cache_key("npub1example", "video", None),
        cid.clone(),
        "webrtc",
        Some(root_event.clone()),
    );

    let resolved = resolve_root_offline(&state, "npub1example", "video", None)
        .await
        .expect("cached root should resolve");

    assert_eq!(resolved.source, "cache");
    assert_eq!(resolved.cid, cid);
    assert_eq!(
        resolved
            .root_event
            .as_ref()
            .and_then(|root| root.encrypted_key.as_deref()),
        Some(encrypted_key.as_str())
    );
}
