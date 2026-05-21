use async_trait::async_trait;
use hashtree_core::{Hash, MemoryStore, Store};
use hashtree_fips_transport::{
    FipsEndpointIo, FipsEndpointPacket, FipsTransportError, HashtreeFipsTransport,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{self, BufRead, Write};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio::time::Duration;

const LOCAL_PEER_ID: &str = "rust";
const REMOTE_PEER_ID: &str = "ts";
const RUST_BLOB: &[u8] = b"rust hashtree fips transport fixture blob";
const LARGE_BLOB_LEN: usize = 2_777;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum InputMessage {
    Frame { data: String },
    Fetch { id: String, hash: String },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum OutputMessage<'a> {
    Ready {
        #[serde(rename = "peerId")]
        peer_id: &'a str,
        hash: String,
        data: String,
        #[serde(rename = "largeHash")]
        large_hash: String,
        #[serde(rename = "largeData")]
        large_data: String,
    },
    Frame {
        #[serde(rename = "peerId")]
        peer_id: &'a str,
        data: String,
    },
    FetchResult {
        id: String,
        data: Option<String>,
    },
    Error {
        message: String,
    },
}

struct StdioEndpoint {
    rx: Mutex<mpsc::UnboundedReceiver<FipsEndpointPacket>>,
    stdout: Mutex<()>,
}

#[async_trait]
impl FipsEndpointIo for StdioEndpoint {
    async fn send(&self, _peer_id: &str, data: Vec<u8>) -> Result<(), FipsTransportError> {
        let _guard = self.stdout.lock().await;
        write_message(&OutputMessage::Frame {
            peer_id: LOCAL_PEER_ID,
            data: hex::encode(data),
        })
        .map_err(|err| FipsTransportError::Send(err.to_string()))
    }

    async fn recv(&self) -> Option<FipsEndpointPacket> {
        self.rx.lock().await.recv().await
    }

    async fn peer_ids(&self) -> Vec<String> {
        vec![REMOTE_PEER_ID.to_string()]
    }

    fn local_peer_id(&self) -> Option<String> {
        Some(LOCAL_PEER_ID.to_string())
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(err) = run().await {
        let _ = write_message(&OutputMessage::Error {
            message: err.to_string(),
        });
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (tx, rx) = mpsc::unbounded_channel();
    let endpoint = Arc::new(StdioEndpoint {
        rx: Mutex::new(rx),
        stdout: Mutex::new(()),
    });
    let store = Arc::new(MemoryStore::new());
    let rust_hash = hash(RUST_BLOB);
    let rust_large_blob = large_blob();
    let rust_large_hash = hash(&rust_large_blob);
    store.put(rust_hash, RUST_BLOB.to_vec()).await?;
    store.put(rust_large_hash, rust_large_blob.clone()).await?;

    let transport = Arc::new(
        HashtreeFipsTransport::new(endpoint.clone(), store)
            .with_request_timeout(Duration::from_millis(5_000)),
    );
    transport.set_peers(vec![REMOTE_PEER_ID.to_string()]).await;
    transport.start();

    write_message(&OutputMessage::Ready {
        peer_id: LOCAL_PEER_ID,
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
            Ok(InputMessage::Frame { data }) => {
                let bytes = hex::decode(data)?;
                tx.send(FipsEndpointPacket {
                    peer_id: REMOTE_PEER_ID.to_string(),
                    data: bytes,
                })?;
            }
            Ok(InputMessage::Fetch { id, hash }) => {
                let Some(hash) = parse_hash(&hash) else {
                    write_message(&OutputMessage::FetchResult { id, data: None })?;
                    continue;
                };
                let transport = transport.clone();
                tokio::spawn(async move {
                    let result = transport
                        .get_from_peers(&hash, &[REMOTE_PEER_ID.to_string()])
                        .await;
                    let message = match result {
                        Ok(data) => OutputMessage::FetchResult {
                            id,
                            data: data.map(hex::encode),
                        },
                        Err(err) => OutputMessage::Error {
                            message: err.to_string(),
                        },
                    };
                    let _ = write_message(&message);
                });
            }
            Err(err) => {
                write_message(&OutputMessage::Error {
                    message: err.to_string(),
                })?;
            }
        }
    }

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

fn write_message(message: &OutputMessage<'_>) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, message)?;
    stdout.write_all(b"\n")?;
    stdout.flush()
}

fn hash(data: &[u8]) -> Hash {
    let digest = Sha256::digest(data);
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
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
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&bytes);
    Some(hash)
}
