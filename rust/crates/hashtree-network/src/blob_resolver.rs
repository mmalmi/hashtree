//! Route-only composition of the shared mesh resolver.
//!
//! This keeps peer discovery and reliable transport outside Hashtree routing:
//! callers register labeled [`BlobRoute`] adapters, while `MeshStoreCore` owns
//! verification, HTL, selection, deadlines, caching, and result semantics.

use std::future::pending;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use hashtree_core::Store;

use crate::{
    MeshRouter, MeshRoutingConfig, MeshStoreCore, PeerLink, PeerLinkFactory, PoolSettings,
    SignalingMessage, SignalingTransport, TransportError,
};

pub type BlobResolver<S> = MeshStoreCore<S, RouteOnlySignaling, RouteOnlyPeerLinks>;

pub fn blob_resolver<S>(
    local_store: Arc<S>,
    peer_id: impl Into<String>,
    request_timeout: Duration,
    routing: MeshRoutingConfig,
) -> BlobResolver<S>
where
    S: Store + Send + Sync + 'static,
{
    let peer_id = peer_id.into();
    let signaling = Arc::new(RouteOnlySignaling {
        peer_id: peer_id.clone(),
    });
    let router = Arc::new(MeshRouter::new(
        peer_id,
        signaling,
        Arc::new(RouteOnlyPeerLinks),
        PoolSettings::default(),
        false,
    ));
    MeshStoreCore::new_with_routing(local_store, router, request_timeout, false, routing)
}

pub struct RouteOnlySignaling {
    peer_id: String,
}

#[async_trait]
impl SignalingTransport for RouteOnlySignaling {
    async fn connect(&self, _relays: &[String]) -> Result<(), TransportError> {
        Ok(())
    }

    async fn disconnect(&self) {}

    async fn publish(&self, _msg: SignalingMessage) -> Result<(), TransportError> {
        Err(TransportError::NotConnected)
    }

    async fn recv(&self) -> Option<SignalingMessage> {
        pending().await
    }

    fn try_recv(&self) -> Option<SignalingMessage> {
        None
    }

    fn peer_id(&self) -> &str {
        &self.peer_id
    }
}

pub struct RouteOnlyPeerLinks;

#[async_trait]
impl PeerLinkFactory for RouteOnlyPeerLinks {
    async fn create_offer(
        &self,
        _target_peer_id: &str,
    ) -> Result<(Arc<dyn PeerLink>, String), TransportError> {
        Err(TransportError::NotConnected)
    }

    async fn accept_offer(
        &self,
        _from_peer_id: &str,
        _offer_sdp: &str,
    ) -> Result<(Arc<dyn PeerLink>, String), TransportError> {
        Err(TransportError::NotConnected)
    }

    async fn handle_answer(
        &self,
        _target_peer_id: &str,
        _answer_sdp: &str,
    ) -> Result<Arc<dyn PeerLink>, TransportError> {
        Err(TransportError::NotConnected)
    }
}
