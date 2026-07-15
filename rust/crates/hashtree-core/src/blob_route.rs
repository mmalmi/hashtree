//! One-hop blob routing values and their compact process/network wire codec.

use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;

use crate::{Hash, Store, StoreError};

pub const BLOB_MAX_BYTES: usize = 16 * 1024 * 1024;
pub const BLOB_REQUEST_BYTES: usize = 36;
pub const BLOB_REPLY_HEADER_BYTES: usize = 7;

const MAGIC: u8 = 0x48;
const VERSION: u8 = 1;
const GET: u8 = 1;
const DATA: u8 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlobRequest {
    pub hash: Hash,
    pub htl: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlobReply {
    Data(Vec<u8>),
    NoResult,
}

#[async_trait]
pub trait BlobRoute: Send + Sync {
    async fn route(&self, request: BlobRequest) -> Result<BlobReply, StoreError>;
}

/// A terminal route that performs exactly one lookup and does not interpret HTL.
pub struct StoreBlobRoute<S: Store + ?Sized> {
    store: Arc<S>,
}

impl<S: Store + ?Sized> StoreBlobRoute<S> {
    pub fn new(store: Arc<S>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl<S: Store + ?Sized + 'static> BlobRoute for StoreBlobRoute<S> {
    async fn route(&self, request: BlobRequest) -> Result<BlobReply, StoreError> {
        Ok(match self.store.get(&request.hash).await? {
            Some(data) => BlobReply::Data(data),
            None => BlobReply::NoResult,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlobReplyHeader {
    Data(usize),
    NoResult,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum BlobCodecError {
    #[error("invalid Hashtree blob wire message: {0}")]
    Invalid(&'static str),
    #[error("Hashtree blob size {0} exceeds the 16 MiB limit")]
    BlobTooLarge(usize),
}

pub fn encode_blob_request(request: &BlobRequest) -> [u8; BLOB_REQUEST_BYTES] {
    let mut bytes = [0; BLOB_REQUEST_BYTES];
    bytes[..4].copy_from_slice(&[MAGIC, VERSION, GET, request.htl]);
    bytes[4..].copy_from_slice(&request.hash);
    bytes
}

pub fn decode_blob_request(bytes: &[u8]) -> Result<BlobRequest, BlobCodecError> {
    if bytes.len() != BLOB_REQUEST_BYTES {
        return Err(BlobCodecError::Invalid("request has the wrong length"));
    }
    if bytes[..3] != [MAGIC, VERSION, GET] {
        return Err(BlobCodecError::Invalid(
            "request has an unsupported prelude",
        ));
    }
    let mut hash = [0; 32];
    hash.copy_from_slice(&bytes[4..]);
    Ok(BlobRequest {
        hash,
        htl: bytes[3],
    })
}

pub fn encode_blob_reply_header(
    reply: &BlobReply,
) -> Result<[u8; BLOB_REPLY_HEADER_BYTES], BlobCodecError> {
    let (status, size) = match reply {
        BlobReply::NoResult => (0, 0),
        BlobReply::Data(data) => (DATA, data.len()),
    };
    if size > BLOB_MAX_BYTES {
        return Err(BlobCodecError::BlobTooLarge(size));
    }
    let mut header = [0; BLOB_REPLY_HEADER_BYTES];
    header[..3].copy_from_slice(&[MAGIC, VERSION, status]);
    header[3..].copy_from_slice(&(size as u32).to_be_bytes());
    Ok(header)
}

pub fn decode_blob_reply_header(bytes: &[u8]) -> Result<BlobReplyHeader, BlobCodecError> {
    if bytes.len() != BLOB_REPLY_HEADER_BYTES {
        return Err(BlobCodecError::Invalid(
            "response header has the wrong length",
        ));
    }
    if bytes[0] != MAGIC || bytes[1] != VERSION {
        return Err(BlobCodecError::Invalid(
            "response has an unsupported prelude",
        ));
    }
    let size = u32::from_be_bytes(bytes[3..].try_into().expect("four-byte length")) as usize;
    if size > BLOB_MAX_BYTES {
        return Err(BlobCodecError::BlobTooLarge(size));
    }
    match (bytes[2], size) {
        (0, 0) => Ok(BlobReplyHeader::NoResult),
        (0, _) => Err(BlobCodecError::Invalid(
            "NoResult response has a non-zero payload length",
        )),
        (DATA, size) => Ok(BlobReplyHeader::Data(size)),
        _ => Err(BlobCodecError::Invalid(
            "response has an unsupported status",
        )),
    }
}
