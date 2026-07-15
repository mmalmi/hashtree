use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use hashtree_core::{
    decode_blob_reply_header, decode_blob_request, encode_blob_reply_header, encode_blob_request,
    BlobReply, BlobReplyHeader, BlobRequest, BlobRoute, Hash, Store, StoreBlobRoute, StoreError,
    BLOB_MAX_BYTES,
};
use sha2::Digest;

#[test]
fn compact_codec_carries_htl_once() {
    let request = BlobRequest {
        hash: std::array::from_fn(|index| index as u8),
        htl: 7,
    };

    let encoded = encode_blob_request(&request);
    assert_eq!(encoded.len(), 36);
    assert_eq!(
        hex::encode(encoded),
        "48010107000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"
    );
    assert_eq!(decode_blob_request(&encoded).unwrap(), request);

    let data = BlobReply::Data(vec![1, 2, 3]);
    assert_eq!(
        hex::encode(encode_blob_reply_header(&data).unwrap()),
        "48010100000003"
    );
    assert_eq!(
        decode_blob_reply_header(&[0x48, 1, 1, 0, 0, 0, 3]).unwrap(),
        BlobReplyHeader::Data(3)
    );
    assert_eq!(
        decode_blob_reply_header(&encode_blob_reply_header(&BlobReply::NoResult).unwrap()).unwrap(),
        BlobReplyHeader::NoResult
    );
}

#[test]
fn compact_codec_rejects_noncanonical_frames() {
    assert!(decode_blob_request(&[0; 35]).is_err());
    let mut request = encode_blob_request(&BlobRequest {
        hash: [0; 32],
        htl: 0,
    });
    request[0] = 0;
    assert!(decode_blob_request(&request).is_err());
    assert!(decode_blob_reply_header(&[0x48, 1, 0, 0, 0, 0, 1]).is_err());
    assert!(decode_blob_reply_header(&[0x48, 1, 2, 0, 0, 0, 0]).is_err());
    assert!(decode_blob_reply_header(&[0x48, 1, 1, 1, 0, 0, 1]).is_err());
    assert!(encode_blob_reply_header(&BlobReply::Data(vec![0; BLOB_MAX_BYTES + 1])).is_err());
}

#[tokio::test]
async fn terminal_store_route_ignores_htl_and_reads_once() {
    let data = b"one terminal lookup".to_vec();
    let hash: Hash = sha2::Sha256::digest(&data).into();
    let store = Arc::new(CountingStore {
        result: GetResult::Data(data),
        gets: AtomicUsize::new(0),
    });
    let route = StoreBlobRoute::new(store.clone());

    assert_eq!(
        route.route(BlobRequest { hash, htl: 99 }).await.unwrap(),
        BlobReply::Data(b"one terminal lookup".to_vec())
    );
    assert_eq!(store.gets.load(Ordering::Acquire), 1);

    let missing = StoreBlobRoute::new(Arc::new(CountingStore {
        result: GetResult::NoResult,
        gets: AtomicUsize::new(0),
    }));
    assert_eq!(
        missing.route(BlobRequest { hash, htl: 3 }).await.unwrap(),
        BlobReply::NoResult
    );
    let failing = StoreBlobRoute::new(Arc::new(CountingStore {
        result: GetResult::Error,
        gets: AtomicUsize::new(0),
    }));
    assert!(failing.route(BlobRequest { hash, htl: 3 }).await.is_err());
}

struct CountingStore {
    result: GetResult,
    gets: AtomicUsize,
}

enum GetResult {
    Data(Vec<u8>),
    NoResult,
    Error,
}

#[async_trait]
impl Store for CountingStore {
    async fn put(&self, _hash: Hash, _data: Vec<u8>) -> Result<bool, StoreError> {
        unreachable!()
    }

    async fn get(&self, _hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        self.gets.fetch_add(1, Ordering::AcqRel);
        match &self.result {
            GetResult::Data(data) => Ok(Some(data.clone())),
            GetResult::NoResult => Ok(None),
            GetResult::Error => Err(StoreError::Other("deliberate store failure".to_string())),
        }
    }

    async fn has(&self, _hash: &Hash) -> Result<bool, StoreError> {
        unreachable!()
    }

    async fn delete(&self, _hash: &Hash) -> Result<bool, StoreError> {
        unreachable!()
    }
}
