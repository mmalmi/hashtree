use super::*;

#[tokio::test]
async fn link_visible_root_unmasks_only_with_its_secret() {
    let keys = Keys::generate();
    let key = format!("{}/shared", keys.public_key().to_bech32().unwrap());
    let relay = TestRelay::with_events(Vec::new());
    let resolver = NostrRootResolver::new(NostrResolverConfig {
        relays: vec![relay.url()],
        resolve_timeout: Duration::from_secs(2),
        secret_key: Some(keys.clone()),
    })
    .await
    .unwrap();
    let root = Cid {
        hash: [0x31; 32],
        key: Some(ObjectKey::from_bytes(&[1; 33]).unwrap()),
    };
    let secret = LinkKeyBytes::from_bytes(&[0x5a; 33]).unwrap();
    assert!(resolver
        .publish_shared(&key, &root, secret.as_ref())
        .await
        .unwrap());

    let window = Duration::from_secs(1);
    assert_eq!(
        resolver
            .resolve_shared(&key, secret.as_ref())
            .await
            .unwrap(),
        Some(root.clone())
    );
    assert_eq!(
        resolver
            .resolve_open_shared(&key, secret.as_ref(), window)
            .await
            .unwrap(),
        root
    );

    // keyId rejects a same-length wrong secret; length rejects a legacy one.
    for wrong in [[0x5b; 33].as_slice(), [0x5a; 32].as_slice()] {
        let quick = resolver.resolve_shared(&key, wrong).await.unwrap_err();
        let open = resolver
            .resolve_open_shared(&key, wrong, window)
            .await
            .unwrap_err();
        for error in [quick, open] {
            assert!(
                error.to_string().contains("link key does not match"),
                "{error}"
            );
        }
    }
    resolver.stop().await.unwrap();
}
