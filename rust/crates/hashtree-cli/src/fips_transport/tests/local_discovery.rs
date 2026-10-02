use super::*;

#[tokio::test]
async fn daemon_blob_resolver_discovers_other_daemons_through_pure_anchor() {
    let rendezvous = reserve_local_rendezvous_addr();
    let anchor = local_only_endpoint(rendezvous, "pure-fips-anchor").await;
    let provider = local_only_endpoint(rendezvous, "drive-provider").await;
    let consumer = local_only_endpoint(rendezvous, "htree-consumer").await;
    let temp = tempfile::tempdir().unwrap();
    let provider_store = Arc::new(HashtreeStore::new(temp.path().join("provider")).unwrap());
    let consumer_store = Arc::new(HashtreeStore::new(temp.path().join("consumer")).unwrap());
    let data = b"found without any configured peer identities".to_vec();
    let hash = Sha256::digest(&data).into();
    provider_store
        .store_arc()
        .put(hash, data.clone())
        .await
        .unwrap();
    let (provider_resolver, provider_transport) = bind_daemon_blob_resolver(
        &provider,
        provider_store.store_arc(),
        &[],
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    let (consumer_resolver, consumer_transport) = bind_daemon_blob_resolver(
        &consumer,
        consumer_store.store_arc(),
        &[],
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    timeout(Duration::from_secs(10), async {
        while !provider_advertised(&consumer, &provider.local_peer_id) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("daemon provider roster converges");
    assert!(
        !consumer
            .native_endpoint
            .peers()
            .await
            .unwrap()
            .iter()
            .any(|peer| { peer.connected && peer.npub == provider.local_peer_id }),
        "the discovered provider is reachable only through the pure anchor"
    );
    assert_eq!(
        consumer_resolver.get(&hash, None).await.unwrap(),
        Some(data.clone())
    );
    assert_eq!(
        consumer_store.store_arc().get(&hash).await.unwrap(),
        Some(data)
    );
    // A mutual miss must exhaust the mesh budget instead of recursing forever.
    let missing = Sha256::digest(b"not in either daemon").into();
    let result = timeout(
        Duration::from_secs(7),
        consumer_resolver.get(&missing, None),
    )
    .await;
    assert!(
        result.is_ok(),
        "discovered daemon forwarding did not terminate"
    );
    assert!(!matches!(result.unwrap(), Ok(Some(_))));

    drop((
        consumer_resolver,
        consumer_transport,
        provider_resolver,
        provider_transport,
    ));
    consumer.native_endpoint.shutdown().await.unwrap();
    provider.native_endpoint.shutdown().await.unwrap();
    anchor.native_endpoint.shutdown().await.unwrap();
}
