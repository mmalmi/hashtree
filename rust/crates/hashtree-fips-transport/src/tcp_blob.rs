use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use fips_core::discovery::local::LocalInstanceCapability;
use fips_core::{FipsEndpoint, PeerIdentity};
use fips_tcp::{Config as TcpConfig, ConnectionId, State};
use fips_tcp_endpoint::FipsTcpEndpoint;
use hashtree_core::{Hash, MemoryStore, Store, StoreError};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tokio::task::{AbortHandle, Id as TaskId, JoinError, JoinHandle, JoinSet};
use tokio::time::{Instant, MissedTickBehavior};

/// Reserved discovery capability for this protocol.
///
/// Advertised binds couple this capability to the live FSP registration. That
/// coupling requires the corrected versioned FIPS/fips-tcp release.
pub const TCP_BLOB_CAPABILITY: &str = "hashtree.blob/1";
pub const TCP_BLOB_SERVICE_PORT: u16 = 39_018;
pub const TCP_BLOB_MAX_BYTES: usize = 16 * 1024 * 1024;

const REQUEST_BYTES: usize = 35;
const RESPONSE_HEADER_BYTES: usize = 7;
const MAGIC: u8 = 0x48;
const VERSION: u8 = 1;
const GET: u8 = 1;
const FOUND: u8 = 1;
const IO_CHUNK_BYTES: usize = 64 * 1024;
const MAX_QUEUED_COMMANDS: usize = 16;
const COMMAND_CHANNEL_CAPACITY: usize = MAX_QUEUED_COMMANDS / 2;
const PENDING_COMMAND_CAPACITY: usize = MAX_QUEUED_COMMANDS - COMMAND_CHANNEL_CAPACITY;
const MAX_TCP_CONNECTIONS: usize = 32;
const MAX_SERVER_CONNECTIONS: usize = 8;
pub(crate) const MAX_OUTBOUND_GETS: usize = 4;
const MAX_STORE_LOADS: usize = 4;
const MAX_SESSION_ATTEMPTS: u8 = 2;
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_millis(5_500);

static NEXT_ISN_SEED: AtomicU64 = AtomicU64::new(0x4854_5245_4554_4350);

/// Hash-verified Hashtree blobs carried by one bounded TCP/FIPS actor.
///
/// Each [`Self::get`] targets one explicit peer and caches verified remote
/// bytes in the supplied store. Ordinary binds preserve the general transport's
/// any-authenticated-peer behavior; same-host wrappers choose stricter inbound
/// policies when they bind it.
pub struct TcpBlobTransport<S: Store + ?Sized + 'static = MemoryStore> {
    store: Arc<S>,
    commands: mpsc::Sender<Command>,
    task: Option<JoinHandle<Result<(), TcpBlobTransportError>>>,
}

/// Runtime limits for a TCP/FIPS blob service.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TcpBlobTransportConfig {
    /// Maximum time without application-level connection progress.
    /// Positive reads, writes, establishment, and store completion refresh it.
    pub idle_timeout: Duration,
}

impl Default for TcpBlobTransportConfig {
    fn default() -> Self {
        Self {
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
        }
    }
}

#[derive(Debug, Error)]
pub enum TcpBlobTransportError {
    #[error("TCP/FIPS blob transport is closed")]
    Closed,
    #[error("TCP/FIPS transport failed: {0}")]
    Transport(String),
    #[error("invalid TCP/FIPS blob protocol: {0}")]
    Protocol(&'static str),
    #[error("TCP/FIPS blob response size {0} exceeds the 16 MiB limit")]
    BlobTooLarge(usize),
    #[error("TCP/FIPS blob hash mismatch")]
    HashMismatch,
    #[error("TCP/FIPS blob request timed out")]
    Timeout,
    #[error("invalid TCP/FIPS blob transport config: {0}")]
    InvalidConfig(&'static str),
    #[error("store failed: {0}")]
    Store(#[from] StoreError),
    #[error("TCP/FIPS blob actor failed: {0}")]
    Task(#[from] JoinError),
}

impl TcpBlobTransport<MemoryStore> {
    pub async fn in_memory(endpoint: Arc<FipsEndpoint>) -> Result<Self, TcpBlobTransportError> {
        Self::bind(endpoint, Arc::new(MemoryStore::new())).await
    }
}

impl<S: Store + ?Sized + 'static> TcpBlobTransport<S> {
    pub async fn bind(
        endpoint: Arc<FipsEndpoint>,
        store: Arc<S>,
    ) -> Result<Self, TcpBlobTransportError> {
        Self::bind_with_config(endpoint, store, TcpBlobTransportConfig::default()).await
    }

    /// Bind the blob service with an explicit idle timeout.
    pub async fn bind_with_config(
        endpoint: Arc<FipsEndpoint>,
        store: Arc<S>,
        transport_config: TcpBlobTransportConfig,
    ) -> Result<Self, TcpBlobTransportError> {
        Self::bind_internal(endpoint, store, transport_config, None, true).await
    }

    /// Bind a client-only transport that rejects every inbound TCP session.
    pub async fn bind_client_with_config(
        endpoint: Arc<FipsEndpoint>,
        store: Arc<S>,
        transport_config: TcpBlobTransportConfig,
    ) -> Result<Self, TcpBlobTransportError> {
        Self::bind_internal(endpoint, store, transport_config, None, false).await
    }

    /// Bind and advertise this store as a reusable same-host blob service.
    pub async fn bind_advertised_with_config(
        endpoint: Arc<FipsEndpoint>,
        store: Arc<S>,
        transport_config: TcpBlobTransportConfig,
        priority: i16,
    ) -> Result<Self, TcpBlobTransportError> {
        Self::bind_internal(endpoint, store, transport_config, Some(priority), true).await
    }

    async fn bind_internal(
        endpoint: Arc<FipsEndpoint>,
        store: Arc<S>,
        transport_config: TcpBlobTransportConfig,
        advertise_priority: Option<i16>,
        serve_inbound: bool,
    ) -> Result<Self, TcpBlobTransportError> {
        if transport_config.idle_timeout.is_zero() {
            return Err(TcpBlobTransportError::InvalidConfig(
                "idle timeout must be non-zero",
            ));
        }
        let config = TcpConfig {
            max_connections: MAX_TCP_CONNECTIONS,
            time_wait_ms: 1_000,
            ..TcpConfig::default()
        };
        let isn_seed = NEXT_ISN_SEED.fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed);
        let tcp = match advertise_priority {
            Some(priority) => {
                let capability =
                    LocalInstanceCapability::service(TCP_BLOB_CAPABILITY, TCP_BLOB_SERVICE_PORT)
                        .with_priority(priority);
                FipsTcpEndpoint::bind_with_capability(endpoint, capability, config, isn_seed)
                    .await
                    .map_err(transport_error)?
            }
            None => FipsTcpEndpoint::bind(endpoint, TCP_BLOB_SERVICE_PORT, config, isn_seed)
                .await
                .map_err(transport_error)?,
        };
        let (commands, command_rx) = mpsc::channel(COMMAND_CHANNEL_CAPACITY);
        let actor = TcpBlobActor {
            tcp,
            serve_inbound,
            store: store.clone(),
            commands: command_rx,
            pending_commands: VecDeque::with_capacity(PENDING_COMMAND_CAPACITY),
            active_gets: HashMap::new(),
            servers: HashMap::new(),
            store_loads: JoinSet::new(),
            store_load_connections: HashMap::new(),
            started: Instant::now(),
            idle_timeout_ms: duration_ms(transport_config.idle_timeout),
        };
        let task = tokio::spawn(actor.run());
        Ok(Self {
            store,
            commands,
            task: Some(task),
        })
    }

    /// Return a verified local or remote blob. `Ok(None)` is reserved for an
    /// explicit protocol-level miss; connection and integrity failures are errors.
    pub async fn get(
        &self,
        hash: &Hash,
        peer: PeerIdentity,
    ) -> Result<Option<Vec<u8>>, TcpBlobTransportError> {
        if let Some(data) = self.store.get(hash).await? {
            if !verify_hash(&data, hash) {
                return Err(TcpBlobTransportError::HashMismatch);
            }
            return Ok(Some(data));
        }

        self.fetch_from_peer(hash, peer).await
    }

    /// Fetch directly from one authenticated peer and cache a verified hit.
    /// Unlike [`Self::get`], this skips the local lookup so a coordinator can
    /// race multiple discovered providers without serial local-store reads.
    pub async fn fetch_from_peer(
        &self,
        hash: &Hash,
        peer: PeerIdentity,
    ) -> Result<Option<Vec<u8>>, TcpBlobTransportError> {
        let data = self.request_from_peer(hash, peer).await?;
        if let Some(data) = &data {
            self.store.put(*hash, data.clone()).await?;
        }
        Ok(data)
    }

    pub(crate) async fn request_from_peer(
        &self,
        hash: &Hash,
        peer: PeerIdentity,
    ) -> Result<Option<Vec<u8>>, TcpBlobTransportError> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(Command::Get {
                peer,
                hash: *hash,
                reply,
            })
            .await
            .map_err(|_| TcpBlobTransportError::Closed)?;
        result.await.map_err(|_| TcpBlobTransportError::Closed)?
    }

    pub async fn shutdown(mut self) -> Result<(), TcpBlobTransportError> {
        let (reply, stopped) = oneshot::channel();
        let sent = self
            .commands
            .send(Command::Shutdown { reply })
            .await
            .is_ok();
        if sent {
            let _ = stopped.await;
        }
        let Some(task) = self.task.take() else {
            return Ok(());
        };
        task.await??;
        Ok(())
    }
}

impl<S: Store + ?Sized + 'static> Drop for TcpBlobTransport<S> {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

enum Command {
    Get {
        peer: PeerIdentity,
        hash: Hash,
        reply: oneshot::Sender<Result<Option<Vec<u8>>, TcpBlobTransportError>>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

struct PendingGet {
    peer: PeerIdentity,
    hash: Hash,
    reply: oneshot::Sender<Result<Option<Vec<u8>>, TcpBlobTransportError>>,
    attempts_started: u8,
}

struct ActiveGet {
    request: PendingGet,
    connection: ConnectionId,
    deadline_ms: u64,
    phase: ClientPhase,
}

enum ClientPhase {
    Connecting,
    WritingRequest { offset: usize },
    ReadingHeader { bytes: Vec<u8> },
    ReadingBody { size: usize, bytes: Vec<u8> },
}

struct ServerConnection {
    deadline_ms: u64,
    phase: ServerPhase,
}

enum ServerPhase {
    ReadingRequest {
        bytes: Vec<u8>,
    },
    WaitingForStore {
        hash: Hash,
    },
    LoadingStore {
        hash: Hash,
        task_id: TaskId,
        abort: AbortHandle,
    },
    WritingResponse {
        header: [u8; RESPONSE_HEADER_BYTES],
        header_offset: usize,
        data: Option<Vec<u8>>,
        data_offset: usize,
    },
}

struct StoreLoad {
    connection: ConnectionId,
    result: Result<Option<Vec<u8>>, StoreError>,
}

struct TcpBlobActor<S: Store + ?Sized + 'static> {
    tcp: FipsTcpEndpoint,
    serve_inbound: bool,
    store: Arc<S>,
    commands: mpsc::Receiver<Command>,
    pending_commands: VecDeque<Command>,
    active_gets: HashMap<ConnectionId, ActiveGet>,
    servers: HashMap<ConnectionId, ServerConnection>,
    store_loads: JoinSet<StoreLoad>,
    store_load_connections: HashMap<TaskId, ConnectionId>,
    started: Instant,
    idle_timeout_ms: u64,
}

impl<S: Store + ?Sized + 'static> TcpBlobActor<S> {
    async fn run(mut self) -> Result<(), TcpBlobTransportError> {
        let mut ticker = tokio::time::interval(POLL_INTERVAL);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            let now_ms = self.now_ms();
            tokio::select! {
                received = self.tcp.receive(now_ms) => {
                    received.map_err(transport_error)?;
                }
                command = self.commands.recv(), if self.pending_commands.len() < PENDING_COMMAND_CAPACITY => {
                    let Some(command) = command else {
                        return Ok(());
                    };
                    self.pending_commands.push_back(command);
                }
                completed = self.store_loads.join_next_with_id(), if !self.store_loads.is_empty() => {
                    self.handle_store_load_completion(completed, now_ms).await;
                }
                _ = ticker.tick() => {
                    self.tcp.poll(now_ms).await.map_err(transport_error)?;
                }
            }

            let now_ms = self.now_ms();
            self.accept_connections(now_ms).await;
            self.drive_servers(now_ms).await;
            self.start_store_loads();
            self.drive_clients(now_ms).await;
            if self.handle_pending_commands(now_ms).await {
                return Ok(());
            }
        }
    }

    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis().min(u64::MAX as u128) as u64
    }

    fn deadline_ms(&self, now_ms: u64) -> u64 {
        refreshed_deadline(now_ms, self.idle_timeout_ms)
    }

    async fn handle_pending_commands(&mut self, now_ms: u64) -> bool {
        self.pending_commands.retain(|command| match command {
            Command::Get { reply, .. } => !reply.is_closed(),
            Command::Shutdown { .. } => true,
        });

        if let Some(position) = self
            .pending_commands
            .iter()
            .position(|command| matches!(command, Command::Shutdown { .. }))
        {
            let Command::Shutdown { reply } = self
                .pending_commands
                .remove(position)
                .expect("shutdown command position")
            else {
                unreachable!("located command is shutdown");
            };
            self.shutdown_actor(now_ms).await;
            let _ = reply.send(());
            return true;
        }

        while self.active_gets.len() < MAX_OUTBOUND_GETS {
            let Some(command) = self.pending_commands.pop_front() else {
                break;
            };
            let Command::Get { peer, hash, reply } = command else {
                unreachable!("shutdown commands are handled first");
            };
            if !reply.is_closed() {
                let request = PendingGet {
                    peer,
                    hash,
                    reply,
                    attempts_started: 0,
                };
                self.start_attempt(request, now_ms, None).await;
            }
        }
        false
    }

    async fn shutdown_actor(&mut self, now_ms: u64) {
        for server in self.servers.values() {
            if let ServerPhase::LoadingStore { abort, .. } = &server.phase {
                abort.abort();
            }
        }
        let mut store_loads = std::mem::take(&mut self.store_loads);
        store_loads.abort_all();
        tokio::spawn(async move {
            store_loads.shutdown().await;
        });
        self.store_load_connections.clear();

        let mut connections = self.servers.keys().copied().collect::<Vec<_>>();
        connections.extend(self.active_gets.keys().copied());
        for connection in connections {
            let _ = self.tcp.close(connection, now_ms).await;
        }
        self.servers.clear();
        self.active_gets.clear();
        self.pending_commands.clear();
    }

    async fn start_attempt(
        &mut self,
        mut request: PendingGet,
        now_ms: u64,
        mut last_error: Option<TcpBlobTransportError>,
    ) {
        if request.reply.is_closed() {
            return;
        }
        while request.attempts_started < MAX_SESSION_ATTEMPTS {
            request.attempts_started += 1;
            match self.tcp.connect(request.peer, now_ms).await {
                Ok(connection) => {
                    self.active_gets.insert(
                        connection,
                        ActiveGet {
                            request,
                            connection,
                            deadline_ms: self.deadline_ms(now_ms),
                            phase: ClientPhase::Connecting,
                        },
                    );
                    return;
                }
                Err(error) => last_error = Some(transport_error(error)),
            }
            if request.reply.is_closed() {
                return;
            }
        }
        let _ = request
            .reply
            .send(Err(last_error.unwrap_or(TcpBlobTransportError::Transport(
                "could not open TCP/FIPS session".to_string(),
            ))));
    }

    async fn drive_clients(&mut self, now_ms: u64) {
        let active_gets = std::mem::take(&mut self.active_gets);
        for (_, mut active) in active_gets {
            if active.request.reply.is_closed() {
                let _ = self.tcp.close(active.connection, now_ms).await;
                continue;
            }
            match drive_active_get(&mut self.tcp, &mut active, now_ms, self.idle_timeout_ms).await {
                ClientDrive::Pending => {
                    if active.request.reply.is_closed() {
                        let _ = self.tcp.close(active.connection, now_ms).await;
                    } else {
                        self.active_gets.insert(active.connection, active);
                    }
                }
                ClientDrive::Complete(data) => {
                    let _ = self.tcp.close(active.connection, now_ms).await;
                    let _ = active.request.reply.send(Ok(data));
                }
                ClientDrive::Failed(error) => {
                    let _ = self.tcp.close(active.connection, now_ms).await;
                    if !active.request.reply.is_closed() {
                        self.start_attempt(active.request, now_ms, Some(error))
                            .await;
                    }
                }
            }
        }
    }

    async fn accept_connections(&mut self, now_ms: u64) {
        while let Some(connection) = self.tcp.accept() {
            if self.servers.len() >= MAX_SERVER_CONNECTIONS || !self.serve_inbound {
                let _ = self.tcp.close(connection, now_ms).await;
                continue;
            }
            self.servers.insert(
                connection,
                ServerConnection {
                    deadline_ms: self.deadline_ms(now_ms),
                    phase: ServerPhase::ReadingRequest {
                        bytes: Vec::with_capacity(REQUEST_BYTES),
                    },
                },
            );
        }
    }

    async fn drive_servers(&mut self, now_ms: u64) {
        let servers = std::mem::take(&mut self.servers);
        for (connection, mut server) in servers {
            if drive_server(
                &mut self.tcp,
                connection,
                &mut server,
                now_ms,
                self.idle_timeout_ms,
            )
            .await
            {
                self.servers.insert(connection, server);
            } else {
                self.cancel_store_load(&server);
                let _ = self.tcp.close(connection, now_ms).await;
            }
        }
    }

    fn start_store_loads(&mut self) {
        let available = MAX_STORE_LOADS.saturating_sub(self.store_loads.len());
        let waiting = self
            .servers
            .iter()
            .filter_map(|(connection, server)| match server.phase {
                ServerPhase::WaitingForStore { hash } => Some((*connection, hash)),
                _ => None,
            })
            .take(available)
            .collect::<Vec<_>>();

        for (connection, hash) in waiting {
            let store = self.store.clone();
            let abort = self.store_loads.spawn(async move {
                StoreLoad {
                    connection,
                    result: store.get(&hash).await,
                }
            });
            let task_id = abort.id();
            self.store_load_connections.insert(task_id, connection);
            if let Some(server) = self.servers.get_mut(&connection) {
                server.phase = ServerPhase::LoadingStore {
                    hash,
                    task_id,
                    abort,
                };
            } else {
                abort.abort();
                self.store_load_connections.remove(&task_id);
            }
        }
    }

    async fn handle_store_load_completion(
        &mut self,
        completed: Option<Result<(TaskId, StoreLoad), JoinError>>,
        now_ms: u64,
    ) {
        let (task_id, connection, result) = match completed {
            Some(Ok((task_id, load))) => (task_id, load.connection, Some(load.result)),
            Some(Err(error)) => {
                let task_id = error.id();
                let Some(connection) = self.store_load_connections.get(&task_id).copied() else {
                    return;
                };
                (task_id, connection, None)
            }
            None => return,
        };
        if self.store_load_connections.remove(&task_id) != Some(connection) {
            return;
        }

        let Some(mut server) = self.servers.remove(&connection) else {
            return;
        };
        let hash = match server.phase {
            ServerPhase::LoadingStore {
                hash,
                task_id: expected,
                ..
            } if expected == task_id => hash,
            _ => {
                let _ = self.tcp.close(connection, now_ms).await;
                return;
            }
        };
        let data = match result {
            Some(Ok(Some(data))) if verify_hash(&data, &hash) => Some(data),
            Some(Ok(None)) => None,
            _ => {
                let _ = self.tcp.close(connection, now_ms).await;
                return;
            }
        };
        if data
            .as_ref()
            .is_some_and(|data| data.len() > TCP_BLOB_MAX_BYTES)
        {
            let _ = self.tcp.close(connection, now_ms).await;
            return;
        }
        let header =
            encode_tcp_blob_response_header(data.is_some(), data.as_ref().map_or(0, Vec::len))
                .expect("validated TCP blob response size");
        server.deadline_ms = self.deadline_ms(now_ms);
        server.phase = ServerPhase::WritingResponse {
            header,
            header_offset: 0,
            data,
            data_offset: 0,
        };
        self.servers.insert(connection, server);
    }

    fn cancel_store_load(&mut self, server: &ServerConnection) {
        if let ServerPhase::LoadingStore { task_id, abort, .. } = &server.phase {
            abort.abort();
            self.store_load_connections.remove(task_id);
        }
    }
}

enum ClientDrive {
    Pending,
    Complete(Option<Vec<u8>>),
    Failed(TcpBlobTransportError),
}

async fn drive_active_get(
    tcp: &mut FipsTcpEndpoint,
    active: &mut ActiveGet,
    now_ms: u64,
    idle_timeout_ms: u64,
) -> ClientDrive {
    if now_ms >= active.deadline_ms {
        return ClientDrive::Failed(TcpBlobTransportError::Timeout);
    }
    if tcp.state(active.connection).is_none() {
        return ClientDrive::Failed(TcpBlobTransportError::Transport(
            "TCP/FIPS session closed".to_string(),
        ));
    }

    match &mut active.phase {
        ClientPhase::Connecting => {
            if tcp.state(active.connection) == Some(State::Established) {
                active.deadline_ms = refreshed_deadline(now_ms, idle_timeout_ms);
                active.phase = ClientPhase::WritingRequest { offset: 0 };
            }
            ClientDrive::Pending
        }
        ClientPhase::WritingRequest { offset } => {
            let request = encode_tcp_blob_request(&active.request.hash);
            match tcp
                .write(active.connection, &request[*offset..], now_ms)
                .await
            {
                Ok(written) => {
                    if written > 0 {
                        active.deadline_ms = refreshed_deadline(now_ms, idle_timeout_ms);
                    }
                    *offset += written;
                    if *offset == request.len() {
                        active.phase = ClientPhase::ReadingHeader {
                            bytes: Vec::with_capacity(RESPONSE_HEADER_BYTES),
                        };
                    }
                    ClientDrive::Pending
                }
                Err(error) => ClientDrive::Failed(transport_error(error)),
            }
        }
        ClientPhase::ReadingHeader { bytes } => {
            match tcp
                .read(
                    active.connection,
                    RESPONSE_HEADER_BYTES - bytes.len(),
                    now_ms,
                )
                .await
            {
                Ok(chunk) => {
                    if !chunk.is_empty() {
                        active.deadline_ms = refreshed_deadline(now_ms, idle_timeout_ms);
                    }
                    bytes.extend_from_slice(&chunk);
                }
                Err(error) => return ClientDrive::Failed(transport_error(error)),
            }
            if bytes.len() == RESPONSE_HEADER_BYTES {
                match decode_response_header(bytes) {
                    Ok(ResponseHeader::Missing) => return ClientDrive::Complete(None),
                    Ok(ResponseHeader::Found(0)) => {
                        let data = Vec::new();
                        return if verify_hash(&data, &active.request.hash) {
                            ClientDrive::Complete(Some(data))
                        } else {
                            ClientDrive::Failed(TcpBlobTransportError::HashMismatch)
                        };
                    }
                    Ok(ResponseHeader::Found(size)) => {
                        active.phase = ClientPhase::ReadingBody {
                            size,
                            bytes: Vec::with_capacity(size),
                        };
                    }
                    Err(error) => return ClientDrive::Failed(error),
                }
            } else if tcp.is_read_closed(active.connection) {
                return ClientDrive::Failed(TcpBlobTransportError::Transport(
                    "TCP/FIPS response closed before its header".to_string(),
                ));
            }
            ClientDrive::Pending
        }
        ClientPhase::ReadingBody { size, bytes } => {
            match tcp
                .read(
                    active.connection,
                    IO_CHUNK_BYTES.min(*size - bytes.len()),
                    now_ms,
                )
                .await
            {
                Ok(chunk) => {
                    if !chunk.is_empty() {
                        active.deadline_ms = refreshed_deadline(now_ms, idle_timeout_ms);
                    }
                    bytes.extend_from_slice(&chunk);
                }
                Err(error) => return ClientDrive::Failed(transport_error(error)),
            }
            if bytes.len() == *size {
                let data = std::mem::take(bytes);
                if verify_hash(&data, &active.request.hash) {
                    ClientDrive::Complete(Some(data))
                } else {
                    ClientDrive::Failed(TcpBlobTransportError::HashMismatch)
                }
            } else if tcp.is_read_closed(active.connection) {
                ClientDrive::Failed(TcpBlobTransportError::Transport(
                    "TCP/FIPS response closed before its payload".to_string(),
                ))
            } else {
                ClientDrive::Pending
            }
        }
    }
}

async fn drive_server(
    tcp: &mut FipsTcpEndpoint,
    connection: ConnectionId,
    server: &mut ServerConnection,
    now_ms: u64,
    idle_timeout_ms: u64,
) -> bool {
    if now_ms >= server.deadline_ms
        || tcp.state(connection).is_none()
        || tcp.is_read_closed(connection)
    {
        return false;
    }

    match &mut server.phase {
        ServerPhase::ReadingRequest { bytes } => {
            let chunk = match tcp
                .read(connection, REQUEST_BYTES - bytes.len(), now_ms)
                .await
            {
                Ok(chunk) => chunk,
                Err(_) => return false,
            };
            if !chunk.is_empty() {
                server.deadline_ms = refreshed_deadline(now_ms, idle_timeout_ms);
            }
            bytes.extend_from_slice(&chunk);
            if bytes.len() == REQUEST_BYTES {
                let hash = match decode_request(bytes) {
                    Ok(hash) => hash,
                    Err(_) => return false,
                };
                server.phase = ServerPhase::WaitingForStore { hash };
            } else if tcp.is_read_closed(connection) {
                return false;
            }
            true
        }
        ServerPhase::WaitingForStore { .. } | ServerPhase::LoadingStore { .. } => true,
        ServerPhase::WritingResponse {
            header,
            header_offset,
            data,
            data_offset,
        } => {
            if *header_offset < header.len() {
                let written = match tcp
                    .write(connection, &header[*header_offset..], now_ms)
                    .await
                {
                    Ok(written) => written,
                    Err(_) => return false,
                };
                if written > 0 {
                    server.deadline_ms = refreshed_deadline(now_ms, idle_timeout_ms);
                }
                *header_offset += written;
                return true;
            }
            let Some(data) = data else {
                return false;
            };
            if *data_offset == data.len() {
                return false;
            }
            let end = (*data_offset + IO_CHUNK_BYTES).min(data.len());
            let written = match tcp
                .write(connection, &data[*data_offset..end], now_ms)
                .await
            {
                Ok(written) => written,
                Err(_) => return false,
            };
            if written > 0 {
                server.deadline_ms = refreshed_deadline(now_ms, idle_timeout_ms);
            }
            *data_offset += written;
            *data_offset < data.len()
        }
    }
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().clamp(1, u64::MAX as u128) as u64
}

fn refreshed_deadline(now_ms: u64, idle_timeout_ms: u64) -> u64 {
    now_ms.saturating_add(idle_timeout_ms)
}

pub fn encode_tcp_blob_request(hash: &Hash) -> [u8; REQUEST_BYTES] {
    let mut request = [0; REQUEST_BYTES];
    request[..3].copy_from_slice(&[MAGIC, VERSION, GET]);
    request[3..].copy_from_slice(hash);
    request
}

pub fn encode_tcp_blob_response_header(
    found: bool,
    size: usize,
) -> Result<[u8; RESPONSE_HEADER_BYTES], TcpBlobTransportError> {
    if size > TCP_BLOB_MAX_BYTES {
        return Err(TcpBlobTransportError::BlobTooLarge(size));
    }
    if !found && size != 0 {
        return Err(TcpBlobTransportError::Protocol(
            "missing response has a non-zero payload length",
        ));
    }
    let mut header = [0; RESPONSE_HEADER_BYTES];
    header[..3].copy_from_slice(&[MAGIC, VERSION, if found { FOUND } else { 0 }]);
    header[3..].copy_from_slice(&(size as u32).to_be_bytes());
    Ok(header)
}

fn decode_request(bytes: &[u8]) -> Result<Hash, TcpBlobTransportError> {
    if bytes.len() != REQUEST_BYTES {
        return Err(TcpBlobTransportError::Protocol(
            "request has the wrong length",
        ));
    }
    if bytes[..3] != [MAGIC, VERSION, GET] {
        return Err(TcpBlobTransportError::Protocol(
            "request has an unsupported prelude",
        ));
    }
    let mut hash = [0; 32];
    hash.copy_from_slice(&bytes[3..]);
    Ok(hash)
}

enum ResponseHeader {
    Missing,
    Found(usize),
}

fn decode_response_header(bytes: &[u8]) -> Result<ResponseHeader, TcpBlobTransportError> {
    if bytes.len() != RESPONSE_HEADER_BYTES {
        return Err(TcpBlobTransportError::Protocol(
            "response header has the wrong length",
        ));
    }
    if bytes[0] != MAGIC || bytes[1] != VERSION {
        return Err(TcpBlobTransportError::Protocol(
            "response has an unsupported prelude",
        ));
    }
    let size = u32::from_be_bytes(bytes[3..].try_into().expect("four-byte length")) as usize;
    if size > TCP_BLOB_MAX_BYTES {
        return Err(TcpBlobTransportError::BlobTooLarge(size));
    }
    match bytes[2] {
        0 if size == 0 => Ok(ResponseHeader::Missing),
        0 => Err(TcpBlobTransportError::Protocol(
            "missing response has a non-zero payload length",
        )),
        FOUND => Ok(ResponseHeader::Found(size)),
        _ => Err(TcpBlobTransportError::Protocol(
            "response has an unsupported status",
        )),
    }
}

fn verify_hash(data: &[u8], expected: &Hash) -> bool {
    Sha256::digest(data).as_slice() == expected
}

fn transport_error(error: impl std::fmt::Display) -> TcpBlobTransportError {
    TcpBlobTransportError::Transport(error.to_string())
}

#[cfg(test)]
mod tests;
