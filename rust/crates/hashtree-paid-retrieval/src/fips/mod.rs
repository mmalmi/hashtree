//! The paid exchange as its own TCP/FIPS service.
//!
//! Free blobs keep their service (`hashtree.blob/1` in
//! `hashtree-fips-transport`); this one is bound beside it on its own port
//! and carries the messages of [`crate::message`] in the framing of
//! [`crate::transport`]. FIPS authenticates the peer, and the provider is
//! told who asks, so grace can be bound to a peer.

mod actor;
mod frame;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use fips_core::discovery::local::{rank_capability_providers, LocalInstanceCapability};
use fips_core::{FipsEndpoint, PeerIdentity};
use fips_tcp::Config as TcpConfig;
use fips_tcp_endpoint::FipsTcpEndpoint;
use hashtree_core::StoreError;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::message::{MAX_HEADER_BYTES, PROTOCOL};
use crate::provider::PaidBlobProvider;
use crate::transport::PaidBlobTransport;
use actor::{Actor, Command, Exchange};

/// Discovery capability of the paid blob service.
pub const PAID_BLOB_CAPABILITY: &str = PROTOCOL;
pub const PAID_BLOB_SERVICE_PORT: u16 = 39_020;

/// A request is a header only; a reply may carry a whole blob.
const MAX_REQUEST_BYTES: usize = MAX_HEADER_BYTES;
const COMMAND_CHANNEL_CAPACITY: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FipsPaidConfig {
    /// The longest a connection may go without progress: no byte read or
    /// written, no reply from the provider. A client keeps an unused
    /// connection for half of it.
    pub idle_timeout: Duration,
}

impl Default for FipsPaidConfig {
    /// Long enough for a provider to redeem a token at its mint.
    fn default() -> Self {
        Self {
            idle_timeout: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FipsPaidError {
    #[error("paid blob service over FIPS is closed")]
    Closed,
    #[error("TCP/FIPS transport failed: {0}")]
    Transport(String),
    #[error("invalid paid blob exchange: {0}")]
    Protocol(&'static str),
    #[error("paid blob exchange timed out")]
    Timeout,
    #[error("invalid paid blob service config: {0}")]
    InvalidConfig(&'static str),
}

fn transport_error(error: impl std::fmt::Display) -> FipsPaidError {
    FipsPaidError::Transport(error.to_string())
}

/// The paid blob service of one FIPS endpoint: sells a provider's blobs to
/// authenticated peers, and carries this node's own exchanges with others.
pub struct FipsPaidBlobService {
    commands: mpsc::Sender<Command>,
    task: Option<JoinHandle<Result<(), FipsPaidError>>>,
}

impl FipsPaidBlobService {
    /// Sell to every authenticated peer that knows this node.
    pub async fn serve(
        endpoint: Arc<FipsEndpoint>,
        provider: PaidBlobProvider,
        config: FipsPaidConfig,
    ) -> Result<Self, FipsPaidError> {
        Self::bind(endpoint, Some(provider), config, None).await
    }

    /// [`Self::serve`], and advertise [`PAID_BLOB_CAPABILITY`] to peers on
    /// this host for as long as the service is bound.
    pub async fn serve_advertised(
        endpoint: Arc<FipsEndpoint>,
        provider: PaidBlobProvider,
        config: FipsPaidConfig,
        priority: i16,
    ) -> Result<Self, FipsPaidError> {
        Self::bind(endpoint, Some(provider), config, Some(priority)).await
    }

    /// Buy only: every inbound connection is closed.
    pub async fn client(
        endpoint: Arc<FipsEndpoint>,
        config: FipsPaidConfig,
    ) -> Result<Self, FipsPaidError> {
        Self::bind(endpoint, None, config, None).await
    }

    async fn bind(
        endpoint: Arc<FipsEndpoint>,
        provider: Option<PaidBlobProvider>,
        config: FipsPaidConfig,
        advertise_priority: Option<i16>,
    ) -> Result<Self, FipsPaidError> {
        if config.idle_timeout.is_zero() {
            return Err(FipsPaidError::InvalidConfig(
                "idle timeout must be non-zero",
            ));
        }
        let tcp_config = TcpConfig {
            max_connections: actor::MAX_TCP_CONNECTIONS,
            time_wait_ms: 1_000,
            ..TcpConfig::default()
        };
        let isn_seed = rand::random();
        let tcp = match advertise_priority {
            Some(priority) => {
                let capability =
                    LocalInstanceCapability::service(PAID_BLOB_CAPABILITY, PAID_BLOB_SERVICE_PORT)
                        .with_priority(priority);
                FipsTcpEndpoint::bind_with_capability(endpoint, capability, tcp_config, isn_seed)
                    .await
                    .map_err(transport_error)?
            }
            None => FipsTcpEndpoint::bind(endpoint, PAID_BLOB_SERVICE_PORT, tcp_config, isn_seed)
                .await
                .map_err(transport_error)?,
        };
        let (commands, receiver) = mpsc::channel(COMMAND_CHANNEL_CAPACITY);
        let actor = Actor::new(tcp, provider, receiver, config.idle_timeout);
        Ok(Self {
            commands,
            task: Some(tokio::spawn(actor.run())),
        })
    }

    /// The carrier of a [`crate::PaidBlobRoute`] to one provider.
    pub fn transport_to(&self, peer: PeerIdentity) -> FipsPaidTransport {
        FipsPaidTransport {
            commands: self.commands.clone(),
            peer,
            npub: peer.npub().into(),
        }
    }

    /// Close every connection and release the service port.
    pub async fn shutdown(mut self) -> Result<(), FipsPaidError> {
        let (reply, stopped) = oneshot::channel();
        if self
            .commands
            .send(Command::Shutdown { reply })
            .await
            .is_ok()
        {
            let _ = stopped.await;
        }
        match self.task.take() {
            Some(task) => task.await.map_err(transport_error)?,
            None => Ok(()),
        }
    }
}

impl Drop for FipsPaidBlobService {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// One request and one reply with one provider. It keeps no service alive:
/// after the service is gone every exchange fails.
#[derive(Clone)]
pub struct FipsPaidTransport {
    commands: mpsc::Sender<Command>,
    peer: PeerIdentity,
    npub: Arc<str>,
}

impl FipsPaidTransport {
    pub async fn request(&self, request: Vec<u8>) -> Result<Vec<u8>, FipsPaidError> {
        if request.len() > MAX_REQUEST_BYTES {
            return Err(FipsPaidError::Protocol("request is too large"));
        }
        let (reply, result) = oneshot::channel();
        let exchange = Exchange {
            peer: self.peer,
            npub: self.npub.to_string(),
            request,
            reply,
        };
        self.commands
            .send(Command::Exchange(Box::new(exchange)))
            .await
            .map_err(|_| FipsPaidError::Closed)?;
        result.await.map_err(|_| FipsPaidError::Closed)?
    }
}

#[async_trait]
impl PaidBlobTransport for FipsPaidTransport {
    async fn exchange(&self, request: Vec<u8>) -> Result<Vec<u8>, StoreError> {
        self.request(request)
            .await
            .map_err(|error| StoreError::Other(error.to_string()))
    }
}

/// Peers on this host that advertise the paid blob service, best first.
/// Providers elsewhere are named by the application.
pub fn discovered_paid_providers(
    endpoint: &FipsEndpoint,
) -> Result<Vec<PeerIdentity>, FipsPaidError> {
    let adverts = endpoint
        .local_instance_advertisements()
        .map_err(transport_error)?;
    let own = endpoint.npub();
    let port = |advert: &&fips_core::discovery::local::LocalInstanceAdvertisement| {
        let capability = advert.capability(PAID_BLOB_CAPABILITY);
        capability.and_then(|capability| capability.fsp_port) == Some(PAID_BLOB_SERVICE_PORT)
    };
    Ok(rank_capability_providers(&adverts, PAID_BLOB_CAPABILITY)
        .into_iter()
        .filter(|advert| advert.npub != own)
        .filter(port)
        .filter_map(|advert| PeerIdentity::from_npub(&advert.npub).ok())
        .collect())
}
