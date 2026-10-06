use super::*;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::{accept_async, tungstenite::Message};

// Exercise the production endpoint's built-in relay discovery client. EOSE on
// an empty relay must leave its discovery subscription open while P2P runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_discovery_stays_open_with_archive_relays_or_explicit_override() {
    for explicit_override in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay = format!("ws://{}", listener.local_addr().unwrap());
        let (requests, mut received) = tokio::sync::mpsc::channel(32);
        let server = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let requests = requests.clone();
                tokio::spawn(async move {
                    let mut socket = accept_async(stream).await.unwrap();
                    while let Some(Ok(message)) = socket.next().await {
                        let Message::Text(text) = message else {
                            continue;
                        };
                        let value: Value = serde_json::from_str(&text).unwrap();
                        if value[0] == "REQ" {
                            socket
                                .send(Message::Text(json!(["EOSE", value[1]]).to_string()))
                                .await
                                .unwrap();
                        }
                        if requests.send(value).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut config = Config::default();
        config.server.fips_local_rendezvous_addr = Some(socket.local_addr().unwrap().to_string());
        drop(socket);
        config.server.fips_discovery_scope = format!("catchup-relay-{}", uuid::Uuid::new_v4());
        config.server.enable_fips_udp = false;
        config.server.enable_fips_webrtc = false;
        config.server.enable_fips_lan_discovery = false;
        config.server.fips_relays = explicit_override.then(|| vec![relay.clone()]);
        let relays = if explicit_override {
            vec![]
        } else {
            vec![relay]
        };
        let runtime = Runtime::start(&config, &relays, Duration::from_millis(250))
            .await
            .unwrap();
        let result = async {
            let subscription = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let request = received.recv().await.unwrap();
                    if request[0] == "REQ"
                        && request[2]["kinds"]
                            .as_array()
                            .is_some_and(|kinds| kinds.contains(&json!(37195)))
                    {
                        return request[1].clone();
                    }
                }
            })
            .await?;
            query(
                runtime.client.as_ref(),
                Filter::new().kind(Kind::TextNote),
                Duration::from_secs(1),
            )
            .await
            .ok_or_else(|| anyhow::anyhow!("P2P query unavailable"))?;
            while let Ok(message) = received.try_recv() {
                ensure!(
                    message[0] != "CLOSE" || message[1] != subscription,
                    "quiet relay discovery must remain subscribed"
                );
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        runtime.shutdown().await;
        server.abort();
        result.unwrap();
    }
}
