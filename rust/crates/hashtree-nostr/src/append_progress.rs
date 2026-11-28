//! Optional observations; no event identifiers, content, or additional store reads.

use hashtree_collection::CollectionBatchPhase;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NostrAppendPhase {
    BuildInitial,
    ReadManifest,
    ValidateEvents,
    LookupIds,
    LookupReplaceable,
    LookupParameterized,
    ReadPreviousEvents,
    BufferEvents,
    FlushPayload,
    Collection(CollectionBatchPhase),
    WriteManifest,
    FlushCommit,
    Complete,
}

/// Batch numbers are one-based. Completion means the batch returned its root
/// after flushing; the caller still owns force-sync and the author checkpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NostrAppendProgress {
    pub batch: usize,
    pub batches: usize,
    pub events: usize,
    pub completed_batches: usize,
    pub phase: NostrAppendPhase,
}

/// Invoked synchronously before each phase and after each successful batch.
/// Observers should return promptly and must not mutate the event store.
pub type NostrAppendObserver<'a> = dyn Fn(NostrAppendProgress) + Send + Sync + 'a;
