use super::*;

struct ProbeRelay {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    task: tokio::task::JoinHandle<()>,
}

impl ProbeRelay {
    async fn new(events: Vec<Value>, eose: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let observed = requests.clone();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (events, requests) = (events.clone(), observed.clone());
                tokio::spawn(async move {
                    let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                    while let Some(Ok(Message::Text(raw))) = socket.next().await {
                        let request: Value = serde_json::from_str(&raw).unwrap();
                        if request[0] != "REQ" {
                            continue;
                        }
                        requests.lock().unwrap().push(request.clone());
                        for event in &events {
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
                        if eose
                            && socket
                                .send(Message::Text(json!(["EOSE", request[1]]).to_string()))
                                .await
                                .is_err()
                        {
                            return;
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
impl Drop for ProbeRelay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
async fn probe_omits_only_author_and_caches_verified_unique_cardinality() {
    let keys = Keys::generate();
    let other = Keys::generate();
    let first = serde_json::to_value(event(&keys, 20, "one")).unwrap();
    let second = serde_json::to_value(event(&other, 30, "two")).unwrap();
    let relay = ProbeRelay::new(vec![first.clone(), first, second], true).await;
    let mut source = RelaySource::new(2, 65536);
    let mut filter = query(&keys);
    assert_eq!(
        source.observed_capacity(&relay.url, &filter).await.unwrap(),
        2
    );
    filter.author = other.public_key().to_hex();
    assert_eq!(
        source.observed_capacity(&relay.url, &filter).await.unwrap(),
        2
    );
    {
        let requests = relay.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0][2],
            json!({"kinds":[1],"since":10,"until":100,"limit":4})
        );
    }
    // New time window or kind set must not inherit another filter's capacity.
    filter.until = 101;
    assert_eq!(
        source.observed_capacity(&relay.url, &filter).await.unwrap(),
        2
    );
    filter.kinds.push(5);
    assert_eq!(
        source.observed_capacity(&relay.url, &filter).await.unwrap(),
        2
    );
    assert_eq!(relay.requests.lock().unwrap().len(), 3);
    let empty = ProbeRelay::new(vec![], true).await;
    assert_eq!(
        source.observed_capacity(&empty.url, &filter).await.unwrap(),
        0
    );
    assert_eq!(
        source.observed_capacity(&empty.url, &filter).await.unwrap(),
        0
    );
    assert_eq!(empty.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn invalid_signature_time_kind_or_missing_eose_never_cache_capacity() {
    let keys = Keys::generate();
    let mut invalid = serde_json::to_value(event(&keys, 20, "valid")).unwrap();
    invalid["content"] = json!("tampered");
    let wrong_kind = EventBuilder::new(Kind::EventDeletion, "wrong kind")
        .custom_created_at(Timestamp::from_secs(20))
        .sign_with_keys(&keys)
        .unwrap();
    for (payload, eose, expected) in [
        (invalid, true, "event verification"),
        (
            serde_json::to_value(event(&keys, 101, "future")).unwrap(),
            true,
            "requested filter",
        ),
        (
            serde_json::to_value(wrong_kind).unwrap(),
            true,
            "requested filter",
        ),
        (
            serde_json::to_value(event(&keys, 20, "incomplete")).unwrap(),
            false,
            "timeout before EOSE",
        ),
    ] {
        let relay = ProbeRelay::new(vec![payload], eose).await;
        let mut source = RelaySource::new(1, 65536);
        source.timeout = Duration::from_millis(150);
        let error = source
            .observed_capacity(&relay.url, &query(&keys))
            .await
            .unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
        assert!(source.capacities.is_empty());
        assert!(source.sockets.is_empty());
    }
}

#[tokio::test]
async fn probe_reuses_event_byte_message_and_requested_limit_guards() {
    let keys = Keys::generate();
    let one = serde_json::to_value(event(&keys, 20, "one")).unwrap();
    let oversized = serde_json::to_value(event(&keys, 20, &"x".repeat(5000))).unwrap();
    let too_many = (0..5)
        .map(|n| serde_json::to_value(event(&keys, 20, &n.to_string())).unwrap())
        .collect();
    for (events, budget, expected) in [
        (vec![oversized], 0, "byte budget"),
        (vec![one; 117], 65536, "message budget"),
        (too_many, 65536, "requested event limit"),
    ] {
        let relay = ProbeRelay::new(events, true).await;
        let mut source = RelaySource::new(2, budget);
        let error = source
            .observed_capacity(&relay.url, &query(&keys))
            .await
            .unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
        assert!(source.capacities.is_empty());
    }
}

#[tokio::test]
async fn normal_author_query_still_rejects_other_authors() {
    let keys = Keys::generate();
    let other = Keys::generate();
    let relay = ProbeRelay::new(
        vec![serde_json::to_value(event(&other, 20, "other author")).unwrap()],
        true,
    )
    .await;
    let mut source = RelaySource::new(2, 65536);
    let error = source.query(&relay.url, &query(&keys)).await.unwrap_err();
    assert!(error.to_string().contains("requested filter"));
    let requests = relay.requests.lock().unwrap();
    assert_eq!(
        requests[0][2]["authors"],
        json!([keys.public_key().to_hex()])
    );
    assert!(source.capacities.is_empty());
}
