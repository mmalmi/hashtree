use std::collections::BTreeMap;
use std::sync::Arc;

use hashtree_core::{Cid, DirEntry, HashTree, HashTreeConfig, LinkType, Store};
use hashtree_index::{BTree, BTreeOptions, SearchIndex};

use crate::helpers::merge_search_index_options;
use crate::manifest::write_collection_manifest_metadata;
use crate::schema::normalize_typed_collection_item;
use crate::{
    create_empty_collection_state, default_search_prefix, load_collection_state,
    CollectionDefinition, CollectionEntryContext, CollectionError, CollectionOptions,
    CollectionState, CollectionWriteContext, COLLECTION_MANIFEST_METADATA_FILE, MANIFEST_BY_ID,
};

pub struct CollectionWriter<S: Store, T> {
    tree: HashTree<S>,
    index: BTree<S>,
    search_indexes: BTreeMap<String, SearchIndex<S>>,
    definition: CollectionDefinition<T>,
    state: CollectionState,
}

impl<S: Store, T> CollectionWriter<S, T> {
    pub fn new(store: Arc<S>, definition: CollectionDefinition<T>) -> Self {
        Self::with_options(store, definition, CollectionOptions::default())
    }

    pub fn with_options(
        store: Arc<S>,
        definition: CollectionDefinition<T>,
        options: CollectionOptions,
    ) -> Self {
        let state = create_empty_collection_state(&definition);
        Self::with_state_and_options(store, definition, state, options)
    }

    pub fn with_state(
        store: Arc<S>,
        definition: CollectionDefinition<T>,
        state: CollectionState,
    ) -> Self {
        Self::with_state_and_options(store, definition, state, CollectionOptions::default())
    }

    pub fn with_state_and_options(
        store: Arc<S>,
        definition: CollectionDefinition<T>,
        state: CollectionState,
        options: CollectionOptions,
    ) -> Self {
        let search_indexes = definition
            .search_indexes()
            .iter()
            .map(|index| {
                (
                    index.name().to_string(),
                    SearchIndex::new(
                        Arc::clone(&store),
                        merge_search_index_options(index.options().clone(), &options),
                    ),
                )
            })
            .collect();

        Self {
            tree: HashTree::new(HashTreeConfig::new(Arc::clone(&store))),
            index: BTree::new(
                store,
                BTreeOptions {
                    order: options.btree_order,
                },
            ),
            search_indexes,
            definition,
            state,
        }
    }

    pub async fn from_root(
        store: Arc<S>,
        definition: CollectionDefinition<T>,
        root: Option<&Cid>,
        options: CollectionOptions,
    ) -> Result<Self, CollectionError> {
        let state = load_collection_state(Arc::clone(&store), &definition, root).await?;
        Ok(Self::with_state_and_options(
            store, definition, state, options,
        ))
    }

    pub fn snapshot(&self) -> CollectionState {
        self.state.clone()
    }

    pub fn state(&self) -> &CollectionState {
        &self.state
    }

    pub async fn write_root(&self) -> Result<Option<Cid>, CollectionError> {
        let mut entries = Vec::new();
        if let Some(cid) = self.state.by_id_root.as_ref() {
            entries.push(DirEntry::from_cid(MANIFEST_BY_ID, cid).with_link_type(LinkType::Dir));
        }
        for index in self.definition.key_indexes() {
            if let Some(cid) = self.state.key_root(index.name()) {
                entries.push(DirEntry::from_cid(index.name(), cid).with_link_type(LinkType::Dir));
            }
        }
        for index in self.definition.search_indexes() {
            if let Some(cid) = self.state.search_root(index.name()) {
                entries.push(DirEntry::from_cid(index.name(), cid).with_link_type(LinkType::Dir));
            }
        }
        if let Some((cid, size)) =
            write_collection_manifest_metadata(&self.tree, &self.definition).await?
        {
            entries.push(
                DirEntry::from_cid(COLLECTION_MANIFEST_METADATA_FILE, &cid)
                    .with_size(size)
                    .with_link_type(LinkType::File),
            );
        }

        if entries.is_empty() {
            return Ok(None);
        }

        Ok(Some(self.tree.put_directory(entries).await?))
    }

    fn read_search_root_group(&self, root_name: &str) -> Option<Cid> {
        for index in self.definition.search_indexes() {
            if index.root_name().unwrap_or(index.name()) == root_name {
                return self.state.search_root(index.name()).cloned();
            }
        }
        None
    }

    fn assign_search_root_groups(&mut self, groups: &BTreeMap<String, Option<Cid>>) {
        for index in self.definition.search_indexes() {
            let root_name = index.root_name().unwrap_or(index.name());
            if let Some(root) = groups.get(root_name) {
                self.state
                    .search_roots
                    .insert(index.name().to_string(), root.clone());
            }
        }
    }

    fn has_derived_indexes(&self) -> bool {
        !self.definition.key_indexes().is_empty() || !self.definition.search_indexes().is_empty()
    }
}

impl<S: Store, T: Clone> CollectionWriter<S, T> {
    pub fn normalize(&self, item: &T) -> Result<T, CollectionError> {
        normalize_typed_collection_item(&self.definition, item)
    }

    pub async fn put(
        &mut self,
        item: &T,
        cid: &Cid,
        previous: Option<&T>,
    ) -> Result<CollectionState, CollectionError> {
        self.put_with_context(item, cid, previous, None, None).await
    }

    pub async fn replace(
        &mut self,
        item: &T,
        cid: &Cid,
        previous: &T,
    ) -> Result<CollectionState, CollectionError> {
        self.replace_with_context(item, cid, previous, None, None)
            .await
    }

    pub async fn put_with_context(
        &mut self,
        item: &T,
        cid: &Cid,
        previous: Option<&T>,
        context: Option<&CollectionWriteContext>,
        previous_context: Option<&CollectionWriteContext>,
    ) -> Result<CollectionState, CollectionError> {
        let next_item = self.normalize(item)?;
        let id = self.definition.item_id(&next_item)?;
        let previous_item = previous
            .map(|previous| self.normalize(previous))
            .transpose()?;

        if previous_item.is_none() && self.has_derived_indexes() {
            let existing = if let Some(root) = self.state.by_id_root.as_ref() {
                self.index.get_link(Some(root), &id).await?
            } else {
                None
            };
            if existing.is_some() {
                return Err(CollectionError::MissingPreviousForOverwrite { id });
            }
        }

        if let Some(previous_item) = previous_item.as_ref() {
            self.delete_normalized_with_context(previous_item, previous_context.or(context))
                .await?;
        }

        self.put_normalized_with_context(&next_item, cid, context)
            .await
    }

    pub async fn replace_with_context(
        &mut self,
        item: &T,
        cid: &Cid,
        previous: &T,
        context: Option<&CollectionWriteContext>,
        previous_context: Option<&CollectionWriteContext>,
    ) -> Result<CollectionState, CollectionError> {
        self.put_with_context(item, cid, Some(previous), context, previous_context)
            .await
    }

    pub async fn delete(&mut self, item: &T) -> Result<CollectionState, CollectionError> {
        self.delete_with_context(item, None).await
    }

    pub async fn delete_with_context(
        &mut self,
        item: &T,
        context: Option<&CollectionWriteContext>,
    ) -> Result<CollectionState, CollectionError> {
        let next_item = self.normalize(item)?;
        self.delete_normalized_with_context(&next_item, context)
            .await
    }

    pub async fn rebuild<I>(&mut self, entries: I) -> Result<CollectionState, CollectionError>
    where
        I: IntoIterator<Item = (T, Cid)>,
    {
        let mut final_entries = BTreeMap::<String, (T, Cid, Option<CollectionWriteContext>)>::new();
        for (item, cid) in entries {
            let next_item = self.normalize(&item)?;
            let id = self.definition.item_id(&next_item)?;
            final_entries.insert(id, (next_item, cid, None));
        }

        self.rebuild_from_final_entries(final_entries).await
    }

    pub async fn reindex<I>(&mut self, entries: I) -> Result<CollectionState, CollectionError>
    where
        I: IntoIterator<Item = (T, Cid)>,
    {
        self.rebuild(entries).await
    }

    pub async fn rebuild_with_context<I>(
        &mut self,
        entries: I,
    ) -> Result<CollectionState, CollectionError>
    where
        I: IntoIterator<Item = (T, Cid, Option<CollectionWriteContext>)>,
    {
        let mut final_entries = BTreeMap::<String, (T, Cid, Option<CollectionWriteContext>)>::new();
        for (item, cid, context) in entries {
            let next_item = self.normalize(&item)?;
            let id = self.definition.item_id(&next_item)?;
            final_entries.insert(id, (next_item, cid, context));
        }

        self.rebuild_from_final_entries(final_entries).await
    }

    pub async fn reindex_with_context<I>(
        &mut self,
        entries: I,
    ) -> Result<CollectionState, CollectionError>
    where
        I: IntoIterator<Item = (T, Cid, Option<CollectionWriteContext>)>,
    {
        self.rebuild_with_context(entries).await
    }

    async fn put_normalized_with_context(
        &mut self,
        item: &T,
        cid: &Cid,
        context: Option<&CollectionWriteContext>,
    ) -> Result<CollectionState, CollectionError> {
        let id = self.definition.item_id(item)?;
        self.state.by_id_root = Some(
            self.index
                .insert_link(self.state.by_id_root.as_ref(), &id, cid)
                .await?,
        );

        for index in self.definition.key_indexes() {
            let mut root = self.state.key_root(index.name()).cloned();
            for key in index.materialize_keys(item) {
                root = Some(self.index.insert_link(root.as_ref(), &key, cid).await?);
            }
            self.state.key_roots.insert(index.name().to_string(), root);
        }

        let mut search_root_groups = BTreeMap::<String, Option<Cid>>::new();
        let entry_context = CollectionEntryContext {
            id: &id,
            cid: Some(cid),
            write_context: context,
        };
        for index in self.definition.search_indexes() {
            let Some(search_index) = self.search_indexes.get(index.name()) else {
                continue;
            };

            let root_name = index.root_name().unwrap_or(index.name()).to_string();
            let mut root = search_root_groups
                .get(&root_name)
                .cloned()
                .unwrap_or_else(|| self.read_search_root_group(&root_name));

            for entry in index.materialize_entries(item, &entry_context) {
                let terms = search_index.parse_keywords(&entry.text);
                if terms.is_empty() {
                    continue;
                }
                let entry_id = entry.id.as_deref().unwrap_or(&id);
                let Some(entry_cid) = entry.cid.as_ref().or(Some(cid)) else {
                    continue;
                };
                let default_prefix = default_search_prefix(index.name());
                root = Some(
                    search_index
                        .index_link(
                            root.as_ref(),
                            entry
                                .prefix
                                .as_deref()
                                .or_else(|| index.prefix())
                                .unwrap_or(&default_prefix),
                            &terms,
                            entry_id,
                            entry_cid,
                        )
                        .await?,
                );
            }

            search_root_groups.insert(root_name, root);
        }

        if !search_root_groups.is_empty() {
            self.assign_search_root_groups(&search_root_groups);
        }

        Ok(self.snapshot())
    }

    async fn delete_normalized_with_context(
        &mut self,
        item: &T,
        context: Option<&CollectionWriteContext>,
    ) -> Result<CollectionState, CollectionError> {
        let id = self.definition.item_id(item)?;
        if let Some(root) = self.state.by_id_root.as_ref() {
            self.state.by_id_root = self.index.delete(root, &id).await?;
        }

        for index in self.definition.key_indexes() {
            let mut root = self.state.key_root(index.name()).cloned();
            for key in index.materialize_keys(item) {
                let Some(active_root) = root.as_ref() else {
                    break;
                };
                root = self.index.delete(active_root, &key).await?;
            }
            self.state.key_roots.insert(index.name().to_string(), root);
        }

        let mut search_root_groups = BTreeMap::<String, Option<Cid>>::new();
        let entry_context = CollectionEntryContext {
            id: &id,
            cid: None,
            write_context: context,
        };
        for index in self.definition.search_indexes() {
            let Some(search_index) = self.search_indexes.get(index.name()) else {
                continue;
            };

            let root_name = index.root_name().unwrap_or(index.name()).to_string();
            let mut root = search_root_groups
                .get(&root_name)
                .cloned()
                .unwrap_or_else(|| self.read_search_root_group(&root_name));
            let Some(existing_root) = root.clone() else {
                continue;
            };
            root = Some(existing_root);

            for entry in index.materialize_entries(item, &entry_context) {
                let terms = search_index.parse_keywords(&entry.text);
                if terms.is_empty() {
                    continue;
                }
                let entry_id = entry.id.as_deref().unwrap_or(&id);
                let Some(active_root) = root.as_ref() else {
                    break;
                };
                let default_prefix = default_search_prefix(index.name());
                root = search_index
                    .remove_link(
                        active_root,
                        entry
                            .prefix
                            .as_deref()
                            .or_else(|| index.prefix())
                            .unwrap_or(&default_prefix),
                        &terms,
                        entry_id,
                    )
                    .await?;
            }

            search_root_groups.insert(root_name, root);
        }

        if !search_root_groups.is_empty() {
            self.assign_search_root_groups(&search_root_groups);
        } else {
            for index in self.definition.search_indexes() {
                let root_name = index.root_name().unwrap_or(index.name()).to_string();
                self.state.search_roots.insert(
                    index.name().to_string(),
                    self.read_search_root_group(&root_name),
                );
            }
        }

        Ok(self.snapshot())
    }

    async fn rebuild_without_search(
        &mut self,
        final_entries: BTreeMap<String, (T, Cid, Option<CollectionWriteContext>)>,
    ) -> Result<CollectionState, CollectionError> {
        let mut by_id = BTreeMap::<String, Cid>::new();
        let mut key_roots = self
            .definition
            .key_indexes()
            .iter()
            .map(|index| (index.name().to_string(), BTreeMap::<String, Cid>::new()))
            .collect::<BTreeMap<_, _>>();

        for (id, (item, cid, _context)) in final_entries {
            by_id.insert(id, cid.clone());
            for index in self.definition.key_indexes() {
                let root = key_roots
                    .get_mut(index.name())
                    .expect("collection key root must exist");
                for key in index.materialize_keys(&item) {
                    root.insert(key, cid.clone());
                }
            }
        }

        self.state = create_empty_collection_state(&self.definition);
        self.state.by_id_root = self.index.build_links(by_id).await?;
        for index in self.definition.key_indexes() {
            let root = self
                .index
                .build_links(key_roots.remove(index.name()).unwrap_or_default())
                .await?;
            self.state.key_roots.insert(index.name().to_string(), root);
        }

        Ok(self.snapshot())
    }

    async fn rebuild_from_final_entries(
        &mut self,
        final_entries: BTreeMap<String, (T, Cid, Option<CollectionWriteContext>)>,
    ) -> Result<CollectionState, CollectionError> {
        if self.definition.search_indexes().is_empty() {
            return self.rebuild_without_search(final_entries).await;
        }

        let mut by_id = BTreeMap::<String, Cid>::new();
        let mut key_roots = self
            .definition
            .key_indexes()
            .iter()
            .map(|index| (index.name().to_string(), BTreeMap::<String, Cid>::new()))
            .collect::<BTreeMap<_, _>>();
        let mut search_root_entries = BTreeMap::<String, BTreeMap<String, Cid>>::new();

        for (id, (item, cid, context)) in final_entries {
            by_id.insert(id.clone(), cid.clone());

            for index in self.definition.key_indexes() {
                let root = key_roots
                    .get_mut(index.name())
                    .expect("collection key root must exist");
                for key in index.materialize_keys(&item) {
                    root.insert(key, cid.clone());
                }
            }

            let entry_context = CollectionEntryContext {
                id: &id,
                cid: Some(&cid),
                write_context: context.as_ref(),
            };

            for index in self.definition.search_indexes() {
                let Some(search_index) = self.search_indexes.get(index.name()) else {
                    continue;
                };

                let root_name = index.root_name().unwrap_or(index.name()).to_string();
                let root_entries = search_root_entries.entry(root_name).or_default();

                for entry in index.materialize_entries(&item, &entry_context) {
                    let terms = search_index.parse_keywords(&entry.text);
                    if terms.is_empty() {
                        continue;
                    }

                    let entry_id = entry.id.as_deref().unwrap_or(&id);
                    let Some(entry_cid) = entry.cid.as_ref().or(Some(&cid)) else {
                        continue;
                    };
                    let default_prefix = default_search_prefix(index.name());
                    let prefix = entry
                        .prefix
                        .as_deref()
                        .or_else(|| index.prefix())
                        .unwrap_or(&default_prefix);

                    for term in terms {
                        root_entries
                            .insert(format!("{prefix}{term}:{entry_id}"), entry_cid.clone());
                    }
                }
            }
        }

        self.state = create_empty_collection_state(&self.definition);
        self.state.by_id_root = self.index.build_links(by_id).await?;
        for index in self.definition.key_indexes() {
            let root = self
                .index
                .build_links(key_roots.remove(index.name()).unwrap_or_default())
                .await?;
            self.state.key_roots.insert(index.name().to_string(), root);
        }

        let mut search_root_groups = BTreeMap::<String, Option<Cid>>::new();
        for (root_name, root_entries) in search_root_entries {
            let Some(search_index) = self.definition.search_indexes().iter().find_map(|index| {
                ((index.root_name().unwrap_or(index.name())) == root_name)
                    .then(|| self.search_indexes.get(index.name()))
                    .flatten()
            }) else {
                continue;
            };

            search_root_groups.insert(root_name, search_index.build_links(root_entries).await?);
        }

        if !search_root_groups.is_empty() {
            self.assign_search_root_groups(&search_root_groups);
        }

        Ok(self.snapshot())
    }
}
