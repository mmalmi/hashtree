//! Repair an encountered author's legacy missing ID links without rewriting events.
use super::*;

#[derive(Debug)]
pub struct NostrIdReconciliation {
    pub root: Cid,
    pub entries_scanned: u64,
    pub ids_restored: u64,
}

impl<S: Store> NostrEventStore<S> {
    /// Audit every ordinary note already retained by the author projection.
    /// `page_size` bounds each read, not the total work for the author.
    /// All prior immutable roots are retained.
    /// The caller must force-sync before committing the returned root/frontier.
    pub async fn reconcile_author_note_ids(
        &self,
        root: &Cid,
        author: &str,
        page_size: usize,
    ) -> Result<NostrIdReconciliation, NostrEventStoreError> {
        validate_lower_hex(author, 64, "pubkey")?;
        if !(1..=1024).contains(&page_size) {
            return Err(NostrEventStoreError::Validation(
                "ID reconciliation page size must be between 1 and 1024".into(),
            ));
        }
        let mut manifest = self.get_manifest(Some(root)).await?;
        let author_root = manifest.by_author_kind_time.clone().ok_or_else(|| {
            NostrEventStoreError::Validation(
                "ID reconciliation needs an author-kind-time index".into(),
            )
        })?;
        if manifest.by_id.is_none() {
            return Err(NostrEventStoreError::Validation(
                "ID reconciliation needs an existing by-ID index".into(),
            ));
        }
        let prefix = format!("{author}:{}:", pad_kind(1));
        // ':' is the last byte of the fixed prefix, so ';' bounds exactly this author/kind.
        let end = format!("{author}:{};", pad_kind(1));
        let mut start = prefix.clone();
        let mut after = false;
        let mut entries_scanned = 0u64;
        let mut ids_restored = 0u64;
        loop {
            let page = self
                .index
                .range_links_required(&author_root, &start, &end, page_size, after)
                .await?;
            if page.is_empty() {
                break;
            }
            ids_restored += self
                .reconcile_note_id_entries(&mut manifest, author, &page)
                .await?;
            entries_scanned += page.len() as u64;
            start = page.last().expect("nonempty page").0.clone();
            after = true;
            if page.len() < page_size {
                break;
            }
        }
        let next = if ids_restored == 0 {
            root.clone()
        } else {
            self.write_manifest(&manifest).await?.ok_or_else(|| {
                NostrEventStoreError::Validation("ID reconciliation discarded nonempty root".into())
            })?
        };
        Ok(NostrIdReconciliation {
            root: next,
            entries_scanned,
            ids_restored,
        })
    }

    /// Reconcile the bounded retention sample needed by an append publication.
    ///
    /// The publisher checks the newest three ordinary notes before and after an
    /// append, without a time cutoff. The latter are a subset of the former plus
    /// incoming notes. Also repair existing projections for incoming notes before
    /// normal ingestion, preserving their original CID and collection counts.
    /// Older unsampled/unfetched gaps remain for the explicit full-history audit.
    pub async fn reconcile_author_note_ids_for_append(
        &self,
        root: &Cid,
        author: &str,
        incoming: &[StoredNostrEvent],
    ) -> Result<NostrIdReconciliation, NostrEventStoreError> {
        validate_lower_hex(author, 64, "pubkey")?;
        let mut manifest = self.get_manifest(Some(root)).await?;
        let author_root = manifest.by_author_kind_time.clone().ok_or_else(|| {
            NostrEventStoreError::Validation(
                "ID reconciliation needs an author-kind-time index".into(),
            )
        })?;
        if manifest.by_id.is_none() {
            return Err(NostrEventStoreError::Validation(
                "ID reconciliation needs an existing by-ID index".into(),
            ));
        }
        let prefix = format!("{author}:{}:", pad_kind(1));
        let end = format!("{author}:{};", pad_kind(1));
        let mut selected = self
            .index
            .range_links_required(&author_root, &prefix, &end, 3, false)
            .await?
            .into_iter()
            .collect::<BTreeMap<_, _>>();
        // The fetch policy already bounds incoming events. Keep each required
        // point lookup bounded too; never enumerate the author's older history.
        for chunk in incoming.chunks(256) {
            let mut keys = BTreeSet::new();
            for event in chunk.iter().filter(|event| event.kind == 1) {
                if event.pubkey != author {
                    return Err(NostrEventStoreError::Validation(
                        "incoming note belongs to another author".into(),
                    ));
                }
                validate_lower_hex(&event.id, 64, "event id")?;
                keys.insert(author_kind_time_key(event));
            }
            selected.extend(self.index.get_links_required(&author_root, keys).await?);
        }
        let selected = selected.into_iter().collect::<Vec<_>>();
        let mut ids_restored = 0;
        for page in selected.chunks(256) {
            ids_restored += self
                .reconcile_note_id_entries(&mut manifest, author, page)
                .await?;
        }
        let next = if ids_restored == 0 {
            root.clone()
        } else {
            self.write_manifest(&manifest).await?.ok_or_else(|| {
                NostrEventStoreError::Validation("ID reconciliation discarded nonempty root".into())
            })?
        };
        Ok(NostrIdReconciliation {
            root: next,
            entries_scanned: selected.len() as u64,
            ids_restored,
        })
    }

    async fn reconcile_note_id_entries(
        &self,
        manifest: &mut NostrEventManifest,
        author: &str,
        entries: &[(String, Cid)],
    ) -> Result<u64, NostrEventStoreError> {
        let prefix = format!("{author}:{}:", pad_kind(1));
        let mut ids = Vec::with_capacity(entries.len());
        for (key, _) in entries {
            let suffix = key.strip_prefix(&prefix).ok_or_else(|| {
                NostrEventStoreError::Validation("ID reconciliation escaped author range".into())
            })?;
            let (timestamp, id) = suffix.split_once(':').ok_or_else(|| {
                NostrEventStoreError::Validation("invalid author-kind-time key".into())
            })?;
            validate_lower_hex(timestamp, 16, "reverse timestamp")?;
            validate_lower_hex(id, 64, "event id")?;
            ids.push(id.to_owned());
        }
        let existing = self
            .index
            .get_links_required(
                manifest.by_id.as_ref().expect("checked by-ID root"),
                ids.clone(),
            )
            .await?;
        let mut changes = BTreeMap::new();
        for ((key, cid), id) in entries.iter().zip(ids) {
            match existing.get(&id) {
                Some(found) if found == cid => continue,
                Some(_) => {
                    return Err(NostrEventStoreError::Validation(format!(
                        "by-ID and author projection disagree for {id}"
                    )))
                }
                None => {}
            }
            let event =
                VerifiedStoredNostrEvent::try_from(self.load_event_blob(cid).await?)?.into_stored();
            if event.kind != 1
                || event.pubkey != author
                || event.id != id
                || author_kind_time_key(&event) != *key
            {
                return Err(NostrEventStoreError::Validation(
                    "retained signed note does not match its author projection key".into(),
                ));
            }
            changes.insert(id, Some(cid.clone()));
        }
        let ids_restored = changes.len() as u64;
        if !changes.is_empty() {
            // Only add proven-absent links; never reconstruct any other index.
            manifest.by_id = self
                .index
                .update_links(manifest.by_id.as_ref(), changes)
                .await?;
            self.store.flush_pending().await.map_err(|error| {
                NostrEventStoreError::HashTree(HashTreeError::Store(error.to_string()))
            })?;
        }
        Ok(ids_restored)
    }
}
