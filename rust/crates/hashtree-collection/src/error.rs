use hashtree_core::HashTreeError;
use hashtree_index::{BTreeError, SearchError};

#[derive(Debug, thiserror::Error)]
pub enum CollectionError {
    #[error("hash tree error: {0}")]
    HashTree(#[from] HashTreeError),
    #[error("index error: {0}")]
    Index(#[from] BTreeError),
    #[error("search error: {0}")]
    Search(#[from] SearchError),
    #[error("put requires previous item when replacing existing id `{id}` in a collection with derived indexes; use replace(...) or rebuild/reindex when the previous item is unavailable")]
    MissingPreviousForOverwrite { id: String },
    #[error("{0}")]
    Validation(String),
}
