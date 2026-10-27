use hashtree_client::{BlobStore, ClientConfig};
use hashtree_core::{sha256, Store};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    task::JoinHandle,
};

const PAYLOAD: &[u8] = b"verified block contents";

#[derive(Clone, Copy)]
enum Behavior {
    TruncateFirst,
    TruncateAlways,
    Corrupt,
    Missing,
    Oversized,
    StatusFirst(u16),
    StatusAlways(u16),
}

struct TestServer {
    url: String,
    reads: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl TestServer {
    async fn start(behavior: Behavior) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let reads = Arc::new(AtomicUsize::new(0));
        let count = reads.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    if socket.read(&mut byte).await.unwrap() == 0 {
                        break;
                    }
                    request.push(byte[0]);
                    assert!(request.len() < 8192);
                }
                let attempt = count.fetch_add(1, Ordering::SeqCst);
                let code = match behavior {
                    Behavior::StatusAlways(code) => code,
                    Behavior::StatusFirst(code) if attempt == 0 => code,
                    _ => 200,
                };
                let status_error = format!("{code} Test Status");
                let (status, body, declared) = match behavior {
                    Behavior::StatusFirst(_) | Behavior::StatusAlways(_) if code != 200 => {
                        (status_error.as_str(), &b""[..], 0)
                    }
                    Behavior::StatusFirst(_) | Behavior::StatusAlways(_) => {
                        ("200 OK", PAYLOAD, PAYLOAD.len())
                    }
                    Behavior::Oversized => ("200 OK", &b""[..], 4 * 1024 * 1024 + 1),
                    Behavior::Missing => ("404 Not Found", &b""[..], 0),
                    Behavior::Corrupt => ("200 OK", &b"wrong contents"[..], 14),
                    Behavior::TruncateAlways => ("200 OK", &PAYLOAD[..3], PAYLOAD.len()),
                    Behavior::TruncateFirst if attempt == 0 => {
                        ("200 OK", &PAYLOAD[..3], PAYLOAD.len())
                    }
                    Behavior::TruncateFirst => ("200 OK", PAYLOAD, PAYLOAD.len()),
                };
                let header = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {declared}\r\nConnection: close\r\n\r\n"
                );
                socket.write_all(header.as_bytes()).await.unwrap();
                socket.write_all(body).await.unwrap();
                socket.shutdown().await.unwrap();
            }
        });
        Self { url, reads, task }
    }

    async fn stop(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

fn config(sources: Vec<String>) -> ClientConfig {
    ClientConfig {
        daemon_url: None,
        local_only: false,
        relays: vec![],
        read_servers: sources,
        resolve_window: Duration::from_millis(50),
        request_timeout: Duration::from_secs(2),
    }
}

#[tokio::test]
async fn interrupted_body_is_retried_once_and_only_verified_bytes_are_cached() {
    let server = TestServer::start(Behavior::TruncateFirst).await;
    let temp = tempfile::tempdir().unwrap();
    let store = BlobStore::new(temp.path(), config(vec![server.url.clone()])).unwrap();
    let hash = sha256(PAYLOAD);
    let result = store.get(&hash).await;
    let reads = server.reads.load(Ordering::SeqCst);
    server.stop().await;
    assert_eq!(result.unwrap().unwrap(), PAYLOAD);
    assert_eq!(reads, 2);
    // The server is gone: the complete hash-verified block must now be cached.
    assert_eq!(store.get(&hash).await.unwrap().unwrap(), PAYLOAD);
}

#[tokio::test]
async fn persistent_body_failure_is_bounded_and_keeps_source_hash_and_cause() {
    let server = TestServer::start(Behavior::TruncateAlways).await;
    let temp = tempfile::tempdir().unwrap();
    let secret_url = server
        .url
        .replacen("http://", "http://private-user:private-password@", 1)
        + "/private-path?token=private-token";
    let store = BlobStore::new(temp.path(), config(vec![secret_url])).unwrap();
    let hash = sha256(PAYLOAD);
    let error = store.get(&hash).await.unwrap_err().to_string();
    let reads = server.reads.load(Ordering::SeqCst);
    let origin = server.url.clone();
    server.stop().await;
    assert_eq!(reads, 2, "retry budget must be finite");
    assert!(error.contains(&origin), "{error}");
    assert!(error.contains(&hex::encode(hash)), "{error}");
    assert!(
        error.contains("end of file before message length reached"),
        "{error}"
    );
    for private in [
        "private-user",
        "private-password",
        "private-path",
        "private-token",
    ] {
        assert!(
            !error.contains(private),
            "diagnostics leaked URL details: {error}"
        );
    }
}

#[tokio::test]
async fn corrupt_blocks_and_definite_misses_are_not_retried() {
    for behavior in [Behavior::Corrupt, Behavior::Missing, Behavior::Oversized] {
        let server = TestServer::start(behavior).await;
        let temp = tempfile::tempdir().unwrap();
        let store = BlobStore::new(temp.path(), config(vec![server.url.clone()])).unwrap();
        let result = store.get(&sha256(PAYLOAD)).await;
        let reads = server.reads.load(Ordering::SeqCst);
        server.stop().await;
        assert_eq!(reads, 1);
        match behavior {
            Behavior::Corrupt => assert!(result
                .unwrap_err()
                .to_string()
                .contains("invalid Hashtree block hash or size")),
            Behavior::Missing => assert!(result.unwrap().is_none()),
            Behavior::Oversized => assert!(result.unwrap_err().to_string().contains("oversized")),
            _ => unreachable!(),
        }
    }
}

#[tokio::test]
async fn failures_retain_each_source_instead_of_only_the_last_error() {
    let first = TestServer::start(Behavior::TruncateAlways).await;
    let second = TestServer::start(Behavior::Corrupt).await;
    let temp = tempfile::tempdir().unwrap();
    let store = BlobStore::new(
        temp.path(),
        config(vec![first.url.clone(), second.url.clone()]),
    )
    .unwrap();
    let error = store.get(&sha256(PAYLOAD)).await.unwrap_err().to_string();
    let origins = [first.url.clone(), second.url.clone()];
    first.stop().await;
    second.stop().await;
    for origin in origins {
        assert!(error.contains(&origin), "{error}");
    }
    assert!(
        error.contains("invalid Hashtree block hash or size"),
        "{error}"
    );
    assert!(
        error.contains("end of file before message length reached"),
        "{error}"
    );
}

#[tokio::test]
async fn transient_http_statuses_retry_once_and_cache_verified_success() {
    for code in [429, 500, 502, 503, 504, 520] {
        let server = TestServer::start(Behavior::StatusFirst(code)).await;
        let temp = tempfile::tempdir().unwrap();
        let store = BlobStore::new(temp.path(), config(vec![server.url.clone()])).unwrap();
        assert_eq!(store.get(&sha256(PAYLOAD)).await.unwrap().unwrap(), PAYLOAD);
        assert_eq!(server.reads.load(Ordering::SeqCst), 2);
        server.stop().await;
        assert_eq!(store.get(&sha256(PAYLOAD)).await.unwrap().unwrap(), PAYLOAD);
    }
}

#[tokio::test]
async fn persistent_http_errors_have_a_bounded_retry_budget() {
    for (code, attempts) in [(429, 2), (503, 2), (520, 2), (400, 1), (401, 1), (403, 1)] {
        let server = TestServer::start(Behavior::StatusAlways(code)).await;
        let temp = tempfile::tempdir().unwrap();
        let store = BlobStore::new(temp.path(), config(vec![server.url.clone()])).unwrap();
        let error = store.get(&sha256(PAYLOAD)).await.unwrap_err().to_string();
        assert!(error.contains(&code.to_string()), "{error}");
        assert_eq!(server.reads.load(Ordering::SeqCst), attempts);
        server.stop().await;
    }
}

#[tokio::test]
async fn a_healthy_hedged_source_can_finish_during_retry_backoff() {
    let failing = TestServer::start(Behavior::StatusAlways(520)).await;
    let healthy = TestServer::start(Behavior::StatusAlways(200)).await;
    let temp = tempfile::tempdir().unwrap();
    let store = BlobStore::new(
        temp.path(),
        config(vec![failing.url.clone(), healthy.url.clone()]),
    )
    .unwrap();
    assert_eq!(store.get(&sha256(PAYLOAD)).await.unwrap().unwrap(), PAYLOAD);
    assert_eq!(healthy.reads.load(Ordering::SeqCst), 1);
    assert!(failing.reads.load(Ordering::SeqCst) <= 2);
    failing.stop().await;
    healthy.stop().await;
}
