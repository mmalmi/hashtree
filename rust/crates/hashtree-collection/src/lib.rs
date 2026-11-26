//! Immutable by-id, key-index, search-index, schema, and federated-search
//! collections for hashtree.

use std::collections::BTreeMap;

use hashtree_core::Cid;

pub const MANIFEST_BY_ID: &str = "by-id";

pub type CollectionWriteContext = BTreeMap<String, Cid>;

mod definition;
mod error;
mod federated;
mod helpers;
mod manifest;
mod progress;
mod schema;
mod source;
mod state;
mod writer;

pub use definition::{
    default_search_prefix, CollectionDefinition, CollectionEntryContext,
    CollectionKeyIndexDefinition, CollectionPublishedSchema, CollectionSearchEntry,
    CollectionSearchIndexDefinition,
};
pub use error::CollectionError;
pub use federated::{
    federated_search, FederatedCollectionSource, FederatedSearchHit, FederatedSearchOptions,
    FederatedSearchSourceHit,
};
pub use manifest::{
    load_collection_manifest_metadata, CollectionManifestMetadata,
    COLLECTION_MANIFEST_METADATA_FILE,
};
pub use progress::{CollectionBatchObserver, CollectionBatchPhase};
pub use schema::{
    get_collection_schema, get_schema_version, normalize_collection_item, CollectionSchema,
    NormalizeCollectionItemOptions,
};
pub use source::{CollectionIndexLinkResult, CollectionSource};
pub use state::{
    create_empty_collection_state, load_collection_state, CollectionOptions, CollectionState,
};
pub use writer::CollectionWriter;

pub use hashtree_index::{SearchIndexOptions, SearchLinkResult, SearchOptions};
