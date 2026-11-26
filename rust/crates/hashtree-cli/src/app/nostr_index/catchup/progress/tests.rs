use super::*;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use hashtree_core::MemoryStore;
use hashtree_nostr::{stored_event_from_nostr_sdk_event, NostrEventStore, NostrEventStoreOptions};
use nostr::{EventBuilder, Keys, Kind, Timestamp};

#[test]
fn authors_share_budget_and_disabled_trace_uses_none() {
    let budget = TraceBudget::with_limit(600);
    let cache = CatchupReadCache::new(Arc::new(MemoryStore::new()), 1024);
    let mut output = Vec::new();
    for ordinal in 0..3 {
        let trace = AppendTrace::new(true, ordinal, &budget);
        trace.write_to(format_args!("phase=Append"), &cache, &mut output);
    }
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("author_ordinal=0"));
    assert!(output.contains("author_ordinal=1"));
    assert!(!output.contains("author_ordinal=2"));
    assert_eq!(output.matches("budget_exhausted").count(), 1);
    assert!(output.len() <= 600);

    let unused = TraceBudget::with_limit(700);
    let mut disabled_output = Vec::new();
    for ordinal in 0..100 {
        AppendTrace::new(false, ordinal, &unused).write_to(
            format_args!("phase=Append"),
            &cache,
            &mut disabled_output,
        );
    }
    assert!(disabled_output.is_empty());
    let enabled = AppendTrace::new(true, 0, &unused);
    enabled.write_to(format_args!("phase=Append"), &cache, &mut disabled_output);
    assert!(String::from_utf8(disabled_output)
        .unwrap()
        .contains("author_ordinal=0"));
}

#[tokio::test]
async fn exhausted_trace_preserves_append_roots_bytes_and_io() {
    let keys = Keys::generate();
    let events = (0..6)
        .map(|ordinal| {
            let event = EventBuilder::new(Kind::TextNote, format!("private-fixture-{ordinal}"))
                .custom_created_at(Timestamp::from_secs(1000 + ordinal))
                .sign_with_keys(&keys)
                .unwrap();
            stored_event_from_nostr_sdk_event(&event)
        })
        .collect::<Vec<_>>();
    let mut reference = None;
    for enabled in [false, true] {
        let base = Arc::new(MemoryStore::new());
        let cache = Arc::new(CatchupReadCache::new(base.clone(), 1024 * 1024));
        let writer = NostrEventStore::with_options(
            cache.clone(),
            NostrEventStoreOptions {
                index_commit_batch_size: Some(1),
                ..Default::default()
            },
        )
        .with_index_write_buffer_bytes(8 * 1024 * 1024);
        let budget = TraceBudget::with_limit(700);
        let output = Mutex::new(Vec::new());
        let mut root = None;
        let mut reports = Vec::new();
        for (ordinal, incoming) in events.chunks(2).enumerate() {
            let trace = AppendTrace::new(enabled, ordinal, &budget);
            let observer = |row: NostrAppendProgress| {
                trace.write_to(
                    format_args!("phase={:?}", row.phase),
                    &cache,
                    &mut *output.lock().unwrap(),
                );
            };
            let report = writer
                .build_with_superseded_nodes_and_observer(
                    root.as_ref(),
                    incoming.to_vec(),
                    enabled.then_some(&observer),
                )
                .await
                .unwrap();
            root = report.root.clone();
            reports.push((report.root, report.superseded_nodes));
        }
        let counters = (cache.read_stats(), cache.write_stats());
        let mut blobs = BTreeMap::new();
        for hash in base.keys() {
            blobs.insert(hash, base.get(&hash).await.unwrap().unwrap());
        }
        let result = (reports, blobs, counters);
        if let Some(expected) = &reference {
            assert_eq!(&result, expected);
        } else {
            reference = Some(result);
        }
        let output = String::from_utf8(output.into_inner().unwrap()).unwrap();
        if enabled {
            assert_eq!(output.matches("budget_exhausted").count(), 1);
            assert!(output.ends_with("budget_exhausted\n"));
            assert!(output.len() <= 700);
            println!(
                "append-budget-evidence {}",
                serde_json::json!({
                "append_calls": 3, "events": 6, "test_budget_bytes": 700,
                    "trace_bytes": output.len(), "exhaustion_markers": 1,
                    "same_roots_and_superseded_nodes_and_bytes_and_io": true,
                })
            );
        } else {
            assert!(output.is_empty());
        }
    }
}
