//! Bounded local reads for reconciliation; never uses the general query path.
use anyhow::{ensure, Result};
use hashtree_core::{Cid, HashTree, HashTreeConfig, Store};
use hashtree_index::{BTree, BTreeOptions};
use hashtree_nostr::NostrEventStore;
use nostr::{Event, Filter};
use std::{collections::HashSet, io, sync::Arc};

pub(crate) const MAX_EVENT_BYTES: usize = 512 * 1024;
const MAX_CANDIDATES: usize = 4096;

pub(crate) struct ReadBudget {
    remaining_candidates: usize,
    remaining_read_bytes: usize,
    remaining_event_bytes: usize,
    remaining_records: usize,
}

impl ReadBudget {
    pub(crate) fn new(records: usize, bytes: usize) -> Self {
        Self {
            remaining_candidates: MAX_CANDIDATES,
            remaining_read_bytes: bytes,
            remaining_event_bytes: bytes,
            remaining_records: records,
        }
    }
}

/// Count JSON bytes without allocating another copy of an already resident event.
pub(crate) fn event_size(event: &Event, limit: usize) -> Result<usize> {
    struct Counter {
        used: usize,
        limit: usize,
    }
    impl io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.used) {
                return Err(io::Error::other("reconciliation event byte limit"));
            }
            self.used += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter {
        used: 0,
        limit: limit.min(MAX_EVENT_BYTES),
    };
    serde_json::to_writer(&mut counter, event)?;
    Ok(counter.used)
}

pub(crate) async fn query<S: Store>(
    store: Arc<S>,
    root: Option<&Cid>,
    filter: &Filter,
    budget: &mut ReadBudget,
) -> Result<Vec<Event>> {
    ensure!(
        filter
            .authors
            .as_ref()
            .is_none_or(|authors| authors.len() <= MAX_CANDIDATES)
            && filter
                .kinds
                .as_ref()
                .is_none_or(|kinds| kinds.len() <= MAX_CANDIDATES),
        "reconciliation index query limit"
    );
    let events = NostrEventStore::new(Arc::clone(&store));
    let manifest = events.get_manifest(root).await?;
    ensure!(
        manifest.by_id.is_some()
            || (manifest.by_author_time.is_none()
                && manifest.by_author_kind_time.is_none()
                && manifest.by_kind_time.is_none()
                && manifest.by_kind_time_author.is_none()
                && manifest.by_time.is_none()
                && manifest.by_tag.is_none()
                && manifest.replaceable.is_none()
                && manifest.parameterized_replaceable.is_none()),
        "missing reconciliation identity index"
    );
    let index = BTree::new(Arc::clone(&store), BTreeOptions::default());
    let tree = HashTree::new(HashTreeConfig::new(store));
    let mut links = Vec::new();
    if let Some(ids) = filter.ids.as_ref() {
        ensure!(
            ids.len() <= budget.remaining_candidates,
            "reconciliation candidate limit"
        );
        budget.remaining_candidates -= ids.len();
        for id in ids {
            if let Some(cid) = index
                .get_link(manifest.by_id.as_ref(), &id.to_hex())
                .await?
            {
                links.push((id.to_hex(), cid));
            }
        }
    } else {
        // A bounded candidate set may conservatively reject a selective filter;
        // it must never claim an incomplete scan is a complete empty match.
        let mut sources = Vec::new();
        if let (Some(authors), Some(kinds), Some(root)) = (
            &filter.authors,
            &filter.kinds,
            &manifest.by_author_kind_time,
        ) {
            ensure!(
                authors.len().saturating_mul(kinds.len()) <= MAX_CANDIDATES,
                "reconciliation index query limit"
            );
            for author in authors {
                for kind in kinds {
                    sources.push((root, format!("{}:{:08x}:", author.to_hex(), kind.as_u16())));
                }
            }
        } else if let (Some(authors), Some(root)) = (&filter.authors, &manifest.by_author_time) {
            for author in authors {
                sources.push((root, format!("{}:", author.to_hex())));
            }
        } else if let (Some(kinds), Some(root)) = (&filter.kinds, &manifest.by_kind_time) {
            for kind in kinds {
                sources.push((root, format!("{:08x}:", kind.as_u16())));
            }
        } else if let Some(root) = &manifest.by_id {
            sources.push((root, String::new()));
        }
        for (root, prefix) in sources {
            let next = if prefix.is_empty() {
                index
                    .links_entries_limited(Some(root), budget.remaining_candidates + 1)
                    .await?
            } else {
                index
                    .prefix_links_limited(root, &prefix, budget.remaining_candidates + 1)
                    .await?
            };
            ensure!(
                next.len() <= budget.remaining_candidates,
                "reconciliation candidate limit"
            );
            budget.remaining_candidates -= next.len();
            links.extend(next);
        }
    }
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    for (_, cid) in links {
        if !seen.insert((cid.hash, cid.key)) {
            continue;
        }
        let maximum = MAX_EVENT_BYTES.min(budget.remaining_read_bytes);
        let bytes = tree
            .get(&cid, Some(maximum as u64))
            .await?
            .ok_or_else(|| anyhow::anyhow!("missing reconciliation event blob"))?;
        budget.remaining_read_bytes -= bytes.len();
        let event = events
            .decode_verified_event(&bytes)?
            .to_nostr_sdk_event()?
            .into_event();
        if filter.match_event(&event, Default::default()) {
            ensure!(budget.remaining_records > 0, "reconciliation record limit");
            budget.remaining_event_bytes -= event_size(&event, budget.remaining_event_bytes)?;
            budget.remaining_records -= 1;
            result.push(event);
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashtree_core::{HashTreeError, MemoryStore};
    use hashtree_nostr::stored_event_from_nostr_sdk_event;
    use nostr::{EventBuilder, Keys, Kind};

    async fn fixture(events: &[Event]) -> (Arc<MemoryStore>, Cid) {
        let store = Arc::new(MemoryStore::new());
        let root = NostrEventStore::new(Arc::clone(&store))
            .build(None, events.iter().map(stored_event_from_nostr_sdk_event))
            .await
            .unwrap()
            .unwrap();
        (store, root)
    }

    #[tokio::test]
    async fn negentropy_bounded_reader_matches_full_graph_filters_and_exact_ids() {
        let mut events = Vec::new();
        for _ in 0..3 {
            let keys = Keys::generate();
            for kind in [Kind::ContactList, Kind::MuteList] {
                events.push(EventBuilder::new(kind, "").sign_with_keys(&keys).unwrap());
            }
        }
        let (store, root) = fixture(&events).await;
        for filter in [
            Filter::new(),
            Filter::new()
                .authors(events.iter().map(|e| e.pubkey))
                .kinds([Kind::ContactList, Kind::MuteList]),
            Filter::new().ids(events.iter().map(|e| e.id)),
            Filter::new().kind(Kind::ContactList),
        ] {
            let got = query(
                Arc::clone(&store),
                Some(&root),
                &filter,
                &mut ReadBudget::new(10, 1024 * 1024),
            )
            .await
            .unwrap();
            assert_eq!(
                got.iter().map(|e| e.id).collect::<HashSet<_>>(),
                events
                    .iter()
                    .filter(|e| filter.match_event(e, Default::default()))
                    .map(|e| e.id)
                    .collect::<HashSet<_>>()
            );
        }
    }

    #[tokio::test]
    async fn negentropy_bounded_reader_rejects_candidate_and_record_exhaustion() {
        let keys = Keys::generate();
        let events = (0..3)
            .map(|i| {
                EventBuilder::new(Kind::TextNote, format!("fixture {i}"))
                    .sign_with_keys(&keys)
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let (store, root) = fixture(&events).await;
        let mut budget = ReadBudget::new(10, 1024 * 1024);
        budget.remaining_candidates = 2;
        let error = query(
            Arc::clone(&store),
            Some(&root),
            &Filter::new().kind(Kind::TextNote),
            &mut budget,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("candidate limit"));
        assert_eq!(
            budget.remaining_read_bytes,
            1024 * 1024,
            "reject before loading event bodies"
        );
        assert!(query(
            store,
            Some(&root),
            &Filter::new(),
            &mut ReadBudget::new(2, 1024 * 1024)
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("record limit"));
    }

    #[tokio::test]
    async fn negentropy_bounded_reader_applies_body_size_limit_before_decoding() {
        let event = EventBuilder::new(Kind::TextNote, "x".repeat(MAX_EVENT_BYTES + 1))
            .sign_with_keys(&Keys::generate())
            .unwrap();
        assert!(event_size(&event, 16 * 1024 * 1024).is_err());
        let (store, root) = fixture(&[event]).await;
        let error = query(
            store,
            Some(&root),
            &Filter::new(),
            &mut ReadBudget::new(10, 16 * 1024 * 1024),
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<HashTreeError>(),
            Some(HashTreeError::SizeLimitExceeded { .. })
        ));
    }
}
