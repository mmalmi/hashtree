use nostr::Filter;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const ALLOCATION_TRIM_MIN_INTERVAL_MS: u64 = 30_000;

#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessMemorySnapshot {
    pub vm_rss_kb: Option<u64>,
    pub rss_anon_kb: Option<u64>,
    pub threads: Option<u64>,
}

#[cfg(target_os = "linux")]
pub fn process_memory_snapshot() -> ProcessMemorySnapshot {
    let mut snapshot = ProcessMemorySnapshot::default();

    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if let Some(value) = line.strip_prefix("VmRSS:") {
                snapshot.vm_rss_kb = parse_status_kb(value);
            } else if let Some(value) = line.strip_prefix("RssAnon:") {
                snapshot.rss_anon_kb = parse_status_kb(value);
            } else if let Some(value) = line.strip_prefix("Threads:") {
                snapshot.threads = value.trim().parse::<u64>().ok();
            }
        }
    }

    snapshot
}

#[cfg(not(target_os = "linux"))]
pub fn process_memory_snapshot() -> ProcessMemorySnapshot {
    ProcessMemorySnapshot::default()
}

pub fn nostr_filter_summary(filter: &Filter) -> String {
    let generic_tag_values = filter
        .generic_tags
        .values()
        .map(|values| values.len())
        .sum::<usize>();
    format!(
        "ids={} authors={} kinds={} tags={} tag_values={} search={} since={} until={} limit={}",
        filter.ids.as_ref().map(|ids| ids.len()).unwrap_or(0),
        filter
            .authors
            .as_ref()
            .map(|authors| authors.len())
            .unwrap_or(0),
        filter.kinds.as_ref().map(|kinds| kinds.len()).unwrap_or(0),
        filter.generic_tags.len(),
        generic_tag_values,
        filter.search.is_some(),
        filter.since.is_some(),
        filter.until.is_some(),
        filter
            .limit
            .map(|limit| limit.to_string())
            .unwrap_or_else(|| "none".to_string()),
    )
}

pub fn nostr_filters_summary(filters: &[Filter]) -> String {
    const MAX_FILTERS_IN_LOG: usize = 4;
    let mut summaries = filters
        .iter()
        .take(MAX_FILTERS_IN_LOG)
        .map(nostr_filter_summary)
        .collect::<Vec<_>>();
    if filters.len() > MAX_FILTERS_IN_LOG {
        summaries.push(format!(
            "...+{} filters",
            filters.len() - MAX_FILTERS_IN_LOG
        ));
    }
    summaries.join("; ")
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn trim_process_allocations() {
    // SAFETY: malloc_trim asks glibc to return free heap pages to the OS.
    unsafe {
        libc::malloc_trim(0);
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub fn trim_process_allocations() {}

fn should_trim_now(last_trim_ms: &AtomicU64, elapsed_ms: u64, min_interval_ms: u64) -> bool {
    let now_token = elapsed_ms.saturating_add(1);
    let mut prev = last_trim_ms.load(Ordering::Relaxed);
    loop {
        if prev != 0 && now_token.saturating_sub(prev) < min_interval_ms {
            return false;
        }
        match last_trim_ms.compare_exchange_weak(
            prev,
            now_token,
            Ordering::AcqRel,
            Ordering::Relaxed,
        ) {
            Ok(_) => return true,
            Err(current) => prev = current,
        }
    }
}

/// Rate-limited variant of [`trim_process_allocations`] for hot request paths
/// so concurrent queries do not repeatedly lock every glibc arena.
pub fn trim_process_allocations_throttled() {
    static START: OnceLock<Instant> = OnceLock::new();
    static LAST_TRIM_MS: AtomicU64 = AtomicU64::new(0);

    let elapsed_ms = START
        .get_or_init(Instant::now)
        .elapsed()
        .min(Duration::from_millis(u64::MAX - 1))
        .as_millis() as u64;
    if should_trim_now(&LAST_TRIM_MS, elapsed_ms, ALLOCATION_TRIM_MIN_INTERVAL_MS) {
        trim_process_allocations();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn throttled_trim_enforces_minimum_interval() {
        let last = AtomicU64::new(0);
        assert!(should_trim_now(&last, 0, 30_000));
        assert!(!should_trim_now(&last, 5_000, 30_000));
        assert!(!should_trim_now(&last, 29_999, 30_000));
        assert!(should_trim_now(&last, 30_000, 30_000));
        assert!(!should_trim_now(&last, 45_000, 30_000));
        assert!(should_trim_now(&last, 60_000, 30_000));
    }
}

#[cfg(target_os = "linux")]
fn parse_status_kb(value: &str) -> Option<u64> {
    value.split_whitespace().next()?.parse::<u64>().ok()
}
