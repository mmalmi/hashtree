use super::*;

async fn gateway(events: Vec<Event>, responses: Vec<(u16, Option<&str>)>) -> Relay {
    Relay::with_upgrades(
        events,
        0,
        None,
        Mode::Complete,
        false,
        0,
        responses
            .into_iter()
            .map(|(status, delay)| (status, delay.map(str::to_owned)))
            .collect(),
    )
    .await
}

#[tokio::test]
async fn temporary_gateway_upgrade_retries_once_then_requires_verified_eose() {
    let keys = Keys::generate();
    let expected = event(&keys, 20, "after gateway recovery");
    for status in [502, 503, 504] {
        let relay = gateway(vec![expected.clone()], vec![(status, None)]).await;
        let started = Instant::now();
        let result = RelaySource::new(4, 65536)
            .query(&relay.url, &query(&keys))
            .await
            .unwrap();
        assert!(started.elapsed() >= GATEWAY_RECONNECT_DELAY);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, expected.id.to_hex());
        assert_eq!(relay.connections.load(Ordering::SeqCst), 2);
        let requests = relay.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0][2],
            json!({
                "authors": [keys.public_key().to_hex()], "kinds": [1],
                "since": 10, "until": 100, "limit": 4,
            })
        );
    }
}

#[tokio::test]
async fn repeated_gateway_is_terminal_after_two_attempts_and_omits_response_body() {
    let relay = gateway(vec![], vec![(503, None), (503, None)]).await;
    let mut source = RelaySource::new(4, 65536);
    let error = source
        .query(&relay.url, &query(&Keys::generate()))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "connect: HTTP status 503");
    assert_eq!(relay.connections.load(Ordering::SeqCst), 2);
    assert!(relay.requests.lock().unwrap().is_empty());
    assert!(source.sockets.is_empty());
}

#[tokio::test]
async fn gateway_retry_after_delta_is_respected_within_original_deadline() {
    let relay = gateway(vec![], vec![(503, Some("2"))]).await;
    let started = Instant::now();
    assert!(RelaySource::new(4, 65536)
        .query(&relay.url, &query(&Keys::generate()))
        .await
        .unwrap()
        .is_empty());
    assert!(started.elapsed() >= Duration::from_secs(2));
    assert_eq!(relay.connections.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn gateway_delay_does_not_start_a_new_page_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let _ = accept_hdr_async(
            stream,
            |_: &tokio_tungstenite::tungstenite::handshake::server::Request,
             _: tokio_tungstenite::tungstenite::handshake::server::Response| {
                Err(tokio_tungstenite::tungstenite::http::Response::builder()
                    .status(503)
                    .body(Some(String::new()))
                    .unwrap())
            },
        )
        .await;
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let Some(Ok(Message::Text(raw))) = socket.next().await else {
            panic!("missing REQ")
        };
        let request: Value = serde_json::from_str(&raw).unwrap();
        tokio::time::sleep(Duration::from_millis(1400)).await;
        let _ = socket
            .send(Message::Text(json!(["EOSE", request[1]]).to_string()))
            .await;
    });
    let mut source = RelaySource::new(2, 65536);
    let error = source
        .query(&url, &query(&Keys::generate()))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("timeout before EOSE"));
    assert!(source.sockets.is_empty());
    server.abort();
}

#[tokio::test]
async fn malformed_dates_and_unaffordable_gateway_delays_never_reconnect() {
    for delay in [
        "invalid",
        "",
        "-1",
        "+1",
        "18446744073709551616",
        "Wed, 21 Oct 2037 07:28:00 GMT",
        "2",
        "18446744073709551615",
    ] {
        let relay = gateway(vec![], vec![(503, Some(delay))]).await;
        let error = RelaySource::new(1, 65536)
            .query(&relay.url, &query(&Keys::generate()))
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "connect: HTTP status 503");
        assert_eq!(relay.connections.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn later_gateway_recovery_does_not_refetch_a_completed_relay() {
    let keys = Keys::generate();
    let first = gateway(vec![event(&keys, 20, "first")], vec![]).await;
    let second = gateway(vec![event(&keys, 30, "second")], vec![(503, None)]).await;
    let policy = CatchupPolicy {
        base_root: "unused".into(),
        source_mode: Default::default(),
        authors_sha256: "unused".into(),
        author_count: 1,
        initial_since: 10,
        overlap_secs: 0,
        relays: vec![first.url.clone(), second.url.clone()],
        pubsub_peers: Vec::new(),
        kinds: vec![1],
        page_size: 4,
        max_pages_per_author: 20,
        max_events_per_author: 20,
        max_bytes_per_author: 65536,
        fetch_timeout_secs: 4,
        index_commit_batch_size: 4,
    };
    let result = fetch_catchup_author(
        &mut RelaySource::new(4, 65536),
        &policy,
        &keys.public_key().to_hex(),
        10,
        100,
    )
    .await
    .unwrap();
    assert_eq!(result.len(), 2);
    assert_eq!(first.connections.load(Ordering::SeqCst), 1);
    assert_eq!(second.connections.load(Ordering::SeqCst), 2);
    assert_eq!(first.requests.lock().unwrap().len(), 3);
    assert_eq!(second.requests.lock().unwrap().len(), 3);
}
