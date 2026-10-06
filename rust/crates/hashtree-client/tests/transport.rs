use axum::{
    extract::{ws::Message, Path, State, WebSocketUpgrade},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use hashtree_client::{Client, ClientConfig, Reference};
use hashtree_core::{HashTree, HashTreeConfig, MemoryStore, Store};
use hashtree_resolver::nostr::NostrRootResolver;
use nostr::{nips::nip19::ToBech32, Event, Keys};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

#[derive(Clone)]
struct Fixture {
    store: Arc<MemoryStore>,
    events: Vec<Event>,
    reads: Arc<AtomicUsize>,
    corrupt: bool,
    primed: Arc<AtomicBool>,
    require_prime: bool,
}

async fn ws(State(state): State<Fixture>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |mut socket| async move {
        while let Some(Ok(message)) = socket.recv().await {
            let Message::Text(text) = message else {
                continue;
            };
            let value: serde_json::Value = serde_json::from_str(&text).unwrap();
            if value[0] != "REQ" {
                continue;
            }
            let id = &value[1];
            socket
                .send(Message::Text(serde_json::json!(["EOSE", id]).to_string()))
                .await
                .unwrap();
            // The client must keep observing after EOSE.
            tokio::time::sleep(Duration::from_millis(30)).await;
            if state.require_prime && !state.primed.load(Ordering::SeqCst) {
                continue;
            }
            for event in &state.events {
                if socket
                    .send(Message::Text(
                        serde_json::json!(["EVENT", id, event]).to_string(),
                    ))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
    })
}

async fn blob(State(state): State<Fixture>, Path(hash): Path<String>) -> Response {
    state.reads.fetch_add(1, Ordering::SeqCst);
    if state.corrupt {
        return b"corrupt".to_vec().into_response();
    }
    let hash: [u8; 32] = hex::decode(
        hash.strip_suffix(".bin")
            .expect("raw block requests require .bin"),
    )
    .unwrap()
    .try_into()
    .unwrap();
    match state.store.get(&hash).await.unwrap() {
        Some(data) => data.into_response(),
        None => axum::http::StatusCode::NOT_FOUND.into_response(),
    }
}

async fn serve(fixture: Fixture) -> (String, tokio::task::JoinHandle<()>) {
    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/ws", get(ws))
        .route(
            "/api/nostr/resolve/:publisher/:tree",
            get(|State(state): State<Fixture>| async move {
                state.primed.store(true, Ordering::SeqCst);
                "{\"cid\":\"untrusted daemon hint\"}"
            }),
        )
        .route("/:hash", get(blob))
        .with_state(fixture);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    (
        url,
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }),
    )
}

fn config(
    daemon_url: Option<String>,
    relays: Vec<String>,
    read_servers: Vec<String>,
) -> ClientConfig {
    ClientConfig {
        daemon_url,
        local_only: false,
        relays,
        read_servers,
        resolve_window: Duration::from_secs(1),
        request_timeout: Duration::from_secs(2),
    }
}

#[tokio::test]
async fn reads_the_same_signed_tree_standalone_and_through_a_daemon() -> anyhow::Result<()> {
    let author = Keys::generate();
    let store = Arc::new(MemoryStore::new());
    let tree = HashTree::new(HashTreeConfig::new(store.clone()));
    let (cid, _) = tree.put(b"verified package bytes").await?;
    let empty = tree.put_directory(vec![]).await?;
    let root = tree
        .set_entry(
            &empty,
            &[],
            "catalog.json",
            &cid,
            22,
            hashtree_core::LinkType::Blob,
        )
        .await?;
    let event = NostrRootResolver::root_event_builder("packages/test", &root, None)
        .sign_with_keys(&author)?;
    let reads = Arc::new(AtomicUsize::new(0));
    let (url, task) = serve(Fixture {
        store,
        events: vec![event],
        reads: reads.clone(),
        primed: Arc::new(AtomicBool::new(false)),
        require_prime: false,
        corrupt: false,
    })
    .await;
    let reference = Reference::parse(&format!(
        "htree://{}/packages%2Ftest",
        author.public_key().to_bech32()?
    ))?;
    for daemon in [false, true] {
        let temp = tempfile::tempdir()?;
        let cfg = if daemon {
            config(Some(url.clone()), vec![], vec![])
        } else {
            config(
                None,
                vec![url.replace("http:", "ws:") + "/ws"],
                vec![url.clone()],
            )
        };
        let client = Client::new(cfg, temp.path())?;
        let resolved = client.resolve(&reference).await?;
        assert_eq!(resolved, root);
        assert_eq!(
            client.read_file(&resolved, "catalog.json", 1024).await?,
            b"verified package bytes"
        );
        let after_download = reads.load(Ordering::SeqCst);
        assert_eq!(
            client.read_file(&resolved, "catalog.json", 1024).await?,
            b"verified package bytes"
        );
        assert_eq!(
            reads.load(Ordering::SeqCst),
            after_download,
            "verified local cache must serve repeat reads"
        );
    }
    task.abort();
    Ok(())
}

#[tokio::test]
async fn rejects_corrupt_blocks_and_can_use_another_verified_source() -> anyhow::Result<()> {
    let store = Arc::new(MemoryStore::new());
    let tree = HashTree::new(HashTreeConfig::new(store.clone()));
    let (cid, _) = tree.put(b"intact").await?;
    let fixture = Fixture {
        store,
        events: vec![],
        reads: Arc::new(AtomicUsize::new(0)),
        primed: Arc::new(AtomicBool::new(false)),
        require_prime: false,
        corrupt: true,
    };
    let (bad, bad_task) = serve(fixture.clone()).await;
    let (good, good_task) = serve(Fixture {
        primed: Arc::new(AtomicBool::new(false)),
        require_prime: false,
        corrupt: false,
        ..fixture
    })
    .await;
    let temp = tempfile::tempdir()?;
    let client = Client::new(
        config(None, vec![], vec![bad.clone()]),
        &temp.path().join("bad"),
    )?;
    assert!(client.store().get(&cid.hash).await.is_err());
    let client = Client::new(
        config(Some(bad), vec![], vec![good]),
        &temp.path().join("good"),
    )?;
    assert!(client.store().get(&cid.hash).await?.is_some());
    bad_task.abort();
    good_task.abort();
    Ok(())
}

#[tokio::test]
async fn local_only_never_falls_back_and_wrong_publisher_does_not_resolve() -> anyhow::Result<()> {
    let keys = Keys::generate();
    let event = NostrRootResolver::root_event_builder(
        "packages",
        &hashtree_core::Cid::public([1; 32]),
        None,
    )
    .sign_with_keys(&keys)?;
    let (url, task) = serve(Fixture {
        store: Arc::new(MemoryStore::new()),
        events: vec![event],
        reads: Arc::new(AtomicUsize::new(0)),
        primed: Arc::new(AtomicBool::new(false)),
        require_prime: false,
        corrupt: false,
    })
    .await;
    let reference = Reference::parse(&format!(
        "htree://{}/packages",
        Keys::generate().public_key().to_bech32()?
    ))?;
    let temp = tempfile::tempdir()?;
    let mut cfg = config(Some(url), vec!["ws://127.0.0.1:1".into()], vec![]);
    cfg.local_only = true;
    let client = Client::new(cfg, temp.path())?;
    assert!(client
        .resolve(&reference)
        .await
        .unwrap_err()
        .to_string()
        .contains("Timed out"));
    task.abort();
    Ok(())
}

#[tokio::test]
async fn cold_daemon_resolves_via_its_provider_but_only_signed_events_are_trusted(
) -> anyhow::Result<()> {
    let keys = Keys::generate();
    let expected = hashtree_core::Cid::public([7; 32]);
    let event =
        NostrRootResolver::root_event_builder("packages", &expected, None).sign_with_keys(&keys)?;
    let primed = Arc::new(AtomicBool::new(false));
    let (url, task) = serve(Fixture {
        store: Arc::new(MemoryStore::new()),
        events: vec![event],
        reads: Arc::new(AtomicUsize::new(0)),
        corrupt: false,
        primed: primed.clone(),
        require_prime: true,
    })
    .await;
    let reference = Reference::parse(&format!(
        "htree://{}/packages",
        keys.public_key().to_bech32()?
    ))?;
    let temp = tempfile::tempdir()?;
    let mut cfg = config(Some(url), vec![], vec![]);
    cfg.local_only = true;
    let client = Client::new(cfg, temp.path())?;
    assert_eq!(client.resolve(&reference).await?, expected);
    assert!(primed.load(Ordering::SeqCst));
    task.abort();
    Ok(())
}
