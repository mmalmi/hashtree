use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[cfg(test)]
use std::sync::atomic::{AtomicU8, Ordering};

use anyhow::{Context, Result};
use hashtree_index::{BTree, BTreeOptions};
use hashtree_nostr::{NostrEventIndex, NostrEventStore, NostrEventStoreOptions};

use super::*;
use crate::app::nostr_index::validate_reachable_root;

#[derive(Debug, Clone)]
pub(crate) struct BulkTrancheBuildOptions {
    pub(crate) staging_data_dir: PathBuf,
    pub(crate) expected_state_sha256: String,
    pub(crate) max_indexes: usize,
    pub(crate) out: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(super) enum BuildFaultPoint {
    IndexRootSyncedBeforeStateCas = 1,
    ManifestSyncedBeforeCandidateCas = 2,
}

#[cfg(test)]
static BUILD_FAULT_POINT: AtomicU8 = AtomicU8::new(0);

#[cfg(test)]
pub(super) struct BuildFaultGuard;

#[cfg(test)]
impl Drop for BuildFaultGuard {
    fn drop(&mut self) {
        BUILD_FAULT_POINT.store(0, Ordering::SeqCst);
    }
}

#[cfg(test)]
pub(super) fn arm_build_fault(point: BuildFaultPoint) -> BuildFaultGuard {
    BUILD_FAULT_POINT
        .compare_exchange(0, point as u8, Ordering::SeqCst, Ordering::SeqCst)
        .expect("only one v3 build fault may be armed at a time");
    BuildFaultGuard
}

#[cfg(test)]
fn inject_build_fault(point: BuildFaultPoint) -> Result<()> {
    if BUILD_FAULT_POINT
        .compare_exchange(point as u8, 0, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        anyhow::bail!("injected v3 build fault at {point:?}");
    }
    Ok(())
}

#[cfg(not(test))]
fn inject_build_fault(_point: BuildFaultPoint) -> Result<()> {
    Ok(())
}

fn load_frozen_stage(
    staging_data_dir: &Path,
    state: &BulkTrancheState,
) -> Result<(StagedNostrCrawlState, String)> {
    let frozen = state
        .working
        .frozen_prefix
        .as_ref()
        .context("v3 candidate build lacks a frozen staged prefix")?;
    let path = staging_data_dir.join(STAGE_DIR).join(STAGE_STATE_FILE);
    let bytes = std::fs::read(&path)
        .with_context(|| format!("read frozen staging state {}", path.display()))?;
    let sha256 = bytes_sha256(&bytes);
    require_sha256(
        "frozen staging state SHA-256",
        &sha256,
        &frozen.observed_stage_state_sha256,
    )?;
    let stage: StagedNostrCrawlState =
        serde_json::from_slice(&bytes).context("parse frozen staging state")?;
    validate_stage_state(&stage, &state.policy, state.ordered_allowlist_count)?;
    if stage.next_author != state.policy.author_count
        || stage.next_author != state.working.next_author
        || stage.events_seen != state.working.events_seen
        || stage.events_selected != state.working.events_selected
        || stage.live_bytes_selected != state.working.live_bytes_selected
    {
        anyhow::bail!(
            "frozen staging state does not match the complete v3 working projection boundary"
        );
    }
    Ok((stage, sha256))
}

fn validate_frozen_build_authority(
    data_dir: &Path,
    staging_data_dir: &Path,
    state: &BulkTrancheState,
    spool: &BulkProjectionSpool,
) -> Result<()> {
    if state.working.next_author != state.policy.author_count {
        anyhow::bail!(
            "v3 candidate build requires the complete {}-author boundary; found {}",
            state.policy.author_count,
            state.working.next_author
        );
    }
    validate_spool_identity(data_dir, &tranche_paths(data_dir).4, &state.spool_identity)?;
    validate_profile_publication_fence(data_dir)?;

    let frozen = state
        .working
        .frozen_prefix
        .as_ref()
        .context("v3 candidate build lacks a frozen staged prefix")?;
    if !frozen.immutable_prefix_eq(&state.working.rolling_prefix) {
        anyhow::bail!("v3 candidate build frozen prefix differs from rolling append authority");
    }
    let (stage, stage_sha256) = load_frozen_stage(staging_data_dir, state)?;
    let reattested = attest_stage_prefix(
        staging_data_dir,
        StagePrefixTarget {
            boundary: state.working.next_author,
            durable_next_author: stage.next_author,
            events_seen: state.working.events_seen,
            events_selected: state.working.events_selected,
            live_bytes_selected: state.working.live_bytes_selected,
        },
        stage_sha256,
        &state.policy,
    )
    .context("reattest complete frozen staging prefix for candidate build")?;
    if !reattested.immutable_prefix_eq(frozen) {
        anyhow::bail!("v3 candidate build staging prefix differs from the Freeze seal");
    }

    let (_, _, evidence_dir, _, _) = tranche_paths(data_dir);
    let rank_authority =
        load_copied_profile_rank_authority(&evidence_dir, &state.profile_rank_authority)?;
    require_profile_rank_policy_binding(
        Some(&rank_authority),
        &state.policy.author_allowlist_sha256,
        state.policy.author_count,
    )?;
    let (distance_sha256, retained_profile_count) = spool
        .profile_distance_seal_for_frozen_authority(&rank_authority.decisions)
        .context("recompute frozen profile-distance authority for candidate build")?;
    let expected_distances = state
        .working
        .frozen_profile_distances
        .as_ref()
        .context("v3 candidate build lacks frozen profile-distance authority")?;
    if distance_sha256 != expected_distances.sha256
        || retained_profile_count != expected_distances.retained_profile_count
    {
        anyhow::bail!("v3 candidate build profile-distance authority differs from the Freeze seal");
    }
    recheck_trusted_profile_rank_decisions(Some(&rank_authority))?;
    Ok(())
}

async fn validate_resumable_roots(
    durable: &hashtree_cli::HashtreeStore,
    state: &BulkTrancheState,
) -> Result<()> {
    validate_build_root_prefix("v3 resumable build", &state.working.built_roots)?;
    let btree = BTree::new(
        durable.store_arc(),
        BTreeOptions {
            order: Some(state.btree_order),
        },
    );
    for index in NostrEventIndex::ALL
        .into_iter()
        .take(state.working.built_roots.len())
    {
        let encoded = state
            .working
            .built_roots
            .get(&index.stable_id())
            .expect("contiguous build root checked");
        let root = parse_root_text(encoded)
            .with_context(|| format!("parse resumable {} root", index.name()))?;
        btree
            .validate_link_tree(Some(&root))
            .await
            .with_context(|| format!("validate resumable {} root", index.name()))?;
    }
    Ok(())
}

fn event_store(
    durable: &hashtree_cli::HashtreeStore,
    state: &BulkTrancheState,
) -> NostrEventStore<hashtree_cli::storage::StorageRouter> {
    NostrEventStore::with_options(
        durable.store_arc(),
        NostrEventStoreOptions {
            btree_order: Some(state.btree_order),
            btree_update_concurrency: Some(state.btree_update_concurrency),
            index_commit_batch_size: Some(state.index_commit_batch_size),
        },
    )
}

async fn materialize_and_validate_candidate(
    durable: &hashtree_cli::HashtreeStore,
    graph: &hashtree_cli::socialgraph::SocialGraphStore,
    state: &BulkTrancheState,
    spool: &BulkProjectionSpool,
    expected_root: Option<&str>,
) -> Result<String> {
    if state.working.built_roots.len() != NostrEventIndex::ALL.len() {
        anyhow::bail!("v3 candidate build has not produced all nine index roots");
    }
    let target = event_store(durable, state);
    super::super::validate_built_index_roots(
        spool,
        &target,
        durable.store_arc(),
        &state.working.built_roots,
        state.btree_order,
    )
    .await
    .context("validate all v3 candidate index roots")?;
    super::super::validate_profile_indexes(spool, graph)
        .context("validate v3 candidate profile indexes")?;

    let roots = NostrEventIndex::ALL
        .into_iter()
        .map(|index| {
            let encoded = state
                .working
                .built_roots
                .get(&index.stable_id())
                .context("v3 candidate state omitted a built index")?;
            Ok((index, Some(parse_root_text(encoded)?)))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let root = target
        .write_bulk_index_manifest(&roots)
        .await
        .context("write v3 bulk index manifest")?
        .context("v3 bulk index manifest was empty")?;
    durable
        .force_sync()
        .context("force-sync v3 bulk index manifest")?;
    inject_build_fault(BuildFaultPoint::ManifestSyncedBeforeCandidateCas)?;
    validate_reachable_root(&target, Some(&root), "v3 bulk candidate root").await?;
    let encoded = cid_to_nhash(&root)?;
    if expected_root.is_some_and(|expected| expected != encoded) {
        anyhow::bail!("reconstructed v3 candidate root differs from the durable Candidate state");
    }
    Ok(encoded)
}

pub(crate) async fn build_bulk_tranche(
    durable: &hashtree_cli::HashtreeStore,
    graph: &hashtree_cli::socialgraph::SocialGraphStore,
    data_dir: &Path,
    options: BulkTrancheBuildOptions,
) -> Result<BulkTrancheTransitionOutput> {
    if options.max_indexes == 0 {
        anyhow::bail!("v3 candidate build max-indexes must be non-zero");
    }
    let (state_path, seals_dir, _, _, _) = tranche_paths(data_dir);
    let (mut state, _, mut state_sha256) = load_state(&state_path, &seals_dir)?
        .context("v3 tranche state does not exist; run prepare, append, and freeze first")?;
    require_sha256(
        "v3 tranche state SHA-256",
        &state_sha256,
        &options.expected_state_sha256,
    )?;
    if !matches!(
        state.phase,
        TranchePhase::Freeze | TranchePhase::Building | TranchePhase::Candidate
    ) {
        anyhow::bail!("v3 candidate build requires Freeze, Building, or Candidate phase");
    }

    let (_, spool_path) = bulk_paths(data_dir);
    let spool = BulkProjectionSpool::open(&spool_path)?;
    validate_frozen_build_authority(data_dir, &options.staging_data_dir, &state, &spool)?;

    if state.phase == TranchePhase::Candidate {
        let expected_root = state
            .working
            .candidate_root
            .as_deref()
            .context("v3 Candidate state omitted its manifest root")?;
        materialize_and_validate_candidate(durable, graph, &state, &spool, Some(expected_root))
            .await?;
        validate_frozen_build_authority(data_dir, &options.staging_data_dir, &state, &spool)?;
        load_v3_candidate_audit_authority(data_dir, &options.staging_data_dir, &state_sha256)
            .context("reload terminal v3 Candidate authority")?;
        let output = transition_output(&state, state_sha256);
        write_output(&output, options.out.as_deref())?;
        return Ok(output);
    }

    if state.phase == TranchePhase::Freeze {
        state.phase = TranchePhase::Building;
        state_sha256 = persist_state_cas(&state_path, &state, Some(&state_sha256))?;
    }
    validate_resumable_roots(durable, &state).await?;

    let mut built_this_run = 0usize;
    for index in NostrEventIndex::ALL {
        if state.working.built_roots.contains_key(&index.stable_id()) {
            continue;
        }
        if built_this_run == options.max_indexes {
            break;
        }
        let started = Instant::now();
        let root = spool
            .build_index_root(index, durable.store_arc(), state.btree_order)
            .await
            .with_context(|| format!("bulk-build v3 {} index", index.name()))?
            .with_context(|| {
                format!(
                    "complete v3 corpus has no entries for required {} index",
                    index.name()
                )
            })?;
        durable
            .force_sync()
            .with_context(|| format!("force-sync v3 {} index", index.name()))?;
        inject_build_fault(BuildFaultPoint::IndexRootSyncedBeforeStateCas)?;
        state
            .working
            .built_roots
            .insert(index.stable_id(), cid_to_nhash(&root)?);
        let previous_sha256 = state_sha256;
        state_sha256 = persist_state_cas(&state_path, &state, Some(&previous_sha256))?;
        built_this_run = built_this_run.saturating_add(1);
        eprintln!(
            "Nostr v3 bulk index complete: index={} built={}/{} elapsed_ms={} state_sha256={}",
            index.name(),
            state.working.built_roots.len(),
            NostrEventIndex::ALL.len(),
            started.elapsed().as_millis(),
            state_sha256
        );
    }

    validate_frozen_build_authority(data_dir, &options.staging_data_dir, &state, &spool)?;
    if state.working.built_roots.len() != NostrEventIndex::ALL.len() {
        validate_resumable_roots(durable, &state).await?;
        let output = transition_output(&state, state_sha256);
        write_output(&output, options.out.as_deref())?;
        return Ok(output);
    }

    let candidate_root =
        materialize_and_validate_candidate(durable, graph, &state, &spool, None).await?;
    validate_frozen_build_authority(data_dir, &options.staging_data_dir, &state, &spool)?;
    state.working.candidate_root = Some(candidate_root);
    state.phase = TranchePhase::Candidate;
    let previous_sha256 = state_sha256;
    state_sha256 = persist_state_cas(&state_path, &state, Some(&previous_sha256))?;
    load_v3_candidate_audit_authority(data_dir, &options.staging_data_dir, &state_sha256)
        .context("validate persisted terminal v3 Candidate authority")?;
    let output = transition_output(&state, state_sha256);
    write_output(&output, options.out.as_deref())?;
    Ok(output)
}
