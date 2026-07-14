use super::*;
use hashtree_core::MemoryStore;
use std::sync::Arc;

type TestStore =
    MeshStoreCore<MemoryStore, crate::mock::MockRelayTransport, crate::mock::MockConnectionFactory>;

fn make_store() -> Arc<TestStore> {
    let relay = crate::mock::MockRelay::new();
    let transport = Arc::new(relay.create_transport("requester".to_string()));
    let factory = Arc::new(crate::mock::MockConnectionFactory::new(
        "requester".to_string(),
        0,
    ));
    let router = Arc::new(MeshRouter::new(
        "requester".to_string(),
        transport,
        factory,
        crate::types::PoolSettings::default(),
        false,
    ));
    Arc::new(TestStore::new(
        Arc::new(MemoryStore::new()),
        router,
        Duration::from_millis(200),
        false,
    ))
}

async fn expect_response(store: &Arc<TestStore>, hash: Hash, peers: &[&str]) {
    let (response_tx, _response_rx) = oneshot::channel();
    store.pending_requests.write().await.insert(
        hash_to_key(&hash),
        PendingRequest {
            response_tx,
            started_at: Instant::now(),
            queried_peers: peers.iter().map(ToString::to_string).collect(),
        },
    );
}

#[tokio::test]
async fn corrupt_response_gets_no_delivery_evidence_or_reciprocity_credit() {
    let store = make_store();
    let payload = b"verified block".to_vec();
    let hash = hashtree_core::sha256(&payload);
    expect_response(&store, hash, &["corrupt", "honest"]).await;

    store
        .handle_response_message("corrupt", create_response(&hash, b"wrong bytes".to_vec()))
        .await;

    assert_eq!(
        store
            .peer_traffic_snapshot("corrupt")
            .await
            .useful_bytes_received,
        0
    );
    assert!(store
        .drain_verified_block_deliveries()
        .await
        .deliveries
        .is_empty());
    assert!(store
        .pending_requests
        .read()
        .await
        .contains_key(&hash_to_key(&hash)));

    store
        .handle_response_message("honest", create_response(&hash, payload.clone()))
        .await;

    assert_eq!(
        store
            .peer_traffic_snapshot("honest")
            .await
            .useful_bytes_received,
        payload.len() as u64
    );
    assert_eq!(
        store.drain_verified_block_deliveries().await.deliveries,
        vec![VerifiedBlockDelivery {
            hash,
            provider_peer_id: "honest".to_string(),
            payload_bytes: payload.len() as u64,
        }]
    );
}

#[tokio::test]
async fn late_duplicate_gets_no_delivery_evidence_or_reciprocity_credit() {
    let store = make_store();
    let payload = b"first response wins".to_vec();
    let hash = hashtree_core::sha256(&payload);
    expect_response(&store, hash, &["winner", "late"]).await;

    store
        .handle_response_message("winner", create_response(&hash, payload.clone()))
        .await;
    store
        .handle_response_message("late", create_response(&hash, payload.clone()))
        .await;

    assert_eq!(
        store
            .peer_traffic_snapshot("winner")
            .await
            .useful_bytes_received,
        payload.len() as u64
    );
    assert_eq!(
        store
            .peer_traffic_snapshot("late")
            .await
            .useful_bytes_received,
        0
    );
    let deliveries = store.drain_verified_block_deliveries().await.deliveries;
    assert_eq!(deliveries.len(), 1);
    assert_eq!(deliveries[0].provider_peer_id, "winner");
}

#[tokio::test]
async fn concurrent_valid_responses_create_exactly_one_evidence_and_credit() {
    let store = make_store();
    let payload = b"concurrent responders".to_vec();
    let hash = hashtree_core::sha256(&payload);
    expect_response(&store, hash, &["alice", "bob"]).await;

    let alice = store.handle_response_message("alice", create_response(&hash, payload.clone()));
    let bob = store.handle_response_message("bob", create_response(&hash, payload.clone()));
    tokio::join!(alice, bob);

    let deliveries = store.drain_verified_block_deliveries().await.deliveries;
    assert_eq!(deliveries.len(), 1);
    let winner = &deliveries[0].provider_peer_id;
    let alice_credit = store
        .peer_traffic_snapshot("alice")
        .await
        .useful_bytes_received;
    let bob_credit = store
        .peer_traffic_snapshot("bob")
        .await
        .useful_bytes_received;
    assert_eq!(
        alice_credit.saturating_add(bob_credit),
        payload.len() as u64
    );
    assert_eq!(
        if winner == "alice" {
            alice_credit
        } else {
            bob_credit
        },
        payload.len() as u64
    );
}

#[tokio::test]
async fn verified_delivery_evidence_queue_is_bounded() {
    let store = make_store();
    for sequence in 0..=VERIFIED_BLOCK_DELIVERY_CAPACITY {
        let payload = sequence.to_le_bytes().to_vec();
        let hash = hashtree_core::sha256(&payload);
        expect_response(&store, hash, &["provider"]).await;
        store
            .handle_response_message("provider", create_response(&hash, payload))
            .await;
    }

    let batch = store.drain_verified_block_deliveries().await;
    assert_eq!(batch.deliveries.len(), VERIFIED_BLOCK_DELIVERY_CAPACITY);
    assert_eq!(batch.dropped_since_last_drain, 1);
    assert_eq!(
        batch.deliveries[0].hash,
        hashtree_core::sha256(&1usize.to_le_bytes())
    );
    assert_eq!(
        batch.deliveries[0].payload_bytes,
        std::mem::size_of::<usize>() as u64
    );
    assert_eq!(
        store.drain_verified_block_deliveries().await,
        VerifiedBlockDeliveryBatch::default()
    );
}
