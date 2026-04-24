//! Native mesh connectivity for hashtree data exchange.
//!
//! The default negotiated path is Nostr signaling plus WebRTC data channels,
//! but this module also layers in nearby/offline transports such as Bluetooth,
//! multicast, and Wi-Fi Aware around the same mesh state and routing logic.

mod bluetooth;
mod bluetooth_peer;
mod cashu;
mod local_bus;
mod multicast;
mod peer;
mod root_events;
mod session;
mod signaling;
pub mod types;
mod wifi_aware;

#[cfg(test)]
mod tests;

pub use bluetooth::{
    install_mobile_bluetooth_bridge, BluetoothBackendState, BluetoothConfig, BluetoothMesh,
    MobileBluetoothBridge, PendingBluetoothLink,
};
pub use bluetooth_peer::{BluetoothFrame, BluetoothLink, BluetoothPeer};
pub use cashu::{cashu_mint_metadata_path, CashuMintMetadataStore, CashuRoutingConfig};
pub use hashtree_network::{KnownPeerRecord, KnownPeerSnapshot};
pub use local_bus::{LocalNostrBus, SharedLocalNostrBus};
pub use multicast::{MulticastConfig, MulticastNostrBus};
pub use peer::{ContentStore, Peer, PendingRequest};
pub use root_events::PeerRootEvent;
pub(crate) use root_events::{build_root_filter, pick_latest_event, root_event_from_peer};
pub use session::MeshPeer;
pub use signaling::{
    ConnectionState, PeerClassifier, PeerEntry, PeerSignalPath, PeerTransport, WebRTCManager,
    WebRTCState,
};
pub use types::{
    encode_request, DataMessage, DataRequest, PeerDirection, PeerId, PeerPool, PoolConfig,
    PoolSettings, RequestDispatchConfig, SelectionStrategy, SignalingMessage, WebRTCConfig,
    MAX_HTL,
};
pub use wifi_aware::{
    install_mobile_wifi_aware_bridge, MobileWifiAwareBridge, WifiAwareConfig, WifiAwareEvent,
    WifiAwareNostrBus,
};
