//! The buying side: this node's own exchanges with providers.

use fips_core::{FipsEndpointError, NodeError};
use fips_tcp::{ConnectionId, StackError, State};
use fips_tcp_endpoint::AdapterError;

use super::super::frame::{Incoming, Outgoing};
use super::super::{transport_error, FipsPaidError};
use super::{
    Actor, Exchange, CONNECT_RETRY_DELAY_MS, MAX_CONNECTIONS_PER_PEER, MAX_IDLE_CONNECTIONS,
    MAX_OUTBOUND_EXCHANGES,
};
use crate::message::MAX_MESSAGE_BYTES;

enum ClientPhase {
    WaitingToConnect { retry_at_ms: u64 },
    Connecting,
    Writing(Outgoing),
    Reading(Incoming),
}

pub(super) struct Active {
    exchange: Exchange,
    pub(super) connection: Option<ConnectionId>,
    /// The connection was kept from an earlier exchange and may have been
    /// closed by the provider meanwhile.
    reused: bool,
    /// A kept connection already failed this exchange: open a new one.
    fresh_only: bool,
    deadline_ms: u64,
    phase: ClientPhase,
}

/// A connection kept after an exchange for the next one with that provider.
pub(super) struct Idle {
    peer: String,
    pub(super) connection: ConnectionId,
    expires_ms: u64,
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
    pub(super) async fn drive_clients(&mut self, now_ms: u64) {
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

    pub(super) async fn expire_idle(&mut self, now_ms: u64) {
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
