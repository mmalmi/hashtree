use super::{BlossomError, BLOB_BATCH_DOWNLOAD_MAGIC};
use futures::io::{self, AsyncRead, AsyncReadExt};
use futures::{stream, TryStreamExt};
use sha2::{Digest, Sha256};
use std::collections::HashSet;

pub(super) const DEFAULT_MAX_BYTES: usize = 64 * 1024 * 1024;
pub(super) const MAX_BYTES: usize = 512 * 1024 * 1024;
const HEADER_BYTES: usize = 12;
const ENTRY_HEADER_BYTES: usize = 40;
const MAX_ENTRIES: usize = 16_384; // Largest request accepted by the daemon.
type Hash = [u8; 32];

fn invalid(message: &str) -> BlossomError {
    BlossomError::DownloadFailed(message.into())
}

fn body_error(error: std::io::Error) -> BlossomError {
    // A transport failure can be retried by the caller. A clean EOF in the
    // middle of a declared frame is a protocol error.
    match error
        .into_inner()
        .and_then(|e| e.downcast::<reqwest::Error>().ok())
    {
        Some(error) => BlossomError::Http(*error),
        None => invalid("truncated batch download frame"),
    }
}

pub(super) async fn read_response(
    response: reqwest::Response,
    requested: &HashSet<Hash>,
    limit: usize,
) -> Result<Vec<(Hash, Vec<u8>)>, BlossomError> {
    let bound = match response.content_length() {
        Some(bytes) if bytes > limit as u64 => {
            return Err(invalid("batch download exceeds byte limit"))
        }
        Some(bytes) => bytes as usize,
        None => limit,
    };
    let chunks = stream::try_unfold(response, |mut response| async move {
        let next = response.chunk().await.map_err(std::io::Error::other)?;
        Ok::<_, std::io::Error>(next.map(|chunk| (chunk, response)))
    });
    let input = chunks.into_async_read();
    futures::pin_mut!(input);
    read_entries(&mut input, requested, bound).await
}

async fn read_entries(
    input: &mut (impl AsyncRead + Unpin),
    requested: &HashSet<Hash>,
    bound: usize,
) -> Result<Vec<(Hash, Vec<u8>)>, BlossomError> {
    let after_header = bound
        .checked_sub(HEADER_BYTES)
        .ok_or_else(|| invalid("batch download header exceeds byte limit"))?;
    let mut header = [0; HEADER_BYTES];
    input.read_exact(&mut header).await.map_err(body_error)?;
    if &header[..8] != BLOB_BATCH_DOWNLOAD_MAGIC {
        return Err(invalid("invalid batch response magic"));
    }
    let count = u32::from_be_bytes(header[8..].try_into().unwrap()) as usize;
    if count > MAX_ENTRIES {
        return Err(invalid("too many batch download entries"));
    }
    let mut payload_left = after_header
        .checked_sub(count * ENTRY_HEADER_BYTES)
        .ok_or_else(|| invalid("batch download headers exceed byte limit"))?;
    let mut items = Vec::new();
    let mut seen = HashSet::new();
    for _ in 0..count {
        let mut entry = [0; ENTRY_HEADER_BYTES];
        input.read_exact(&mut entry).await.map_err(body_error)?;
        let hash: Hash = entry[..32].try_into().unwrap();
        let size = u64::from_be_bytes(entry[32..].try_into().unwrap());
        let len = usize::try_from(size).map_err(|_| invalid("batch entry length too large"))?;
        payload_left = payload_left
            .checked_sub(len)
            .ok_or_else(|| invalid("batch download exceeds byte limit"))?;
        if !requested.contains(&hash) || !seen.insert(hash) {
            let copied = io::copy((&mut *input).take(size), &mut io::sink())
                .await
                .map_err(body_error)?;
            if copied != size {
                return Err(invalid("truncated batch download frame"));
            }
            continue;
        }
        let mut data = vec![0; len];
        input.read_exact(&mut data).await.map_err(body_error)?;
        let actual: Hash = Sha256::digest(&data).into();
        if actual != hash {
            return Err(BlossomError::HashMismatch {
                expected: hex::encode(hash),
                actual: hex::encode(actual),
            });
        }
        items.push((hash, data));
    }
    if input.read(&mut [0]).await.map_err(body_error)? != 0 {
        return Err(invalid("batch response has trailing bytes"));
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::io::Cursor;

    fn frame(data: &[u8], hash: Hash, size: u64) -> Vec<u8> {
        let mut frame = BLOB_BATCH_DOWNLOAD_MAGIC.to_vec();
        frame.extend_from_slice(&1u32.to_be_bytes());
        frame.extend_from_slice(&hash);
        frame.extend_from_slice(&size.to_be_bytes());
        frame.extend_from_slice(data);
        frame
    }

    #[tokio::test]
    async fn checks_budget_including_headers_without_content_length() {
        let data = b"ordinary batch bytes";
        let hash = Sha256::digest(data).into();
        let requested = HashSet::from([hash]);
        let body = frame(data, hash, data.len() as u64);
        assert_eq!(
            read_entries(&mut Cursor::new(&body), &requested, body.len())
                .await
                .unwrap(),
            vec![(hash, data.to_vec())]
        );
        for limit in [0, HEADER_BYTES, body.len() - 1] {
            assert!(read_entries(&mut Cursor::new(&body), &requested, limit)
                .await
                .is_err());
        }
    }

    #[tokio::test]
    async fn rejects_incomplete_frames_invalid_lengths_and_hash_mismatches() {
        let data = b"verified batch fragment";
        let hash = Sha256::digest(data).into();
        let requested = HashSet::from([hash]);
        let body = frame(data, hash, data.len() as u64);
        for len in [8, HEADER_BYTES + 3, body.len() - 1] {
            let cut = &body[..len];
            assert!(
                read_entries(&mut Cursor::new(cut), &requested, DEFAULT_MAX_BYTES)
                    .await
                    .is_err()
            );
        }
        let invalid_size = frame(&[], hash, u64::MAX);
        assert!(read_entries(
            &mut Cursor::new(invalid_size),
            &requested,
            DEFAULT_MAX_BYTES
        )
        .await
        .is_err());
        let mut trailing = body.clone();
        trailing.push(0);
        assert!(
            read_entries(&mut Cursor::new(trailing), &requested, DEFAULT_MAX_BYTES)
                .await
                .is_err()
        );
        let mut changed = body;
        *changed.last_mut().unwrap() ^= 1;
        assert!(matches!(
            read_entries(&mut Cursor::new(changed), &requested, DEFAULT_MAX_BYTES).await,
            Err(BlossomError::HashMismatch { .. })
        ));
    }
}
