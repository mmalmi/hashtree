mod common;

use common::test_relay::{TestRelay, TestRelayOptions};
use git_remote_htree::nostr_client::{NostrClient, KIND_HASHTREE_ROOT};
use hashtree_config::Config;
use nostr::prelude::Keys;
use std::io::{Read, Write};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

#[test]
fn test_fetch_refs_uses_partial_relay_results_instead_of_not_found() {
    let good_relay = TestRelay::new();
    let hanging_relay = TestRelay::with_options(TestRelayOptions {
        // Simulate a relay that never answers hashtree-root REQ.
        ignore_req_kinds: vec![KIND_HASHTREE_ROOT as u64],
        ..Default::default()
    });

    let keys = Keys::generate();
    let pubkey_hex = hex::encode(keys.public_key().to_bytes());
    let secret_hex = hex::encode(keys.secret_key().to_secret_bytes());

    let mut config = Config::default();
    config.nostr.relays = vec![good_relay.url(), hanging_relay.url()];
    // Force a deterministic failure *after* event discovery. If event discovery fails,
    // we'd get "Repository ... not found" instead.
    config.blossom.read_servers = vec!["http://127.0.0.1:9".to_string()];
    config.blossom.write_servers = config.blossom.read_servers.clone();

    let publisher = NostrClient::new(&pubkey_hex, Some(secret_hex), None, false, &config)
        .expect("publisher client");
    publisher
        .publish_repo(
            "relay-timeout-repro",
            "1111111111111111111111111111111111111111111111111111111111111111",
            None,
        )
        .expect("publish to relay");

    let mut reader =
        NostrClient::new(&pubkey_hex, None, None, false, &config).expect("reader client");
    let err = reader
        .fetch_refs("relay-timeout-repro")
        .expect_err("fetch should fail at blossom download stage")
        .to_string();

    assert!(
        !err.contains("Repository 'relay-timeout-repro' not found"),
        "should not report missing repo when one relay has the event; got: {}",
        err
    );
    assert!(
        err.contains("Failed to download root hash"),
        "should fail after resolving event and trying blossom download; got: {}",
        err
    );
}

#[test]
fn test_fetch_refs_retries_after_empty_repo_lookup_before_reporting_not_found() {
    let flaky_relay = TestRelay::with_options(TestRelayOptions {
        // First repo lookup returns EOSE without the historical event, so the
        // client needs to retry discovery instead of surfacing a false "not found".
        respond_empty_req_kinds_once: vec![KIND_HASHTREE_ROOT as u64],
        ..Default::default()
    });

    let keys = Keys::generate();
    let pubkey_hex = hex::encode(keys.public_key().to_bytes());
    let secret_hex = hex::encode(keys.secret_key().to_secret_bytes());

    let mut config = Config::default();
    config.nostr.relays = vec![flaky_relay.url()];
    config.blossom.read_servers = vec!["http://127.0.0.1:9".to_string()];
    config.blossom.write_servers = config.blossom.read_servers.clone();

    let publisher = NostrClient::new(&pubkey_hex, Some(secret_hex), None, false, &config)
        .expect("publisher client");
    publisher
        .publish_repo(
            "retry-after-empty-lookup",
            "2222222222222222222222222222222222222222222222222222222222222222",
            None,
        )
        .expect("publish to relay");

    let mut reader =
        NostrClient::new(&pubkey_hex, None, None, false, &config).expect("reader client");
    let err = reader
        .fetch_refs("retry-after-empty-lookup")
        .expect_err("fetch should fail at blossom download stage")
        .to_string();

    assert!(
        !err.contains("Repository 'retry-after-empty-lookup' not found"),
        "should retry repo discovery before reporting missing repo; got: {}",
        err
    );
    assert!(
        err.contains("Failed to download root hash"),
        "should fail after resolving the event and trying blossom download; got: {}",
        err
    );
}

#[test]
fn test_fetch_refs_discards_bad_local_daemon_root_and_retries_relays() {
    let relay = TestRelay::new();
    let daemon_root = "aa".repeat(32);
    let relay_root = "bb".repeat(32);
    let fake_daemon = FakeResolveDaemon::start(daemon_root.clone());

    let keys = Keys::generate();
    let pubkey_hex = hex::encode(keys.public_key().to_bytes());
    let secret_hex = hex::encode(keys.secret_key().to_secret_bytes());

    let mut config = Config::default();
    config.nostr.relays = vec![relay.url()];
    config.blossom.read_servers = vec![fake_daemon.base_url(), "http://127.0.0.1:9".to_string()];
    config.blossom.write_servers.clear();

    let publisher = NostrClient::new(&pubkey_hex, Some(secret_hex), None, false, &config)
        .expect("publisher client");
    publisher
        .publish_repo("daemon-fallback-repro", &relay_root, None)
        .expect("publish to relay");

    let mut reader =
        NostrClient::new(&pubkey_hex, None, None, false, &config).expect("reader client");
    let err = reader
        .fetch_refs("daemon-fallback-repro")
        .expect_err("fetch should fail at blossom download stage")
        .to_string();

    fake_daemon.stop();

    assert!(
        err.contains(&relay_root[..12]),
        "final error should come from the relay root, got: {err}"
    );
    assert_eq!(
        reader.get_cached_root_hash("daemon-fallback-repro"),
        Some(&relay_root)
    );
}

struct FakeResolveDaemon {
    url: String,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FakeResolveDaemon {
    fn start(root_hash: String) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fake daemon");
        listener
            .set_nonblocking(true)
            .expect("set fake daemon nonblocking");
        let addr = listener.local_addr().expect("fake daemon addr");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);

        let thread = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(15);
            while !stop_for_thread.load(Ordering::Relaxed) && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut request = [0; 2048];
                        let read = stream.read(&mut request).unwrap_or(0);
                        let request = String::from_utf8_lossy(&request[..read]);
                        if request.starts_with("GET /api/nostr/resolve/") {
                            let body = serde_json::json!({
                                "hash": root_hash,
                                "source": "stale-cache",
                            })
                            .to_string();
                            write!(
                                stream,
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            )
                            .ok();
                        } else {
                            write!(
                                stream,
                                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            )
                            .ok();
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            url: format!("http://{}", addr),
            stop,
            thread: Some(thread),
        }
    }

    fn base_url(&self) -> String {
        self.url.clone()
    }

    fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for FakeResolveDaemon {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
