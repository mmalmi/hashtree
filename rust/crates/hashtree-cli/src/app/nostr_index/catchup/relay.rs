use std::collections::BTreeMap;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use hashtree_nostr::catchup::{CatchupError, CatchupQuery, CatchupSource, Result};
use hashtree_nostr::{stored_event_from_nostr_sdk_event, StoredNostrEvent};
use tokio::net::TcpStream;
use tokio::time::Instant;
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{
        error::ProtocolError,
        protocol::{frame::coding::CloseCode, WebSocketConfig},
        Error as WebSocketError, Message,
    },
    MaybeTlsStream, WebSocketStream,
};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
mod count;
const MAX_EVENT_BYTES: usize = 1024 * 1024;
const RECONNECT_DELAY: Duration = Duration::from_millis(250);
const GATEWAY_RECONNECT_DELAY: Duration = Duration::from_secs(1);
const CLOSE_TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Debug)]
struct QueryFailure {
    error: CatchupError,
    reconnect: bool,
    gateway_retry_after: Option<Duration>,
}

impl From<CatchupError> for QueryFailure {
    fn from(error: CatchupError) -> Self {
        Self {
            error,
            reconnect: false,
            gateway_retry_after: None,
        }
    }
}

impl QueryFailure {
    fn transport(context: &str, error: WebSocketError) -> Self {
        if let WebSocketError::Http(response) = &error {
            let status = response.status().as_u16();
            let delay = if matches!(status, 502 | 503 | 504) {
                match response.headers().get("retry-after") {
                    None => Some(GATEWAY_RECONNECT_DELAY),
                    Some(value) => value.to_str().ok().and_then(|value| {
                        let value = value.trim();
                        // HTTP dates and malformed values remain terminal. Never
                        // reconnect early when the requested delay is unknown.
                        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                            return None;
                        }
                        value.parse::<u64>().ok().map(|seconds| {
                            Duration::from_secs(seconds).max(GATEWAY_RECONNECT_DELAY)
                        })
                    }),
                }
            } else {
                None
            };
            return Self {
                // Do not include gateway bodies or arbitrary response headers.
                error: CatchupError(format!("{context}: HTTP status {status}")),
                reconnect: delay.is_some(),
                gateway_retry_after: delay,
            };
        }
        let reconnect = match &error {
            WebSocketError::ConnectionClosed
            | WebSocketError::AlreadyClosed
            | WebSocketError::Protocol(ProtocolError::ResetWithoutClosingHandshake) => true,
            WebSocketError::Io(error) => matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::UnexpectedEof
            ),
            _ => false,
        };
        Self {
            error: CatchupError(format!("{context}: {error}")),
            reconnect,
            gateway_retry_after: None,
        }
    }
}

#[derive(Default)]
struct QueryBudget {
    bytes: usize,
    messages: usize,
}

#[derive(Debug)]
struct CompletedPage {
    events: Vec<StoredNostrEvent>,
    subscription: String,
}

pub(super) struct RelaySource {
    sockets: BTreeMap<String, Socket>,
    sequence: u64,
    timeout: Duration,
    max_bytes: usize,
}

impl RelaySource {
    pub(super) fn new(timeout_secs: u64, max_bytes: usize) -> Self {
        Self {
            sockets: BTreeMap::new(),
            sequence: 0,
            timeout: Duration::from_secs(timeout_secs),
            max_bytes,
        }
    }

    async fn ensure_connected(&mut self, relay: &str) -> std::result::Result<(), QueryFailure> {
        if !self.sockets.contains_key(relay) {
            let config = WebSocketConfig {
                max_message_size: Some(MAX_EVENT_BYTES + 4096),
                max_frame_size: Some(MAX_EVENT_BYTES + 4096),
                ..Default::default()
            };
            let (socket, _) = connect_async_with_config(relay, Some(config), false)
                .await
                .map_err(|err| QueryFailure::transport("connect", err))?;
            self.sockets.insert(relay.to_owned(), socket);
        }
        Ok(())
    }

    async fn query_inner(
        &mut self,
        relay: &str,
        query: &CatchupQuery,
        budget: &mut QueryBudget,
    ) -> std::result::Result<CompletedPage, QueryFailure> {
        self.ensure_connected(relay).await?;
        self.sequence += 1;
        let subscription = format!("catchup-{}", self.sequence);
        let socket = self.sockets.get_mut(relay).expect("connected source");
        socket
            .send(Message::Text(
                serde_json::json!(["REQ", subscription, {
                    "authors": [query.author], "kinds": query.kinds,
                    "since": query.since, "until": query.until, "limit": query.limit,
                }])
                .to_string(),
            ))
            .await
            .map_err(|err| QueryFailure::transport("send REQ", err))?;
        let mut events = BTreeMap::new();
        loop {
            let message = socket
                .next()
                .await
                .ok_or_else(|| QueryFailure {
                    error: CatchupError("socket ended before EOSE".into()),
                    reconnect: true,
                    gateway_retry_after: None,
                })?
                .map_err(|err| QueryFailure::transport("socket before EOSE", err))?;
            budget.messages += 1;
            if budget.messages > query.limit.saturating_mul(4).saturating_add(100) {
                return Err(
                    CatchupError("source message budget exhausted before EOSE".into()).into(),
                );
            }
            let raw = match message {
                Message::Text(text) => text,
                Message::Ping(payload) => {
                    socket
                        .send(Message::Pong(payload))
                        .await
                        .map_err(|err| QueryFailure::transport("send PONG", err))?;
                    continue;
                }
                Message::Pong(_) => continue,
                Message::Close(frame) => {
                    return Err(QueryFailure {
                        error: CatchupError("socket closed before EOSE".into()),
                        reconnect: frame.as_ref().is_none_or(|frame| {
                            matches!(frame.code, CloseCode::Normal | CloseCode::Away)
                        }),
                        gateway_retry_after: None,
                    })
                }
                _ => return Err(CatchupError("unexpected non-text relay message".into()).into()),
            };
            budget.bytes = budget.bytes.saturating_add(raw.len());
            if budget.bytes > self.max_bytes.saturating_add(4096) {
                return Err(CatchupError("source byte budget exhausted before EOSE".into()).into());
            }
            let value: serde_json::Value = serde_json::from_str(&raw)
                .map_err(|err| CatchupError(format!("invalid relay JSON: {err}")))?;
            let parts = value
                .as_array()
                .ok_or_else(|| CatchupError("relay message must be an array".into()))?;
            if parts.get(1).and_then(|part| part.as_str()) != Some(subscription.as_str()) {
                continue;
            }
            match parts.first().and_then(|part| part.as_str()) {
                Some("EVENT") => {
                    let raw_event = parts
                        .get(2)
                        .ok_or_else(|| CatchupError("EVENT missing payload".into()))?;
                    if serde_json::to_vec(raw_event)
                        .map_err(|err| CatchupError(err.to_string()))?
                        .len()
                        > MAX_EVENT_BYTES
                    {
                        return Err(CatchupError("event exceeds 1 MiB".into()).into());
                    }
                    let event: nostr::Event = serde_json::from_value(raw_event.clone())
                        .map_err(|err| CatchupError(format!("invalid event: {err}")))?;
                    event
                        .verify()
                        .map_err(|err| CatchupError(format!("event verification: {err}")))?;
                    if event.pubkey.to_hex() != query.author
                        || event.created_at.as_secs() < query.since
                        || event.created_at.as_secs() > query.until
                        || !query.kinds.contains(&event.kind.as_u16())
                    {
                        return Err(CatchupError(
                            "signed event does not match requested filter".into(),
                        )
                        .into());
                    }
                    events.insert(event.id, stored_event_from_nostr_sdk_event(&event));
                    if events.len() > query.limit {
                        return Err(
                            CatchupError("source exceeded requested event limit".into()).into()
                        );
                    }
                }
                Some("EOSE") if parts.len() == 2 => {
                    return Ok(CompletedPage {
                        events: events.into_values().collect(),
                        subscription,
                    });
                }
                Some("CLOSED") => {
                    return Err(CatchupError("relay CLOSED subscription before EOSE".into()).into())
                }
                _ => return Err(CatchupError("unexpected subscription message".into()).into()),
            }
        }
    }

    async fn finish_page(
        &mut self,
        relay: &str,
        page: CompletedPage,
        deadline: Instant,
    ) -> Vec<StoredNostrEvent> {
        let close_deadline = deadline.min(Instant::now() + CLOSE_TIMEOUT);
        let closed = if let Some(socket) = self.sockets.get_mut(relay) {
            matches!(
                tokio::time::timeout_at(
                    close_deadline,
                    socket.send(Message::Text(
                        serde_json::json!(["CLOSE", page.subscription]).to_string()
                    ))
                )
                .await,
                Ok(Ok(()))
            )
        } else {
            false
        };
        if !closed {
            self.sockets.remove(relay);
        }
        // EOSE already established completion. Cleanup failure cannot discard
        // verified results, and an unhealthy connection is never reused.
        page.events
    }
}

impl CatchupSource for RelaySource {
    async fn exact_count(&mut self, relay: &str, query: &CatchupQuery) -> Result<Option<usize>> {
        self.count(relay, query).await
    }

    async fn query(&mut self, relay: &str, query: &CatchupQuery) -> Result<Vec<StoredNostrEvent>> {
        let deadline = Instant::now() + self.timeout;
        let mut budget = QueryBudget::default();
        let result = tokio::time::timeout_at(deadline, async {
            for attempt in 0..=1 {
                match self.query_inner(relay, query, &mut budget).await {
                    Ok(page) => return Ok(page),
                    Err(failure) => {
                        self.sockets.remove(relay);
                        if attempt == 1 || !failure.reconnect {
                            return Err(failure.error);
                        }
                        if failure.gateway_retry_after.is_some_and(|delay| {
                            delay >= deadline.saturating_duration_since(Instant::now())
                        }) {
                            return Err(failure.error);
                        }
                        tokio::time::sleep(failure.gateway_retry_after.unwrap_or(RECONNECT_DELAY))
                            .await;
                    }
                }
            }
            unreachable!("bounded reconnect loop always returns")
        })
        .await
        .unwrap_or_else(|_| {
            Err(CatchupError(
                "timeout before EOSE; coverage incomplete".into(),
            ))
        });
        match result {
            Ok(page) => Ok(self.finish_page(relay, page, deadline).await),
            Err(error) => {
                self.sockets.remove(relay);
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests;
