use super::*;

impl WebRTCManager {
    /// Start the native peer router - connects transports and handles signaling.
    pub async fn run(&mut self) -> Result<()> {
        info!(
            "Starting peer router with peer ID: {}",
            self.my_peer_id.short()
        );

        let (event_tx, mut event_rx) = mpsc::channel::<(String, nostr_sdk::nostr::Event)>(100);
        let (relay_msg_tx, mut relay_msg_rx) = mpsc::channel::<SignalingMessage>(100);

        // Take the signaling receiver
        let mut signaling_rx = self
            .signaling_rx
            .take()
            .expect("signaling_rx already taken");

        // Take the state event receiver
        let mut state_event_rx = self
            .state_event_rx
            .take()
            .expect("state_event_rx already taken");
        let mut mesh_frame_rx = self
            .mesh_frame_rx
            .take()
            .expect("mesh_frame_rx already taken");

        if self.config.bluetooth.is_enabled() {
            let bluetooth = BluetoothMesh::new(self.config.bluetooth.clone());
            let context = BluetoothRuntimeContext {
                my_peer_id: self.my_peer_id.clone(),
                store: if bluetooth_nostr_only_mode() {
                    None
                } else {
                    self.store.clone()
                },
                nostr_relay: self.nostr_relay.clone(),
                mesh_frame_tx: self.mesh_frame_tx.clone(),
                registrar: BluetoothPeerRegistrar::new(
                    self.state.clone(),
                    self.peer_classifier.clone(),
                    self.config.pools.clone(),
                    self.config.bluetooth.max_peers,
                ),
            };
            let _ = bluetooth.start(context).await;
        }

        let relay_transport = if self.config.signaling_enabled {
            let transport = Arc::new(NostrRelayTransport::new(
                self.keys.clone(),
                self.config.debug,
            ));
            transport
                .connect(&self.config.relays)
                .await
                .map_err(|e| anyhow::anyhow!(e.to_string()))?;

            let relay_reader = transport.clone();
            let relay_msg_tx = relay_msg_tx.clone();
            tokio::spawn(async move {
                while let Some(msg) = relay_reader.recv().await {
                    if relay_msg_tx.send(msg).await.is_err() {
                        break;
                    }
                }
            });

            Some(transport)
        } else {
            None
        };

        if self.config.multicast.is_enabled() {
            if let Some(relay) = self.nostr_relay.clone() {
                let relay = relay as crate::SharedMeshEventStore;
                match MulticastNostrBus::bind(
                    self.config.multicast.clone(),
                    self.keys.clone(),
                    relay,
                )
                .await
                {
                    Ok(bus) => {
                        let local_bus: SharedLocalNostrBus = bus.clone();
                        self.state.add_local_bus(local_bus.clone()).await;
                        self.local_buses.push(local_bus);
                        let shutdown_rx = self.shutdown_rx.clone();
                        let signaling_tx = event_tx.clone();
                        tokio::spawn(async move {
                            if let Err(err) = bus.run(shutdown_rx, signaling_tx).await {
                                error!("Multicast bus error: {}", err);
                            }
                        });
                    }
                    Err(err) => {
                        warn!("Failed to start multicast bus: {}", err);
                    }
                }
            } else {
                warn!("Multicast enabled but Nostr relay is unavailable");
            }
        }

        if self.config.wifi_aware.is_enabled() {
            if let Some(relay) = self.nostr_relay.clone() {
                if let Some(bridge) = mobile_wifi_aware_bridge() {
                    let relay = relay as crate::SharedMeshEventStore;
                    let bus = WifiAwareNostrBus::new(
                        self.config.wifi_aware.clone(),
                        self.keys.clone(),
                        relay,
                        bridge,
                    );
                    let local_bus: SharedLocalNostrBus = bus.clone();
                    self.state.add_local_bus(local_bus.clone()).await;
                    self.local_buses.push(local_bus);
                    let shutdown_rx = self.shutdown_rx.clone();
                    let signaling_tx = event_tx.clone();
                    let local_peer_id = self.my_peer_id.to_string();
                    tokio::spawn(async move {
                        if let Err(err) = bus.run(local_peer_id, shutdown_rx, signaling_tx).await {
                            error!("Wi-Fi Aware bus error: {}", err);
                        }
                    });
                } else {
                    warn!("Wi-Fi Aware enabled but no mobile bridge is installed");
                }
            } else {
                warn!("Wi-Fi Aware enabled but Nostr relay is unavailable");
            }
        }

        if self.config.signaling_enabled {
            let transport = Arc::new(RouterSignalingBridge::new(
                self.my_peer_id.to_string(),
                self.signaling_tx.clone(),
            ));
            let factory = Arc::new(SharedRouterPeerFactory::new(
                self.my_peer_id.clone(),
                self.signaling_tx.clone(),
                self.config.stun_servers.clone(),
                self.store.clone(),
                self.state.clone(),
                self.state_event_tx.clone(),
                self.nostr_relay.clone(),
                self.mesh_frame_tx.clone(),
                self.peer_classifier.clone(),
            ));
            let (classifier_tx, mut classifier_rx) = mpsc::channel::<SharedClassifyRequest>(32);
            let classifier = self.peer_classifier.clone();
            tokio::spawn(async move {
                while let Some(request) = classifier_rx.recv().await {
                    let _ = request.response.send(classifier(&request.pubkey));
                }
            });

            let mut router = MeshRouter::new(
                self.my_peer_id.to_string(),
                transport,
                factory.clone(),
                self.config.pools.clone(),
                self.config.debug,
            );
            router.set_classifier(classifier_tx);
            router.set_hash_get_enabled(self.config.hash_get_enabled);
            self.shared_router = Some(Arc::new(router));
        }

        // Process incoming events and outgoing signaling messages
        let mut shutdown_rx = self.shutdown_rx.clone();
        // Cleanup interval - run every 30 seconds as a fallback (not for real-time sync)
        let mut cleanup_interval = tokio::time::interval(Duration::from_secs(30));
        let mut hello_ticker =
            tokio::time::interval(Duration::from_millis(self.config.hello_interval_ms));
        if self.config.signaling_enabled {
            if let Some(shared_router) = self.shared_router.as_ref() {
                let _ = shared_router.send_hello(Vec::new()).await;
            }
        }
        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        info!("WebRTC manager shutting down");
                        break;
                    }
                }
                Some(msg) = relay_msg_rx.recv() => {
                    if let Err(e) = self
                        .handle_signaling_message("relay", msg, self.shared_router.as_ref())
                        .await
                    {
                        debug!("Error handling relay signaling message: {}", e);
                    }
                }
                Some((relay, event)) = event_rx.recv() => {
                    if let Err(e) = self
                        .handle_event(&relay, &event, self.shared_router.as_ref())
                        .await
                    {
                        debug!("Error handling event from {}: {}", relay, e);
                    }
                }
                Some(msg) = signaling_rx.recv() => {
                    self.dispatch_signaling_message(msg, relay_transport.as_ref()).await;
                }
                Some(event) = state_event_rx.recv() => {
                    // Handle peer state events (connected, failed, disconnected)
                    self.handle_peer_state_event(event).await;
                }
                Some((from_peer_id, frame)) = mesh_frame_rx.recv() => {
                    self.handle_mesh_frame(from_peer_id, frame).await;
                }
                _ = hello_ticker.tick(), if self.config.signaling_enabled => {
                    if let Some(shared_router) = self.shared_router.as_ref() {
                        let _ = shared_router.send_hello(Vec::new()).await;
                    }
                }
                _ = cleanup_interval.tick() => {
                    // Periodic cleanup of stale peers and state sync (fallback)
                    self.cleanup_stale_peers().await;
                }
            }
        }

        Ok(())
    }

    async fn mark_seen_frame_id(&self, frame_id: String) -> bool {
        let mut seen = self.seen_frame_ids.lock().await;
        seen.insert_if_new(frame_id)
    }

    async fn mark_seen_event_id(&self, event_id: String) -> bool {
        let mut seen = self.seen_event_ids.lock().await;
        seen.insert_if_new(event_id)
    }

    async fn dispatch_signaling_message(
        &self,
        msg: SignalingMessage,
        relay_transport: Option<&Arc<NostrRelayTransport>>,
    ) {
        if let Err(err) = crate::dispatch_signaling_message(
            self.config.signaling_enabled,
            &self.keys,
            &self.my_peer_id,
            &self.state.runtime,
            relay_transport,
            &self.local_buses,
            &self.seen_frame_ids,
            &self.seen_event_ids,
            msg,
            MESH_SIGNALING_EVENT_KIND as u64,
        )
        .await
        {
            debug!("Failed to dispatch signaling message: {}", err);
        }
    }

    async fn forward_mesh_frame(
        &self,
        frame: &MeshNostrFrame,
        exclude_peer_id: Option<&str>,
    ) -> usize {
        crate::forward_mesh_frame_from_runtime(&self.state.runtime, frame, exclude_peer_id).await
    }

    async fn handle_mesh_frame(&self, from_peer_id: PeerId, frame: MeshNostrFrame) {
        if let Err(reason) = validate_mesh_frame(&frame) {
            debug!(
                "Ignoring mesh frame from {} (invalid: {})",
                from_peer_id.short(),
                reason
            );
            return;
        }

        if !self.mark_seen_frame_id(frame.frame_id.clone()).await {
            self.state.record_mesh_duplicate_drop();
            return;
        }

        let event = match &frame.payload {
            MeshNostrPayload::Event { event } => event.clone(),
        };

        if !self.mark_seen_event_id(event.id.to_hex()).await {
            self.state.record_mesh_duplicate_drop();
            return;
        }

        if event.verify().is_err() {
            debug!(
                "Ignoring mesh event from {} due to invalid signature",
                from_peer_id.short()
            );
            return;
        }

        self.state.record_mesh_received();

        if let Err(e) = self
            .handle_event("mesh", &event, self.shared_router.as_ref())
            .await
        {
            debug!(
                "Error handling mesh event from {}: {}",
                from_peer_id.short(),
                e
            );
        }

        let forwarded = self
            .forward_mesh_frame(&frame, Some(&from_peer_id.to_string()))
            .await;
        if forwarded > 0 {
            self.state.record_mesh_forwarded(forwarded as u64);
        }
    }

    /// Handle an incoming event
    ///
    /// Messages may be:
    /// 1. Hello messages: kind 25050 with #l: "hello" tag and peerId
    /// 2. Gift-wrapped directed messages: kind 25050 with #p tag, encrypted with ephemeral key
    async fn handle_event(
        &self,
        relay: &str,
        event: &nostr_sdk::nostr::Event,
        shared_router: Option<&Arc<SharedProductionRouter>>,
    ) -> Result<()> {
        crate::handle_signaling_event(
            self.config.signaling_enabled,
            &self.my_peer_id,
            &self.keys,
            &self.state.runtime,
            relay,
            self.local_bus_max_peers(relay),
            event,
            shared_router,
        )
        .await
    }

    async fn handle_signaling_message(
        &self,
        source: &str,
        msg: SignalingMessage,
        shared_router: Option<&Arc<SharedProductionRouter>>,
    ) -> Result<()> {
        crate::handle_signaling_message(
            &self.state.runtime,
            source,
            self.local_bus_max_peers(source),
            msg,
            shared_router,
        )
        .await
    }

    /// Handle peer state change events from peer connections
    async fn handle_peer_state_event(&self, event: PeerStateEvent) {
        crate::handle_peer_state_event(&self.state.runtime, event, self.shared_router.as_ref())
            .await;
    }

    /// Cleanup stale peers and sync connection states (fallback, runs every 30s)
    async fn cleanup_stale_peers(&self) {
        crate::cleanup_stale_peers(&self.state.runtime, Duration::from_secs(60)).await;
    }
}

// Keep the old PeerState for backward compatibility with tests
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct PeerState {
    pub peer_id: PeerId,
    pub direction: PeerDirection,
    pub state: String,
    pub last_seen: Instant,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::root_events::{self, PeerRootEvent};
    use crate::session::TestMeshPeer;
    use crate::LocalNostrBus;
    use crate::SelectionStrategy;
    use crate::{build_hedged_wave_plan, normalize_dispatch_config};
    use anyhow::Result as AnyResult;
    use async_trait::async_trait;
    use nostr_sdk::nostr::{EventBuilder, Keys, Tag};
    use std::time::Duration;

    struct TestLocalBus {
        source: &'static str,
        root: Option<PeerRootEvent>,
    }

    #[async_trait]
    impl LocalNostrBus for TestLocalBus {
        fn source_name(&self) -> &'static str {
            self.source
        }

        async fn broadcast_event(&self, _event: &nostr_sdk::nostr::Event) -> AnyResult<()> {
            Ok(())
        }

        async fn query_root(
            &self,
            _owner_pubkey: &str,
            _tree_name: &str,
            _timeout: Duration,
        ) -> Option<PeerRootEvent> {
            self.root.clone()
        }
    }

    #[test]
    fn root_event_from_peer_extracts_tags() {
        let keys = Keys::generate();
        let hash = "ab".repeat(32);
        let event = EventBuilder::new(
            Kind::Custom(root_events::HASHTREE_KIND),
            "",
            [
                Tag::parse(&["d", "repo"]).unwrap(),
                Tag::parse(&["l", root_events::HASHTREE_LABEL]).unwrap(),
                Tag::parse(&["hash", &hash]).unwrap(),
                Tag::parse(&["encryptedKey", &"11".repeat(32)]).unwrap(),
            ],
        )
        .to_event(&keys)
        .unwrap();

        let parsed = root_events::root_event_from_peer(&event, "peer-a", "repo").unwrap();
        let expected_encrypted = "11".repeat(32);
        assert_eq!(parsed.hash, hash);
        assert_eq!(parsed.peer_id, "peer-a");
        assert_eq!(
            parsed.encrypted_key.as_deref(),
            Some(expected_encrypted.as_str())
        );
        assert!(parsed.key.is_none());
    }

    #[test]
    fn pick_latest_event_prefers_higher_event_id_on_timestamp_tie() {
        let keys = Keys::generate();
        let created_at = nostr_sdk::nostr::Timestamp::from_secs(1_700_000_000);
        let event_a = EventBuilder::new(Kind::Custom(root_events::HASHTREE_KIND), "", [])
            .custom_created_at(created_at)
            .to_event(&keys)
            .unwrap();
        let event_b = EventBuilder::new(Kind::Custom(root_events::HASHTREE_KIND), "", [])
            .custom_created_at(created_at)
            .to_event(&keys)
            .unwrap();

        let expected = if event_a.id > event_b.id {
            event_a.id
        } else {
            event_b.id
        };
        let picked = root_events::pick_latest_event([&event_a, &event_b]).unwrap();
        assert_eq!(picked.id, expected);
    }

    #[tokio::test]
    async fn resolve_root_from_local_buses_returns_source_and_first_match() {
        let state = WebRTCState::new();
        let root = PeerRootEvent {
            hash: "ab".repeat(32),
            key: None,
            encrypted_key: None,
            self_encrypted_key: None,
            event_id: "event-1".to_string(),
            created_at: 1,
            peer_id: "bus-peer".to_string(),
        };

        state
            .set_local_buses(vec![
                Arc::new(TestLocalBus {
                    source: "empty",
                    root: None,
                }),
                Arc::new(TestLocalBus {
                    source: "mock-bus",
                    root: Some(root.clone()),
                }),
            ])
            .await;

        let resolved = state
            .resolve_root_from_local_buses_with_source("owner", "tree", Duration::from_millis(10))
            .await
            .expect("expected root from local bus");

        assert_eq!(resolved.0, "mock-bus");
        assert_eq!(resolved.1.hash, root.hash);
        assert_eq!(resolved.1.peer_id, root.peer_id);
    }

    #[tokio::test]
    async fn can_track_local_bus_peer_enforces_wifi_aware_limit() {
        let keys = Keys::generate();
        let mut config = WebRTCConfig::default();
        config.wifi_aware.enabled = true;
        config.wifi_aware.max_peers = 1;
        let manager = WebRTCManager::new(keys, config);
        let existing_peer = PeerId::new("peer-a".to_string());
        let existing_key = existing_peer.to_string();
        let mut peers = HashMap::new();
        peers.insert(
            existing_key.clone(),
            PeerEntry {
                peer_id: existing_peer,
                direction: PeerDirection::Outbound,
                state: ConnectionState::Discovered,
                last_seen: Instant::now(),
                peer: None,
                pool: PeerPool::Other,
                transport: PeerTransport::WebRtc,
                signal_paths: BTreeSet::from([PeerSignalPath::WifiAware]),
                bytes_sent: 0,
                bytes_received: 0,
            },
        );

        assert!(manager.can_track_local_bus_peer(WIFI_AWARE_SOURCE, &existing_key, &peers,));
        assert!(!manager.can_track_local_bus_peer(WIFI_AWARE_SOURCE, "peer-b:sess-b", &peers,));
        assert!(manager.can_track_local_bus_peer("relay", "peer-c:sess-c", &peers));
    }

    #[tokio::test]
    async fn request_from_peers_with_source_accepts_generic_mesh_peers() {
        let state = WebRTCState::new();
        let data = b"offline-over-ble".to_vec();
        let hash_hex = hex::encode(hashtree_core::sha256(&data));

        state.runtime.peers.write().await.insert(
            "peer-a".to_string(),
            PeerEntry {
                peer_id: PeerId::new("peer-a-pub".to_string()),
                direction: PeerDirection::Outbound,
                state: ConnectionState::Connected,
                last_seen: Instant::now(),
                peer: Some(MeshPeer::mock_for_tests(TestMeshPeer::with_response(Some(
                    data.clone(),
                )))),
                pool: PeerPool::Other,
                transport: PeerTransport::Bluetooth,
                signal_paths: BTreeSet::from([PeerSignalPath::Bluetooth]),
                bytes_sent: 0,
                bytes_received: 0,
            },
        );

        let resolved = state
            .request_from_peers_with_source(&hash_hex)
            .await
            .expect("expected mock mesh peer response");

        assert_eq!(resolved.0, data);
        assert_eq!(resolved.1, "peer-a-pub");
    }

    #[tokio::test]
    async fn request_from_peers_with_source_waits_full_timeout_for_last_generic_peer() {
        let state = WebRTCState::new_with_routing_and_cashu(
            SelectionStrategy::Weighted,
            true,
            RequestDispatchConfig {
                initial_fanout: 1,
                hedge_fanout: 1,
                max_fanout: 1,
                hedge_interval_ms: 50,
            },
            Duration::from_millis(400),
            CashuRoutingConfig::default(),
            None,
            None,
        );
        let data = b"slow-offline-over-ble".to_vec();
        let hash_hex = hex::encode(hashtree_core::sha256(&data));

        state.runtime.peers.write().await.insert(
            "peer-a".to_string(),
            PeerEntry {
                peer_id: PeerId::new("peer-a-pub".to_string()),
                direction: PeerDirection::Outbound,
                state: ConnectionState::Connected,
                last_seen: Instant::now(),
                peer: Some(MeshPeer::mock_for_tests(
                    TestMeshPeer::with_delayed_response(
                        Some(data.clone()),
                        Duration::from_millis(200),
                    ),
                )),
                pool: PeerPool::Other,
                transport: PeerTransport::Bluetooth,
                signal_paths: BTreeSet::from([PeerSignalPath::Bluetooth]),
                bytes_sent: 0,
                bytes_received: 0,
            },
        );

        let resolved = state
            .request_from_peers_with_source(&hash_hex)
            .await
            .expect("expected delayed mock mesh peer response");

        assert_eq!(resolved.0, data);
        assert_eq!(resolved.1, "peer-a-pub");
    }

    #[tokio::test]
    async fn request_from_peers_with_source_skips_peers_with_hash_get_disabled() {
        let state = WebRTCState::new();
        let capable_data = b"hash-get-capable".to_vec();
        let capable_hash_hex = hex::encode(hashtree_core::sha256(&capable_data));

        state.runtime.peers.write().await.insert(
            "peer-assist".to_string(),
            PeerEntry {
                peer_id: PeerId::new("peer-assist-pub".to_string()),
                direction: PeerDirection::Outbound,
                state: ConnectionState::Connected,
                last_seen: Instant::now(),
                peer: Some(MeshPeer::mock_for_tests(TestMeshPeer::with_response(Some(
                    b"assist-should-not-be-queried".to_vec(),
                )))),
                pool: PeerPool::Other,
                transport: PeerTransport::Bluetooth,
                signal_paths: BTreeSet::from([PeerSignalPath::Bluetooth]),
                bytes_sent: 0,
                bytes_received: 0,
            },
        );
        state
            .runtime
            .set_peer_hash_get("peer-assist-pub", false)
            .await;

        state.runtime.peers.write().await.insert(
            "peer-capable".to_string(),
            PeerEntry {
                peer_id: PeerId::new("peer-capable-pub".to_string()),
                direction: PeerDirection::Outbound,
                state: ConnectionState::Connected,
                last_seen: Instant::now(),
                peer: Some(MeshPeer::mock_for_tests(TestMeshPeer::with_response(Some(
                    capable_data.clone(),
                )))),
                pool: PeerPool::Other,
                transport: PeerTransport::Bluetooth,
                signal_paths: BTreeSet::from([PeerSignalPath::Bluetooth]),
                bytes_sent: 0,
                bytes_received: 0,
            },
        );
        state
            .runtime
            .set_peer_hash_get("peer-capable-pub", true)
            .await;

        let resolved = state
            .request_from_peers_with_source(&capable_hash_hex)
            .await
            .expect("expected capable peer response");

        assert_eq!(resolved.0, capable_data);
        assert_eq!(resolved.1, "peer-capable-pub");
    }

    #[tokio::test]
    async fn dispatch_signaling_message_is_noop_when_signaling_disabled() {
        let keys = Keys::generate();
        let mut config = WebRTCConfig::default();
        config.signaling_enabled = false;
        let manager = WebRTCManager::new(keys, config);
        let peer_id = PeerId::new("peer-a-pub".to_string());
        let peer_key = peer_id.to_string();
        let peer = MeshPeer::mock_for_tests(TestMeshPeer::with_response(None));
        let peer_ref = peer.mock_ref().expect("mock peer").clone();

        manager.state.runtime.peers.write().await.insert(
            peer_key,
            PeerEntry {
                peer_id,
                direction: PeerDirection::Outbound,
                state: ConnectionState::Connected,
                last_seen: Instant::now(),
                peer: Some(peer),
                pool: PeerPool::Other,
                transport: PeerTransport::Bluetooth,
                signal_paths: BTreeSet::from([PeerSignalPath::Bluetooth]),
                bytes_sent: 0,
                bytes_received: 0,
            },
        );

        manager
            .dispatch_signaling_message(
                SignalingMessage::Hello {
                    peer_id: manager.my_peer_id.to_string(),
                    roots: Vec::new(),
                    hash_get: true,
                },
                None,
            )
            .await;

        assert_eq!(peer_ref.sent_frame_count().await, 0);
    }

    #[tokio::test]
    async fn failed_peer_cleanup_does_not_hold_peer_map_lock_while_closing() {
        let keys = Keys::generate();
        let manager = Arc::new(WebRTCManager::new(keys, WebRTCConfig::default()));
        let peer_id = PeerId::new("peer-a-pub".to_string());
        let peer_key = peer_id.to_string();

        manager.state.runtime.peers.write().await.insert(
            peer_key.clone(),
            PeerEntry {
                peer_id: peer_id.clone(),
                direction: PeerDirection::Outbound,
                state: ConnectionState::Connected,
                last_seen: Instant::now(),
                peer: Some(MeshPeer::mock_for_tests(TestMeshPeer::with_delayed_close(
                    Duration::from_millis(200),
                ))),
                pool: PeerPool::Other,
                transport: PeerTransport::Bluetooth,
                signal_paths: BTreeSet::from([PeerSignalPath::Bluetooth]),
                bytes_sent: 0,
                bytes_received: 0,
            },
        );

        let manager_for_task = manager.clone();
        let peer_id_for_task = peer_id.clone();
        let cleanup_task = tokio::spawn(async move {
            manager_for_task
                .handle_peer_state_event(PeerStateEvent::Failed(peer_id_for_task))
                .await;
        });

        tokio::time::sleep(Duration::from_millis(20)).await;

        let remaining = tokio::time::timeout(Duration::from_millis(50), async {
            manager.state.runtime.peers.read().await.len()
        })
        .await
        .expect("peer map read should not block on close");

        assert_eq!(remaining, 0);
        cleanup_task.await.expect("cleanup task");
    }

    #[tokio::test]
    async fn resolve_root_from_peers_does_not_hold_peer_map_lock_while_querying() {
        let keys = Keys::generate();
        let manager = Arc::new(WebRTCManager::new(keys.clone(), WebRTCConfig::default()));
        let owner_keys = Keys::generate();
        let owner_pubkey = owner_keys.public_key().to_hex();
        let tree_name = "video";
        let hash = "ab".repeat(32);
        let event = EventBuilder::new(
            Kind::Custom(root_events::HASHTREE_KIND),
            "",
            [
                Tag::parse(&["d", tree_name]).unwrap(),
                Tag::parse(&["l", root_events::HASHTREE_LABEL]).unwrap(),
                Tag::parse(&["hash", &hash]).unwrap(),
            ],
        )
        .to_event(&owner_keys)
        .unwrap();

        let peer_id = PeerId::new("peer-a-pub".to_string());
        let peer_key = peer_id.to_string();

        manager.state.runtime.peers.write().await.insert(
            peer_key.clone(),
            PeerEntry {
                peer_id,
                direction: PeerDirection::Outbound,
                state: ConnectionState::Connected,
                last_seen: Instant::now(),
                peer: Some(MeshPeer::mock_for_tests(TestMeshPeer::with_delayed_events(
                    vec![event],
                    Duration::from_millis(200),
                ))),
                pool: PeerPool::Other,
                transport: PeerTransport::Bluetooth,
                signal_paths: BTreeSet::from([PeerSignalPath::Bluetooth]),
                bytes_sent: 0,
                bytes_received: 0,
            },
        );

        let manager_for_task = manager.clone();
        let owner_pubkey_for_task = owner_pubkey.clone();
        let resolve_task = tokio::spawn(async move {
            manager_for_task
                .state
                .resolve_root_from_peers(
                    &owner_pubkey_for_task,
                    tree_name,
                    Duration::from_millis(500),
                )
                .await
        });

        tokio::time::sleep(Duration::from_millis(20)).await;

        let manager_for_writer = manager.clone();
        let peer_key_for_writer = peer_key.clone();
        let writer_task = tokio::spawn(async move {
            let mut peers = manager_for_writer.state.runtime.peers.write().await;
            if let Some(entry) = peers.get_mut(&peer_key_for_writer) {
                entry.bytes_received += 1;
            }
        });

        tokio::time::sleep(Duration::from_millis(20)).await;

        let status_count = tokio::time::timeout(Duration::from_millis(50), async {
            manager.state.runtime.peers.read().await.len()
        })
        .await
        .expect("peer map read should not block on root query");

        assert_eq!(status_count, 1);
        assert!(resolve_task.await.expect("resolve task").is_some());
        writer_task.await.expect("writer task");
    }

    #[test]
    fn test_formal_timed_seen_set_rejects_duplicates() {
        let mut seen = TimedSeenSet::new(4, Duration::from_secs(60));
        assert!(seen.insert_if_new("frame-1".to_string()));
        assert!(!seen.insert_if_new("frame-1".to_string()));
        assert!(seen.insert_if_new("frame-2".to_string()));
    }

    #[test]
    fn test_formal_timed_seen_set_evicts_oldest_when_capacity_exceeded() {
        let mut seen = TimedSeenSet::new(2, Duration::from_secs(60));
        assert!(seen.insert_if_new("a".to_string()));
        assert!(seen.insert_if_new("b".to_string()));
        assert!(seen.insert_if_new("c".to_string()));

        // "a" should be evicted due to cap=2, so re-insert becomes new again.
        assert!(seen.insert_if_new("a".to_string()));
        assert!(!seen.insert_if_new("a".to_string()));
    }

    #[test]
    fn test_request_dispatch_normalization_caps_to_available_peers() {
        let normalized = normalize_dispatch_config(
            RequestDispatchConfig {
                initial_fanout: 8,
                hedge_fanout: 6,
                max_fanout: 5,
                hedge_interval_ms: 120,
            },
            3,
        );
        assert_eq!(normalized.max_fanout, 3);
        assert_eq!(normalized.initial_fanout, 3);
        assert_eq!(normalized.hedge_fanout, 3);
    }

    #[test]
    fn test_hedged_wave_plan_matches_dispatch_policy() {
        let plan = build_hedged_wave_plan(
            7,
            RequestDispatchConfig {
                initial_fanout: 2,
                hedge_fanout: 3,
                max_fanout: 6,
                hedge_interval_ms: 120,
            },
        );
        assert_eq!(plan, vec![2, 3, 1]);
    }
}
