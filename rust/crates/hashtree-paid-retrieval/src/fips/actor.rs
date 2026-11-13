//! One task that owns the service's TCP/FIPS stack: it answers inbound
//! requests through the provider and runs this node's own exchanges.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use fips_core::{FipsEndpointError, NodeError, PeerIdentity};
use fips_tcp::{ConnectionId, StackError, State};
use fips_tcp_endpoint::{AdapterError, FipsTcpEndpoint};
use tokio::sync::{mpsc, oneshot};
use tokio::task::{AbortHandle, Id as TaskId, JoinError, JoinSet};
use tokio::time::Instant;

use super::frame::{Incoming, Outgoing};
use super::{transport_error, FipsPaidError, MAX_REQUEST_BYTES};
use crate::message::MAX_MESSAGE_BYTES;
use crate::provider::PaidBlobProvider;

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

enum ClientPhase {
    WaitingToConnect { retry_at_ms: u64 },
    Connecting,
    Writing(Outgoing),
    Reading(Incoming),
}

struct Active {
    exchange: Exchange,
    connection: Option<ConnectionId>,
    /// The connection was kept from an earlier exchange and may have been
    /// closed by the provider meanwhile.
    reused: bool,
    /// A kept connection already failed this exchange: open a new one.
    fresh_only: bool,
    deadline_ms: u64,
    phase: ClientPhase,
}

/// A connection kept after an exchange for the next one with that provider.
struct Idle {
    peer: String,
    connection: ConnectionId,
    expires_ms: u64,
}

enum ServerPhase {
    Reading(Incoming),
    Handling(AbortHandle),
    Writing { reply: Vec<u8>, outgoing: Outgoing },
}

struct Server {
    /// The peer FIPS authenticated, as the provider is told.
    peer: String,
    deadline_ms: u64,
    phase: ServerPhase,
}

/// A provider's reply and the connection that asked for it.
type Handled = (ConnectionId, Vec<u8>);

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

fn transient_connect_error(error: &AdapterError) -> bool {
    matches!(
        error,
        AdapterError::Tcp(StackError::ConnectionLimit | StackError::NoEphemeralPort)
            | AdapterError::Fips(FipsEndpointError::Node(
                NodeError::PeerNotFound(_)
                    | NodeError::HandshakeIncomplete(_)
                    | NodeError::NoSession(_)
                    | NodeError::LocalRouteUnavailable(_)
            ))
    )
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

    // The selling side.

    async fn accept(&mut self, now_ms: u64) {
        while let Some(connection) = self.tcp.accept() {
            let peer = self.tcp.peer(connection).map(|peer| peer.npub());
            let seats = |peer: &String| {
                // A connection the peer already closed frees its seat, even
                // before it is taken out below.
                let taken = self.servers.iter().filter(|(connection, server)| {
                    server.peer == *peer && !self.tcp.is_read_closed(**connection)
                });
                self.servers.len() < MAX_SERVER_CONNECTIONS
                    && taken.count() < MAX_CONNECTIONS_PER_PEER
            };
            match peer.filter(|peer| self.provider.is_some() && seats(peer)) {
                Some(peer) => {
                    let server = Server {
                        peer,
                        deadline_ms: self.deadline(now_ms),
                        phase: ServerPhase::Reading(Incoming::new(MAX_REQUEST_BYTES)),
                    };
                    self.servers.insert(connection, server);
                }
                None => self.close(connection).await,
            }
        }
    }

    async fn drive_servers(&mut self, now_ms: u64) {
        let connections: Vec<_> = self.servers.keys().copied().collect();
        for connection in connections {
            let Some(mut server) = self.servers.remove(&connection) else {
                continue;
            };
            if self.drive_server(connection, &mut server, now_ms).await {
                self.servers.insert(connection, server);
            } else {
                if let ServerPhase::Handling(handler) = &server.phase {
                    handler.abort();
                }
                self.close(connection).await;
            }
        }
    }

    /// Advance one inbound connection; `false` closes it.
    async fn drive_server(
        &mut self,
        connection: ConnectionId,
        server: &mut Server,
        now_ms: u64,
    ) -> bool {
        let Some(provider) = &self.provider else {
            return false;
        };
        if now_ms >= server.deadline_ms || self.tcp.state(connection).is_none() {
            return false;
        }
        let mut progress = false;
        let open = loop {
            match &mut server.phase {
                ServerPhase::Reading(incoming) => {
                    let read = incoming.read(&mut self.tcp, connection, now_ms, &mut progress);
                    match read.await {
                        Ok(Some(request)) => {
                            let provider = provider.clone();
                            let peer = server.peer.clone();
                            let handler = self.handlers.spawn(async move {
                                let reply = provider.handle(Some(&peer), &request).await;
                                (connection, reply)
                            });
                            server.phase = ServerPhase::Handling(handler);
                            progress = true;
                        }
                        Ok(None) => break !self.tcp.is_read_closed(connection),
                        Err(_) => break false,
                    }
                }
                ServerPhase::Handling(_) => break true,
                ServerPhase::Writing { reply, outgoing } => {
                    let write =
                        outgoing.write(&mut self.tcp, connection, reply, now_ms, &mut progress);
                    match write.await {
                        // The connection stays for the peer's next request.
                        Ok(true) => {
                            server.phase = ServerPhase::Reading(Incoming::new(MAX_REQUEST_BYTES));
                        }
                        Ok(false) => break true,
                        Err(_) => break false,
                    }
                }
            }
        };
        if progress {
            server.deadline_ms = self.deadline(now_ms);
        }
        open
    }

    /// A provider answered; the answer goes out on the connection that asked.
    async fn handled(&mut self, handled: Option<Result<(TaskId, Handled), JoinError>>) {
        let handling = |server: &Server, task: TaskId| matches!(&server.phase, ServerPhase::Handling(handler) if handler.id() == task);
        let deadline_ms = self.deadline(self.now_ms());
        match handled {
            Some(Ok((task, (connection, reply)))) => {
                // The id may belong to a newer connection by now: only the
                // request this very task answered gets the reply.
                let Some(server) = self.servers.get_mut(&connection) else {
                    return;
                };
                if !handling(server, task) {
                    return;
                }
                match Outgoing::new(&reply) {
                    Ok(outgoing) => {
                        server.phase = ServerPhase::Writing { reply, outgoing };
                        server.deadline_ms = deadline_ms;
                    }
                    Err(_) => {
                        self.servers.remove(&connection);
                        self.close(connection).await;
                    }
                }
            }
            Some(Err(error)) => {
                let mut failed = self.servers.iter();
                let failed = failed.find(|(_, server)| handling(server, error.id()));
                if let Some(connection) = failed.map(|(connection, _)| *connection) {
                    self.servers.remove(&connection);
                    self.close(connection).await;
                }
            }
            None => {}
        }
    }

    // The buying side.

    async fn drive_clients(&mut self, now_ms: u64) {
        for exchange in std::mem::take(&mut self.queued) {
            let with_peer = self.active.iter();
            let with_peer = with_peer.filter(|active| active.exchange.npub == exchange.npub);
            if self.active.len() >= MAX_OUTBOUND_EXCHANGES
                || with_peer.count() >= MAX_CONNECTIONS_PER_PEER
            {
                self.queued.push_back(exchange);
                continue;
            }
            self.active.push(Active {
                exchange,
                connection: None,
                reused: false,
                fresh_only: false,
                deadline_ms: self.deadline(now_ms),
                phase: ClientPhase::WaitingToConnect {
                    retry_at_ms: now_ms,
                },
            });
        }
        for mut active in std::mem::take(&mut self.active) {
            let outcome = match active.exchange.reply.is_closed() {
                // The caller gave up; a half-done exchange cannot be reused.
                true => Some(Err(FipsPaidError::Closed)),
                false if now_ms >= active.deadline_ms => Some(Err(FipsPaidError::Timeout)),
                false => self.drive_client(&mut active, now_ms).await,
            };
            match outcome {
                None => self.active.push(active),
                Some(Ok(reply)) => {
                    self.keep(&active, now_ms).await;
                    let _ = active.exchange.reply.send(Ok(reply));
                }
                Some(Err(error)) => {
                    if let Some(connection) = active.connection {
                        self.close(connection).await;
                    }
                    let _ = active.exchange.reply.send(Err(error));
                }
            }
        }
    }

    /// Advance one exchange; `Some` is its end.
    async fn drive_client(
        &mut self,
        active: &mut Active,
        now_ms: u64,
    ) -> Option<Result<Vec<u8>, FipsPaidError>> {
        let mut progress = false;
        let outcome = loop {
            match &mut active.phase {
                ClientPhase::WaitingToConnect { retry_at_ms } => {
                    if now_ms < *retry_at_ms {
                        break None;
                    }
                    active.connection = None;
                    let kept = match active.fresh_only {
                        // What failed once is not trusted again: the other
                        // connections kept for this provider go as well.
                        true => {
                            self.close_idle(&active.exchange.npub).await;
                            None
                        }
                        false => self.take_idle(&active.exchange.npub),
                    };
                    if let Some(connection) = kept {
                        active.connection = Some(connection);
                        active.reused = true;
                        active.phase = match Outgoing::new(&active.exchange.request) {
                            Ok(outgoing) => ClientPhase::Writing(outgoing),
                            Err(error) => break Some(Err(error)),
                        };
                        continue;
                    }
                    match self.tcp.connect(active.exchange.peer, now_ms).await {
                        Ok(connection) => {
                            active.connection = Some(connection);
                            active.reused = false;
                            active.phase = ClientPhase::Connecting;
                        }
                        Err(error) if transient_connect_error(&error) => {
                            active.phase = ClientPhase::WaitingToConnect {
                                retry_at_ms: now_ms.saturating_add(CONNECT_RETRY_DELAY_MS),
                            };
                            break None;
                        }
                        Err(error) => break Some(Err(transport_error(error))),
                    }
                }
                ClientPhase::Connecting => {
                    let connection = active.connection?;
                    match self.tcp.state(connection) {
                        // Refused or reset before it opened: try again.
                        None => {
                            active.connection = None;
                            active.phase = ClientPhase::WaitingToConnect {
                                retry_at_ms: now_ms.saturating_add(CONNECT_RETRY_DELAY_MS),
                            };
                            break None;
                        }
                        Some(State::Established) => {
                            progress = true;
                            active.phase = match Outgoing::new(&active.exchange.request) {
                                Ok(outgoing) => ClientPhase::Writing(outgoing),
                                Err(error) => break Some(Err(error)),
                            };
                        }
                        Some(_) => break None,
                    }
                }
                ClientPhase::Writing(outgoing) => {
                    let connection = active.connection?;
                    let request = &active.exchange.request;
                    let write =
                        outgoing.write(&mut self.tcp, connection, request, now_ms, &mut progress);
                    match write.await {
                        Ok(true) => {
                            active.phase = ClientPhase::Reading(Incoming::new(MAX_MESSAGE_BYTES));
                        }
                        Ok(false) if self.tcp.state(connection).is_none() => {
                            break self.retry_fresh(active, now_ms, "closed").await;
                        }
                        Ok(false) => break None,
                        Err(error) => {
                            let error = error.to_string();
                            break self.retry_fresh(active, now_ms, &error).await;
                        }
                    }
                }
                ClientPhase::Reading(incoming) => {
                    let connection = active.connection?;
                    let read = incoming.read(&mut self.tcp, connection, now_ms, &mut progress);
                    let closed = match read.await {
                        Ok(Some(reply)) => break Some(Ok(reply)),
                        Ok(None) => {
                            self.tcp.is_read_closed(connection)
                                || self.tcp.state(connection).is_none()
                        }
                        // A reset connection is gone from the stack.
                        Err(FipsPaidError::Transport(_)) => true,
                        Err(error) => break Some(Err(error)),
                    };
                    if !closed {
                        break None;
                    }
                    break match incoming.is_empty() {
                        true => self.retry_fresh(active, now_ms, "closed").await,
                        false => Some(Err(FipsPaidError::Transport(
                            "the provider closed the connection inside a reply".into(),
                        ))),
                    };
                }
            }
        };
        if progress {
            active.deadline_ms = self.deadline(now_ms);
        }
        outcome
    }

    /// A kept connection may have been closed by the provider while it was
    /// unused. Requests are idempotent, so one new connection may repeat the
    /// request as long as no byte of a reply has arrived.
    async fn retry_fresh(
        &mut self,
        active: &mut Active,
        now_ms: u64,
        failure: &str,
    ) -> Option<Result<Vec<u8>, FipsPaidError>> {
        if !active.reused {
            return Some(Err(FipsPaidError::Transport(format!(
                "connection to the provider failed: {failure}"
            ))));
        }
        if let Some(connection) = active.connection.take() {
            self.close(connection).await;
        }
        active.reused = false;
        active.fresh_only = true;
        active.phase = ClientPhase::WaitingToConnect {
            retry_at_ms: now_ms,
        };
        None
    }

    async fn close_idle(&mut self, peer: &str) {
        let (closed, kept) = std::mem::take(&mut self.idle)
            .into_iter()
            .partition(|idle| idle.peer == peer);
        self.idle = kept;
        let closed: Vec<Idle> = closed;
        for idle in closed {
            self.close(idle.connection).await;
        }
    }

    fn take_idle(&mut self, peer: &str) -> Option<ConnectionId> {
        let at = self.idle.iter().position(|idle| {
            idle.peer == peer
                && self.tcp.state(idle.connection) == Some(State::Established)
                && !self.tcp.is_read_closed(idle.connection)
        })?;
        Some(self.idle.swap_remove(at).connection)
    }

    /// Keep the connection of a finished exchange for the next one.
    async fn keep(&mut self, active: &Active, now_ms: u64) {
        let Some(connection) = active.connection else {
            return;
        };
        if self.idle.len() >= MAX_IDLE_CONNECTIONS {
            return self.close(connection).await;
        }
        self.idle.push(Idle {
            peer: active.exchange.npub.clone(),
            connection,
            // Shorter than the provider's patience, so the client is the one
            // that closes an unused connection.
            expires_ms: now_ms.saturating_add(self.idle_timeout_ms / 2),
        });
    }

    async fn expire_idle(&mut self, now_ms: u64) {
        let (kept, expired) = std::mem::take(&mut self.idle)
            .into_iter()
            .partition(|idle| {
                now_ms < idle.expires_ms
                    && self.tcp.state(idle.connection) == Some(State::Established)
                    && !self.tcp.is_read_closed(idle.connection)
            });
        self.idle = kept;
        let expired: Vec<Idle> = expired;
        for idle in expired {
            self.close(idle.connection).await;
        }
    }
}
