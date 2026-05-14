use nostr::Filter;

#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessMemorySnapshot {
    pub vm_rss_kb: Option<u64>,
    pub rss_anon_kb: Option<u64>,
    pub anonymous_kb: Option<u64>,
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

    if let Ok(rollup) = std::fs::read_to_string("/proc/self/smaps_rollup") {
        for line in rollup.lines() {
            if let Some(value) = line.strip_prefix("Anonymous:") {
                snapshot.anonymous_kb = parse_status_kb(value);
                break;
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

#[cfg(target_os = "linux")]
fn parse_status_kb(value: &str) -> Option<u64> {
    value.split_whitespace().next()?.parse::<u64>().ok()
}
