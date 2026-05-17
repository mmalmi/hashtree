use std::sync::OnceLock;
use tokio::sync::{Semaphore, SemaphorePermit};

pub(super) const BLOB_READ_BUSY: &str = "blob read queue is full";
const DEFAULT_MAX_CONCURRENT_BLOB_READS: usize = 32;
const MAX_CONCURRENT_BLOB_READS_ENV: &str = "HTREE_MAX_CONCURRENT_BLOB_READS";

fn blob_read_limiter() -> &'static Semaphore {
    static LIMITER: OnceLock<Semaphore> = OnceLock::new();
    LIMITER.get_or_init(|| Semaphore::new(max_concurrent_blob_reads()))
}

fn max_concurrent_blob_reads() -> usize {
    std::env::var(MAX_CONCURRENT_BLOB_READS_ENV)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MAX_CONCURRENT_BLOB_READS)
}

pub(super) fn try_acquire_blob_read() -> Result<SemaphorePermit<'static>, &'static str> {
    blob_read_limiter()
        .try_acquire()
        .map_err(|_| BLOB_READ_BUSY)
}
