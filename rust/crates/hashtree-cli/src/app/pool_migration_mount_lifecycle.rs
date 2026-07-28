use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::pool_migration_launch::{FileAuthorityV3, FileIdentityV3, LmdbIdentityV3};
use super::pool_migration_mount::{
    cleanup_planned_source_mounts, inspect_source_read_only_mount_teardown_state,
    plan_source_read_only_mount_teardown, teardown_one_source_read_only_mount,
    validate_planned_source_mount_underlying_identity, validate_source_read_only_mount_authority,
    validate_source_read_only_mount_underlying_identity, ReadOnlyMountTeardownEntryV3,
    ReadOnlyMountTeardownStateV3, SourceReadOnlyMountAuthorityV3, SourceReadOnlyMountPlanV3,
};
use super::pool_migration_teardown::{
    durable_create_root_receipt, parse_strict, read_bounded_file_authority,
    read_exact_root_receipt, recover_or_create_full_terminal_teardown, require_boot_id,
    require_safe_component, serialize_json_line, sha256_bytes, validate_completed_teardown_attempt,
    BoundedFileAuthorityV3, MOUNT_TEARDOWN_RECEIPT_FILE,
};

pub(super) const MOUNT_LIFECYCLE_INTENT_FILE: &str = "mount-lifecycle-intent.json";
pub(super) const MOUNT_LIFECYCLE_MOUNTED_FILE: &str = "mount-lifecycle-mounted.json";
pub(super) const MOUNT_LIFECYCLE_RETAINED_FILE: &str = "mount-lifecycle-retained.json";
pub(super) const MOUNT_LIFECYCLE_CLOSED_FILE: &str = "mount-lifecycle-closed.json";
const MOUNT_LIFECYCLE_INTENT_SCHEMA: &str = "hashtree-pool-migration-mount-lifecycle-intent/v3";
const MOUNT_LIFECYCLE_MOUNTED_SCHEMA: &str = "hashtree-pool-migration-mount-lifecycle-mounted/v3";
const MOUNT_LIFECYCLE_RETAINED_SCHEMA: &str = "hashtree-pool-migration-mount-lifecycle-retained/v3";
const MOUNT_LIFECYCLE_CLOSED_SCHEMA: &str = "hashtree-pool-migration-mount-lifecycle-closed/v3";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct MountLifecycleIntentV3 {
    pub(super) schema: String,
    pub(super) status: String,
    pub(super) boot_id: String,
    pub(super) rollout_id: String,
    pub(super) attempt_nonce: String,
    pub(super) phase: String,
    pub(super) controller_state_sha256: String,
    pub(super) mount_namespace_identity: FileIdentityV3,
    pub(super) source_path: Option<PathBuf>,
    pub(super) source_lmdb_identity: Option<LmdbIdentityV3>,
    pub(super) source_external_path: Option<PathBuf>,
    pub(super) source_external_identity: Option<FileIdentityV3>,
    pub(super) source_plan: Option<SourceReadOnlyMountPlanV3>,
    pub(super) adopted_source_mounts: Vec<SourceReadOnlyMountAuthorityV3>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct MountLifecycleMountedV3 {
    pub(super) schema: String,
    pub(super) status: String,
    pub(super) boot_id: String,
    pub(super) rollout_id: String,
    pub(super) attempt_nonce: String,
    pub(super) intent_path: PathBuf,
    pub(super) intent_sha256: String,
    pub(super) source_mounts: SourceReadOnlyMountAuthorityV3,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct MountLifecycleRetainedV3 {
    pub(super) schema: String,
    pub(super) status: String,
    pub(super) boot_id: String,
    pub(super) rollout_id: String,
    pub(super) attempt_nonce: String,
    pub(super) intent_path: PathBuf,
    pub(super) intent_sha256: String,
    pub(super) mounted_path: PathBuf,
    pub(super) mounted_sha256: String,
    pub(super) source_terminal_authority: BoundedFileAuthorityV3,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct MountLifecycleClosedV3 {
    pub(super) schema: String,
    pub(super) status: String,
    pub(super) boot_id: String,
    pub(super) rollout_id: String,
    pub(super) attempt_nonce: String,
    pub(super) intent_path: PathBuf,
    pub(super) intent_sha256: String,
    pub(super) reason: String,
    pub(super) removed_mount_ids: Vec<u64>,
    pub(super) teardown_receipts: Vec<FileAuthorityV3>,
}

#[derive(Clone)]
pub(super) struct PreparedMountLifecycleV3 {
    pub(super) intent: MountLifecycleIntentV3,
    pub(super) intent_path: PathBuf,
    pub(super) intent_sha256: String,
}

pub(super) fn create_source_mount_lifecycle(
    attempt_dir: &Path,
    boot_id: &str,
    rollout_id: &str,
    attempt_nonce: &str,
    controller_state_sha256: &str,
    source_path: &Path,
    source_lmdb_identity: LmdbIdentityV3,
    source_external_path: Option<&Path>,
    source_external_identity: Option<FileIdentityV3>,
    plan: SourceReadOnlyMountPlanV3,
) -> Result<PreparedMountLifecycleV3> {
    let intent = MountLifecycleIntentV3 {
        schema: MOUNT_LIFECYCLE_INTENT_SCHEMA.to_string(),
        status: "authorized".to_string(),
        boot_id: boot_id.to_string(),
        rollout_id: rollout_id.to_string(),
        attempt_nonce: attempt_nonce.to_string(),
        phase: "final-stopped-source".to_string(),
        controller_state_sha256: controller_state_sha256.to_string(),
        mount_namespace_identity: plan.mount_namespace_identity,
        source_path: Some(source_path.to_path_buf()),
        source_lmdb_identity: Some(source_lmdb_identity),
        source_external_path: source_external_path.map(Path::to_path_buf),
        source_external_identity,
        source_plan: Some(plan),
        adopted_source_mounts: Vec::new(),
    };
    publish_intent(attempt_dir, intent)
}

pub(super) fn create_full_mount_lifecycle(
    attempt_dir: &Path,
    boot_id: &str,
    rollout_id: &str,
    attempt_nonce: &str,
    controller_state_sha256: &str,
    authorities: Vec<SourceReadOnlyMountAuthorityV3>,
) -> Result<PreparedMountLifecycleV3> {
    let first = authorities
        .first()
        .context("full-final lifecycle requires receipt-owned source mounts")?;
    if authorities
        .iter()
        .any(|authority| authority.mount_namespace_identity != first.mount_namespace_identity)
    {
        bail!("full-final lifecycle source mounts span multiple mount namespaces");
    }
    plan_source_read_only_mount_teardown(&authorities)
        .context("validate full-final lifecycle adopted source mounts")?;
    let intent = MountLifecycleIntentV3 {
        schema: MOUNT_LIFECYCLE_INTENT_SCHEMA.to_string(),
        status: "authorized".to_string(),
        boot_id: boot_id.to_string(),
        rollout_id: rollout_id.to_string(),
        attempt_nonce: attempt_nonce.to_string(),
        phase: "final-stopped-full".to_string(),
        controller_state_sha256: controller_state_sha256.to_string(),
        mount_namespace_identity: first.mount_namespace_identity,
        source_path: None,
        source_lmdb_identity: None,
        source_external_path: None,
        source_external_identity: None,
        source_plan: None,
        adopted_source_mounts: authorities,
    };
    publish_intent(attempt_dir, intent)
}

fn publish_intent(
    attempt_dir: &Path,
    intent: MountLifecycleIntentV3,
) -> Result<PreparedMountLifecycleV3> {
    validate_intent(&intent, attempt_dir, &intent.rollout_id)?;
    let bytes = serialize_json_line(&intent, "mount lifecycle intent")?;
    let path = attempt_dir.join(MOUNT_LIFECYCLE_INTENT_FILE);
    durable_create_root_receipt(&path, &bytes, &intent.attempt_nonce)?;
    Ok(PreparedMountLifecycleV3 {
        intent,
        intent_path: path,
        intent_sha256: sha256_bytes(&bytes),
    })
}

pub(super) fn record_source_mounts_created(
    attempt_dir: &Path,
    lifecycle: &PreparedMountLifecycleV3,
    source_mounts: SourceReadOnlyMountAuthorityV3,
) -> Result<FileAuthorityV3> {
    let source_path = lifecycle
        .intent
        .source_path
        .as_deref()
        .context("source lifecycle has no source path")?;
    validate_source_read_only_mount_authority(
        &source_mounts,
        source_path,
        lifecycle
            .intent
            .source_lmdb_identity
            .context("source lifecycle has no LMDB identity")?,
        lifecycle.intent.source_external_path.as_deref(),
        lifecycle.intent.source_external_identity,
    )?;
    if source_mounts.mount_namespace_identity != lifecycle.intent.mount_namespace_identity {
        bail!("created source mounts differ from lifecycle mount namespace");
    }
    let mounted = MountLifecycleMountedV3 {
        schema: MOUNT_LIFECYCLE_MOUNTED_SCHEMA.to_string(),
        status: "mounted".to_string(),
        boot_id: lifecycle.intent.boot_id.clone(),
        rollout_id: lifecycle.intent.rollout_id.clone(),
        attempt_nonce: lifecycle.intent.attempt_nonce.clone(),
        intent_path: lifecycle.intent_path.clone(),
        intent_sha256: lifecycle.intent_sha256.clone(),
        source_mounts,
    };
    let bytes = serialize_json_line(&mounted, "mount lifecycle mounted receipt")?;
    let path = attempt_dir.join(MOUNT_LIFECYCLE_MOUNTED_FILE);
    durable_create_root_receipt(&path, &bytes, &lifecycle.intent.attempt_nonce)?;
    Ok(FileAuthorityV3 {
        path,
        sha256: sha256_bytes(&bytes),
    })
}

pub(super) fn record_source_mounts_retained(
    attempt_dir: &Path,
    lifecycle: &PreparedMountLifecycleV3,
    mounted: &FileAuthorityV3,
    source_terminal_authority: BoundedFileAuthorityV3,
) -> Result<FileAuthorityV3> {
    if lifecycle.intent.phase != "final-stopped-source"
        || mounted.path != attempt_dir.join(MOUNT_LIFECYCLE_MOUNTED_FILE)
    {
        bail!("source retention does not bind the exact source lifecycle attempt");
    }
    read_bounded_file_authority(
        &source_terminal_authority,
        "source terminal bound by mount retention",
    )?;
    let retained = MountLifecycleRetainedV3 {
        schema: MOUNT_LIFECYCLE_RETAINED_SCHEMA.to_string(),
        status: "retained".to_string(),
        boot_id: lifecycle.intent.boot_id.clone(),
        rollout_id: lifecycle.intent.rollout_id.clone(),
        attempt_nonce: lifecycle.intent.attempt_nonce.clone(),
        intent_path: lifecycle.intent_path.clone(),
        intent_sha256: lifecycle.intent_sha256.clone(),
        mounted_path: mounted.path.clone(),
        mounted_sha256: mounted.sha256.clone(),
        source_terminal_authority,
    };
    let bytes = serialize_json_line(&retained, "mount lifecycle retention receipt")?;
    let path = attempt_dir.join(MOUNT_LIFECYCLE_RETAINED_FILE);
    durable_create_root_receipt(&path, &bytes, &lifecycle.intent.attempt_nonce)?;
    Ok(FileAuthorityV3 {
        path,
        sha256: sha256_bytes(&bytes),
    })
}

pub(super) fn recover_source_mounts_retained(
    attempt_dir: &Path,
    rollout_id: &str,
    current_boot_id: &str,
    source_terminal_authority: BoundedFileAuthorityV3,
) -> Result<FileAuthorityV3> {
    let lifecycle = load_intent(attempt_dir, rollout_id)?;
    if lifecycle.intent.phase != "final-stopped-source"
        || lifecycle.intent.boot_id != current_boot_id
    {
        bail!("source mount retention recovery requires its exact same-boot source lifecycle");
    }
    let mounted = load_mounted(attempt_dir, &lifecycle)?;
    validate_lifecycle_source_mounts(&lifecycle.intent, &mounted.source_mounts)?;
    let mounted_path = attempt_dir.join(MOUNT_LIFECYCLE_MOUNTED_FILE);
    let mounted_bytes =
        read_exact_root_receipt(&mounted_path, "recoverable source mounted receipt")?;
    let mounted_authority = FileAuthorityV3 {
        path: mounted_path,
        sha256: sha256_bytes(&mounted_bytes),
    };
    record_source_mounts_retained(
        attempt_dir,
        &lifecycle,
        &mounted_authority,
        source_terminal_authority,
    )
}

pub(super) fn recover_full_mount_lifecycle_closed(
    attempt_dir: &Path,
    rollout_id: &str,
    current_boot_id: &str,
    launch_request_sha256: &str,
    terminal_audit_authority: BoundedFileAuthorityV3,
) -> Result<FileAuthorityV3> {
    let lifecycle = load_intent(attempt_dir, rollout_id)?;
    if lifecycle.intent.phase != "final-stopped-full" || lifecycle.intent.boot_id != current_boot_id
    {
        bail!("full mount teardown recovery requires its exact same-boot lifecycle");
    }
    let mounts = lifecycle
        .intent
        .adopted_source_mounts
        .iter()
        .flat_map(source_mount_entries)
        .collect::<Vec<_>>();
    let teardown = recover_or_create_full_terminal_teardown(
        attempt_dir,
        rollout_id,
        current_boot_id,
        &lifecycle.intent.attempt_nonce,
        launch_request_sha256,
        terminal_audit_authority,
        lifecycle.intent.mount_namespace_identity,
        mounts,
    )?;
    record_full_mount_lifecycle_closed(attempt_dir, &lifecycle, teardown)
}

pub(super) fn record_full_mount_lifecycle_closed(
    attempt_dir: &Path,
    lifecycle: &PreparedMountLifecycleV3,
    teardown_receipt: FileAuthorityV3,
) -> Result<FileAuthorityV3> {
    if lifecycle.intent.phase != "final-stopped-full" {
        bail!("only full-final lifecycle may close through terminal teardown");
    }
    let mounts = lifecycle
        .intent
        .adopted_source_mounts
        .iter()
        .flat_map(source_mount_entries)
        .collect::<Vec<_>>();
    for mount in &mounts {
        if inspect_source_read_only_mount_teardown_state(mount)?
            != ReadOnlyMountTeardownStateV3::Removed
        {
            bail!("full-final lifecycle closed while an adopted source mount remains");
        }
    }
    publish_closed(
        attempt_dir,
        lifecycle,
        "verified-full-teardown",
        mounts.iter().map(|entry| entry.mount.mount_id).collect(),
        vec![teardown_receipt],
    )
}

fn publish_closed(
    attempt_dir: &Path,
    lifecycle: &PreparedMountLifecycleV3,
    reason: &str,
    mut removed_mount_ids: Vec<u64>,
    mut teardown_receipts: Vec<FileAuthorityV3>,
) -> Result<FileAuthorityV3> {
    removed_mount_ids.sort_unstable();
    removed_mount_ids.dedup();
    teardown_receipts.sort_by(|left, right| left.path.cmp(&right.path));
    teardown_receipts.dedup();
    let closed = MountLifecycleClosedV3 {
        schema: MOUNT_LIFECYCLE_CLOSED_SCHEMA.to_string(),
        status: "closed".to_string(),
        boot_id: lifecycle.intent.boot_id.clone(),
        rollout_id: lifecycle.intent.rollout_id.clone(),
        attempt_nonce: lifecycle.intent.attempt_nonce.clone(),
        intent_path: lifecycle.intent_path.clone(),
        intent_sha256: lifecycle.intent_sha256.clone(),
        reason: reason.to_string(),
        removed_mount_ids,
        teardown_receipts,
    };
    validate_closed(&closed, lifecycle)?;
    let bytes = serialize_json_line(&closed, "mount lifecycle closed receipt")?;
    let path = attempt_dir.join(MOUNT_LIFECYCLE_CLOSED_FILE);
    durable_create_root_receipt(&path, &bytes, &lifecycle.intent.attempt_nonce)?;
    Ok(FileAuthorityV3 {
        path,
        sha256: sha256_bytes(&bytes),
    })
}

pub(super) fn recover_rollout_mount_lifecycle_state(
    attempts_dir: &Path,
    rollout_id: &str,
    current_boot_id: &str,
) -> Result<()> {
    require_boot_id(current_boot_id)?;
    let entries = match std::fs::read_dir(attempts_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("scan prior mount lifecycle attempts"),
    };
    let mut full_attempts = Vec::new();
    let mut source_attempts = Vec::new();
    for entry in entries {
        let entry = entry.context("read prior mount lifecycle attempt")?;
        if !entry
            .file_type()
            .context("inspect mount lifecycle attempt type")?
            .is_dir()
        {
            continue;
        }
        let attempt_dir = entry.path();
        if !attempt_dir.join(MOUNT_LIFECYCLE_INTENT_FILE).exists() {
            continue;
        }
        let lifecycle = load_intent(&attempt_dir, rollout_id)?;
        match lifecycle.intent.phase.as_str() {
            "final-stopped-full" => full_attempts.push(attempt_dir),
            "final-stopped-source" => source_attempts.push(attempt_dir),
            _ => bail!("mount lifecycle intent has unsupported recovery phase"),
        }
    }
    full_attempts.sort();
    source_attempts.sort();

    let teardown_authorities =
        completed_teardown_authorities(attempts_dir, rollout_id, current_boot_id)?;
    for attempt_dir in &full_attempts {
        recover_attempt(
            attempt_dir,
            rollout_id,
            current_boot_id,
            &teardown_authorities,
        )
        .with_context(|| {
            format!(
                "recover full mount lifecycle attempt {}",
                attempt_dir.display()
            )
        })?;
    }
    let release_authorities = completed_release_authorities(
        attempts_dir,
        rollout_id,
        current_boot_id,
        teardown_authorities,
    )?;
    for attempt_dir in &source_attempts {
        recover_attempt(
            &attempt_dir,
            rollout_id,
            current_boot_id,
            &release_authorities,
        )
        .with_context(|| {
            format!(
                "recover source mount lifecycle attempt {}",
                attempt_dir.display()
            )
        })?;
    }
    Ok(())
}

pub(super) fn validated_terminal_completion_authority(
    attempt_dir: &Path,
    rollout_id: &str,
    current_boot_id: &str,
    phase: &str,
) -> Result<Option<FileAuthorityV3>> {
    let intent_path = attempt_dir.join(MOUNT_LIFECYCLE_INTENT_FILE);
    if !intent_path.exists() {
        return Ok(None);
    }
    let lifecycle = load_intent(attempt_dir, rollout_id)?;
    if lifecycle.intent.phase != phase {
        bail!("terminal publication phase differs from its mount lifecycle");
    }
    match phase {
        "final-stopped-source" => {
            if lifecycle.intent.boot_id != current_boot_id {
                return Ok(None);
            }
            let retained_path = attempt_dir.join(MOUNT_LIFECYCLE_RETAINED_FILE);
            if !retained_path.exists() {
                return Ok(None);
            }
            let bytes = read_exact_root_receipt(&retained_path, "source mount retention receipt")?;
            let retained: MountLifecycleRetainedV3 =
                parse_strict(&bytes, "source mount retention receipt")?;
            let mounted = load_mounted(attempt_dir, &lifecycle)?;
            validate_retained(&retained, &lifecycle, &mounted)?;
            read_bounded_file_authority(
                &retained.source_terminal_authority,
                "retained source terminal authority",
            )?;
            validate_lifecycle_source_mounts(&lifecycle.intent, &mounted.source_mounts)?;
            Ok(Some(FileAuthorityV3 {
                path: retained_path,
                sha256: sha256_bytes(&bytes),
            }))
        }
        "final-stopped-full" => {
            let closed_path = attempt_dir.join(MOUNT_LIFECYCLE_CLOSED_FILE);
            if !closed_path.exists() {
                return Ok(None);
            }
            let bytes =
                read_exact_root_receipt(&closed_path, "full mount lifecycle closed receipt")?;
            let closed: MountLifecycleClosedV3 =
                parse_strict(&bytes, "full mount lifecycle closed receipt")?;
            validate_closed(&closed, &lifecycle)?;
            if closed.reason != "verified-full-teardown" || closed.teardown_receipts.is_empty() {
                return Ok(None);
            }
            validate_lifecycle_absent(&lifecycle.intent, current_boot_id)?;
            Ok(Some(FileAuthorityV3 {
                path: closed_path,
                sha256: sha256_bytes(&bytes),
            }))
        }
        _ => bail!("terminal publication has an unsupported stopped phase"),
    }
}

fn recover_attempt(
    attempt_dir: &Path,
    rollout_id: &str,
    current_boot_id: &str,
    teardown_authorities: &HashMap<(PathBuf, u64), FileAuthorityV3>,
) -> Result<()> {
    let lifecycle = load_intent(attempt_dir, rollout_id)?;
    let closed_path = attempt_dir.join(MOUNT_LIFECYCLE_CLOSED_FILE);
    if closed_path.exists() {
        let bytes = read_exact_root_receipt(&closed_path, "mount lifecycle closed receipt")?;
        let closed: MountLifecycleClosedV3 =
            parse_strict(&bytes, "mount lifecycle closed receipt")?;
        validate_closed(&closed, &lifecycle)?;
        validate_lifecycle_absent(&lifecycle.intent, current_boot_id)?;
        return Ok(());
    }

    if lifecycle.intent.phase == "final-stopped-source" {
        let retained_path = attempt_dir.join(MOUNT_LIFECYCLE_RETAINED_FILE);
        if retained_path.exists() {
            let retained_bytes =
                read_exact_root_receipt(&retained_path, "mount lifecycle retention receipt")?;
            let retained: MountLifecycleRetainedV3 =
                parse_strict(&retained_bytes, "mount lifecycle retention receipt")?;
            let mounted = load_mounted(attempt_dir, &lifecycle)?;
            validate_retained(&retained, &lifecycle, &mounted)?;
            read_bounded_file_authority(
                &retained.source_terminal_authority,
                "retained source terminal authority",
            )?;
            if lifecycle.intent.boot_id != current_boot_id {
                validate_prior_boot_namespace_destroyed(&lifecycle.intent, current_boot_id)?;
                return publish_closed(
                    attempt_dir,
                    &lifecycle,
                    "prior-boot-mount-namespace-destroyed",
                    source_mount_ids(&mounted.source_mounts),
                    Vec::new(),
                )
                .map(|_| ());
            }
            match validate_lifecycle_source_mounts(&lifecycle.intent, &mounted.source_mounts) {
                Ok(()) => return Ok(()),
                Err(mount_error) => {
                    let ids = source_mount_entries(&mounted.source_mounts);
                    let mut receipts = Vec::new();
                    for entry in &ids {
                        if inspect_source_read_only_mount_teardown_state(entry)?
                            != ReadOnlyMountTeardownStateV3::Removed
                        {
                            return Err(mount_error).context(
                                "retained source mount changed without a completed full teardown",
                            );
                        }
                        let authority = teardown_authorities
                            .get(&(entry.mount.path.clone(), entry.mount.mount_id))
                            .context(
                                "retained source mount disappeared without a completed full teardown authority",
                            )?;
                        receipts.push(authority.clone());
                    }
                    return publish_closed(
                        attempt_dir,
                        &lifecycle,
                        "released-by-completed-full-teardown",
                        ids.iter().map(|entry| entry.mount.mount_id).collect(),
                        receipts,
                    )
                    .map(|_| ());
                }
            }
        }
        if lifecycle.intent.boot_id == current_boot_id
            && attempt_dir
                .join("terminal-publication-intent.json")
                .exists()
        {
            // Terminal readiness publishes its root journal before ACK. Preserve
            // its exact same-boot mount until terminal-publication recovery has
            // validated that journal and either records retention or fails closed.
            let mounted = load_mounted(attempt_dir, &lifecycle)?;
            validate_lifecycle_source_mounts(&lifecycle.intent, &mounted.source_mounts)
                .context("revalidate source mounts retained by terminal publication intent")?;
            return Ok(());
        }
    }

    if lifecycle.intent.phase == "final-stopped-full" && lifecycle.intent.boot_id == current_boot_id
    {
        let mounts = lifecycle
            .intent
            .adopted_source_mounts
            .iter()
            .flat_map(source_mount_entries)
            .collect::<Vec<_>>();
        let states = mounts
            .iter()
            .map(inspect_source_read_only_mount_teardown_state)
            .collect::<Result<Vec<_>>>()?;
        let all_removed = states
            .iter()
            .all(|state| *state == ReadOnlyMountTeardownStateV3::Removed);
        if all_removed {
            let mut receipts = Vec::new();
            for entry in &mounts {
                let authority = teardown_authorities
                    .get(&(entry.mount.path.clone(), entry.mount.mount_id))
                    .context(
                        "full lifecycle mounts disappeared without completed teardown authority",
                    )?;
                receipts.push(authority.clone());
            }
            receipts.sort_by(|left, right| left.path.cmp(&right.path));
            receipts.dedup();
            return publish_closed(
                attempt_dir,
                &lifecycle,
                "verified-full-teardown",
                mounts.iter().map(|entry| entry.mount.mount_id).collect(),
                receipts,
            )
            .map(|_| ());
        }
        let all_mounted = states
            .iter()
            .all(|state| *state == ReadOnlyMountTeardownStateV3::Mounted);
        if !all_mounted {
            bail!("incomplete full-final lifecycle has a mixed retained/removed source mount set");
        }
        plan_source_read_only_mount_teardown(&lifecycle.intent.adopted_source_mounts)
            .context("revalidate retained source mounts for incomplete full-final recovery")?;
        return Ok(());
    }

    let removed = cleanup_unretained_lifecycle(&lifecycle, current_boot_id)?;
    publish_closed(
        attempt_dir,
        &lifecycle,
        if lifecycle.intent.boot_id == current_boot_id {
            "recovered-incomplete-attempt"
        } else {
            "prior-boot-mount-namespace-destroyed"
        },
        removed,
        Vec::new(),
    )
    .map(|_| ())
}

fn cleanup_unretained_lifecycle(
    lifecycle: &PreparedMountLifecycleV3,
    current_boot_id: &str,
) -> Result<Vec<u64>> {
    if lifecycle.intent.boot_id != current_boot_id {
        validate_prior_boot_namespace_destroyed(&lifecycle.intent, current_boot_id)?;
        return Ok(intent_mount_ids(&lifecycle.intent));
    }
    if let Some(plan) = &lifecycle.intent.source_plan {
        cleanup_planned_source_mounts(plan)?;
        // The durable pre-mount plan authorizes cleanup by namespace, path identity,
        // parent mount, device, and root. It intentionally cannot know the fresh
        // mount ID before MS_BIND, so do not make an unbound numeric claim here.
        return Ok(Vec::new());
    }
    let plan = plan_source_read_only_mount_teardown(&lifecycle.intent.adopted_source_mounts)?;
    let mut removed = Vec::with_capacity(plan.len());
    for mount in &plan {
        match inspect_source_read_only_mount_teardown_state(mount)? {
            ReadOnlyMountTeardownStateV3::Mounted => {
                teardown_one_source_read_only_mount(mount)?;
                removed.push(mount.mount.mount_id);
            }
            ReadOnlyMountTeardownStateV3::Removed => {
                removed.push(mount.mount.mount_id);
            }
        }
    }
    Ok(removed)
}

fn completed_teardown_authorities(
    attempts_dir: &Path,
    rollout_id: &str,
    current_boot_id: &str,
) -> Result<HashMap<(PathBuf, u64), FileAuthorityV3>> {
    let mut authorities = HashMap::new();
    let entries = match std::fs::read_dir(attempts_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(authorities),
        Err(error) => return Err(error).context("scan completed teardown authorities"),
    };
    for entry in entries {
        let entry = entry.context("read completed teardown attempt")?;
        if !entry
            .file_type()
            .context("inspect completed teardown attempt type")?
            .is_dir()
        {
            continue;
        }
        let attempt = entry.path();
        let receipt_path = attempt.join(MOUNT_TEARDOWN_RECEIPT_FILE);
        if !receipt_path.exists() {
            continue;
        }
        let receipt = validate_completed_teardown_attempt(&attempt, rollout_id, current_boot_id)?;
        let bytes = read_exact_root_receipt(&receipt_path, "completed mount teardown receipt")?;
        let authority = FileAuthorityV3 {
            path: receipt_path,
            sha256: sha256_bytes(&bytes),
        };
        for mount in receipt.removed_mounts {
            let key = (mount.mount.path, mount.mount.mount_id);
            if authorities.insert(key, authority.clone()).is_some() {
                bail!("multiple completed teardowns claim the same exact source mount");
            }
        }
    }
    Ok(authorities)
}

fn completed_release_authorities(
    attempts_dir: &Path,
    rollout_id: &str,
    current_boot_id: &str,
    mut authorities: HashMap<(PathBuf, u64), FileAuthorityV3>,
) -> Result<HashMap<(PathBuf, u64), FileAuthorityV3>> {
    let entries = match std::fs::read_dir(attempts_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(authorities),
        Err(error) => return Err(error).context("scan completed mount-release authorities"),
    };
    for entry in entries {
        let entry = entry.context("read completed mount-release attempt")?;
        if !entry
            .file_type()
            .context("inspect completed mount-release attempt type")?
            .is_dir()
        {
            continue;
        }
        let attempt_dir = entry.path();
        if !attempt_dir.join(MOUNT_LIFECYCLE_INTENT_FILE).exists()
            || !attempt_dir.join(MOUNT_LIFECYCLE_CLOSED_FILE).exists()
        {
            continue;
        }
        let lifecycle = load_intent(&attempt_dir, rollout_id)?;
        if lifecycle.intent.phase != "final-stopped-full" {
            continue;
        }
        let closed_path = attempt_dir.join(MOUNT_LIFECYCLE_CLOSED_FILE);
        let closed_bytes =
            read_exact_root_receipt(&closed_path, "full mount lifecycle closed receipt")?;
        let closed: MountLifecycleClosedV3 =
            parse_strict(&closed_bytes, "full mount lifecycle closed receipt")?;
        validate_closed(&closed, &lifecycle)?;
        validate_lifecycle_absent(&lifecycle.intent, current_boot_id)?;
        let authority = FileAuthorityV3 {
            path: closed_path,
            sha256: sha256_bytes(&closed_bytes),
        };
        for source in &lifecycle.intent.adopted_source_mounts {
            for mount in source_mount_entries(source) {
                authorities
                    .entry((mount.mount.path, mount.mount.mount_id))
                    .or_insert_with(|| authority.clone());
            }
        }
    }
    Ok(authorities)
}

fn load_intent(attempt_dir: &Path, rollout_id: &str) -> Result<PreparedMountLifecycleV3> {
    let path = attempt_dir.join(MOUNT_LIFECYCLE_INTENT_FILE);
    let bytes = read_exact_root_receipt(&path, "mount lifecycle intent")?;
    let intent: MountLifecycleIntentV3 = parse_strict(&bytes, "mount lifecycle intent")?;
    validate_intent(&intent, attempt_dir, rollout_id)?;
    Ok(PreparedMountLifecycleV3 {
        intent,
        intent_path: path,
        intent_sha256: sha256_bytes(&bytes),
    })
}

fn load_mounted(
    attempt_dir: &Path,
    lifecycle: &PreparedMountLifecycleV3,
) -> Result<MountLifecycleMountedV3> {
    let path = attempt_dir.join(MOUNT_LIFECYCLE_MOUNTED_FILE);
    let bytes = read_exact_root_receipt(&path, "mount lifecycle mounted receipt")?;
    let mounted: MountLifecycleMountedV3 = parse_strict(&bytes, "mount lifecycle mounted receipt")?;
    validate_mounted(&mounted, lifecycle)?;
    Ok(mounted)
}

fn validate_intent(
    intent: &MountLifecycleIntentV3,
    attempt_dir: &Path,
    rollout_id: &str,
) -> Result<()> {
    require_boot_id(&intent.boot_id)?;
    require_safe_component("mount lifecycle rollout ID", &intent.rollout_id, 128)?;
    require_safe_component("mount lifecycle attempt nonce", &intent.attempt_nonce, 128)?;
    if intent.schema != MOUNT_LIFECYCLE_INTENT_SCHEMA
        || intent.status != "authorized"
        || intent.rollout_id != rollout_id
        || attempt_dir.file_name().and_then(|name| name.to_str())
            != Some(intent.attempt_nonce.as_str())
        || intent.controller_state_sha256.len() != 64
        || !intent
            .controller_state_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || intent.mount_namespace_identity.device == 0
        || intent.mount_namespace_identity.inode == 0
    {
        bail!("mount lifecycle intent has invalid authority fields");
    }
    match intent.phase.as_str() {
        "final-stopped-source" => {
            let plan = intent
                .source_plan
                .as_ref()
                .context("source lifecycle intent has no pre-mount plan")?;
            if plan.mount_namespace_identity != intent.mount_namespace_identity
                || intent.source_path.is_none()
                || intent.source_lmdb_identity.is_none()
                || intent.source_external_path.is_none()
                    != intent.source_external_identity.is_none()
                || !intent.adopted_source_mounts.is_empty()
            {
                bail!("source mount lifecycle intent is structurally inconsistent");
            }
        }
        "final-stopped-full" => {
            if intent.source_plan.is_some()
                || intent.source_path.is_some()
                || intent.source_lmdb_identity.is_some()
                || intent.source_external_path.is_some()
                || intent.source_external_identity.is_some()
                || intent.adopted_source_mounts.is_empty()
                || intent.adopted_source_mounts.iter().any(|mounts| {
                    mounts.mount_namespace_identity != intent.mount_namespace_identity
                })
            {
                bail!("full mount lifecycle intent is structurally inconsistent");
            }
        }
        _ => bail!("mount lifecycle intent has unsupported phase"),
    }
    Ok(())
}

fn validate_mounted(
    mounted: &MountLifecycleMountedV3,
    lifecycle: &PreparedMountLifecycleV3,
) -> Result<()> {
    if mounted.schema != MOUNT_LIFECYCLE_MOUNTED_SCHEMA
        || mounted.status != "mounted"
        || mounted.boot_id != lifecycle.intent.boot_id
        || mounted.rollout_id != lifecycle.intent.rollout_id
        || mounted.attempt_nonce != lifecycle.intent.attempt_nonce
        || mounted.intent_path != lifecycle.intent_path
        || mounted.intent_sha256 != lifecycle.intent_sha256
        || mounted.source_mounts.mount_namespace_identity
            != lifecycle.intent.mount_namespace_identity
    {
        bail!("mount lifecycle mounted receipt breaks its exact intent chain");
    }
    Ok(())
}

fn validate_retained(
    retained: &MountLifecycleRetainedV3,
    lifecycle: &PreparedMountLifecycleV3,
    _mounted: &MountLifecycleMountedV3,
) -> Result<()> {
    if retained.schema != MOUNT_LIFECYCLE_RETAINED_SCHEMA
        || retained.status != "retained"
        || retained.boot_id != lifecycle.intent.boot_id
        || retained.rollout_id != lifecycle.intent.rollout_id
        || retained.attempt_nonce != lifecycle.intent.attempt_nonce
        || retained.intent_path != lifecycle.intent_path
        || retained.intent_sha256 != lifecycle.intent_sha256
        || retained.mounted_path
            != lifecycle
                .intent_path
                .parent()
                .context("lifecycle intent path has no attempt directory")?
                .join(MOUNT_LIFECYCLE_MOUNTED_FILE)
    {
        bail!("mount lifecycle retention receipt breaks its exact intent chain");
    }
    let mounted_bytes =
        read_exact_root_receipt(&retained.mounted_path, "retained mounted receipt")?;
    if sha256_bytes(&mounted_bytes) != retained.mounted_sha256 {
        bail!("mount lifecycle retention receipt mounted authority changed");
    }
    Ok(())
}

fn validate_closed(
    closed: &MountLifecycleClosedV3,
    lifecycle: &PreparedMountLifecycleV3,
) -> Result<()> {
    let valid_reason = matches!(
        closed.reason.as_str(),
        "recovered-incomplete-attempt"
            | "prior-boot-mount-namespace-destroyed"
            | "verified-full-teardown"
            | "released-by-completed-full-teardown"
    );
    if closed.schema != MOUNT_LIFECYCLE_CLOSED_SCHEMA
        || closed.status != "closed"
        || closed.boot_id != lifecycle.intent.boot_id
        || closed.rollout_id != lifecycle.intent.rollout_id
        || closed.attempt_nonce != lifecycle.intent.attempt_nonce
        || closed.intent_path != lifecycle.intent_path
        || closed.intent_sha256 != lifecycle.intent_sha256
        || !valid_reason
    {
        bail!("mount lifecycle closed receipt breaks its exact intent chain");
    }
    let mut expected_ids = intent_mount_ids(&lifecycle.intent)
        .into_iter()
        .collect::<HashSet<_>>();
    if lifecycle.intent.phase == "final-stopped-source" {
        let attempt_dir = lifecycle
            .intent_path
            .parent()
            .context("lifecycle intent path has no attempt directory")?;
        let mounted_path = attempt_dir.join(MOUNT_LIFECYCLE_MOUNTED_FILE);
        if mounted_path.exists() {
            let mounted = load_mounted(attempt_dir, lifecycle)?;
            expected_ids.extend(source_mount_ids(&mounted.source_mounts));
        }
    }
    if !closed
        .removed_mount_ids
        .iter()
        .all(|mount_id| expected_ids.contains(mount_id))
    {
        bail!("mount lifecycle closed receipt names an unauthorized mount ID");
    }
    Ok(())
}

fn validate_lifecycle_source_mounts(
    intent: &MountLifecycleIntentV3,
    mounts: &SourceReadOnlyMountAuthorityV3,
) -> Result<()> {
    validate_source_read_only_mount_authority(
        mounts,
        intent
            .source_path
            .as_deref()
            .context("source lifecycle has no source path")?,
        intent
            .source_lmdb_identity
            .context("source lifecycle has no LMDB identity")?,
        intent.source_external_path.as_deref(),
        intent.source_external_identity,
    )
}

fn validate_lifecycle_absent(intent: &MountLifecycleIntentV3, current_boot_id: &str) -> Result<()> {
    if intent.boot_id != current_boot_id {
        return validate_prior_boot_namespace_destroyed(intent, current_boot_id);
    }
    if let Some(plan) = &intent.source_plan {
        if !super::pool_migration_mount::recover_planned_source_read_only_mounts(plan)?.is_empty() {
            bail!("closed source mount lifecycle has a surviving exact mount");
        }
        return Ok(());
    }
    for mount in intent
        .adopted_source_mounts
        .iter()
        .flat_map(source_mount_entries)
    {
        if inspect_source_read_only_mount_teardown_state(&mount)?
            != ReadOnlyMountTeardownStateV3::Removed
        {
            bail!("closed full mount lifecycle has a surviving exact mount");
        }
    }
    Ok(())
}

fn validate_prior_boot_namespace_destroyed(
    intent: &MountLifecycleIntentV3,
    current_boot_id: &str,
) -> Result<()> {
    if intent.boot_id == current_boot_id {
        bail!("prior-boot mount namespace proof was requested for the current boot");
    }
    if let Some(plan) = &intent.source_plan {
        validate_planned_source_mount_underlying_identity(plan)?;
    } else {
        for source in &intent.adopted_source_mounts {
            for mount in source_mount_entries(source) {
                validate_source_read_only_mount_underlying_identity(&mount)?;
            }
        }
    }
    Ok(())
}

fn source_mount_entries(
    mounts: &SourceReadOnlyMountAuthorityV3,
) -> Vec<ReadOnlyMountTeardownEntryV3> {
    let mut entries = vec![ReadOnlyMountTeardownEntryV3 {
        path_type: "regular-single-link".to_string(),
        mount: mounts.data.clone(),
    }];
    if let Some(external) = &mounts.external {
        entries.push(ReadOnlyMountTeardownEntryV3 {
            path_type: "directory".to_string(),
            mount: external.clone(),
        });
    }
    entries
}

fn source_mount_ids(mounts: &SourceReadOnlyMountAuthorityV3) -> Vec<u64> {
    source_mount_entries(mounts)
        .into_iter()
        .map(|entry| entry.mount.mount_id)
        .collect()
}

fn intent_mount_ids(intent: &MountLifecycleIntentV3) -> Vec<u64> {
    intent
        .adopted_source_mounts
        .iter()
        .flat_map(source_mount_ids)
        .collect()
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::super::pool_migration_mount::{
        ensure_source_read_only_mount_authority, require_host_mount_administrator,
    };
    use super::*;
    use std::os::unix::fs::MetadataExt;

    struct MountCleanup(Vec<ReadOnlyMountTeardownEntryV3>);

    impl Drop for MountCleanup {
        fn drop(&mut self) {
            for entry in &self.0 {
                if inspect_source_read_only_mount_teardown_state(entry)
                    .is_ok_and(|state| state == ReadOnlyMountTeardownStateV3::Mounted)
                {
                    let _ = teardown_one_source_read_only_mount(entry);
                }
            }
        }
    }

    fn current_boot_id() -> String {
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .expect("read generated test boot ID")
            .trim()
            .to_string()
    }

    fn generated_source(
        root: &Path,
        name: &str,
    ) -> (PathBuf, LmdbIdentityV3, SourceReadOnlyMountAuthorityV3) {
        let source = root.join(name);
        std::fs::create_dir(&source).expect("create generated source");
        let data = source.join("data.mdb");
        let lock = source.join("lock.mdb");
        std::fs::write(&data, format!("generated retained source {name}\n"))
            .expect("write generated source data");
        std::fs::write(&lock, b"generated lock\n").expect("write generated source lock");
        let file_identity = |path: &Path| {
            let metadata = std::fs::metadata(path).expect("inspect generated source");
            FileIdentityV3 {
                device: metadata.dev(),
                inode: metadata.ino(),
            }
        };
        let identity = LmdbIdentityV3 {
            directory: file_identity(&source),
            data: file_identity(&data),
            lock: file_identity(&lock),
        };
        let authority = ensure_source_read_only_mount_authority(&source, identity, None, None)
            .expect("create generated exact read-only source mount");
        (source, identity, authority)
    }

    #[test]
    fn incomplete_full_recovery_preserves_same_boot_receipt_mounts() {
        if unsafe { libc::geteuid() } != 0 || require_host_mount_administrator().is_err() {
            eprintln!("skip: retained full lifecycle test requires host CAP_SYS_ADMIN");
            return;
        }
        let temp = tempfile::tempdir().expect("create generated lifecycle root");
        let (_, _, authority) = generated_source(temp.path(), "source");
        let entries = source_mount_entries(&authority);
        let _cleanup = MountCleanup(entries.clone());
        let attempts = temp.path().join("attempts-v3");
        std::fs::create_dir(&attempts).expect("create generated attempts namespace");
        let rollout_id = "generated-retained-full";
        let boot_id = current_boot_id();
        let first_nonce = "1".repeat(64);
        let first_attempt = attempts.join(&first_nonce);
        std::fs::create_dir(&first_attempt).expect("create generated failed full attempt");
        create_full_mount_lifecycle(
            &first_attempt,
            &boot_id,
            rollout_id,
            &first_nonce,
            &"a".repeat(64),
            vec![authority.clone()],
        )
        .expect("publish generated full lifecycle intent");

        recover_attempt(&first_attempt, rollout_id, &boot_id, &HashMap::new())
            .expect("recover generated incomplete full attempt");
        recover_attempt(&first_attempt, rollout_id, &boot_id, &HashMap::new())
            .expect("repeat generated incomplete full recovery");
        assert!(
            !first_attempt.join(MOUNT_LIFECYCLE_CLOSED_FILE).exists(),
            "incomplete full recovery incorrectly closed its retained mounts"
        );
        for entry in &entries {
            assert_eq!(
                inspect_source_read_only_mount_teardown_state(entry)
                    .expect("inspect retained generated source mount"),
                ReadOnlyMountTeardownStateV3::Mounted
            );
        }

        let second_nonce = "2".repeat(64);
        let second_attempt = attempts.join(&second_nonce);
        std::fs::create_dir(&second_attempt).expect("create generated restarted full attempt");
        create_full_mount_lifecycle(
            &second_attempt,
            &boot_id,
            rollout_id,
            &second_nonce,
            &"b".repeat(64),
            vec![authority],
        )
        .expect("a fresh full attempt can adopt the retained receipt mounts");
    }

    #[test]
    fn incomplete_full_recovery_rejects_mixed_mount_loss_without_removing_survivor() {
        if unsafe { libc::geteuid() } != 0 || require_host_mount_administrator().is_err() {
            eprintln!("skip: mixed full lifecycle test requires host CAP_SYS_ADMIN");
            return;
        }
        let temp = tempfile::tempdir().expect("create generated mixed lifecycle root");
        let (_, _, first) = generated_source(temp.path(), "source-first");
        let (_, _, second) = generated_source(temp.path(), "source-second");
        let mut entries = source_mount_entries(&first);
        entries.extend(source_mount_entries(&second));
        let _cleanup = MountCleanup(entries.clone());
        let attempts = temp.path().join("attempts-v3");
        std::fs::create_dir(&attempts).expect("create generated attempts namespace");
        let rollout_id = "generated-mixed-full";
        let boot_id = current_boot_id();
        let nonce = "3".repeat(64);
        let attempt = attempts.join(&nonce);
        std::fs::create_dir(&attempt).expect("create generated mixed full attempt");
        create_full_mount_lifecycle(
            &attempt,
            &boot_id,
            rollout_id,
            &nonce,
            &"c".repeat(64),
            vec![first, second],
        )
        .expect("publish generated mixed full lifecycle intent");

        teardown_one_source_read_only_mount(&entries[0])
            .expect("simulate one exact adopted mount disappearing");
        let error = recover_attempt(&attempt, rollout_id, &boot_id, &HashMap::new())
            .expect_err("mixed retained/removed source mounts must fail closed");
        assert!(
            format!("{error:#}").contains("mixed retained/removed"),
            "unexpected mixed recovery error: {error:#}"
        );
        assert_eq!(
            inspect_source_read_only_mount_teardown_state(&entries[1])
                .expect("inspect surviving generated source mount"),
            ReadOnlyMountTeardownStateV3::Mounted,
            "mixed recovery removed the surviving receipt mount"
        );
        assert!(
            !attempt.join(MOUNT_LIFECYCLE_CLOSED_FILE).exists(),
            "mixed recovery incorrectly certified a closed lifecycle"
        );
    }
}
