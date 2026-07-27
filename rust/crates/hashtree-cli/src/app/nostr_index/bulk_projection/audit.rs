use std::collections::{BTreeMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use hashtree_core::{Cid, HashTree, HashTreeConfig, Store};
use hashtree_index::{BTree, BTreeOptions};
use hashtree_lmdb::{ReadOnlyPoolStore, SHARED_BLOB_POOL_DIR_NAME};
use hashtree_nostr::{
    nostr_event_index_entries, ListEventsOptions, NostrEventIndex, NostrEventStore,
    StoredNostrEvent,
};
use heed::types::Bytes;
use heed::{Database, EnvFlags, EnvOpenOptions};
use nostr::{Event, JsonUtil};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::super::{
    cid_to_nhash, parse_root_text, StagedNostrCrawlState, STAGE_DIR, STAGE_FORMAT_VERSION,
    STAGE_STATE_FILE,
};
use super::{
    bulk_paths, encode_cid, validate_terminal_stage_state, BulkProjectionSpool,
    BulkProjectionState, EntryTrieCursor, SpoolEventRecord, BULK_PROJECTION_VERSION,
};

const PROFILE_RANK_DECISION_FORMAT: &str = "iris-social/profile-search-v3-rank-decisions@1";
const PROFILE_RANK_DECISION_REPORT_FORMAT: &str =
    "iris-social/profile-search-v3-rank-decision-artifacts@1";
const PROFILE_RANK_DECISION_CENSUS_FORMAT: &str = "iris-social/social-graph-crawl-census@2";
const PROFILE_RANK_POLICY: &str = "follow-distance@1";
const PROFILE_EXCLUSION_POLICY: &str = "all-nonselected-graph-identities@1";
const PROFILE_OVERMUTE_THRESHOLD: usize = 1;
const PROFILE_UNREACHABLE_DISTANCE: u32 = 1_000;

#[derive(Debug, Clone)]
pub(crate) struct BulkProjectionAuditOptions {
    pub(crate) staging_data_dir: PathBuf,
    pub(crate) expected_state_sha256: String,
    pub(crate) expected_stage_state_sha256: String,
    pub(crate) expected_policy_sha256: String,
    pub(crate) expected_profile_distance_seal_sha256: Option<String>,
    pub(crate) profile_rank_decisions_file: Option<PathBuf>,
    pub(crate) expected_profile_rank_decisions_file_sha256: Option<String>,
    pub(crate) profile_rank_decisions_report: Option<PathBuf>,
    pub(crate) expected_profile_rank_decisions_report_sha256: Option<String>,
    pub(crate) expected_full_author_count: usize,
    pub(crate) allow_recovery_tranche: bool,
    pub(crate) btree_order: usize,
    pub(crate) page_size: usize,
    pub(crate) query_limit: usize,
    pub(crate) out: PathBuf,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct BulkProjectionIndexAudit {
    index: String,
    root: Option<String>,
    nodes: u64,
    links: u64,
    durable_values_validated: u64,
    entries_sha256: String,
    retained_set_sha256: String,
    first_key: Option<String>,
    last_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct BulkProjectionQueryAudit {
    query: String,
    parameters: serde_json::Value,
    event_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct BulkProjectionBlockEvidence {
    role: String,
    nhash: String,
    sha256: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct BulkProjectionProfileAudit {
    by_pubkey_root: String,
    by_pubkey_root_file_sha256: String,
    by_pubkey_nodes: u64,
    by_pubkey_links: u64,
    by_pubkey_entries_sha256: String,
    search_root: String,
    search_root_file_sha256: String,
    search_nodes: u64,
    search_entries: u64,
    search_entries_sha256: String,
    sample_pubkey: String,
    sample_event_id: String,
    sample_name: String,
    follow_distance_binding: String,
    follow_distance_seal_sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StrictStoredProfileSearchEntry {
    pubkey: String,
    name: String,
    aliases: Vec<String>,
    nip05: Option<String>,
    #[serde(default)]
    follow_distance: Option<u32>,
    created_at: u64,
    event_nhash: String,
}

impl From<StrictStoredProfileSearchEntry> for hashtree_cli::socialgraph::StoredProfileSearchEntry {
    fn from(value: StrictStoredProfileSearchEntry) -> Self {
        Self {
            pubkey: value.pubkey,
            name: value.name,
            aliases: value.aliases,
            nip05: value.nip05,
            follow_distance: value.follow_distance,
            created_at: value.created_at,
            event_nhash: value.event_nhash,
        }
    }
}

#[derive(Debug, Clone)]
struct ExpectedProfileSearchEntry {
    pubkey: String,
    event: Event,
    mirrored_cid: Cid,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct ProfileDistanceProvenance {
    pub(super) format: String,
    pub(super) census_format: String,
    pub(super) rank_decisions_file_sha256: String,
    pub(super) rank_decisions_report_sha256: String,
    pub(super) rank_decisions_sha256: String,
    pub(super) social_graph_root: String,
    pub(super) social_graph_sha256: String,
    pub(super) eligible_authors_sha256: String,
    pub(super) record_count: usize,
    pub(super) eligible_count: usize,
    pub(super) excluded_count: usize,
    pub(super) overmute_threshold: usize,
    pub(super) census_max_distance: Option<u32>,
    pub(super) rank_policy: String,
    pub(super) exclusion_policy: String,
}

#[derive(Debug)]
pub(super) struct TrustedProfileRankDecisions {
    pub(super) decisions: BTreeMap<String, Option<u32>>,
    pub(super) decisions_path: PathBuf,
    pub(super) decisions_bytes: Vec<u8>,
    pub(super) report_path: PathBuf,
    pub(super) report_bytes: Vec<u8>,
    pub(super) evidence: ProfileDistanceProvenance,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProfileRankDecisionHeader {
    format: String,
    eligible_ranks_sha256: String,
    record_count: usize,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProfileRankDecisionRecord {
    pubkey: String,
    decision: String,
    rank_hint: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProfileRankDecisionReport {
    format: String,
    census_format: String,
    social_graph_root: String,
    social_graph_sha256: String,
    eligible_authors_sha256: String,
    overmute_threshold: usize,
    max_distance: Option<u32>,
    rank_policy: String,
    exclusion_policy: String,
    record_count: usize,
    eligible_count: usize,
    excluded_count: usize,
    reachable_count: usize,
    reachable_overmuted_count: usize,
    distance_excluded_count: usize,
    unreachable_count: usize,
    all_graph_overmuted_count: usize,
    rank_decisions_sha256: String,
    rank_decisions_file_sha256: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct BulkProjectionAuditOutput {
    version: u32,
    candidate_root: String,
    state_sha256: String,
    stage_state_sha256: String,
    trusted_policy_sha256: String,
    trusted_profile_distance_seal_sha256: Option<String>,
    profile_distance_provenance: Option<ProfileDistanceProvenance>,
    trusted_full_author_count: usize,
    crawl_policy_max_follow_distance: Option<u32>,
    audit_mode: String,
    cutover_eligible: bool,
    pool_catalog_sha256: String,
    pool_manifest_sha256: String,
    pool_stored_locations: u64,
    authors_processed: usize,
    authors_total: usize,
    recovery_tranche_only: bool,
    indexes: Vec<BulkProjectionIndexAudit>,
    profile: BulkProjectionProfileAudit,
    queries: Vec<BulkProjectionQueryAudit>,
    representative_blocks: Vec<BulkProjectionBlockEvidence>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct EntrySetProof {
    count: u64,
    xor: [u8; 32],
}

impl EntrySetProof {
    fn insert(&mut self, index: NostrEventIndex, key: &str, cid: &Cid) -> Result<()> {
        let mut digest = Sha256::new();
        digest.update(b"hashtree-nostr-retained-index-entry-v1\0");
        digest.update([index.stable_id()]);
        digest.update((key.len() as u64).to_be_bytes());
        digest.update(key.as_bytes());
        let encoded_cid = encode_cid(cid);
        digest.update((encoded_cid.len() as u64).to_be_bytes());
        digest.update(encoded_cid);
        let entry: [u8; 32] = digest.finalize().into();
        for (target, byte) in self.xor.iter_mut().zip(entry) {
            *target ^= byte;
        }
        self.count = self
            .count
            .checked_add(1)
            .context("retained index entry count overflow")?;
        Ok(())
    }

    fn evidence_sha256(self) -> String {
        let mut digest = Sha256::new();
        digest.update(b"hashtree-nostr-retained-index-set-v1\0");
        digest.update(self.count.to_be_bytes());
        digest.update(self.xor);
        hex::encode(digest.finalize())
    }
}

impl BulkProjectionSpool {
    fn open_read_only(path: &Path) -> Result<Self> {
        if !path.join("data.mdb").is_file() {
            anyhow::bail!("bulk projection spool is missing at {}", path.display());
        }
        let mut options = EnvOpenOptions::new();
        options.max_dbs(3).max_readers(32);
        unsafe {
            options.flags(EnvFlags::READ_ONLY | EnvFlags::NO_READ_AHEAD);
        }
        let env = unsafe { options.open(path) }
            .with_context(|| format!("open bulk projection spool {} read-only", path.display()))?;
        let rtxn = env.read_txn()?;
        let open_database = |name| -> Result<Database<Bytes, Bytes>> {
            env.open_database(&rtxn, Some(name))?
                .with_context(|| format!("bulk projection spool omitted {name} database"))
        };
        let events = open_database("events")?;
        let slots = open_database("slots")?;
        let entries = open_database("entries")?;
        // Publishing DBI handles commits only the reader transaction. The
        // environment itself is READ_ONLY; LMDB may update reader slots in
        // lock.mdb, but this cannot mutate data.mdb.
        rtxn.commit()?;
        Ok(Self {
            env,
            entries,
            events,
            slots,
        })
    }

    fn event_record(&self, event_id: &str) -> Result<Option<SpoolEventRecord>> {
        let rtxn = self.env.read_txn()?;
        self.events
            .get(&rtxn, event_id.as_bytes())?
            .map(|encoded| rmp_serde::from_slice(encoded).context("decode bulk spool event record"))
            .transpose()
    }

    fn event_record_count(&self) -> Result<u64> {
        let rtxn = self.env.read_txn()?;
        let mut count = 0u64;
        for item in self.events.iter(&rtxn)? {
            item?;
            count = count
                .checked_add(1)
                .context("bulk spool event record count overflow")?;
        }
        Ok(count)
    }

    fn retained_profile_records(&self) -> Result<BTreeMap<String, SpoolEventRecord>> {
        let rtxn = self.env.read_txn()?;
        let mut profiles = BTreeMap::new();
        for item in self.events.iter(&rtxn)? {
            let (event_id, encoded) = item?;
            let event_id =
                std::str::from_utf8(event_id).context("bulk spool event key is not UTF-8")?;
            let record: SpoolEventRecord =
                rmp_serde::from_slice(encoded).context("decode retained profile spool event")?;
            if record.event.id != event_id {
                anyhow::bail!(
                    "bulk spool event key `{event_id}` differs from record id `{}`",
                    record.event.id
                );
            }
            if record.event.kind != 0 {
                continue;
            }
            if profiles
                .insert(record.event.pubkey.clone(), record)
                .is_some()
            {
                anyhow::bail!("bulk spool retained multiple kind-0 winners for one pubkey");
            }
        }
        Ok(profiles)
    }
}

fn bytes_sha256(bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(bytes);
    hex::encode(digest.finalize())
}

fn require_expected_sha256(label: &str, actual: &str, expected: &str) -> Result<()> {
    if expected.len() != 64
        || !expected
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        anyhow::bail!("{label} SHA-256 pin must be 64 lowercase hexadecimal characters");
    }
    if expected != actual {
        anyhow::bail!("{label} SHA-256 mismatch: expected {expected}, found {actual}");
    }
    Ok(())
}

fn require_sha256_text(label: &str, value: &str) -> Result<()> {
    require_expected_sha256(label, value, value)
}

fn parse_profile_rank_decisions(bytes: &[u8]) -> Result<(BTreeMap<String, Option<u32>>, String)> {
    let text = std::str::from_utf8(bytes).context("profile rank-decisions file is not UTF-8")?;
    if !text.ends_with('\n') || text.contains('\r') {
        anyhow::bail!("profile rank-decisions file must be canonical LF-terminated JSONL");
    }
    let lines = text[..text.len() - 1].split('\n').collect::<Vec<_>>();
    if lines.len() < 2 || lines.iter().any(|line| line.is_empty()) {
        anyhow::bail!("profile rank-decisions file requires one header and records");
    }
    let header: ProfileRankDecisionHeader =
        serde_json::from_str(lines[0]).context("strictly decode profile rank-decisions header")?;
    if serde_json::to_string(&header)? != lines[0]
        || header.format != PROFILE_RANK_DECISION_FORMAT
        || header.record_count == 0
        || header.record_count != lines.len() - 1
    {
        anyhow::bail!("profile rank-decisions header is noncanonical or inconsistent");
    }
    require_sha256_text(
        "embedded profile rank-decisions digest",
        &header.eligible_ranks_sha256,
    )?;

    let mut decisions = BTreeMap::new();
    let mut previous = None::<String>;
    for (position, line) in lines.iter().enumerate().skip(1) {
        let record: ProfileRankDecisionRecord = serde_json::from_str(line)
            .with_context(|| format!("strictly decode profile rank decision {position}"))?;
        if serde_json::to_string(&record)? != *line
            || record.pubkey.len() != 64
            || !record
                .pubkey
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            anyhow::bail!("profile rank decision {position} is noncanonical");
        }
        if previous
            .as_deref()
            .is_some_and(|previous| record.pubkey.as_str() <= previous)
        {
            anyhow::bail!("profile rank decisions are duplicate or not strictly pubkey sorted");
        }
        let rank = match (record.decision.as_str(), record.rank_hint) {
            ("eligible", Some(rank)) if rank < PROFILE_UNREACHABLE_DISTANCE => Some(rank),
            ("excluded", None) => None,
            _ => anyhow::bail!("profile rank decision {position} has invalid decision/rank"),
        };
        previous = Some(record.pubkey.clone());
        decisions.insert(record.pubkey, rank);
    }

    let mut digest = Sha256::new();
    digest.update(PROFILE_RANK_DECISION_FORMAT.as_bytes());
    digest.update(b"\n");
    for (pubkey, rank) in &decisions {
        let row = match rank {
            Some(rank) => serde_json::json!([pubkey, "eligible", rank]),
            None => serde_json::json!([pubkey, "excluded", null]),
        };
        digest.update(serde_json::to_string(&row)?.as_bytes());
        digest.update(b"\n");
    }
    let decisions_sha256 = hex::encode(digest.finalize());
    if decisions_sha256 != header.eligible_ranks_sha256 {
        anyhow::bail!(
            "profile rank-decisions digest mismatch: embedded {} computed {}",
            header.eligible_ranks_sha256,
            decisions_sha256
        );
    }
    Ok((decisions, decisions_sha256))
}

fn eligible_profile_rank_authors_sha256(decisions: &BTreeMap<String, Option<u32>>) -> String {
    let mut digest = Sha256::new();
    for pubkey in decisions
        .iter()
        .filter_map(|(pubkey, rank)| rank.is_some().then_some(pubkey))
    {
        digest.update(pubkey.as_bytes());
        digest.update(b"\n");
    }
    hex::encode(digest.finalize())
}

fn canonical_read_pinned_file(
    label: &str,
    path: &Path,
    expected_sha256: &str,
) -> Result<(PathBuf, Vec<u8>, String)> {
    if !path.is_absolute() {
        anyhow::bail!("{label} path must be absolute");
    }
    let path = path
        .canonicalize()
        .with_context(|| format!("canonicalize {label} {}", path.display()))?;
    if !path.is_file() {
        anyhow::bail!("{label} is not a regular file: {}", path.display());
    }
    let bytes = std::fs::read(&path).with_context(|| format!("read {label} {}", path.display()))?;
    let sha256 = bytes_sha256(&bytes);
    require_expected_sha256(label, &sha256, expected_sha256)?;
    Ok((path, bytes, sha256))
}

fn load_trusted_profile_rank_decisions(
    options: &BulkProjectionAuditOptions,
) -> Result<Option<TrustedProfileRankDecisions>> {
    let supplied = [
        options.profile_rank_decisions_file.is_some(),
        options
            .expected_profile_rank_decisions_file_sha256
            .is_some(),
        options.profile_rank_decisions_report.is_some(),
        options
            .expected_profile_rank_decisions_report_sha256
            .is_some(),
    ];
    if supplied.iter().all(|supplied| !supplied) {
        if options.allow_recovery_tranche {
            return Ok(None);
        }
        anyhow::bail!(
            "full-policy cutover requires the independently derived profile rank-decisions \
             file/report and both exact SHA-256 pins"
        );
    }
    if supplied.iter().any(|supplied| !supplied) {
        anyhow::bail!(
            "profile rank-decisions provenance requires file, report, and both exact SHA-256 pins"
        );
    }
    Ok(Some(load_pinned_profile_rank_decisions(
        options
            .profile_rank_decisions_file
            .as_deref()
            .expect("all provenance options checked"),
        options
            .expected_profile_rank_decisions_file_sha256
            .as_deref()
            .expect("all provenance options checked"),
        options
            .profile_rank_decisions_report
            .as_deref()
            .expect("all provenance options checked"),
        options
            .expected_profile_rank_decisions_report_sha256
            .as_deref()
            .expect("all provenance options checked"),
    )?))
}

pub(super) fn load_pinned_profile_rank_decisions(
    profile_rank_decisions_file: &Path,
    expected_profile_rank_decisions_file_sha256: &str,
    profile_rank_decisions_report: &Path,
    expected_profile_rank_decisions_report_sha256: &str,
) -> Result<TrustedProfileRankDecisions> {
    let (decisions_path, decisions_bytes, decisions_file_sha256) = canonical_read_pinned_file(
        "profile rank-decisions file",
        profile_rank_decisions_file,
        expected_profile_rank_decisions_file_sha256,
    )?;
    let (report_path, report_bytes, report_sha256) = canonical_read_pinned_file(
        "profile rank-decisions report",
        profile_rank_decisions_report,
        expected_profile_rank_decisions_report_sha256,
    )?;
    if decisions_path == report_path {
        anyhow::bail!("profile rank-decisions file and report must be distinct");
    }

    let (decisions, rank_decisions_sha256) = parse_profile_rank_decisions(&decisions_bytes)?;
    let report: ProfileRankDecisionReport = serde_json::from_slice(&report_bytes)
        .context("strictly decode profile rank-decisions report")?;
    for (label, value) in [
        (
            "report social graph SHA-256",
            report.social_graph_sha256.as_str(),
        ),
        (
            "report eligible authors SHA-256",
            report.eligible_authors_sha256.as_str(),
        ),
        (
            "report rank-decisions SHA-256",
            report.rank_decisions_sha256.as_str(),
        ),
        (
            "report rank-decisions file SHA-256",
            report.rank_decisions_file_sha256.as_str(),
        ),
    ] {
        require_sha256_text(label, value)?;
    }
    if report.format != PROFILE_RANK_DECISION_REPORT_FORMAT
        || report.census_format != PROFILE_RANK_DECISION_CENSUS_FORMAT
        || report.rank_policy != PROFILE_RANK_POLICY
        || report.exclusion_policy != PROFILE_EXCLUSION_POLICY
        || report.social_graph_root.len() != 64
        || !report
            .social_graph_root
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        anyhow::bail!("profile rank-decisions report has the wrong provenance contract");
    }
    let eligible_count = decisions.values().filter(|rank| rank.is_some()).count();
    let eligible_authors_sha256 = eligible_profile_rank_authors_sha256(&decisions);
    let excluded_count = decisions.len() - eligible_count;
    let reachable_partition = report
        .eligible_count
        .checked_add(report.reachable_overmuted_count)
        .and_then(|count| count.checked_add(report.distance_excluded_count));
    let excluded_partition = report
        .reachable_overmuted_count
        .checked_add(report.distance_excluded_count)
        .and_then(|count| count.checked_add(report.unreachable_count));
    if report.record_count != decisions.len()
        || report.eligible_count != eligible_count
        || report.excluded_count != excluded_count
        || report.eligible_count.checked_add(report.excluded_count) != Some(report.record_count)
        || report.reachable_count.checked_add(report.unreachable_count) != Some(report.record_count)
        || reachable_partition != Some(report.reachable_count)
        || excluded_partition != Some(report.excluded_count)
        || report.reachable_overmuted_count > report.reachable_count
        || report.all_graph_overmuted_count > report.record_count
        || report.all_graph_overmuted_count < report.reachable_overmuted_count
        || report.overmute_threshold != PROFILE_OVERMUTE_THRESHOLD
        || report.max_distance.is_some_and(|max_distance| {
            decisions
                .values()
                .flatten()
                .any(|rank| *rank > max_distance)
        })
        || report.rank_decisions_sha256 != rank_decisions_sha256
        || report.rank_decisions_file_sha256 != decisions_file_sha256
    {
        anyhow::bail!("profile rank-decisions report does not match its exact decisions artifact");
    }
    if report.eligible_authors_sha256 != eligible_authors_sha256 {
        anyhow::bail!(
            "profile rank-decisions report eligible-author digest {} does not match actual \
             eligible decision keys {}",
            report.eligible_authors_sha256,
            eligible_authors_sha256
        );
    }

    Ok(TrustedProfileRankDecisions {
        decisions,
        decisions_path,
        decisions_bytes,
        report_path,
        report_bytes,
        evidence: ProfileDistanceProvenance {
            format: report.format,
            census_format: report.census_format,
            rank_decisions_file_sha256: decisions_file_sha256,
            rank_decisions_report_sha256: report_sha256,
            rank_decisions_sha256,
            social_graph_root: report.social_graph_root,
            social_graph_sha256: report.social_graph_sha256,
            eligible_authors_sha256,
            record_count: report.record_count,
            eligible_count: report.eligible_count,
            excluded_count: report.excluded_count,
            overmute_threshold: report.overmute_threshold,
            census_max_distance: report.max_distance,
            rank_policy: report.rank_policy,
            exclusion_policy: report.exclusion_policy,
        },
    })
}

pub(super) fn recheck_trusted_profile_rank_decisions(
    trusted: Option<&TrustedProfileRankDecisions>,
) -> Result<()> {
    let Some(trusted) = trusted else {
        return Ok(());
    };
    let decisions = std::fs::read(&trusted.decisions_path).with_context(|| {
        format!(
            "re-read profile rank-decisions file {}",
            trusted.decisions_path.display()
        )
    })?;
    let report = std::fs::read(&trusted.report_path).with_context(|| {
        format!(
            "re-read profile rank-decisions report {}",
            trusted.report_path.display()
        )
    })?;
    if decisions != trusted.decisions_bytes || report != trusted.report_bytes {
        anyhow::bail!("profile rank-decisions provenance changed during read-only audit");
    }
    Ok(())
}

pub(super) fn require_profile_rank_policy_binding(
    trusted: Option<&TrustedProfileRankDecisions>,
    policy_author_allowlist_sha256: &str,
    policy_author_count: usize,
) -> Result<()> {
    let Some(trusted) = trusted else {
        return Ok(());
    };
    if trusted.evidence.eligible_authors_sha256 != policy_author_allowlist_sha256 {
        anyhow::bail!(
            "profile rank-decisions eligible-author digest {} does not match crawl-policy \
             allowlist digest {}",
            trusted.evidence.eligible_authors_sha256,
            policy_author_allowlist_sha256
        );
    }
    if trusted.evidence.eligible_count != policy_author_count {
        anyhow::bail!(
            "profile rank-decisions eligible count {} does not match crawl-policy author count {}",
            trusted.evidence.eligible_count,
            policy_author_count
        );
    }
    Ok(())
}

fn manifest_root_for_index(
    manifest: &hashtree_nostr::NostrEventManifest,
    index: NostrEventIndex,
) -> Option<&Cid> {
    match index {
        NostrEventIndex::ById => manifest.by_id.as_ref(),
        NostrEventIndex::ByAuthorTime => manifest.by_author_time.as_ref(),
        NostrEventIndex::ByAuthorKindTime => manifest.by_author_kind_time.as_ref(),
        NostrEventIndex::ByKindTime => manifest.by_kind_time.as_ref(),
        NostrEventIndex::ByKindTimeAuthor => manifest.by_kind_time_author.as_ref(),
        NostrEventIndex::ByTime => manifest.by_time.as_ref(),
        NostrEventIndex::ByTag => manifest.by_tag.as_ref(),
        NostrEventIndex::Replaceable => manifest.replaceable.as_ref(),
        NostrEventIndex::ParameterizedReplaceable => manifest.parameterized_replaceable.as_ref(),
    }
}

async fn audit_index_root(
    spool: &BulkProjectionSpool,
    target: &NostrEventStore<ReadOnlyPoolStore>,
    btree: &BTree<ReadOnlyPoolStore>,
    index: NostrEventIndex,
    root: Option<&Cid>,
    page_size: usize,
    expected_entries: &mut [EntrySetProof; 9],
) -> Result<(BulkProjectionIndexAudit, EntrySetProof)> {
    let mut digest = Sha256::new();
    digest.update(b"hashtree-nostr-bulk-index-parity-v1\0");
    digest.update(index.name().as_bytes());
    digest.update([0]);
    let mut retained_set = EntrySetProof::default();
    let mut cursor = EntryTrieCursor::new(spool, index);
    let Some(root) = root else {
        if let Some((key, _)) = cursor.next_entry()? {
            anyhow::bail!(
                "manifest {} root is empty but spool starts with `{key}`",
                index.name()
            );
        }
        return Ok((
            BulkProjectionIndexAudit {
                index: index.name().to_string(),
                root: None,
                nodes: 0,
                links: 0,
                durable_values_validated: 0,
                entries_sha256: hex::encode(digest.finalize()),
                retained_set_sha256: retained_set.evidence_sha256(),
                first_key: None,
                last_key: None,
            },
            retained_set,
        ));
    };

    let structural = btree
        .validate_link_tree(Some(root))
        .await
        .with_context(|| format!("exhaustively validate {} link tree", index.name()))?;
    let mut start = None::<String>;
    let mut links = 0u64;
    let mut durable_values_validated = 0u64;
    let mut first_key = None;
    let mut last_key = None;
    loop {
        let page = btree
            .range_links_limited(root, start.as_deref(), None, page_size)
            .await
            .with_context(|| format!("read {} root parity page", index.name()))?;
        if page.is_empty() {
            break;
        }
        for (key, cid) in &page {
            let Some((spool_key, spool_cid)) = cursor.next_entry()? else {
                anyhow::bail!(
                    "{} root has an extra key `{key}` at row {links}",
                    index.name()
                );
            };
            if spool_key != *key {
                anyhow::bail!(
                    "{} key mismatch at row {links}: spool=`{spool_key}` root=`{key}`",
                    index.name()
                );
            }
            if spool_cid != *cid {
                anyhow::bail!("{} CID mismatch at row {links}, key=`{key}`", index.name());
            }
            retained_set.insert(index, key, cid)?;
            if index == NostrEventIndex::ById {
                let record = spool
                    .event_record(key)?
                    .with_context(|| format!("by-id spool key `{key}` has no event record"))?;
                let record_cid = Cid {
                    hash: record.cid_hash,
                    key: record.cid_key,
                };
                if record_cid != *cid {
                    anyhow::bail!("by-id event record CID differs at key `{key}`");
                }
                let durable = target
                    .load_event_blob(cid)
                    .await
                    .with_context(|| format!("exhaustively load durable by-id event `{key}`"))?;
                if durable.id != *key || durable != record.event {
                    anyhow::bail!(
                        "durable by-id event `{key}` differs from its exact spool record"
                    );
                }
                durable_values_validated = durable_values_validated
                    .checked_add(1)
                    .context("bulk index durable value validation count overflow")?;
                let entries = nostr_event_index_entries(&record.event, cid);
                for (position, entry) in entries.iter().enumerate() {
                    if entries[..position]
                        .iter()
                        .any(|seen| seen.index == entry.index && seen.key == entry.key)
                    {
                        continue;
                    }
                    expected_entries[entry.index.stable_id() as usize].insert(
                        entry.index,
                        &entry.key,
                        &entry.cid,
                    )?;
                }
            }
            digest.update((key.len() as u64).to_be_bytes());
            digest.update(key.as_bytes());
            let encoded_cid = encode_cid(cid);
            digest.update((encoded_cid.len() as u64).to_be_bytes());
            digest.update(encoded_cid);
            if first_key.is_none() {
                first_key = Some(key.clone());
            }
            last_key = Some(key.clone());
            links = links
                .checked_add(1)
                .context("bulk index parity link count overflow")?;
        }
        start = Some(format!(
            "{}\0",
            page.last().expect("non-empty index parity page").0
        ));
        if page.len() < page_size {
            break;
        }
    }
    if let Some((key, _)) = cursor.next_entry()? {
        anyhow::bail!(
            "{} root ended at row {links} before spool key `{key}`",
            index.name()
        );
    }
    if links != structural.links {
        anyhow::bail!(
            "{} structural link count {} differs from exact parity count {links}",
            index.name(),
            structural.links
        );
    }
    Ok((
        BulkProjectionIndexAudit {
            index: index.name().to_string(),
            root: Some(cid_to_nhash(root)?),
            nodes: structural.nodes,
            links,
            durable_values_validated,
            entries_sha256: hex::encode(digest.finalize()),
            retained_set_sha256: retained_set.evidence_sha256(),
            first_key,
            last_key,
        },
        retained_set,
    ))
}

async fn load_spool_prefix_events(
    spool: &BulkProjectionSpool,
    target: &NostrEventStore<ReadOnlyPoolStore>,
    index: NostrEventIndex,
    prefix: &str,
    limit: usize,
) -> Result<Vec<StoredNostrEvent>> {
    let mut cursor = EntryTrieCursor::new(spool, index);
    let mut events = Vec::new();
    while let Some((key, cid)) = cursor.next_entry()? {
        if !key.starts_with(prefix) {
            if !events.is_empty() {
                break;
            }
            continue;
        }
        events.push(
            target
                .load_event_blob(&cid)
                .await
                .with_context(|| format!("load {} query parity event `{key}`", index.name()))?,
        );
        if events.len() == limit {
            break;
        }
    }
    Ok(events)
}

fn event_ids(events: &[StoredNostrEvent]) -> Vec<String> {
    events.iter().map(|event| event.id.clone()).collect()
}

fn checked_query(
    query: &str,
    parameters: serde_json::Value,
    expected: &[StoredNostrEvent],
    actual: &[StoredNostrEvent],
) -> Result<BulkProjectionQueryAudit> {
    let expected_ids = event_ids(expected);
    let actual_ids = event_ids(actual);
    if actual_ids != expected_ids {
        anyhow::bail!(
            "{query} query differs from deterministic spool truth: expected={expected_ids:?} actual={actual_ids:?}"
        );
    }
    Ok(BulkProjectionQueryAudit {
        query: query.to_string(),
        parameters,
        event_ids: actual_ids,
    })
}

fn first_spool_entry(spool: &BulkProjectionSpool, index: NostrEventIndex) -> Result<(String, Cid)> {
    EntryTrieCursor::new(spool, index)
        .next_entry()?
        .with_context(|| format!("{} spool has no real query candidate", index.name()))
}

async fn audit_real_queries(
    spool: &BulkProjectionSpool,
    target: &NostrEventStore<ReadOnlyPoolStore>,
    root: &Cid,
    limit: usize,
) -> Result<(Vec<BulkProjectionQueryAudit>, Cid)> {
    let list_options = || ListEventsOptions {
        limit: Some(limit),
        since: None,
        until: None,
    };
    let mut queries = Vec::new();

    let (by_id_key, representative_event_cid) = first_spool_entry(spool, NostrEventIndex::ById)?;
    let expected_by_id =
        load_spool_prefix_events(spool, target, NostrEventIndex::ById, &by_id_key, 1).await?;
    let actual_by_id = target
        .get_by_id(Some(root), &by_id_key)
        .await
        .context("query by-id terminal candidate")?
        .into_iter()
        .collect::<Vec<_>>();
    queries.push(checked_query(
        "by-id",
        serde_json::json!({"id": by_id_key}),
        &expected_by_id,
        &actual_by_id,
    )?);

    let (_, author_cid) = first_spool_entry(spool, NostrEventIndex::ByAuthorTime)?;
    let author_event = target.load_event_blob(&author_cid).await?;
    let author_prefix = format!("{}:", author_event.pubkey);
    let expected_author = load_spool_prefix_events(
        spool,
        target,
        NostrEventIndex::ByAuthorTime,
        &author_prefix,
        limit,
    )
    .await?;
    let actual_author = target
        .list_by_author(Some(root), &author_event.pubkey, list_options())
        .await?;
    queries.push(checked_query(
        "by-author",
        serde_json::json!({"author": author_event.pubkey, "limit": limit}),
        &expected_author,
        &actual_author,
    )?);

    let (_, author_kind_cid) = first_spool_entry(spool, NostrEventIndex::ByAuthorKindTime)?;
    let author_kind_event = target.load_event_blob(&author_kind_cid).await?;
    let author_kind_prefix = format!(
        "{}:{:08x}:",
        author_kind_event.pubkey, author_kind_event.kind
    );
    let expected_author_kind = load_spool_prefix_events(
        spool,
        target,
        NostrEventIndex::ByAuthorKindTime,
        &author_kind_prefix,
        limit,
    )
    .await?;
    let actual_author_kind = target
        .list_by_author_and_kind(
            Some(root),
            &author_kind_event.pubkey,
            author_kind_event.kind,
            list_options(),
        )
        .await?;
    queries.push(checked_query(
        "by-author-kind",
        serde_json::json!({
            "author": author_kind_event.pubkey,
            "kind": author_kind_event.kind,
            "limit": limit
        }),
        &expected_author_kind,
        &actual_author_kind,
    )?);

    let (_, kind_cid) = first_spool_entry(spool, NostrEventIndex::ByKindTime)?;
    let kind_event = target.load_event_blob(&kind_cid).await?;
    let kind_prefix = format!("{:08x}:", kind_event.kind);
    let expected_kind = load_spool_prefix_events(
        spool,
        target,
        NostrEventIndex::ByKindTime,
        &kind_prefix,
        limit,
    )
    .await?;
    let actual_kind = target
        .list_by_kind(Some(root), kind_event.kind, list_options())
        .await?;
    queries.push(checked_query(
        "by-kind",
        serde_json::json!({"kind": kind_event.kind, "limit": limit}),
        &expected_kind,
        &actual_kind,
    )?);

    let expected_recent =
        load_spool_prefix_events(spool, target, NostrEventIndex::ByTime, "", limit).await?;
    let actual_recent = target.list_recent(Some(root), list_options()).await?;
    queries.push(checked_query(
        "recent",
        serde_json::json!({"limit": limit}),
        &expected_recent,
        &actual_recent,
    )?);

    let (tag_key, _) = first_spool_entry(spool, NostrEventIndex::ByTag)?;
    let tag_prefix = tag_key
        .rsplit_once(':')
        .and_then(|(without_id, _)| without_id.rsplit_once(':').map(|(prefix, _)| prefix))
        .context("first by-tag spool key has no timestamp/event suffix")?;
    let (tag_name, tag_value) = tag_prefix
        .split_once(':')
        .context("first by-tag spool key has no name/value prefix")?;
    let expected_tag = load_spool_prefix_events(
        spool,
        target,
        NostrEventIndex::ByTag,
        &format!("{tag_prefix}:"),
        limit,
    )
    .await?;
    let actual_tag = target
        .list_by_tag(Some(root), tag_name, tag_value, list_options())
        .await?;
    queries.push(checked_query(
        "by-tag",
        serde_json::json!({
            "tag": tag_name,
            "value": tag_value,
            "limit": limit
        }),
        &expected_tag,
        &actual_tag,
    )?);

    let (_, replaceable_cid) = first_spool_entry(spool, NostrEventIndex::Replaceable)?;
    let replaceable_event = target.load_event_blob(&replaceable_cid).await?;
    let expected_replaceable = vec![replaceable_event.clone()];
    let actual_replaceable = target
        .get_replaceable(
            Some(root),
            &replaceable_event.pubkey,
            replaceable_event.kind,
        )
        .await?
        .into_iter()
        .collect::<Vec<_>>();
    queries.push(checked_query(
        "replaceable",
        serde_json::json!({
            "author": replaceable_event.pubkey,
            "kind": replaceable_event.kind
        }),
        &expected_replaceable,
        &actual_replaceable,
    )?);

    let (_, parameterized_cid) =
        first_spool_entry(spool, NostrEventIndex::ParameterizedReplaceable)?;
    let parameterized_event = target.load_event_blob(&parameterized_cid).await?;
    let d_tag = parameterized_event
        .tags
        .iter()
        .find_map(|tag| match tag.as_slice() {
            [name, value, ..] if name == "d" => Some(value.as_str()),
            _ => None,
        })
        .unwrap_or_default();
    let expected_parameterized = vec![parameterized_event.clone()];
    let actual_parameterized = target
        .get_parameterized_replaceable(
            Some(root),
            &parameterized_event.pubkey,
            parameterized_event.kind,
            d_tag,
        )
        .await?
        .into_iter()
        .collect::<Vec<_>>();
    queries.push(checked_query(
        "parameterized-replaceable",
        serde_json::json!({
            "author": parameterized_event.pubkey,
            "kind": parameterized_event.kind,
            "d": d_tag
        }),
        &expected_parameterized,
        &actual_parameterized,
    )?);

    Ok((queries, representative_event_cid))
}

async fn block_evidence(
    role: impl Into<String>,
    cid: &Cid,
    store: &ReadOnlyPoolStore,
) -> Result<BulkProjectionBlockEvidence> {
    store
        .get(&cid.hash)
        .await
        .with_context(|| format!("read representative block {}", hex::encode(cid.hash)))?
        .with_context(|| format!("representative block {} is missing", hex::encode(cid.hash)))?;
    Ok(BulkProjectionBlockEvidence {
        role: role.into(),
        nhash: cid_to_nhash(cid)?,
        sha256: hex::encode(cid.hash),
    })
}

fn validate_profile_search_value(
    key: &str,
    value: &str,
    expected: &ExpectedProfileSearchEntry,
    bound_values: &mut BTreeMap<String, hashtree_cli::socialgraph::StoredProfileSearchEntry>,
) -> Result<hashtree_cli::socialgraph::StoredProfileSearchEntry> {
    let actual: StrictStoredProfileSearchEntry = serde_json::from_str(value)
        .with_context(|| format!("strictly decode profile-search entry `{key}`"))?;
    let actual: hashtree_cli::socialgraph::StoredProfileSearchEntry = actual.into();
    if actual.pubkey != expected.pubkey {
        anyhow::bail!(
            "profile-search entry `{key}` names pubkey `{}` instead of `{}`",
            actual.pubkey,
            expected.pubkey
        );
    }

    // Version 2 profile indexes sealed a graph-derived distance at projection
    // time. That historic value cannot be recomputed from today's graph.
    // Bind the first observed value per pubkey, reconstruct every other field
    // through the exact builder, and require every derived key to repeat the
    // same full value. Version 3 must make this distance deterministic or
    // include it in a separately attested input seal.
    let bound_distance = bound_values
        .get(&expected.pubkey)
        .map(|entry| entry.follow_distance)
        .unwrap_or(actual.follow_distance);
    let reconstructed = hashtree_cli::socialgraph::stored_profile_search_entry_for_event(
        &expected.event,
        &expected.mirrored_cid,
        bound_distance,
    )
    .with_context(|| format!("reconstruct exact profile-search entry `{key}`"))?;
    if actual != reconstructed {
        anyhow::bail!("profile-search entry `{key}` differs from its exact builder reconstruction");
    }
    if let Some(bound) = bound_values.get(&expected.pubkey) {
        if actual != *bound {
            anyhow::bail!(
                "profile-search entry `{key}` is not identical across all keys for pubkey `{}`",
                expected.pubkey
            );
        }
    } else {
        bound_values.insert(expected.pubkey.clone(), actual.clone());
    }
    Ok(actual)
}

fn require_profile_distance_attestation(
    allow_recovery_tranche: bool,
    expected: Option<&str>,
    actual: &str,
    has_independent_provenance: bool,
) -> Result<()> {
    if let Some(expected) = expected {
        require_expected_sha256("profile follow-distance seal", actual, expected)?;
    }
    if !allow_recovery_tranche && !has_independent_provenance {
        anyhow::bail!(
            "full-policy cutover audit requires independently pinned profile rank-decisions \
             provenance; an opaque v2 distance seal cannot self-attest"
        );
    }
    Ok(())
}

fn validate_bound_profile_distances(
    bound_values: &BTreeMap<String, hashtree_cli::socialgraph::StoredProfileSearchEntry>,
    trusted_decisions: &BTreeMap<String, Option<u32>>,
) -> Result<()> {
    for (pubkey, entry) in bound_values {
        match trusted_decisions.get(pubkey) {
            Some(Some(distance)) if entry.follow_distance == Some(*distance) => {}
            Some(Some(distance)) => anyhow::bail!(
                "profile-search retained pubkey `{pubkey}` with distance {:?}, \
                 but trusted rank decision requires {distance}",
                entry.follow_distance
            ),
            Some(None) => anyhow::bail!(
                "profile-search retained pubkey `{pubkey}` despite its trusted exclusion decision"
            ),
            None => anyhow::bail!(
                "profile-search retained pubkey `{pubkey}` without a trusted rank decision"
            ),
        }
    }
    Ok(())
}

async fn audit_profile_indexes(
    data_dir: &Path,
    spool: &BulkProjectionSpool,
    store: Arc<ReadOnlyPoolStore>,
    trusted_rank_decisions: Option<&BTreeMap<String, Option<u32>>>,
) -> Result<(
    BulkProjectionProfileAudit,
    Vec<Cid>,
    hashtree_cli::socialgraph::ProfileIndexRoots,
)> {
    let roots_before = hashtree_cli::socialgraph::read_profile_index_roots(data_dir)?;
    let by_pubkey_root = roots_before
        .by_pubkey
        .clone()
        .context("profile-by-pubkey root is missing")?;
    let by_pubkey_root_file_sha256 = roots_before
        .by_pubkey_file_sha256
        .clone()
        .context("profile-by-pubkey root file hash is missing")?;
    let search_root = roots_before
        .search
        .clone()
        .context("profile-search root is missing")?;
    let search_root_file_sha256 = roots_before
        .search_file_sha256
        .clone()
        .context("profile-search root file hash is missing")?;
    let btree = BTree::new(Arc::clone(&store), BTreeOptions { order: Some(64) });
    let by_pubkey = btree
        .validate_link_tree(Some(&by_pubkey_root))
        .await
        .context("exhaustively validate profile-by-pubkey root")?;
    let by_pubkey_entries = btree
        .links_entries(Some(&by_pubkey_root))
        .await
        .context("exhaustively traverse profile-by-pubkey root")?;
    if by_pubkey_entries.len() as u64 != by_pubkey.links {
        anyhow::bail!(
            "profile-by-pubkey structural count {} differs from traversal count {}",
            by_pubkey.links,
            by_pubkey_entries.len()
        );
    }
    let mut retained_profiles = spool.retained_profile_records()?;
    if retained_profiles.is_empty() {
        anyhow::bail!("bulk spool contains no retained kind-0 profiles");
    }
    let tree = HashTree::new(HashTreeConfig::new(Arc::clone(&store)));
    let mut by_pubkey_digest = Sha256::new();
    by_pubkey_digest.update(b"hashtree-profile-by-pubkey-audit-v1\0");
    let mut expected_search = BTreeMap::<String, ExpectedProfileSearchEntry>::new();
    let mut representative_profile_cid = None;
    for (pubkey, mirrored_cid) in &by_pubkey_entries {
        let record = retained_profiles
            .remove(pubkey)
            .with_context(|| format!("profile-by-pubkey contains unexpected pubkey `{pubkey}`"))?;
        let expected_event = record
            .event
            .to_nostr_sdk_event()
            .with_context(|| format!("decode retained profile event {}", record.event.id))?;
        let mirrored_bytes = tree
            .get(mirrored_cid, None)
            .await
            .with_context(|| format!("read mirrored profile event for {pubkey}"))?
            .with_context(|| format!("mirrored profile event for {pubkey} is missing"))?;
        let mirrored = Event::from_json(
            String::from_utf8(mirrored_bytes)
                .with_context(|| format!("decode mirrored profile event for {pubkey} as UTF-8"))?,
        )
        .with_context(|| format!("decode mirrored profile event JSON for {pubkey}"))?;
        if mirrored != expected_event {
            anyhow::bail!(
                "profile-by-pubkey mirrored event for `{pubkey}` differs from retained event {}",
                record.event.id
            );
        }
        for key in hashtree_cli::socialgraph::profile_search_keys_for_event(&mirrored) {
            let expected = ExpectedProfileSearchEntry {
                pubkey: pubkey.clone(),
                event: mirrored.clone(),
                mirrored_cid: mirrored_cid.clone(),
            };
            if expected_search.insert(key.clone(), expected).is_some() {
                anyhow::bail!("retained profiles produced duplicate search key `{key}`");
            }
        }
        by_pubkey_digest.update((pubkey.len() as u64).to_be_bytes());
        by_pubkey_digest.update(pubkey.as_bytes());
        let encoded_cid = encode_cid(mirrored_cid);
        by_pubkey_digest.update((encoded_cid.len() as u64).to_be_bytes());
        by_pubkey_digest.update(encoded_cid);
        representative_profile_cid.get_or_insert_with(|| mirrored_cid.clone());
    }
    if !retained_profiles.is_empty() {
        anyhow::bail!(
            "profile-by-pubkey omitted {} retained profiles; first missing pubkey `{}`",
            retained_profiles.len(),
            retained_profiles
                .first_key_value()
                .expect("non-empty retained profiles")
                .0
        );
    }

    let search_structural = btree
        .validate_value_tree(Some(&search_root))
        .await
        .context("exhaustively validate profile-search root structure")?;
    let search_entries = btree
        .entries(Some(&search_root))
        .await
        .context("exhaustively traverse profile-search root")?;
    if search_entries.len() as u64 != search_structural.entries {
        anyhow::bail!(
            "profile-search structural count {} differs from traversal count {}",
            search_structural.entries,
            search_entries.len()
        );
    }
    let mut search_digest = Sha256::new();
    search_digest.update(b"hashtree-profile-search-audit-v1\0");
    let mut bound_values = BTreeMap::new();
    let mut sample = None;
    for (key, value) in &search_entries {
        let expected = expected_search
            .remove(key)
            .with_context(|| format!("profile-search contains unexpected key `{key}`"))?;
        let entry = validate_profile_search_value(key, value, &expected, &mut bound_values)?;
        sample.get_or_insert_with(|| {
            (
                expected.pubkey.clone(),
                expected.event.id.to_hex(),
                entry.name.clone(),
            )
        });
        search_digest.update((key.len() as u64).to_be_bytes());
        search_digest.update(key.as_bytes());
        search_digest.update((value.len() as u64).to_be_bytes());
        search_digest.update(value.as_bytes());
    }
    if !expected_search.is_empty() {
        anyhow::bail!(
            "profile-search omitted {} expected keys; first missing key `{}`",
            expected_search.len(),
            expected_search
                .first_key_value()
                .expect("non-empty expected search entries")
                .0
        );
    }
    if bound_values.len() != by_pubkey_entries.len() {
        anyhow::bail!(
            "profile-search bound {} historic distances for {} retained profiles",
            bound_values.len(),
            by_pubkey_entries.len()
        );
    }
    if let Some(trusted_rank_decisions) = trusted_rank_decisions {
        validate_bound_profile_distances(&bound_values, trusted_rank_decisions)?;
    }
    let bound_distances = bound_values
        .iter()
        .map(|(pubkey, entry)| (pubkey.clone(), entry.follow_distance))
        .collect();
    let follow_distance_seal_sha256 =
        hashtree_cli::socialgraph::profile_follow_distance_seal_v2(&bound_distances);
    let (sample_pubkey, sample_event_id, sample_name) =
        sample.context("profile-search root contains no entries")?;
    let roots_after = hashtree_cli::socialgraph::read_profile_index_roots(data_dir)?;
    if roots_after != roots_before {
        anyhow::bail!("profile index root files changed during read-only audit");
    }
    let representative_profile_cid =
        representative_profile_cid.context("profile-by-pubkey root contains no entries")?;
    Ok((
        BulkProjectionProfileAudit {
            by_pubkey_root: cid_to_nhash(&by_pubkey_root)?,
            by_pubkey_root_file_sha256,
            by_pubkey_nodes: by_pubkey.nodes,
            by_pubkey_links: by_pubkey.links,
            by_pubkey_entries_sha256: hex::encode(by_pubkey_digest.finalize()),
            search_root: cid_to_nhash(&search_root)?,
            search_root_file_sha256,
            search_nodes: search_structural.nodes,
            search_entries: search_entries.len() as u64,
            search_entries_sha256: hex::encode(search_digest.finalize()),
            sample_pubkey,
            sample_event_id,
            sample_name,
            follow_distance_binding:
                "opaque-historic-v2-first-observed-per-pubkey; v3-requires-deterministic-or-attested-distance"
                    .to_string(),
            follow_distance_seal_sha256,
        },
        vec![by_pubkey_root, search_root, representative_profile_cid],
        roots_before,
    ))
}

#[derive(Debug)]
struct PreparedAuditOutput {
    path: PathBuf,
    parent: PathBuf,
}

fn prepare_audit_output_path(
    data_dir: &Path,
    staging_data_dir: &Path,
    out: &Path,
) -> Result<PreparedAuditOutput> {
    if !out.is_absolute() {
        anyhow::bail!("bulk projection audit --out must be an absolute path");
    }
    let file_name = out
        .file_name()
        .filter(|name| !name.is_empty())
        .context("bulk projection audit --out must name a file")?;
    let parent = out
        .parent()
        .context("bulk projection audit --out has no parent directory")?
        .canonicalize()
        .with_context(|| {
            format!(
                "canonicalize pre-existing audit output parent {}",
                out.parent().expect("parent checked").display()
            )
        })?;
    let data_tree = data_dir
        .canonicalize()
        .with_context(|| format!("canonicalize audited data tree {}", data_dir.display()))?;
    let staging_tree = staging_data_dir.canonicalize().with_context(|| {
        format!(
            "canonicalize audited staging tree {}",
            staging_data_dir.display()
        )
    })?;
    if parent.starts_with(&data_tree) || parent.starts_with(&staging_tree) {
        anyhow::bail!(
            "bulk projection audit evidence parent {} is inside an audited live data tree",
            parent.display()
        );
    }
    let path = parent.join(file_name);
    match std::fs::symlink_metadata(&path) {
        Ok(_) => anyhow::bail!(
            "bulk projection audit evidence target already exists or is a symlink: {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect audit evidence target {}", path.display()));
        }
    }
    Ok(PreparedAuditOutput { path, parent })
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .with_context(|| format!("open directory {} for evidence fsync", path.display()))?
        .sync_all()
        .with_context(|| format!("fsync evidence directory {}", path.display()))
}

fn install_audit_evidence_noreplace(bytes: &[u8], prepared: &PreparedAuditOutput) -> Result<()> {
    let mut temp_path = None;
    let mut temp_file = None;
    for _ in 0..16 {
        let candidate = prepared.parent.join(format!(
            ".bulk-projection-audit.{}.{}.tmp",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => {
                temp_path = Some(candidate);
                temp_file = Some(file);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "create same-directory audit evidence temp file {}",
                        candidate.display()
                    )
                });
            }
        }
    }
    let temp_path = temp_path.context("could not allocate unique audit evidence temp file")?;
    let mut temp_file = temp_file.expect("temp path and file are assigned together");
    let write_result = (|| -> Result<()> {
        temp_file
            .write_all(bytes)
            .context("write complete audit evidence temp file")?;
        temp_file
            .sync_all()
            .context("fsync complete audit evidence temp file")?;
        drop(temp_file);

        std::fs::hard_link(&temp_path, &prepared.path).with_context(|| {
            format!(
                "atomically install no-replace audit evidence {}",
                prepared.path.display()
            )
        })?;
        sync_directory(&prepared.parent)?;
        std::fs::remove_file(&temp_path).with_context(|| {
            format!(
                "remove linked audit evidence temp file {}",
                temp_path.display()
            )
        })?;
        sync_directory(&prepared.parent)?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
        let _ = sync_directory(&prepared.parent);
    }
    write_result
}

fn write_audit_output_noreplace(
    output: &BulkProjectionAuditOutput,
    prepared: &PreparedAuditOutput,
) -> Result<()> {
    let mut bytes =
        serde_json::to_vec_pretty(output).context("serialize bulk projection audit evidence")?;
    bytes.push(b'\n');
    install_audit_evidence_noreplace(&bytes, prepared)
}

pub(crate) async fn audit_bulk_projection(
    data_dir: &Path,
    options: BulkProjectionAuditOptions,
) -> Result<BulkProjectionAuditOutput> {
    if options.btree_order < 2 || options.page_size == 0 || options.query_limit == 0 {
        anyhow::bail!(
            "audit B-tree order must be at least 2 and page/query limits must be non-zero"
        );
    }
    if options.expected_full_author_count == 0 {
        anyhow::bail!("expected full author count must be non-zero");
    }
    let prepared_output =
        prepare_audit_output_path(data_dir, &options.staging_data_dir, &options.out)?;
    let (state_path, spool_path) = bulk_paths(data_dir);
    let stage_state_path = options
        .staging_data_dir
        .join(STAGE_DIR)
        .join(STAGE_STATE_FILE);
    let state_bytes = std::fs::read(&state_path)
        .with_context(|| format!("read bulk projection state {}", state_path.display()))?;
    let stage_bytes = std::fs::read(&stage_state_path)
        .with_context(|| format!("read staging state {}", stage_state_path.display()))?;
    let state_sha256 = bytes_sha256(&state_bytes);
    let stage_state_sha256 = bytes_sha256(&stage_bytes);
    require_expected_sha256(
        "bulk projection state",
        &state_sha256,
        &options.expected_state_sha256,
    )?;
    require_expected_sha256(
        "staging state",
        &stage_state_sha256,
        &options.expected_stage_state_sha256,
    )?;
    let state: BulkProjectionState =
        serde_json::from_slice(&state_bytes).context("parse bulk projection state")?;
    let stage: StagedNostrCrawlState =
        serde_json::from_slice(&stage_bytes).context("parse staging state")?;
    if state.version != BULK_PROJECTION_VERSION {
        anyhow::bail!(
            "unsupported bulk projection state version {}",
            state.version
        );
    }
    if stage.version != STAGE_FORMAT_VERSION {
        anyhow::bail!("unsupported frozen staging state version {}", stage.version);
    }
    if state.author_allowlist_source != stage.author_allowlist_source {
        anyhow::bail!(
            "terminal bulk projection allowlist source differs from frozen staging source"
        );
    }
    if state.policy.author_count != options.expected_full_author_count {
        anyhow::bail!(
            "bulk projection policy author count mismatch: expected trusted full count {}, found {}",
            options.expected_full_author_count,
            state.policy.author_count
        );
    }
    let policy_bytes =
        serde_json::to_vec(&state.policy).context("serialize trusted bulk projection policy")?;
    let policy_sha256 = bytes_sha256(&policy_bytes);
    require_expected_sha256(
        "bulk projection policy",
        &policy_sha256,
        &options.expected_policy_sha256,
    )?;
    if state.next_author > state.policy.author_count {
        anyhow::bail!(
            "terminal bulk projection author watermark {} exceeds policy author count {}",
            state.next_author,
            state.policy.author_count
        );
    }
    if state.policy.max_authors != options.expected_full_author_count {
        anyhow::bail!(
            "bulk projection policy max_authors {} differs from trusted full count {}",
            state.policy.max_authors,
            options.expected_full_author_count
        );
    }
    if options.allow_recovery_tranche {
        if state.next_author == 0 || state.next_author >= options.expected_full_author_count {
            anyhow::bail!(
                "recovery-tranche audit requires a nonzero partial watermark below trusted full \
                 count {}; found next_author={}",
                options.expected_full_author_count,
                state.next_author
            );
        }
    } else if state.next_author != options.expected_full_author_count {
        anyhow::bail!(
            "full-policy audit requires the terminal and frozen-stage author watermark to equal \
             trusted full count {}; found next_author={}; use --allow-recovery-tranche only for \
             internal non-cutover evidence",
            options.expected_full_author_count,
            state.next_author
        );
    }
    validate_terminal_stage_state(&state, &stage)?;
    let trusted_profile_rank_decisions = load_trusted_profile_rank_decisions(&options)?;
    require_profile_rank_policy_binding(
        trusted_profile_rank_decisions.as_ref(),
        &state.policy.author_allowlist_sha256,
        state.policy.author_count,
    )?;
    if state.built_roots.len() != NostrEventIndex::ALL.len()
        || NostrEventIndex::ALL
            .iter()
            .any(|index| !state.built_roots.contains_key(&index.stable_id()))
    {
        anyhow::bail!("bulk projection state must contain exactly all nine index roots");
    }
    let candidate_root = state
        .complete_root
        .as_deref()
        .context("bulk projection has no complete candidate root")
        .and_then(parse_root_text)?;

    let spool = BulkProjectionSpool::open_read_only(&spool_path)?;
    let pool_path = data_dir.join(SHARED_BLOB_POOL_DIR_NAME);
    let store = Arc::new(
        ReadOnlyPoolStore::open(&pool_path)
            .with_context(|| format!("open exact native PoolStore {}", pool_path.display()))?,
    );
    let catalog_before = store
        .validate_committed_catalog()
        .context("validate fully committed PoolStore catalog")?;
    let target = NostrEventStore::new(Arc::clone(&store));
    let canonical_manifest = target
        .get_canonical_manifest(&candidate_root)
        .await
        .context("validate exact canonical bulk projection manifest")?;
    let manifest = canonical_manifest.manifest;
    let manifest_metadata = canonical_manifest.metadata_cid;
    let btree = BTree::new(
        Arc::clone(&store),
        BTreeOptions {
            order: Some(options.btree_order),
        },
    );

    let mut indexes = Vec::with_capacity(NostrEventIndex::ALL.len());
    let mut expected_entries = [EntrySetProof::default(); 9];
    let mut representative_cids = vec![candidate_root.clone(), manifest_metadata];
    for index in NostrEventIndex::ALL {
        let encoded = state
            .built_roots
            .get(&index.stable_id())
            .expect("all nine state roots checked");
        if encoded.is_empty() {
            anyhow::bail!(
                "bulk projection state omitted required canonical `{}` root",
                index.name()
            );
        }
        let state_root = Some(
            parse_root_text(encoded)
                .with_context(|| format!("parse state {} root", index.name()))?,
        );
        let manifest_root = manifest_root_for_index(&manifest, index);
        if manifest_root != state_root.as_ref() {
            anyhow::bail!(
                "manifest {} root differs from exact projection state",
                index.name()
            );
        }
        let (audit, retained_set) = audit_index_root(
            &spool,
            &target,
            &btree,
            index,
            manifest_root,
            options.page_size,
            &mut expected_entries,
        )
        .await?;
        let expected = expected_entries[index.stable_id() as usize];
        if retained_set != expected {
            anyhow::bail!(
                "{} root/spool entries do not exactly match the retained by-id event set: \
                 actual_count={} expected_count={} actual_digest={} expected_digest={}",
                index.name(),
                retained_set.count,
                expected.count,
                retained_set.evidence_sha256(),
                expected.evidence_sha256()
            );
        }
        indexes.push(audit);
        if let Some(root) = manifest_root {
            representative_cids.push(root.clone());
        }
    }
    let event_records = spool.event_record_count()?;
    if event_records != indexes[0].durable_values_validated {
        anyhow::bail!(
            "bulk spool contains {event_records} event records but by-id validated {}",
            indexes[0].durable_values_validated
        );
    }

    let (profile, profile_cids, profile_roots_before) = audit_profile_indexes(
        data_dir,
        &spool,
        Arc::clone(&store),
        trusted_profile_rank_decisions
            .as_ref()
            .map(|trusted| &trusted.decisions),
    )
    .await?;
    require_profile_distance_attestation(
        options.allow_recovery_tranche,
        options.expected_profile_distance_seal_sha256.as_deref(),
        &profile.follow_distance_seal_sha256,
        trusted_profile_rank_decisions.is_some(),
    )?;
    representative_cids.extend(profile_cids);
    let (queries, representative_event_cid) =
        audit_real_queries(&spool, &target, &candidate_root, options.query_limit).await?;
    representative_cids.push(representative_event_cid);

    let mut representative_blocks = Vec::new();
    let mut seen_blocks = HashSet::new();
    for (position, cid) in representative_cids.iter().enumerate() {
        if seen_blocks.insert(cid.hash) {
            let role = match position {
                0 => "manifest".to_string(),
                1 => "manifest-metadata".to_string(),
                2..=10 => format!("index-root-{}", position - 2),
                _ => format!("representative-{position}"),
            };
            representative_blocks.push(block_evidence(role, cid, &store).await?);
        }
    }

    let final_state_bytes = std::fs::read(&state_path)
        .with_context(|| format!("re-read bulk projection state {}", state_path.display()))?;
    let final_stage_bytes = std::fs::read(&stage_state_path)
        .with_context(|| format!("re-read staging state {}", stage_state_path.display()))?;
    if bytes_sha256(&final_state_bytes) != state_sha256
        || bytes_sha256(&final_stage_bytes) != stage_state_sha256
    {
        anyhow::bail!("projection or staging state changed during read-only audit");
    }
    let catalog_after = store
        .validate_committed_catalog()
        .context("revalidate PoolStore catalog after audit")?;
    if catalog_after != catalog_before {
        anyhow::bail!("PoolStore catalog changed during read-only audit");
    }
    let profile_roots_after = hashtree_cli::socialgraph::read_profile_index_roots(data_dir)?;
    if profile_roots_after != profile_roots_before {
        anyhow::bail!("profile index root files changed during read-only audit");
    }
    recheck_trusted_profile_rank_decisions(trusted_profile_rank_decisions.as_ref())?;

    let output = BulkProjectionAuditOutput {
        version: 2,
        candidate_root: cid_to_nhash(&candidate_root)?,
        state_sha256,
        stage_state_sha256,
        trusted_policy_sha256: policy_sha256,
        trusted_profile_distance_seal_sha256: options.expected_profile_distance_seal_sha256,
        profile_distance_provenance: trusted_profile_rank_decisions
            .as_ref()
            .map(|trusted| trusted.evidence.clone()),
        trusted_full_author_count: options.expected_full_author_count,
        crawl_policy_max_follow_distance: state.policy.max_follow_distance,
        audit_mode: if options.allow_recovery_tranche {
            "recovery-tranche-internal-non-cutover"
        } else {
            "full-policy-cutover"
        }
        .to_string(),
        cutover_eligible: !options.allow_recovery_tranche
            && trusted_profile_rank_decisions.is_some(),
        pool_catalog_sha256: catalog_before.sha256,
        pool_manifest_sha256: catalog_before.manifest_sha256,
        pool_stored_locations: catalog_before.stored_locations,
        authors_processed: state.next_author,
        authors_total: state.policy.author_count,
        recovery_tranche_only: options.allow_recovery_tranche,
        indexes,
        profile,
        queries,
        representative_blocks,
    };
    write_audit_output_noreplace(&output, &prepared_output)?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashtree_config::StorageBackend;
    use hashtree_nostr::{stored_event_from_nostr_sdk_event, NostrEventStoreOptions};
    use nostr::{EventBuilder, Keys, Kind, Tag, Timestamp};

    use super::super::super::{
        persist_stage_state, run_nostr_bulk_projection_audit, CrawlStateLock,
        IndexedNostrCrawlPolicy, StagedNostrCrawlState, STAGE_FORMAT_VERSION,
    };

    fn policy(author_count: usize) -> IndexedNostrCrawlPolicy {
        IndexedNostrCrawlPolicy {
            base_root: None,
            author_allowlist_sha256: "ab".repeat(32),
            author_count,
            relays: vec!["wss://relay.example".to_string()],
            require_all_relays: false,
            max_events_seen: None,
            max_authors: author_count,
            max_follow_distance: Some(0),
            max_live_bytes: 1_000_000,
            author_batch_size: 1,
            checkpoint_authors: 1,
            per_author_event_limit: 256,
            per_author_kind_event_limit: None,
            per_author_live_bytes: Some(64 * 1024 * 1024),
            fetch_timeout_millis: 30_000,
            relay_event_max_bytes: Some(1024 * 1024),
            global_relay_scan: false,
            full_author_history: true,
            negentropy_only: false,
            relay_page_size: 1_000,
            max_relay_pages: 67,
            kinds: Some(vec![0, 1, 30_000]),
        }
    }

    fn expected_profile_search_entry() -> (
        ExpectedProfileSearchEntry,
        hashtree_cli::socialgraph::StoredProfileSearchEntry,
    ) {
        let keys = Keys::generate();
        let event = EventBuilder::new(
            Kind::Metadata,
            r#"{"name":"Alice","display_name":"Alice Example","nip05":"alice@example.com"}"#,
        )
        .custom_created_at(Timestamp::from_secs(10))
        .sign_with_keys(&keys)
        .unwrap();
        let mirrored_cid = Cid {
            hash: [7; 32],
            key: None,
        };
        let stored = hashtree_cli::socialgraph::stored_profile_search_entry_for_event(
            &event,
            &mirrored_cid,
            Some(1),
        )
        .unwrap();
        (
            ExpectedProfileSearchEntry {
                pubkey: event.pubkey.to_hex(),
                event,
                mirrored_cid,
            },
            stored,
        )
    }

    fn rank_decision_artifacts(
        decisions: &BTreeMap<String, Option<u32>>,
    ) -> (Vec<u8>, Vec<u8>, String, String) {
        let mut digest = Sha256::new();
        digest.update(PROFILE_RANK_DECISION_FORMAT.as_bytes());
        digest.update(b"\n");
        for (pubkey, rank) in decisions {
            let row = match rank {
                Some(rank) => serde_json::json!([pubkey, "eligible", rank]),
                None => serde_json::json!([pubkey, "excluded", null]),
            };
            digest.update(serde_json::to_string(&row).unwrap().as_bytes());
            digest.update(b"\n");
        }
        let decisions_sha256 = hex::encode(digest.finalize());
        let mut lines = vec![serde_json::to_string(&ProfileRankDecisionHeader {
            format: PROFILE_RANK_DECISION_FORMAT.to_string(),
            eligible_ranks_sha256: decisions_sha256.clone(),
            record_count: decisions.len(),
        })
        .unwrap()];
        lines.extend(decisions.iter().map(|(pubkey, rank)| {
            serde_json::to_string(&ProfileRankDecisionRecord {
                pubkey: pubkey.clone(),
                decision: if rank.is_some() {
                    "eligible"
                } else {
                    "excluded"
                }
                .to_string(),
                rank_hint: *rank,
            })
            .unwrap()
        }));
        let decisions_bytes = format!("{}\n", lines.join("\n")).into_bytes();
        let decisions_file_sha256 = bytes_sha256(&decisions_bytes);
        let eligible_count = decisions.values().filter(|rank| rank.is_some()).count();
        let eligible_authors_sha256 = eligible_profile_rank_authors_sha256(decisions);
        let report = serde_json::json!({
            "format": PROFILE_RANK_DECISION_REPORT_FORMAT,
            "censusFormat": PROFILE_RANK_DECISION_CENSUS_FORMAT,
            "socialGraphRoot": "a".repeat(64),
            "socialGraphSha256": "b".repeat(64),
            "eligibleAuthorsSha256": eligible_authors_sha256,
            "overmuteThreshold": PROFILE_OVERMUTE_THRESHOLD,
            "maxDistance": 4,
            "rankPolicy": PROFILE_RANK_POLICY,
            "exclusionPolicy": PROFILE_EXCLUSION_POLICY,
            "recordCount": decisions.len(),
            "eligibleCount": eligible_count,
            "excludedCount": decisions.len() - eligible_count,
            "reachableCount": decisions.len(),
            "reachableOvermutedCount": 0,
            "distanceExcludedCount": 0,
            "unreachableCount": 0,
            "allGraphOvermutedCount": 0,
            "rankDecisionsSha256": decisions_sha256,
            "rankDecisionsFileSha256": decisions_file_sha256,
        });
        let report_bytes =
            format!("{}\n", serde_json::to_string_pretty(&report).unwrap()).into_bytes();
        let report_sha256 = bytes_sha256(&report_bytes);
        (
            decisions_bytes,
            report_bytes,
            decisions_file_sha256,
            report_sha256,
        )
    }

    #[test]
    fn loads_pinned_rank_decision_provenance_and_rechecks_exact_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let pubkey = "d".repeat(64);
        let decisions = BTreeMap::from([(pubkey.clone(), Some(1))]);
        let (decisions_bytes, report_bytes, decisions_sha256, report_sha256) =
            rank_decision_artifacts(&decisions);
        let decisions_path = temp.path().join("rank-decisions.jsonl");
        let report_path = temp.path().join("rank-decisions-report.json");
        std::fs::write(&decisions_path, decisions_bytes).unwrap();
        std::fs::write(&report_path, report_bytes).unwrap();
        let options = BulkProjectionAuditOptions {
            staging_data_dir: temp.path().join("unused-staging"),
            expected_state_sha256: "1".repeat(64),
            expected_stage_state_sha256: "2".repeat(64),
            expected_policy_sha256: "3".repeat(64),
            expected_profile_distance_seal_sha256: None,
            profile_rank_decisions_file: Some(decisions_path.clone()),
            expected_profile_rank_decisions_file_sha256: Some(decisions_sha256),
            profile_rank_decisions_report: Some(report_path.clone()),
            expected_profile_rank_decisions_report_sha256: Some(report_sha256),
            expected_full_author_count: 1,
            allow_recovery_tranche: false,
            btree_order: 2,
            page_size: 1,
            query_limit: 1,
            out: temp.path().join("unused-evidence.json"),
        };
        let trusted = load_trusted_profile_rank_decisions(&options)
            .unwrap()
            .expect("full provenance");
        assert_eq!(trusted.decisions, decisions);
        assert_eq!(trusted.evidence.eligible_count, 1);
        assert_eq!(trusted.evidence.census_max_distance, Some(4));
        // The real wrapper uses policy depth 0 to disable live graph
        // expansion while the independently frozen census covers distance 4.
        let crawl_policy_max_follow_distance = Some(0);
        assert_ne!(
            trusted.evidence.census_max_distance,
            crawl_policy_max_follow_distance
        );
        let eligible_authors_sha256 = eligible_profile_rank_authors_sha256(&decisions);
        require_profile_rank_policy_binding(Some(&trusted), &eligible_authors_sha256, 1).unwrap();
        assert!(
            require_profile_rank_policy_binding(Some(&trusted), &"f".repeat(64), 1)
                .unwrap_err()
                .to_string()
                .contains("does not match crawl-policy allowlist digest")
        );
        assert!(
            require_profile_rank_policy_binding(Some(&trusted), &eligible_authors_sha256, 2)
                .unwrap_err()
                .to_string()
                .contains("does not match crawl-policy author count")
        );
        recheck_trusted_profile_rank_decisions(Some(&trusted)).unwrap();

        std::fs::write(&report_path, b"changed").unwrap();
        assert!(recheck_trusted_profile_rank_decisions(Some(&trusted))
            .unwrap_err()
            .to_string()
            .contains("changed during read-only audit"));
    }

    #[test]
    fn pinned_rank_decisions_reject_same_count_swapped_eligible_pubkey() {
        let temp = tempfile::tempdir().unwrap();
        let expected_pubkey = "d".repeat(64);
        let expected = BTreeMap::from([(expected_pubkey, Some(1))]);
        let expected_eligible_sha256 = eligible_profile_rank_authors_sha256(&expected);

        let swapped_pubkey = "e".repeat(64);
        let swapped = BTreeMap::from([(swapped_pubkey, Some(1))]);
        let (decisions_bytes, report_bytes, decisions_sha256, _) =
            rank_decision_artifacts(&swapped);
        let mut report: serde_json::Value = serde_json::from_slice(&report_bytes).unwrap();
        report["eligibleAuthorsSha256"] = serde_json::Value::String(expected_eligible_sha256);
        let report_bytes =
            format!("{}\n", serde_json::to_string_pretty(&report).unwrap()).into_bytes();
        let report_sha256 = bytes_sha256(&report_bytes);
        let decisions_path = temp.path().join("rank-decisions.jsonl");
        let report_path = temp.path().join("rank-decisions-report.json");
        std::fs::write(&decisions_path, decisions_bytes).unwrap();
        std::fs::write(&report_path, report_bytes).unwrap();

        let error = load_pinned_profile_rank_decisions(
            &decisions_path,
            &decisions_sha256,
            &report_path,
            &report_sha256,
        )
        .expect_err("same-count swapped eligible keys must not inherit the policy digest");
        assert!(error
            .to_string()
            .contains("does not match actual eligible decision keys"));
    }

    #[test]
    fn profile_search_audit_rejects_corrupt_known_fields_and_unknown_fields() {
        let (expected, stored) = expected_profile_search_entry();
        for field in ["name", "aliases", "nip05"] {
            let mut value = serde_json::to_value(&stored).unwrap();
            let object = value.as_object_mut().unwrap();
            match field {
                "name" => object.insert(field.to_string(), serde_json::json!("Mallory")),
                "aliases" => object.insert(field.to_string(), serde_json::json!(["Wrong Alias"])),
                "nip05" => object.insert(field.to_string(), serde_json::json!("wrong@example.com")),
                _ => unreachable!(),
            };
            let mut bound = BTreeMap::new();
            assert!(
                validate_profile_search_value(
                    "p:test:pubkey",
                    &serde_json::to_string(&value).unwrap(),
                    &expected,
                    &mut bound,
                )
                .unwrap_err()
                .to_string()
                .contains("builder reconstruction"),
                "corrupt {field} must fail exact reconstruction"
            );
        }

        let mut value = serde_json::to_value(&stored).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("unknown".to_string(), serde_json::json!(true));
        let error = validate_profile_search_value(
            "p:test:pubkey",
            &serde_json::to_string(&value).unwrap(),
            &expected,
            &mut BTreeMap::new(),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("strictly decode"),
            "unexpected unknown-field error: {error:#}"
        );
    }

    #[test]
    fn profile_search_audit_binds_one_historic_distance_across_all_keys() {
        let (expected, first) = expected_profile_search_entry();
        let mut bound = BTreeMap::new();
        validate_profile_search_value(
            "p:alice:pubkey",
            &serde_json::to_string(&first).unwrap(),
            &expected,
            &mut bound,
        )
        .unwrap();

        let second = hashtree_cli::socialgraph::stored_profile_search_entry_for_event(
            &expected.event,
            &expected.mirrored_cid,
            Some(2),
        )
        .unwrap();
        let error = validate_profile_search_value(
            "p:example:pubkey",
            &serde_json::to_string(&second).unwrap(),
            &expected,
            &mut bound,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("builder reconstruction"),
            "unexpected distance mismatch error: {error:#}"
        );

        let mut consistently_wrong = BTreeMap::new();
        for key in ["p:alice:pubkey", "p:example:pubkey"] {
            validate_profile_search_value(
                key,
                &serde_json::to_string(&second).unwrap(),
                &expected,
                &mut consistently_wrong,
            )
            .unwrap();
        }
        let wrong_distances = consistently_wrong
            .iter()
            .map(|(pubkey, entry)| (pubkey.clone(), entry.follow_distance))
            .collect();
        let wrong_seal =
            hashtree_cli::socialgraph::profile_follow_distance_seal_v2(&wrong_distances);
        let correct_values = BTreeMap::from([(expected.pubkey.clone(), first)]);
        let correct_distances = correct_values
            .iter()
            .map(|(pubkey, entry)| (pubkey.clone(), entry.follow_distance))
            .collect();
        let correct_seal =
            hashtree_cli::socialgraph::profile_follow_distance_seal_v2(&correct_distances);
        assert_ne!(wrong_seal, correct_seal);
        let trusted_decisions = BTreeMap::from([(expected.pubkey.clone(), Some(1))]);
        let provenance_error =
            validate_bound_profile_distances(&consistently_wrong, &trusted_decisions)
                .expect_err("trusted rank decision must reject consistently wrong distance");
        assert!(
            provenance_error
                .to_string()
                .contains("trusted rank decision requires 1"),
            "unexpected provenance mismatch: {provenance_error:#}"
        );
        let error =
            require_profile_distance_attestation(false, Some(&correct_seal), &wrong_seal, true)
                .expect_err("full cutover must reject consistently wrong historic distance");
        assert!(
            error.to_string().contains("SHA-256 mismatch"),
            "unexpected full-cutover distance seal error: {error:#}"
        );
    }

    #[test]
    fn audit_output_refuses_live_trees_existing_targets_and_race_overwrites() {
        let temp = tempfile::tempdir().unwrap();
        let data_dir = temp.path().join("data");
        let staging_dir = temp.path().join("staging");
        let evidence_dir = temp.path().join("evidence");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::create_dir_all(&staging_dir).unwrap();
        std::fs::create_dir_all(&evidence_dir).unwrap();

        let protected = data_dir.join("audit.json");
        assert!(
            prepare_audit_output_path(&data_dir, &staging_dir, &protected)
                .unwrap_err()
                .to_string()
                .contains("inside an audited live data tree")
        );

        let existing = evidence_dir.join("existing.json");
        std::fs::write(&existing, b"trusted-existing").unwrap();
        assert!(
            prepare_audit_output_path(&data_dir, &staging_dir, &existing)
                .unwrap_err()
                .to_string()
                .contains("already exists")
        );

        #[cfg(unix)]
        {
            let symlink = evidence_dir.join("symlink.json");
            std::os::unix::fs::symlink(&existing, &symlink).unwrap();
            assert!(prepare_audit_output_path(&data_dir, &staging_dir, &symlink)
                .unwrap_err()
                .to_string()
                .contains("already exists"));
        }

        let raced = evidence_dir.join("raced.json");
        let prepared = prepare_audit_output_path(&data_dir, &staging_dir, &raced).unwrap();
        std::fs::write(&raced, b"racing-writer").unwrap();
        assert!(install_audit_evidence_noreplace(b"new-evidence", &prepared).is_err());
        assert_eq!(std::fs::read(&raced).unwrap(), b"racing-writer");
        assert!(
            std::fs::read_dir(&evidence_dir)
                .unwrap()
                .filter_map(|entry| entry.ok())
                .all(|entry| !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".bulk-projection-audit.")),
            "failed no-replace write left a partial temp file"
        );
    }

    #[test]
    fn audit_output_installs_complete_bytes_once_without_temp_files() {
        let temp = tempfile::tempdir().unwrap();
        let data_dir = temp.path().join("data");
        let staging_dir = temp.path().join("staging");
        let evidence_dir = temp.path().join("evidence");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::create_dir_all(&staging_dir).unwrap();
        std::fs::create_dir_all(&evidence_dir).unwrap();
        let target = evidence_dir.join("audit.json");
        let prepared = prepare_audit_output_path(&data_dir, &staging_dir, &target).unwrap();

        install_audit_evidence_noreplace(b"complete-evidence", &prepared).unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"complete-evidence");
        assert_eq!(std::fs::read_dir(&evidence_dir).unwrap().count(), 1);
        assert!(install_audit_evidence_noreplace(b"replacement", &prepared).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"complete-evidence");
    }

    #[tokio::test]
    async fn audits_real_pool_spool_manifest_profiles_and_queries_read_only() {
        let temp = tempfile::tempdir().unwrap();
        let data_dir = temp.path().join("projection");
        let staging_data_dir = temp.path().join("staging");
        let evidence_path = temp.path().join("audit-evidence.json");
        let store = hashtree_cli::HashtreeStore::with_options_and_backend(
            &data_dir,
            None,
            0,
            false,
            &StorageBackend::Lmdb,
        )
        .unwrap();
        let graph = hashtree_cli::socialgraph::open_social_graph_store_with_storage(
            &data_dir,
            store.store_arc(),
            Some(128 * 1024 * 1024),
        )
        .unwrap();

        let keys = Keys::generate();
        let profile = EventBuilder::new(Kind::Metadata, r#"{"name":"Audit Alice"}"#)
            .custom_created_at(Timestamp::from_secs(10))
            .sign_with_keys(&keys)
            .unwrap();
        let note = EventBuilder::new(Kind::TextNote, "real audit note")
            .tags([Tag::parse(["t", "hashtree"]).unwrap()])
            .custom_created_at(Timestamp::from_secs(20))
            .sign_with_keys(&keys)
            .unwrap();
        let parameterized = EventBuilder::new(Kind::Custom(30_000), "real parameterized event")
            .tags([Tag::identifier("audit-article")])
            .custom_created_at(Timestamp::from_secs(30))
            .sign_with_keys(&keys)
            .unwrap();
        let stored = [&profile, &note, &parameterized]
            .into_iter()
            .map(stored_event_from_nostr_sdk_event)
            .collect::<Vec<_>>();
        let event_store = NostrEventStore::with_options(
            store.store_arc(),
            NostrEventStoreOptions {
                btree_order: Some(8),
                ..NostrEventStoreOptions::default()
            },
        );
        let event_cids = event_store.store_event_blobs(stored.clone()).await.unwrap();
        store.force_sync().unwrap();

        let (state_path, spool_path) = bulk_paths(&data_dir);
        let spool = BulkProjectionSpool::open(&spool_path).unwrap();
        spool
            .apply(stored.clone().into_iter().zip(event_cids).collect())
            .unwrap();
        graph
            .sync_profile_index_for_events(std::slice::from_ref(&profile))
            .unwrap();
        graph.force_sync().unwrap();

        let mut roots = BTreeMap::new();
        for index in NostrEventIndex::ALL {
            roots.insert(
                index,
                spool
                    .build_index_root(index, store.store_arc(), 8)
                    .await
                    .unwrap(),
            );
        }
        let candidate_root = event_store
            .write_bulk_index_manifest(&roots)
            .await
            .unwrap()
            .unwrap();
        store.force_sync().unwrap();

        // Match the real recovery shape: the pinned policy remains the full
        // 101,267-author policy while the frozen terminal stage stops at
        // author 17,177.
        let policy = policy(101_267);
        let state = BulkProjectionState {
            version: BULK_PROJECTION_VERSION,
            author_allowlist_source: Some("file:///audit-authors".to_string()),
            policy: policy.clone(),
            next_author: 17_177,
            segment_event_offset: 0,
            events_seen: 3,
            events_selected: 3,
            live_bytes_selected: 123,
            built_roots: roots
                .iter()
                .map(|(index, root)| {
                    (
                        index.stable_id(),
                        root.as_ref()
                            .map(cid_to_nhash)
                            .transpose()
                            .unwrap()
                            .unwrap_or_default(),
                    )
                })
                .collect(),
            complete_root: Some(cid_to_nhash(&candidate_root).unwrap()),
        };
        super::super::persist_bulk_state(&state_path, &state).unwrap();
        let stage = StagedNostrCrawlState {
            version: STAGE_FORMAT_VERSION,
            author_allowlist_source: Some("file:///audit-authors".to_string()),
            policy,
            next_author: state.next_author,
            events_seen: state.events_seen,
            events_selected: state.events_selected,
            live_bytes_selected: state.live_bytes_selected,
        };
        persist_stage_state(&staging_data_dir, &stage).unwrap();
        drop(CrawlStateLock::acquire(&data_dir).unwrap());
        drop(CrawlStateLock::acquire_stage(&staging_data_dir).unwrap());

        drop(event_store);
        drop(graph);
        // Heed caches opened environments process-wide. Explicitly remove the
        // writer from that cache so this same-process integration test can
        // exercise the auditor's deliberately incompatible READ_ONLY open.
        let spool_closing = spool.env.clone().prepare_for_closing();
        drop(spool);
        spool_closing.wait();
        drop(store);

        let state_sha256 = bytes_sha256(&std::fs::read(&state_path).unwrap());
        let stage_path = staging_data_dir.join(STAGE_DIR).join(STAGE_STATE_FILE);
        let stage_sha256 = bytes_sha256(&std::fs::read(stage_path).unwrap());
        let policy_sha256 = bytes_sha256(&serde_json::to_vec(&state.policy).unwrap());
        let full_mode_error = run_nostr_bulk_projection_audit(
            data_dir.clone(),
            BulkProjectionAuditOptions {
                staging_data_dir: staging_data_dir.clone(),
                expected_state_sha256: state_sha256.clone(),
                expected_stage_state_sha256: stage_sha256.clone(),
                expected_policy_sha256: policy_sha256.clone(),
                expected_profile_distance_seal_sha256: None,
                profile_rank_decisions_file: None,
                expected_profile_rank_decisions_file_sha256: None,
                profile_rank_decisions_report: None,
                expected_profile_rank_decisions_report_sha256: None,
                expected_full_author_count: 101_267,
                allow_recovery_tranche: false,
                btree_order: 8,
                page_size: 2,
                query_limit: 2,
                out: temp.path().join("must-not-write-full-evidence.json"),
            },
        )
        .await
        .unwrap_err();
        assert!(
            full_mode_error.to_string().contains("full-policy audit"),
            "unexpected full-mode error: {full_mode_error:#}"
        );

        let count_error = run_nostr_bulk_projection_audit(
            data_dir.clone(),
            BulkProjectionAuditOptions {
                staging_data_dir: staging_data_dir.clone(),
                expected_state_sha256: state_sha256.clone(),
                expected_stage_state_sha256: stage_sha256.clone(),
                expected_policy_sha256: policy_sha256.clone(),
                expected_profile_distance_seal_sha256: None,
                profile_rank_decisions_file: None,
                expected_profile_rank_decisions_file_sha256: None,
                profile_rank_decisions_report: None,
                expected_profile_rank_decisions_report_sha256: None,
                expected_full_author_count: 101_268,
                allow_recovery_tranche: true,
                btree_order: 8,
                page_size: 2,
                query_limit: 2,
                out: temp.path().join("must-not-write-count-evidence.json"),
            },
        )
        .await
        .unwrap_err();
        assert!(
            count_error
                .to_string()
                .contains("policy author count mismatch"),
            "unexpected trusted-count error: {count_error:#}"
        );

        run_nostr_bulk_projection_audit(
            data_dir,
            BulkProjectionAuditOptions {
                staging_data_dir,
                expected_state_sha256: state_sha256,
                expected_stage_state_sha256: stage_sha256,
                expected_policy_sha256: policy_sha256.clone(),
                expected_profile_distance_seal_sha256: None,
                profile_rank_decisions_file: None,
                expected_profile_rank_decisions_file_sha256: None,
                profile_rank_decisions_report: None,
                expected_profile_rank_decisions_report_sha256: None,
                expected_full_author_count: 101_267,
                allow_recovery_tranche: true,
                btree_order: 8,
                page_size: 2,
                query_limit: 2,
                out: evidence_path.clone(),
            },
        )
        .await
        .unwrap();

        let output: serde_json::Value =
            serde_json::from_slice(&std::fs::read(evidence_path).unwrap()).unwrap();
        assert_eq!(
            output["candidate_root"],
            cid_to_nhash(&candidate_root).unwrap()
        );
        assert_eq!(output["recovery_tranche_only"], true);
        assert_eq!(
            output["audit_mode"],
            "recovery-tranche-internal-non-cutover"
        );
        assert_eq!(output["cutover_eligible"], false);
        assert_eq!(output["trusted_policy_sha256"], policy_sha256);
        assert_eq!(output["trusted_full_author_count"], 101_267);
        assert_eq!(output["crawl_policy_max_follow_distance"], 0);
        assert_eq!(output["authors_processed"], 17_177);
        assert_eq!(output["authors_total"], 101_267);
        assert_eq!(output["indexes"].as_array().unwrap().len(), 9);
        assert_eq!(output["indexes"][0]["durable_values_validated"], 3);
        assert_eq!(output["profile"]["by_pubkey_links"], 1);
        assert!(output["profile"]["search_entries"].as_u64().unwrap() >= 1);
        assert_eq!(
            output["profile"]["by_pubkey_root_file_sha256"]
                .as_str()
                .unwrap()
                .len(),
            64
        );
        assert_eq!(
            output["profile"]["search_root_file_sha256"]
                .as_str()
                .unwrap()
                .len(),
            64
        );
        let query_names = output["queries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|query| query["query"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert!(query_names.contains(&"replaceable"));
        assert!(query_names.contains(&"parameterized-replaceable"));
        assert!(!output["representative_blocks"]
            .as_array()
            .unwrap()
            .is_empty());
    }
}
