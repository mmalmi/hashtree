use crate::runtime_config::runtime_capacity;
use std::fmt;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub(super) const BLOB_READ_BUSY: &str = "blob read queue is full";
pub(super) const BLOB_WRITE_BUSY: &str = "blob write queue is full";
const MAX_CONCURRENT_BLOB_READS_ENV: &str = "HTREE_MAX_CONCURRENT_BLOB_READS";
const DEFAULT_BLOB_READ_TIMEOUT_MS: u64 = 5_000;
const BLOB_READ_TIMEOUT_MS_ENV: &str = "HTREE_BLOB_READ_TIMEOUT_MS";
const DEFAULT_BLOB_READ_QUEUE_TIMEOUT_MS: u64 = 2_000;
const BLOB_READ_QUEUE_TIMEOUT_MS_ENV: &str = "HTREE_BLOB_READ_QUEUE_TIMEOUT_MS";
const MAX_CONCURRENT_BLOB_WRITES_ENV: &str = "HTREE_MAX_CONCURRENT_BLOB_WRITES";
const DEFAULT_BLOB_WRITE_QUEUE_TIMEOUT_MS: u64 = 30_000;
const BLOB_WRITE_QUEUE_TIMEOUT_MS_ENV: &str = "HTREE_BLOB_WRITE_QUEUE_TIMEOUT_MS";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct BlobIoLimits {
    pub blocking_threads: usize,
    pub reserved_blocking_threads: usize,
    pub total_limit: usize,
    pub bulk_limit: usize,
    pub read_limit: usize,
    pub metadata_read_limit: usize,
    pub data_read_limit: usize,
    pub write_limit: usize,
}

impl BlobIoLimits {
    /// Derive safe server storage concurrency from the process-wide blocking
    /// pool. Legacy environment values are ceilings, not requests to consume
    /// the whole pool. This preserves headroom for Nostr, status, and other
    /// blocking work even when an operator carries an old oversized override.
    pub(super) fn derive(
        blocking_threads: usize,
        cpu_parallelism: usize,
        read_ceiling: Option<usize>,
        write_ceiling: Option<usize>,
    ) -> Self {
        let blocking_threads = blocking_threads.max(4);
        let cpu_parallelism = cpu_parallelism.max(1);
        let reserved_blocking_threads = (blocking_threads / 4).clamp(2, 16);
        let blocking_capacity = blocking_threads
            .saturating_sub(reserved_blocking_threads)
            .max(2);
        // Scale with the machine, but never admit more storage work than two
        // jobs per available CPU or the blocking pool can safely isolate.
        // Writes get at most one quarter of that budget; reads use the rest.
        // This keeps small machines responsive without imposing an arbitrary
        // ceiling on larger hosts.
        let storage_capacity = blocking_capacity
            .min(cpu_parallelism.saturating_mul(2).max(2))
            .max(2);
        let automatic_writes = cpu_parallelism
            .div_ceil(4)
            .max(1)
            .min((storage_capacity / 4).max(1));
        let automatic_reads = storage_capacity.saturating_sub(automatic_writes).max(1);
        let mut read_limit = read_ceiling
            .filter(|value| *value > 0)
            .unwrap_or(automatic_reads)
            .min(automatic_reads);
        let mut write_limit = write_ceiling
            .filter(|value| *value > 0)
            .unwrap_or(automatic_writes)
            .min(automatic_writes);

        if read_limit.saturating_add(write_limit) > storage_capacity {
            write_limit = write_limit.min(storage_capacity.saturating_sub(1).max(1));
            read_limit = read_limit.min(storage_capacity.saturating_sub(write_limit).max(1));
        }

        let metadata_read_limit = read_limit.div_ceil(4).clamp(1, 4).min(read_limit);
        let data_read_limit = read_limit.saturating_sub(metadata_read_limit);
        let bulk_limit = data_read_limit.saturating_add(write_limit);
        let total_limit = metadata_read_limit.saturating_add(bulk_limit);

        Self {
            blocking_threads,
            reserved_blocking_threads,
            total_limit,
            bulk_limit,
            read_limit,
            metadata_read_limit,
            data_read_limit,
            write_limit,
        }
    }

    fn from_process() -> Self {
        let capacity = runtime_capacity();
        Self::derive(
            capacity.max_blocking_threads,
            capacity.cpu_parallelism,
            env_positive_usize(MAX_CONCURRENT_BLOB_READS_ENV),
            env_positive_usize(MAX_CONCURRENT_BLOB_WRITES_ENV),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlobIoClass {
    MetadataRead,
    DataRead,
    Write,
}

#[derive(Debug)]
pub(super) enum BlobIoTaskError {
    Busy(&'static str),
    TimedOut(&'static str),
    Join(tokio::task::JoinError),
}

impl BlobIoTaskError {
    pub(super) fn is_busy(&self) -> bool {
        matches!(self, Self::Busy(_))
    }

    pub(super) fn is_timeout(&self) -> bool {
        matches!(self, Self::TimedOut(_))
    }
}

impl fmt::Display for BlobIoTaskError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy(reason) | Self::TimedOut(reason) => formatter.write_str(reason),
            Self::Join(error) => write!(formatter, "blocking storage task failed: {error}"),
        }
    }
}

impl std::error::Error for BlobIoTaskError {}

struct BlobIoPermits {
    _class: OwnedSemaphorePermit,
    _bulk: Option<OwnedSemaphorePermit>,
    _total: OwnedSemaphorePermit,
}

pub(super) struct BlobIoAdmission {
    limits: BlobIoLimits,
    total: Arc<Semaphore>,
    bulk: Arc<Semaphore>,
    metadata_reads: Arc<Semaphore>,
    data_reads: Arc<Semaphore>,
    writes: Arc<Semaphore>,
    read_queue_timeout: Duration,
    read_task_timeout: Duration,
    write_queue_timeout: Duration,
}

impl BlobIoAdmission {
    fn new(
        limits: BlobIoLimits,
        read_queue_timeout: Duration,
        read_task_timeout: Duration,
        write_queue_timeout: Duration,
    ) -> Self {
        Self {
            total: Arc::new(Semaphore::new(limits.total_limit)),
            bulk: Arc::new(Semaphore::new(limits.bulk_limit)),
            metadata_reads: Arc::new(Semaphore::new(limits.metadata_read_limit)),
            data_reads: Arc::new(Semaphore::new(limits.data_read_limit)),
            writes: Arc::new(Semaphore::new(limits.write_limit)),
            limits,
            read_queue_timeout,
            read_task_timeout,
            write_queue_timeout,
        }
    }

    fn from_process() -> Self {
        Self::new(
            BlobIoLimits::from_process(),
            duration_from_env(
                BLOB_READ_QUEUE_TIMEOUT_MS_ENV,
                DEFAULT_BLOB_READ_QUEUE_TIMEOUT_MS,
            ),
            duration_from_env(BLOB_READ_TIMEOUT_MS_ENV, DEFAULT_BLOB_READ_TIMEOUT_MS),
            duration_from_env(
                BLOB_WRITE_QUEUE_TIMEOUT_MS_ENV,
                DEFAULT_BLOB_WRITE_QUEUE_TIMEOUT_MS,
            ),
        )
    }

    #[cfg(test)]
    pub(super) fn new_for_test(
        limits: BlobIoLimits,
        read_queue_timeout: Duration,
        read_task_timeout: Duration,
        write_queue_timeout: Duration,
    ) -> Self {
        Self::new(
            limits,
            read_queue_timeout,
            read_task_timeout,
            write_queue_timeout,
        )
    }

    async fn acquire(&self, class: BlobIoClass) -> Result<BlobIoPermits, BlobIoTaskError> {
        let queue_timeout = match class {
            BlobIoClass::Write => self.write_queue_timeout,
            BlobIoClass::MetadataRead | BlobIoClass::DataRead => self.read_queue_timeout,
        };
        let busy = match class {
            BlobIoClass::Write => BLOB_WRITE_BUSY,
            BlobIoClass::MetadataRead | BlobIoClass::DataRead => BLOB_READ_BUSY,
        };

        let acquire = async {
            let class_permit = match class {
                BlobIoClass::MetadataRead => self.metadata_reads.clone().acquire_owned().await,
                BlobIoClass::DataRead => self.data_reads.clone().acquire_owned().await,
                BlobIoClass::Write => self.writes.clone().acquire_owned().await,
            }
            .map_err(|_| BlobIoTaskError::Busy(busy))?;
            let bulk_permit = match class {
                BlobIoClass::MetadataRead => None,
                BlobIoClass::DataRead | BlobIoClass::Write => Some(
                    self.bulk
                        .clone()
                        .acquire_owned()
                        .await
                        .map_err(|_| BlobIoTaskError::Busy(busy))?,
                ),
            };
            let total_permit = self
                .total
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| BlobIoTaskError::Busy(busy))?;
            Ok(BlobIoPermits {
                _class: class_permit,
                _bulk: bulk_permit,
                _total: total_permit,
            })
        };

        match tokio::time::timeout(queue_timeout, acquire).await {
            Ok(result) => result,
            Err(_) => Err(BlobIoTaskError::Busy(busy)),
        }
    }

    async fn run_read<F, T>(&self, class: BlobIoClass, task: F) -> Result<T, BlobIoTaskError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let permits = self.acquire(class).await?;
        let task = tokio::task::spawn_blocking(move || {
            // spawn_blocking work cannot be cancelled. Keeping the complete
            // permit bundle inside the closure means an HTTP timeout or caller
            // cancellation cannot admit replacement work while this disk I/O
            // is still running.
            let _permits = permits;
            task()
        });
        match tokio::time::timeout(self.read_task_timeout, task).await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(error)) => Err(BlobIoTaskError::Join(error)),
            Err(_) => Err(BlobIoTaskError::TimedOut("blob read timed out")),
        }
    }

    pub(super) async fn run_metadata_read<F, T>(&self, task: F) -> Result<T, BlobIoTaskError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        self.run_read(BlobIoClass::MetadataRead, task).await
    }

    pub(super) async fn run_data_read<F, T>(&self, task: F) -> Result<T, BlobIoTaskError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        // A legacy read ceiling of one cannot provide distinct metadata and
        // body lanes. Share that single admitted lane rather than turning all
        // body reads into permanent queue timeouts. At normal automatically
        // derived limits, data reads remain capped below the metadata reserve.
        let class = if self.limits.data_read_limit == 0 {
            BlobIoClass::MetadataRead
        } else {
            BlobIoClass::DataRead
        };
        self.run_read(class, task).await
    }

    pub(super) async fn run_write<F, T>(&self, task: F) -> Result<T, BlobIoTaskError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let permits = self.acquire(BlobIoClass::Write).await?;
        tokio::task::spawn_blocking(move || {
            let _permits = permits;
            task()
        })
        .await
        .map_err(BlobIoTaskError::Join)
    }

    fn snapshot(&self) -> BlobIoQueueSnapshot {
        let metadata_read_available = self
            .metadata_reads
            .available_permits()
            .min(self.limits.metadata_read_limit);
        let data_read_available = self
            .data_reads
            .available_permits()
            .min(self.limits.data_read_limit);
        let read_available = metadata_read_available.saturating_add(data_read_available);
        let write_available = self.writes.available_permits().min(self.limits.write_limit);
        let total_available = self.total.available_permits().min(self.limits.total_limit);

        BlobIoQueueSnapshot {
            blocking_threads: self.limits.blocking_threads,
            reserved_blocking_threads: self.limits.reserved_blocking_threads,
            total_limit: self.limits.total_limit,
            total_available,
            total_in_use: self.limits.total_limit.saturating_sub(total_available),
            read_limit: self.limits.read_limit,
            read_available,
            read_in_use: self.limits.read_limit.saturating_sub(read_available),
            metadata_read_limit: self.limits.metadata_read_limit,
            metadata_read_available,
            metadata_read_in_use: self
                .limits
                .metadata_read_limit
                .saturating_sub(metadata_read_available),
            write_limit: self.limits.write_limit,
            write_available,
            write_in_use: self.limits.write_limit.saturating_sub(write_available),
            write_queue_timeout_ms: duration_millis_u64(self.write_queue_timeout),
            read_queue_timeout_ms: duration_millis_u64(self.read_queue_timeout),
            read_task_timeout_ms: duration_millis_u64(self.read_task_timeout),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct BlobIoQueueSnapshot {
    pub blocking_threads: usize,
    pub reserved_blocking_threads: usize,
    pub total_limit: usize,
    pub total_available: usize,
    pub total_in_use: usize,
    pub read_limit: usize,
    pub read_available: usize,
    pub read_in_use: usize,
    pub metadata_read_limit: usize,
    pub metadata_read_available: usize,
    pub metadata_read_in_use: usize,
    pub write_limit: usize,
    pub write_available: usize,
    pub write_in_use: usize,
    pub write_queue_timeout_ms: u64,
    pub read_queue_timeout_ms: u64,
    pub read_task_timeout_ms: u64,
}

fn admission() -> &'static BlobIoAdmission {
    static ADMISSION: OnceLock<BlobIoAdmission> = OnceLock::new();
    ADMISSION.get_or_init(BlobIoAdmission::from_process)
}

pub(super) async fn run_blob_metadata_read<F, T>(task: F) -> Result<T, BlobIoTaskError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    admission().run_metadata_read(task).await
}

pub(super) async fn run_blob_read<F, T>(task: F) -> Result<T, BlobIoTaskError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    admission().run_data_read(task).await
}

pub(super) async fn run_blob_write<F, T>(task: F) -> Result<T, BlobIoTaskError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    admission().run_write(task).await
}

pub(super) fn blob_io_queue_snapshot() -> BlobIoQueueSnapshot {
    admission().snapshot()
}

fn env_positive_usize(name: &str) -> Option<usize> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
}

fn duration_from_env(name: &str, default_millis: u64) -> Duration {
    let millis = std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default_millis);
    Duration::from_millis(millis)
}

fn duration_millis_u64(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
#[path = "blob_read_overload_tests.rs"]
mod overload_tests;
