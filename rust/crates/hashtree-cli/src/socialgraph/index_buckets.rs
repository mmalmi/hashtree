use super::*;

pub(super) struct EventIndexBucket {
    pub(super) event_store: NostrEventStore<StorageRouter>,
    pub(super) root_path: PathBuf,
}

pub(super) struct ProfileIndexBucket {
    pub(super) store: Arc<StorageRouter>,
    pub(super) tree: HashTree<StorageRouter>,
    pub(super) index: BTree<StorageRouter>,
    pub(super) by_pubkey_root_path: PathBuf,
    pub(super) search_root_path: PathBuf,
    pub(super) root_pair_commit_path: PathBuf,
    pub(super) root_pair_lock_path: PathBuf,
}

impl EventIndexBucket {
    pub(super) fn events_root(&self) -> Result<Option<Cid>> {
        let _profile = NostrProfileGuard::new("socialgraph.events_root.read");
        read_root_file(&self.root_path)
    }

    pub(super) fn write_events_root(&self, root: Option<&Cid>) -> Result<()> {
        let _profile = NostrProfileGuard::new("socialgraph.events_root.write");
        write_root_file(&self.root_path, root)
    }

    pub(super) fn write_events_root_durable(&self, root: Option<&Cid>) -> Result<()> {
        let _profile = NostrProfileGuard::new("socialgraph.events_root.write_durable");
        write_root_file_durable(&self.root_path, root)
    }

    pub(super) fn events_root_for_write(&self) -> Result<Option<Cid>> {
        let root = self.events_root()?;
        let Some(root_ref) = root.as_ref() else {
            return Ok(None);
        };

        if let Err(err) = block_on(self.event_store.validate_index_root(Some(root_ref))) {
            tracing::warn!(
                "Ignoring invalid social graph event index root {} before write: {}",
                hex::encode(root_ref.hash),
                err
            );
            self.write_events_root(None)?;
            return Ok(None);
        }

        Ok(root)
    }

    pub(super) fn store_event(&self, root: Option<&Cid>, event: &Event) -> Result<Cid> {
        let stored = stored_event_from_nostr_sdk_event(event);
        let _profile = NostrProfileGuard::new("socialgraph.event_store.build_single");
        block_on(self.event_store.build(root, std::iter::once(stored)))
            .map_err(map_event_store_error)?
            .context("single Nostr event did not produce an event root")
    }

    pub(super) fn load_event_by_id(&self, root: &Cid, event_id: &str) -> Result<Option<Event>> {
        let stored = block_on(self.event_store.get_by_id(Some(root), event_id))
            .map_err(map_event_store_error)?;
        stored.map(stored_event_to_nostr_event).transpose()
    }

    fn load_events_for_author(
        &self,
        root: &Cid,
        author: &nostr::PublicKey,
        filter: &Filter,
        limit: usize,
        exact: bool,
    ) -> Result<Vec<Event>> {
        let kind_filter = filter.kinds.as_ref().and_then(|kinds| {
            if kinds.len() == 1 {
                kinds.iter().next().map(|kind| kind.as_u16() as u32)
            } else {
                None
            }
        });
        let author_hex = author.to_hex();
        let options = filter_list_options(filter, limit, exact);
        let stored = match kind_filter {
            Some(kind) => block_on(self.event_store.list_by_author_and_kind(
                Some(root),
                &author_hex,
                kind,
                options.clone(),
            ))
            .map_err(map_event_store_error)?,
            None => block_on(
                self.event_store
                    .list_by_author(Some(root), &author_hex, options),
            )
            .map_err(map_event_store_error)?,
        };
        stored
            .into_iter()
            .map(stored_event_to_nostr_event)
            .collect::<Result<Vec<_>>>()
    }

    fn load_events_for_kind(
        &self,
        root: &Cid,
        kind: Kind,
        filter: &Filter,
        limit: usize,
        exact: bool,
    ) -> Result<Vec<Event>> {
        let stored = block_on(self.event_store.list_by_kind(
            Some(root),
            kind.as_u16() as u32,
            filter_list_options(filter, limit, exact),
        ))
        .map_err(map_event_store_error)?;
        stored
            .into_iter()
            .map(stored_event_to_nostr_event)
            .collect::<Result<Vec<_>>>()
    }

    fn load_events_for_author_and_kind(
        &self,
        root: &Cid,
        author: &nostr::PublicKey,
        kind: Kind,
        filter: &Filter,
        limit: usize,
        exact: bool,
    ) -> Result<Vec<Event>> {
        let stored = block_on(self.event_store.list_by_author_and_kind(
            Some(root),
            &author.to_hex(),
            kind.as_u16() as u32,
            filter_list_options(filter, limit, exact),
        ))
        .map_err(map_event_store_error)?;
        stored
            .into_iter()
            .map(stored_event_to_nostr_event)
            .collect::<Result<Vec<_>>>()
    }

    fn load_recent_events(
        &self,
        root: &Cid,
        filter: &Filter,
        limit: usize,
        exact: bool,
    ) -> Result<Vec<Event>> {
        let stored = block_on(
            self.event_store
                .list_recent(Some(root), filter_list_options(filter, limit, exact)),
        )
        .map_err(map_event_store_error)?;
        stored
            .into_iter()
            .map(stored_event_to_nostr_event)
            .collect::<Result<Vec<_>>>()
    }

    fn load_events_for_tag(
        &self,
        root: &Cid,
        tag_name: &str,
        values: &[String],
        filter: &Filter,
        limit: usize,
        exact: bool,
    ) -> Result<Vec<Event>> {
        let mut events = Vec::new();
        let mut seen = HashSet::new();
        let options = filter_list_options(filter, limit, exact);
        for value in values {
            let remaining = limit.saturating_sub(events.len());
            if remaining == 0 {
                break;
            }
            let stored = block_on(self.event_store.list_by_tag(
                Some(root),
                tag_name,
                value,
                ListEventsOptions {
                    limit: Some(remaining.max(1)),
                    ..options.clone()
                },
            ))
            .map_err(map_event_store_error)?;
            let next_events = stored
                .into_iter()
                .map(stored_event_to_nostr_event)
                .collect::<Result<Vec<_>>>()?;
            extend_unique_events(&mut events, &mut seen, next_events, limit);
        }
        Ok(events)
    }

    fn choose_tag_source(&self, filter: &Filter) -> Option<(String, Vec<String>)> {
        filter
            .generic_tags
            .iter()
            .min_by_key(|(_, values)| values.len())
            .map(|(tag, values)| {
                (
                    tag.as_char().to_ascii_lowercase().to_string(),
                    values.iter().cloned().collect(),
                )
            })
    }

    fn load_major_index_candidates(
        &self,
        root: &Cid,
        filter: &Filter,
        limit: usize,
    ) -> Result<Option<Vec<Event>>> {
        if let Some(events) = self.load_direct_replaceable_candidates(root, filter)? {
            return Ok(Some(events));
        }

        if let Some((tag_name, values)) = self.choose_tag_source(filter) {
            let exact = filter.authors.is_none()
                && filter.kinds.is_none()
                && filter.search.is_none()
                && filter.generic_tags.len() == 1;
            return Ok(Some(self.load_events_for_tag(
                root, &tag_name, &values, filter, limit, exact,
            )?));
        }

        if let (Some(authors), Some(kinds)) = (filter.authors.as_ref(), filter.kinds.as_ref()) {
            let mut events = Vec::new();
            let mut seen = HashSet::new();
            let exact = filter.generic_tags.is_empty() && filter.search.is_none();
            for author in authors {
                for kind in kinds {
                    let remaining = limit.saturating_sub(events.len());
                    if remaining == 0 {
                        break;
                    }
                    let next_events = self.load_events_for_author_and_kind(
                        root, author, *kind, filter, remaining, exact,
                    )?;
                    extend_unique_events(&mut events, &mut seen, next_events, limit);
                }
                if events.len() >= limit {
                    break;
                }
            }
            return Ok(Some(events));
        }

        if let Some(authors) = filter.authors.as_ref() {
            let mut events = Vec::new();
            let mut seen = HashSet::new();
            let exact = filter.generic_tags.is_empty() && filter.search.is_none();
            for author in authors {
                let remaining = limit.saturating_sub(events.len());
                if remaining == 0 {
                    break;
                }
                let next_events =
                    self.load_events_for_author(root, author, filter, remaining, exact)?;
                extend_unique_events(&mut events, &mut seen, next_events, limit);
            }
            return Ok(Some(events));
        }

        if let Some(kinds) = filter.kinds.as_ref() {
            let mut events = Vec::new();
            let mut seen = HashSet::new();
            let exact = filter.authors.is_none()
                && filter.generic_tags.is_empty()
                && filter.search.is_none();
            for kind in kinds {
                let remaining = limit.saturating_sub(events.len());
                if remaining == 0 {
                    break;
                }
                let next_events =
                    self.load_events_for_kind(root, *kind, filter, remaining, exact)?;
                extend_unique_events(&mut events, &mut seen, next_events, limit);
            }
            return Ok(Some(events));
        }

        Ok(None)
    }

    fn load_direct_replaceable_candidates(
        &self,
        root: &Cid,
        filter: &Filter,
    ) -> Result<Option<Vec<Event>>> {
        let Some(authors) = filter.authors.as_ref() else {
            return Ok(None);
        };
        let Some(kinds) = filter.kinds.as_ref() else {
            return Ok(None);
        };
        if kinds.len() != 1 {
            return Ok(None);
        }

        let kind = kinds.iter().next().expect("checked single kind").as_u16() as u32;

        if is_parameterized_replaceable_kind(kind) {
            let d_tag = SingleLetterTag::lowercase(nostr::Alphabet::D);
            let Some(d_values) = filter.generic_tags.get(&d_tag) else {
                return Ok(None);
            };
            let mut events = Vec::new();
            for author in authors {
                let author_hex = author.to_hex();
                for d_value in d_values {
                    if let Some(stored) = block_on(self.event_store.get_parameterized_replaceable(
                        Some(root),
                        &author_hex,
                        kind,
                        d_value,
                    ))
                    .map_err(map_event_store_error)?
                    {
                        events.push(stored_event_to_nostr_event(stored)?);
                    }
                }
            }
            return Ok(Some(dedupe_events(events)));
        }

        if is_replaceable_kind(kind) {
            let mut events = Vec::new();
            for author in authors {
                if let Some(stored) = block_on(self.event_store.get_replaceable(
                    Some(root),
                    &author.to_hex(),
                    kind,
                ))
                .map_err(map_event_store_error)?
                {
                    events.push(stored_event_to_nostr_event(stored)?);
                }
            }
            return Ok(Some(dedupe_events(events)));
        }

        Ok(None)
    }

    pub(super) fn query_events(&self, filter: &Filter, limit: usize) -> Result<Vec<Event>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let events_root = self.events_root()?;
        let Some(root) = events_root.as_ref() else {
            return Ok(Vec::new());
        };
        let mut candidates = Vec::new();
        let mut seen: HashSet<[u8; 32]> = HashSet::new();

        if let Some(ids) = filter.ids.as_ref() {
            for id in ids {
                let id_bytes = id.to_bytes();
                if !seen.insert(id_bytes) {
                    continue;
                }
                if let Some(event) = self.load_event_by_id(root, &id.to_hex())? {
                    if filter.match_event(&event, Default::default()) {
                        candidates.push(event);
                    }
                }
                if candidates.len() >= limit {
                    break;
                }
            }
        } else {
            let base_events = match self.load_major_index_candidates(root, filter, limit)? {
                Some(events) => events,
                None => self.load_recent_events(
                    root,
                    filter,
                    limit,
                    filter.authors.is_none()
                        && filter.kinds.is_none()
                        && filter.generic_tags.is_empty()
                        && filter.search.is_none(),
                )?,
            };

            for event in base_events {
                let id_bytes = event.id.to_bytes();
                if !seen.insert(id_bytes) {
                    continue;
                }
                if filter.match_event(&event, Default::default()) {
                    candidates.push(event);
                }
                if candidates.len() >= limit {
                    break;
                }
            }
        }

        candidates.sort_by(|a, b| {
            b.created_at
                .as_secs()
                .cmp(&a.created_at.as_secs())
                .then_with(|| a.id.cmp(&b.id))
        });
        candidates.truncate(limit);
        Ok(candidates)
    }
}

impl ProfileIndexBucket {
    #[cfg(test)]
    pub(super) fn roots(&self) -> Result<(Option<Cid>, Option<Cid>)> {
        let _transaction = self.acquire_exclusive_root_pair_transaction()?;
        self.recover_pending_root_pair_commit_locked()?;
        let db_dir = self.root_pair_lock_path.parent().with_context(|| {
            format!(
                "{} has no parent directory",
                self.root_pair_lock_path.display()
            )
        })?;
        require_no_pending_profile_projection(db_dir)?;
        self.roots_locked()
    }

    pub(super) fn roots_locked(&self) -> Result<(Option<Cid>, Option<Cid>)> {
        Ok((
            read_root_file(&self.by_pubkey_root_path)?,
            read_root_file(&self.search_root_path)?,
        ))
    }

    pub(super) fn acquire_exclusive_root_pair_transaction(
        &self,
    ) -> Result<ProfileRootPairTransactionGuard> {
        acquire_profile_root_pair_lock(
            &self.root_pair_lock_path,
            ProfileRootPairLockMode::Exclusive,
            true,
        )
    }

    pub(super) fn acquire_exclusive_root_pair_transaction_with_timeout(
        &self,
        timeout: Duration,
    ) -> Result<ProfileRootPairTransactionGuard> {
        acquire_profile_root_pair_lock_with_timeout(
            &self.root_pair_lock_path,
            ProfileRootPairLockMode::Exclusive,
            true,
            timeout,
        )
    }

    pub(super) async fn acquire_exclusive_root_pair_transaction_async(
        &self,
    ) -> Result<ProfileRootPairTransactionGuard> {
        acquire_profile_root_pair_lock_async(
            &self.root_pair_lock_path,
            ProfileRootPairLockMode::Exclusive,
            true,
        )
        .await
    }

    pub(super) fn recover_pending_root_pair_commit_locked(&self) -> Result<()> {
        let Some(commit) = load_profile_root_pair_commit(&self.root_pair_commit_path)? else {
            return Ok(());
        };
        install_profile_root_pair_commit_with(
            &self.by_pubkey_root_path,
            &self.search_root_path,
            &self.root_pair_commit_path,
            &commit,
            || Ok(()),
        )
    }

    #[cfg(test)]
    pub(super) fn write_roots(
        &self,
        by_pubkey_root: Option<&Cid>,
        search_root: Option<&Cid>,
    ) -> Result<()> {
        self.write_roots_with_hooks(by_pubkey_root, search_root, || Ok(()), || Ok(()))
    }

    #[cfg(test)]
    fn write_roots_with_hooks<F, G>(
        &self,
        by_pubkey_root: Option<&Cid>,
        search_root: Option<&Cid>,
        after_intent: F,
        after_search: G,
    ) -> Result<()>
    where
        F: FnOnce() -> Result<()>,
        G: FnOnce() -> Result<()>,
    {
        let _transaction = self.acquire_exclusive_root_pair_transaction()?;
        self.write_roots_with_hooks_locked(by_pubkey_root, search_root, after_intent, after_search)
    }

    pub(super) fn write_roots_with_hooks_locked<F, G>(
        &self,
        by_pubkey_root: Option<&Cid>,
        search_root: Option<&Cid>,
        after_intent: F,
        after_search: G,
    ) -> Result<()>
    where
        F: FnOnce() -> Result<()>,
        G: FnOnce() -> Result<()>,
    {
        self.recover_pending_root_pair_commit_locked()?;
        self.store
            .force_sync()
            .map_err(|error| anyhow::anyhow!("force-sync profile index blocks: {error}"))?;
        let old_by_pubkey = read_root_file(&self.by_pubkey_root_path)?;
        let old_search = read_root_file(&self.search_root_path)?;
        let commit = ProfileRootPairCommit {
            version: PROFILE_ROOT_PAIR_COMMIT_VERSION,
            old_search: old_search.as_ref().map(stored_cid),
            old_by_pubkey: old_by_pubkey.as_ref().map(stored_cid),
            new_search: search_root.map(stored_cid),
            new_by_pubkey: by_pubkey_root.map(stored_cid),
        };
        let bytes = profile_root_pair_commit_bytes(&commit)?;
        replace_file_durable(
            &self.root_pair_commit_path,
            &bytes,
            "profile root-pair commit",
        )?;
        after_intent()?;
        install_profile_root_pair_commit_with(
            &self.by_pubkey_root_path,
            &self.search_root_path,
            &self.root_pair_commit_path,
            &commit,
            after_search,
        )
    }

    #[cfg(test)]
    pub(super) fn write_roots_interrupted_after_intent(
        &self,
        by_pubkey_root: Option<&Cid>,
        search_root: Option<&Cid>,
    ) -> Result<()> {
        self.write_roots_with_hooks(
            by_pubkey_root,
            search_root,
            || anyhow::bail!("injected interruption after durable profile root-pair intent"),
            || Ok(()),
        )
    }

    #[cfg(test)]
    pub(super) fn write_roots_interrupted_after_search(
        &self,
        by_pubkey_root: Option<&Cid>,
        search_root: Option<&Cid>,
    ) -> Result<()> {
        self.write_roots_with_hooks(
            by_pubkey_root,
            search_root,
            || Ok(()),
            || anyhow::bail!("injected interruption after durable profile-search root"),
        )
    }

    fn mirror_profile_event(&self, event: &Event) -> Result<Cid> {
        let bytes = event.as_json().into_bytes();
        block_on(self.tree.put_file(&bytes))
            .map(|(cid, _size)| cid)
            .context("store mirrored profile event")
    }

    pub(super) fn load_profile_event(&self, cid: &Cid) -> Result<Option<Event>> {
        let bytes = block_on(self.tree.get(cid, None)).context("read mirrored profile event")?;
        let Some(bytes) = bytes else {
            return Ok(None);
        };
        let json = String::from_utf8(bytes).context("decode mirrored profile event as utf-8")?;
        Ok(Some(
            Event::from_json(json).context("decode mirrored profile event json")?,
        ))
    }

    pub(super) fn profile_event_for_pubkey_at_root(
        &self,
        root: Option<&Cid>,
        pubkey_hex: &str,
    ) -> Result<Option<Event>> {
        let Some(cid) = block_on(self.index.get_link(root, pubkey_hex))
            .context("read mirrored profile event cid by pubkey")?
        else {
            return Ok(None);
        };
        self.load_profile_event(&cid)
    }

    pub(super) fn search_entries_for_prefix_at_root(
        &self,
        root: &Cid,
        prefix: &str,
    ) -> Result<Vec<(String, StoredProfileSearchEntry)>> {
        let entries =
            block_on(self.index.prefix(root, prefix)).context("query profile search prefix")?;
        entries
            .into_iter()
            .map(|(key, value)| {
                let entry = serde_json::from_str(&value)
                    .context("decode stored profile search entry json")?;
                Ok((key, entry))
            })
            .collect()
    }

    pub(super) fn rebuild_profile_events_with_distances_locked<'a, I, F>(
        &self,
        events: I,
        mut follow_distance_for_event: F,
    ) -> Result<(Option<Cid>, Option<Cid>)>
    where
        I: IntoIterator<Item = &'a Event>,
        F: FnMut(&Event) -> Result<Option<u32>>,
    {
        let mut by_pubkey_entries = Vec::<(String, Cid)>::new();
        let mut search_entries = Vec::<(String, String)>::new();

        for event in events {
            let pubkey = event.pubkey.to_hex();
            let mirrored_cid = self.mirror_profile_event(event)?;
            let follow_distance = follow_distance_for_event(event)?;
            let search_value = serialize_profile_search_entry(&build_profile_search_entry(
                event,
                &mirrored_cid,
                follow_distance,
            )?)?;
            by_pubkey_entries.push((pubkey.clone(), mirrored_cid.clone()));
            for term in profile_search_terms_for_event(event) {
                search_entries.push((
                    format!("{PROFILE_SEARCH_PREFIX}{term}:{pubkey}"),
                    search_value.clone(),
                ));
            }
        }

        let by_pubkey_root = block_on(self.index.build_links(by_pubkey_entries))
            .context("bulk build mirrored profile-by-pubkey index")?;
        let search_root = block_on(self.index.build(search_entries))
            .context("bulk build mirrored profile search index")?;
        Ok((by_pubkey_root, search_root))
    }

    pub(super) fn rebuild_profile_events_and_commit_with_distances_locked<'a, I, F>(
        &self,
        events: I,
        follow_distance_for_event: F,
    ) -> Result<()>
    where
        I: IntoIterator<Item = &'a Event>,
        F: FnMut(&Event) -> Result<Option<u32>>,
    {
        self.recover_pending_root_pair_commit_locked()?;
        let (by_pubkey_root, search_root) =
            self.rebuild_profile_events_with_distances_locked(events, follow_distance_for_event)?;
        self.write_roots_with_hooks_locked(
            by_pubkey_root.as_ref(),
            search_root.as_ref(),
            || Ok(()),
            || Ok(()),
        )
    }

    pub(super) async fn rebuild_profile_events_async_with_distances_locked<'a, I, F>(
        &self,
        events: I,
        mut follow_distance_for_event: F,
    ) -> Result<(Option<Cid>, Option<Cid>)>
    where
        I: IntoIterator<Item = &'a Event>,
        F: FnMut(&Event) -> Result<Option<u32>>,
    {
        let mut by_pubkey_entries = Vec::<(String, Cid)>::new();
        let mut search_entries = Vec::<(String, String)>::new();

        for event in events {
            let pubkey = event.pubkey.to_hex();
            let bytes = event.as_json().into_bytes();
            let (mirrored_cid, _size) = self
                .tree
                .put_file(&bytes)
                .await
                .context("store mirrored profile event")?;
            let follow_distance = follow_distance_for_event(event)?;
            let search_value = serialize_profile_search_entry(&build_profile_search_entry(
                event,
                &mirrored_cid,
                follow_distance,
            )?)?;
            by_pubkey_entries.push((pubkey.clone(), mirrored_cid.clone()));
            for term in profile_search_terms_for_event(event) {
                search_entries.push((
                    format!("{PROFILE_SEARCH_PREFIX}{term}:{pubkey}"),
                    search_value.clone(),
                ));
            }
        }

        let by_pubkey_root = self
            .index
            .build_links(by_pubkey_entries)
            .await
            .context("bulk build mirrored profile-by-pubkey index")?;
        let search_root = self
            .index
            .build(search_entries)
            .await
            .context("bulk build mirrored profile search index")?;
        Ok((by_pubkey_root, search_root))
    }

    pub(super) async fn rebuild_profile_events_async_and_commit_with_distances_locked<'a, I, F>(
        &self,
        events: I,
        follow_distance_for_event: F,
    ) -> Result<()>
    where
        I: IntoIterator<Item = &'a Event>,
        F: FnMut(&Event) -> Result<Option<u32>>,
    {
        self.recover_pending_root_pair_commit_locked()?;
        let (by_pubkey_root, search_root) = self
            .rebuild_profile_events_async_with_distances_locked(events, follow_distance_for_event)
            .await?;
        self.write_roots_with_hooks_locked(
            by_pubkey_root.as_ref(),
            search_root.as_ref(),
            || Ok(()),
            || Ok(()),
        )
    }

    pub(super) fn update_profile_events_and_commit_locked(
        &self,
        updates: &[(&Event, Option<u32>, bool, bool)],
    ) -> Result<bool> {
        if updates.is_empty() {
            return Ok(false);
        }
        self.recover_pending_root_pair_commit_locked()?;
        let (by_pubkey_root, search_root) = self.roots_locked()?;
        let (next_by_pubkey_root, next_search_root, changed) = self.update_profile_events_locked(
            by_pubkey_root.as_ref(),
            search_root.as_ref(),
            updates,
        )?;
        if changed {
            self.write_roots_with_hooks_locked(
                next_by_pubkey_root.as_ref(),
                next_search_root.as_ref(),
                || Ok(()),
                || Ok(()),
            )?;
        }
        Ok(changed)
    }

    pub(super) fn update_profile_events_locked(
        &self,
        by_pubkey_root: Option<&Cid>,
        search_root: Option<&Cid>,
        updates: &[(&Event, Option<u32>, bool, bool)],
    ) -> Result<(Option<Cid>, Option<Cid>, bool)> {
        if updates.is_empty() {
            return Ok((by_pubkey_root.cloned(), search_root.cloned(), false));
        }

        let pubkeys = updates
            .iter()
            .map(|(event, _, _, _)| event.pubkey.to_hex())
            .collect::<Vec<_>>();
        let existing_cids = block_on(self.index.get_links(by_pubkey_root, pubkeys))
            .context("lookup existing mirrored profile events")?;
        let buffered_store = Arc::new(BufferedStore::new_optimistic(Arc::clone(&self.store)));
        let buffered_tree = HashTree::new(HashTreeConfig::new(Arc::clone(&buffered_store)));
        let buffered_index = BTree::new(
            Arc::clone(&buffered_store),
            hashtree_index::BTreeOptions {
                order: Some(PROFILE_SEARCH_INDEX_ORDER),
            },
        );
        let mut by_pubkey_changes = BTreeMap::<String, Option<Cid>>::new();
        let mut search_changes = BTreeMap::<String, Option<String>>::new();

        for (event, follow_distance, remove, force_existing_search_value) in updates {
            let pubkey = event.pubkey.to_hex();
            let existing_event = match existing_cids.get(&pubkey) {
                Some(cid) => self.load_profile_event(cid)?,
                None => None,
            };

            if *remove {
                if existing_cids.contains_key(&pubkey) {
                    by_pubkey_changes.insert(pubkey.clone(), None);
                }
                if let Some(current) = existing_event.as_ref() {
                    for term in profile_search_terms_for_event(current) {
                        search_changes
                            .insert(format!("{PROFILE_SEARCH_PREFIX}{term}:{pubkey}"), None);
                    }
                }
                continue;
            }

            let existing_order = existing_event
                .as_ref()
                .map(|current| compare_nostr_events(event, current));
            if existing_order.is_some_and(std::cmp::Ordering::is_lt)
                || (existing_order.is_some_and(std::cmp::Ordering::is_eq)
                    && !force_existing_search_value)
            {
                continue;
            }
            let mirrored_cid = if existing_order.is_some_and(std::cmp::Ordering::is_eq) {
                existing_cids
                    .get(&pubkey)
                    .cloned()
                    .context("existing profile event has no mirrored CID")?
            } else {
                let bytes = event.as_json().into_bytes();
                let mirrored_cid = block_on(buffered_tree.put_file(&bytes))
                    .map(|(cid, _size)| cid)
                    .context("buffer mirrored profile event")?;
                by_pubkey_changes.insert(pubkey.clone(), Some(mirrored_cid.clone()));
                mirrored_cid
            };
            if let Some(current) = existing_event.as_ref() {
                for term in profile_search_terms_for_event(current) {
                    search_changes.insert(format!("{PROFILE_SEARCH_PREFIX}{term}:{pubkey}"), None);
                }
            }

            let search_value = serialize_profile_search_entry(&build_profile_search_entry(
                event,
                &mirrored_cid,
                *follow_distance,
            )?)?;
            for term in profile_search_terms_for_event(event) {
                search_changes.insert(
                    format!("{PROFILE_SEARCH_PREFIX}{term}:{pubkey}"),
                    Some(search_value.clone()),
                );
            }
        }

        if by_pubkey_changes.is_empty() && search_changes.is_empty() {
            return Ok((by_pubkey_root.cloned(), search_root.cloned(), false));
        }

        let next_by_pubkey_root =
            block_on(buffered_index.update_links(by_pubkey_root, by_pubkey_changes))
                .context("batch update mirrored profile event index")?;
        let next_search_root = block_on(buffered_index.update(search_root, search_changes))
            .context("batch update profile search terms")?;
        block_on(buffered_store.flush()).context("flush batched profile index update")?;
        Ok((next_by_pubkey_root, next_search_root, true))
    }
}

pub(super) fn latest_metadata_events_by_pubkey(events: &[Event]) -> BTreeMap<String, &Event> {
    let mut latest_by_pubkey = BTreeMap::<String, &Event>::new();
    for event in events.iter().filter(|event| event.kind == Kind::Metadata) {
        let pubkey = event.pubkey.to_hex();
        match latest_by_pubkey.get(&pubkey) {
            Some(current) if compare_nostr_events(event, current).is_le() => {}
            _ => {
                latest_by_pubkey.insert(pubkey, event);
            }
        }
    }
    latest_by_pubkey
}

fn serialize_profile_search_entry(entry: &StoredProfileSearchEntry) -> Result<String> {
    serde_json::to_string(entry).context("encode stored profile search entry json")
}

fn cid_to_nhash(cid: &Cid) -> Result<String> {
    nhash_encode_full(&NHashData {
        hash: cid.hash,
        decrypt_key: cid.key,
    })
    .context("encode mirrored profile event nhash")
}

pub(super) fn build_profile_search_entry(
    event: &Event,
    mirrored_cid: &Cid,
    follow_distance: Option<u32>,
) -> Result<StoredProfileSearchEntry> {
    let profile = match serde_json::from_str::<serde_json::Value>(&event.content) {
        Ok(serde_json::Value::Object(profile)) => profile,
        _ => serde_json::Map::new(),
    };
    let names = extract_profile_names(&profile);
    let primary_name = names.first().cloned();
    let nip05 = normalize_profile_nip05(&profile, primary_name.as_deref());
    let name = primary_name
        .clone()
        .or_else(|| nip05.clone())
        .unwrap_or_else(|| event.pubkey.to_hex());

    Ok(StoredProfileSearchEntry {
        pubkey: event.pubkey.to_hex(),
        name,
        aliases: names.into_iter().skip(1).collect(),
        nip05,
        follow_distance,
        created_at: event.created_at.as_secs(),
        event_nhash: cid_to_nhash(mirrored_cid)?,
    })
}

fn filter_list_options(filter: &Filter, limit: usize, _exact: bool) -> ListEventsOptions {
    ListEventsOptions {
        limit: Some(limit.max(1)),
        since: filter.since.map(|timestamp| timestamp.as_secs()),
        until: filter.until.map(|timestamp| timestamp.as_secs()),
    }
}

pub(super) fn dedupe_events(events: Vec<Event>) -> Vec<Event> {
    let mut seen = HashSet::new();
    let mut deduped = Vec::new();
    for event in events {
        if seen.insert(event.id.to_bytes()) {
            deduped.push(event);
        }
    }
    deduped.sort_by(|a, b| {
        b.created_at
            .as_secs()
            .cmp(&a.created_at.as_secs())
            .then_with(|| a.id.cmp(&b.id))
    });
    deduped
}

fn extend_unique_events(
    events: &mut Vec<Event>,
    seen: &mut HashSet<[u8; 32]>,
    next_events: Vec<Event>,
    limit: usize,
) {
    for event in next_events {
        if seen.insert(event.id.to_bytes()) {
            events.push(event);
            if events.len() >= limit {
                break;
            }
        }
    }
}
