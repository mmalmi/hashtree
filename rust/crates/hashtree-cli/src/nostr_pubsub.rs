use std::sync::Arc;

use anyhow::{Context, Result};
use hashtree_core::Store;
use hashtree_fips_transport::FipsMeshPubsub;
use hashtree_network::{
    MeshStoreCore, PeerLinkFactory, PubsubEvent, PubsubPublishStats, SignalingTransport,
};
use nostr::{Event, JsonUtil};
use tracing::warn;

use crate::nostr_relay::NostrRelay;

pub const NOSTR_EVENT_PUBSUB_STREAM: &str = "nostr.events.v1";
pub const MAX_NOSTR_PUBSUB_EVENT_BYTES: usize = 256 * 1024;

pub async fn subscribe_nostr_events<S, R, F>(
    mesh: &Arc<MeshStoreCore<S, R, F>>,
) -> PubsubPublishStats
where
    S: Store + Send + Sync + 'static,
    R: SignalingTransport + Send + Sync + 'static,
    F: PeerLinkFactory + Send + Sync + 'static,
{
    mesh.subscribe_pubsub(NOSTR_EVENT_PUBSUB_STREAM).await
}

pub async fn publish_nostr_event<S, R, F>(
    mesh: &Arc<MeshStoreCore<S, R, F>>,
    seq: u64,
    event: &Event,
) -> Result<PubsubPublishStats>
where
    S: Store + Send + Sync + 'static,
    R: SignalingTransport + Send + Sync + 'static,
    F: PeerLinkFactory + Send + Sync + 'static,
{
    publish_nostr_event_with_limit(mesh, seq, event, MAX_NOSTR_PUBSUB_EVENT_BYTES).await
}

pub async fn publish_nostr_event_with_limit<S, R, F>(
    mesh: &Arc<MeshStoreCore<S, R, F>>,
    seq: u64,
    event: &Event,
    max_event_bytes: usize,
) -> Result<PubsubPublishStats>
where
    S: Store + Send + Sync + 'static,
    R: SignalingTransport + Send + Sync + 'static,
    F: PeerLinkFactory + Send + Sync + 'static,
{
    event
        .verify()
        .map_err(|err| anyhow::anyhow!("invalid nostr event signature: {err}"))?;
    let payload = event.as_json().into_bytes();
    if payload.len() > max_event_bytes {
        anyhow::bail!("nostr pubsub event exceeds {} bytes", max_event_bytes);
    }
    Ok(mesh
        .publish_pubsub(NOSTR_EVENT_PUBSUB_STREAM, seq, payload)
        .await)
}

pub async fn publish_fips_nostr_event<S>(
    mesh: &FipsMeshPubsub<S>,
    seq: u64,
    event: &Event,
) -> Result<PubsubPublishStats>
where
    S: Store + Send + Sync + 'static,
{
    publish_fips_nostr_event_with_limit(mesh, seq, event, MAX_NOSTR_PUBSUB_EVENT_BYTES).await
}

pub async fn publish_fips_nostr_event_with_limit<S>(
    mesh: &FipsMeshPubsub<S>,
    seq: u64,
    event: &Event,
    max_event_bytes: usize,
) -> Result<PubsubPublishStats>
where
    S: Store + Send + Sync + 'static,
{
    event
        .verify()
        .map_err(|err| anyhow::anyhow!("invalid nostr event signature: {err}"))?;
    let payload = event.as_json().into_bytes();
    if payload.len() > max_event_bytes {
        anyhow::bail!("nostr pubsub event exceeds {} bytes", max_event_bytes);
    }
    Ok(mesh
        .publish_pubsub(NOSTR_EVENT_PUBSUB_STREAM, seq, payload)
        .await)
}

pub async fn ingest_nostr_pubsub_payload(
    relay: &NostrRelay,
    stream_id: &str,
    payload: &[u8],
) -> Result<Option<Event>> {
    ingest_nostr_pubsub_payload_with_limit(relay, stream_id, payload, MAX_NOSTR_PUBSUB_EVENT_BYTES)
        .await
}

pub async fn ingest_nostr_pubsub_payload_with_limit(
    relay: &NostrRelay,
    stream_id: &str,
    payload: &[u8],
    max_event_bytes: usize,
) -> Result<Option<Event>> {
    if stream_id != NOSTR_EVENT_PUBSUB_STREAM {
        anyhow::bail!("unexpected nostr pubsub stream {}", stream_id);
    }
    if payload.len() > max_event_bytes {
        anyhow::bail!("nostr pubsub event exceeds {} bytes", max_event_bytes);
    }

    let json = std::str::from_utf8(payload).context("nostr pubsub payload is not utf8")?;
    let event = Event::from_json(json).context("decode nostr pubsub event")?;
    if relay.ingest_peer_event_silent(event.clone()).await? {
        Ok(Some(event))
    } else {
        Ok(None)
    }
}

pub async fn ingest_nostr_pubsub_event(
    relay: &NostrRelay,
    delivery: PubsubEvent,
) -> Result<Option<Event>> {
    ingest_nostr_pubsub_payload(relay, &delivery.stream_id, &delivery.payload).await
}

pub async fn start_nostr_pubsub_ingest<S, R, F>(
    mesh: Arc<MeshStoreCore<S, R, F>>,
    relay: Arc<NostrRelay>,
) -> tokio::task::JoinHandle<()>
where
    S: Store + Send + Sync + 'static,
    R: SignalingTransport + Send + Sync + 'static,
    F: PeerLinkFactory + Send + Sync + 'static,
{
    let _ = subscribe_nostr_events(&mesh).await;
    tokio::spawn(async move {
        loop {
            let delivery = mesh.recv_pubsub_event().await;
            if let Err(err) = ingest_nostr_pubsub_event(&relay, delivery).await {
                warn!("nostr decentralized pubsub ingest failed: {err:#}");
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use anyhow::Result;
    use hashtree_core::MemoryStore;
    use hashtree_network::{
        MeshRouter, MeshRoutingConfig, MockConnectionFactory, MockLatencyMode, MockRelay,
        MockRelayTransport, PoolConfig, PoolSettings, PubsubDeliveryMode,
    };
    use nostr::{
        Alphabet, EventBuilder, Filter, Keys, Kind, RelayMessage, SingleLetterTag, SubscriptionId,
        Tag, Timestamp,
    };
    use std::collections::HashSet;
    use std::sync::OnceLock;
    use tempfile::TempDir;
    use tokio::sync::mpsc;
    use tokio::time::{timeout, Duration};

    struct MeshNode {
        store: Arc<MeshStoreCore<MemoryStore, MockRelayTransport, MockConnectionFactory>>,
        transport: Arc<MockRelayTransport>,
    }

    fn mesh_pubsub_test_lock() -> &'static tokio::sync::Mutex<()> {
        static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
    }

    async fn make_mesh_node(relay: Arc<MockRelay>, node_id: &str) -> Result<MeshNode> {
        let node_id = node_id.to_string();
        let transport = Arc::new(relay.create_transport(node_id.clone()));
        let conn_factory = Arc::new(MockConnectionFactory::new_with_latency_mode(
            node_id.clone(),
            0,
            MockLatencyMode::YieldOnly,
        ));
        let router = Arc::new(MeshRouter::new(
            node_id,
            transport.clone(),
            conn_factory,
            PoolSettings {
                follows: PoolConfig {
                    max_connections: 0,
                    satisfied_connections: 0,
                },
                other: PoolConfig {
                    max_connections: 8,
                    satisfied_connections: 2,
                },
            },
            false,
        ));
        let store = Arc::new(MeshStoreCore::new_with_routing(
            Arc::new(MemoryStore::new()),
            router,
            Duration::from_secs(1),
            false,
            MeshRoutingConfig {
                pubsub_delivery_mode: PubsubDeliveryMode::HtlInvWant,
                ..Default::default()
            },
        ));
        transport.connect(&[]).await?;
        store.start().await?;
        Ok(MeshNode { store, transport })
    }

    async fn pump_mesh(nodes: &[&MeshNode], steps: usize) -> Result<()> {
        for _ in 0..steps {
            for node in nodes {
                while let Some(msg) = node.transport.try_recv() {
                    node.store.process_signaling(msg).await?;
                }
            }
            for node in nodes {
                let _ = node.store.drain_available_data_messages().await;
            }
            tokio::task::yield_now().await;
        }
        Ok(())
    }

    async fn broadcast_hellos(nodes: &[&MeshNode]) -> Result<()> {
        for node in nodes {
            node.store.send_hello().await?;
        }
        Ok(())
    }

    async fn recv_relay_message(
        rx: &mut mpsc::UnboundedReceiver<String>,
    ) -> Result<RelayMessage<'_>> {
        let msg = timeout(Duration::from_secs(1), rx.recv())
            .await?
            .ok_or_else(|| anyhow::anyhow!("channel closed"))?;
        Ok(RelayMessage::from_json(msg)?)
    }

    fn rating_fact_event(
        keys: &Keys,
        subject: &str,
        scope: &str,
        created_at: u64,
    ) -> Result<Event> {
        let created_at_tag = created_at.to_string();
        let rater = keys.public_key().to_hex();
        let scope_index = scope.to_lowercase();
        Ok(EventBuilder::new(Kind::Custom(7368), "")
            .tags([
                Tag::parse(["i", scope_index.as_str()])?,
                Tag::parse(["type", "rating"])?,
                Tag::parse(["schema", "1"])?,
                Tag::parse(["created_at", created_at_tag.as_str()])?,
                Tag::parse(["rater", rater.as_str()])?,
                Tag::parse(["subject", subject])?,
                Tag::parse(["rating", "90"])?,
                Tag::parse(["min_rating", "0"])?,
                Tag::parse(["max_rating", "100"])?,
                Tag::parse(["scope", scope])?,
            ])
            .custom_created_at(Timestamp::from_secs(created_at))
            .sign_with_keys(keys)?)
    }

    fn paid_exit_offer_event(keys: &Keys, offer_id: &str, created_at: u64) -> Result<Event> {
        Ok(EventBuilder::new(
            Kind::Custom(37196),
            r#"{"route":"internet-exit","price_msat":1000}"#,
        )
        .tags([
            Tag::parse(["d", offer_id])?,
            Tag::parse(["i", "fips/paid-route-offer"])?,
            Tag::parse(["app", "nostr-vpn"])?,
        ])
        .custom_created_at(Timestamp::from_secs(created_at))
        .sign_with_keys(keys)?)
    }

    fn event_has_tag(event: &Event, parts: &[&str]) -> bool {
        let expected = parts
            .iter()
            .map(|part| part.to_string())
            .collect::<Vec<_>>();
        event
            .tags
            .iter()
            .any(|tag| tag.clone().to_vec() == expected)
    }

    #[tokio::test]
    async fn decentralized_pubsub_delivers_rating_fact_to_relay_history() -> Result<()> {
        let _guard = mesh_pubsub_test_lock().lock().await;
        let mesh_relay = MockRelay::new_with_capacity(128);
        let publisher = make_mesh_node(mesh_relay.clone(), "publisher").await?;
        let subscriber = make_mesh_node(mesh_relay, "subscriber").await?;
        let nodes = [&publisher, &subscriber];
        for _ in 0..3 {
            broadcast_hellos(&nodes).await?;
            pump_mesh(&nodes, 16).await?;
        }

        subscribe_nostr_events(&subscriber.store).await;
        pump_mesh(&nodes, 64).await?;

        let tmp = TempDir::new()?;
        let ingest_graph_dir = tmp.path().join("ingest-graph");
        let replay_graph_dir = tmp.path().join("replay-graph");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir)?;
        let ingest_graph = {
            let _guard = crate::socialgraph::test_lock().await;
            crate::socialgraph::open_test_social_graph_store_with_mapsize(
                &ingest_graph_dir,
                Some(128 * 1024 * 1024),
            )?
        };
        let ingest_backend: Arc<dyn crate::socialgraph::SocialGraphBackend> = ingest_graph.clone();
        let store = Arc::new(crate::storage::HashtreeStore::with_options(
            &data_dir,
            None,
            128 * 1024 * 1024,
        )?);
        let relay = NostrRelay::new(
            ingest_backend,
            data_dir.clone(),
            HashSet::new(),
            None,
            crate::nostr_relay::NostrRelayConfig {
                spambox_db_max_bytes: 0,
                ..Default::default()
            },
        )?
        .with_historical_nostr_index(store.store_arc(), data_dir.clone());

        let keys = Keys::generate();
        let subject = Keys::generate().public_key().to_hex();
        let event = rating_fact_event(&keys, &subject, "fips.peer", 51)?;
        publish_nostr_event(&publisher.store, 1, &event).await?;
        pump_mesh(&nodes, 128).await?;

        let deliveries = subscriber.store.drain_pubsub_events().await;
        let delivery = deliveries
            .into_iter()
            .find(|delivery| delivery.stream_id == NOSTR_EVENT_PUBSUB_STREAM)
            .ok_or_else(|| anyhow::anyhow!("subscriber did not receive nostr pubsub event"))?;
        let accepted = ingest_nostr_pubsub_event(&relay, delivery).await?;
        assert_eq!(accepted.as_ref().map(|event| event.id), Some(event.id));
        assert!(data_dir.join("nostr-index/latest-root.txt").exists());

        let replay_graph = {
            let _guard = crate::socialgraph::test_lock().await;
            crate::socialgraph::open_test_social_graph_store_with_mapsize(
                &replay_graph_dir,
                Some(128 * 1024 * 1024),
            )?
        };
        let replay_backend: Arc<dyn crate::socialgraph::SocialGraphBackend> = replay_graph.clone();
        let replay = NostrRelay::new(
            replay_backend,
            data_dir.clone(),
            HashSet::new(),
            None,
            crate::nostr_relay::NostrRelayConfig {
                spambox_db_max_bytes: 0,
                ..Default::default()
            },
        )?
        .with_historical_nostr_index(store.store_arc(), data_dir);
        let (tx, mut rx) = mpsc::unbounded_channel();
        replay.register_client(1, tx, None).await;

        let sub_id = SubscriptionId::new("decentralized-ratings");
        let filter = Filter::new()
            .kind(Kind::Custom(7368))
            .custom_tag(SingleLetterTag::lowercase(Alphabet::I), "fips.peer")
            .limit(10);
        replay
            .handle_client_message(1, nostr::ClientMessage::req(sub_id.clone(), vec![filter]))
            .await;

        match recv_relay_message(&mut rx).await? {
            RelayMessage::Event {
                subscription_id,
                event: replayed,
            } => {
                assert_eq!(subscription_id.as_ref(), &sub_id);
                assert_eq!(replayed.id, event.id);
                assert!(event_has_tag(&replayed, &["scope", "fips.peer"]));
                assert!(event_has_tag(&replayed, &["created_at", "51"]));
            }
            other => anyhow::bail!("expected historical EVENT, got {:?}", other),
        }
        match recv_relay_message(&mut rx).await? {
            RelayMessage::EndOfStoredEvents(subscription_id) => {
                assert_eq!(subscription_id.as_ref(), &sub_id);
            }
            other => anyhow::bail!("expected EOSE, got {:?}", other),
        }

        Ok(())
    }

    #[tokio::test]
    async fn decentralized_pubsub_delivers_paid_exit_offer_to_relay_history() -> Result<()> {
        let _guard = mesh_pubsub_test_lock().lock().await;
        let mesh_relay = MockRelay::new_with_capacity(128);
        let publisher = make_mesh_node(mesh_relay.clone(), "publisher").await?;
        let subscriber = make_mesh_node(mesh_relay, "subscriber").await?;
        let nodes = [&publisher, &subscriber];
        for _ in 0..3 {
            broadcast_hellos(&nodes).await?;
            pump_mesh(&nodes, 16).await?;
        }

        subscribe_nostr_events(&subscriber.store).await;
        pump_mesh(&nodes, 64).await?;

        let tmp = TempDir::new()?;
        let graph_dir = tmp.path().join("graph");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir)?;
        let graph = {
            let _guard = crate::socialgraph::test_lock().await;
            crate::socialgraph::open_test_social_graph_store_with_mapsize(
                &graph_dir,
                Some(128 * 1024 * 1024),
            )?
        };
        let backend: Arc<dyn crate::socialgraph::SocialGraphBackend> = graph.clone();
        let store = Arc::new(crate::storage::HashtreeStore::with_options(
            &data_dir,
            None,
            128 * 1024 * 1024,
        )?);
        let relay = NostrRelay::new(
            backend,
            data_dir.clone(),
            HashSet::new(),
            None,
            crate::nostr_relay::NostrRelayConfig {
                spambox_db_max_bytes: 0,
                ..Default::default()
            },
        )?
        .with_historical_nostr_index(store.store_arc(), data_dir.clone());

        let keys = Keys::generate();
        let event = paid_exit_offer_event(&keys, "paid-exit-fi", 60)?;
        publish_nostr_event(&publisher.store, 1, &event).await?;
        pump_mesh(&nodes, 128).await?;

        let deliveries = subscriber.store.drain_pubsub_events().await;
        let delivery = deliveries
            .into_iter()
            .find(|delivery| delivery.stream_id == NOSTR_EVENT_PUBSUB_STREAM)
            .ok_or_else(|| anyhow::anyhow!("subscriber did not receive paid-exit offer"))?;
        let accepted = ingest_nostr_pubsub_event(&relay, delivery).await?;
        assert_eq!(accepted.as_ref().map(|event| event.id), Some(event.id));

        let filter = Filter::new().kind(Kind::Custom(37196)).limit(10);
        let indexed = relay.query_events(&filter, 10).await;
        assert_eq!(indexed.len(), 1);
        assert_eq!(indexed[0].id, event.id);
        assert!(event_has_tag(&indexed[0], &["i", "fips/paid-route-offer"]));

        Ok(())
    }

    #[tokio::test]
    async fn decentralized_pubsub_ignores_untrusted_author_without_indexing() -> Result<()> {
        let tmp = TempDir::new()?;
        let graph_dir = tmp.path().join("graph");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir)?;
        let graph_store = {
            let _guard = crate::socialgraph::test_lock().await;
            crate::socialgraph::open_test_social_graph_store_with_mapsize(
                &graph_dir,
                Some(128 * 1024 * 1024),
            )?
        };
        crate::socialgraph::set_social_graph_root(&graph_store, &[1u8; 32]);
        std::thread::sleep(std::time::Duration::from_millis(100));
        let backend: Arc<dyn crate::socialgraph::SocialGraphBackend> = graph_store.clone();
        let access = Arc::new(crate::socialgraph::SocialGraphAccessControl::new(
            Arc::clone(&backend),
            0,
            HashSet::new(),
        ));
        let store = Arc::new(crate::storage::HashtreeStore::with_options(
            &data_dir,
            None,
            128 * 1024 * 1024,
        )?);
        let relay = NostrRelay::new(
            backend,
            data_dir.clone(),
            HashSet::new(),
            Some(access),
            crate::nostr_relay::NostrRelayConfig {
                spambox_db_max_bytes: 0,
                ..Default::default()
            },
        )?
        .with_historical_nostr_index(store.store_arc(), data_dir.clone());

        let keys = Keys::generate();
        let subject = Keys::generate().public_key().to_hex();
        let event = rating_fact_event(&keys, &subject, "fips.peer", 52)?;
        let delivery = PubsubEvent {
            stream_id: NOSTR_EVENT_PUBSUB_STREAM.to_string(),
            seq: 1,
            origin_peer_id: "publisher".to_string(),
            from_peer_id: "publisher".to_string(),
            payload: event.as_json().into_bytes(),
        };

        let accepted = ingest_nostr_pubsub_event(&relay, delivery).await?;
        assert!(accepted.is_none());
        assert!(!data_dir.join("nostr-index/latest-root.txt").exists());

        let filter = Filter::new()
            .kind(Kind::Custom(7368))
            .custom_tag(SingleLetterTag::lowercase(Alphabet::I), "fips.peer")
            .limit(10);
        assert!(relay.query_events(&filter, 10).await.is_empty());

        Ok(())
    }
}
