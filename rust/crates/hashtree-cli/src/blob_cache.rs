use lru::LruCache;
use std::num::NonZeroUsize;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const DEFAULT_BLOB_BODY_CACHE_BYTES: usize = 128 * 1024 * 1024;
const DEFAULT_BLOB_BODY_CACHE_MAX_ITEM_BYTES: usize = 256 * 1024;
const DEFAULT_BLOB_SIZE_CACHE_ENTRIES: usize = 65_536;
const BLOB_BODY_CACHE_BYTES_ENV: &str = "HTREE_BLOB_BODY_CACHE_BYTES";
const BLOB_BODY_CACHE_MAX_ITEM_BYTES_ENV: &str = "HTREE_BLOB_BODY_CACHE_MAX_ITEM_BYTES";
const BLOB_SIZE_CACHE_ENTRIES_ENV: &str = "HTREE_BLOB_SIZE_CACHE_ENTRIES";
const BLOB_SIZE_CACHE_HIT_TTL: Duration = Duration::from_secs(3600);
const BLOB_SIZE_CACHE_MISS_TTL: Duration = Duration::from_secs(1);

#[derive(Clone)]
struct TimedValue<V> {
    value: V,
    expires_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlobSizeLookup {
    Hit(u64),
    Miss,
}

impl BlobSizeLookup {
    fn from_option(value: Option<u64>) -> Self {
        match value {
            Some(value) => Self::Hit(value),
            None => Self::Miss,
        }
    }

    fn into_option(self) -> Option<u64> {
        match self {
            Self::Hit(value) => Some(value),
            Self::Miss => None,
        }
    }

    fn ttl(self) -> Duration {
        match self {
            Self::Hit(_) => BLOB_SIZE_CACHE_HIT_TTL,
            Self::Miss => BLOB_SIZE_CACHE_MISS_TTL,
        }
    }
}

struct BlobSizeCache {
    cache: LruCache<String, TimedValue<BlobSizeLookup>>,
}

impl BlobSizeCache {
    fn new(capacity: usize) -> Self {
        Self {
            cache: LruCache::new(NonZeroUsize::new(capacity.max(1)).unwrap()),
        }
    }

    fn get(&mut self, key: &str) -> Option<Option<u64>> {
        let now = Instant::now();
        if let Some(entry) = self.cache.get(key) {
            if entry.expires_at > now {
                return Some(entry.value.into_option());
            }
        }
        self.cache.pop(key);
        None
    }

    fn put(&mut self, key: String, value: Option<u64>) {
        let value = BlobSizeLookup::from_option(value);
        self.cache.put(
            key,
            TimedValue {
                value,
                expires_at: Instant::now() + value.ttl(),
            },
        );
    }

    fn remove(&mut self, key: &str) {
        self.cache.pop(key);
    }
}

struct BlobBodyEntry {
    bytes: Vec<u8>,
}

struct BlobBodyCache {
    cache: LruCache<String, BlobBodyEntry>,
    current_bytes: usize,
    max_bytes: usize,
    max_item_bytes: usize,
}

impl BlobBodyCache {
    fn new(max_bytes: usize, max_item_bytes: usize) -> Self {
        Self {
            cache: LruCache::unbounded(),
            current_bytes: 0,
            max_bytes,
            max_item_bytes,
        }
    }

    fn get(&mut self, key: &str) -> Option<Vec<u8>> {
        self.cache.get(key).map(|entry| entry.bytes.clone())
    }

    fn put(&mut self, key: String, bytes: &[u8]) {
        if self.max_bytes == 0 || self.max_item_bytes == 0 || bytes.len() > self.max_item_bytes {
            return;
        }

        if let Some(previous) = self.cache.pop(&key) {
            self.current_bytes = self.current_bytes.saturating_sub(previous.bytes.len());
        }

        let entry = BlobBodyEntry {
            bytes: bytes.to_vec(),
        };
        self.current_bytes = self.current_bytes.saturating_add(entry.bytes.len());
        self.cache.put(key, entry);

        while self.current_bytes > self.max_bytes {
            let Some((_key, entry)) = self.cache.pop_lru() else {
                self.current_bytes = 0;
                break;
            };
            self.current_bytes = self.current_bytes.saturating_sub(entry.bytes.len());
        }
    }

    fn remove(&mut self, key: &str) {
        if let Some(entry) = self.cache.pop(key) {
            self.current_bytes = self.current_bytes.saturating_sub(entry.bytes.len());
        }
    }
}

pub(crate) struct BlobCache {
    bodies: Mutex<BlobBodyCache>,
    sizes: Mutex<BlobSizeCache>,
}

impl BlobCache {
    pub(crate) fn from_env() -> Self {
        Self::new(
            env_usize(BLOB_BODY_CACHE_BYTES_ENV, DEFAULT_BLOB_BODY_CACHE_BYTES),
            env_usize(
                BLOB_BODY_CACHE_MAX_ITEM_BYTES_ENV,
                DEFAULT_BLOB_BODY_CACHE_MAX_ITEM_BYTES,
            ),
            env_usize(BLOB_SIZE_CACHE_ENTRIES_ENV, DEFAULT_BLOB_SIZE_CACHE_ENTRIES),
        )
    }

    #[cfg(test)]
    pub(super) fn for_tests() -> Self {
        Self::new(
            DEFAULT_BLOB_BODY_CACHE_BYTES,
            DEFAULT_BLOB_BODY_CACHE_MAX_ITEM_BYTES,
            DEFAULT_BLOB_SIZE_CACHE_ENTRIES,
        )
    }

    fn new(body_max_bytes: usize, body_max_item_bytes: usize, size_entries: usize) -> Self {
        Self {
            bodies: Mutex::new(BlobBodyCache::new(body_max_bytes, body_max_item_bytes)),
            sizes: Mutex::new(BlobSizeCache::new(size_entries)),
        }
    }

    pub(crate) fn get_body(&self, hash_hex: &str) -> Option<Vec<u8>> {
        self.bodies
            .lock()
            .ok()
            .and_then(|mut cache| cache.get(hash_hex))
    }

    pub(crate) fn put_body(&self, hash_hex: String, bytes: &[u8]) {
        if let Ok(mut cache) = self.bodies.lock() {
            cache.put(hash_hex, bytes);
        }
    }

    pub(crate) fn get_size(&self, hash_hex: &str) -> Option<Option<u64>> {
        self.sizes
            .lock()
            .ok()
            .and_then(|mut cache| cache.get(hash_hex))
    }

    pub(crate) fn put_size(&self, hash_hex: String, size: Option<u64>) {
        if let Ok(mut cache) = self.sizes.lock() {
            cache.put(hash_hex, size);
        }
    }

    pub(crate) fn remove(&self, hash_hex: &str) {
        if let Ok(mut cache) = self.bodies.lock() {
            cache.remove(hash_hex);
        }
        if let Ok(mut cache) = self.sizes.lock() {
            cache.remove(hash_hex);
        }
    }
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_cache_respects_byte_limit() {
        let cache = BlobCache::new(5, 4, 8);

        cache.put_body("a".to_string(), b"1234");
        cache.put_body("b".to_string(), b"56");

        assert!(cache.get_body("a").is_none());
        assert_eq!(cache.get_body("b"), Some(b"56".to_vec()));
    }

    #[test]
    fn body_cache_skips_large_items() {
        let cache = BlobCache::new(16, 4, 8);

        cache.put_body("large".to_string(), b"12345");

        assert!(cache.get_body("large").is_none());
    }

    #[test]
    fn size_cache_keeps_hits_and_misses() {
        let cache = BlobCache::new(0, 0, 8);

        cache.put_size("hit".to_string(), Some(42));
        cache.put_size("miss".to_string(), None);

        assert_eq!(cache.get_size("hit"), Some(Some(42)));
        assert_eq!(cache.get_size("miss"), Some(None));
        assert_eq!(cache.get_size("unknown"), None);
    }
}
