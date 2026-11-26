use std::{
    future::Future,
    net::SocketAddr,
    sync::{Arc, OnceLock},
    time::Duration,
};

use anyhow::{anyhow, bail, Context, Result};
use tokio::{
    net::{lookup_host, TcpStream},
    time::{sleep, timeout},
};
use tokio_tungstenite::{
    client_async_tls_with_config, tungstenite::http::Uri, Connector, MaybeTlsStream,
    WebSocketStream,
};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
const OTHER_FAMILY_DELAY: Duration = Duration::from_millis(250);
const ADDRESS_TIMEOUT: Duration = Duration::from_secs(4);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);

fn shared_tls_connector() -> Connector {
    static CONNECTOR: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    let config = CONNECTOR.get_or_init(|| {
        let mut root_store = rustls::RootCertStore::empty();
        root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let builder = match rustls::crypto::CryptoProvider::get_default() {
            Some(provider) => rustls::ClientConfig::builder_with_provider(Arc::clone(provider)),
            None => rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            )),
        };
        let config = builder
            .with_safe_default_protocol_versions()
            .expect("valid default TLS protocol versions")
            .with_root_certificates(root_store)
            .with_no_client_auth();
        Arc::new(config)
    });
    Connector::Rustls(Arc::clone(config))
}

pub(super) async fn connect(url: &str) -> Result<Socket> {
    timeout(CONNECT_TIMEOUT, async {
        let uri: Uri = url.parse()?;
        let (default_port, connector) = match uri.scheme_str() {
            Some("wss") => (443, Some(shared_tls_connector())),
            Some("ws") => (80, None),
            _ => bail!("unsupported relay URL scheme"),
        };
        let host = uri.host().context("missing relay hostname")?;
        let host = host.trim_start_matches('[').trim_end_matches(']');
        let addresses = lookup_host((host, uri.port_u16().unwrap_or(default_port))).await?;
        race_families(addresses, |address| {
            handshake(url, address, connector.clone())
        })
        .await
    })
    .await
    .context("upstream relay connection deadline")?
}

async fn handshake(url: &str, address: SocketAddr, connector: Option<Connector>) -> Result<Socket> {
    let stream = TcpStream::connect(address).await?;
    // Keep the requested hostname for TLS verification, SNI and the WebSocket Host header.
    let (socket, _) = client_async_tls_with_config(url, stream, None, connector).await?;
    Ok(socket)
}

async fn race_families<T, F, Fut>(
    addresses: impl IntoIterator<Item = SocketAddr>,
    dial: F,
) -> Result<T>
where
    F: Fn(SocketAddr) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut addresses = addresses.into_iter().peekable();
    let prefer_ipv6 = addresses.peek().context("no relay addresses")?.is_ipv6();
    let (preferred, alternate): (Vec<_>, Vec<_>) =
        addresses.partition(|address| address.is_ipv6() == prefer_ipv6);
    let attempt = |addresses: Vec<SocketAddr>| {
        let dial = &dial;
        async move {
            let mut last_error = anyhow!("no relay addresses");
            for address in addresses {
                match timeout(ADDRESS_TIMEOUT, dial(address)).await {
                    Ok(Ok(socket)) => return Ok(socket),
                    Ok(Err(error)) => last_error = error,
                    Err(error) => last_error = error.into(),
                }
            }
            Err(last_error)
        }
    };
    let first = attempt(preferred);
    if alternate.is_empty() {
        return first.await;
    }
    tokio::pin!(first);
    tokio::select! {
        result = &mut first => return match result {
            Ok(socket) => Ok(socket),
            Err(_) => attempt(alternate).await,
        },
        _ = sleep(OTHER_FAMILY_DELAY) => {}
    }

    // Each family has at most one active handshake. Dropping the race closes all losing sockets.
    let second = attempt(alternate);
    tokio::pin!(second);
    tokio::select! {
        result = &mut first => match result { Ok(socket) => Ok(socket), Err(_) => second.await },
        result = &mut second => match result { Ok(socket) => Ok(socket), Err(_) => first.await },
    }
}

#[cfg(test)]
mod tests;
