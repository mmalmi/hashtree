use super::*;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{header, Request, StatusCode};
use std::net::SocketAddr;
use tower::ServiceExt;

const SNAPSHOT: &str =
    "/api/socialgraph/snapshot?maxNodes=1&maxEdges=1&maxDistance=0&maxEdgesPerNode=1";
const EVENTS: &str = "/api/nostr/events";
const MARKERS: &[(&str, &str)] = &[
    ("Forwarded", "for=127.0.0.1"),
    ("X-Forwarded-For", "127.0.0.1"),
    ("X-Forwarded-Host", "localhost"),
    ("X-Forwarded-Proto", "https"),
    ("X-Real-IP", "127.0.0.1"),
    ("CF-Connecting-IP", "127.0.0.1"),
    ("CF-Connecting-IPv6", "::1"),
    ("CF-Ray", "fixture"),
    ("Via", "1.1 fixture"),
    ("X-Forwarded-Port", "443"),
];

const BLOB: &[u8] = b"private test fixture: public blob";

fn fixture(dir: &TempDir, public: bool) -> Result<(HashtreeServer, Arc<StaticProvider>)> {
    let store = Arc::new(HashtreeStore::new_with_backend(
        dir.path().join("blobs"),
        StorageBackend::Fs,
        16 * 1024 * 1024,
    )?);
    store.put_blob(BLOB)?;
    let provider = Arc::new(StaticProvider {
        event: None,
        queries: AtomicUsize::new(0),
        publishes: AtomicUsize::new(0),
        mode: PubsubProviderMode::LocalOnly,
    });
    let graph = crate::socialgraph::open_test_social_graph_store(&dir.path().join("graph"))?;
    let server = HashtreeServer::new(store, "127.0.0.1:0".into())
        .with_socialgraph_snapshot(graph, Keys::generate().public_key().to_bytes(), public)
        .with_nostr_provider(provider.clone());
    Ok((server, provider))
}

async fn serve(server: HashtreeServer) -> Result<String> {
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", socket.local_addr()?);
    tokio::spawn(server.run_with_listener(socket));
    Ok(base)
}

fn events() -> Result<(serde_json::Value, serde_json::Value)> {
    let event = EventBuilder::new(Kind::TextNote, "synthetic local event")
        .sign_with_keys(&Keys::generate())?;
    let valid = serde_json::to_value(event)?;
    let mut invalid = valid.clone();
    invalid["id"] = json!("00".repeat(32));
    Ok((valid, invalid))
}

#[tokio::test]
async fn snapshot_access_follows_public_setting_and_connection() -> Result<()> {
    let client = reqwest::Client::builder().no_proxy().build()?;
    for public in [false, true] {
        let dir = TempDir::new()?;
        let (server, _) = fixture(&dir, public)?;
        let base = serve(server).await?;
        let url = base.clone() + SNAPSHOT;
        let direct = client.get(&url).send().await?;
        assert_eq!(direct.status(), StatusCode::OK);
        assert_eq!(
            direct
                .headers()
                .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN),
            public
        );
        assert!(!direct.bytes().await?.is_empty());
        for (name, value) in MARKERS {
            let status = client
                .get(format!("{base}/api/status"))
                .header(*name, *value)
                .send()
                .await?;
            assert_eq!(status.status(), StatusCode::FORBIDDEN, "{name}");
            for method in [Method::GET, Method::HEAD] {
                let res = client
                    .request(method.clone(), &url)
                    .header(*name, *value)
                    .send()
                    .await?;
                if public {
                    assert_eq!(res.status(), StatusCode::OK, "{method} {name}");
                    assert_eq!(res.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
                    assert!(res.headers().contains_key(header::CONTENT_DISPOSITION));
                } else {
                    assert_eq!(res.status(), StatusCode::FORBIDDEN, "{method} {name}");
                    assert!(!res.headers().contains_key(header::CONTENT_DISPOSITION));
                }
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn only_direct_publication_reaches_event_verification_and_provider() -> Result<()> {
    let dir = TempDir::new()?;
    let (server, provider) = fixture(&dir, false)?;
    let url = serve(server).await? + EVENTS;
    let client = reqwest::Client::builder().no_proxy().build()?;
    let (valid, invalid) = events()?;
    for (name, value) in MARKERS {
        for event in [&valid, &invalid] {
            let res = client
                .post(&url)
                .header(*name, *value)
                .json(event)
                .send()
                .await?;
            assert_eq!(res.status(), StatusCode::FORBIDDEN, "{name}");
        }
        assert_eq!(
            client
                .post(&url)
                .header(*name, *value)
                .body("not JSON")
                .send()
                .await?
                .status(),
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        client.post(&url).json(&invalid).send().await?.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(provider.publishes.load(Ordering::Relaxed), 0);
    assert_eq!(
        client.post(&url).json(&valid).send().await?.status(),
        StatusCode::ACCEPTED
    );
    assert_eq!(provider.publishes.load(Ordering::Relaxed), 1);
    Ok(())
}

#[tokio::test]
async fn raw_blob_source_header_is_only_visible_on_direct_requests() -> Result<()> {
    let dir = TempDir::new()?;
    let (server, _) = fixture(&dir, false)?;
    let legacy = blossom::head_blob(
        State(server.state.clone()),
        axum::extract::Path(to_hex(&sha256(BLOB))),
        axum::extract::ConnectInfo("127.0.0.1:1234".parse()?),
    )
    .await
    .into_response();
    assert_eq!(legacy.status(), StatusCode::OK);
    assert!(!legacy.headers().contains_key("x-source"));
    let url = format!("{}/{}.bin", serve(server).await?, to_hex(&sha256(BLOB)));
    let client = reqwest::Client::builder().no_proxy().build()?;
    for method in [Method::GET, Method::HEAD] {
        let direct = client.request(method.clone(), &url).send().await?;
        assert_eq!(direct.status(), StatusCode::OK);
        assert_eq!(direct.headers()["x-source"], "local");
        let proxied = client
            .request(method.clone(), &url)
            .header("X-Forwarded-For", "127.0.0.1")
            .send()
            .await?;
        assert_eq!(proxied.status(), StatusCode::OK);
        assert!(
            !proxied.headers().contains_key("x-source"),
            "{method} exposed source"
        );
        if method == Method::GET {
            assert_eq!(proxied.bytes().await?.as_ref(), BLOB);
        }
    }
    Ok(())
}

#[tokio::test]
async fn non_loopback_peers_cannot_gain_local_access() -> Result<()> {
    let peer: SocketAddr = "192.0.2.19:43210".parse()?;
    for public in [false, true] {
        let dir = TempDir::new()?;
        let (server, provider) = fixture(&dir, public)?;
        let routes = super::super::local_access::routes(public)
            .layer(MockConnectInfo(peer))
            .with_state(server.state);
        for spoof in [false, true] {
            for (method, path, expected) in [
                (
                    Method::HEAD,
                    SNAPSHOT,
                    if public {
                        StatusCode::OK
                    } else {
                        StatusCode::FORBIDDEN
                    },
                ),
                (Method::GET, "/api/status", StatusCode::FORBIDDEN),
                (Method::POST, EVENTS, StatusCode::FORBIDDEN),
            ] {
                let mut req = Request::builder().method(method).uri(path);
                if spoof {
                    req = req.header("X-Forwarded-For", "127.0.0.1");
                }
                let res = routes
                    .clone()
                    .oneshot(req.body(axum::body::Body::empty())?)
                    .await?;
                assert_eq!(res.status(), expected, "{path}, spoof={spoof}");
            }
        }
        assert_eq!(provider.publishes.load(Ordering::Relaxed), 0);
    }
    Ok(())
}

#[tokio::test]
async fn proxy_response_does_not_identify_the_upstream_store() -> Result<()> {
    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let upstream_url = format!("http://{}", upstream.local_addr()?);
    let a = b"synthetic blob for a direct read";
    let b = b"synthetic blob for a proxied read";
    let data = Router::new()
        .route(
            &format!("/{}.bin", to_hex(&sha256(a))),
            get(|| async { a.as_slice() }),
        )
        .route(
            &format!("/{}.bin", to_hex(&sha256(b))),
            get(|| async { b.as_slice() }),
        );
    tokio::spawn(async move {
        axum::serve(upstream, data).await.unwrap();
    });
    let dir = TempDir::new()?;
    let (server, _) = fixture(&dir, false)?;
    let base = serve(server.with_upstream_blossom(vec![upstream_url.clone()])).await?;
    let client = reqwest::Client::builder().no_proxy().build()?;
    let res = client
        .get(format!("{}/{}.bin", base, to_hex(&sha256(a))))
        .send()
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers()["x-source"].to_str()?,
        format!("upstream:{upstream_url}")
    );
    let res = client
        .get(format!("{}/{}.bin", base, to_hex(&sha256(b))))
        .header("Forwarded", "for=127.0.0.1")
        .send()
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(!res.headers().contains_key("x-source"));
    assert_eq!(res.bytes().await?.as_ref(), b);
    Ok(())
}
