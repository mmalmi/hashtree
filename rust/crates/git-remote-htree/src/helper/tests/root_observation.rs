use super::*;
use hashtree_core::{nhash_encode_full, NHashData};
use hashtree_nostr::{stored_event_from_nostr_sdk_event, NostrEventStore};
use nostr::{EventBuilder, Kind, Tag};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct SourceRequests {
    reads: AtomicUsize,
    writes: AtomicUsize,
}

struct UnavailablePrivateSource {
    url: String,
    requests: Arc<SourceRequests>,
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl UnavailablePrivateSource {
    fn new() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/private", listener.local_addr().unwrap());
        let requests = Arc::new(SourceRequests::default());
        let state = requests.clone();
        let (shutdown, stopped) = oneshot::channel();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let app = Router::new()
                    .fallback(
                        |State(requests): State<Arc<SourceRequests>>,
                         method: axum::http::Method| async move {
                            if method == axum::http::Method::GET
                                || method == axum::http::Method::HEAD
                            {
                                requests.reads.fetch_add(1, Ordering::SeqCst);
                            } else {
                                requests.writes.fetch_add(1, Ordering::SeqCst);
                            }
                            StatusCode::SERVICE_UNAVAILABLE
                        },
                    )
                    .with_state(state);
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                axum::serve(listener, app)
                    .with_graceful_shutdown(async {
                        let _ = stopped.await;
                    })
                    .await
                    .unwrap();
            });
        });
        Self {
            url,
            requests,
            shutdown: Some(shutdown),
            thread: Some(thread),
        }
    }
}

impl Drop for UnavailablePrivateSource {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            thread.join().expect("local source thread");
        }
    }
}

#[test]
fn ordinary_push_rejects_incomplete_observation_despite_private_in_source_url() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let (home, repo, _, _, _) = create_repo_with_diverged_master_and_dev();
    let _home = HomeGuard::set(home.path());
    let _cwd = CwdGuard::set(repo.path());
    let _git_dir = EnvGuard::clear("GIT_DIR");
    let _mode = EnvGuard::clear("HTREE_GIT_REBUILD_FROM_LOCAL");
    let _relays = EnvGuard::clear("NOSTR_RELAYS");
    let _local = EnvGuard::set("NOSTR_PREFER_LOCAL", "0");
    let _daemon = EnvGuard::set("HTREE_PREFER_LOCAL_DAEMON", "0");
    let _local_only = EnvGuard::set("HTREE_LOCAL_DAEMON_ONLY", "0");
    let data = TempDir::new().unwrap();
    let _data = EnvGuard::set("HTREE_DATA_DIR", data.path().to_str().unwrap());
    let source = UnavailablePrivateSource::new();
    let keys = nostr::Keys::generate();
    let event = EventBuilder::new(Kind::Custom(30064), "")
        .tags([
            Tag::identifier("test-repo"),
            Tag::parse(["l", "hashtree"]).unwrap(),
            Tag::parse(["hash", &"11".repeat(32)]).unwrap(),
        ])
        .sign_with_keys(&keys)
        .unwrap();
    // Build an actual signed-event index, then retain only its valid checkpoint.
    // Its absent blocks must exercise real local/HTTP cache reads.
    let head = block_on_result(async {
        Ok(NostrEventStore::new(Arc::new(MemoryStore::new()))
            .build(None, [stored_event_from_nostr_sdk_event(&event)])
            .await?
            .unwrap())
    })
    .unwrap();
    let cache = data.path().join("git-root-events");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(
        cache.join("head.nhash"),
        nhash_encode_full(&NHashData {
            hash: head.hash,
            decrypt_key: head.key,
        })
        .unwrap(),
    )
    .unwrap();
    let mut config = Config::default();
    config.storage.data_dir = data.path().to_string_lossy().into_owned();
    config.server.bind_address = "127.0.0.1:0".to_string();
    config.nostr.relays.clear();
    config.blossom.servers.clear();
    config.blossom.read_servers = vec![source.url.clone()];
    config.blossom.write_servers = vec![source.url.clone()];
    let mut helper = RemoteHelper::new(
        &keys.public_key().to_hex(),
        "test-repo",
        Some(hex::encode(keys.secret_key().to_secret_bytes())),
        None,
        false,
        config,
    )
    .unwrap();

    let error = helper.nostr.fetch_refs_with_root("test-repo").unwrap_err();
    assert!(error
        .downcast_ref::<crate::nostr_client::RootObservationIncomplete>()
        .is_some());
    assert!(error.to_string().contains("private"), "{error}");
    helper
        .queue_push("refs/heads/master:refs/heads/master")
        .unwrap();
    let outcome = helper.execute_push();
    assert!(source.requests.reads.load(Ordering::SeqCst) > 0);
    assert_eq!(
        source.requests.writes.load(Ordering::SeqCst),
        0,
        "incomplete root observation must reject ordinary push before upload/publication"
    );
    let statuses = outcome.unwrap().unwrap();
    assert!(
        statuses.iter().any(|status| status.contains("remote-state-unreadable")),
        "incomplete observation cannot be treated as a private-repo visibility change: {statuses:?}"
    );
    assert!(!statuses.iter().any(|status| status.starts_with("ok ")));
}
