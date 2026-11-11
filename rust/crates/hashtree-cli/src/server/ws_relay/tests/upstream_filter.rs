use super::*;
use std::time::Duration;

async fn next_relay_message(
    rx: &mut mpsc::UnboundedReceiver<Message>,
) -> Result<NostrRelayMessage<'static>> {
    let Some(Message::Text(text)) = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await?
    else {
        panic!("expected a text relay message");
    };
    Ok(NostrRelayMessage::from_json(text.as_str())?)
}

/// One upstream relay answers at once. A second one is reachable only over
/// IPv4 (its IPv6 socket accepts and stays silent) and offers a forged copy
/// before the signed event.
#[tokio::test]
async fn slow_relay_family_and_unverified_events_do_not_reach_the_client() -> Result<()> {
    let tmp = TempDir::new()?;
    let graph_store = {
        let _guard = crate::socialgraph::test_lock().await;
        crate::socialgraph::open_test_social_graph_store_with_mapsize(
            tmp.path(),
            Some(128 * 1024 * 1024),
        )?
    };
    let backend: Arc<dyn crate::socialgraph::SocialGraphBackend> = graph_store.clone();
    let access = Arc::new(crate::socialgraph::SocialGraphAccessControl::new(
        Arc::clone(&backend),
        0,
        HashSet::new(),
    ));
    let keys = Keys::generate();
    let relay = Arc::new(NostrRelay::new(
        Arc::clone(&backend),
        tmp.path().to_path_buf(),
        HashSet::from([keys.public_key().to_hex()]),
        Some(access),
        NostrRelayConfig {
            spambox_db_max_bytes: 0,
            ..Default::default()
        },
    )?);

    let tags = [
        nostr::Tag::parse(["d", "videos/Test"]).expect("d tag"),
        nostr::Tag::parse(["l", "hashtree"]).expect("label tag"),
    ];
    let event = EventBuilder::new(Kind::from(30078_u16), "first")
        .custom_created_at(nostr::Timestamp::from(1_000_u64))
        .tags(tags.clone())
        .sign_with_keys(&keys)?;
    let newer = EventBuilder::new(event.kind, "newer")
        .custom_created_at(nostr::Timestamp::from(1_001_u64))
        .tags(tags)
        .sign_with_keys(&keys)?;
    let mut forged = newer.clone();
    forged.sig = Signature::from_slice(&[0u8; 64])?;

    let relay_url = spawn_mock_upstream_relay(vec![event.clone()]).await;
    let (release, waiting) = tokio::sync::oneshot::channel();
    let delayed = spawn_mock_upstream_after(vec![forged, newer.clone()], Some(waiting)).await;
    let delayed_address: std::net::SocketAddr = delayed.strip_prefix("ws://").unwrap().parse()?;
    let _silent_ipv6 =
        TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, delayed_address.port())).await?;
    let mut state = test_app_state(&tmp, relay.clone(), relay_url)?;
    state
        .nostr_relay_urls
        .push(format!("ws://localhost:{}", delayed_address.port()));

    let client_id = 12_u64;
    let (ws_tx, mut ws_rx) = mpsc::unbounded_channel();
    let (relay_tx, _relay_rx) = mpsc::unbounded_channel();
    state.ws_relay.clients.lock().await.insert(client_id, ws_tx);
    relay.register_client(client_id, relay_tx, None).await;

    let subscription_id = SubscriptionId::new("feed");
    let filters = vec![Filter::new()
        .authors(vec![event.pubkey])
        .kinds(vec![event.kind])];
    let request = NostrClientMessage::req(subscription_id.clone(), filters.clone()).as_json();
    handle_message(client_id, Message::Text(request), &state).await;

    match next_relay_message(&mut ws_rx).await? {
        NostrRelayMessage::Event { event: first, .. } => assert_eq!(first.id, event.id),
        other => panic!("expected upstream EVENT before EOSE, got {other:?}"),
    }
    // The delayed relay has not answered, so the aggregate EOSE must wait.
    assert!(
        tokio::time::timeout(Duration::from_millis(350), ws_rx.recv())
            .await
            .is_err()
    );
    release.send(()).expect("delayed relay active");
    match next_relay_message(&mut ws_rx).await? {
        NostrRelayMessage::Event { event: second, .. } => {
            assert_eq!(second.id, newer.id);
            second.verify()?;
        }
        other => panic!("expected newer EVENT before aggregate EOSE, got {other:?}"),
    }
    match next_relay_message(&mut ws_rx).await? {
        NostrRelayMessage::EndOfStoredEvents(sid) => assert_eq!(sid.as_ref(), &subscription_id),
        other => panic!("expected aggregated EOSE, got {other:?}"),
    }

    // A signed event that answers no filter of this subscription is dropped
    // and never reaches the local relay.
    let unrequested = EventBuilder::new(Kind::TextNote, "unrequested").sign_with_keys(&keys)?;
    let text = NostrRelayMessage::event(subscription_id.clone(), unrequested.clone()).as_json();
    forward_upstream_nostr_message(&state, client_id, &subscription_id, &filters, &text).await;
    assert!(ws_rx.try_recv().is_err());
    let stored = relay
        .query_events(&Filter::new().ids(vec![unrequested.id]), 10)
        .await;
    assert!(stored.is_empty());
    Ok(())
}
