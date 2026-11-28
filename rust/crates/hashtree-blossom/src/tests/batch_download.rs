use super::*;
use std::io::Result as IoResult;
use std::sync::Barrier;
use std::time::Instant;

#[derive(Clone, Copy)]
struct Blob {
    byte: u8,
    len: usize,
    hash: [u8; 32],
}

impl Blob {
    fn new(byte: u8, len: usize) -> Self {
        let mut hasher = Sha256::new();
        let mut blob = Self {
            byte,
            len,
            hash: [0; 32],
        };
        blob.chunks(|bytes| {
            hasher.update(bytes);
            Ok(())
        })
        .unwrap();
        blob.hash = hasher.finalize().into();
        blob
    }

    fn chunks(&self, mut emit: impl FnMut(&[u8]) -> IoResult<()>) -> IoResult<()> {
        let block = [self.byte; 16 * 1024];
        let mut remaining = self.len;
        while remaining > 0 {
            let n = remaining.min(block.len());
            emit(&block[..n])?;
            remaining -= n;
        }
        Ok(())
    }
}

// Keep the server's footprint constant so process peak RSS measures the client.
fn serve_batch(blobs: Vec<Blob>, chunked: bool) -> (String, thread::JoinHandle<IoResult<()>>) {
    serve_batch_with_barrier(blobs, chunked, None)
}

fn serve_batch_with_barrier(
    blobs: Vec<Blob>,
    chunked: bool,
    requests_ready: Option<Arc<Barrier>>,
) -> (String, thread::JoinHandle<IoResult<()>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept()?;
        drain_http_request(&mut socket, &mut [0; 8192]);
        if let Some(requests_ready) = requests_ready {
            requests_ready.wait();
        }
        let wire_len = 12 + blobs.iter().map(|b| 40 + b.len).sum::<usize>();
        write!(socket, "HTTP/1.1 200 OK\r\nConnection: close\r\n")?;
        if chunked {
            write!(socket, "Transfer-Encoding: chunked\r\n\r\n")?;
        } else {
            write!(socket, "Content-Length: {wire_len}\r\n\r\n")?;
        }
        {
            let mut send = |data: &[u8]| -> IoResult<()> {
                if chunked {
                    write!(socket, "{:x}\r\n", data.len())?;
                }
                socket.write_all(data)?;
                if chunked {
                    socket.write_all(b"\r\n")?;
                }
                Ok(())
            };
            send(BLOB_BATCH_DOWNLOAD_MAGIC)?;
            send(&(blobs.len() as u32).to_be_bytes())?;
            for blob in blobs {
                send(&blob.hash)?;
                send(&(blob.len as u64).to_be_bytes())?;
                blob.chunks(&mut send)?;
            }
        }
        if chunked {
            socket.write_all(b"0\r\n\r\n")?;
        }
        Ok(())
    });
    (format!("http://{addr}"), server)
}

#[tokio::test]
async fn mixed_size_chunked_response_preserves_only_requested_blobs() {
    let empty = Blob::new(1, 0);
    let small = Blob::new(2, 37);
    let large = Blob::new(3, 3 * 1024 * 1024 + 7);
    let extra = Blob::new(4, 16 * 1024 + 2);
    let missing = Blob::new(5, 1);
    let (url, server) = serve_batch(vec![empty, extra, large, large, small], true);
    let hashes = [small, empty, large, missing].map(|b| hex::encode(b.hash));
    let client = BlossomClient::new_empty(Keys::generate()).with_read_servers(vec![url]);
    let results = client.download_batch(&hashes).await.unwrap().unwrap();
    server.join().unwrap().unwrap();
    assert_eq!(results.len(), 3);
    for ((hash, data), blob) in results.iter().zip([empty, large, small]) {
        assert_eq!(*hash, blob.hash);
        assert_eq!(data.len(), blob.len);
        assert!(data.iter().all(|b| *b == blob.byte));
    }
}

#[tokio::test]
#[ignore = "use a fresh process for comparable peak-RSS measurements"]
async fn measure_mixed_batch_download() {
    let sizes = [
        79,
        512 * 1024,
        8 * 1024 * 1024,
        24 * 1024 * 1024,
        12 * 1024 * 1024 + 19,
    ];
    let blobs: Vec<_> = sizes
        .iter()
        .enumerate()
        .map(|(i, &len)| Blob::new(i as u8, len))
        .collect();
    let hashes: Vec<_> = blobs.iter().map(|b| hex::encode(b.hash)).collect();
    let expected_bytes: usize = sizes.iter().sum();
    let (url, server) = serve_batch(blobs, false);
    let client = BlossomClient::new_empty(Keys::generate()).with_read_servers(vec![url]);
    let start = Instant::now();
    let results = client.download_batch(&hashes).await.unwrap().unwrap();
    let elapsed = start.elapsed();
    server.join().unwrap().unwrap();
    assert_eq!(results.len(), sizes.len());
    assert_eq!(
        results.iter().map(|(_, data)| data.len()).sum::<usize>(),
        expected_bytes
    );
    assert!(results
        .iter()
        .zip(hashes)
        .all(|((hash, _), want)| hex::encode(hash) == want));
    eprintln!(
        "batch_download frames={} wire_bytes={} payload_bytes={expected_bytes} elapsed_us={}",
        sizes.len(),
        12 + sizes.len() * 40 + expected_bytes,
        elapsed.as_micros()
    );
}

#[tokio::test]
#[ignore = "use a fresh process for comparable peak-RSS measurements"]
async fn measure_four_concurrent_mixed_batch_downloads() {
    use futures::stream::{FuturesUnordered, StreamExt};

    const REQUESTS: usize = 4;
    const FRAMES: usize = 64;
    // Repeated eight times, these sizes leave room for framing under 8 MiB.
    let sizes_kib = [1, 3, 8, 24, 61, 160, 254, 512];
    let requests_ready = Arc::new(Barrier::new(REQUESTS));
    let mut downloads = FuturesUnordered::new();
    let mut expected_total = 0;
    for batch in 0..REQUESTS {
        let blobs: Vec<_> = (0..FRAMES)
            .map(|i| {
                Blob::new(
                    (batch * FRAMES + i) as u8,
                    sizes_kib[i % sizes_kib.len()] * 1024,
                )
            })
            .collect();
        let expected_bytes: usize = blobs.iter().map(|blob| blob.len).sum();
        assert!(12 + FRAMES * 40 + expected_bytes <= 8 * 1024 * 1024);
        expected_total += expected_bytes;
        let hashes: Vec<_> = blobs.iter().map(|b| hex::encode(b.hash)).collect();
        let (url, server) =
            serve_batch_with_barrier(blobs.clone(), false, Some(requests_ready.clone()));
        let client = BlossomClient::new_empty(Keys::generate()).with_read_servers(vec![url]);
        downloads.push(async move {
            let results = client.download_batch(&hashes).await.unwrap().unwrap();
            server.join().unwrap().unwrap();
            (blobs, results)
        });
    }

    // Consume each completed batch before polling for the next, as the callers do.
    let start = Instant::now();
    let mut verified_bytes = 0;
    while let Some((blobs, results)) = downloads.next().await {
        assert_eq!(results.len(), FRAMES);
        for (blob, (hash, data)) in blobs.into_iter().zip(results) {
            assert_eq!(hash, blob.hash);
            assert_eq!(data.len(), blob.len);
            let expected = [blob.byte; 16 * 1024];
            assert!(data
                .chunks(expected.len())
                .all(|chunk| chunk == &expected[..chunk.len()]));
            verified_bytes += data.len();
        }
    }
    let elapsed = start.elapsed();
    assert_eq!(verified_bytes, expected_total);
    eprintln!(
        "batch_download requests={REQUESTS} frames={} wire_bytes={} payload_bytes={verified_bytes} elapsed_us={}",
        REQUESTS * FRAMES,
        REQUESTS * (12 + FRAMES * 40) + verified_bytes,
        elapsed.as_micros()
    );
}
