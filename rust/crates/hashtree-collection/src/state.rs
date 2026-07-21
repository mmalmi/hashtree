use std::collections::BTreeMap;
use std::sync::Arc;

use hashtree_core::{Cid, HashTree, HashTreeConfig, Store};

use crate::helpers::find_manifest_cid;
use crate::{CollectionDefinition, CollectionError, MANIFEST_BY_ID};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CollectionState {
    pub by_id_root: Option<Cid>,
    pub key_roots: BTreeMap<String, Option<Cid>>,
    pub search_roots: BTreeMap<String, Option<Cid>>,
}

impl CollectionState {
    pub fn key_root(&self, name: &str) -> Option<&Cid> {
        self.key_roots.get(name).and_then(Option::as_ref)
    }

    pub fn search_root(&self, name: &str) -> Option<&Cid> {
        self.search_roots.get(name).and_then(Option::as_ref)
    }
}

#[derive(Debug, Clone, Default)]
pub struct CollectionOptions {
    pub btree_order: Option<usize>,
    pub btree_update_concurrency: Option<usize>,
}

pub fn create_empty_collection_state<T>(definition: &CollectionDefinition<T>) -> CollectionState {
    CollectionState {
        by_id_root: None,
        key_roots: definition
            .key_indexes()
            .iter()
            .map(|index| (index.name().to_string(), None))
            .collect(),
        search_roots: definition
            .search_indexes()
            .iter()
            .map(|index| (index.name().to_string(), None))
            .collect(),
    }
}

pub async fn load_collection_state<S: Store, T>(
    store: Arc<S>,
    definition: &CollectionDefinition<T>,
    root: Option<&Cid>,
) -> Result<CollectionState, CollectionError> {
    let mut state = create_empty_collection_state(definition);
    let Some(root) = root else {
        return Ok(state);
    };

    let tree = HashTree::new(HashTreeConfig::new(store));
    let entries = tree.list_directory(root).await?;
    state.by_id_root = find_manifest_cid(&entries, MANIFEST_BY_ID);
    for index in definition.key_indexes() {
        state.key_roots.insert(
            index.name().to_string(),
            find_manifest_cid(&entries, index.name()),
        );
    }
    for index in definition.search_indexes() {
        state.search_roots.insert(
            index.name().to_string(),
            find_manifest_cid(&entries, index.name()),
        );
    }
    Ok(state)
}
