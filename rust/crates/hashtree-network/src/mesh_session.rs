use anyhow::Result;
use async_trait::async_trait;
use nostr_sdk::nostr::{Event, Filter};
use std::sync::Arc;
use std::time::Duration;

use crate::local_bus::SharedLocalNostrBus;
use crate::root_events::{
    build_root_filter, hashtree_event_identifier, is_hashtree_labeled_event, pick_latest_event,
    root_event_from_peer, PeerRootEvent,
};
use crate::types::{
    decrement_htl_with_policy, should_forward_htl, MeshNostrFrame, PeerHTLConfig, MESH_EVENT_POLICY,
};

#[async_trait]
pub trait MeshSession: Send + Sync {
    fn is_ready(&self) -> bool;
    fn is_connected(&self) -> bool;
    fn htl_config(&self) -> PeerHTLConfig;

    async fn request(&self, hash_hex: &str, timeout: Duration) -> Result<Option<Vec<u8>>>;

    async fn query_nostr_events(
        &self,
        filters: Vec<Filter>,
        timeout: Duration,
    ) -> Result<Vec<Event>>;

    async fn send_mesh_frame_text(&self, frame: &MeshNostrFrame) -> Result<()>;

    async fn close(&self) -> Result<()>;

    fn transport_debug_state(&self) -> Option<String> {
        None
    }
}

pub async fn resolve_root_from_peer_sessions(
    peer_refs: Vec<(String, Arc<dyn MeshSession>)>,
    owner_pubkey: &str,
    tree_name: &str,
    per_peer_timeout: Duration,
) -> Option<PeerRootEvent> {
    let filter = build_root_filter(owner_pubkey, tree_name)?;

    for (peer_label, peer) in peer_refs {
        if !peer.is_ready() {
            continue;
        }

        let events = match peer
            .query_nostr_events(vec![filter.clone()], per_peer_timeout)
            .await
        {
            Ok(events) => events,
            Err(_) => continue,
        };

        let latest = pick_latest_event(events.iter().filter(|event| {
            hashtree_event_identifier(event).as_deref() == Some(tree_name)
                && is_hashtree_labeled_event(event)
        }));
        if let Some(event) = latest {
            if let Some(root) = root_event_from_peer(event, &peer_label, tree_name) {
                return Some(root);
            }
        }
    }

    None
}

pub async fn resolve_root_from_local_buses_with_source(
    buses: Vec<SharedLocalNostrBus>,
    owner_pubkey: &str,
    tree_name: &str,
    timeout: Duration,
) -> Option<(&'static str, PeerRootEvent)> {
    for bus in buses {
        if let Some(root) = bus.query_root(owner_pubkey, tree_name, timeout).await {
            return Some((bus.source_name(), root));
        }
    }
    None
}

pub async fn forward_mesh_frame_to_sessions(
    sessions: Vec<(String, Arc<dyn MeshSession>)>,
    frame: &MeshNostrFrame,
    exclude_peer_id: Option<&str>,
) -> usize {
    let mut forwarded = 0usize;

    for (peer_id, session) in sessions {
        if exclude_peer_id
            .map(|exclude| exclude == peer_id.as_str())
            .unwrap_or(false)
        {
            continue;
        }
        if !session.is_ready() {
            continue;
        }

        let next_htl =
            decrement_htl_with_policy(frame.htl, &MESH_EVENT_POLICY, &session.htl_config());
        if !should_forward_htl(next_htl) {
            continue;
        }

        let mut outbound = frame.clone();
        outbound.htl = next_htl;
        if session.send_mesh_frame_text(&outbound).await.is_ok() {
            forwarded += 1;
        }
    }

    forwarded
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_bus::LocalNostrBus;
    use crate::root_events::{HASHTREE_KIND, HASHTREE_LABEL};
    use crate::types::{MESH_DEFAULT_HTL, NOSTR_KIND_HASHTREE};
    use nostr_sdk::nostr::{EventBuilder, Keys, Kind, Tag, Timestamp};
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::Mutex;

    struct TestSession {
        ready: bool,
        connected: bool,
        htl_config: PeerHTLConfig,
        query_events: Mutex<Vec<Event>>,
        sent_frames: Mutex<Vec<MeshNostrFrame>>,
        closed: AtomicBool,
    }

    impl TestSession {
        fn with_events(events: Vec<Event>) -> Arc<Self> {
            Arc::new(Self {
                ready: true,
                connected: true,
                htl_config: PeerHTLConfig::from_flags(false, false),
                query_events: Mutex::new(events),
                sent_frames: Mutex::new(Vec::new()),
                closed: AtomicBool::new(false),
            })
        }

        fn with_htl_config(htl_config: PeerHTLConfig) -> Arc<Self> {
            Arc::new(Self {
                ready: true,
                connected: true,
                htl_config,
                query_events: Mutex::new(Vec::new()),
                sent_frames: Mutex::new(Vec::new()),
                closed: AtomicBool::new(false),
            })
        }
    }

    #[async_trait]
    impl MeshSession for TestSession {
        fn is_ready(&self) -> bool {
            self.ready
        }

        fn is_connected(&self) -> bool {
            self.connected
        }

        fn htl_config(&self) -> PeerHTLConfig {
            self.htl_config
        }

        async fn request(&self, _hash_hex: &str, _timeout: Duration) -> Result<Option<Vec<u8>>> {
            Ok(None)
        }

        async fn query_nostr_events(
            &self,
            _filters: Vec<Filter>,
            _timeout: Duration,
        ) -> Result<Vec<Event>> {
            Ok(self.query_events.lock().await.clone())
        }

        async fn send_mesh_frame_text(&self, frame: &MeshNostrFrame) -> Result<()> {
            self.sent_frames.lock().await.push(frame.clone());
            Ok(())
        }

        async fn close(&self) -> Result<()> {
            self.closed.store(true, Ordering::Relaxed);
            Ok(())
        }
    }

    struct TestLocalBus {
        source: &'static str,
        root: Option<PeerRootEvent>,
    }

    #[async_trait]
    impl LocalNostrBus for TestLocalBus {
        fn source_name(&self) -> &'static str {
            self.source
        }

        async fn broadcast_event(&self, _event: &Event) -> Result<()> {
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

    fn build_root_event(keys: &Keys, tree_name: &str, hash_hex: &str, created_at: u64) -> Event {
        EventBuilder::new(Kind::Custom(HASHTREE_KIND), "")
            .tags([
                Tag::parse(["d", tree_name]).expect("d tag"),
                Tag::parse(["l", HASHTREE_LABEL]).expect("label tag"),
                Tag::parse(["hash", hash_hex]).expect("hash tag"),
            ])
            .custom_created_at(Timestamp::from_secs(created_at))
            .sign_with_keys(keys)
            .expect("root event")
    }

    fn build_mesh_event(keys: &Keys) -> Event {
        EventBuilder::new(Kind::Custom(NOSTR_KIND_HASHTREE), "mesh")
            .sign_with_keys(keys)
            .expect("mesh event")
    }

    #[tokio::test]
    async fn resolve_root_from_peer_sessions_returns_matching_latest_event() {
        let owner_keys = Keys::generate();
        let tree_name = "repo";
        let newer_hash = "bb".repeat(32);
        let older_hash = "aa".repeat(32);
        let older = build_root_event(&owner_keys, tree_name, &older_hash, 10);
        let newer = build_root_event(&owner_keys, tree_name, &newer_hash, 20);
        let peer = TestSession::with_events(vec![older, newer]);

        let resolved = resolve_root_from_peer_sessions(
            vec![("peer-a".to_string(), peer as Arc<dyn MeshSession>)],
            &owner_keys.public_key().to_hex(),
            tree_name,
            Duration::from_millis(10),
        )
        .await
        .expect("resolved root");

        assert_eq!(resolved.hash, newer_hash);
        assert_eq!(resolved.peer_id, "peer-a");
    }

    #[tokio::test]
    async fn resolve_root_from_local_buses_with_source_returns_first_match() {
        let root = PeerRootEvent {
            hash: "ab".repeat(32),
            key: None,
            encrypted_key: None,
            self_encrypted_key: None,
            event_id: "event-1".to_string(),
            created_at: 1,
            peer_id: "bus-peer".to_string(),
        };

        let resolved = resolve_root_from_local_buses_with_source(
            vec![
                Arc::new(TestLocalBus {
                    source: "empty",
                    root: None,
                }) as SharedLocalNostrBus,
                Arc::new(TestLocalBus {
                    source: "mock-bus",
                    root: Some(root.clone()),
                }) as SharedLocalNostrBus,
            ],
            "owner",
            "tree",
            Duration::from_millis(10),
        )
        .await
        .expect("resolved root");

        assert_eq!(resolved.0, "mock-bus");
        assert_eq!(resolved.1, root);
    }

    #[tokio::test]
    async fn forward_mesh_frame_to_sessions_skips_excluded_and_applies_per_peer_htl() {
        let keys = Keys::generate();
        let frame = MeshNostrFrame::new_event(build_mesh_event(&keys), "sender", MESH_DEFAULT_HTL);
        let first = TestSession::with_htl_config(PeerHTLConfig::from_flags(false, false));
        let second = TestSession::with_htl_config(PeerHTLConfig::from_flags(true, false));

        let forwarded = forward_mesh_frame_to_sessions(
            vec![
                ("peer-a".to_string(), first.clone() as Arc<dyn MeshSession>),
                ("peer-b".to_string(), second.clone() as Arc<dyn MeshSession>),
            ],
            &frame,
            Some("peer-a"),
        )
        .await;

        assert_eq!(forwarded, 1);
        assert!(first.sent_frames.lock().await.is_empty());
        let sent = second.sent_frames.lock().await;
        assert_eq!(sent.len(), 1);
        assert!(sent[0].htl < frame.htl);
    }
}
