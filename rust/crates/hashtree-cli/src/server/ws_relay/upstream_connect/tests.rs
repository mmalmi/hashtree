use super::*;
use std::sync::Arc;
use tokio::{io::AsyncReadExt, net::TcpListener};
use tokio_tungstenite::{accept_async, accept_hdr_async, tungstenite::handshake::server};

#[tokio::test]
async fn pending_ipv6_tcp_attempt_allows_ipv4_and_is_dropped() -> Result<()> {
    let ipv6 = "[::1]:443".parse()?;
    let ipv4 = "127.0.0.1:443".parse()?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let result = timeout(
        Duration::from_secs(2),
        race_families([ipv6, ipv4], |addr| {
            let tx = tx.clone();
            async move {
                let (guard, dropped) = tokio::sync::oneshot::channel::<()>();
                tx.send((addr, dropped)).unwrap();
                if addr.is_ipv6() {
                    std::future::pending::<()>().await;
                }
                drop(guard);
                Ok(addr)
            }
        }),
    )
    .await??;
    assert_eq!(result, ipv4);
    let (first, lost) = rx.recv().await.unwrap();
    assert_eq!(first, ipv6);
    assert!(lost.await.is_err());
    Ok(())
}

#[tokio::test]
async fn cancelling_connect_drops_both_pending_families() -> Result<()> {
    let addrs = ["[::1]:443".parse()?, "127.0.0.1:443".parse()?];
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let attempt = tokio::spawn(async move {
        race_families(addrs, |_| {
            let tx = tx.clone();
            async move {
                let (_guard, dropped) = tokio::sync::oneshot::channel::<()>();
                tx.send(dropped).unwrap();
                std::future::pending::<Result<()>>().await
            }
        })
        .await
    });
    let first = timeout(Duration::from_secs(2), rx.recv()).await?.unwrap();
    let second = timeout(Duration::from_secs(2), rx.recv()).await?.unwrap();
    attempt.abort();
    assert!(attempt.await.unwrap_err().is_cancelled());
    for dropped in [first, second] {
        assert!(timeout(Duration::from_millis(500), dropped).await?.is_err());
    }
    Ok(())
}

#[tokio::test]
async fn working_ipv6_websocket_is_used() -> Result<()> {
    let listener = TcpListener::bind("[::1]:0").await?;
    let addr = listener.local_addr()?;
    let server = tokio::spawn(async move {
        accept_async(listener.accept().await?.0)
            .await
            .map_err(anyhow::Error::from)
    });
    let _client = connect(&format!("ws://{addr}/relay")).await?;
    server.await??;
    Ok(())
}

// The handshake callback's error type is fixed by tungstenite.
#[allow(clippy::result_large_err)]
#[tokio::test]
async fn silent_ipv6_handshake_falls_back_with_original_tls_identity() -> Result<()> {
    // Only this fixture CA is trusted by the test connector. The production default uses WebPKI.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let name = "relay.example.invalid";
    let cert = rcgen::generate_simple_self_signed(vec![name.into()])?;
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.cert.der().clone())?;
    let tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.cert.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()).into(),
        )?;
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
    let connector = Connector::Rustls(Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ));
    let v6 = TcpListener::bind("[::1]:0").await?;
    let v6_addr = v6.local_addr()?;
    let v4 = TcpListener::bind(("127.0.0.1", v6_addr.port())).await?;
    let v4_addr = v4.local_addr()?;
    let silent = tokio::spawn(async move {
        let (mut stream, _) = v6.accept().await?;
        let mut input = Vec::new();
        stream.read_to_end(&mut input).await?;
        anyhow::ensure!(
            !input.is_empty(),
            "expected ClientHello before cancellation"
        );
        Ok::<_, anyhow::Error>(())
    });
    let authority = format!("{name}:{}", v4_addr.port());
    let url = format!("wss://{authority}/relay?q=1");
    let server = tokio::spawn(async move {
        let (tcp, _) = v4.accept().await?;
        let stream = acceptor.accept(tcp).await?;
        assert_eq!(stream.get_ref().1.server_name(), Some(name));
        let _ws = accept_hdr_async(stream, |req: &server::Request, res: server::Response| {
            assert_eq!(req.headers()["host"].to_str().unwrap(), authority);
            assert_eq!(req.uri().path_and_query().unwrap().as_str(), "/relay?q=1");
            Ok(res)
        })
        .await?;
        // A mismatched hostname and an untrusted issuer both fail before any WebSocket request.
        for _ in 0..2 {
            assert!(acceptor.accept(v4.accept().await?.0).await.is_err());
        }
        Ok::<_, anyhow::Error>(())
    });
    let _ws = timeout(
        Duration::from_secs(2),
        race_families([v6_addr, v4_addr], |addr| {
            handshake(&url, addr, Some(connector.clone()))
        }),
    )
    .await??;
    timeout(Duration::from_secs(1), silent).await???;
    assert!(
        handshake("wss://wrong.example.invalid/", v4_addr, Some(connector))
            .await
            .is_err()
    );
    assert!(handshake(&url, v4_addr, None).await.is_err());
    timeout(Duration::from_secs(2), server).await???;
    Ok(())
}

#[tokio::test]
async fn silent_single_family_upgrade_has_a_deadline() -> Result<()> {
    let listener = TcpListener::bind("[::1]:0").await?;
    let url = format!("ws://{}/relay", listener.local_addr()?);
    let error = timeout(ADDRESS_TIMEOUT + Duration::from_secs(1), connect(&url))
        .await?
        .expect_err("silent upstream unexpectedly upgraded");
    assert!(
        error
            .downcast_ref::<tokio::time::error::Elapsed>()
            .is_some(),
        "{error}"
    );
    Ok(())
}
