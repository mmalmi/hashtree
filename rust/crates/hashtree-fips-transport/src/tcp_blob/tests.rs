use super::*;
use fips_core::config::{PeerConfig, TransportInstances};
use fips_core::{
    encode_nsec, Config, FipsEndpointServiceReceiver, Identity, PeerIdentity, UdpConfig,
};
use fips_tcp::wire::{Flags, Segment};
use std::collections::HashSet;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::Mutex;
use tokio::sync::Notify;
use tokio::time::timeout;

#[test]
fn tcp_blob_poll_interval_backs_off_without_application_work() {
    assert_eq!(tcp_blob_poll_interval(false), IDLE_POLL_INTERVAL);
    assert!(IDLE_POLL_INTERVAL >= Duration::from_millis(250));
}

#[test]
fn tcp_blob_poll_interval_stays_responsive_during_application_work() {
    assert_eq!(tcp_blob_poll_interval(true), ACTIVE_POLL_INTERVAL);
    assert_eq!(ACTIVE_POLL_INTERVAL, Duration::from_millis(10));
}

#[test]
fn request_matches_shared_codec_vector() {
    let hash = std::array::from_fn(|index| index as u8);

    assert_eq!(
        hex::encode(encode_blob_request(&BlobRequest { hash, htl: 0 })),
        "48010100000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"
    );
}

#[test]
fn retries_only_transient_connect_readiness_errors() {
    assert!(transient_connect_error(&AdapterError::Tcp(
        StackError::ConnectionLimit
    )));
    assert!(!transient_connect_error(&AdapterError::Tcp(
        StackError::InvalidConfig("invalid test config")
    )));
    assert!(!transient_connect_error(&AdapterError::Fips(
        FipsEndpointError::Node(NodeError::AccessDenied("test denial".to_string()))
    )));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn streams_concurrent_blobs_and_explicit_miss_over_two_real_fips_endpoints() {
    // Keep the complete transfer on Tokio's ordinary worker stack so this
    // integration test catches oversized transport futures.
    tokio::spawn(streams_concurrent_blobs_and_explicit_miss_on_worker())
        .await
        .expect("loopback worker task panicked");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn custom_server_route_preserves_htl_and_distinguishes_no_result_from_error() {
    tokio::spawn(custom_server_route_on_worker())
        .await
        .expect("custom-route worker task panicked");
}

async fn custom_server_route_on_worker() {
    let (endpoint_a, endpoint_b, peer_a) = connected_endpoints().await;
    let data = b"mesh result served through the shared blob route".to_vec();
    let data_hash = hash(&data);
    let missing_hash = [0x51; 32];
    let failing_hash = [0x52; 32];
    let route = Arc::new(RecordingRoute::new(data_hash, data, failing_hash));
    let transport_a = TcpBlobTransport::bind_route_with_config(
        endpoint_a.clone(),
        Arc::new(MemoryStore::new()),
        route.clone(),
        test_config(Duration::from_secs(2)),
    )
    .await
    .expect("bind custom blob route");
    let transport_b = Arc::new(
        TcpBlobTransport::bind_with_config(
            endpoint_b.clone(),
            Arc::new(MemoryStore::new()),
            test_config(Duration::from_secs(2)),
        )
        .await
        .expect("bind client blob service"),
    );
    let peer_route = transport_b.route_to(peer_a);

    assert!(matches!(
        peer_route
            .route(BlobRequest {
                hash: data_hash,
                htl: 7,
            })
            .await,
        Ok(BlobReply::Data(_))
    ));
    assert_eq!(
        peer_route
            .route(BlobRequest {
                hash: missing_hash,
                htl: 3,
            })
            .await
            .expect("explicit NoResult became a transport failure"),
        BlobReply::NoResult
    );
    assert!(
        peer_route
            .route(BlobRequest {
                hash: failing_hash,
                htl: 1,
            })
            .await
            .is_err(),
        "route failure became an explicit NoResult"
    );

    {
        let requests = route.requests.lock().expect("recorded requests");
        assert!(requests.contains(&BlobRequest {
            hash: data_hash,
            htl: 7,
        }));
        assert!(requests.contains(&BlobRequest {
            hash: missing_hash,
            htl: 3,
        }));
        assert!(requests.contains(&BlobRequest {
            hash: failing_hash,
            htl: 1,
        }));
    }
    drop(peer_route);

    transport_a.shutdown().await.unwrap();
    match Arc::try_unwrap(transport_b) {
        Ok(transport) => transport.shutdown().await.unwrap(),
        Err(_) => panic!("client transport still has task references"),
    }
    endpoint_a.shutdown().await.unwrap();
    endpoint_b.shutdown().await.unwrap();
}

async fn streams_concurrent_blobs_and_explicit_miss_on_worker() {
    let (endpoint_a, endpoint_b, peer_a) = connected_endpoints().await;
    let store_a = Arc::new(MemoryStore::new());
    let store_b = Arc::new(MemoryStore::new());
    let large = (0..(1024 * 1024 + 12_345))
        .map(|index| (index % 251) as u8)
        .collect::<Vec<_>>();
    let small = b"small concurrent TCP/FIPS blob".to_vec();
    let large_hash = hash(&large);
    let small_hash = hash(&small);
    store_a.put(large_hash, large.clone()).await.unwrap();
    store_a.put(small_hash, small.clone()).await.unwrap();
    let transport_a = TcpBlobTransport::bind_with_config(
        endpoint_a.clone(),
        store_a.clone(),
        test_config(Duration::from_secs(10)),
    )
    .await
    .expect("bind blob service A");
    let transport_b = TcpBlobTransport::bind_with_config(
        endpoint_b.clone(),
        store_b.clone(),
        test_config(Duration::from_secs(10)),
    )
    .await
    .expect("bind blob service B");

    let (large_result, small_result) = timeout(Duration::from_secs(15), async {
        tokio::join!(
            transport_b.get(&large_hash, peer_a),
            transport_b.get(&small_hash, peer_a),
        )
    })
    .await
    .expect("concurrent blob transfers timed out");
    assert_eq!(large_result.unwrap(), Some(large.clone()));
    assert_eq!(small_result.unwrap(), Some(small.clone()));
    assert_eq!(store_b.get(&large_hash).await.unwrap(), Some(large));
    assert_eq!(store_b.get(&small_hash).await.unwrap(), Some(small));

    let missing = [0x5a; 32];
    assert_eq!(
        timeout(Duration::from_secs(5), transport_b.get(&missing, peer_a))
            .await
            .expect("missing response timed out")
            .expect("missing response became a transport failure"),
        None
    );

    let corrupt_local_hash = hash(b"expected local bytes");
    store_b
        .put(corrupt_local_hash, b"corrupt local bytes".to_vec())
        .await
        .unwrap();
    assert!(matches!(
        transport_b.get(&corrupt_local_hash, peer_a).await,
        Err(TcpBlobTransportError::HashMismatch)
    ));

    let corrupt_remote_hash = hash(b"expected remote bytes");
    store_a
        .put(corrupt_remote_hash, b"corrupt remote bytes".to_vec())
        .await
        .unwrap();
    assert!(
        timeout(
            Duration::from_secs(5),
            transport_b.get(&corrupt_remote_hash, peer_a),
        )
        .await
        .expect("corrupt remote response timed out")
        .is_err(),
        "corrupt remote data must not become an explicit miss"
    );

    transport_a.shutdown().await.unwrap();
    transport_b.shutdown().await.unwrap();
    endpoint_a.shutdown().await.unwrap();
    endpoint_b.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn hedged_reads_do_not_let_a_failing_provider_starve_a_healthy_peer() {
    let source_identity = Identity::generate();
    let failing_identity = Identity::generate();
    let client_identity = Identity::generate();
    let source_address = reserve_udp_address();
    let failing_address = reserve_udp_address();
    let client_address = reserve_udp_address();
    let source_endpoint = bind_endpoint(
        &source_identity,
        source_address,
        &[(&client_identity, client_address)],
    )
    .await;
    let failing_endpoint = bind_endpoint(
        &failing_identity,
        failing_address,
        &[(&client_identity, client_address)],
    )
    .await;
    let client_endpoint = bind_endpoint(
        &client_identity,
        client_address,
        &[
            (&source_identity, source_address),
            (&failing_identity, failing_address),
        ],
    )
    .await;
    wait_for_peer(&source_endpoint, &client_identity.npub()).await;
    wait_for_peer(&failing_endpoint, &client_identity.npub()).await;
    wait_for_peer(&client_endpoint, &source_identity.npub()).await;
    wait_for_peer(&client_endpoint, &failing_identity.npub()).await;

    let data = b"healthy response raced against another provider failure".to_vec();
    let data_hash = hash(&data);
    let source_store = Arc::new(MemoryStore::new());
    source_store.put(data_hash, data.clone()).await.unwrap();
    let source = TcpBlobTransport::bind_with_config(
        source_endpoint.clone(),
        source_store,
        test_config(Duration::from_secs(2)),
    )
    .await
    .unwrap();
    let failing = TcpBlobTransport::bind_route_with_config(
        failing_endpoint.clone(),
        Arc::new(MemoryStore::new()),
        Arc::new(RecordingRoute::new([0x11; 32], Vec::new(), data_hash)),
        test_config(Duration::from_secs(2)),
    )
    .await
    .unwrap();
    let client = Arc::new(
        TcpBlobTransport::bind_with_config(
            client_endpoint.clone(),
            Arc::new(MemoryStore::new()),
            test_config(Duration::from_secs(2)),
        )
        .await
        .unwrap(),
    );
    let source_peer = PeerIdentity::from_npub(&source_identity.npub()).unwrap();
    let failing_peer = PeerIdentity::from_npub(&failing_identity.npub()).unwrap();
    let route =
        crate::FipsBlobRoute::explicit(client.clone(), vec![failing_peer, source_peer], 2).unwrap();

    assert_eq!(
        route
            .route(BlobRequest {
                hash: data_hash,
                htl: 0,
            })
            .await
            .expect("healthy hedged provider failed"),
        BlobReply::Data(data.clone()),
    );

    drop(route);
    Arc::try_unwrap(client)
        .ok()
        .expect("provider route retained the client transport")
        .shutdown()
        .await
        .unwrap();
    failing.shutdown().await.unwrap();
    source.shutdown().await.unwrap();
    client_endpoint.shutdown().await.unwrap();
    failing_endpoint.shutdown().await.unwrap();
    source_endpoint.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn fresh_get_waits_for_a_real_fips_tcp_service_to_become_ready() {
    tokio::spawn(fresh_get_during_service_readiness_on_worker())
        .await
        .expect("service-readiness worker task panicked");
}

async fn fresh_get_during_service_readiness_on_worker() {
    let (provider_endpoint, client_endpoint, provider_peer) = connected_endpoints().await;
    let receiver = provider_endpoint
        .register_service_receiver(TCP_BLOB_SERVICE_PORT)
        .await
        .expect("reserve provider service before it is ready");
    let client = Arc::new(
        TcpBlobTransport::bind_with_config(
            client_endpoint.clone(),
            Arc::new(MemoryStore::new()),
            test_config(Duration::from_secs(5)),
        )
        .await
        .expect("bind client blob service"),
    );
    let data = b"first request survives provider TCP readiness".to_vec();
    let data_hash = hash(&data);
    let fetch = {
        let client = client.clone();
        tokio::spawn(async move { client.get(&data_hash, provider_peer).await })
    };

    reset_initial_sessions(provider_endpoint.clone(), receiver, 2).await;
    let provider_store = Arc::new(MemoryStore::new());
    provider_store.put(data_hash, data.clone()).await.unwrap();
    let provider = timeout(Duration::from_secs(3), async {
        loop {
            match TcpBlobTransport::bind_with_config(
                provider_endpoint.clone(),
                provider_store.clone(),
                test_config(Duration::from_secs(5)),
            )
            .await
            {
                Ok(transport) => break transport,
                Err(TcpBlobTransportError::Transport(_)) => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(error) => panic!("provider bind failed: {error}"),
            }
        }
    })
    .await
    .expect("closed readiness receiver was not released");

    assert_eq!(
        timeout(Duration::from_secs(5), fetch)
            .await
            .expect("fresh blob request timed out")
            .expect("fresh blob task panicked")
            .expect("fresh blob request failed during provider readiness"),
        Some(data)
    );

    provider.shutdown().await.unwrap();
    match Arc::try_unwrap(client) {
        Ok(transport) => transport.shutdown().await.unwrap(),
        Err(_) => panic!("client transport still has task references"),
    }
    provider_endpoint.shutdown().await.unwrap();
    client_endpoint.shutdown().await.unwrap();
}

async fn reset_initial_sessions(
    endpoint: Arc<FipsEndpoint>,
    receiver: FipsEndpointServiceReceiver,
    count: usize,
) {
    timeout(Duration::from_secs(2), async {
        let mut ports = HashSet::new();
        let mut datagrams = Vec::new();
        while ports.len() < count {
            receiver
                .recv_batch_into(&mut datagrams, 8)
                .await
                .expect("readiness receiver closed");
            for datagram in datagrams.drain(..) {
                let request = Segment::decode(datagram.data.as_slice()).expect("decode TCP SYN");
                if !request.flags.contains(Flags::SYN) || request.flags.contains(Flags::ACK) {
                    continue;
                }
                ports.insert(request.src_port);
                let mut reset = Segment::new(request.dst_port, request.src_port, 0);
                reset.ack = Some(request.seq.wrapping_add(1));
                reset.flags = Flags::RST | Flags::ACK;
                reset.window = 0;
                endpoint
                    .send_datagram(
                        datagram.source_peer,
                        TCP_BLOB_SERVICE_PORT,
                        TCP_BLOB_SERVICE_PORT,
                        reset.encode().expect("encode TCP reset"),
                    )
                    .await
                    .expect("send TCP reset");
            }
        }
    })
    .await
    .expect("client did not make the expected initial sessions");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn outgoing_gets_do_not_head_of_line_block_and_abandoned_gets_cancel() {
    tokio::spawn(outgoing_cancellation_on_worker())
        .await
        .expect("outgoing cancellation worker task panicked");
}

async fn outgoing_cancellation_on_worker() {
    let (endpoint_a, endpoint_b, peer_a) = connected_endpoints().await;
    let store_a = Arc::new(MemoryStore::new());
    let healthy = b"healthy request behind unreachable peer".to_vec();
    let healthy_hash = hash(&healthy);
    let after_cancel = b"healthy request after cancellation".to_vec();
    let after_cancel_hash = hash(&after_cancel);
    store_a.put(healthy_hash, healthy.clone()).await.unwrap();
    store_a
        .put(after_cancel_hash, after_cancel.clone())
        .await
        .unwrap();
    let transport_a = TcpBlobTransport::bind_with_config(
        endpoint_a.clone(),
        store_a,
        test_config(Duration::from_secs(10)),
    )
    .await
    .expect("bind provider blob service");
    let transport_b = Arc::new(
        TcpBlobTransport::bind_with_config(
            endpoint_b.clone(),
            Arc::new(MemoryStore::new()),
            test_config(Duration::from_secs(10)),
        )
        .await
        .expect("bind client blob service"),
    );
    let unreachable = PeerIdentity::from_pubkey_full(Identity::generate().pubkey_full());

    {
        let unreachable_get = transport_b.get(&[0x71; 32], unreachable);
        tokio::pin!(unreachable_get);
        tokio::select! {
            result = &mut unreachable_get => panic!("unreachable request ended early: {result:?}"),
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }

        let fetched = timeout(
            Duration::from_secs(2),
            transport_b.get(&healthy_hash, peer_a),
        )
        .await
        .expect("healthy request serialized behind unreachable request")
        .expect("healthy request failed");
        assert_eq!(fetched, Some(healthy));
    }

    let mut abandoned = Vec::new();
    for index in 0..6u8 {
        let transport = transport_b.clone();
        abandoned.push(tokio::spawn(async move {
            transport.get(&[0x80 + index; 32], unreachable).await
        }));
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    for task in abandoned {
        task.abort();
        let _ = task.await;
    }

    let fetched = timeout(
        Duration::from_secs(2),
        transport_b.get(&after_cancel_hash, peer_a),
    )
    .await
    .expect("abandoned requests retained outbound slots")
    .expect("post-cancellation request failed");
    assert_eq!(fetched, Some(after_cancel));

    transport_a.shutdown().await.unwrap();
    match Arc::try_unwrap(transport_b) {
        Ok(transport) => transport.shutdown().await.unwrap(),
        Err(_) => panic!("client transport still has task references"),
    }
    endpoint_a.shutdown().await.unwrap();
    endpoint_b.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn blocked_store_load_does_not_block_another_server_session() {
    tokio::spawn(blocked_store_load_on_worker())
        .await
        .expect("store-load worker task panicked");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn cancelled_clients_release_blocked_server_store_loads() {
    tokio::spawn(cancelled_server_loads_on_worker())
        .await
        .expect("server cancellation worker task panicked");
}

async fn cancelled_server_loads_on_worker() {
    let (endpoint_a, endpoint_b, peer_a) = connected_endpoints().await;
    let blocked_data = b"canceled blocked provider load".to_vec();
    let blocked_hash = hash(&blocked_data);
    let healthy_data = b"healthy fetch after canceled provider loads".to_vec();
    let healthy_hash = hash(&healthy_data);
    let store_a = Arc::new(GatedStore::new(blocked_hash));
    store_a.put(blocked_hash, blocked_data).await.unwrap();
    store_a
        .put(healthy_hash, healthy_data.clone())
        .await
        .unwrap();
    let transport_a = TcpBlobTransport::bind_with_config(
        endpoint_a.clone(),
        store_a.clone(),
        test_config(Duration::from_secs(10)),
    )
    .await
    .expect("bind gated provider service");
    let transport_b = Arc::new(
        TcpBlobTransport::bind_with_config(
            endpoint_b.clone(),
            Arc::new(MemoryStore::new()),
            test_config(Duration::from_secs(10)),
        )
        .await
        .expect("bind client service"),
    );

    let mut blocked = Vec::new();
    for _ in 0..MAX_STORE_LOADS {
        let transport = transport_b.clone();
        blocked.push(tokio::spawn(async move {
            transport.get(&blocked_hash, peer_a).await
        }));
    }
    wait_for_blocked_loads(&store_a, MAX_STORE_LOADS).await;
    for task in blocked {
        task.abort();
        let _ = task.await;
    }

    let fetched = timeout(
        Duration::from_secs(2),
        transport_b.get(&healthy_hash, peer_a),
    )
    .await
    .expect("canceled clients retained all provider store-load slots")
    .expect("healthy fetch after provider cancellation failed");
    assert_eq!(fetched, Some(healthy_data));

    transport_a.shutdown().await.unwrap();
    match Arc::try_unwrap(transport_b) {
        Ok(transport) => transport.shutdown().await.unwrap(),
        Err(_) => panic!("client transport still has task references"),
    }
    endpoint_a.shutdown().await.unwrap();
    endpoint_b.shutdown().await.unwrap();
}

async fn blocked_store_load_on_worker() {
    let (endpoint_a, endpoint_b, peer_a) = connected_endpoints().await;
    let blocked_data = b"store load held behind a test gate".to_vec();
    let blocked_hash = hash(&blocked_data);
    let fast_data = b"independent fast store load".to_vec();
    let fast_hash = hash(&fast_data);
    let store_a = Arc::new(GatedStore::new(blocked_hash));
    store_a
        .put(blocked_hash, blocked_data.clone())
        .await
        .unwrap();
    store_a.put(fast_hash, fast_data.clone()).await.unwrap();
    let transport_a = TcpBlobTransport::bind_with_config(
        endpoint_a.clone(),
        store_a.clone(),
        test_config(Duration::from_secs(10)),
    )
    .await
    .expect("bind gated provider service");
    let transport_b = Arc::new(
        TcpBlobTransport::bind_with_config(
            endpoint_b.clone(),
            Arc::new(MemoryStore::new()),
            test_config(Duration::from_secs(10)),
        )
        .await
        .expect("bind client service"),
    );

    let blocked_task = {
        let transport = transport_b.clone();
        tokio::spawn(async move { transport.get(&blocked_hash, peer_a).await })
    };
    wait_for_blocked_loads(&store_a, 1).await;

    let fetched = timeout(Duration::from_secs(2), transport_b.get(&fast_hash, peer_a))
        .await
        .expect("fast server session blocked behind store load")
        .expect("fast server session failed");
    assert_eq!(fetched, Some(fast_data));

    store_a.release.notify_one();
    assert_eq!(
        timeout(Duration::from_secs(2), blocked_task)
            .await
            .expect("released store load timed out")
            .expect("blocked fetch task panicked")
            .expect("blocked fetch failed"),
        Some(blocked_data)
    );

    transport_a.shutdown().await.unwrap();
    match Arc::try_unwrap(transport_b) {
        Ok(transport) => transport.shutdown().await.unwrap(),
        Err(_) => panic!("client transport still has task references"),
    }
    endpoint_a.shutdown().await.unwrap();
    endpoint_b.shutdown().await.unwrap();
}

#[tokio::test]
async fn unreachable_peer_is_failure_not_missing() {
    let endpoint = Arc::new(
        FipsEndpoint::builder()
            .without_system_tun()
            .bind()
            .await
            .expect("bind endpoint"),
    );
    assert!(matches!(
        TcpBlobTransport::bind_with_config(
            endpoint.clone(),
            Arc::new(MemoryStore::new()),
            TcpBlobTransportConfig {
                idle_timeout: Duration::ZERO,
            },
        )
        .await,
        Err(TcpBlobTransportError::InvalidConfig(_))
    ));
    let transport = TcpBlobTransport::bind_with_config(
        endpoint.clone(),
        Arc::new(MemoryStore::new()),
        test_config(Duration::from_millis(100)),
    )
    .await
    .expect("bind blob service");
    let unreachable = PeerIdentity::from_pubkey_full(Identity::generate().pubkey_full());

    assert!(
        transport.get(&[9; 32], unreachable).await.is_err(),
        "an unreachable peer must not become a content miss"
    );

    transport.shutdown().await.unwrap();
    endpoint.shutdown().await.unwrap();
}

struct GatedStore {
    inner: MemoryStore,
    blocked_hash: Hash,
    blocked_started: AtomicUsize,
    release: Notify,
}

struct RecordingRoute {
    data_hash: Hash,
    data: Vec<u8>,
    failing_hash: Hash,
    requests: Mutex<Vec<BlobRequest>>,
}

impl RecordingRoute {
    fn new(data_hash: Hash, data: Vec<u8>, failing_hash: Hash) -> Self {
        Self {
            data_hash,
            data,
            failing_hash,
            requests: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl BlobRoute for RecordingRoute {
    async fn route(&self, request: BlobRequest) -> Result<BlobReply, StoreError> {
        self.requests
            .lock()
            .expect("record route request")
            .push(request);
        if request.hash == self.data_hash {
            Ok(BlobReply::Data(self.data.clone()))
        } else if request.hash == self.failing_hash {
            Err(StoreError::Other("injected route failure".to_string()))
        } else {
            Ok(BlobReply::NoResult)
        }
    }
}

impl GatedStore {
    fn new(blocked_hash: Hash) -> Self {
        Self {
            inner: MemoryStore::new(),
            blocked_hash,
            blocked_started: AtomicUsize::new(0),
            release: Notify::new(),
        }
    }
}

#[async_trait::async_trait]
impl Store for GatedStore {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> Result<bool, StoreError> {
        self.inner.put(hash, data).await
    }

    async fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        if hash == &self.blocked_hash {
            self.blocked_started.fetch_add(1, AtomicOrdering::AcqRel);
            self.release.notified().await;
        }
        self.inner.get(hash).await
    }

    async fn has(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.inner.has(hash).await
    }

    async fn delete(&self, hash: &Hash) -> Result<bool, StoreError> {
        self.inner.delete(hash).await
    }
}

async fn wait_for_blocked_loads(store: &GatedStore, expected: usize) {
    timeout(Duration::from_secs(2), async {
        while store.blocked_started.load(AtomicOrdering::Acquire) < expected {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("blocked store loads never started");
}

fn test_config(idle_timeout: Duration) -> TcpBlobTransportConfig {
    TcpBlobTransportConfig { idle_timeout }
}

async fn connected_endpoints() -> (Arc<FipsEndpoint>, Arc<FipsEndpoint>, PeerIdentity) {
    let identity_a = Identity::generate();
    let identity_b = Identity::generate();
    let address_a = reserve_udp_address();
    let address_b = reserve_udp_address();
    let config_a = endpoint_config(&identity_a, address_a, &identity_b, address_b);
    let config_b = endpoint_config(&identity_b, address_b, &identity_a, address_a);
    let endpoint_a = Arc::new(
        FipsEndpoint::builder()
            .config(config_a)
            .without_system_tun()
            .bind()
            .await
            .expect("bind endpoint A"),
    );
    let endpoint_b = Arc::new(
        FipsEndpoint::builder()
            .config(config_b)
            .without_system_tun()
            .bind()
            .await
            .expect("bind endpoint B"),
    );
    wait_for_peer(&endpoint_a, &identity_b.npub()).await;
    wait_for_peer(&endpoint_b, &identity_a.npub()).await;
    let peer_a = PeerIdentity::from_npub(&identity_a.npub()).unwrap();
    (endpoint_a, endpoint_b, peer_a)
}

async fn bind_endpoint(
    identity: &Identity,
    bind_address: SocketAddr,
    peers: &[(&Identity, SocketAddr)],
) -> Arc<FipsEndpoint> {
    let mut config = Config::new();
    config.node.identity.nsec = Some(encode_nsec(&identity.keypair().secret_key()));
    config.node.discovery.nostr.enabled = false;
    config.node.discovery.lan.enabled = false;
    config.node.discovery.local.enabled = false;
    config.transports.udp = TransportInstances::Single(UdpConfig {
        bind_addr: Some(bind_address.to_string()),
        advertise_on_nostr: Some(false),
        public: Some(true),
        ..UdpConfig::default()
    });
    config.peers = peers
        .iter()
        .map(|(peer, address)| PeerConfig::new(peer.npub(), "udp", address.to_string()))
        .collect();
    Arc::new(
        FipsEndpoint::builder()
            .config(config)
            .without_system_tun()
            .bind()
            .await
            .expect("bind test endpoint"),
    )
}

fn reserve_udp_address() -> SocketAddr {
    UdpSocket::bind("127.0.0.1:0")
        .expect("reserve loopback UDP port")
        .local_addr()
        .expect("reserved UDP address")
}

fn endpoint_config(
    identity: &Identity,
    bind_address: SocketAddr,
    peer: &Identity,
    peer_address: SocketAddr,
) -> Config {
    let mut config = Config::new();
    config.node.identity.nsec = Some(encode_nsec(&identity.keypair().secret_key()));
    config.node.discovery.nostr.enabled = false;
    config.node.discovery.lan.enabled = false;
    config.node.discovery.local.enabled = false;
    config.transports.udp = TransportInstances::Single(UdpConfig {
        bind_addr: Some(bind_address.to_string()),
        advertise_on_nostr: Some(false),
        public: Some(true),
        ..UdpConfig::default()
    });
    config.peers = vec![PeerConfig::new(
        peer.npub(),
        "udp",
        peer_address.to_string(),
    )];
    config
}

async fn wait_for_peer(endpoint: &FipsEndpoint, peer_npub: &str) {
    timeout(Duration::from_secs(5), async {
        loop {
            if endpoint
                .peers()
                .await
                .expect("peer snapshot")
                .iter()
                .any(|peer| peer.npub == peer_npub && peer.connected)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("FIPS peers did not connect");
}

fn hash(data: &[u8]) -> Hash {
    Sha256::digest(data).into()
}
