use super::*;

pub(super) struct EventIndexBucket {
    pub(super) event_store: NostrEventStore<StorageRouter>,
    pub(super) root_path: PathBuf,
}

pub(super) struct ProfileIndexBucket {
    pub(super) tree: HashTree<StorageRouter>,
    pub(super) index: BTree<StorageRouter>,
    pub(super) by_pubkey_root_path: PathBuf,
    pub(super) search_root_path: PathBuf,
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
        let stored = stored_event_from_nostr(event);
        let _profile = NostrProfileGuard::new("socialgraph.event_store.add");
        block_on(self.event_store.add(root, stored)).map_err(map_event_store_error)
    }

    fn load_event_by_id(&self, root: &Cid, event_id: &str) -> Result<Option<Event>> {
        let stored = block_on(self.event_store.get_by_id(Some(root), event_id))
            .map_err(map_event_store_error)?;
        stored.map(nostr_event_from_stored).transpose()
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
            .map(nostr_event_from_stored)
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
            .map(nostr_event_from_stored)
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
            .map(nostr_event_from_stored)
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
        let options = filter_list_options(filter, limit, exact);
        for value in values {
            let stored = block_on(self.event_store.list_by_tag(
                Some(root),
                tag_name,
                value,
                options.clone(),
            ))
            .map_err(map_event_store_error)?;
            events.extend(
                stored
                    .into_iter()
                    .map(nostr_event_from_stored)
                    .collect::<Result<Vec<_>>>()?,
            );
        }
        Ok(dedupe_events(events))
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
            if authors.len() == 1 && kinds.len() == 1 {
                let author = authors.iter().next().expect("checked single author");
                let exact = filter.generic_tags.is_empty() && filter.search.is_none();
                return Ok(Some(
                    self.load_events_for_author(root, author, filter, limit, exact)?,
                ));
            }

            if kinds.len() < authors.len() {
                let mut events = Vec::new();
                for kind in kinds {
                    events.extend(self.load_events_for_kind(root, *kind, filter, limit, false)?);
                }
                return Ok(Some(dedupe_events(events)));
            }

            let mut events = Vec::new();
            for author in authors {
                events.extend(self.load_events_for_author(root, author, filter, limit, false)?);
            }
            return Ok(Some(dedupe_events(events)));
        }

        if let Some(authors) = filter.authors.as_ref() {
            let mut events = Vec::new();
            let exact = filter.generic_tags.is_empty() && filter.search.is_none();
            for author in authors {
                events.extend(self.load_events_for_author(root, author, filter, limit, exact)?);
            }
            return Ok(Some(dedupe_events(events)));
        }

        if let Some(kinds) = filter.kinds.as_ref() {
            let mut events = Vec::new();
            let exact = filter.authors.is_none()
                && filter.generic_tags.is_empty()
                && filter.search.is_none();
            for kind in kinds {
                events.extend(self.load_events_for_kind(root, *kind, filter, limit, exact)?);
            }
            return Ok(Some(dedupe_events(events)));
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
                        events.push(nostr_event_from_stored(stored)?);
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
                    events.push(nostr_event_from_stored(stored)?);
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
                    if filter.match_event(&event) {
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
                if filter.match_event(&event) {
                    candidates.push(event);
                }
                if candidates.len() >= limit {
                    break;
                }
            }
        }

        candidates.sort_by(|a, b| {
            b.created_at
                .as_u64()
                .cmp(&a.created_at.as_u64())
                .then_with(|| a.id.cmp(&b.id))
        });
        candidates.truncate(limit);
        Ok(candidates)
    }
}

impl ProfileIndexBucket {
    pub(super) fn by_pubkey_root(&self) -> Result<Option<Cid>> {
        let _profile = NostrProfileGuard::new("socialgraph.profile_by_pubkey_root.read");
        read_root_file(&self.by_pubkey_root_path)
    }

    pub(super) fn search_root(&self) -> Result<Option<Cid>> {
        let _profile = NostrProfileGuard::new("socialgraph.profile_search_root.read");
        read_root_file(&self.search_root_path)
    }

    pub(super) fn write_by_pubkey_root(&self, root: Option<&Cid>) -> Result<()> {
        let _profile = NostrProfileGuard::new("socialgraph.profile_by_pubkey_root.write");
        write_root_file(&self.by_pubkey_root_path, root)
    }

    pub(super) fn write_search_root(&self, root: Option<&Cid>) -> Result<()> {
        let _profile = NostrProfileGuard::new("socialgraph.profile_search_root.write");
        write_root_file(&self.search_root_path, root)
    }

    fn mirror_profile_event(&self, event: &Event) -> Result<Cid> {
        let bytes = event.as_json().into_bytes();
        block_on(self.tree.put_file(&bytes))
            .map(|(cid, _size)| cid)
            .context("store mirrored profile event")
    }

    fn load_profile_event(&self, cid: &Cid) -> Result<Option<Event>> {
        let bytes = block_on(self.tree.get(cid, None)).context("read mirrored profile event")?;
        let Some(bytes) = bytes else {
            return Ok(None);
        };
        let json = String::from_utf8(bytes).context("decode mirrored profile event as utf-8")?;
        Ok(Some(
            Event::from_json(json).context("decode mirrored profile event json")?,
        ))
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn profile_event_for_pubkey(&self, pubkey_hex: &str) -> Result<Option<Event>> {
        let root = self.by_pubkey_root()?;
        let Some(cid) = block_on(self.index.get_link(root.as_ref(), pubkey_hex))
            .context("read mirrored profile event cid by pubkey")?
        else {
            return Ok(None);
        };
        self.load_profile_event(&cid)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn search_entries_for_prefix(
        &self,
        prefix: &str,
    ) -> Result<Vec<(String, StoredProfileSearchEntry)>> {
        let Some(root) = self.search_root()? else {
            return Ok(Vec::new());
        };
        let entries =
            block_on(self.index.prefix(&root, prefix)).context("query profile search prefix")?;
        entries
            .into_iter()
            .map(|(key, value)| {
                let entry = serde_json::from_str(&value)
                    .context("decode stored profile search entry json")?;
                Ok((key, entry))
            })
            .collect()
    }

    pub(super) fn rebuild_profile_events<'a, I>(
        &self,
        events: I,
    ) -> Result<(Option<Cid>, Option<Cid>)>
    where
        I: IntoIterator<Item = &'a Event>,
    {
        let mut by_pubkey_entries = Vec::<(String, Cid)>::new();
        let mut search_entries = Vec::<(String, String)>::new();

        for event in events {
            let pubkey = event.pubkey.to_hex();
            let mirrored_cid = self.mirror_profile_event(event)?;
            let search_value =
                serialize_profile_search_entry(&build_profile_search_entry(event, &mirrored_cid)?)?;
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

    pub(super) async fn rebuild_profile_events_async<'a, I>(
        &self,
        events: I,
    ) -> Result<(Option<Cid>, Option<Cid>)>
    where
        I: IntoIterator<Item = &'a Event>,
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
            let search_value =
                serialize_profile_search_entry(&build_profile_search_entry(event, &mirrored_cid)?)?;
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

    pub(super) fn update_profile_event(
        &self,
        by_pubkey_root: Option<&Cid>,
        search_root: Option<&Cid>,
        event: &Event,
    ) -> Result<(Option<Cid>, Option<Cid>, bool)> {
        let pubkey = event.pubkey.to_hex();
        let existing_cid = block_on(self.index.get_link(by_pubkey_root, &pubkey))
            .context("lookup existing mirrored profile event")?;

        let existing_event = match existing_cid.as_ref() {
            Some(cid) => self.load_profile_event(cid)?,
            None => None,
        };

        if existing_event
            .as_ref()
            .is_some_and(|current| compare_nostr_events(event, current).is_le())
        {
            return Ok((by_pubkey_root.cloned(), search_root.cloned(), false));
        }

        let mirrored_cid = self.mirror_profile_event(event)?;
        let next_by_pubkey_root = Some(
            block_on(
                self.index
                    .insert_link(by_pubkey_root, &pubkey, &mirrored_cid),
            )
            .context("write mirrored profile event index")?,
        );

        let mut next_search_root = search_root.cloned();
        if let Some(current) = existing_event.as_ref() {
            for term in profile_search_terms_for_event(current) {
                let Some(root) = next_search_root.as_ref() else {
                    break;
                };
                next_search_root = block_on(
                    self.index
                        .delete(root, &format!("{PROFILE_SEARCH_PREFIX}{term}:{pubkey}")),
                )
                .context("remove stale profile search term")?;
            }
        }

        let search_value =
            serialize_profile_search_entry(&build_profile_search_entry(event, &mirrored_cid)?)?;
        for term in profile_search_terms_for_event(event) {
            next_search_root = Some(
                block_on(self.index.insert(
                    next_search_root.as_ref(),
                    &format!("{PROFILE_SEARCH_PREFIX}{term}:{pubkey}"),
                    &search_value,
                ))
                .context("write profile search term")?,
            );
        }

        Ok((next_by_pubkey_root, next_search_root, true))
    }

    pub(super) fn remove_profile_event(
        &self,
        by_pubkey_root: Option<&Cid>,
        search_root: Option<&Cid>,
        pubkey: &str,
    ) -> Result<(Option<Cid>, Option<Cid>, bool)> {
        let existing_cid = block_on(self.index.get_link(by_pubkey_root, pubkey))
            .context("lookup mirrored profile event for removal")?;
        let Some(existing_cid) = existing_cid else {
            return Ok((by_pubkey_root.cloned(), search_root.cloned(), false));
        };

        let existing_event = self.load_profile_event(&existing_cid)?;
        let next_by_pubkey_root = match by_pubkey_root {
            Some(root) => block_on(self.index.delete(root, pubkey))
                .context("remove mirrored profile-by-pubkey entry")?,
            None => None,
        };

        let mut next_search_root = search_root.cloned();
        if let Some(current) = existing_event.as_ref() {
            for term in profile_search_terms_for_event(current) {
                let Some(root) = next_search_root.as_ref() else {
                    break;
                };
                next_search_root = block_on(
                    self.index
                        .delete(root, &format!("{PROFILE_SEARCH_PREFIX}{term}:{pubkey}")),
                )
                .context("remove overmuted profile search term")?;
            }
        }

        Ok((next_by_pubkey_root, next_search_root, true))
    }
}

pub(super) fn latest_metadata_events_by_pubkey<'a>(
    events: &'a [Event],
) -> BTreeMap<String, &'a Event> {
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

fn build_profile_search_entry(
    event: &Event,
    mirrored_cid: &Cid,
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
        created_at: event.created_at.as_u64(),
        event_nhash: cid_to_nhash(mirrored_cid)?,
    })
}

fn filter_list_options(filter: &Filter, limit: usize, exact: bool) -> ListEventsOptions {
    ListEventsOptions {
        limit: exact.then_some(limit.max(1)),
        since: filter.since.map(|timestamp| timestamp.as_u64()),
        until: filter.until.map(|timestamp| timestamp.as_u64()),
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
            .as_u64()
            .cmp(&a.created_at.as_u64())
            .then_with(|| a.id.cmp(&b.id))
    });
    deduped
}
