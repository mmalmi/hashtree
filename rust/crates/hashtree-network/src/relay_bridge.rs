use anyhow::Result;
use async_trait::async_trait;
use nostr_sdk::nostr::{ClientMessage, Event, Filter, SubscriptionId};
use std::sync::Arc;
use tokio::sync::mpsc;

#[async_trait]
pub trait MeshEventStore: Send + Sync {
    async fn ingest_trusted_event(&self, event: Event) -> Result<()>;

    async fn query_events(&self, filter: &Filter, limit: usize) -> Vec<Event>;
}

pub type SharedMeshEventStore = Arc<dyn MeshEventStore>;

#[async_trait]
pub trait MeshRelayClient: MeshEventStore {
    fn next_client_id(&self) -> u64;

    async fn register_client(
        &self,
        client_id: u64,
        sender: mpsc::UnboundedSender<String>,
        pubkey: Option<String>,
    );

    async fn unregister_client(&self, client_id: u64);

    async fn handle_client_message(&self, client_id: u64, msg: ClientMessage<'static>);

    async fn register_subscription_query(
        &self,
        client_id: u64,
        subscription_id: SubscriptionId,
        filters: Vec<Filter>,
    ) -> std::result::Result<Vec<Event>, &'static str>;

    async fn ingest_trusted_event_from_peer(
        &self,
        event: Event,
        _peer_id: Option<String>,
    ) -> Result<()> {
        self.ingest_trusted_event(event).await
    }
}

pub type SharedMeshRelayClient = Arc<dyn MeshRelayClient>;
