use anyhow::{bail, Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
#[cfg(target_os = "linux")]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use super::pool_migration_mount::{
    current_mount_namespace_identity, inspect_source_read_only_mount_teardown_state,
    teardown_one_source_read_only_mount, validate_source_read_only_mount_removed,
    validate_source_read_only_mount_underlying_identity, ReadOnlyMountTeardownEntryV3,
    ReadOnlyMountTeardownStateV3,
};
use super::pool_migration_protocol::{FileAuthorityV3, FileIdentityV3};

pub(super) const MOUNT_TEARDOWN_INTENT_SCHEMA: &str =
    "hashtree-pool-migration-mount-teardown-intent/v3";
pub(super) const MOUNT_TEARDOWN_STEP_SCHEMA: &str =
    "hashtree-pool-migration-mount-teardown-step/v3";
pub(super) const MOUNT_TEARDOWN_RECEIPT_SCHEMA: &str = "hashtree-pool-migration-mount-teardown/v3";
pub(super) const MOUNT_TEARDOWN_INTENT_FILE: &str = "mount-teardown-intent.json";
pub(super) const MOUNT_TEARDOWN_RECEIPT_FILE: &str = "mount-teardown.json";
const MOUNT_TEARDOWN_STEP_PREFIX: &str = "mount-teardown-step-";
const MAX_TEARDOWN_FILE_BYTES: u64 = 4 * 1024 * 1024;
pub(super) const MAX_TERMINAL_AUDIT_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct BoundedFileAuthorityV3 {
    pub(super) path: PathBuf,
    pub(super) sha256: String,
    pub(super) identity: FileIdentityV3,
    pub(super) len: u64,
    pub(super) uid: u32,
    pub(super) gid: u32,
    pub(super) mode: u32,
    pub(super) links: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct MountTeardownIntentV3 {
    pub(super) schema: String,
    pub(super) status: String,
    pub(super) boot_id: String,
    pub(super) rollout_id: String,
    pub(super) attempt_nonce: String,
    pub(super) launch_request_sha256: String,
    pub(super) terminal_audit_path: PathBuf,
    pub(super) terminal_audit_sha256: String,
    pub(super) terminal_audit_authority: BoundedFileAuthorityV3,
    pub(super) non_lazy: bool,
    pub(super) mount_namespace_identity: FileIdentityV3,
    pub(super) mounts: Vec<ReadOnlyMountTeardownEntryV3>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct MountTeardownStepReceiptV3 {
    pub(super) schema: String,
    pub(super) status: String,
    pub(super) boot_id: String,
    pub(super) rollout_id: String,
    pub(super) attempt_nonce: String,
    pub(super) intent_path: PathBuf,
    pub(super) intent_sha256: String,
    pub(super) step_index: u64,
    pub(super) step_count: u64,
    pub(super) previous_step_sha256: Option<String>,
    pub(super) non_lazy: bool,
    pub(super) outcome: String,
    pub(super) removed_mount: ReadOnlyMountTeardownEntryV3,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct MountTeardownStepAuthorityV3 {
    pub(super) step_index: u64,
    pub(super) path: PathBuf,
    pub(super) sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct MountTeardownReceiptV3 {
    pub(super) schema: String,
    pub(super) status: String,
    pub(super) boot_id: String,
    pub(super) rollout_id: String,
    pub(super) attempt_nonce: String,
    pub(super) launch_request_sha256: String,
    pub(super) terminal_audit_path: PathBuf,
    pub(super) terminal_audit_sha256: String,
    pub(super) terminal_audit_authority: BoundedFileAuthorityV3,
    pub(super) intent_path: PathBuf,
    pub(super) intent_sha256: String,
    pub(super) non_lazy: bool,
    pub(super) steps: Vec<MountTeardownStepAuthorityV3>,
    pub(super) removed_mounts: Vec<ReadOnlyMountTeardownEntryV3>,
}

pub(super) fn step_file_name(index: usize) -> String {
    format!("{MOUNT_TEARDOWN_STEP_PREFIX}{index:020}.json")
}

pub(super) fn serialize_json_line<T: Serialize>(value: &T, label: &str) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(value).with_context(|| format!("serialize {label}"))?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub(super) fn sha256_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[cfg(target_os = "linux")]
pub(super) fn capture_bounded_worker_file_authority(
    path: &Path,
    expected_uid: u32,
    expected_gid: u32,
    expected_mode: u32,
    label: &str,
) -> Result<(BoundedFileAuthorityV3, Vec<u8>)> {
    let (metadata, bytes) = read_bounded_regular_file(path, label)?;
    if metadata.uid() != expected_uid
        || metadata.gid() != expected_gid
        || metadata.mode() & 0o7777 != expected_mode
        || metadata.nlink() != 1
    {
        bail!("{label} has invalid worker-owned terminal authority");
    }
    let authority = BoundedFileAuthorityV3 {
        path: path.to_path_buf(),
        sha256: sha256_bytes(&bytes),
        identity: FileIdentityV3 {
            device: metadata.dev(),
            inode: metadata.ino(),
        },
        len: metadata.len(),
        uid: metadata.uid(),
        gid: metadata.gid(),
        mode: metadata.mode() & 0o7777,
        links: metadata.nlink(),
    };
    validate_bounded_terminal_authority(&authority)?;
    Ok((authority, bytes))
}

#[cfg(not(target_os = "linux"))]
pub(super) fn capture_bounded_worker_file_authority(
    _path: &Path,
    _expected_uid: u32,
    _expected_gid: u32,
    _expected_mode: u32,
    _label: &str,
) -> Result<(BoundedFileAuthorityV3, Vec<u8>)> {
    bail!("bounded worker-file authority is supported only on Linux")
}

pub(super) fn validate_bounded_terminal_authority(
    authority: &BoundedFileAuthorityV3,
) -> Result<()> {
    validate_sha256("bounded terminal audit", &authority.sha256)?;
    if !authority.path.is_absolute()
        || authority.identity.device == 0
        || authority.identity.inode == 0
        || authority.len == 0
        || authority.len > MAX_TERMINAL_AUDIT_BYTES
        || authority.uid == 0
        || !matches!(authority.mode, 0o600 | 0o640)
        || authority.links != 1
    {
        bail!("bounded terminal audit has invalid path, inode, ownership, mode, links, or size");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn read_bounded_regular_file(path: &Path, label: &str) -> Result<(std::fs::Metadata, Vec<u8>)> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("open bounded {label} {}", path.display()))?;
    let before = file
        .metadata()
        .with_context(|| format!("inspect open bounded {label}"))?;
    let named = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspect named bounded {label}"))?;
    if !before.file_type().is_file()
        || !same_file(&before, &named)
        || before.len() == 0
        || before.len() > MAX_TERMINAL_AUDIT_BYTES
    {
        bail!("{label} is not a stable bounded regular file");
    }
    let mut bytes = Vec::with_capacity(before.len() as usize);
    (&mut file)
        .take(MAX_TERMINAL_AUDIT_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read bounded {label}"))?;
    let after = file
        .metadata()
        .with_context(|| format!("reinspect open bounded {label}"))?;
    let renamed = std::fs::symlink_metadata(path)
        .with_context(|| format!("reinspect named bounded {label}"))?;
    if bytes.len() as u64 != before.len()
        || !bytes.ends_with(b"\n")
        || !same_file(&before, &after)
        || !same_file(&before, &renamed)
        || before.len() != after.len()
    {
        bail!("{label} changed while read or lacks its terminal newline");
    }
    Ok((before, bytes))
}

#[cfg(target_os = "linux")]
pub(super) fn read_bounded_file_authority(
    authority: &BoundedFileAuthorityV3,
    label: &str,
) -> Result<Vec<u8>> {
    validate_bounded_terminal_authority(authority)?;
    let (metadata, bytes) = read_bounded_regular_file(&authority.path, label)?;
    let actual = BoundedFileAuthorityV3 {
        path: authority.path.clone(),
        sha256: sha256_bytes(&bytes),
        identity: FileIdentityV3 {
            device: metadata.dev(),
            inode: metadata.ino(),
        },
        len: metadata.len(),
        uid: metadata.uid(),
        gid: metadata.gid(),
        mode: metadata.mode() & 0o7777,
        links: metadata.nlink(),
    };
    if &actual != authority {
        bail!("{label} differs from its exact bounded inode authority");
    }
    Ok(bytes)
}

#[cfg(not(target_os = "linux"))]
pub(super) fn read_bounded_file_authority(
    _authority: &BoundedFileAuthorityV3,
    _label: &str,
) -> Result<Vec<u8>> {
    bail!("bounded worker-file authority is supported only on Linux")
}

pub(super) fn validate_teardown_intent(
    intent: &MountTeardownIntentV3,
    attempt_dir: &Path,
    expected_rollout_id: &str,
) -> Result<()> {
    if intent.schema != MOUNT_TEARDOWN_INTENT_SCHEMA
        || intent.status != "authorized"
        || !intent.non_lazy
    {
        bail!("mount-teardown intent has an unsupported schema, status, or teardown mode");
    }
    require_boot_id(&intent.boot_id)?;
    require_safe_component("mount-teardown rollout ID", &intent.rollout_id, 128)?;
    require_safe_component("mount-teardown attempt nonce", &intent.attempt_nonce, 128)?;
    validate_sha256(
        "mount-teardown launch request",
        &intent.launch_request_sha256,
    )?;
    validate_sha256(
        "mount-teardown terminal audit",
        &intent.terminal_audit_sha256,
    )?;
    if intent.rollout_id != expected_rollout_id
        || attempt_dir.file_name().and_then(|name| name.to_str())
            != Some(intent.attempt_nonce.as_str())
        || intent.terminal_audit_path != attempt_dir.join("terminal-audit.json")
    {
        bail!("mount-teardown intent does not bind its exact rollout and attempt namespace");
    }
    validate_bounded_terminal_authority(&intent.terminal_audit_authority)?;
    if intent.terminal_audit_authority.mode != 0o600 {
        bail!("mount-teardown intent must bind the mode 0600 full-final terminal audit");
    }
    if intent.terminal_audit_authority.path != intent.terminal_audit_path
        || intent.terminal_audit_authority.sha256 != intent.terminal_audit_sha256
    {
        bail!(
            "mount-teardown intent terminal audit path/digest is not bound to its inode authority"
        );
    }
    if intent.mount_namespace_identity.device == 0 || intent.mount_namespace_identity.inode == 0 {
        bail!("mount-teardown intent has an invalid mount namespace identity");
    }
    validate_teardown_plan_shape(&intent.mounts)
}

pub(super) fn validate_teardown_step(
    step: &MountTeardownStepReceiptV3,
    intent: &MountTeardownIntentV3,
    intent_path: &Path,
    intent_sha256: &str,
    expected_index: usize,
    previous_step_sha256: Option<&str>,
) -> Result<()> {
    if step.schema != MOUNT_TEARDOWN_STEP_SCHEMA
        || step.status != "removed"
        || !step.non_lazy
        || step.boot_id != intent.boot_id
        || step.rollout_id != intent.rollout_id
        || step.attempt_nonce != intent.attempt_nonce
        || step.intent_path != intent_path
        || step.intent_sha256 != intent_sha256
        || step.step_index != expected_index as u64
        || step.step_count != intent.mounts.len() as u64
        || step.previous_step_sha256.as_deref() != previous_step_sha256
        || !matches!(
            step.outcome.as_str(),
            "unmounted"
                | "confirmed-absent-after-authorized-intent"
                | "prior-boot-mount-namespace-destroyed"
        )
        || step.removed_mount != intent.mounts[expected_index]
    {
        bail!("mount-teardown step receipt breaks the exact ordered intent chain");
    }
    Ok(())
}

pub(super) fn validate_teardown_receipt(
    receipt: &MountTeardownReceiptV3,
    intent: &MountTeardownIntentV3,
    intent_path: &Path,
    intent_sha256: &str,
    steps: &[MountTeardownStepAuthorityV3],
) -> Result<()> {
    if receipt.schema != MOUNT_TEARDOWN_RECEIPT_SCHEMA
        || receipt.status != "verified"
        || !receipt.non_lazy
        || receipt.boot_id != intent.boot_id
        || receipt.rollout_id != intent.rollout_id
        || receipt.attempt_nonce != intent.attempt_nonce
        || receipt.launch_request_sha256 != intent.launch_request_sha256
        || receipt.terminal_audit_path != intent.terminal_audit_path
        || receipt.terminal_audit_sha256 != intent.terminal_audit_sha256
        || receipt.terminal_audit_authority != intent.terminal_audit_authority
        || receipt.intent_path != intent_path
        || receipt.intent_sha256 != intent_sha256
        || receipt.steps != steps
        || receipt.removed_mounts != intent.mounts
    {
        bail!("mount-teardown receipt does not close the exact intent and step chain");
    }
    Ok(())
}

/// Replay every authorized teardown journal before a new worker may launch.
///
/// On the same boot this removes only the exact intended mount ID, or records
/// that the ID already disappeared after the durable intent. Across boots it
/// proves the old mount namespace is gone and never touches a current mount.
pub(super) fn recover_rollout_teardown_state(
    attempts_dir: &Path,
    rollout_id: &str,
    current_boot_id: &str,
) -> Result<()> {
    require_boot_id(current_boot_id)?;
    let entries = match std::fs::read_dir(attempts_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("scan prior Pool migration attempts for teardown"),
    };
    for entry in entries {
        let entry = entry.context("read prior Pool migration attempt entry")?;
        let file_type = entry
            .file_type()
            .context("inspect prior Pool migration attempt entry type")?;
        if !file_type.is_dir() {
            continue;
        }
        let attempt_dir = entry.path();
        let names = teardown_artifact_names(&attempt_dir)?;
        if names.is_empty() {
            continue;
        }
        recover_teardown_attempt(&attempt_dir, rollout_id, current_boot_id, &names).with_context(
            || {
                format!(
                    "prior attempt {} has an invalid or unrecoverable mount-teardown journal",
                    attempt_dir.display()
                )
            },
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn recover_or_create_full_terminal_teardown(
    attempt_dir: &Path,
    rollout_id: &str,
    current_boot_id: &str,
    attempt_nonce: &str,
    launch_request_sha256: &str,
    terminal_audit_authority: BoundedFileAuthorityV3,
    mount_namespace_identity: FileIdentityV3,
    mounts: Vec<ReadOnlyMountTeardownEntryV3>,
) -> Result<FileAuthorityV3> {
    if current_boot_id.is_empty()
        || terminal_audit_authority.path != attempt_dir.join("terminal-audit.json")
    {
        bail!("reconstructed full-final teardown has invalid boot or terminal audit authority");
    }
    let intent = MountTeardownIntentV3 {
        schema: MOUNT_TEARDOWN_INTENT_SCHEMA.to_string(),
        status: "authorized".to_string(),
        boot_id: current_boot_id.to_string(),
        rollout_id: rollout_id.to_string(),
        attempt_nonce: attempt_nonce.to_string(),
        launch_request_sha256: launch_request_sha256.to_string(),
        terminal_audit_path: terminal_audit_authority.path.clone(),
        terminal_audit_sha256: terminal_audit_authority.sha256.clone(),
        terminal_audit_authority,
        non_lazy: true,
        mount_namespace_identity,
        mounts,
    };
    validate_teardown_intent(&intent, attempt_dir, rollout_id)?;
    let mut artifacts = teardown_artifact_names(attempt_dir)?;
    if artifacts.is_empty() {
        let bytes = serialize_json_line(&intent, "reconstructed full-final teardown intent")?;
        durable_create_root_receipt(
            &attempt_dir.join(MOUNT_TEARDOWN_INTENT_FILE),
            &bytes,
            attempt_nonce,
        )?;
        artifacts = teardown_artifact_names(attempt_dir)?;
    } else {
        let (existing, _, _) = load_teardown_intent(attempt_dir, rollout_id, &artifacts)?;
        if existing != intent {
            bail!("existing full-final teardown intent differs from terminal publication");
        }
    }
    recover_teardown_attempt(attempt_dir, rollout_id, current_boot_id, &artifacts)?;
    let receipt_path = attempt_dir.join(MOUNT_TEARDOWN_RECEIPT_FILE);
    let receipt_bytes =
        read_exact_root_receipt(&receipt_path, "reconstructed full-final teardown receipt")?;
    Ok(FileAuthorityV3 {
        path: receipt_path,
        sha256: sha256_bytes(&receipt_bytes),
    })
}

pub(super) fn validate_completed_teardown_attempt(
    attempt_dir: &Path,
    rollout_id: &str,
    current_boot_id: &str,
) -> Result<MountTeardownReceiptV3> {
    let names = teardown_artifact_names(attempt_dir)?;
    validate_completed_teardown_attempt_with_names(attempt_dir, rollout_id, current_boot_id, &names)
}

fn validate_completed_teardown_attempt_with_names(
    attempt_dir: &Path,
    rollout_id: &str,
    current_boot_id: &str,
    artifact_names: &HashSet<String>,
) -> Result<MountTeardownReceiptV3> {
    let (intent, intent_path, intent_sha256) =
        load_teardown_intent(attempt_dir, rollout_id, artifact_names)?;
    let steps = load_contiguous_teardown_steps(
        attempt_dir,
        &intent,
        &intent_path,
        &intent_sha256,
        artifact_names,
        true,
    )?;
    validate_removed_mounts_for_current_boot(&intent, current_boot_id)?;
    if !artifact_names.contains(MOUNT_TEARDOWN_RECEIPT_FILE) {
        bail!("mount-teardown steps have no terminal receipt");
    }
    let receipt_path = attempt_dir.join(MOUNT_TEARDOWN_RECEIPT_FILE);
    let receipt_bytes = read_exact_root_receipt(&receipt_path, "mount-teardown receipt")?;
    let receipt: MountTeardownReceiptV3 = parse_strict(&receipt_bytes, "mount-teardown receipt")?;
    validate_teardown_receipt(&receipt, &intent, &intent_path, &intent_sha256, &steps)?;
    Ok(receipt)
}

fn recover_teardown_attempt(
    attempt_dir: &Path,
    rollout_id: &str,
    current_boot_id: &str,
    artifact_names: &HashSet<String>,
) -> Result<MountTeardownReceiptV3> {
    let (intent, intent_path, intent_sha256) =
        load_teardown_intent(attempt_dir, rollout_id, artifact_names)?;
    let mut steps = load_contiguous_teardown_steps(
        attempt_dir,
        &intent,
        &intent_path,
        &intent_sha256,
        artifact_names,
        false,
    )?;
    if artifact_names.contains(MOUNT_TEARDOWN_RECEIPT_FILE) {
        if steps.len() != intent.mounts.len() {
            bail!("terminal mount-teardown receipt was published before every step");
        }
        return validate_completed_teardown_attempt_with_names(
            attempt_dir,
            rollout_id,
            current_boot_id,
            artifact_names,
        );
    }

    let current_namespace = current_mount_namespace_identity()?;
    let same_boot = intent.boot_id == current_boot_id;
    if same_boot && current_namespace != intent.mount_namespace_identity {
        bail!("same-boot teardown recovery is running in a different mount namespace");
    }
    if same_boot {
        for (index, mount) in intent.mounts[..steps.len()].iter().enumerate() {
            validate_source_read_only_mount_removed(mount)
                .with_context(|| format!("revalidate recovered source mount step {index}"))?;
        }
    } else {
        for (index, mount) in intent.mounts[..steps.len()].iter().enumerate() {
            validate_source_read_only_mount_underlying_identity(mount).with_context(|| {
                format!("revalidate cross-boot source identity for prior step {index}")
            })?;
        }
    }
    for index in steps.len()..intent.mounts.len() {
        let mount = &intent.mounts[index];
        let outcome = if same_boot {
            match inspect_source_read_only_mount_teardown_state(mount)? {
                ReadOnlyMountTeardownStateV3::Mounted => {
                    teardown_one_source_read_only_mount(mount)?;
                    "unmounted"
                }
                ReadOnlyMountTeardownStateV3::Removed => "confirmed-absent-after-authorized-intent",
            }
        } else {
            validate_source_read_only_mount_underlying_identity(mount)?;
            "prior-boot-mount-namespace-destroyed"
        };
        let previous_step_sha256 = steps.last().map(|step| step.sha256.clone());
        let step = MountTeardownStepReceiptV3 {
            schema: MOUNT_TEARDOWN_STEP_SCHEMA.to_string(),
            status: "removed".to_string(),
            boot_id: intent.boot_id.clone(),
            rollout_id: intent.rollout_id.clone(),
            attempt_nonce: intent.attempt_nonce.clone(),
            intent_path: intent_path.clone(),
            intent_sha256: intent_sha256.clone(),
            step_index: index as u64,
            step_count: intent.mounts.len() as u64,
            previous_step_sha256: previous_step_sha256.clone(),
            non_lazy: true,
            outcome: outcome.to_string(),
            removed_mount: mount.clone(),
        };
        validate_teardown_step(
            &step,
            &intent,
            &intent_path,
            &intent_sha256,
            index,
            previous_step_sha256.as_deref(),
        )?;
        let bytes = serialize_json_line(&step, "recovered mount-teardown step receipt")?;
        let path = attempt_dir.join(step_file_name(index));
        durable_create_root_receipt(&path, &bytes, &intent.attempt_nonce)?;
        steps.push(MountTeardownStepAuthorityV3 {
            step_index: index as u64,
            path,
            sha256: sha256_bytes(&bytes),
        });
    }

    let receipt = MountTeardownReceiptV3 {
        schema: MOUNT_TEARDOWN_RECEIPT_SCHEMA.to_string(),
        status: "verified".to_string(),
        boot_id: intent.boot_id.clone(),
        rollout_id: intent.rollout_id.clone(),
        attempt_nonce: intent.attempt_nonce.clone(),
        launch_request_sha256: intent.launch_request_sha256.clone(),
        terminal_audit_path: intent.terminal_audit_path.clone(),
        terminal_audit_sha256: intent.terminal_audit_sha256.clone(),
        terminal_audit_authority: intent.terminal_audit_authority.clone(),
        intent_path: intent_path.clone(),
        intent_sha256: intent_sha256.clone(),
        non_lazy: true,
        steps: steps.clone(),
        removed_mounts: intent.mounts.clone(),
    };
    validate_teardown_receipt(&receipt, &intent, &intent_path, &intent_sha256, &steps)?;
    let bytes = serialize_json_line(&receipt, "recovered terminal mount-teardown receipt")?;
    durable_create_root_receipt(
        &attempt_dir.join(MOUNT_TEARDOWN_RECEIPT_FILE),
        &bytes,
        &intent.attempt_nonce,
    )?;
    let names = teardown_artifact_names(attempt_dir)?;
    validate_completed_teardown_attempt_with_names(attempt_dir, rollout_id, current_boot_id, &names)
}

fn load_teardown_intent(
    attempt_dir: &Path,
    rollout_id: &str,
    artifact_names: &HashSet<String>,
) -> Result<(MountTeardownIntentV3, PathBuf, String)> {
    if !artifact_names.contains(MOUNT_TEARDOWN_INTENT_FILE) {
        bail!("mount-teardown artifacts exist without the root intent");
    }
    let intent_path = attempt_dir.join(MOUNT_TEARDOWN_INTENT_FILE);
    let intent_bytes = read_exact_root_receipt(&intent_path, "mount-teardown intent")?;
    let intent: MountTeardownIntentV3 = parse_strict(&intent_bytes, "mount-teardown intent")?;
    validate_teardown_intent(&intent, attempt_dir, rollout_id)?;
    let intent_sha256 = sha256_bytes(&intent_bytes);
    read_bounded_file_authority(
        &intent.terminal_audit_authority,
        "terminal audit bound by mount teardown",
    )?;
    Ok((intent, intent_path, intent_sha256))
}

fn load_contiguous_teardown_steps(
    attempt_dir: &Path,
    intent: &MountTeardownIntentV3,
    intent_path: &Path,
    intent_sha256: &str,
    artifact_names: &HashSet<String>,
    require_complete: bool,
) -> Result<Vec<MountTeardownStepAuthorityV3>> {
    let mut previous_step_sha256 = None;
    let mut steps = Vec::with_capacity(intent.mounts.len());
    for index in 0..intent.mounts.len() {
        let name = step_file_name(index);
        if !artifact_names.contains(&name) {
            if require_complete {
                bail!("mount-teardown chain is missing durable step {index}");
            }
            if ((index + 1)..intent.mounts.len())
                .any(|later| artifact_names.contains(&step_file_name(later)))
            {
                bail!("mount-teardown chain contains a step after a missing predecessor");
            }
            break;
        }
        let path = attempt_dir.join(&name);
        let bytes = read_exact_root_receipt(&path, "mount-teardown step receipt")?;
        let step: MountTeardownStepReceiptV3 = parse_strict(&bytes, "mount-teardown step receipt")?;
        validate_teardown_step(
            &step,
            intent,
            intent_path,
            intent_sha256,
            index,
            previous_step_sha256.as_deref(),
        )?;
        let sha256 = sha256_bytes(&bytes);
        steps.push(MountTeardownStepAuthorityV3 {
            step_index: index as u64,
            path,
            sha256: sha256.clone(),
        });
        previous_step_sha256 = Some(sha256);
    }
    let exact_names = artifact_names
        .iter()
        .filter(|name| name.starts_with(MOUNT_TEARDOWN_STEP_PREFIX))
        .count();
    if exact_names != intent.mounts.len() {
        if require_complete || exact_names != steps.len() {
            bail!("mount-teardown attempt contains an unexpected step receipt");
        }
    }
    Ok(steps)
}

fn validate_removed_mounts_for_current_boot(
    intent: &MountTeardownIntentV3,
    current_boot_id: &str,
) -> Result<()> {
    let current_namespace = current_mount_namespace_identity()?;
    if intent.boot_id == current_boot_id {
        if current_namespace != intent.mount_namespace_identity {
            bail!("same-boot completed teardown is in a different mount namespace");
        }
        for (index, mount) in intent.mounts.iter().enumerate() {
            validate_source_read_only_mount_removed(mount)
                .with_context(|| format!("revalidate removed source mount step {index}"))?;
        }
    } else {
        for (index, mount) in intent.mounts.iter().enumerate() {
            validate_source_read_only_mount_underlying_identity(mount).with_context(|| {
                format!("revalidate cross-boot source identity for teardown step {index}")
            })?;
        }
    }
    Ok(())
}

fn teardown_artifact_names(attempt_dir: &Path) -> Result<HashSet<String>> {
    let mut names = HashSet::new();
    for entry in std::fs::read_dir(attempt_dir)
        .with_context(|| format!("scan teardown artifacts in {}", attempt_dir.display()))?
    {
        let entry = entry.context("read mount-teardown artifact entry")?;
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if name == MOUNT_TEARDOWN_INTENT_FILE
            || name == MOUNT_TEARDOWN_RECEIPT_FILE
            || name.starts_with(MOUNT_TEARDOWN_STEP_PREFIX)
        {
            if !names.insert(name) {
                bail!("mount-teardown attempt contains a duplicate artifact name");
            }
        }
    }
    Ok(names)
}

fn validate_teardown_plan_shape(mounts: &[ReadOnlyMountTeardownEntryV3]) -> Result<()> {
    if mounts.is_empty() {
        bail!("mount-teardown intent must contain a nonempty exact mount plan");
    }
    let mut paths = HashSet::new();
    let mut mount_ids = HashSet::new();
    for (index, entry) in mounts.iter().enumerate() {
        if !matches!(
            entry.path_type.as_str(),
            "regular-single-link" | "directory"
        ) || !entry.mount.path.is_absolute()
            || entry.mount.path_identity.device == 0
            || entry.mount.path_identity.inode == 0
            || entry.mount.mount_id == 0
            || entry.mount.parent_mount_id == 0
            || entry.mount.filesystem_type.is_empty()
            || entry.mount.mount_source.is_empty()
            || !entry
                .mount
                .mount_options
                .iter()
                .any(|option| option == "ro")
        {
            bail!("mount-teardown intent contains an invalid exact mount authority");
        }
        if !paths.insert(&entry.mount.path) || !mount_ids.insert(entry.mount.mount_id) {
            bail!("mount-teardown intent contains duplicate path or mount identities");
        }
        if index > 0 {
            let previous = &mounts[index - 1];
            let order = previous
                .mount
                .path
                .components()
                .count()
                .cmp(&entry.mount.path.components().count())
                .reverse()
                .then_with(|| previous.mount.path.cmp(&entry.mount.path))
                .then_with(|| previous.mount.mount_id.cmp(&entry.mount.mount_id));
            if order != std::cmp::Ordering::Less {
                bail!("mount-teardown intent is not in its exact stable removal order");
            }
        }
    }
    Ok(())
}

pub(super) fn parse_strict<T: DeserializeOwned>(bytes: &[u8], label: &str) -> Result<T> {
    serde_json::from_slice(bytes).with_context(|| format!("parse strict {label}"))
}

pub(super) fn read_exact_root_receipt(path: &Path, label: &str) -> Result<Vec<u8>> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(target_os = "linux")]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut file = options
        .open(path)
        .with_context(|| format!("open {label} {}", path.display()))?;
    let open = file
        .metadata()
        .with_context(|| format!("inspect open {label}"))?;
    let named =
        std::fs::symlink_metadata(path).with_context(|| format!("inspect named {label}"))?;
    validate_root_receipt_metadata(&open, label)?;
    validate_root_receipt_metadata(&named, label)?;
    if !same_file(&open, &named) {
        bail!("{label} path identity differs from the opened file");
    }
    if open.len() == 0 || open.len() > MAX_TEARDOWN_FILE_BYTES {
        bail!("{label} has an invalid bounded size");
    }
    let mut bytes = Vec::with_capacity(open.len() as usize);
    file.read_to_end(&mut bytes)
        .with_context(|| format!("read {label}"))?;
    if bytes.len() as u64 != open.len() || !bytes.ends_with(b"\n") {
        bail!("{label} changed while read or lacks its terminal newline");
    }
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("rewind {label}"))?;
    let after = file
        .metadata()
        .with_context(|| format!("reinspect open {label}"))?;
    let renamed =
        std::fs::symlink_metadata(path).with_context(|| format!("reinspect named {label}"))?;
    if !same_file(&open, &after) || !same_file(&open, &renamed) || open.len() != after.len() {
        bail!("{label} changed while it was validated");
    }
    Ok(bytes)
}

#[cfg(target_os = "linux")]
pub(super) fn durable_create_root_receipt(path: &Path, bytes: &[u8], nonce: &str) -> Result<()> {
    if !path.is_absolute() {
        bail!("durable teardown receipt path must be absolute");
    }
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("inspect durable teardown receipt destination"),
        Ok(_) => bail!("durable teardown receipt destination already exists"),
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .context("durable teardown receipt has no parent")?;
    if parent
        .canonicalize()
        .context("canonicalize teardown parent")?
        != parent
    {
        bail!("durable teardown receipt parent is not an exact canonical path");
    }
    let name = path
        .file_name()
        .context("durable teardown receipt has no file name")?
        .to_string_lossy();
    let temporary = parent.join(format!(
        ".{name}.recovery-{}-{}.tmp",
        std::process::id(),
        &nonce[..16]
    ));
    match std::fs::symlink_metadata(&temporary) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("inspect teardown recovery staging file"),
        Ok(_) => bail!("teardown recovery staging file already exists"),
    }
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o400)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)
            .context("create teardown recovery staging file")?;
        if unsafe { libc::fchown(file.as_raw_fd(), 0, 0) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("chown teardown recovery staging file");
        }
        if unsafe { libc::fchmod(file.as_raw_fd(), 0o400) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("chmod teardown recovery staging file");
        }
        file.write_all(bytes)
            .context("write teardown recovery staging file")?;
        file.sync_all()
            .context("fsync teardown recovery staging file")?;
        drop(file);
        let old = std::ffi::CString::new(temporary.as_os_str().as_encoded_bytes())
            .context("teardown staging path contains NUL")?;
        let new = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
            .context("teardown receipt path contains NUL")?;
        let status = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                old.as_ptr(),
                libc::AT_FDCWD,
                new.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if status != 0 {
            return Err(std::io::Error::last_os_error())
                .context("publish teardown recovery receipt without replacement");
        }
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(parent)
            .context("open teardown recovery parent for fsync")?
            .sync_all()
            .context("fsync teardown recovery parent")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(not(target_os = "linux"))]
pub(super) fn durable_create_root_receipt(_path: &Path, _bytes: &[u8], _nonce: &str) -> Result<()> {
    bail!("mount-teardown recovery is supported only on Linux")
}

fn validate_file_sha256(path: &Path, expected: &str, label: &str) -> Result<()> {
    validate_sha256(label, expected)?;
    let bytes = std::fs::read(path).with_context(|| format!("read {label} {}", path.display()))?;
    if sha256_bytes(&bytes) != expected {
        bail!("{label} SHA-256 changed");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_root_receipt_metadata(metadata: &std::fs::Metadata, label: &str) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    if !metadata.file_type().is_file()
        || metadata.uid() != 0
        || metadata.gid() != 0
        || metadata.mode() & 0o7777 != 0o400
        || metadata.nlink() != 1
    {
        bail!("{label} must be an exact root:root single-link regular file with mode 0400");
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn validate_root_receipt_metadata(metadata: &std::fs::Metadata, label: &str) -> Result<()> {
    if !metadata.file_type().is_file() {
        bail!("{label} must be a regular file");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn same_file(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

#[cfg(not(target_os = "linux"))]
fn same_file(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    left.len() == right.len() && left.modified().ok() == right.modified().ok()
}

fn validate_sha256(label: &str, value: &str) -> Result<()> {
    require_lower_hex(label, value, 64)
}

pub(super) fn require_boot_id(value: &str) -> Result<()> {
    let compact = value.replace('-', "");
    require_lower_hex("mount-teardown boot ID", &compact, 32)
}

fn require_lower_hex(label: &str, value: &str, length: usize) -> Result<()> {
    if value.len() != length
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("{label} must be exactly {length} lowercase hexadecimal characters");
    }
    Ok(())
}

pub(super) fn require_safe_component(label: &str, value: &str, maximum: usize) -> Result<()> {
    if value.is_empty()
        || value.len() > maximum
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        bail!("{label} is not a safe bounded path component");
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::app::pool_migration_mount::{
        ensure_source_read_only_mount_authority, plan_source_read_only_mount_teardown,
        teardown_one_source_read_only_mount, SourceReadOnlyMountAuthorityV3,
    };
    use crate::app::pool_migration_protocol::{FileIdentityV3, LmdbIdentityV3};
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use tempfile::TempDir;

    fn generated_mount(path: PathBuf, mount_id: u64) -> ReadOnlyMountTeardownEntryV3 {
        ReadOnlyMountTeardownEntryV3 {
            path_type: "directory".to_string(),
            mount: super::super::pool_migration_mount::ReadOnlyBindMountAuthorityV3 {
                path,
                path_identity: FileIdentityV3 {
                    device: 7,
                    inode: mount_id + 100,
                },
                mount_id,
                parent_mount_id: 1,
                device_major: 0,
                device_minor: 7,
                root: PathBuf::from("/"),
                mount_options: vec!["ro".to_string()],
                optional_fields: Vec::new(),
                filesystem_type: "ext4".to_string(),
                mount_source: "/dev/generated".to_string(),
                super_options: vec!["ro".to_string()],
            },
        }
    }

    struct ExactMountCleanup(PathBuf);

    impl Drop for ExactMountCleanup {
        fn drop(&mut self) {
            if let Ok(target) = CString::new(self.0.as_os_str().as_bytes()) {
                unsafe {
                    libc::umount2(target.as_ptr(), 0);
                }
            }
        }
    }

    fn generated_source_fence(root: &Path) -> (SourceReadOnlyMountAuthorityV3, ExactMountCleanup) {
        let source = root.join("source");
        std::fs::create_dir(&source).expect("create generated source");
        let data = source.join("data.mdb");
        std::fs::write(&data, b"generated teardown recovery payload\n")
            .expect("write generated source");
        let metadata = std::fs::metadata(&data).expect("generated data metadata");
        let directory = std::fs::metadata(&source).expect("generated source metadata");
        let identity = LmdbIdentityV3 {
            directory: FileIdentityV3 {
                device: directory.dev(),
                inode: directory.ino(),
            },
            data: FileIdentityV3 {
                device: metadata.dev(),
                inode: metadata.ino(),
            },
            lock: FileIdentityV3 {
                device: metadata.dev(),
                inode: metadata.ino(),
            },
        };
        let authority = ensure_source_read_only_mount_authority(&source, identity, None, None)
            .expect("establish generated source fence");
        (authority, ExactMountCleanup(data))
    }

    fn current_boot_id_for_test() -> String {
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .expect("read generated test boot ID")
            .trim()
            .to_string()
    }

    fn generated_terminal_authority(path: &Path, bytes: &[u8]) -> BoundedFileAuthorityV3 {
        std::fs::write(path, bytes).expect("write generated terminal evidence");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .expect("protect generated terminal evidence");
        let path_c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
            .expect("generated terminal path has no NUL");
        assert_eq!(
            unsafe { libc::chown(path_c.as_ptr(), 65_534, 65_534) },
            0,
            "chown generated terminal evidence"
        );
        capture_bounded_worker_file_authority(
            path,
            65_534,
            65_534,
            0o600,
            "generated terminal evidence",
        )
        .expect("capture generated terminal authority")
        .0
    }

    fn synthetic_terminal_authority(path: PathBuf, sha256: String) -> BoundedFileAuthorityV3 {
        BoundedFileAuthorityV3 {
            path,
            sha256,
            identity: FileIdentityV3 {
                device: 10,
                inode: 11,
            },
            len: 1,
            uid: 65_534,
            gid: 65_534,
            mode: 0o600,
            links: 1,
        }
    }

    #[test]
    fn generated_restart_recovers_every_teardown_crash_boundary() {
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("skip: generated teardown recovery test requires root");
            return;
        }
        for stage in 0..3 {
            let temp = TempDir::new().expect("generated rollout");
            let (authority, _cleanup) = generated_source_fence(temp.path());
            let plan = plan_source_read_only_mount_teardown(&[authority.clone()])
                .expect("plan generated teardown");
            let rollout = temp.path().join("generated-rollout");
            let attempts = rollout.join("attempts-v3");
            let nonce = format!("{stage:x}").repeat(64);
            let attempt = attempts.join(&nonce);
            std::fs::create_dir_all(&attempt).expect("create generated attempt");
            let terminal_path = attempt.join("terminal-audit.json");
            let terminal = b"{\"schema\":\"generated-terminal\"}\n";
            let terminal_authority = generated_terminal_authority(&terminal_path, terminal);
            let intent = MountTeardownIntentV3 {
                schema: MOUNT_TEARDOWN_INTENT_SCHEMA.to_string(),
                status: "authorized".to_string(),
                boot_id: current_boot_id_for_test(),
                rollout_id: "generated-rollout".to_string(),
                attempt_nonce: nonce,
                launch_request_sha256: "1".repeat(64),
                terminal_audit_path: terminal_path,
                terminal_audit_sha256: sha256_bytes(terminal),
                terminal_audit_authority: terminal_authority,
                non_lazy: true,
                mount_namespace_identity: authority.mount_namespace_identity,
                mounts: plan,
            };
            let intent_bytes = serialize_json_line(&intent, "generated recovery intent")
                .expect("serialize intent");
            let intent_path = attempt.join(MOUNT_TEARDOWN_INTENT_FILE);
            std::fs::write(&intent_path, &intent_bytes).expect("write generated intent");
            std::fs::set_permissions(&intent_path, std::fs::Permissions::from_mode(0o400))
                .expect("protect generated intent");

            if stage >= 1 {
                teardown_one_source_read_only_mount(&intent.mounts[0])
                    .expect("simulate crash after exact unmount");
            }
            if stage >= 2 {
                let intent_sha256 = sha256_bytes(&intent_bytes);
                let step = MountTeardownStepReceiptV3 {
                    schema: MOUNT_TEARDOWN_STEP_SCHEMA.to_string(),
                    status: "removed".to_string(),
                    boot_id: intent.boot_id.clone(),
                    rollout_id: intent.rollout_id.clone(),
                    attempt_nonce: intent.attempt_nonce.clone(),
                    intent_path: intent_path.clone(),
                    intent_sha256,
                    step_index: 0,
                    step_count: 1,
                    previous_step_sha256: None,
                    non_lazy: true,
                    outcome: "unmounted".to_string(),
                    removed_mount: intent.mounts[0].clone(),
                };
                let step_bytes =
                    serialize_json_line(&step, "generated recovery step").expect("serialize step");
                durable_create_root_receipt(
                    &attempt.join(step_file_name(0)),
                    &step_bytes,
                    &intent.attempt_nonce,
                )
                .expect("publish generated pre-crash step");
            }

            recover_rollout_teardown_state(&attempts, "generated-rollout", &intent.boot_id)
                .expect("recover generated teardown journal");
            let receipt =
                validate_completed_teardown_attempt(&attempt, "generated-rollout", &intent.boot_id)
                    .expect("validate recovered terminal teardown chain");
            assert_eq!(receipt.removed_mounts, intent.mounts);
            recover_rollout_teardown_state(&attempts, "generated-rollout", &intent.boot_id)
                .expect("completed recovery is idempotently revalidated");
        }
    }

    #[test]
    fn generated_step_chain_rejects_skipped_or_reordered_mount() {
        let temp = TempDir::new().expect("generated attempt");
        let nonce = "b".repeat(64);
        let attempt = temp.path().join(&nonce);
        std::fs::create_dir(&attempt).expect("create generated attempt");
        let intent_path = attempt.join(MOUNT_TEARDOWN_INTENT_FILE);
        let first = generated_mount(attempt.join("deep").join("source"), 43);
        let second = generated_mount(attempt.join("source"), 44);
        let terminal_path = attempt.join("terminal-audit.json");
        let terminal_sha256 = "3".repeat(64);
        let intent = MountTeardownIntentV3 {
            schema: MOUNT_TEARDOWN_INTENT_SCHEMA.to_string(),
            status: "authorized".to_string(),
            boot_id: "00000000-0000-0000-0000-000000000001".to_string(),
            rollout_id: "generated-rollout".to_string(),
            attempt_nonce: nonce,
            launch_request_sha256: "2".repeat(64),
            terminal_audit_path: terminal_path.clone(),
            terminal_audit_sha256: terminal_sha256.clone(),
            terminal_audit_authority: synthetic_terminal_authority(terminal_path, terminal_sha256),
            non_lazy: true,
            mount_namespace_identity: FileIdentityV3 {
                device: 8,
                inode: 9,
            },
            mounts: vec![first.clone(), second.clone()],
        };
        validate_teardown_intent(&intent, &attempt, "generated-rollout")
            .expect("valid ordered generated intent");
        let step = MountTeardownStepReceiptV3 {
            schema: MOUNT_TEARDOWN_STEP_SCHEMA.to_string(),
            status: "removed".to_string(),
            boot_id: intent.boot_id.clone(),
            rollout_id: intent.rollout_id.clone(),
            attempt_nonce: intent.attempt_nonce.clone(),
            intent_path: intent_path.clone(),
            intent_sha256: "4".repeat(64),
            step_index: 0,
            step_count: 2,
            previous_step_sha256: None,
            non_lazy: true,
            outcome: "unmounted".to_string(),
            removed_mount: second,
        };
        let error = validate_teardown_step(&step, &intent, &intent_path, &"4".repeat(64), 0, None)
            .expect_err("reordered teardown step must fail closed");
        assert!(error
            .to_string()
            .contains("breaks the exact ordered intent"));
    }
}
