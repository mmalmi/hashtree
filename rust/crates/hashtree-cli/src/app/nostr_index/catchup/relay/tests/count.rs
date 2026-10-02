use super::*;

#[derive(Clone)]
enum Reply {
    Exact,
    Payload(Value),
    Closed,
    WrongId,
    Flood,
    Oversized,
}

struct CountRelay {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    task: tokio::task::JoinHandle<()>,
}

impl CountRelay {
    async fn new(events: Vec<Event>, cap: usize, reply: Reply) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let observed = requests.clone();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (events, reply, requests) = (events.clone(), reply.clone(), observed.clone());
                tokio::spawn(async move {
                    let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                    while let Some(Ok(Message::Text(raw))) = socket.next().await {
                        let request: Value = serde_json::from_str(&raw).unwrap();
                        if !matches!(request[0].as_str(), Some("REQ" | "COUNT")) {
                            continue;
                        }
                        requests.lock().unwrap().push(request.clone());
                        let filter = &request[2];
                        let mut matching = events
                            .iter()
                            .filter(|event| {
                                event.pubkey.to_hex() == filter["authors"][0].as_str().unwrap()
                                    && event.created_at.as_secs()
                                        >= filter["since"].as_u64().unwrap()
                                    && event.created_at.as_secs()
                                        <= filter["until"].as_u64().unwrap()
                                    && filter["kinds"]
                                        .as_array()
                                        .unwrap()
                                        .contains(&json!(event.kind.as_u16()))
                            })
                            .cloned()
                            .collect::<Vec<_>>();
                        matching
                            .sort_by_key(|event| (std::cmp::Reverse(event.created_at), event.id));
                        if request[0] == "REQ" {
                            matching.truncate(cap.min(filter["limit"].as_u64().unwrap() as usize));
                            for event in matching {
                                if socket
                                    .send(Message::Text(
                                        json!(["EVENT", request[1], event]).to_string(),
                                    ))
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                            }
                            if socket
                                .send(Message::Text(json!(["EOSE", request[1]]).to_string()))
                                .await
                                .is_err()
                            {
                                return;
                            }
                            continue;
                        }
                        let response = match &reply {
                            Reply::Exact => json!(["COUNT",request[1],{"count":matching.len()}]),
                            Reply::Payload(value) => json!(["COUNT", request[1], value]),
                            Reply::Closed => json!(["CLOSED", request[1], "unsupported"]),
                            Reply::WrongId | Reply::Flood => {
                                json!(["COUNT","wrong-query",{"count":2}])
                            }
                            Reply::Oversized => {
                                json!(["COUNT",request[1],{"count":2,"padding":"x".repeat(5000)}])
                            }
                        };
                        let repeats = if matches!(reply, Reply::Flood) {
                            120
                        } else {
                            1
                        };
                        for _ in 0..repeats {
                            if socket
                                .send(Message::Text(response.to_string()))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                });
            }
        });
        Self {
            url,
            requests,
            task,
        }
    }
}

impl Drop for CountRelay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
async fn exact_count_uses_fresh_id_and_full_filter_without_limit() {
    let keys = Keys::generate();
    let relay = CountRelay::new(
        vec![event(&keys, 20, "a"), event(&keys, 20, "b")],
        2,
        Reply::Exact,
    )
    .await;
    let mut source = RelaySource::new(2, 65536);
    for _ in 0..2 {
        assert_eq!(
            source.exact_count(&relay.url, &query(&keys)).await.unwrap(),
            Some(2)
        );
    }
    let requests = relay.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_ne!(requests[0][1], requests[1][1]);
    assert_eq!(
        requests[0][2],
        json!({"authors":[keys.public_key().to_hex()],"kinds":[1],"since":10,"until":100})
    );
    assert!(source.sockets.contains_key(&relay.url));
}

#[tokio::test]
async fn approximate_hll_and_unsupported_counts_do_not_confirm_coverage() {
    for reply in [
        Reply::Payload(json!({"count":2,"approximate":true})),
        Reply::Payload(json!({"count":2,"hll":"00"})),
        Reply::Closed,
    ] {
        let relay = CountRelay::new(vec![], 2, reply).await;
        assert_eq!(
            RelaySource::new(2, 65536)
                .exact_count(&relay.url, &query(&Keys::generate()))
                .await
                .unwrap(),
            None
        );
    }
    let relay = CountRelay::new(
        vec![],
        2,
        Reply::Payload(json!({"count":0,"approximate":false})),
    )
    .await;
    assert_eq!(
        RelaySource::new(2, 65536)
            .exact_count(&relay.url, &query(&Keys::generate()))
            .await
            .unwrap(),
        Some(0)
    );
}

#[tokio::test]
async fn malformed_count_payloads_drop_connection() {
    for payload in [
        json!({}),
        json!({"count":-1}),
        json!({"count":2.5}),
        json!({"count":"2"}),
        json!({"count":9007199254740992u64}),
        json!({"count":2,"approximate":"false"}),
        json!([]),
    ] {
        let relay = CountRelay::new(vec![], 2, Reply::Payload(payload)).await;
        let mut source = RelaySource::new(2, 65536);
        assert!(source
            .exact_count(&relay.url, &query(&Keys::generate()))
            .await
            .is_err());
        assert!(source.sockets.is_empty());
    }
}

#[tokio::test]
async fn wrong_query_count_and_floods_remain_bounded_and_drop_connection() {
    for (reply, max_bytes, expected) in [
        (Reply::WrongId, 65536, "timeout before COUNT"),
        (Reply::Flood, 65536, "message budget"),
        (Reply::Oversized, 0, "byte budget"),
    ] {
        let relay = CountRelay::new(vec![], 2, reply).await;
        let mut source = RelaySource::new(1, max_bytes);
        source.timeout = Duration::from_millis(150);
        let error = source
            .exact_count(&relay.url, &query(&Keys::generate()))
            .await
            .unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
        assert!(source.sockets.is_empty());
    }
}

#[tokio::test]
async fn complete_ties_pass_but_hidden_capped_ties_remain_incomplete() {
    let keys = Keys::generate();
    for count in [2, 3] {
        let events = (0..count)
            .map(|n| event(&keys, 20, &n.to_string()))
            .collect();
        let relay = CountRelay::new(events, 2, Reply::Exact).await;
        let policy = CatchupPolicy {
            base_root: "unused".into(),
            authors_sha256: "unused".into(),
            author_count: 1,
            initial_since: 10,
            overlap_secs: 0,
            relays: vec![relay.url.clone()],
            kinds: vec![1],
            page_size: 4,
            max_pages_per_author: 20,
            max_events_per_author: 20,
            max_bytes_per_author: 65536,
            fetch_timeout_secs: 2,
            index_commit_batch_size: 4,
        };
        let result = fetch_catchup_author(
            &mut RelaySource::new(2, 65536),
            &policy,
            &keys.public_key().to_hex(),
            10,
            100,
        )
        .await;
        if count == 2 {
            assert_eq!(result.unwrap().len(), 2);
        } else {
            assert!(result.is_err());
        }
        let requests = relay.requests.lock().unwrap();
        let counts = requests
            .iter()
            .filter(|request| request[0] == "COUNT")
            .collect::<Vec<_>>();
        assert_eq!(counts.len(), 1);
        assert_eq!(counts[0][2]["since"], 20);
        assert_eq!(counts[0][2]["until"], 20);
    }
}
