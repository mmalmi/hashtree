use std::collections::BTreeMap;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use hashtree_nostr::catchup::{CatchupError, CatchupQuery, CatchupSource, Result};
use hashtree_nostr::{stored_event_from_nostr_sdk_event, StoredNostrEvent};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{protocol::WebSocketConfig, Message},
    MaybeTlsStream, WebSocketStream,
};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
const MAX_EVENT_BYTES: usize = 1024 * 1024;

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

    async fn query_inner(
        &mut self,
        relay: &str,
        query: &CatchupQuery,
    ) -> Result<Vec<StoredNostrEvent>> {
        if !self.sockets.contains_key(relay) {
            let config = WebSocketConfig {
                max_message_size: Some(MAX_EVENT_BYTES + 4096),
                max_frame_size: Some(MAX_EVENT_BYTES + 4096),
                ..Default::default()
            };
            let (socket, _) = connect_async_with_config(relay, Some(config), false)
                .await
                .map_err(|err| CatchupError(format!("connect: {err}")))?;
            self.sockets.insert(relay.to_owned(), socket);
        }
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
            .map_err(|err| CatchupError(format!("send REQ: {err}")))?;
        let mut events = BTreeMap::new();
        let mut bytes = 0usize;
        let mut messages = 0usize;
        loop {
            let message = socket
                .next()
                .await
                .ok_or_else(|| CatchupError("socket ended before EOSE".into()))?
                .map_err(|err| CatchupError(format!("socket before EOSE: {err}")))?;
            messages += 1;
            if messages > query.limit.saturating_mul(4).saturating_add(100) {
                return Err(CatchupError(
                    "source message budget exhausted before EOSE".into(),
                ));
            }
            let raw = match message {
                Message::Text(text) => text,
                Message::Ping(payload) => {
                    socket
                        .send(Message::Pong(payload))
                        .await
                        .map_err(|err| CatchupError(err.to_string()))?;
                    continue;
                }
                Message::Pong(_) => continue,
                Message::Close(_) => return Err(CatchupError("socket closed before EOSE".into())),
                _ => return Err(CatchupError("unexpected non-text relay message".into())),
            };
            bytes = bytes.saturating_add(raw.len());
            if bytes > self.max_bytes.saturating_add(4096) {
                return Err(CatchupError(
                    "source byte budget exhausted before EOSE".into(),
                ));
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
                        return Err(CatchupError("event exceeds 1 MiB".into()));
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
                        ));
                    }
                    events.insert(event.id, stored_event_from_nostr_sdk_event(&event));
                    if events.len() > query.limit {
                        return Err(CatchupError("source exceeded requested event limit".into()));
                    }
                }
                Some("EOSE") if parts.len() == 2 => {
                    socket
                        .send(Message::Text(
                            serde_json::json!(["CLOSE", subscription]).to_string(),
                        ))
                        .await
                        .map_err(|err| CatchupError(format!("close subscription: {err}")))?;
                    return Ok(events.into_values().collect());
                }
                Some("CLOSED") => {
                    return Err(CatchupError("relay CLOSED subscription before EOSE".into()))
                }
                _ => return Err(CatchupError("unexpected subscription message".into())),
            }
        }
    }
}

impl CatchupSource for RelaySource {
    async fn query(&mut self, relay: &str, query: &CatchupQuery) -> Result<Vec<StoredNostrEvent>> {
        let result = tokio::time::timeout(self.timeout, self.query_inner(relay, query))
            .await
            .unwrap_or_else(|_| {
                Err(CatchupError(
                    "timeout before EOSE; coverage incomplete".into(),
                ))
            });
        if result.is_err() {
            self.sockets.remove(relay);
        }
        result
    }
}
