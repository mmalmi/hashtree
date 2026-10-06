use super::*;
use hashtree_nostr::catchup::{fetch_catchup_author, CatchupPolicy};
use nostr::{Event, EventBuilder, Keys, Kind, Timestamp};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio_tungstenite::{accept_hdr_async, tungstenite::protocol::CloseFrame};

mod capacity;
mod count;
mod gateway;

#[derive(Clone, Copy)]
enum Mode {
    Complete,
    InvalidEvent,
    SubscriptionClosed,
    PolicyClose,
}

struct Relay {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    connections: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Relay {
    async fn new(
        events: Vec<Event>,
        failures: usize,
        partial: Option<Event>,
        mode: Mode,
        close_after_first: bool,
        notice_bytes: usize,
    ) -> Self {
        Self::with_upgrades(
            events,
            failures,
            partial,
            mode,
            close_after_first,
            notice_bytes,
            vec![],
        )
        .await
    }

    async fn with_upgrades(
        events: Vec<Event>,
        failures: usize,
        partial: Option<Event>,
        mode: Mode,
        close_after_first: bool,
        notice_bytes: usize,
        upgrades: Vec<(u16, Option<String>)>,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(AtomicUsize::new(0));
        let remaining = Arc::new(AtomicUsize::new(failures));
        let close_once = Arc::new(AtomicBool::new(close_after_first));
        let upgrades = Arc::new(Mutex::new(std::collections::VecDeque::from(upgrades)));
        let (observed, count) = (requests.clone(), connections.clone());
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                count.fetch_add(1, Ordering::SeqCst);
                let (requests, remaining, close_once) =
                    (observed.clone(), remaining.clone(), close_once.clone());
                let (events, partial) = (events.clone(), partial.clone());
                let upgrade = upgrades.lock().unwrap().pop_front();
                tokio::spawn(async move {
                    let Ok(mut socket) = accept_hdr_async(stream, move |_: &tokio_tungstenite::tungstenite::handshake::server::Request,
                        response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                        if let Some((status, retry_after)) = upgrade {
                            let mut response = tokio_tungstenite::tungstenite::http::Response::builder().status(status);
                            if let Some(value) = retry_after { response = response.header("Retry-After", value); }
                            Err(response.body(Some("private gateway detail".to_owned())).unwrap())
                        } else { Ok(response) }
                    }).await else { return; };
                    while let Some(Ok(Message::Text(raw))) = socket.next().await {
                        let request: Value = serde_json::from_str(&raw).unwrap();
                        if request[0] != "REQ" {
                            continue;
                        }
                        requests.lock().unwrap().push(request.clone());
                        let subscription = &request[1];
                        let filter = &request[2];
                        if notice_bytes > 0 {
                            if socket
                                .send(Message::Text(
                                    json!(["NOTICE", "x".repeat(notice_bytes)]).to_string(),
                                ))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                        if remaining
                            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                            .is_ok()
                        {
                            if let Some(event) = partial {
                                let _ = socket
                                    .send(Message::Text(
                                        json!(["EVENT", subscription, event]).to_string(),
                                    ))
                                    .await;
                            }
                            // Drop TCP without a WebSocket close handshake, as
                            // in the observed relay reset before EOSE.
                            return;
                        }
                        match mode {
                            Mode::SubscriptionClosed => {
                                let _ = socket
                                    .send(Message::Text(
                                        json!(["CLOSED", subscription, "blocked"]).to_string(),
                                    ))
                                    .await;
                                continue;
                            }
                            Mode::PolicyClose => {
                                let _ = socket
                                    .send(Message::Close(Some(CloseFrame {
                                        code: CloseCode::Policy,
                                        reason: "policy".into(),
                                    })))
                                    .await;
                                return;
                            }
                            Mode::InvalidEvent => {
                                let mut event = serde_json::to_value(&events[0]).unwrap();
                                event["content"] = json!("tampered");
                                let _ = socket
                                    .send(Message::Text(
                                        json!(["EVENT", subscription, event]).to_string(),
                                    ))
                                    .await;
                                continue;
                            }
                            Mode::Complete => {}
                        }
                        let mut matching = events
                            .iter()
                            .filter(|event| {
                                event.pubkey.to_hex() == filter["authors"][0].as_str().unwrap()
                                    && event.created_at.as_secs()
                                        >= filter["since"].as_u64().unwrap()
                                    && event.created_at.as_secs()
                                        <= filter["until"].as_u64().unwrap()
                            })
                            .cloned()
                            .collect::<Vec<_>>();
                        matching.sort_by_key(|event| std::cmp::Reverse(event.created_at));
                        matching.truncate(filter["limit"].as_u64().unwrap() as usize);
                        for event in matching {
                            if socket
                                .send(Message::Text(
                                    json!(["EVENT", subscription, event]).to_string(),
                                ))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                        if socket
                            .send(Message::Text(json!(["EOSE", subscription]).to_string()))
                            .await
                            .is_err()
                        {
                            return;
                        }
                        if close_once.swap(false, Ordering::SeqCst) {
                            let _ = socket.close(None).await;
                            return;
                        }
                    }
                });
            }
        });
        Self {
            url,
            requests,
            connections,
            task,
        }
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn event(keys: &Keys, at: u64, content: &str) -> Event {
    EventBuilder::new(Kind::TextNote, content)
        .custom_created_at(Timestamp::from_secs(at))
        .sign_with_keys(keys)
        .unwrap()
}

fn query(keys: &Keys) -> CatchupQuery {
    CatchupQuery {
        author: keys.public_key().to_hex(),
        kinds: vec![1],
        since: 10,
        until: 100,
        limit: 4,
    }
}

#[tokio::test]
async fn reset_reconnects_exact_page_and_discards_partial_events() {
    let keys = Keys::generate();
    let expected = event(&keys, 20, "complete attempt");
    let partial = event(&keys, 30, "only the failed attempt");
    let relay = Relay::new(
        vec![expected.clone()],
        1,
        Some(partial),
        Mode::Complete,
        false,
        0,
    )
    .await;
    let events = RelaySource::new(2, 65536)
        .query(&relay.url, &query(&keys))
        .await
        .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].id, expected.id.to_hex());
    let requests = relay.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0][2], requests[1][2],
        "retry must freeze the exact filter"
    );
    assert_ne!(
        requests[0][1], requests[1][1],
        "retry gets its own subscription"
    );
    assert_eq!(relay.connections.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn closed_idle_socket_reconnects_between_successful_queries() {
    let keys = Keys::generate();
    let relay = Relay::new(
        vec![event(&keys, 20, "event")],
        0,
        None,
        Mode::Complete,
        true,
        0,
    )
    .await;
    let mut source = RelaySource::new(2, 65536);
    let first = source.query(&relay.url, &query(&keys)).await.unwrap();
    let second = source.query(&relay.url, &query(&keys)).await.unwrap();
    assert_eq!(first, second);
    assert_eq!(relay.connections.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn later_relay_reconnect_does_not_refetch_completed_source() {
    let keys = Keys::generate();
    let first = Relay::new(
        vec![event(&keys, 20, "first")],
        0,
        None,
        Mode::Complete,
        false,
        0,
    )
    .await;
    let second = Relay::new(
        vec![event(&keys, 30, "second")],
        1,
        None,
        Mode::Complete,
        false,
        0,
    )
    .await;
    let policy = CatchupPolicy {
        base_root: "unused".into(),
        source_mode: Default::default(),
        authors_sha256: "unused".into(),
        author_count: 1,
        initial_since: 10,
        overlap_secs: 0,
        relays: vec![first.url.clone(), second.url.clone()],
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
    .await
    .unwrap();
    assert_eq!(result.len(), 2);
    assert_eq!(
        first.requests.lock().unwrap().len(),
        3,
        "broad page, boundary, and empty tail only"
    );
    assert_eq!(
        second.requests.lock().unwrap().len(),
        4,
        "only interrupted broad page retried"
    );
}

#[tokio::test]
async fn reconnect_exhaustion_and_delay_share_original_deadline() {
    let keys = Keys::generate();
    let relay = Relay::new(vec![], usize::MAX, None, Mode::Complete, false, 0).await;
    let mut source = RelaySource::new(2, 65536);
    assert!(source.query(&relay.url, &query(&keys)).await.is_err());
    assert_eq!(relay.connections.load(Ordering::SeqCst), 2);
    assert!(source.sockets.is_empty());
    let bounded = Relay::new(vec![], usize::MAX, None, Mode::Complete, false, 0).await;
    source.timeout = Duration::from_millis(50);
    let started = Instant::now();
    let error = source.query(&bounded.url, &query(&keys)).await.unwrap_err();
    assert!(error.to_string().contains("timeout before EOSE"));
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(
        bounded.connections.load(Ordering::SeqCst),
        1,
        "deadline must cancel retry delay"
    );
}

#[tokio::test]
async fn invalid_events_subscription_rejection_and_policy_close_never_retry() {
    let keys = Keys::generate();
    for mode in [
        Mode::InvalidEvent,
        Mode::SubscriptionClosed,
        Mode::PolicyClose,
    ] {
        let relay = Relay::new(vec![event(&keys, 20, "signed")], 0, None, mode, false, 0).await;
        assert!(RelaySource::new(2, 65536)
            .query(&relay.url, &query(&keys))
            .await
            .is_err());
        assert_eq!(relay.connections.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn wire_budget_is_shared_across_attempts_and_budget_errors_do_not_retry() {
    let keys = Keys::generate();
    let relay = Relay::new(vec![], 1, None, Mode::Complete, false, 3000).await;
    let error = RelaySource::new(2, 0)
        .query(&relay.url, &query(&keys))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("source byte budget"));
    assert_eq!(relay.connections.load(Ordering::SeqCst), 2);
    let oversized = Relay::new(vec![], 0, None, Mode::Complete, false, 5000).await;
    assert!(RelaySource::new(2, 0)
        .query(&oversized.url, &query(&keys))
        .await
        .is_err());
    assert_eq!(oversized.connections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn completed_eose_survives_failed_close_and_discards_unhealthy_socket() {
    let keys = Keys::generate();
    let relay = Relay::new(
        vec![event(&keys, 20, "complete")],
        0,
        None,
        Mode::Complete,
        false,
        0,
    )
    .await;
    let mut source = RelaySource::new(2, 65536);
    let page = source
        .query_inner(&relay.url, &query(&keys), &mut QueryBudget::default(), true)
        .await
        .unwrap();
    // Closing locally makes the next application-data write deterministically
    // fail with SendAfterClosing, independent of TCP delivery timing.
    source
        .sockets
        .get_mut(&relay.url)
        .unwrap()
        .close(None)
        .await
        .unwrap();
    let events = source
        .finish_page(&relay.url, page, Instant::now() + Duration::from_secs(1))
        .await;
    assert_eq!(events.len(), 1);
    assert!(!source.sockets.contains_key(&relay.url));
    assert_eq!(
        relay.requests.lock().unwrap().len(),
        1,
        "completed page not refetched"
    );
}

#[tokio::test]
async fn forbidden_and_rate_limited_upgrade_never_reconnect() {
    for status in [403u16, 429] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let count = Arc::new(AtomicUsize::new(0));
        let observed = count.clone();
        let server = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                observed.fetch_add(1, Ordering::SeqCst);
                let _ = accept_hdr_async(stream, move |_: &tokio_tungstenite::tungstenite::handshake::server::Request,
                    _: tokio_tungstenite::tungstenite::handshake::server::Response| {
                    Err(tokio_tungstenite::tungstenite::http::Response::builder()
                        .status(status)
                        .header("Retry-After", "60")
                        .body(Some("blocked".to_string()))
                        .unwrap())
                })
                .await;
            }
        });
        let error = RelaySource::new(2, 65536)
            .query(&url, &query(&Keys::generate()))
            .await
            .unwrap_err();
        assert!(error.to_string().contains(&status.to_string()));
        assert_eq!(count.load(Ordering::SeqCst), 1);
        server.abort();
    }
}

#[test]
fn tls_and_other_protocol_errors_are_terminal() {
    for error in [
        WebSocketError::Tls(tokio_tungstenite::tungstenite::error::TlsError::InvalidDnsName),
        WebSocketError::Protocol(ProtocolError::SendAfterClosing),
        WebSocketError::Utf8,
    ] {
        assert!(!QueryFailure::transport("test", error).reconnect);
    }
}

#[tokio::test]
async fn wss_source_starts_tls_before_websocket_handshake() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("wss://localhost:{}", listener.local_addr().unwrap().port());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut header = [0; 5];
        tokio::time::timeout(Duration::from_secs(5), socket.read_exact(&mut header))
            .await
            .expect("TLS ClientHello timeout")
            .expect("WSS client must start TLS, not reject an uncompiled backend");
        assert_eq!(header[0], 0x16, "first record must be a TLS handshake");
        assert_eq!(header[1], 0x03, "TLS record version");
    });
    let error = RelaySource::new(5, 1024)
        .query(
            &url,
            &CatchupQuery {
                author: "00".repeat(32),
                kinds: vec![1],
                since: 0,
                until: 1,
                limit: 1,
            },
        )
        .await
        .unwrap_err();
    server.await.unwrap();
    assert!(!error.to_string().contains("TLS support not compiled in"));
    // The local listener deliberately closes before presenting a trusted
    // certificate. WSS must fail, never downgrade to plaintext Nostr.
    assert!(error.to_string().starts_with("connect:"));
}
