//! The selling side: inbound connections answered through the provider.

use fips_tcp::ConnectionId;
use tokio::task::{AbortHandle, Id as TaskId, JoinError};

use super::super::frame::{Incoming, Outgoing};
use super::super::MAX_REQUEST_BYTES;
use super::{Actor, MAX_CONNECTIONS_PER_PEER, MAX_SERVER_CONNECTIONS};

enum ServerPhase {
    Reading(Incoming),
    Handling(AbortHandle),
    Writing { reply: Vec<u8>, outgoing: Outgoing },
}

pub(super) struct Server {
    /// The peer FIPS authenticated, as the provider is told.
    peer: String,
    deadline_ms: u64,
    phase: ServerPhase,
}

/// A provider's reply and the connection that asked for it.
pub(super) type Handled = (ConnectionId, Vec<u8>);

impl Actor {
    pub(super) async fn accept(&mut self, now_ms: u64) {
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

    pub(super) async fn drive_servers(&mut self, now_ms: u64) {
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
    pub(super) async fn handled(&mut self, handled: Option<Result<(TaskId, Handled), JoinError>>) {
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
}
