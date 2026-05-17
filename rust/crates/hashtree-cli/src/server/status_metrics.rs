use axum::{
    body::Body,
    http::{Request, StatusCode},
    middleware::Next,
    response::Response,
};
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex, OnceLock,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const HTTP_STATUS_BUCKETS: usize = 60;

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct StatusClassCounts {
    pub total: u64,
    pub status_1xx: u64,
    pub status_2xx: u64,
    pub status_3xx: u64,
    pub status_4xx: u64,
    pub status_5xx: u64,
    pub other: u64,
}

impl StatusClassCounts {
    fn add_assign(&mut self, other: StatusClassCounts) {
        self.total = self.total.saturating_add(other.total);
        self.status_1xx = self.status_1xx.saturating_add(other.status_1xx);
        self.status_2xx = self.status_2xx.saturating_add(other.status_2xx);
        self.status_3xx = self.status_3xx.saturating_add(other.status_3xx);
        self.status_4xx = self.status_4xx.saturating_add(other.status_4xx);
        self.status_5xx = self.status_5xx.saturating_add(other.status_5xx);
        self.other = self.other.saturating_add(other.other);
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct HttpStatusSnapshot {
    pub window_seconds: u64,
    pub recent: StatusClassCounts,
    pub lifetime: StatusClassCounts,
}

struct AtomicStatusClassCounts {
    total: AtomicU64,
    status_1xx: AtomicU64,
    status_2xx: AtomicU64,
    status_3xx: AtomicU64,
    status_4xx: AtomicU64,
    status_5xx: AtomicU64,
    other: AtomicU64,
}

impl AtomicStatusClassCounts {
    fn new() -> Self {
        Self {
            total: AtomicU64::new(0),
            status_1xx: AtomicU64::new(0),
            status_2xx: AtomicU64::new(0),
            status_3xx: AtomicU64::new(0),
            status_4xx: AtomicU64::new(0),
            status_5xx: AtomicU64::new(0),
            other: AtomicU64::new(0),
        }
    }

    fn increment(&self, status: StatusCode) {
        self.total.fetch_add(1, Ordering::Relaxed);
        match status.as_u16() {
            100..=199 => self.status_1xx.fetch_add(1, Ordering::Relaxed),
            200..=299 => self.status_2xx.fetch_add(1, Ordering::Relaxed),
            300..=399 => self.status_3xx.fetch_add(1, Ordering::Relaxed),
            400..=499 => self.status_4xx.fetch_add(1, Ordering::Relaxed),
            500..=599 => self.status_5xx.fetch_add(1, Ordering::Relaxed),
            _ => self.other.fetch_add(1, Ordering::Relaxed),
        };
    }

    fn reset(&self) {
        self.total.store(0, Ordering::Relaxed);
        self.status_1xx.store(0, Ordering::Relaxed);
        self.status_2xx.store(0, Ordering::Relaxed);
        self.status_3xx.store(0, Ordering::Relaxed);
        self.status_4xx.store(0, Ordering::Relaxed);
        self.status_5xx.store(0, Ordering::Relaxed);
        self.other.store(0, Ordering::Relaxed);
    }

    fn snapshot(&self) -> StatusClassCounts {
        StatusClassCounts {
            total: self.total.load(Ordering::Relaxed),
            status_1xx: self.status_1xx.load(Ordering::Relaxed),
            status_2xx: self.status_2xx.load(Ordering::Relaxed),
            status_3xx: self.status_3xx.load(Ordering::Relaxed),
            status_4xx: self.status_4xx.load(Ordering::Relaxed),
            status_5xx: self.status_5xx.load(Ordering::Relaxed),
            other: self.other.load(Ordering::Relaxed),
        }
    }
}

struct HttpStatusBucket {
    epoch_second: AtomicU64,
    counts: AtomicStatusClassCounts,
    reset_lock: Mutex<()>,
}

impl HttpStatusBucket {
    fn new() -> Self {
        Self {
            epoch_second: AtomicU64::new(0),
            counts: AtomicStatusClassCounts::new(),
            reset_lock: Mutex::new(()),
        }
    }
}

struct HttpStatusMetrics {
    buckets: [HttpStatusBucket; HTTP_STATUS_BUCKETS],
    lifetime: AtomicStatusClassCounts,
}

impl HttpStatusMetrics {
    fn new() -> Self {
        Self {
            buckets: std::array::from_fn(|_| HttpStatusBucket::new()),
            lifetime: AtomicStatusClassCounts::new(),
        }
    }

    fn record(&self, status: StatusCode) {
        let now = current_unix_secs();
        let bucket = &self.buckets[(now as usize) % HTTP_STATUS_BUCKETS];

        if bucket.epoch_second.load(Ordering::Acquire) != now {
            let _guard = bucket
                .reset_lock
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            if bucket.epoch_second.load(Ordering::Acquire) != now {
                bucket.counts.reset();
                bucket.epoch_second.store(now, Ordering::Release);
            }
        }

        bucket.counts.increment(status);
        self.lifetime.increment(status);
    }

    fn snapshot(&self) -> HttpStatusSnapshot {
        let now = current_unix_secs();
        let mut recent = StatusClassCounts::default();

        for bucket in &self.buckets {
            let epoch_second = bucket.epoch_second.load(Ordering::Acquire);
            if epoch_second <= now && now.saturating_sub(epoch_second) < HTTP_STATUS_BUCKETS as u64
            {
                recent.add_assign(bucket.counts.snapshot());
            }
        }

        HttpStatusSnapshot {
            window_seconds: HTTP_STATUS_BUCKETS as u64,
            recent,
            lifetime: self.lifetime.snapshot(),
        }
    }
}

fn metrics() -> &'static HttpStatusMetrics {
    static METRICS: OnceLock<HttpStatusMetrics> = OnceLock::new();
    METRICS.get_or_init(HttpStatusMetrics::new)
}

fn current_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

pub(super) async fn record_http_status(request: Request<Body>, next: Next) -> Response<Body> {
    let response = next.run(request).await;
    metrics().record(response.status());
    response
}

pub(super) fn http_status_snapshot() -> HttpStatusSnapshot {
    metrics().snapshot()
}

#[cfg(test)]
pub(super) fn record_http_status_for_test(status: StatusCode) {
    metrics().record(status);
}
