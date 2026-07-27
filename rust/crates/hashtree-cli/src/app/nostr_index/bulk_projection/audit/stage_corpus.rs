use std::path::PathBuf;

use anyhow::{Context, Result};
use hashtree_core::Store;
use hashtree_nostr::{NostrEventStore, VerifiedStoredNostrEvent};
use heed::types::Bytes;
use heed::Database;
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::super::super::{
    load_stage_segment_with_bytes, parse_root_text, IndexedNostrCrawlPolicy,
};
use super::super::BulkProjectionSpool;

const STAGE_EVENT_LOAD_BATCH_SIZE: usize = 2_048;
const RETAINED_RECORDS_DIGEST_DOMAIN: &[u8] = b"hashtree-nostr-v3-stage-retained-records-v1\0";
const REPLACEABLE_SLOTS_DIGEST_DOMAIN: &[u8] = b"hashtree-nostr-v3-stage-replaceable-slots-v1\0";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct V3StageCorpusSpec {
    pub(super) staging_data_dir: PathBuf,
    pub(super) policy: IndexedNostrCrawlPolicy,
    pub(super) expected_segment_count: usize,
    pub(super) expected_event_cid_count: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(super) struct BulkProjectionStageCorpusAudit {
    pub(super) segment_count: u64,
    pub(super) event_cids_validated: u64,
    pub(super) retained_records: u64,
    pub(super) retained_records_sha256: String,
    pub(super) replaceable_slots: u64,
    pub(super) replaceable_slots_sha256: String,
}

#[derive(Debug)]
struct DatabaseComparison {
    count: u64,
    sha256: String,
}

fn compare_database_exact(
    label: &str,
    digest_domain: &[u8],
    expected: &Database<Bytes, Bytes>,
    expected_txn: &heed::RoTxn<'_>,
    actual: &Database<Bytes, Bytes>,
    actual_txn: &heed::RoTxn<'_>,
) -> Result<DatabaseComparison> {
    let mut expected_rows = expected
        .iter(expected_txn)
        .with_context(|| format!("iterate expected {label}"))?;
    let mut actual_rows = actual
        .iter(actual_txn)
        .with_context(|| format!("iterate retained spool {label}"))?;
    let mut digest = Sha256::new();
    digest.update(digest_domain);
    let mut count = 0u64;

    loop {
        let expected_row = expected_rows
            .next()
            .transpose()
            .with_context(|| format!("read expected {label} row"))?;
        let actual_row = actual_rows
            .next()
            .transpose()
            .with_context(|| format!("read retained spool {label} row"))?;
        match (expected_row, actual_row) {
            (None, None) => break,
            (Some((expected_key, expected_value)), Some((actual_key, actual_value)))
                if expected_key == actual_key && expected_value == actual_value =>
            {
                digest.update((expected_key.len() as u64).to_be_bytes());
                digest.update(expected_key);
                digest.update((expected_value.len() as u64).to_be_bytes());
                digest.update(expected_value);
                count = count
                    .checked_add(1)
                    .with_context(|| format!("{label} row count overflow"))?;
            }
            (Some(_), Some(_)) => {
                anyhow::bail!(
                    "sealed stage corpus differs from retained spool {label} at row {count}"
                );
            }
            (Some(_), None) => {
                anyhow::bail!(
                    "sealed stage corpus differs from retained spool: expected an additional \
                     {label} row at {count}"
                );
            }
            (None, Some(_)) => {
                anyhow::bail!(
                    "sealed stage corpus differs from retained spool: the spool has an \
                     additional {label} row at {count}"
                );
            }
        }
    }
    digest.update(count.to_be_bytes());
    Ok(DatabaseComparison {
        count,
        sha256: hex::encode(digest.finalize()),
    })
}

fn compare_retained_spool_exact(
    expected: &BulkProjectionSpool,
    actual: &BulkProjectionSpool,
) -> Result<(DatabaseComparison, DatabaseComparison)> {
    let expected_txn = expected
        .env
        .read_txn()
        .context("open expected stage-corpus spool read transaction")?;
    let actual_txn = actual
        .env
        .read_txn()
        .context("open retained spool read transaction")?;
    let records = compare_database_exact(
        "event records",
        RETAINED_RECORDS_DIGEST_DOMAIN,
        &expected.events,
        &expected_txn,
        &actual.events,
        &actual_txn,
    )?;
    let slots = compare_database_exact(
        "replaceable slots",
        REPLACEABLE_SLOTS_DIGEST_DOMAIN,
        &expected.slots,
        &expected_txn,
        &actual.slots,
        &actual_txn,
    )?;
    Ok((records, slots))
}

pub(super) async fn reconcile_v3_stage_corpus<S: Store>(
    spec: &V3StageCorpusSpec,
    actual_spool: &BulkProjectionSpool,
    target: &NostrEventStore<S>,
) -> Result<BulkProjectionStageCorpusAudit> {
    let temporary = tempfile::tempdir().context("create v3 stage-corpus audit spool")?;
    let expected_spool =
        BulkProjectionSpool::open(temporary.path()).context("open v3 stage-corpus audit spool")?;
    let mut next_author = 0usize;
    let mut segment_count = 0usize;
    let mut event_cids_validated = 0usize;

    while next_author < spec.policy.author_count {
        let (_, _, segment) =
            load_stage_segment_with_bytes(&spec.staging_data_dir, next_author, &spec.policy)
                .with_context(|| {
                    format!("load sealed v3 stage segment beginning at author {next_author}")
                })?;
        for encoded_cids in segment.event_cids.chunks(STAGE_EVENT_LOAD_BATCH_SIZE) {
            let cids = encoded_cids
                .iter()
                .map(|encoded| {
                    parse_root_text(encoded)
                        .with_context(|| format!("parse sealed stage event CID `{encoded}`"))
                })
                .collect::<Result<Vec<_>>>()?;
            let blobs = target
                .load_validated_event_blobs(cids)
                .await
                .with_context(|| {
                    format!(
                        "load exact durable event blobs for sealed stage segment {}..{}",
                        segment.start_author, segment.end_author
                    )
                })?;
            let mut events = Vec::with_capacity(blobs.len());
            for blob in blobs {
                let event_id = blob.event().id.clone();
                let event = VerifiedStoredNostrEvent::try_from(blob.event().clone())
                    .with_context(|| {
                        format!(
                            "verify event id and Schnorr signature for sealed stage event \
                             {event_id}"
                        )
                    })?
                    .into_stored();
                events.push((event, blob.cid().clone()));
            }
            event_cids_validated = event_cids_validated
                .checked_add(events.len())
                .context("sealed stage event-CID count overflow")?;
            expected_spool
                .apply_expected_corpus(events)
                .context("apply sealed stage corpus retention semantics")?;
        }
        next_author = segment.end_author;
        segment_count = segment_count
            .checked_add(1)
            .context("sealed stage segment count overflow")?;
    }

    if next_author != spec.policy.author_count
        || segment_count != spec.expected_segment_count
        || event_cids_validated != spec.expected_event_cid_count
    {
        anyhow::bail!(
            "sealed stage corpus counters differ from the Candidate Freeze seal: \
             authors={next_author}/{} segments={segment_count}/{} event_cids={event_cids_validated}/{}",
            spec.policy.author_count,
            spec.expected_segment_count,
            spec.expected_event_cid_count
        );
    }

    let comparison = compare_retained_spool_exact(&expected_spool, actual_spool);
    let closing = expected_spool.env.clone().prepare_for_closing();
    drop(expected_spool);
    closing.wait();
    let (records, slots) = comparison?;

    Ok(BulkProjectionStageCorpusAudit {
        segment_count: segment_count as u64,
        event_cids_validated: event_cids_validated as u64,
        retained_records: records.count,
        retained_records_sha256: records.sha256,
        replaceable_slots: slots.count,
        replaceable_slots_sha256: slots.sha256,
    })
}
