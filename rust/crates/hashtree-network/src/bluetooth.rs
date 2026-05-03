use anyhow::{anyhow, Result};
use async_trait::async_trait;
#[cfg(target_os = "macos")]
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
#[cfg(target_os = "macos")]
use std::time::Instant;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::session::MeshPeer;
#[cfg(test)]
use super::types::PeerPool;
use super::types::{MeshNostrFrame, PeerId, PoolSettings};
use crate::bluetooth_peer::{BluetoothFrame, BluetoothLink, BluetoothPeer};
use crate::manager::{PeerClassifier, WebRTCState};
use crate::peer::ContentStore;
use crate::relay_bridge::SharedMeshRelayClient;
use crate::runtime_peer::PeerDirection;
use crate::runtime_peer::{PeerSignalPath, PeerTransport, TransportPeerRegistrar};

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const HTREE_BLE_SERVICE_UUID: &str = "f18ef5f6-b7ee-4f40-b869-10a2d4f35932";
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const HTREE_BLE_RX_CHARACTERISTIC_UUID: &str = "0bb5f5c9-6369-4511-a84f-4d4c14d8f8d4";
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const HTREE_BLE_TX_CHARACTERISTIC_UUID: &str = "4ec9c0c2-97c6-4f46-9fd1-927d699b2f6d";
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
// Keep BLE chunks comfortably below conservative cross-platform GATT write budgets.
pub const HTREE_BLE_CHUNK_BYTES: usize = 64;
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

/// Configuration for the optional Bluetooth peer transport.
#[derive(Debug, Clone, Default)]
pub struct BluetoothConfig {
    pub enabled: bool,
    pub max_peers: usize,
}

impl BluetoothConfig {
    pub fn is_enabled(&self) -> bool {
        self.enabled && self.max_peers > 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BluetoothBackendState {
    Disabled,
    Running,
    Unsupported,
}

#[derive(Clone)]
pub struct PendingBluetoothLink {
    pub link: Arc<dyn BluetoothLink>,
    pub direction: PeerDirection,
    pub local_hello_sent: bool,
    pub peer_hint: Option<String>,
}

#[async_trait]
pub trait MobileBluetoothBridge: Send + Sync {
    async fn start(&self, local_peer_id: String) -> Result<mpsc::Receiver<PendingBluetoothLink>>;
}

static MOBILE_BLUETOOTH_BRIDGE: OnceLock<Arc<dyn MobileBluetoothBridge>> = OnceLock::new();

pub fn install_mobile_bluetooth_bridge(bridge: Arc<dyn MobileBluetoothBridge>) -> Result<()> {
    MOBILE_BLUETOOTH_BRIDGE
        .set(bridge)
        .map_err(|_| anyhow!("mobile bluetooth bridge already installed"))
}

fn mobile_bluetooth_bridge() -> Option<Arc<dyn MobileBluetoothBridge>> {
    MOBILE_BLUETOOTH_BRIDGE.get().cloned()
}

#[derive(Clone)]
pub struct BluetoothPeerRegistrar {
    pub state: Arc<WebRTCState>,
    inner: TransportPeerRegistrar<MeshPeer>,
}

impl BluetoothPeerRegistrar {
    pub fn new(
        state: Arc<WebRTCState>,
        peer_classifier: PeerClassifier,
        pools: PoolSettings,
        max_bluetooth_peers: usize,
    ) -> Self {
        Self {
            inner: TransportPeerRegistrar::new(
                state.runtime.peers.clone(),
                state.runtime.connected_count.clone(),
                peer_classifier,
                pools,
                PeerTransport::Bluetooth,
                PeerSignalPath::Bluetooth,
                max_bluetooth_peers,
            ),
            state,
        }
    }

    pub async fn register_connected_peer(
        &self,
        peer_id: PeerId,
        direction: PeerDirection,
        peer: MeshPeer,
    ) -> bool {
        let accepted = self
            .inner
            .register_connected_peer(peer_id.clone(), direction, peer)
            .await;
        if !accepted {
            warn!(
                "Bluetooth peer {} rejected by shared registrar",
                peer_id.short()
            );
        }
        accepted
    }

    pub async fn unregister_peer(&self, peer_id: &PeerId) {
        self.inner.unregister_peer(peer_id).await;
    }

    pub async fn unregister_bluetooth_peer_if_current(
        &self,
        peer_id: &PeerId,
        expected_peer: &Arc<BluetoothPeer>,
    ) {
        self.inner
            .unregister_peer_if(peer_id, |peer| match peer {
                MeshPeer::Bluetooth(current) => Arc::ptr_eq(current, expected_peer),
                _ => false,
            })
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bluetooth_peer::{BluetoothLink, MockBluetoothLink};
    use crate::session::{MeshPeer, TestMeshPeer};

    #[tokio::test]
    async fn register_connected_peer_closes_replaced_session() {
        let state = Arc::new(WebRTCState::new());
        let registrar = BluetoothPeerRegistrar::new(
            state.clone(),
            Arc::new(|_| PeerPool::Other),
            PoolSettings::default(),
            2,
        );

        let peer_id = PeerId::new("peer-pub".to_string());
        let first = MeshPeer::mock_for_tests(TestMeshPeer::with_response(None));
        let second = MeshPeer::mock_for_tests(TestMeshPeer::with_response(None));
        let first_ref = first.mock_ref().expect("mock peer").clone();

        assert!(
            registrar
                .register_connected_peer(peer_id.clone(), PeerDirection::Outbound, first)
                .await
        );
        assert!(
            registrar
                .register_connected_peer(peer_id, PeerDirection::Outbound, second)
                .await
        );

        assert!(first_ref.is_closed());
    }

    #[tokio::test]
    async fn register_connected_peer_replaces_existing_bluetooth_session_for_same_pubkey() {
        let state = Arc::new(WebRTCState::new());
        let registrar = BluetoothPeerRegistrar::new(
            state.clone(),
            Arc::new(|_| PeerPool::Other),
            PoolSettings::default(),
            2,
        );

        let first_peer_id = PeerId::new("peer-pub".to_string());
        let second_peer_id = PeerId::new("peer-pub".to_string());
        let first = MeshPeer::mock_for_tests(TestMeshPeer::with_response(None));
        let second = MeshPeer::mock_for_tests(TestMeshPeer::with_response(None));
        let first_ref = first.mock_ref().expect("mock peer").clone();

        assert!(
            registrar
                .register_connected_peer(first_peer_id.clone(), PeerDirection::Outbound, first)
                .await
        );
        assert!(
            registrar
                .register_connected_peer(second_peer_id.clone(), PeerDirection::Outbound, second)
                .await
        );

        assert!(first_ref.is_closed());
        let peers = state.runtime.peers.read().await;
        assert!(peers.contains_key(&second_peer_id.to_string()));
        assert_eq!(peers.len(), 1);
        assert_eq!(
            state
                .runtime
                .connected_count
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }

    #[tokio::test]
    async fn handle_pending_link_unregisters_peer_after_transport_closes() {
        let (link_a, link_b) = MockBluetoothLink::pair();
        let state = Arc::new(WebRTCState::new());
        let registrar = BluetoothPeerRegistrar::new(
            state.clone(),
            Arc::new(|_| PeerPool::Other),
            PoolSettings::default(),
            2,
        );
        let (mesh_frame_tx, _mesh_frame_rx) = mpsc::channel(4);
        let local_peer_id = PeerId::new("local-pub".to_string());
        let remote_peer_id = PeerId::new("remote-pub".to_string());
        let remote_link: Arc<dyn BluetoothLink> = link_b.clone();

        send_hello(&remote_link, &remote_peer_id)
            .await
            .expect("send hello");

        let accepted = handle_pending_link(
            PendingBluetoothLink {
                link: link_a.clone(),
                direction: PeerDirection::Outbound,
                local_hello_sent: false,
                peer_hint: Some("mock-ble".to_string()),
            },
            BluetoothRuntimeContext {
                my_peer_id: local_peer_id,
                store: None,
                nostr_relay: None,
                mesh_frame_tx,
                registrar: registrar.clone(),
            },
        )
        .await
        .expect("handle pending link");

        assert!(accepted);
        assert!(state
            .runtime
            .peers
            .read()
            .await
            .contains_key(&remote_peer_id.to_string()));

        link_a.close().await.expect("close local transport");

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if !state
                    .runtime
                    .peers
                    .read()
                    .await
                    .contains_key(&remote_peer_id.to_string())
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("peer should be unregistered");
    }
}

#[derive(Clone)]
pub struct BluetoothRuntimeContext {
    pub my_peer_id: PeerId,
    pub store: Option<Arc<dyn ContentStore>>,
    pub nostr_relay: Option<SharedMeshRelayClient>,
    pub mesh_frame_tx: mpsc::Sender<(PeerId, MeshNostrFrame)>,
    pub registrar: BluetoothPeerRegistrar,
}

/// Native Bluetooth backend. Nearby BLE links become transport-native mesh peers.
pub struct BluetoothMesh {
    config: BluetoothConfig,
}

impl BluetoothMesh {
    pub fn new(config: BluetoothConfig) -> Self {
        Self { config }
    }

    pub async fn start(&self, context: BluetoothRuntimeContext) -> BluetoothBackendState {
        if !self.config.is_enabled() {
            info!("Bluetooth transport disabled");
            return BluetoothBackendState::Disabled;
        }

        let mut started = false;

        if let Some(bridge) = mobile_bluetooth_bridge() {
            info!(
                "Starting mobile Bluetooth bridge for peer {}",
                context.my_peer_id.short()
            );
            match bridge.start(context.my_peer_id.to_string()).await {
                Ok(mut rx) => {
                    info!("Mobile Bluetooth bridge is running");
                    let ctx = context.clone();
                    tokio::spawn(async move {
                        while let Some(link) = rx.recv().await {
                            if let Err(err) = handle_pending_link(link, ctx.clone()).await {
                                warn!("Mobile Bluetooth link failed: {}", err);
                            }
                        }
                    });
                    started = true;
                }
                Err(err) => {
                    warn!("Failed to start mobile Bluetooth bridge: {}", err);
                }
            }
        }

        #[cfg(target_os = "macos")]
        {
            if let Err(err) = macos::start_macos_central(self.config.clone(), context.clone()).await
            {
                warn!("Failed to start macOS Bluetooth central: {}", err);
            } else {
                started = true;
            }
        }

        if started {
            BluetoothBackendState::Running
        } else {
            warn!(
                "Bluetooth transport requested (max_peers={}) but no native backend is available for this build",
                self.config.max_peers
            );
            BluetoothBackendState::Unsupported
        }
    }
}

async fn handle_pending_link(
    link: PendingBluetoothLink,
    context: BluetoothRuntimeContext,
) -> Result<bool> {
    if let Some(peer_hint) = link.peer_hint.as_deref() {
        debug!("Handling pending Bluetooth link {}", peer_hint);
    }
    info!(
        "Handling pending Bluetooth link direction={} local_hello_sent={}",
        link.direction, link.local_hello_sent
    );
    let remote_peer_id = receive_remote_hello(&link.link).await?;
    info!(
        "Received Bluetooth hello from {} while attaching pending link",
        remote_peer_id.short()
    );
    if !link.local_hello_sent {
        send_hello(&link.link, &context.my_peer_id).await?;
        info!(
            "Sent Bluetooth hello to {} while attaching pending link",
            remote_peer_id.short()
        );
    }

    let bluetooth_peer = BluetoothPeer::new(
        remote_peer_id.clone(),
        link.direction,
        link.link,
        context.store.clone(),
        context.nostr_relay.clone(),
        Some(context.mesh_frame_tx.clone()),
        Some(context.registrar.state.clone()),
    );
    let peer = MeshPeer::Bluetooth(bluetooth_peer.clone());

    if !context
        .registrar
        .register_connected_peer(remote_peer_id.clone(), link.direction, peer)
        .await
    {
        warn!("Rejecting Bluetooth peer {}", remote_peer_id.short());
        bluetooth_peer.close().await?;
        return Ok(false);
    } else {
        info!("Bluetooth peer {} connected", remote_peer_id.short());
        spawn_bluetooth_disconnect_watch(remote_peer_id, bluetooth_peer, context.registrar.clone());
    }
    Ok(true)
}

fn spawn_bluetooth_disconnect_watch(
    peer_id: PeerId,
    peer: Arc<BluetoothPeer>,
    registrar: BluetoothPeerRegistrar,
) {
    tokio::spawn(async move {
        loop {
            if !peer.is_connected() {
                registrar
                    .unregister_bluetooth_peer_if_current(&peer_id, &peer)
                    .await;
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    });
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct BluetoothHello {
    #[serde(rename = "type")]
    kind: String,
    peer_id: String,
}

async fn send_hello(link: &Arc<dyn BluetoothLink>, my_peer_id: &PeerId) -> Result<()> {
    let hello = BluetoothHello {
        kind: "hello".to_string(),
        peer_id: my_peer_id.to_string(),
    };
    link.send(BluetoothFrame::Text(serde_json::to_string(&hello)?))
        .await
}

async fn receive_remote_hello(link: &Arc<dyn BluetoothLink>) -> Result<PeerId> {
    let result = tokio::time::timeout(HELLO_TIMEOUT, async {
        loop {
            match link.recv().await {
                Some(BluetoothFrame::Text(text)) => {
                    if let Ok(hello) = serde_json::from_str::<BluetoothHello>(&text) {
                        if hello.kind == "hello" {
                            return PeerId::from_string(&hello.peer_id)
                                .ok_or_else(|| anyhow!("invalid peer id in Bluetooth hello"));
                        }
                    }
                }
                Some(BluetoothFrame::Binary(_)) => {}
                None => return Err(anyhow!("bluetooth link closed before hello")),
            }
        }
    })
    .await;

    match result {
        Ok(peer_id) => peer_id,
        Err(_) => Err(anyhow!("timed out waiting for Bluetooth hello")),
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use btleplug::api::{
        Central, CentralEvent, CharPropFlags, Characteristic, Manager as _, Peripheral as _,
        ScanFilter, ValueNotification, WriteType,
    };
    use btleplug::platform::{Manager, Peripheral};
    use futures::StreamExt;
    use std::collections::HashSet;
    use tokio::sync::Mutex;
    use uuid::Uuid;

    const FRESH_ADVERTISEMENT_WINDOW: Duration = Duration::from_secs(10);

    #[derive(Clone)]
    struct AdvertisementSnapshot {
        last_seen: Instant,
        peer_hint: Option<String>,
    }

    pub(super) async fn start_macos_central(
        config: BluetoothConfig,
        context: BluetoothRuntimeContext,
    ) -> Result<()> {
        struct ConnectedPeripheral {
            peripheral: Peripheral,
            link: Arc<dyn BluetoothLink>,
            peer_hint: Option<String>,
        }

        let manager = Manager::new().await?;
        let adapters = manager.adapters().await?;
        let Some(adapter) = adapters.into_iter().next() else {
            warn!("No Bluetooth adapters available on macOS");
            return Ok(());
        };
        info!(
            "Starting macOS Bluetooth central for peer {} (max_peers={})",
            context.my_peer_id.short(),
            config.max_peers
        );
        let service_uuid = Uuid::parse_str(HTREE_BLE_SERVICE_UUID).expect("valid UUID");
        let mut events = adapter.events().await?;
        let fresh_advertisements =
            Arc::new(Mutex::new(HashMap::<String, AdvertisementSnapshot>::new()));
        let fresh_advertisements_for_events = fresh_advertisements.clone();
        tokio::spawn(async move {
            while let Some(event) = events.next().await {
                match event {
                    CentralEvent::ServicesAdvertisement { id, services }
                        if services.contains(&service_uuid) =>
                    {
                        let mut advertisements = fresh_advertisements_for_events.lock().await;
                        let entry = advertisements.entry(id.to_string()).or_insert_with(|| {
                            AdvertisementSnapshot {
                                last_seen: Instant::now(),
                                peer_hint: None,
                            }
                        });
                        entry.last_seen = Instant::now();
                    }
                    CentralEvent::ServiceDataAdvertisement { id, service_data } => {
                        if let Some(data) = service_data.get(&service_uuid) {
                            let mut advertisements = fresh_advertisements_for_events.lock().await;
                            let entry = advertisements.entry(id.to_string()).or_insert_with(|| {
                                AdvertisementSnapshot {
                                    last_seen: Instant::now(),
                                    peer_hint: None,
                                }
                            });
                            entry.last_seen = Instant::now();
                            if !data.is_empty() {
                                entry.peer_hint = Some(hex::encode(data));
                            }
                        }
                    }
                    CentralEvent::DeviceDisconnected(id) => {
                        fresh_advertisements_for_events
                            .lock()
                            .await
                            .remove(&id.to_string());
                    }
                    _ => {}
                }
            }
        });

        tokio::spawn(async move {
            let mut connected: HashMap<String, ConnectedPeripheral> = HashMap::new();
            loop {
                if let Err(err) = adapter
                    .start_scan(ScanFilter {
                        services: vec![service_uuid],
                    })
                    .await
                {
                    warn!("Failed to start BLE scan: {}", err);
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;

                let peripherals = match adapter.peripherals().await {
                    Ok(peripherals) => peripherals,
                    Err(err) => {
                        warn!("Failed to list BLE peripherals: {}", err);
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        continue;
                    }
                };

                connected.retain(|_, connection| {
                    futures::executor::block_on(async {
                        let peripheral_connected =
                            connection.peripheral.is_connected().await.unwrap_or(false);
                        let link_open = connection.link.is_open();
                        if !peripheral_connected || !link_open {
                            if peripheral_connected {
                                let _ = connection.peripheral.disconnect().await;
                            }
                            false
                        } else {
                            true
                        }
                    })
                });

                let advertisement_snapshot = {
                    let now = Instant::now();
                    let mut seen = fresh_advertisements.lock().await;
                    seen.retain(|_, advertisement| {
                        now.saturating_duration_since(advertisement.last_seen)
                            <= FRESH_ADVERTISEMENT_WINDOW
                    });
                    seen.clone()
                };

                let mut candidates = Vec::new();
                for peripheral in peripherals {
                    let peripheral_id = peripheral.id().to_string();
                    if connected.contains_key(&peripheral_id) {
                        continue;
                    }
                    let Some(advertisement) = advertisement_snapshot.get(&peripheral_id).cloned()
                    else {
                        debug!(
                            "Skipping cached BLE peripheral {} without a fresh advertisement",
                            peripheral_id
                        );
                        continue;
                    };
                    let properties = match peripheral.properties().await {
                        Ok(Some(properties)) => properties,
                        _ => continue,
                    };
                    if !properties.services.contains(&service_uuid) {
                        continue;
                    }
                    candidates.push((peripheral, peripheral_id, advertisement));
                }

                candidates.sort_by(|left, right| right.2.last_seen.cmp(&left.2.last_seen));
                let mut represented_peer_hints = connected
                    .values()
                    .filter_map(|connection| connection.peer_hint.clone())
                    .collect::<HashSet<_>>();

                for (peripheral, peripheral_id, advertisement) in candidates {
                    if connected.len() >= config.max_peers {
                        break;
                    }
                    if let Some(peer_hint) = advertisement.peer_hint.as_ref() {
                        if !represented_peer_hints.insert(peer_hint.clone()) {
                            debug!(
                                "Skipping stale BLE peripheral {} because peer hint {} is already represented by a fresher advertisement or live link",
                                peripheral_id,
                                peer_hint
                            );
                            continue;
                        }
                    }
                    info!(
                        "Discovered BLE peripheral {} advertising htree service{}",
                        peripheral_id,
                        advertisement
                            .peer_hint
                            .as_ref()
                            .map(|hint| format!(" (peer hint {})", hint))
                            .unwrap_or_default()
                    );
                    match connect_peripheral(peripheral.clone()).await {
                        Ok(Some(link)) => {
                            let tracked_link = link.clone();
                            let pending = PendingBluetoothLink {
                                link,
                                direction: PeerDirection::Outbound,
                                local_hello_sent: false,
                                peer_hint: advertisement
                                    .peer_hint
                                    .clone()
                                    .or(Some(peripheral_id.clone())),
                            };
                            match handle_pending_link(pending, context.clone()).await {
                                Ok(true) => {
                                    connected.insert(
                                        peripheral_id.clone(),
                                        ConnectedPeripheral {
                                            peripheral,
                                            link: tracked_link,
                                            peer_hint: advertisement.peer_hint.clone(),
                                        },
                                    );
                                }
                                Ok(false) => {}
                                Err(err) => {
                                    warn!("Failed to attach BLE peripheral: {}", err);
                                }
                            }
                        }
                        Ok(None) => {}
                        Err(err) => {
                            warn!("Skipping BLE peripheral {}: {}", peripheral_id, err);
                        }
                    }
                }

                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });

        Ok(())
    }

    async fn connect_peripheral(peripheral: Peripheral) -> Result<Option<Arc<dyn BluetoothLink>>> {
        let rx_uuid = Uuid::parse_str(HTREE_BLE_RX_CHARACTERISTIC_UUID)?;
        let tx_uuid = Uuid::parse_str(HTREE_BLE_TX_CHARACTERISTIC_UUID)?;
        let peripheral_id = peripheral.id().to_string();

        let mut connect_timed_out = false;
        if !peripheral.is_connected().await? {
            info!("Connecting to BLE peripheral {}", peripheral_id);
            match tokio::time::timeout(Duration::from_secs(25), peripheral.connect()).await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => return Err(err.into()),
                Err(_) => {
                    warn!(
                        "BLE connect timed out for {}; probing services before giving up",
                        peripheral_id
                    );
                    connect_timed_out = true;
                }
            }
        }
        if connect_timed_out && !peripheral.is_connected().await.unwrap_or(false) {
            warn!(
                "BLE peripheral {} never reached a connected state after timeout; retrying later",
                peripheral_id
            );
            let _ = peripheral.disconnect().await;
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut rx_char = None;
        let mut tx_char = None;
        for attempt in 1..=8 {
            info!(
                "Discovering BLE services for {} (attempt {}/8)",
                peripheral_id, attempt
            );
            if let Err(err) = peripheral.discover_services().await {
                if attempt == 8 {
                    if connect_timed_out {
                        warn!(
                            "BLE service discovery failed for {} after soft connect timeout: {}",
                            peripheral_id, err
                        );
                        let _ = peripheral.disconnect().await;
                        return Ok(None);
                    }
                    return Err(err.into());
                }
                warn!(
                    "BLE service discovery attempt {}/8 failed for {}: {}",
                    attempt, peripheral_id, err
                );
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }

            let characteristics = peripheral.characteristics();
            if !characteristics.is_empty() {
                let uuids = characteristics
                    .iter()
                    .map(|c| c.uuid.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                info!(
                    "BLE peripheral {} characteristics: {}",
                    peripheral_id, uuids
                );
            }
            rx_char = characteristics.iter().find(|c| c.uuid == rx_uuid).cloned();
            tx_char = characteristics.iter().find(|c| c.uuid == tx_uuid).cloned();
            if rx_char.is_some() && tx_char.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let Some(rx_char) = rx_char else {
            warn!("BLE peripheral {} missing RX characteristic", peripheral_id);
            let _ = peripheral.disconnect().await;
            return Ok(None);
        };
        let Some(tx_char) = tx_char else {
            warn!("BLE peripheral {} missing TX characteristic", peripheral_id);
            let _ = peripheral.disconnect().await;
            return Ok(None);
        };
        if !tx_char.properties.contains(CharPropFlags::NOTIFY) {
            warn!(
                "BLE peripheral {} TX characteristic is missing NOTIFY",
                peripheral_id
            );
            let _ = peripheral.disconnect().await;
            return Ok(None);
        }
        let notifications = peripheral.notifications().await?;
        info!("Subscribing to BLE notifications for {}", peripheral_id);
        peripheral.subscribe(&tx_char).await?;

        let initial_frames = match peripheral.read(&tx_char).await {
            Ok(bytes) if !bytes.is_empty() => {
                info!(
                    "Read initial BLE hello bytes from {} ({} bytes)",
                    peripheral_id,
                    bytes.len()
                );
                let mut decoder = FrameDecoder::new();
                match decoder.push(&bytes) {
                    Ok(frames) => frames,
                    Err(err) => {
                        warn!(
                            "Discarding malformed initial BLE frame from {}: {}",
                            peripheral_id, err
                        );
                        Vec::new()
                    }
                }
            }
            Ok(_) => Vec::new(),
            Err(err) => {
                warn!("Initial BLE read failed for {}: {}", peripheral_id, err);
                Vec::new()
            }
        };

        info!("BLE peripheral {} ready for mesh traffic", peripheral_id);
        Ok(Some(MacosCentralLink::new(
            peripheral,
            rx_char,
            notifications,
            initial_frames,
        )))
    }

    struct FrameDecoder {
        buffer: Vec<u8>,
    }

    impl FrameDecoder {
        fn new() -> Self {
            Self { buffer: Vec::new() }
        }

        fn push(&mut self, chunk: &[u8]) -> Result<Vec<BluetoothFrame>> {
            self.buffer.extend_from_slice(chunk);
            let mut frames = Vec::new();
            loop {
                if self.buffer.len() < 5 {
                    break;
                }
                let kind = self.buffer[0];
                let len = u32::from_be_bytes([
                    self.buffer[1],
                    self.buffer[2],
                    self.buffer[3],
                    self.buffer[4],
                ]) as usize;
                if self.buffer.len() < 5 + len {
                    break;
                }
                let payload = self.buffer[5..5 + len].to_vec();
                self.buffer.drain(..5 + len);
                let frame = match kind {
                    1 => BluetoothFrame::Text(String::from_utf8(payload)?),
                    2 => BluetoothFrame::Binary(payload),
                    _ => return Err(anyhow!("unknown bluetooth frame kind {}", kind)),
                };
                frames.push(frame);
            }
            Ok(frames)
        }
    }

    fn encode_frame(frame: BluetoothFrame) -> Vec<u8> {
        match frame {
            BluetoothFrame::Text(text) => {
                let payload = text.into_bytes();
                let mut out = Vec::with_capacity(5 + payload.len());
                out.push(1);
                out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
                out.extend_from_slice(&payload);
                out
            }
            BluetoothFrame::Binary(payload) => {
                let mut out = Vec::with_capacity(5 + payload.len());
                out.push(2);
                out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
                out.extend_from_slice(&payload);
                out
            }
        }
    }

    struct MacosCentralLink {
        peripheral: Peripheral,
        rx_char: Characteristic,
        open: std::sync::atomic::AtomicBool,
        rx: Mutex<mpsc::Receiver<BluetoothFrame>>,
    }

    impl MacosCentralLink {
        fn new(
            peripheral: Peripheral,
            rx_char: Characteristic,
            mut notifications: impl futures::Stream<Item = ValueNotification> + Send + Unpin + 'static,
            initial_frames: Vec<BluetoothFrame>,
        ) -> Arc<Self> {
            let (tx, rx) = mpsc::channel(64);
            let link = Arc::new(Self {
                peripheral,
                rx_char,
                open: std::sync::atomic::AtomicBool::new(true),
                rx: Mutex::new(rx),
            });

            for frame in initial_frames {
                let _ = tx.try_send(frame);
            }

            let weak = Arc::downgrade(&link);
            tokio::spawn(async move {
                let mut decoder = FrameDecoder::new();
                while let Some(notification) = notifications.next().await {
                    let Ok(frames) = decoder.push(&notification.value) else {
                        break;
                    };
                    for frame in frames {
                        if tx.send(frame).await.is_err() {
                            break;
                        }
                    }
                }
                if let Some(link) = weak.upgrade() {
                    link.open.store(false, std::sync::atomic::Ordering::Relaxed);
                }
            });

            link
        }
    }

    #[async_trait]
    impl BluetoothLink for MacosCentralLink {
        async fn send(&self, frame: BluetoothFrame) -> Result<()> {
            let bytes = encode_frame(frame);
            for chunk in bytes.chunks(HTREE_BLE_CHUNK_BYTES) {
                self.peripheral
                    .write(&self.rx_char, chunk, WriteType::WithResponse)
                    .await?;
            }
            Ok(())
        }

        async fn recv(&self) -> Option<BluetoothFrame> {
            self.rx.lock().await.recv().await
        }

        fn is_open(&self) -> bool {
            self.open.load(std::sync::atomic::Ordering::Relaxed)
        }

        async fn close(&self) -> Result<()> {
            self.open.store(false, std::sync::atomic::Ordering::Relaxed);
            let _ = self.peripheral.disconnect().await;
            Ok(())
        }
    }
}
