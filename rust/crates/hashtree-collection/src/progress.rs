//! Optional, content-free observations of an incremental collection batch.

/// A phase starts before its potentially blocking work. Index zero is by-id;
/// subsequent indices follow the collection definition's key-index order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CollectionBatchPhase {
    Prepare,
    LookupExistingIds,
    PrepareIndex { index: usize },
    UpdateIndex { index: usize, changes: usize },
    FlushIndex { index: usize },
}

/// Observers should return promptly and must not write to the collection.
pub type CollectionBatchObserver<'a> = dyn Fn(CollectionBatchPhase) + Send + Sync + 'a;
