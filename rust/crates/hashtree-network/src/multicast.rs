use anyhow::{Context, Result};
use async_trait::async_trait;
use nostr_sdk::nostr::{
    ClientMessage, Event, Filter, JsonUtil, Keys, RelayMessage, SingleLetterTag, SubscriptionId,
};
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch, Mutex};
use tokio::time::Sleep;
use tracing::{debug, warn};

use crate::local_bus::LocalNostrBus;
use crate::relay_bridge::SharedMeshEventStore;
use crate::root_events::{
    build_root_filter, hashtree_root_kinds, is_hashtree_labeled_event, is_hashtree_root_kind,
    pick_latest_event, root_event_from_peer, PeerRootEvent, HASHTREE_LABEL,
};

#[derive(Debug, Clone)]
pub struct MulticastConfig {
    pub enabled: bool,
    pub group: String,
    pub port: u16,
    pub max_peers: usize,
    pub announce_interval_ms: u64,
}

#[async_trait]
impl LocalNostrBus for MulticastNostrBus {
    fn source_name(&self) -> &'static str {
        "multicast"
    }

    async fn broadcast_event(&self, event: &Event) -> Result<()> {
        MulticastNostrBus::broadcast_event(self, event).await
    }

    async fn query_root(
        &self,
        owner_pubkey: &str,
        tree_name: &str,
        timeout: Duration,
    ) -> Option<PeerRootEvent> {
        MulticastNostrBus::query_root(self, owner_pubkey, tree_name, timeout).await
    }
}

impl MulticastConfig {
    pub fn is_enabled(&self) -> bool {
        self.enabled && self.max_peers > 0
    }
}

impl Default for MulticastConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            group: "239.255.42.98".to_string(),
            port: 48555,
            max_peers: 0,
            announce_interval_ms: 2_000,
        }
    }
}

pub struct MulticastNostrBus {
    config: MulticastConfig,
    keys: Keys,
    relay: SharedMeshEventStore,
    socket: Arc<UdpSocket>,
    target_addr: SocketAddr,
    pending_queries: Arc<Mutex<HashMap<String, mpsc::UnboundedSender<RelayMessage<'static>>>>>,
    announced_event_ids: Arc<Mutex<HashSet<String>>>,
}

const QUERY_SETTLE_GRACE_MS: u64 = 150;

impl MulticastNostrBus {
    pub async fn bind(
        config: MulticastConfig,
        keys: Keys,
        relay: SharedMeshEventStore,
    ) -> Result<Arc<Self>> {
        let group: Ipv4Addr = config
            .group
            .parse()
            .with_context(|| format!("invalid multicast group {}", config.group))?;
        let std_socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        std_socket.set_reuse_address(true)?;
        std_socket.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, config.port).into())?;
        std_socket.set_multicast_loop_v4(true)?;
        std_socket.join_multicast_v4(&group, &Ipv4Addr::UNSPECIFIED)?;
        std_socket.set_nonblocking(true)?;

        let socket = UdpSocket::from_std(std_socket.into())?;
        let target_addr = SocketAddr::V4(SocketAddrV4::new(group, config.port));

        Ok(Arc::new(Self {
            config,
            keys,
            relay,
            socket: Arc::new(socket),
            target_addr,
            pending_queries: Arc::new(Mutex::new(HashMap::new())),
            announced_event_ids: Arc::new(Mutex::new(HashSet::new())),
        }))
    }

    pub async fn run(
        self: Arc<Self>,
        mut shutdown_rx: watch::Receiver<bool>,
        signaling_tx: mpsc::Sender<(String, Event)>,
    ) -> Result<()> {
        let mut announce_ticker = tokio::time::interval(Duration::from_millis(
            self.config.announce_interval_ms.max(1),
        ));
        let mut buf = vec![0u8; 64 * 1024];

        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                }
                _ = announce_ticker.tick() => {
                    if let Err(err) = self.broadcast_known_root_updates().await {
                        debug!("multicast root announcement failed: {}", err);
                    }
                }
                recv = self.socket.recv_from(&mut buf) => {
                    let (len, _src) = match recv {
                        Ok(value) => value,
                        Err(err) => {
                            warn!("multicast receive failed: {}", err);
                            continue;
                        }
                    };
                    let text = match std::str::from_utf8(&buf[..len]) {
                        Ok(text) => text,
                        Err(err) => {
                            debug!("ignoring non-utf8 multicast datagram: {}", err);
                            continue;
                        }
                    };
                    self.handle_datagram(text, &signaling_tx).await;
                }
            }
        }

        Ok(())
    }

    pub async fn broadcast_event(&self, event: &Event) -> Result<()> {
        let payload = event.as_json();
        let copies = if event.kind.is_ephemeral() { 3 } else { 1 };
        for _ in 0..copies {
            self.socket
                .send_to(payload.as_bytes(), self.target_addr)
                .await?;
        }
        Ok(())
    }

    pub async fn query_root(
        &self,
        owner_pubkey: &str,
        tree_name: &str,
        timeout: Duration,
    ) -> Option<PeerRootEvent> {
        let filter = build_root_filter(owner_pubkey, tree_name)?;
        let subscription_id = format!("multicast-root-{}", rand::random::<u64>());
        let request =
            ClientMessage::req(SubscriptionId::new(subscription_id.clone()), vec![filter]);
        let (tx, mut rx) = mpsc::unbounded_channel();
        self.pending_queries
            .lock()
            .await
            .insert(subscription_id.clone(), tx);

        if self
            .socket
            .send_to(request.as_json().as_bytes(), self.target_addr)
            .await
            .is_err()
        {
            self.pending_queries.lock().await.remove(&subscription_id);
            return None;
        }

        let mut events = Vec::new();
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);
        let mut settle_deadline: Option<std::pin::Pin<Box<Sleep>>> = None;

        loop {
            tokio::select! {
                _ = &mut deadline => break,
                _ = async {
                    if let Some(deadline) = &mut settle_deadline {
                        deadline.as_mut().await;
                    }
                }, if settle_deadline.is_some() => break,
                maybe_msg = rx.recv() => {
                    let Some(msg) = maybe_msg else {
                        break;
                    };
                    match msg {
                        RelayMessage::Event { subscription_id: sid, event }
                            if sid.to_string() == subscription_id =>
                        {
                            events.push(event.into_owned());
                            settle_deadline = Some(Box::pin(tokio::time::sleep(Duration::from_millis(
                                QUERY_SETTLE_GRACE_MS,
                            ))));
                        }
                        RelayMessage::EndOfStoredEvents(sid)
                            if sid.to_string() == subscription_id
                                && !events.is_empty()
                                && settle_deadline.is_none() =>
                        {
                            settle_deadline = Some(Box::pin(tokio::time::sleep(Duration::from_millis(
                                QUERY_SETTLE_GRACE_MS,
                            ))));
                        }
                        _ => {}
                    }
                }
            }
        }

        self.pending_queries.lock().await.remove(&subscription_id);

        let latest = pick_latest_event(events.iter())?;
        root_event_from_peer(latest, self.source_name(), tree_name)
    }

    async fn handle_datagram(&self, text: &str, signaling_tx: &mpsc::Sender<(String, Event)>) {
        if let Ok(event) = Event::from_json(text) {
            if event.pubkey == self.keys.public_key() {
                return;
            }

            if event.kind.is_ephemeral() {
                let _ = signaling_tx.send(("multicast".to_string(), event)).await;
                return;
            }

            if is_hashtree_root_kind(event.kind)
                && is_hashtree_labeled_event(&event)
                && event.verify().is_ok()
            {
                let _ = self.relay.ingest_trusted_event(event).await;
            }
            return;
        }

        if let Ok(msg) = ClientMessage::from_json(text) {
            if let ClientMessage::Req {
                subscription_id,
                filters,
            } = msg
            {
                for filter in filters {
                    let limit = filter.limit.unwrap_or(50).min(50);
                    for event in self.relay.query_events(&filter, limit).await {
                        let relay_msg =
                            RelayMessage::event(subscription_id.clone().into_owned(), event);
                        let _ = self
                            .socket
                            .send_to(relay_msg.as_json().as_bytes(), self.target_addr)
                            .await;
                    }
                }
                let eose = RelayMessage::eose(subscription_id.into_owned());
                let _ = self
                    .socket
                    .send_to(eose.as_json().as_bytes(), self.target_addr)
                    .await;
            }
            return;
        }

        if let Ok(msg) = RelayMessage::from_json(text) {
            match &msg {
                RelayMessage::Event {
                    subscription_id,
                    event,
                } => {
                    if is_hashtree_root_kind(event.kind)
                        && is_hashtree_labeled_event(event)
                        && event.verify().is_ok()
                    {
                        let _ = self.relay.ingest_trusted_event((**event).clone()).await;
                    }
                    let tx = self
                        .pending_queries
                        .lock()
                        .await
                        .get(&subscription_id.to_string())
                        .cloned();
                    if let Some(tx) = tx {
                        let _ = tx.send(RelayMessage::event(
                            subscription_id.clone().into_owned(),
                            event.clone().into_owned(),
                        ));
                    }
                }
                RelayMessage::EndOfStoredEvents(subscription_id) => {
                    let tx = self
                        .pending_queries
                        .lock()
                        .await
                        .get(&subscription_id.to_string())
                        .cloned();
                    if let Some(tx) = tx {
                        let _ = tx.send(RelayMessage::eose(subscription_id.clone().into_owned()));
                    }
                }
                _ => {}
            }
        }
    }

    async fn broadcast_known_root_updates(&self) -> Result<()> {
        let filter = Filter::new()
            .kinds(hashtree_root_kinds())
            .author(self.keys.public_key())
            .custom_tag(
                SingleLetterTag::lowercase(nostr_sdk::nostr::Alphabet::L),
                HASHTREE_LABEL.to_string(),
            )
            .limit(256);
        let events = self.relay.query_events(&filter, 256).await;
        let mut announced = self.announced_event_ids.lock().await;
        for event in events {
            let event_id = event.id.to_hex();
            if announced.insert(event_id) {
                self.broadcast_event(&event).await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay_bridge::MeshEventStore;
    use crate::root_events::HASHTREE_KIND;
    use anyhow::Result;
    use nostr_sdk::nostr::{Alphabet, EventBuilder, Kind, Tag, TagKind};
    use std::time::{SystemTime, UNIX_EPOCH};

    const HASHTREE_LABEL: &str = "hashtree";

    #[derive(Default)]
    struct TestEventStore {
        events: Mutex<Vec<Event>>,
    }

    #[async_trait]
    impl MeshEventStore for TestEventStore {
        async fn ingest_trusted_event(&self, event: Event) -> Result<()> {
            self.events.lock().await.push(event);
            Ok(())
        }

        async fn query_events(&self, filter: &Filter, limit: usize) -> Vec<Event> {
            self.events
                .lock()
                .await
                .iter()
                .filter(|event| filter.match_event(event, Default::default()))
                .take(limit)
                .cloned()
                .collect()
        }
    }

    fn unique_multicast_port() -> u16 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        40000 + (nanos % 2000) as u16
    }

    fn build_root_event(keys: &Keys, tree_name: &str, hash_hex: &str) -> Event {
        EventBuilder::new(Kind::Custom(HASHTREE_KIND), "")
            .tags([
                Tag::identifier(tree_name.to_string()),
                Tag::custom(
                    TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::L)),
                    vec![HASHTREE_LABEL.to_string()],
                ),
                Tag::custom(TagKind::Custom("hash".into()), vec![hash_hex.to_string()]),
            ])
            .sign_with_keys(keys)
            .expect("root event")
    }

    #[tokio::test]
    async fn query_root_ignores_early_eose_until_grace_period_expires() -> Result<()> {
        let keys = Keys::generate();
        let owner_keys = Keys::generate();
        let relay = Arc::new(TestEventStore::default()) as SharedMeshEventStore;
        let bus = MulticastNostrBus::bind(
            MulticastConfig {
                enabled: true,
                group: "239.255.43.10".to_string(),
                port: unique_multicast_port(),
                max_peers: 4,
                announce_interval_ms: 60_000,
            },
            keys,
            relay,
        )
        .await?;

        let tree_name = "eose-race";
        let hash_hex = "ef".repeat(32);
        let event = build_root_event(&owner_keys, tree_name, &hash_hex);

        let query_bus = Arc::clone(&bus);
        let query = tokio::spawn(async move {
            query_bus
                .query_root(
                    &owner_keys.public_key().to_hex(),
                    tree_name,
                    Duration::from_millis(500),
                )
                .await
        });

        let subscription_id = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some(subscription_id) =
                    bus.pending_queries.lock().await.keys().next().cloned()
                {
                    break subscription_id;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("query registered pending subscription");

        let (signal_tx, _signal_rx) = mpsc::channel(1);
        bus.handle_datagram(
            &RelayMessage::eose(SubscriptionId::new(subscription_id.clone())).as_json(),
            &signal_tx,
        )
        .await;
        bus.handle_datagram(
            &RelayMessage::event(SubscriptionId::new(subscription_id), event.clone()).as_json(),
            &signal_tx,
        )
        .await;

        let resolved = query.await.expect("query task completed");
        let resolved = resolved.expect("query returned root event after early eose");
        assert_eq!(resolved.hash, hash_hex);
        assert_eq!(resolved.event_id, event.id.to_hex());
        Ok(())
    }
}
