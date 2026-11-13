//! Length-prefixed messages on a polled TCP/FIPS connection, in the framing
//! of `transport.rs`.

use fips_tcp::ConnectionId;
use fips_tcp_endpoint::{AdapterError, FipsTcpEndpoint};

use super::FipsPaidError;
use crate::transport::{frame_length, frame_prefix, FRAME_PREFIX_BYTES};

const IO_CHUNK_BYTES: usize = 64 * 1024;

/// How far one message has been written.
pub(super) struct Outgoing {
    prefix: [u8; FRAME_PREFIX_BYTES],
    sent: usize,
}

impl Outgoing {
    pub(super) fn new(message: &[u8]) -> Result<Self, FipsPaidError> {
        let prefix = frame_prefix(message.len())
            .map_err(|_| FipsPaidError::Protocol("message is too large"))?;
        Ok(Self { prefix, sent: 0 })
    }

    /// Write as much of `message` as the connection takes now. `Ok(true)`
    /// once all of it is written; `progress` is set when any of it was.
    pub(super) async fn write(
        &mut self,
        tcp: &mut FipsTcpEndpoint,
        connection: ConnectionId,
        message: &[u8],
        now_ms: u64,
        progress: &mut bool,
    ) -> Result<bool, AdapterError> {
        loop {
            let chunk = match self.sent.checked_sub(FRAME_PREFIX_BYTES) {
                None => &self.prefix[self.sent..],
                Some(at) if at == message.len() => return Ok(true),
                Some(at) => &message[at..message.len().min(at + IO_CHUNK_BYTES)],
            };
            let written = tcp.write(connection, chunk, now_ms).await?;
            if written == 0 {
                return Ok(false);
            }
            self.sent += written;
            *progress = true;
        }
    }
}

/// One message being read; reusable for the next one.
pub(super) struct Incoming {
    limit: usize,
    prefix: Vec<u8>,
    body: Option<(usize, Vec<u8>)>,
}

impl Incoming {
    /// A longer message than `limit` bytes is a protocol error.
    pub(super) fn new(limit: usize) -> Self {
        Self {
            limit,
            prefix: Vec::with_capacity(FRAME_PREFIX_BYTES),
            body: None,
        }
    }

    /// Whether no byte of a message has arrived yet.
    pub(super) fn is_empty(&self) -> bool {
        self.prefix.is_empty()
    }

    /// Read what has arrived. `Ok(Some(message))` once it is complete.
    pub(super) async fn read(
        &mut self,
        tcp: &mut FipsTcpEndpoint,
        connection: ConnectionId,
        now_ms: u64,
        progress: &mut bool,
    ) -> Result<Option<Vec<u8>>, FipsPaidError> {
        loop {
            let wanted = match &self.body {
                None => FRAME_PREFIX_BYTES - self.prefix.len(),
                Some((length, body)) => IO_CHUNK_BYTES.min(length - body.len()),
            };
            if wanted == 0 {
                self.prefix.clear();
                return Ok(self.body.take().map(|(_, body)| body));
            }
            let chunk = tcp
                .read(connection, wanted, now_ms)
                .await
                .map_err(|error| FipsPaidError::Transport(error.to_string()))?;
            if chunk.is_empty() {
                return Ok(None);
            }
            *progress = true;
            match &mut self.body {
                Some((_, body)) => body.extend_from_slice(&chunk),
                None => {
                    self.prefix.extend_from_slice(&chunk);
                    if let Some(prefix) = self.prefix.first_chunk() {
                        let length = frame_length(*prefix).ok().filter(|l| *l <= self.limit);
                        let length =
                            length.ok_or(FipsPaidError::Protocol("message is too large"))?;
                        // Grown as bytes arrive: a prefix alone reserves nothing.
                        self.body = Some((length, Vec::new()));
                    }
                }
            }
        }
    }
}
