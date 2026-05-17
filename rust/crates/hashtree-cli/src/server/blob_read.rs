use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub(super) const BLOB_READ_BUSY: &str = "blob read queue is full";
const DEFAULT_MAX_CONCURRENT_BLOB_READS: usize = 16;
const MAX_CONCURRENT_BLOB_READS_ENV: &str = "HTREE_MAX_CONCURRENT_BLOB_READS";
const DEFAULT_BLOB_READ_TIMEOUT_MS: u64 = 5_000;
const BLOB_READ_TIMEOUT_MS_ENV: &str = "HTREE_BLOB_READ_TIMEOUT_MS";
const DEFAULT_MAX_CONCURRENT_BLOB_WRITES: usize = 4;
const MAX_CONCURRENT_BLOB_WRITES_ENV: &str = "HTREE_MAX_CONCURRENT_BLOB_WRITES";

fn blob_read_limiter() -> &'static Arc<Semaphore> {
    static LIMITER: OnceLock<Arc<Semaphore>> = OnceLock::new();
    LIMITER.get_or_init(|| Arc::new(Semaphore::new(max_concurrent_blob_reads())))
}

fn blob_write_limiter() -> &'static Arc<Semaphore> {
    static LIMITER: OnceLock<Arc<Semaphore>> = OnceLock::new();
    LIMITER.get_or_init(|| Arc::new(Semaphore::new(max_concurrent_blob_writes())))
}

fn max_concurrent_blob_reads() -> usize {
    std::env::var(MAX_CONCURRENT_BLOB_READS_ENV)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MAX_CONCURRENT_BLOB_READS)
}

fn max_concurrent_blob_writes() -> usize {
    std::env::var(MAX_CONCURRENT_BLOB_WRITES_ENV)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MAX_CONCURRENT_BLOB_WRITES)
}

pub(super) fn try_acquire_blob_read() -> Result<OwnedSemaphorePermit, &'static str> {
    blob_read_limiter()
        .clone()
        .try_acquire_owned()
        .map_err(|_| BLOB_READ_BUSY)
}

pub(super) async fn acquire_blob_write() -> Result<OwnedSemaphorePermit, &'static str> {
    blob_write_limiter()
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| "blob write queue is closed")
}

pub(super) fn blob_read_timeout() -> Duration {
    let millis = std::env::var(BLOB_READ_TIMEOUT_MS_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_BLOB_READ_TIMEOUT_MS);
    Duration::from_millis(millis)
}
