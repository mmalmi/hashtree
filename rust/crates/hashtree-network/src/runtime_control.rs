use anyhow::Result;
use nostr_sdk::nostr::{Event, Keys, Kind};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{debug, info};

use crate::local_bus::SharedLocalNostrBus;
use crate::mesh_session::{forward_mesh_frame_to_sessions, MeshSession};
use crate::nostr::{decode_signaling_event, encode_signaling_event};
use crate::runtime_peer::{
    can_track_signal_path_peer, remember_peer_signal_path, ConnectionState, MeshPeerEntry,
    PeerSignalPath,
};
use crate::runtime_state::MeshRuntimeState;
use crate::signaling::MeshRouter;
use crate::transport::{PeerLinkFactory, SignalingTransport};
use crate::types::{MeshNostrFrame, PeerId, SignalingMessage, TimedSeenSet, MESH_DEFAULT_HTL};

#[derive(Debug, Clone)]
pub enum PeerStateEvent {
    Connected(PeerId),
    SignalHints(PeerId, Vec<String>),
    Failed(PeerId),
    Disconnected(PeerId),
}

pub fn can_track_source_peer<P>(
    source: &str,
    peer_key: &str,
    peers: &HashMap<String, MeshPeerEntry<P>>,
    max_peers: Option<usize>,
) -> bool {
    match max_peers {
        Some(max_peers) => can_track_signal_path_peer(
            PeerSignalPath::from_source_name(source),
            max_peers,
            peer_key,
            peers,
        ),
        None => true,
    }
}

pub async fn forward_mesh_frame_from_runtime<P>(
    runtime: &MeshRuntimeState<P>,
    frame: &MeshNostrFrame,
    exclude_peer_id: Option<&str>,
) -> usize
where
    P: MeshSession + Clone + Send + Sync + 'static,
{
    let peers = runtime.peers.read().await;
    let peer_refs: Vec<(String, Arc<dyn MeshSession>)> = peers
        .values()
        .filter(|entry| entry.state == ConnectionState::Connected)
        .filter_map(|entry| {
            entry.peer.as_ref().map(|peer| {
                (
                    entry.peer_id.to_string(),
                    Arc::new(peer.clone()) as Arc<dyn MeshSession>,
                )
            })
        })
        .collect();
    drop(peers);

    forward_mesh_frame_to_sessions(peer_refs, frame, exclude_peer_id).await
}

pub async fn create_signaling_event(
    keys: &Keys,
    msg: &SignalingMessage,
    signaling_kind: u64,
) -> Result<Event> {
    encode_signaling_event(
        keys,
        msg.peer_id(),
        msg,
        Kind::from_u16(signaling_kind as u16),
    )
    .map_err(|e| anyhow::anyhow!(e.to_string()))
}

#[allow(clippy::too_many_arguments)]
pub async fn handle_signaling_event<P, R, F>(
    signaling_enabled: bool,
    my_peer_id: &PeerId,
    keys: &Keys,
    runtime: &MeshRuntimeState<P>,
    source: &str,
    source_max_peers: Option<usize>,
    event: &Event,
    shared_router: Option<&Arc<MeshRouter<R, F>>>,
) -> Result<()>
where
    P: MeshSession + Send + Sync + 'static,
    R: SignalingTransport + 'static,
    F: PeerLinkFactory + 'static,
{
    if !signaling_enabled {
        return Ok(());
    }

    let Some(msg) = decode_signaling_event(
        event,
        &my_peer_id.to_string(),
        &keys.public_key().to_hex(),
        keys,
    ) else {
        return Ok(());
    };

    handle_signaling_message(runtime, source, source_max_peers, msg, shared_router).await
}

pub async fn handle_signaling_message<P, R, F>(
    runtime: &MeshRuntimeState<P>,
    source: &str,
    source_max_peers: Option<usize>,
    msg: SignalingMessage,
    shared_router: Option<&Arc<MeshRouter<R, F>>>,
) -> Result<()>
where
    P: MeshSession + Send + Sync + 'static,
    R: SignalingTransport + 'static,
    F: PeerLinkFactory + 'static,
{
    let Some(shared_router) = shared_router else {
        return Ok(());
    };

    if matches!(
        msg,
        SignalingMessage::Hello { .. } | SignalingMessage::Offer { .. }
    ) {
        let peers = runtime.peers.read().await;
        if !can_track_source_peer(source, msg.peer_id(), &peers, source_max_peers) {
            return Ok(());
        }
    }

    debug!(
        "Received {} from {} via {}",
        msg.msg_type(),
        msg.peer_id(),
        source
    );
    let peer_id = msg.peer_id().to_string();
    let peer_hash_get = match &msg {
        SignalingMessage::Hello { hash_get, .. } => Some(*hash_get),
        _ => None,
    };
    shared_router
        .handle_message(msg)
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    if let Some(hash_get) = peer_hash_get {
        runtime.set_peer_hash_get(&peer_id, hash_get).await;
    }
    remember_peer_signal_path(runtime.peers.as_ref(), &peer_id, source).await;

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn dispatch_signaling_message<P, S>(
    signaling_enabled: bool,
    keys: &Keys,
    my_peer_id: &PeerId,
    runtime: &MeshRuntimeState<P>,
    relay_transport: Option<&S>,
    local_buses: &[SharedLocalNostrBus],
    seen_frame_ids: &Arc<Mutex<TimedSeenSet>>,
    seen_event_ids: &Arc<Mutex<TimedSeenSet>>,
    msg: SignalingMessage,
    signaling_kind: u64,
) -> Result<()>
where
    P: MeshSession + Clone + Send + Sync + 'static,
    S: SignalingTransport + Send + Sync + 'static,
{
    if !signaling_enabled {
        debug!(
            "Skipping signaling message {} because signaling is disabled",
            msg.msg_type()
        );
        return Ok(());
    }

    let event = create_signaling_event(keys, &msg, signaling_kind).await?;

    for bus in local_buses {
        if let Err(err) = bus.broadcast_event(&event).await {
            debug!(
                "Failed to broadcast signaling event over {} ({}): {}",
                bus.source_name(),
                msg.msg_type(),
                err
            );
        }
    }

    let mut frame = MeshNostrFrame::new_event(event, &my_peer_id.to_string(), MESH_DEFAULT_HTL);
    if !mark_seen(seen_frame_ids, frame.frame_id.clone()).await {
        runtime.record_mesh_duplicate_drop();
        return Ok(());
    }
    if !mark_seen(seen_event_ids, frame.event().id.to_hex()).await {
        runtime.record_mesh_duplicate_drop();
        return Ok(());
    }

    frame.sender_peer_id = my_peer_id.to_string();
    let forwarded = forward_mesh_frame_from_runtime(runtime, &frame, None).await;
    if forwarded > 0 {
        runtime.record_mesh_forwarded(forwarded as u64);
    }

    if let Some(relay_transport) = relay_transport {
        match tokio::time::timeout(
            Duration::from_millis(500),
            relay_transport.publish(msg.clone()),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                debug!(
                    "Failed to publish signaling message {} via relay transport: {}",
                    msg.msg_type(),
                    err
                );
            }
            Err(_) => {
                debug!(
                    "Timed out publishing signaling message {} via relay transport",
                    msg.msg_type()
                );
            }
        }
    }

    Ok(())
}

pub async fn handle_peer_state_event<P, R, F>(
    runtime: &MeshRuntimeState<P>,
    event: PeerStateEvent,
    shared_router: Option<&Arc<MeshRouter<R, F>>>,
) where
    P: MeshSession + Send + Sync + 'static,
    R: SignalingTransport + 'static,
    F: PeerLinkFactory + 'static,
{
    match event {
        PeerStateEvent::Connected(peer_id) => {
            let peer_key = peer_id.to_string();
            let mut emit_hello = false;
            let mut peers = runtime.peers.write().await;
            if let Some(entry) = peers.get_mut(&peer_key) {
                if entry.state != ConnectionState::Connected {
                    info!("Peer {} connected (via state event)", peer_id.short());
                    entry.state = ConnectionState::Connected;
                    emit_hello = true;
                    runtime
                        .connected_count
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
            drop(peers);
            if emit_hello {
                if let Some(shared_router) = shared_router {
                    let _ = shared_router.send_hello(Vec::new()).await;
                }
            }
        }
        PeerStateEvent::SignalHints(peer_id, signal_urls) => {
            runtime
                .record_known_peer_signal_urls(&peer_id.to_string(), &signal_urls, "peer-link")
                .await;
        }
        PeerStateEvent::Failed(peer_id) => {
            remove_peer_from_runtime(runtime, shared_router, peer_id, "connection failed").await;
        }
        PeerStateEvent::Disconnected(peer_id) => {
            remove_peer_from_runtime(runtime, shared_router, peer_id, "disconnected").await;
        }
    }
}

pub async fn cleanup_stale_peers<P>(runtime: &MeshRuntimeState<P>, stale_timeout: Duration)
where
    P: MeshSession + Send + Sync + 'static,
{
    let mut peers = runtime.peers.write().await;
    let mut connected_count = 0usize;
    let mut to_remove = Vec::new();

    for (key, entry) in peers.iter_mut() {
        if let Some(ref peer) = entry.peer {
            if peer.is_connected() {
                if entry.state != ConnectionState::Connected {
                    info!(
                        "Peer {} is now connected (sync fallback)",
                        entry.peer_id.short()
                    );
                    entry.state = ConnectionState::Connected;
                }
                connected_count += 1;
            } else if entry.state == ConnectionState::Connected {
                info!(
                    "Removing disconnected peer {} after transport closed",
                    entry.peer_id.short()
                );
                to_remove.push(key.clone());
            } else if entry.state == ConnectionState::Connecting
                && entry.last_seen.elapsed() > stale_timeout
            {
                info!(
                    "Removing stale peer {} (stuck in Connecting for {:?})",
                    entry.peer_id.short(),
                    entry.last_seen.elapsed()
                );
                to_remove.push(key.clone());
            }
        } else if entry.state == ConnectionState::Discovered
            && entry.last_seen.elapsed() > stale_timeout
        {
            debug!("Removing stale discovered peer {}", entry.peer_id.short());
            to_remove.push(key.clone());
        }
    }

    let mut removed_peers = Vec::new();
    for key in to_remove {
        if let Some(entry) = peers.remove(&key) {
            removed_peers.push(entry);
        }
    }
    drop(peers);

    for entry in removed_peers {
        if let Some(peer) = entry.peer {
            let _ = peer.close().await;
        }
    }

    runtime
        .connected_count
        .store(connected_count, std::sync::atomic::Ordering::Relaxed);
}

async fn mark_seen(seen: &Arc<Mutex<TimedSeenSet>>, id: String) -> bool {
    let mut seen = seen.lock().await;
    seen.insert_if_new(id)
}

async fn remove_peer_from_runtime<P, R, F>(
    runtime: &MeshRuntimeState<P>,
    shared_router: Option<&Arc<MeshRouter<R, F>>>,
    peer_id: PeerId,
    reason: &str,
) where
    P: MeshSession + Send + Sync + 'static,
    R: SignalingTransport + 'static,
    F: PeerLinkFactory + 'static,
{
    let peer_key = peer_id.to_string();
    info!("Peer {} {} - removing from pool", peer_id.short(), reason);
    let removed = {
        let mut peers = runtime.peers.write().await;
        peers.remove(&peer_key)
    };
    runtime.clear_peer_hash_get(&peer_key).await;
    if let Some(entry) = removed {
        if entry.state == ConnectionState::Connected {
            runtime
                .connected_count
                .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
        if let Some(peer) = entry.peer {
            let _ = peer.close().await;
        }
    }
    if let Some(shared_router) = shared_router {
        if let Some(channel) = shared_router.remove_peer(&peer_key).await {
            channel.close().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result as AnyResult;
    use async_trait::async_trait;
    use nostr_sdk::nostr::{EventBuilder, Filter, Kind};
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Instant;

    use crate::runtime_peer::{MeshPeerEntry, PeerDirection, PeerTransport};
    use crate::types::{PeerHTLConfig, PeerPool};

    #[derive(Clone)]
    struct TestSession {
        connected: bool,
        close_delay: Duration,
        closed: Arc<AtomicBool>,
    }

    #[async_trait]
    impl MeshSession for TestSession {
        fn is_ready(&self) -> bool {
            true
        }

        fn is_connected(&self) -> bool {
            self.connected
        }

        fn htl_config(&self) -> PeerHTLConfig {
            PeerHTLConfig::from_flags(false, false)
        }

        async fn request(&self, _hash_hex: &str, _timeout: Duration) -> AnyResult<Option<Vec<u8>>> {
            Ok(None)
        }

        async fn query_nostr_events(
            &self,
            _filters: Vec<Filter>,
            _timeout: Duration,
        ) -> AnyResult<Vec<Event>> {
            Ok(Vec::new())
        }

        async fn send_mesh_frame_text(&self, _frame: &MeshNostrFrame) -> AnyResult<()> {
            Ok(())
        }

        async fn close(&self) -> AnyResult<()> {
            if !self.close_delay.is_zero() {
                tokio::time::sleep(self.close_delay).await;
            }
            self.closed.store(true, Ordering::Relaxed);
            Ok(())
        }
    }

    #[test]
    fn can_track_source_peer_respects_optional_limits() {
        let peer_id = PeerId::new("peer-a".to_string());
        let peer_key = peer_id.to_string();
        let mut peers = HashMap::new();
        peers.insert(
            peer_key.clone(),
            MeshPeerEntry {
                peer_id,
                direction: PeerDirection::Outbound,
                state: ConnectionState::Discovered,
                last_seen: Instant::now(),
                peer: None::<TestSession>,
                pool: PeerPool::Other,
                transport: PeerTransport::Direct,
                signal_paths: BTreeSet::from([PeerSignalPath::WifiAware]),
                bytes_sent: 0,
                bytes_received: 0,
            },
        );

        assert!(can_track_source_peer("relay", "peer-b", &peers, None));
        assert!(can_track_source_peer(
            "wifi-aware",
            &peer_key,
            &peers,
            Some(1)
        ));
        assert!(!can_track_source_peer(
            "wifi-aware",
            "peer-b",
            &peers,
            Some(1),
        ));
    }

    #[tokio::test]
    async fn cleanup_stale_peers_removes_stale_entries_and_syncs_connected_count() {
        let runtime = MeshRuntimeState::<TestSession>::new();
        let stale_id = PeerId::new("peer-stale".to_string());
        runtime.peers.write().await.insert(
            stale_id.to_string(),
            MeshPeerEntry {
                peer_id: stale_id,
                direction: PeerDirection::Outbound,
                state: ConnectionState::Discovered,
                last_seen: Instant::now() - Duration::from_secs(120),
                peer: None,
                pool: PeerPool::Other,
                transport: PeerTransport::Direct,
                signal_paths: BTreeSet::new(),
                bytes_sent: 0,
                bytes_received: 0,
            },
        );

        let active_id = PeerId::new("peer-active".to_string());
        runtime.peers.write().await.insert(
            active_id.to_string(),
            MeshPeerEntry {
                peer_id: active_id.clone(),
                direction: PeerDirection::Outbound,
                state: ConnectionState::Connecting,
                last_seen: Instant::now(),
                peer: Some(TestSession {
                    connected: true,
                    close_delay: Duration::ZERO,
                    closed: Arc::new(AtomicBool::new(false)),
                }),
                pool: PeerPool::Other,
                transport: PeerTransport::Direct,
                signal_paths: BTreeSet::new(),
                bytes_sent: 0,
                bytes_received: 0,
            },
        );

        cleanup_stale_peers(&runtime, Duration::from_secs(60)).await;

        let peers = runtime.peers.read().await;
        assert!(!peers.contains_key("peer-stale"));
        assert_eq!(
            peers.get(&active_id.to_string()).unwrap().state,
            ConnectionState::Connected
        );
        assert_eq!(
            runtime
                .connected_count
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }

    #[tokio::test]
    async fn handle_peer_state_event_does_not_hold_peer_map_lock_while_closing() {
        let runtime = Arc::new(MeshRuntimeState::<TestSession>::new());
        let peer_id = PeerId::new("peer-a-pub".to_string());
        runtime.peers.write().await.insert(
            peer_id.to_string(),
            MeshPeerEntry {
                peer_id: peer_id.clone(),
                direction: PeerDirection::Outbound,
                state: ConnectionState::Connected,
                last_seen: Instant::now(),
                peer: Some(TestSession {
                    connected: false,
                    close_delay: Duration::from_millis(200),
                    closed: Arc::new(AtomicBool::new(false)),
                }),
                pool: PeerPool::Other,
                transport: PeerTransport::Bluetooth,
                signal_paths: BTreeSet::from([PeerSignalPath::Bluetooth]),
                bytes_sent: 0,
                bytes_received: 0,
            },
        );

        let runtime_for_task = runtime.clone();
        let peer_id_for_task = peer_id.clone();
        let cleanup_task = tokio::spawn(async move {
            handle_peer_state_event::<
                TestSession,
                crate::mock::MockRelayTransport,
                crate::mock::MockConnectionFactory,
            >(
                runtime_for_task.as_ref(),
                PeerStateEvent::Failed(peer_id_for_task),
                None,
            )
            .await;
        });

        tokio::time::sleep(Duration::from_millis(20)).await;

        let remaining = tokio::time::timeout(Duration::from_millis(50), async {
            runtime.peers.read().await.len()
        })
        .await
        .expect("peer map read should not block on close");

        assert_eq!(remaining, 0);
        cleanup_task.await.expect("cleanup task");
    }

    #[tokio::test]
    async fn forward_mesh_frame_from_runtime_sends_to_connected_peers() {
        let runtime = MeshRuntimeState::<TestSession>::new();
        let closed = Arc::new(AtomicBool::new(false));
        let peer_id = PeerId::new("peer-a".to_string());
        runtime.peers.write().await.insert(
            peer_id.to_string(),
            MeshPeerEntry {
                peer_id: peer_id.clone(),
                direction: PeerDirection::Outbound,
                state: ConnectionState::Connected,
                last_seen: Instant::now(),
                peer: Some(TestSession {
                    connected: true,
                    close_delay: Duration::ZERO,
                    closed: closed.clone(),
                }),
                pool: PeerPool::Other,
                transport: PeerTransport::Direct,
                signal_paths: BTreeSet::new(),
                bytes_sent: 0,
                bytes_received: 0,
            },
        );
        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::Custom(25050), "mesh")
            .sign_with_keys(&keys)
            .unwrap();
        let frame = MeshNostrFrame::new_event_with_id(event, "sender", "frame-1", 4);

        let forwarded = forward_mesh_frame_from_runtime(&runtime, &frame, None).await;
        assert_eq!(forwarded, 1);
        assert!(!closed.load(Ordering::Relaxed));
    }
}
