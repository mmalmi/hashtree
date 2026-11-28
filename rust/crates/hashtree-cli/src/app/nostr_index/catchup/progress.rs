//! Opt-in phase starts, not a heartbeat or a publication/checkpoint claim.
use std::fmt::Arguments;
use std::io::Write;
use std::time::Instant;

use hashtree_core::Store;
use hashtree_nostr::NostrAppendProgress;

use super::read_cache::CatchupReadCache;

mod budget;
pub(super) use budget::TraceBudget;

#[derive(Debug)]
pub(super) enum AuthorPhase {
    Reconcile,
    Append,
    Validate,
    Sync,
    Checkpoint,
    Saved,
}

pub(super) struct AppendTrace<'a> {
    enabled: bool,
    budget: &'a TraceBudget,
    ordinal: usize,
    started: Instant,
}

impl<'a> AppendTrace<'a> {
    pub(super) fn new(enabled: bool, ordinal: usize, budget: &'a TraceBudget) -> Self {
        Self {
            enabled,
            budget,
            ordinal,
            started: Instant::now(),
        }
    }

    pub(super) fn phase<S: Store>(&self, phase: AuthorPhase, cache: &CatchupReadCache<S>) {
        self.write(format_args!("phase={phase:?}"), cache);
    }

    pub(super) fn batch<S: Store>(&self, row: NostrAppendProgress, cache: &CatchupReadCache<S>) {
        self.write(
            format_args!(
                "phase={:?} batch={} batches={} batch_events={} completed_batches={}",
                row.phase, row.batch, row.batches, row.events, row.completed_batches,
            ),
            cache,
        );
    }

    fn write<S: Store>(&self, phase: Arguments<'_>, cache: &CatchupReadCache<S>) {
        if self.enabled {
            self.write_to(phase, cache, &mut std::io::stderr().lock());
        }
    }

    fn write_to<S: Store>(
        &self,
        phase: Arguments<'_>,
        cache: &CatchupReadCache<S>,
        sink: &mut impl Write,
    ) {
        if !self.enabled {
            return;
        }
        // Diagnostics must not turn an unavailable stderr sink into a failed
        // data operation. No identities, roots, paths, tags or payloads enter it.
        self.budget.write(|| {
            let (hits, misses, bytes, entries) = cache.read_stats();
            let (write_calls, write_bytes) = cache.write_stats();
            format!(
            "Nostr catchup phase: author_ordinal={} {} writer_ms={} read_cache_hits={} read_cache_misses={} read_cache_bytes={} read_cache_entries={} store_write_calls={} store_submitted_write_bytes={}\n",
            self.ordinal, phase, self.started.elapsed().as_millis(), hits, misses,
            bytes, entries, write_calls, write_bytes,
            )
        }, sink);
    }
}

#[cfg(test)]
mod tests;
