use std::sync::OnceLock;
use tokio::sync::{Semaphore, SemaphorePermit};

pub(super) const BLOB_READ_BUSY: &str = "blob read queue is full";
const MAX_CONCURRENT_BLOB_READS: usize = 8;

fn blob_read_limiter() -> &'static Semaphore {
    static LIMITER: OnceLock<Semaphore> = OnceLock::new();
    LIMITER.get_or_init(|| Semaphore::new(MAX_CONCURRENT_BLOB_READS))
}

pub(super) fn try_acquire_blob_read() -> Result<SemaphorePermit<'static>, &'static str> {
    blob_read_limiter()
        .try_acquire()
        .map_err(|_| BLOB_READ_BUSY)
}
