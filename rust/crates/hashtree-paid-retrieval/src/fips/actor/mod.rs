//! One task that owns the service's TCP/FIPS stack: it answers inbound
//! requests through the provider and runs this node's own exchanges.

mod buying;
mod selling;

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use fips_core::PeerIdentity;
use fips_tcp::ConnectionId;
use fips_tcp_endpoint::FipsTcpEndpoint;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tokio::time::Instant;

use super::{transport_error, FipsPaidError};
use crate::provider::PaidBlobProvider;
use buying::{Active, Idle};
use selling::{Handled, Server};

const MAX_QUEUED_EXCHANGES: usize = 16;
const MAX_OUTBOUND_EXCHANGES: usize = 8;
const MAX_IDLE_CONNECTIONS: usize = 8;
const MAX_SERVER_CONNECTIONS: usize = 16;
/// One peer cannot hold every seat of a provider, and a client never asks
/// one provider for more seats than this.
const MAX_CONNECTIONS_PER_PEER: usize = 4;
/// Room for all of the above and for connections that are closing.
pub(super) const MAX_TCP_CONNECTIONS: usize = 48;
const ACTIVE_POLL_INTERVAL: Duration = Duration::from_millis(10);
const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(250);
const CONNECT_RETRY_DELAY_MS: u64 = 50;

pub(super) struct Exchange {
    pub(super) peer: PeerIdentity,
    /// `peer` as text, for comparing.
    pub(super) npub: String,
    pub(super) request: Vec<u8>,
    pub(super) reply: oneshot::Sender<Result<Vec<u8>, FipsPaidError>>,
}

pub(super) enum Command {
    Exchange(Box<Exchange>),
    Shutdown { reply: oneshot::Sender<()> },
}
pub(super) struct Actor {
    tcp: FipsTcpEndpoint,
    provider: Option<PaidBlobProvider>,
    commands: mpsc::Receiver<Command>,
    queued: VecDeque<Exchange>,
    active: Vec<Active>,
    idle: Vec<Idle>,
    servers: HashMap<ConnectionId, Server>,
    handlers: JoinSet<Handled>,
    started: Instant,
    idle_timeout_ms: u64,
}

impl Actor {
    pub(super) fn new(
        tcp: FipsTcpEndpoint,
        provider: Option<PaidBlobProvider>,
        commands: mpsc::Receiver<Command>,
        idle_timeout: Duration,
    ) -> Self {
        Self {
            tcp,
            provider,
            commands,
            queued: VecDeque::new(),
            active: Vec::new(),
            idle: Vec::new(),
            servers: HashMap::new(),
            handlers: JoinSet::new(),
            started: Instant::now(),
            idle_timeout_ms: idle_timeout.as_millis().clamp(1, u64::MAX as u128) as u64,
        }
    }

    pub(super) async fn run(mut self) -> Result<(), FipsPaidError> {
        loop {
            let busy =
                !(self.queued.is_empty() && self.active.is_empty() && self.servers.is_empty());
            let poll_interval = match busy {
                true => ACTIVE_POLL_INTERVAL,
                false => IDLE_POLL_INTERVAL,
            };
            let mut input = false;
            tokio::select! {
                ready = self.tcp.wait_for_input() => {
                    ready.map_err(transport_error)?;
                    input = true;
                }
                command = self.commands.recv(), if self.queued.len() < MAX_QUEUED_EXCHANGES => {
                    match command {
                        None => return Ok(()),
                        Some(Command::Exchange(exchange)) => self.queued.push_back(*exchange),
                        Some(Command::Shutdown { reply }) => {
                            self.close_all().await;
                            let _ = reply.send(());
                            return Ok(());
                        }
                    }
                }
                handled = self.handlers.join_next_with_id(), if !self.handlers.is_empty() => {
                    self.handled(handled).await;
                }
                _ = tokio::time::sleep(poll_interval) => {}
            }
            let now_ms = self.now_ms();
            if input {
                // The readiness wait above is safe to abandon; processing
                // the batch is not, so it runs outside the selection.
                self.tcp.receive(now_ms).await.map_err(transport_error)?;
            }
            self.tcp.poll(now_ms).await.map_err(transport_error)?;
            self.accept(now_ms).await;
            self.drive_servers(now_ms).await;
            self.drive_clients(now_ms).await;
            self.expire_idle(now_ms).await;
        }
    }

    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis().min(u64::MAX as u128) as u64
    }

    fn deadline(&self, now_ms: u64) -> u64 {
        now_ms.saturating_add(self.idle_timeout_ms)
    }

    async fn close(&mut self, connection: ConnectionId) {
        let now_ms = self.now_ms();
        let _ = self.tcp.close(connection, now_ms).await;
    }

    async fn close_all(&mut self) {
        // A redemption a handler started still runs to its end: the seller
        // gave it a task of its own.
        self.handlers.abort_all();
        let servers = self.servers.drain().map(|(connection, _)| connection);
        let active = self.active.drain(..).filter_map(|active| active.connection);
        let idle = self.idle.drain(..).map(|idle| idle.connection);
        let connections: Vec<_> = servers.chain(active).chain(idle).collect();
        for connection in connections {
            self.close(connection).await;
        }
        self.queued.clear();
    }
}
