use super::TimedLruCache;
use std::hash::{Hash, Hasher};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

struct Key {
    id: u8,
    panic_on_drop: Option<Arc<AtomicBool>>,
}

impl Key {
    fn plain(id: u8) -> Self {
        Self {
            id,
            panic_on_drop: None,
        }
    }
}

impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for Key {}

impl Hash for Key {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        if let Some(armed) = &self.panic_on_drop {
            if armed.swap(false, Ordering::SeqCst) {
                panic!("stored cache key drop panic");
            }
        }
    }
}

#[test]
fn expired_cache_key_drop_panic_preserves_entries_and_eviction() {
    let armed = Arc::new(AtomicBool::new(true));
    let mut cache = TimedLruCache::new(2);
    let live_ttl = Duration::from_secs(60);
    cache.put(
        Key {
            id: 0,
            panic_on_drop: Some(Arc::clone(&armed)),
        },
        "expired",
        Duration::ZERO,
    );
    cache.put(Key::plain(1), "survivor", live_ttl);

    // Only the stored key panics; the equal lookup key and cleanup keys do not.
    let result = catch_unwind(AssertUnwindSafe(|| cache.get_cloned(&Key::plain(0))));
    assert!(result.is_err(), "expiry must drop the stored key");
    assert!(!armed.load(Ordering::SeqCst));

    // Continue through ordinary lookups, insertion and capacity eviction after
    // unwinding, so a dangling LRU link cannot hide behind a successful catch.
    assert_eq!(cache.get_cloned(&Key::plain(1)), Some("survivor"));
    cache.put(Key::plain(2), "second", live_ttl);
    cache.put(Key::plain(3), "third", live_ttl);
    assert_eq!(cache.get_cloned(&Key::plain(0)), None);
    assert_eq!(cache.get_cloned(&Key::plain(1)), None);
    assert_eq!(cache.get_cloned(&Key::plain(2)), Some("second"));
    assert_eq!(cache.get_cloned(&Key::plain(3)), Some("third"));
}
