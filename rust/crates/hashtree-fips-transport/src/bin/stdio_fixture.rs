use fips_core::config::{PeerConfig, TransportInstances};
use fips_core::{encode_nsec, Config, FipsEndpoint, Identity, PeerIdentity, UdpConfig};
use hashtree_core::{Hash, MemoryStore, Store};
use hashtree_fips_transport::{TcpBlobTransport, TcpBlobTransportConfig};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::env;
use std::io::{self, BufRead, Write};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::timeout;

const RUST_BLOB: &[u8] = b"rust TCP blob v1 fixture blob";
const LARGE_BLOB_LEN: usize = 180_017;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum InputMessage {
    Fetch { id: String, hash: String },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum OutputMessage {
    Ready {
        #[serde(rename = "peerId")]
        peer_id: String,
        hash: String,
        data: String,
        #[serde(rename = "largeHash")]
        large_hash: String,
        #[serde(rename = "largeData")]
        large_data: String,
    },
    FetchResult {
        id: String,
        data: Option<String>,
    },
    Error {
        message: String,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(error) = run().await {
        let _ = write_message(&OutputMessage::Error {
            message: error.to_string(),
        });
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (remote_peer, remote_address) = fixture_args()?;
    let identity = Identity::generate();
    let config = endpoint_config(&identity, remote_peer, remote_address);
    let endpoint = Arc::new(
        FipsEndpoint::builder()
            .config(config)
            .without_system_tun()
            .bind()
            .await?,
    );
    let store = Arc::new(MemoryStore::new());
    let rust_hash = hash(RUST_BLOB);
    let rust_large_blob = large_blob();
    let rust_large_hash = hash(&rust_large_blob);
    store.put(rust_hash, RUST_BLOB.to_vec()).await?;
    store.put(rust_large_hash, rust_large_blob.clone()).await?;
    let transport = TcpBlobTransport::bind_with_config(
        endpoint.clone(),
        store,
        TcpBlobTransportConfig {
            idle_timeout: Duration::from_secs(10),
        },
    )
    .await?;

    wait_for_peer(&endpoint, remote_peer).await?;
    write_message(&OutputMessage::Ready {
        peer_id: hex::encode(identity.pubkey_full().serialize()),
        hash: hex::encode(rust_hash),
        data: hex::encode(RUST_BLOB),
        large_hash: hex::encode(rust_large_hash),
        large_data: hex::encode(&rust_large_blob),
    })?;

    let mut lines = spawn_stdin_reader();
    while let Some(line) = lines.recv().await {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<InputMessage>(&line) {
            Ok(InputMessage::Fetch { id, hash }) => {
                let result = match parse_hash(&hash) {
                    Some(hash) => transport.get(&hash, remote_peer).await,
                    None => Ok(None),
                };
                match result {
                    Ok(data) => write_message(&OutputMessage::FetchResult {
                        id,
                        data: data.map(hex::encode),
                    })?,
                    Err(error) => write_message(&OutputMessage::Error {
                        message: error.to_string(),
                    })?,
                }
            }
            Err(error) => write_message(&OutputMessage::Error {
                message: error.to_string(),
            })?,
        }
    }

    transport.shutdown().await?;
    endpoint.shutdown().await?;
    Ok(())
}

fn fixture_args() -> io::Result<(PeerIdentity, SocketAddr)> {
    let mut args = env::args().skip(1);
    let npub = args
        .next()
        .ok_or_else(|| invalid_input("missing TypeScript peer npub"))?;
    let address = args
        .next()
        .ok_or_else(|| invalid_input("missing TypeScript UDP address"))?
        .parse::<SocketAddr>()
        .map_err(|error| invalid_input(format!("invalid TypeScript UDP address: {error}")))?;
    if args.next().is_some() {
        return Err(invalid_input("unexpected fixture argument"));
    }
    if !address.ip().is_loopback() {
        return Err(invalid_input("TypeScript UDP address is not loopback"));
    }
    let peer = PeerIdentity::from_npub(&npub)
        .map_err(|error| invalid_input(format!("invalid TypeScript peer npub: {error}")))?;
    Ok((peer, address))
}

fn endpoint_config(identity: &Identity, peer: PeerIdentity, peer_address: SocketAddr) -> Config {
    let mut config = Config::new();
    config.node.identity.nsec = Some(encode_nsec(&identity.keypair().secret_key()));
    config.node.discovery.nostr.enabled = false;
    config.node.discovery.lan.enabled = false;
    config.node.discovery.local.enabled = false;
    config.transports.udp = TransportInstances::Single(UdpConfig {
        bind_addr: Some("127.0.0.1:0".to_string()),
        advertise_on_nostr: Some(false),
        public: Some(true),
        outbound_only: Some(false),
        accept_connections: Some(true),
        ..UdpConfig::default()
    });
    config.peers = vec![PeerConfig::new(
        peer.npub(),
        "udp",
        peer_address.to_string(),
    )];
    config
}

async fn wait_for_peer(
    endpoint: &FipsEndpoint,
    remote: PeerIdentity,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    timeout(Duration::from_secs(10), async {
        loop {
            if endpoint.peers().await.is_ok_and(|peers| {
                peers
                    .iter()
                    .any(|peer| peer.npub == remote.npub() && peer.connected)
            }) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(|_| invalid_input("FIPS peers did not connect"))?;
    Ok(())
}

fn spawn_stdin_reader() -> mpsc::UnboundedReceiver<String> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            let Ok(line) = line else {
                break;
            };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

fn write_message(message: &OutputMessage) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, message)?;
    stdout.write_all(b"\n")?;
    stdout.flush()
}

fn hash(data: &[u8]) -> Hash {
    Sha256::digest(data).into()
}

fn large_blob() -> Vec<u8> {
    (0..LARGE_BLOB_LEN)
        .map(|index| (index % 251) as u8)
        .collect()
}

fn parse_hash(hex_hash: &str) -> Option<Hash> {
    let bytes = hex::decode(hex_hash).ok()?;
    if bytes.len() != 32 {
        return None;
    }
    bytes.try_into().ok()
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}
