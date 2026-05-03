use anyhow::{anyhow, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{debug, warn};

use crate::manager::WebRTCState;
use crate::peer::ContentStore;
use crate::relay_bridge::SharedMeshRelayClient;
use crate::{
    encode_request, encode_response, hash_to_key, parse_message, DataMessage, DataRequest,
    DataResponse, MeshNostrFrame, PeerDirection, PeerHTLConfig, PeerId, TimedSeenSet,
    BLOB_REQUEST_POLICY,
};
use nostr_sdk::nostr::{
    ClientMessage as NostrClientMessage, Event, Filter as NostrFilter, JsonUtil as NostrJsonUtil,
    RelayMessage as NostrRelayMessage, SubscriptionId as NostrSubscriptionId, Timestamp,
};

const BLUETOOTH_SEEN_EVENT_CAP: usize = 2048;
const BLUETOOTH_SEEN_EVENT_TTL: Duration = Duration::from_secs(600);

type PendingBlobRequests = Arc<Mutex<HashMap<String, oneshot::Sender<Option<Vec<u8>>>>>>;
type PendingNostrQueries = Arc<Mutex<HashMap<String, mpsc::UnboundedSender<NostrRelayMessage>>>>;

#[derive(Debug, Clone)]
pub enum BluetoothFrame {
    Text(String),
    Binary(Vec<u8>),
}

#[async_trait]
pub trait BluetoothLink: Send + Sync {
    async fn send(&self, frame: BluetoothFrame) -> Result<()>;
    async fn recv(&self) -> Option<BluetoothFrame>;
    fn is_open(&self) -> bool;
    async fn close(&self) -> Result<()>;
}

pub struct BluetoothPeer {
    pub peer_id: PeerId,
    pub direction: PeerDirection,
    pub created_at: std::time::Instant,
    pub connected_at: Option<std::time::Instant>,
    link: Arc<dyn BluetoothLink>,
    store: Option<Arc<dyn ContentStore>>,
    pending_requests: PendingBlobRequests,
    pending_nostr_queries: PendingNostrQueries,
    nostr_relay: Option<SharedMeshRelayClient>,
    mesh_frame_tx: Option<mpsc::Sender<(PeerId, MeshNostrFrame)>>,
    traffic_state: Option<Arc<WebRTCState>>,
    seen_event_ids: Arc<Mutex<TimedSeenSet>>,
    htl_config: PeerHTLConfig,
}

impl BluetoothPeer {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        peer_id: PeerId,
        direction: PeerDirection,
        link: Arc<dyn BluetoothLink>,
        store: Option<Arc<dyn ContentStore>>,
        nostr_relay: Option<SharedMeshRelayClient>,
        mesh_frame_tx: Option<mpsc::Sender<(PeerId, MeshNostrFrame)>>,
        traffic_state: Option<Arc<WebRTCState>>,
    ) -> Arc<Self> {
        let peer = Arc::new(Self {
            peer_id,
            direction,
            created_at: std::time::Instant::now(),
            connected_at: Some(std::time::Instant::now()),
            link,
            store,
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
            pending_nostr_queries: Arc::new(Mutex::new(HashMap::new())),
            nostr_relay,
            mesh_frame_tx,
            traffic_state,
            seen_event_ids: Arc::new(Mutex::new(TimedSeenSet::new(
                BLUETOOTH_SEEN_EVENT_CAP,
                BLUETOOTH_SEEN_EVENT_TTL,
            ))),
            htl_config: PeerHTLConfig::random(),
        });
        Self::spawn_reader(peer.clone());
        peer
    }

    async fn mark_seen_event_id(&self, event_id: String) -> bool {
        self.seen_event_ids.lock().await.insert_if_new(event_id)
    }

    fn spawn_reader(peer: Arc<Self>) {
        tokio::spawn(async move {
            let mut nostr_forward_task = None;
            let mut nostr_client_id = None;

            if let Some(relay) = peer.nostr_relay.as_ref() {
                let client_id = relay.next_client_id();
                let (nostr_tx, mut nostr_rx) = mpsc::unbounded_channel::<String>();
                relay
                    .register_client(client_id, nostr_tx, Some(peer.peer_id.pubkey.clone()))
                    .await;
                nostr_client_id = Some(client_id);

                let live_subscription_id =
                    NostrSubscriptionId::new(format!("bluetooth-live-{}", rand::random::<u64>()));
                let _ = relay
                    .register_subscription_query(
                        client_id,
                        live_subscription_id.clone(),
                        vec![NostrFilter::new().since(Timestamp::now())],
                    )
                    .await;

                let peer_for_forward = peer.clone();
                nostr_forward_task = Some(tokio::spawn(async move {
                    while let Some(text) = nostr_rx.recv().await {
                        if let Ok(NostrRelayMessage::Event {
                            subscription_id,
                            event,
                        }) = NostrRelayMessage::from_json(&text)
                        {
                            if subscription_id == live_subscription_id {
                                if event.kind.is_ephemeral()
                                    || !peer_for_forward.mark_seen_event_id(event.id.to_hex()).await
                                {
                                    continue;
                                }
                                if peer_for_forward
                                    .send_frame(BluetoothFrame::Text(event.as_json()))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                                continue;
                            }
                        }
                        if peer_for_forward
                            .send_frame(BluetoothFrame::Text(text))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }));
            }

            while let Some(frame) = peer.link.recv().await {
                match frame {
                    BluetoothFrame::Binary(data) => {
                        if let Err(err) = peer.handle_binary_frame(data).await {
                            debug!(
                                "[BluetoothPeer {}] Ignoring invalid binary frame: {}",
                                peer.peer_id.short(),
                                err
                            );
                        }
                    }
                    BluetoothFrame::Text(text) => {
                        peer.handle_text_frame(text, nostr_client_id).await;
                    }
                }
            }

            if let (Some(relay), Some(client_id)) = (peer.nostr_relay.as_ref(), nostr_client_id) {
                relay.unregister_client(client_id).await;
            }

            if let Some(task) = nostr_forward_task {
                let _ = task.await;
            }
        });
    }

    async fn handle_binary_frame(&self, data: Vec<u8>) -> Result<()> {
        self.record_received(data.len() as u64).await;
        match parse_message(&data).ok_or_else(|| anyhow!("invalid Bluetooth data frame"))? {
            DataMessage::Request(req) => {
                let hash_hex = hash_to_key(&req.h);
                if let Some(store) = self.store.as_ref() {
                    if let Ok(Some(data)) = store.get(&hash_hex) {
                        let response = DataResponse {
                            h: req.h,
                            d: data,
                            i: None,
                            n: None,
                        };
                        let wire = encode_response(&response);
                        self.send_frame(BluetoothFrame::Binary(wire)).await?;
                    }
                }
            }
            DataMessage::Response(res) => {
                let hash_hex = hash_to_key(&res.h);
                if let Some(sender) = self.pending_requests.lock().await.remove(&hash_hex) {
                    let _ = sender.send(Some(res.d));
                }
            }
            other => {
                debug!(
                    "[BluetoothPeer {}] Ignoring unsupported Bluetooth data frame {:?}",
                    self.peer_id.short(),
                    other
                );
            }
        }
        Ok(())
    }

    async fn handle_text_frame(&self, text: String, nostr_client_id: Option<u64>) {
        self.record_received(text.len() as u64).await;
        if let Ok(mesh_frame) = serde_json::from_str::<MeshNostrFrame>(&text) {
            if let Some(tx) = self.mesh_frame_tx.as_ref() {
                let _ = tx.send((self.peer_id.clone(), mesh_frame)).await;
                return;
            }
        }

        if let Ok(relay_msg) = NostrRelayMessage::from_json(&text) {
            if let Some(sub_id) = relay_subscription_id(&relay_msg) {
                let sender = {
                    let pending = self.pending_nostr_queries.lock().await;
                    pending.get(&sub_id).cloned()
                };
                if let Some(tx) = sender {
                    let _ = tx.send(relay_msg);
                    return;
                }
            }
        }

        if let Some(relay) = self.nostr_relay.as_ref() {
            if let Ok(event) = Event::from_json(&text) {
                if self.mark_seen_event_id(event.id.to_hex()).await {
                    let _ = relay
                        .ingest_trusted_event_from_peer(event, Some(self.peer_id.to_string()))
                        .await;
                }
                return;
            }

            if let Ok(nostr_msg) = NostrClientMessage::from_json(&text) {
                if let Some(client_id) = nostr_client_id {
                    relay.handle_client_message(client_id, nostr_msg).await;
                }
            }
        }
    }

    pub fn is_connected(&self) -> bool {
        self.link.is_open()
    }

    pub fn htl_config(&self) -> &PeerHTLConfig {
        &self.htl_config
    }

    async fn record_sent(&self, bytes: u64) {
        if let Some(state) = self.traffic_state.as_ref() {
            state.record_sent(&self.peer_id.to_string(), bytes).await;
        }
    }

    async fn record_received(&self, bytes: u64) {
        if let Some(state) = self.traffic_state.as_ref() {
            state
                .record_received(&self.peer_id.to_string(), bytes)
                .await;
        }
    }

    async fn send_frame(&self, frame: BluetoothFrame) -> Result<()> {
        let bytes = match &frame {
            BluetoothFrame::Text(text) => text.len() as u64,
            BluetoothFrame::Binary(payload) => payload.len() as u64,
        };
        if let Err(err) = self.link.send(frame).await {
            warn!(
                "[BluetoothPeer {}] Failed to send frame over BLE: {}",
                self.peer_id.short(),
                err
            );
            let _ = self.link.close().await;
            return Err(err);
        }
        self.record_sent(bytes).await;
        Ok(())
    }

    pub async fn request_with_timeout(
        &self,
        hash_hex: &str,
        timeout: Duration,
    ) -> Result<Option<Vec<u8>>> {
        if !self.link.is_open() {
            return Ok(None);
        }

        let hash = hex::decode(hash_hex)?;
        let request = DataRequest {
            h: hash,
            htl: BLOB_REQUEST_POLICY.max_htl,
            q: None,
        };
        let wire = encode_request(&request);
        let (tx, rx) = oneshot::channel();
        self.pending_requests
            .lock()
            .await
            .insert(hash_hex.to_string(), tx);
        self.send_frame(BluetoothFrame::Binary(wire)).await?;

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(data)) => Ok(data),
            Ok(Err(_)) => Ok(None),
            Err(_) => {
                self.pending_requests.lock().await.remove(hash_hex);
                Ok(None)
            }
        }
    }

    pub async fn query_nostr_events(
        &self,
        filters: Vec<NostrFilter>,
        timeout: Duration,
    ) -> Result<Vec<Event>> {
        let subscription_id = NostrSubscriptionId::generate();
        let subscription_key = subscription_id.to_string();
        let (tx, mut rx) = mpsc::unbounded_channel::<NostrRelayMessage>();

        self.pending_nostr_queries
            .lock()
            .await
            .insert(subscription_key.clone(), tx);

        let req = NostrClientMessage::req(subscription_id.clone(), filters);
        self.send_frame(BluetoothFrame::Text(req.as_json())).await?;

        let mut events = Vec::new();
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                break;
            }
            match tokio::time::timeout(deadline - now, rx.recv()).await {
                Ok(Some(NostrRelayMessage::Event {
                    subscription_id: sid,
                    event,
                })) if sid == subscription_id => events.push(*event),
                Ok(Some(NostrRelayMessage::EndOfStoredEvents(sid))) if sid == subscription_id => {
                    break;
                }
                Ok(Some(NostrRelayMessage::Closed {
                    subscription_id: sid,
                    ..
                })) if sid == subscription_id => break,
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }

        let close = NostrClientMessage::close(subscription_id.clone());
        let _ = self.send_frame(BluetoothFrame::Text(close.as_json())).await;
        self.pending_nostr_queries
            .lock()
            .await
            .remove(&subscription_key);
        Ok(events)
    }

    pub async fn send_mesh_frame_text(&self, frame: &MeshNostrFrame) -> Result<()> {
        let text = serde_json::to_string(frame)?;
        self.send_frame(BluetoothFrame::Text(text)).await
    }

    pub async fn close(&self) -> Result<()> {
        self.link.close().await
    }
}

fn relay_subscription_id(msg: &NostrRelayMessage) -> Option<String> {
    match msg {
        NostrRelayMessage::Event {
            subscription_id, ..
        } => Some(subscription_id.to_string()),
        NostrRelayMessage::EndOfStoredEvents(subscription_id) => Some(subscription_id.to_string()),
        NostrRelayMessage::Closed {
            subscription_id, ..
        } => Some(subscription_id.to_string()),
        NostrRelayMessage::Count {
            subscription_id, ..
        } => Some(subscription_id.to_string()),
        _ => None,
    }
}

#[cfg(test)]
pub struct MockBluetoothLink {
    open: std::sync::atomic::AtomicBool,
    tx: mpsc::Sender<BluetoothFrame>,
    rx: Mutex<mpsc::Receiver<BluetoothFrame>>,
}

#[cfg(test)]
impl MockBluetoothLink {
    pub fn pair() -> (Arc<Self>, Arc<Self>) {
        let (tx_a, rx_a) = mpsc::channel(32);
        let (tx_b, rx_b) = mpsc::channel(32);
        (
            Arc::new(Self {
                open: std::sync::atomic::AtomicBool::new(true),
                tx: tx_a,
                rx: Mutex::new(rx_b),
            }),
            Arc::new(Self {
                open: std::sync::atomic::AtomicBool::new(true),
                tx: tx_b,
                rx: Mutex::new(rx_a),
            }),
        )
    }
}

#[cfg(test)]
#[async_trait]
impl BluetoothLink for MockBluetoothLink {
    async fn send(&self, frame: BluetoothFrame) -> Result<()> {
        use std::sync::atomic::Ordering;
        if !self.open.load(Ordering::Relaxed) {
            return Ok(());
        }
        self.tx.send(frame).await.map_err(Into::into)
    }

    async fn recv(&self) -> Option<BluetoothFrame> {
        self.rx.lock().await.recv().await
    }

    fn is_open(&self) -> bool {
        use std::sync::atomic::Ordering;
        self.open.load(Ordering::Relaxed)
    }

    async fn close(&self) -> Result<()> {
        use std::sync::atomic::Ordering;
        self.open.store(false, Ordering::Relaxed);
        Ok(())
    }
}
