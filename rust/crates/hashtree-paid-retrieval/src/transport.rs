//! Carriers: the request/reply seam and length-prefixed byte streams.

use std::future::Future;
use std::io;
use std::pin::Pin;

use async_trait::async_trait;
use hashtree_core::StoreError;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex;

use crate::message::MAX_MESSAGE_BYTES;
use crate::provider::PaidBlobProvider;

/// Delivers one encoded request to one provider and returns its encoded reply.
///
/// The route drops the returned future at its deadline, so an implementation
/// must stay usable after an abandoned exchange.
#[async_trait]
pub trait PaidBlobTransport: Send + Sync {
    async fn exchange(&self, request: Vec<u8>) -> Result<Vec<u8>, StoreError>;
}

pub trait ByteStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> ByteStream for T {}

pub type BoxedStream = Box<dyn ByteStream>;
type Connect =
    dyn Fn() -> Pin<Box<dyn Future<Output = io::Result<BoxedStream>> + Send>> + Send + Sync;

pub const FRAME_PREFIX_BYTES: usize = 4;

/// The prefix of a message of `length` bytes on a byte stream: its length as
/// a 32-bit big-endian integer.
pub fn frame_prefix(length: usize) -> io::Result<[u8; FRAME_PREFIX_BYTES]> {
    u32::try_from(length)
        .ok()
        .filter(|length| *length as usize <= MAX_MESSAGE_BYTES)
        .map(u32::to_be_bytes)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "message is too large"))
}

/// The length of the message that follows `prefix`.
pub fn frame_length(prefix: [u8; FRAME_PREFIX_BYTES]) -> io::Result<usize> {
    let length = u32::from_be_bytes(prefix) as usize;
    if length > MAX_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "message is too large",
        ));
    }
    Ok(length)
}

/// Write one message with its length prefix.
pub async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, message: &[u8]) -> io::Result<()> {
    writer.write_all(&frame_prefix(message.len())?).await?;
    writer.write_all(message).await?;
    writer.flush().await
}

/// Read one message, or `None` when the peer closed between messages.
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut prefix = [0; FRAME_PREFIX_BYTES];
    match reader.read_exact(&mut prefix).await {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let mut message = vec![0; frame_length(prefix)?];
    reader.read_exact(&mut message).await?;
    Ok(Some(message))
}

/// Serve one connection until the peer closes it.
pub async fn serve_stream<S: AsyncRead + AsyncWrite + Unpin>(
    provider: &PaidBlobProvider,
    peer: Option<&str>,
    mut stream: S,
) -> io::Result<()> {
    while let Some(request) = read_frame(&mut stream).await? {
        let reply = provider.handle(peer, &request).await;
        write_frame(&mut stream, &reply).await?;
    }
    Ok(())
}

async fn round_trip(stream: &mut BoxedStream, request: &[u8]) -> io::Result<Option<Vec<u8>>> {
    write_frame(stream, request).await?;
    read_frame(stream).await
}

/// One reused connection to a provider, reopened after any failure.
pub struct StreamTransport {
    connect: Box<Connect>,
    idle: Mutex<Option<BoxedStream>>,
}

impl StreamTransport {
    pub fn new<F, Fut>(connect: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = io::Result<BoxedStream>> + Send + 'static,
    {
        Self {
            connect: Box::new(move || Box::pin(connect())),
            idle: Mutex::new(None),
        }
    }
}

#[async_trait]
impl PaidBlobTransport for StreamTransport {
    async fn exchange(&self, request: Vec<u8>) -> Result<Vec<u8>, StoreError> {
        let failed =
            |error: io::Error| StoreError::Other(format!("paid route transport failed: {error}"));
        let mut idle = self.idle.lock().await;
        // The stream leaves the slot while in use, so an abandoned or failed
        // exchange closes it instead of leaving a half-read reply behind.
        if let Some(mut stream) = idle.take() {
            // A reused connection may have been closed while idle. Requests
            // are idempotent, so one fresh connection may repeat it.
            if let Ok(Some(reply)) = round_trip(&mut stream, &request).await {
                *idle = Some(stream);
                return Ok(reply);
            }
        }
        let mut stream = (self.connect)().await.map_err(failed)?;
        let reply = round_trip(&mut stream, &request)
            .await
            .map_err(failed)?
            .ok_or_else(|| StoreError::Other("paid route provider closed the connection".into()))?;
        *idle = Some(stream);
        Ok(reply)
    }
}
