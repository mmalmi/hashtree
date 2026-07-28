#[cfg(target_os = "linux")]
use anyhow::Context;
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::{
    ffi::{CString, OsStr},
    fs::File,
    fs::OpenOptions,
};

use super::pool_migration_launch::{FileIdentityV3, LmdbIdentityV3};

#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt;
#[cfg(target_os = "linux")]
use std::os::unix::io::{AsRawFd, FromRawFd};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ExecutionNamespaceAuthorityV3 {
    pub(super) user: FileIdentityV3,
    pub(super) pid: FileIdentityV3,
    pub(super) mount: FileIdentityV3,
}

#[cfg(target_os = "linux")]
pub(super) fn require_host_execution_namespace(paths: &[(&Path, &str)]) -> Result<()> {
    host_execution_namespace_authority(paths).map(|_| ())
}

#[cfg(target_os = "linux")]
pub(super) fn host_execution_namespace_authority(
    paths: &[(&Path, &str)],
) -> Result<ExecutionNamespaceAuthorityV3> {
    require_initial_user_namespace()?;
    require_initial_pid_namespace()?;
    require_host_mount_namespace()?;
    validate_host_systemd_runtime_identity()?;
    for (path, label) in paths {
        validate_local_host_filesystem(path, label)?;
    }
    current_execution_namespace_authority()
}

#[cfg(target_os = "linux")]
pub(super) fn require_attested_execution_namespace(
    authority: ExecutionNamespaceAuthorityV3,
    paths: &[(&Path, &str)],
) -> Result<()> {
    require_initial_user_namespace()?;
    let current = current_execution_namespace_authority()?;
    if current != authority {
        bail!(
            "Pool migration worker execution namespaces differ from the root controller's host namespace authority"
        );
    }
    for (path, label) in paths {
        validate_local_host_filesystem(path, label)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(super) fn require_host_mount_administrator() -> Result<()> {
    require_effective_capability(21, "CAP_SYS_ADMIN")
}

#[cfg(not(target_os = "linux"))]
pub(super) fn require_host_execution_namespace(_paths: &[(&Path, &str)]) -> Result<()> {
    bail!("host execution namespace authority is supported only on Linux")
}

#[cfg(not(target_os = "linux"))]
pub(super) fn host_execution_namespace_authority(
    _paths: &[(&Path, &str)],
) -> Result<ExecutionNamespaceAuthorityV3> {
    bail!("host execution namespace authority is supported only on Linux")
}

#[cfg(not(target_os = "linux"))]
pub(super) fn require_attested_execution_namespace(
    _authority: ExecutionNamespaceAuthorityV3,
    _paths: &[(&Path, &str)],
) -> Result<()> {
    bail!("host execution namespace authority is supported only on Linux")
}

#[cfg(not(target_os = "linux"))]
pub(super) fn require_host_mount_administrator() -> Result<()> {
    bail!("host mount authority is supported only on Linux")
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct SourceReadOnlyMountAuthorityV3 {
    pub(super) mount_namespace_identity: FileIdentityV3,
    pub(super) data: ReadOnlyBindMountAuthorityV3,
    pub(super) external: Option<ReadOnlyBindMountAuthorityV3>,
}

#[derive(Clone, Copy)]
pub(super) struct SourceReadOnlyMountValidationV3<'a> {
    pub(super) authority: &'a SourceReadOnlyMountAuthorityV3,
    pub(super) source_path: &'a Path,
    pub(super) source_identity: LmdbIdentityV3,
    pub(super) external_path: Option<&'a Path>,
    pub(super) external_identity: Option<FileIdentityV3>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ReadOnlyBindMountAuthorityV3 {
    pub(super) path: PathBuf,
    pub(super) path_identity: FileIdentityV3,
    pub(super) mount_id: u64,
    pub(super) parent_mount_id: u64,
    pub(super) device_major: u64,
    pub(super) device_minor: u64,
    pub(super) root: PathBuf,
    pub(super) mount_options: Vec<String>,
    pub(super) optional_fields: Vec<String>,
    pub(super) filesystem_type: String,
    pub(super) mount_source: String,
    pub(super) super_options: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct SourceReadOnlyMountPlanV3 {
    pub(super) mount_namespace_identity: FileIdentityV3,
    pub(super) data: PlannedReadOnlyBindMountV3,
    pub(super) external: Option<PlannedReadOnlyBindMountV3>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct PlannedReadOnlyBindMountV3 {
    pub(super) path_type: String,
    pub(super) path: PathBuf,
    pub(super) path_identity: FileIdentityV3,
    pub(super) parent_mount_id: u64,
    pub(super) device_major: u64,
    pub(super) device_minor: u64,
    pub(super) root: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ReadOnlyMountTeardownEntryV3 {
    pub(super) path_type: String,
    pub(super) mount: ReadOnlyBindMountAuthorityV3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReadOnlyMountTeardownStateV3 {
    Mounted,
    Removed,
}

#[cfg(target_os = "linux")]
pub(super) fn ensure_source_read_only_mount_authority(
    source_path: &Path,
    source_identity: LmdbIdentityV3,
    external_path: Option<&Path>,
    external_identity: Option<FileIdentityV3>,
) -> Result<SourceReadOnlyMountAuthorityV3> {
    let plan = plan_source_read_only_mount_authority(
        source_path,
        source_identity,
        external_path,
        external_identity,
    )?;
    ensure_source_read_only_mount_authority_from_plan(
        &plan,
        source_path,
        source_identity,
        external_path,
        external_identity,
    )
}

#[cfg(target_os = "linux")]
pub(super) fn plan_source_read_only_mount_authority(
    source_path: &Path,
    source_identity: LmdbIdentityV3,
    external_path: Option<&Path>,
    external_identity: Option<FileIdentityV3>,
) -> Result<SourceReadOnlyMountPlanV3> {
    let mount_namespace_identity = require_host_mount_namespace()?;
    let data = plan_read_only_self_bind(
        &source_path.join("data.mdb"),
        source_identity.data,
        ExpectedPathType::RegularSingleLink,
        "source LMDB data.mdb",
    )?;
    let external = match (external_path, external_identity) {
        (Some(path), Some(identity)) => Some(plan_read_only_self_bind(
            path,
            identity,
            ExpectedPathType::Directory,
            "source external corpus",
        )?),
        (None, None) => None,
        _ => bail!("source external path and identity authority is incomplete"),
    };
    Ok(SourceReadOnlyMountPlanV3 {
        mount_namespace_identity,
        data,
        external,
    })
}

#[cfg(target_os = "linux")]
pub(super) fn ensure_source_read_only_mount_authority_from_plan(
    plan: &SourceReadOnlyMountPlanV3,
    source_path: &Path,
    source_identity: LmdbIdentityV3,
    external_path: Option<&Path>,
    external_identity: Option<FileIdentityV3>,
) -> Result<SourceReadOnlyMountAuthorityV3> {
    if plan.mount_namespace_identity != require_host_mount_namespace()? {
        bail!("source read-only mount plan belongs to a different mount namespace");
    }
    validate_planned_mount(
        &plan.data,
        &source_path.join("data.mdb"),
        source_identity.data,
        ExpectedPathType::RegularSingleLink,
        "source LMDB data.mdb",
    )?;
    match (&plan.external, external_path, external_identity) {
        (Some(planned), Some(path), Some(identity)) => validate_planned_mount(
            planned,
            path,
            identity,
            ExpectedPathType::Directory,
            "source external corpus",
        )?,
        (None, None, None) => {}
        _ => bail!("source read-only mount plan external authority is incomplete"),
    }
    let data = ensure_read_only_self_bind(
        &plan.data,
        ExpectedPathType::RegularSingleLink,
        "source LMDB data.mdb",
    )?;
    let external = match (external_path, external_identity) {
        (Some(_), Some(_)) => {
            let planned = plan
                .external
                .as_ref()
                .context("source external mount plan is missing")?;
            match ensure_read_only_self_bind(
                planned,
                ExpectedPathType::Directory,
                "source external corpus",
            ) {
                Ok(mount) => Some(mount),
                Err(error) => {
                    let rollback = ReadOnlyMountTeardownEntryV3 {
                        path_type: "regular-single-link".to_string(),
                        mount: data,
                    };
                    return Err(error_with_mount_rollback(error, &[rollback]));
                }
            }
        }
        (None, None) => None,
        _ => {
            let rollback = ReadOnlyMountTeardownEntryV3 {
                path_type: "regular-single-link".to_string(),
                mount: data,
            };
            return Err(error_with_mount_rollback(
                anyhow!("source external path and identity authority is incomplete"),
                &[rollback],
            ));
        }
    };
    let authority = SourceReadOnlyMountAuthorityV3 {
        mount_namespace_identity: plan.mount_namespace_identity,
        data,
        external,
    };
    if let Err(error) = validate_source_read_only_mount_authority(
        &authority,
        source_path,
        source_identity,
        external_path,
        external_identity,
    ) {
        let mut rollback = Vec::with_capacity(2);
        if let Some(external) = &authority.external {
            rollback.push(ReadOnlyMountTeardownEntryV3 {
                path_type: "directory".to_string(),
                mount: external.clone(),
            });
        }
        rollback.push(ReadOnlyMountTeardownEntryV3 {
            path_type: "regular-single-link".to_string(),
            mount: authority.data.clone(),
        });
        return Err(error_with_mount_rollback(error, &rollback));
    }
    Ok(authority)
}

#[cfg(target_os = "linux")]
fn error_with_mount_rollback(
    original: anyhow::Error,
    mounts: &[ReadOnlyMountTeardownEntryV3],
) -> anyhow::Error {
    for (index, mount) in mounts.iter().enumerate() {
        if let Err(rollback) = teardown_one_source_read_only_mount(mount) {
            return anyhow!(
                "source read-only mount setup failed: {original:#}; exact rollback step {index} also failed: {rollback:#}"
            );
        }
    }
    original.context("source read-only mount setup rolled back every mount created by this attempt")
}

#[cfg(target_os = "linux")]
pub(super) fn validate_source_read_only_mount_authority(
    authority: &SourceReadOnlyMountAuthorityV3,
    source_path: &Path,
    source_identity: LmdbIdentityV3,
    external_path: Option<&Path>,
    external_identity: Option<FileIdentityV3>,
) -> Result<()> {
    validate_source_read_only_mount_authorities(&[SourceReadOnlyMountValidationV3 {
        authority,
        source_path,
        source_identity,
        external_path,
        external_identity,
    }])
}

#[cfg(target_os = "linux")]
pub(super) fn validate_source_read_only_mount_authorities(
    validations: &[SourceReadOnlyMountValidationV3<'_>],
) -> Result<()> {
    if validations.is_empty() {
        bail!("source read-only mount validation requires a nonempty authority set");
    }
    let namespace = require_host_mount_namespace()?;
    let mounts = read_mountinfo()?;
    for validation in validations {
        if validation.authority.mount_namespace_identity != namespace {
            bail!("source read-only authority belongs to a different mount namespace");
        }
        validate_read_only_mount_against_snapshot(
            &mounts,
            &validation.authority.data,
            &validation.source_path.join("data.mdb"),
            validation.source_identity.data,
            ExpectedPathType::RegularSingleLink,
            "source LMDB data.mdb",
        )?;
        match (
            &validation.authority.external,
            validation.external_path,
            validation.external_identity,
        ) {
            (Some(mount), Some(path), Some(identity)) => validate_read_only_mount_against_snapshot(
                &mounts,
                mount,
                path,
                identity,
                ExpectedPathType::Directory,
                "source external corpus",
            )?,
            (None, None, None) => {}
            _ => bail!("source external read-only mount authority is incomplete"),
        }
    }
    if require_host_mount_namespace()? != namespace {
        bail!("source read-only mount namespace changed during batched validation");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(super) fn validate_cached_source_read_only_mount_authorities(
    authorities: &[&SourceReadOnlyMountAuthorityV3],
) -> Result<()> {
    if authorities.is_empty() {
        bail!("cached source mount validation requires a nonempty authority set");
    }
    let namespace = require_host_mount_namespace()?;
    let mounts = read_mountinfo()?;
    let mut validated = Vec::<(
        &ReadOnlyBindMountAuthorityV3,
        ExpectedPathType,
        &'static str,
    )>::with_capacity(authorities.len().saturating_mul(2));
    for authority in authorities {
        if authority.mount_namespace_identity != namespace {
            bail!("cached source read-only authority belongs to a different mount namespace");
        }
        for (mount, expected_type, label) in std::iter::once((
            &authority.data,
            ExpectedPathType::RegularSingleLink,
            "source LMDB data.mdb",
        ))
        .chain(
            authority
                .external
                .as_ref()
                .map(|mount| (mount, ExpectedPathType::Directory, "source external corpus")),
        ) {
            if let Some((existing, existing_type, _)) = validated
                .iter()
                .find(|(existing, _, _)| existing.mount_id == mount.mount_id)
            {
                if *existing != mount || *existing_type != expected_type {
                    bail!("duplicate cached source mount ID has conflicting authority");
                }
                continue;
            }
            validate_read_only_mount_against_snapshot(
                &mounts,
                mount,
                &mount.path,
                mount.path_identity,
                expected_type,
                label,
            )?;
            validated.push((mount, expected_type, label));
        }
    }
    if require_host_mount_namespace()? != namespace {
        bail!("source read-only mount namespace changed during cached batched validation");
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub(super) fn validate_source_read_only_mount_authorities(
    _validations: &[SourceReadOnlyMountValidationV3<'_>],
) -> Result<()> {
    bail!("source read-only mount authority is supported only on Linux")
}

#[cfg(not(target_os = "linux"))]
pub(super) fn validate_cached_source_read_only_mount_authorities(
    _authorities: &[&SourceReadOnlyMountAuthorityV3],
) -> Result<()> {
    bail!("source read-only mount authority is supported only on Linux")
}

#[cfg(target_os = "linux")]
fn validate_read_only_mount_against_snapshot(
    mounts: &[MountInfo],
    authority: &ReadOnlyBindMountAuthorityV3,
    expected_path: &Path,
    expected_identity: FileIdentityV3,
    expected_type: ExpectedPathType,
    label: &str,
) -> Result<()> {
    if authority.path != expected_path || authority.path_identity != expected_identity {
        bail!("{label} read-only mount path/identity differs from authority");
    }
    validate_path_identity(expected_path, expected_identity, expected_type, label)?;
    let mounted = exact_mount_for_path(mounts, expected_path)
        .with_context(|| format!("{label} read-only mount disappeared"))?;
    let actual = authority_from_mount(mounted, expected_path, expected_identity);
    if &actual != authority {
        bail!("{label} read-only mount identity/options changed");
    }
    if !mount_is_strictly_read_only(mounted) {
        bail!("{label} mount is not strictly read-only");
    }
    if mounted.device_major != linux_device_major(expected_identity.device)
        || mounted.device_minor != linux_device_minor(expected_identity.device)
    {
        bail!("{label} mount device differs from the pinned path identity");
    }
    if matches!(expected_type, ExpectedPathType::RegularSingleLink)
        && OpenOptions::new().write(true).open(expected_path).is_ok()
    {
        bail!("{label} accepted a write-capable open despite its read-only mount authority");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_read_only_mount(
    authority: &ReadOnlyBindMountAuthorityV3,
    expected_path: &Path,
    expected_identity: FileIdentityV3,
    expected_type: ExpectedPathType,
    label: &str,
) -> Result<()> {
    let mounts = read_mountinfo()?;
    validate_read_only_mount_against_snapshot(
        &mounts,
        authority,
        expected_path,
        expected_identity,
        expected_type,
        label,
    )
}

#[cfg(target_os = "linux")]
pub(super) fn recover_planned_source_read_only_mounts(
    plan: &SourceReadOnlyMountPlanV3,
) -> Result<Vec<ReadOnlyMountTeardownEntryV3>> {
    if plan.mount_namespace_identity != require_host_mount_namespace()? {
        bail!("planned source mounts belong to a different current-boot mount namespace");
    }
    let mut recovered = Vec::with_capacity(2);
    for planned in std::iter::once(&plan.data).chain(plan.external.iter()) {
        let (expected_type, label) = planned_entry_type(planned)?;
        validate_path_identity(&planned.path, planned.path_identity, expected_type, label)?;
        let mounts = read_mountinfo()?;
        let Some(mounted) = exact_mount_for_path(&mounts, &planned.path) else {
            let covering = covering_mount_for_path(&mounts, &planned.path)
                .with_context(|| format!("{label} has no covering mount during recovery"))?;
            let relative = planned
                .path
                .strip_prefix(&covering.mount_point)
                .with_context(|| format!("{label} left its planned covering mount"))?;
            if covering.mount_id != planned.parent_mount_id
                || normalize_absolute_mount_root(&covering.root, relative)? != planned.root
            {
                bail!("{label} covering mount changed before lifecycle recovery");
            }
            continue;
        };
        let authority = authority_from_mount(mounted, &planned.path, planned.path_identity);
        if authority.parent_mount_id != planned.parent_mount_id
            || authority.device_major != planned.device_major
            || authority.device_minor != planned.device_minor
            || authority.root != planned.root
            || !mount_is_strictly_read_only(mounted)
        {
            bail!("{label} exact mount does not match the durable pre-mount authorization");
        }
        recovered.push(ReadOnlyMountTeardownEntryV3 {
            path_type: planned.path_type.clone(),
            mount: authority,
        });
    }
    recovered.sort_by(|left, right| {
        right
            .mount
            .path
            .components()
            .count()
            .cmp(&left.mount.path.components().count())
            .then_with(|| left.mount.path.cmp(&right.mount.path))
            .then_with(|| left.mount.mount_id.cmp(&right.mount.mount_id))
    });
    Ok(recovered)
}

#[cfg(target_os = "linux")]
pub(super) fn cleanup_planned_source_mounts(
    plan: &SourceReadOnlyMountPlanV3,
) -> Result<Vec<ReadOnlyBindMountAuthorityV3>> {
    if plan.mount_namespace_identity != require_host_mount_namespace()? {
        bail!("planned source mounts belong to a different current-boot mount namespace");
    }
    let mut planned_mounts = std::iter::once(&plan.data)
        .chain(plan.external.iter())
        .collect::<Vec<_>>();
    planned_mounts.sort_by(|left, right| {
        right
            .path
            .components()
            .count()
            .cmp(&left.path.components().count())
            .then_with(|| left.path.cmp(&right.path))
    });
    let mut removed = Vec::new();
    for planned in planned_mounts {
        let (expected_type, label) = planned_entry_type(planned)?;
        validate_path_identity(&planned.path, planned.path_identity, expected_type, label)?;
        let mounts = read_mountinfo()?;
        let Some(mounted) = exact_mount_for_path(&mounts, &planned.path) else {
            continue;
        };
        let authority = authority_from_mount(mounted, &planned.path, planned.path_identity);
        if authority.parent_mount_id != planned.parent_mount_id
            || authority.device_major != planned.device_major
            || authority.device_minor != planned.device_minor
            || authority.root != planned.root
        {
            bail!("{label} exact mount does not match the durable pre-mount cleanup authority");
        }
        let target = c_string(planned.path.as_os_str(), label)?;
        if unsafe { libc::umount2(target.as_ptr(), 0) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("non-lazy lifecycle cleanup of {label}"));
        }
        if read_mountinfo()?
            .iter()
            .any(|candidate| candidate.mount_id == authority.mount_id)
        {
            bail!("{label} mount identity remained after lifecycle cleanup");
        }
        validate_path_identity(&planned.path, planned.path_identity, expected_type, label)?;
        removed.push(authority);
    }
    Ok(removed)
}

#[cfg(target_os = "linux")]
pub(super) fn teardown_source_read_only_mount_authorities(
    authorities: &[SourceReadOnlyMountAuthorityV3],
) -> Result<Vec<ReadOnlyBindMountAuthorityV3>> {
    let plan = plan_source_read_only_mount_teardown(authorities)?;
    let mut removed = Vec::with_capacity(plan.len());
    for entry in plan {
        teardown_one_source_read_only_mount(&entry)?;
        removed.push(entry.mount);
    }
    Ok(removed)
}

#[cfg(target_os = "linux")]
pub(super) fn plan_source_read_only_mount_teardown(
    authorities: &[SourceReadOnlyMountAuthorityV3],
) -> Result<Vec<ReadOnlyMountTeardownEntryV3>> {
    if authorities.is_empty() {
        bail!("source read-only mount teardown requires a nonempty exact authority set");
    }
    let namespace = require_host_mount_namespace()?;
    let mut mounts = Vec::<ReadOnlyMountTeardownEntryV3>::new();
    for authority in authorities {
        if authority.mount_namespace_identity != namespace {
            bail!("source mount teardown authority belongs to a different mount namespace");
        }
        mounts.push(ReadOnlyMountTeardownEntryV3 {
            path_type: "regular-single-link".to_string(),
            mount: authority.data.clone(),
        });
        if let Some(external) = &authority.external {
            mounts.push(ReadOnlyMountTeardownEntryV3 {
                path_type: "directory".to_string(),
                mount: external.clone(),
            });
        }
    }
    mounts.sort_by(|left, right| {
        right
            .mount
            .path
            .components()
            .count()
            .cmp(&left.mount.path.components().count())
            .then_with(|| left.mount.path.cmp(&right.mount.path))
            .then_with(|| left.mount.mount_id.cmp(&right.mount.mount_id))
    });
    mounts.dedup_by(|left, right| {
        left.mount.mount_id == right.mount.mount_id
            && left.mount.path == right.mount.path
            && left == right
    });
    let mut paths = std::collections::HashSet::new();
    let mut mount_ids = std::collections::HashSet::new();
    for entry in &mounts {
        let (expected_type, label) = teardown_entry_type(entry)?;
        let mount = &entry.mount;
        if !paths.insert(mount.path.clone()) || !mount_ids.insert(mount.mount_id) {
            bail!("source mount teardown contains conflicting path or mount identities");
        }
        validate_read_only_mount(
            mount,
            &mount.path,
            mount.path_identity,
            expected_type,
            label,
        )?;
    }
    Ok(mounts)
}

#[cfg(target_os = "linux")]
pub(super) fn teardown_one_source_read_only_mount(
    entry: &ReadOnlyMountTeardownEntryV3,
) -> Result<()> {
    let (expected_type, label) = teardown_entry_type(entry)?;
    let mount = &entry.mount;
    let before = read_mountinfo()?;
    let exact = exact_mount_for_path(&before, &mount.path)
        .with_context(|| format!("{label} disappeared before exact teardown"))?;
    if exact.mount_id != mount.mount_id {
        bail!("{label} mount identity changed before exact teardown");
    }
    validate_read_only_mount(
        mount,
        &mount.path,
        mount.path_identity,
        expected_type,
        label,
    )?;
    let target = c_string(mount.path.as_os_str(), label)?;
    if unsafe { libc::umount2(target.as_ptr(), 0) } != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| {
            format!(
                "non-lazy exact teardown of {label} mount {}",
                mount.path.display()
            )
        });
    }
    validate_source_read_only_mount_removed(entry)
}

#[cfg(target_os = "linux")]
pub(super) fn validate_source_read_only_mount_removed(
    entry: &ReadOnlyMountTeardownEntryV3,
) -> Result<()> {
    let (expected_type, label) = teardown_entry_type(entry)?;
    let mount = &entry.mount;
    let after = read_mountinfo()?;
    if after
        .iter()
        .any(|candidate| candidate.mount_id == mount.mount_id)
    {
        bail!("{label} mount identity remained after non-lazy exact teardown");
    }
    validate_path_identity(&mount.path, mount.path_identity, expected_type, label)
}

#[cfg(target_os = "linux")]
pub(super) fn inspect_source_read_only_mount_teardown_state(
    entry: &ReadOnlyMountTeardownEntryV3,
) -> Result<ReadOnlyMountTeardownStateV3> {
    let (expected_type, label) = teardown_entry_type(entry)?;
    let mounts = read_mountinfo()?;
    let matching_id = mounts
        .iter()
        .find(|candidate| candidate.mount_id == entry.mount.mount_id);
    match matching_id {
        Some(candidate) => {
            let exact = exact_mount_for_path(&mounts, &entry.mount.path)
                .with_context(|| format!("{label} intended mount ID is no longer exact"))?;
            if candidate.mount_point != entry.mount.path || exact.mount_id != entry.mount.mount_id {
                bail!("{label} intended mount ID was reused, moved, or covered");
            }
            validate_read_only_mount(
                &entry.mount,
                &entry.mount.path,
                entry.mount.path_identity,
                expected_type,
                label,
            )?;
            Ok(ReadOnlyMountTeardownStateV3::Mounted)
        }
        None => {
            validate_source_read_only_mount_removed(entry)?;
            Ok(ReadOnlyMountTeardownStateV3::Removed)
        }
    }
}

#[cfg(target_os = "linux")]
pub(super) fn validate_source_read_only_mount_underlying_identity(
    entry: &ReadOnlyMountTeardownEntryV3,
) -> Result<()> {
    let (expected_type, label) = teardown_entry_type(entry)?;
    validate_path_identity(
        &entry.mount.path,
        entry.mount.path_identity,
        expected_type,
        label,
    )
}

#[cfg(target_os = "linux")]
pub(super) fn validate_planned_source_mount_underlying_identity(
    plan: &SourceReadOnlyMountPlanV3,
) -> Result<()> {
    for planned in std::iter::once(&plan.data).chain(plan.external.iter()) {
        let (expected_type, label) = planned_entry_type(planned)?;
        validate_path_identity(&planned.path, planned.path_identity, expected_type, label)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(super) fn current_mount_namespace_identity() -> Result<FileIdentityV3> {
    require_host_mount_namespace()
}

#[cfg(target_os = "linux")]
fn teardown_entry_type(
    entry: &ReadOnlyMountTeardownEntryV3,
) -> Result<(ExpectedPathType, &'static str)> {
    match entry.path_type.as_str() {
        "regular-single-link" => Ok((ExpectedPathType::RegularSingleLink, "source LMDB data.mdb")),
        "directory" => Ok((ExpectedPathType::Directory, "source external corpus")),
        _ => bail!("source mount teardown entry has an unsupported path type"),
    }
}

#[cfg(target_os = "linux")]
fn planned_entry_type(
    entry: &PlannedReadOnlyBindMountV3,
) -> Result<(ExpectedPathType, &'static str)> {
    match entry.path_type.as_str() {
        "regular-single-link" => Ok((ExpectedPathType::RegularSingleLink, "source LMDB data.mdb")),
        "directory" => Ok((ExpectedPathType::Directory, "source external corpus")),
        _ => bail!("source pre-mount plan has an unsupported path type"),
    }
}

#[cfg(not(target_os = "linux"))]
pub(super) fn validate_source_read_only_mount_authority(
    _authority: &SourceReadOnlyMountAuthorityV3,
    _source_path: &Path,
    _source_identity: LmdbIdentityV3,
    _external_path: Option<&Path>,
    _external_identity: Option<FileIdentityV3>,
) -> Result<()> {
    bail!("source read-only mount authority is supported only on Linux")
}

#[cfg(not(target_os = "linux"))]
pub(super) fn plan_source_read_only_mount_authority(
    _source_path: &Path,
    _source_identity: LmdbIdentityV3,
    _external_path: Option<&Path>,
    _external_identity: Option<FileIdentityV3>,
) -> Result<SourceReadOnlyMountPlanV3> {
    bail!("source read-only mount planning is supported only on Linux")
}

#[cfg(not(target_os = "linux"))]
pub(super) fn ensure_source_read_only_mount_authority_from_plan(
    _plan: &SourceReadOnlyMountPlanV3,
    _source_path: &Path,
    _source_identity: LmdbIdentityV3,
    _external_path: Option<&Path>,
    _external_identity: Option<FileIdentityV3>,
) -> Result<SourceReadOnlyMountAuthorityV3> {
    bail!("source read-only mount creation is supported only on Linux")
}

#[cfg(not(target_os = "linux"))]
pub(super) fn recover_planned_source_read_only_mounts(
    _plan: &SourceReadOnlyMountPlanV3,
) -> Result<Vec<ReadOnlyMountTeardownEntryV3>> {
    bail!("source read-only mount recovery is supported only on Linux")
}

#[cfg(not(target_os = "linux"))]
pub(super) fn cleanup_planned_source_mounts(
    _plan: &SourceReadOnlyMountPlanV3,
) -> Result<Vec<ReadOnlyBindMountAuthorityV3>> {
    bail!("source mount lifecycle cleanup is supported only on Linux")
}

#[cfg(not(target_os = "linux"))]
pub(super) fn teardown_source_read_only_mount_authorities(
    _authorities: &[SourceReadOnlyMountAuthorityV3],
) -> Result<Vec<ReadOnlyBindMountAuthorityV3>> {
    bail!("source read-only mount teardown is supported only on Linux")
}

#[cfg(not(target_os = "linux"))]
pub(super) fn plan_source_read_only_mount_teardown(
    _authorities: &[SourceReadOnlyMountAuthorityV3],
) -> Result<Vec<ReadOnlyMountTeardownEntryV3>> {
    bail!("source read-only mount teardown is supported only on Linux")
}

#[cfg(not(target_os = "linux"))]
pub(super) fn teardown_one_source_read_only_mount(
    _entry: &ReadOnlyMountTeardownEntryV3,
) -> Result<()> {
    bail!("source read-only mount teardown is supported only on Linux")
}

#[cfg(not(target_os = "linux"))]
pub(super) fn validate_source_read_only_mount_removed(
    _entry: &ReadOnlyMountTeardownEntryV3,
) -> Result<()> {
    bail!("source read-only mount teardown is supported only on Linux")
}

#[cfg(not(target_os = "linux"))]
pub(super) fn inspect_source_read_only_mount_teardown_state(
    _entry: &ReadOnlyMountTeardownEntryV3,
) -> Result<ReadOnlyMountTeardownStateV3> {
    bail!("source read-only mount teardown is supported only on Linux")
}

#[cfg(not(target_os = "linux"))]
pub(super) fn validate_source_read_only_mount_underlying_identity(
    _entry: &ReadOnlyMountTeardownEntryV3,
) -> Result<()> {
    bail!("source read-only mount teardown is supported only on Linux")
}

#[cfg(not(target_os = "linux"))]
pub(super) fn validate_planned_source_mount_underlying_identity(
    _plan: &SourceReadOnlyMountPlanV3,
) -> Result<()> {
    bail!("source read-only mount teardown is supported only on Linux")
}

#[cfg(not(target_os = "linux"))]
pub(super) fn current_mount_namespace_identity() -> Result<FileIdentityV3> {
    bail!("source read-only mount teardown is supported only on Linux")
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, PartialEq, Eq)]
enum ExpectedPathType {
    RegularSingleLink,
    Directory,
}

#[cfg(target_os = "linux")]
fn ensure_read_only_self_bind(
    planned: &PlannedReadOnlyBindMountV3,
    expected_type: ExpectedPathType,
    label: &str,
) -> Result<ReadOnlyBindMountAuthorityV3> {
    let path = &planned.path;
    let expected_identity = planned.path_identity;
    validate_planned_mount(planned, path, expected_identity, expected_type, label)?;
    let before = read_mountinfo()?;
    if let Some(existing) = exact_mount_for_path(&before, path) {
        bail!(
            "{label} already has exact mount ID {}; refusing to adopt or later tear down a mount not freshly created by this controller attempt",
            existing.mount_id
        );
    }

    let source = c_string(path.as_os_str(), label)?;
    let target = c_string(path.as_os_str(), label)?;
    let bind_status = unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND,
            std::ptr::null(),
        )
    };
    if bind_status != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("self-bind {label} {}", path.display()));
    }
    let remount_flags = libc::MS_BIND
        | libc::MS_REMOUNT
        | libc::MS_RDONLY
        | libc::MS_NOSUID
        | libc::MS_NODEV
        | libc::MS_NOEXEC;
    let remount_status = unsafe {
        libc::mount(
            std::ptr::null(),
            target.as_ptr(),
            std::ptr::null(),
            remount_flags,
            std::ptr::null(),
        )
    };
    if remount_status != 0 {
        let error = std::io::Error::last_os_error();
        unsafe {
            libc::umount2(target.as_ptr(), 0);
        }
        return Err(error).with_context(|| format!("remount {label} read-only"));
    }

    let result = (|| {
        validate_path_identity(path, expected_identity, expected_type, label)?;
        let mounts = read_mountinfo()?;
        let mounted = exact_mount_for_path(&mounts, path)
            .with_context(|| format!("{label} has no exact self-bind mount after mount"))?;
        if !mount_is_strictly_read_only(mounted) {
            bail!("{label} self-bind is missing ro,nosuid,nodev,noexec mount options");
        }
        if existing_mount_ids(&before).contains(&mounted.mount_id) {
            bail!("{label} self-bind did not create a fresh mount identity");
        }
        let authority = authority_from_mount(mounted, path, expected_identity);
        if authority.parent_mount_id != planned.parent_mount_id
            || authority.device_major != planned.device_major
            || authority.device_minor != planned.device_minor
            || authority.root != planned.root
        {
            bail!("{label} self-bind differs from its durable pre-mount plan");
        }
        Ok(authority)
    })();
    if result.is_err() {
        unsafe {
            libc::umount2(target.as_ptr(), 0);
        }
    }
    result
}

#[cfg(target_os = "linux")]
fn plan_read_only_self_bind(
    path: &Path,
    expected_identity: FileIdentityV3,
    expected_type: ExpectedPathType,
    label: &str,
) -> Result<PlannedReadOnlyBindMountV3> {
    validate_path_identity(path, expected_identity, expected_type, label)?;
    let mounts = read_mountinfo()?;
    if let Some(existing) = exact_mount_for_path(&mounts, path) {
        bail!(
            "{label} already has exact mount ID {}; refusing to plan adoption of a mount not freshly created by this controller attempt",
            existing.mount_id
        );
    }
    let covering = covering_mount_for_path(&mounts, path)
        .with_context(|| format!("{label} has no covering mount for durable planning"))?;
    let relative = path
        .strip_prefix(&covering.mount_point)
        .with_context(|| format!("{label} is not beneath its covering mount"))?;
    let root = normalize_absolute_mount_root(&covering.root, relative)?;
    Ok(PlannedReadOnlyBindMountV3 {
        path_type: match expected_type {
            ExpectedPathType::RegularSingleLink => "regular-single-link",
            ExpectedPathType::Directory => "directory",
        }
        .to_string(),
        path: path.to_path_buf(),
        path_identity: expected_identity,
        parent_mount_id: covering.mount_id,
        device_major: linux_device_major(expected_identity.device),
        device_minor: linux_device_minor(expected_identity.device),
        root,
    })
}

#[cfg(target_os = "linux")]
fn validate_planned_mount(
    planned: &PlannedReadOnlyBindMountV3,
    expected_path: &Path,
    expected_identity: FileIdentityV3,
    expected_type: ExpectedPathType,
    label: &str,
) -> Result<()> {
    let expected_path_type = match expected_type {
        ExpectedPathType::RegularSingleLink => "regular-single-link",
        ExpectedPathType::Directory => "directory",
    };
    if planned.path_type != expected_path_type
        || planned.path != expected_path
        || planned.path_identity != expected_identity
        || planned.parent_mount_id == 0
        || planned.device_major != linux_device_major(expected_identity.device)
        || planned.device_minor != linux_device_minor(expected_identity.device)
        || !planned.root.is_absolute()
    {
        bail!("{label} durable pre-mount plan is invalid");
    }
    validate_path_identity(expected_path, expected_identity, expected_type, label)?;
    let mounts = read_mountinfo()?;
    if let Some(existing) = exact_mount_for_path(&mounts, expected_path) {
        bail!(
            "{label} acquired exact mount ID {} after planning and before authorized creation",
            existing.mount_id
        );
    }
    let covering = covering_mount_for_path(&mounts, expected_path)
        .with_context(|| format!("{label} lost its planned covering mount"))?;
    let relative = expected_path
        .strip_prefix(&covering.mount_point)
        .with_context(|| format!("{label} left its planned covering mount"))?;
    if covering.mount_id != planned.parent_mount_id
        || normalize_absolute_mount_root(&covering.root, relative)? != planned.root
    {
        bail!("{label} covering mount changed after durable planning");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_path_identity(
    path: &Path,
    expected: FileIdentityV3,
    expected_type: ExpectedPathType,
    label: &str,
) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path).with_context(|| format!("inspect {label}"))?;
    let type_matches = match expected_type {
        ExpectedPathType::RegularSingleLink => metadata.file_type().is_file(),
        ExpectedPathType::Directory => metadata.file_type().is_dir(),
    };
    if !type_matches || metadata.dev() != expected.device || metadata.ino() != expected.inode {
        bail!("{label} type/device/inode differs from its pinned authority");
    }
    if matches!(expected_type, ExpectedPathType::RegularSingleLink) && metadata.nlink() != 1 {
        bail!("{label} must be single-link before and throughout its read-only fence");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn require_host_mount_namespace() -> Result<FileIdentityV3> {
    let current = namespace_identity(Path::new("/proc/self/ns/mnt"))?;
    let host = namespace_identity(Path::new("/proc/1/ns/mnt"))?;
    if current != host {
        bail!("Pool migration controller/worker is not in the host mount namespace");
    }
    Ok(current)
}

#[cfg(target_os = "linux")]
fn current_execution_namespace_authority() -> Result<ExecutionNamespaceAuthorityV3> {
    Ok(ExecutionNamespaceAuthorityV3 {
        user: namespace_identity(Path::new("/proc/self/ns/user"))?,
        pid: namespace_identity(Path::new("/proc/self/ns/pid"))?,
        mount: namespace_identity(Path::new("/proc/self/ns/mnt"))?,
    })
}

#[cfg(target_os = "linux")]
fn require_initial_pid_namespace() -> Result<()> {
    if namespace_identity(Path::new("/proc/self/ns/pid"))?
        != namespace_identity(Path::new("/proc/1/ns/pid"))?
    {
        bail!("Pool migration controller is not in the host PID namespace");
    }
    require_namespace_without_parent(Path::new("/proc/self/ns/pid"), "PID")
}

#[cfg(target_os = "linux")]
fn require_initial_user_namespace() -> Result<()> {
    let current = namespace_identity(Path::new("/proc/self/ns/user"))?;
    let pid_namespace = File::open("/proc/self/ns/pid").context("open current PID namespace")?;
    let owning_user_namespace_fd =
        unsafe { libc::ioctl(pid_namespace.as_raw_fd(), libc::NS_GET_USERNS) };
    if owning_user_namespace_fd < 0 {
        return Err(std::io::Error::last_os_error())
            .context("query the owning user namespace of the host PID namespace");
    }
    let owning_user_namespace = unsafe { File::from_raw_fd(owning_user_namespace_fd) };
    let owning_metadata = owning_user_namespace
        .metadata()
        .context("inspect the owning user namespace of the host PID namespace")?;
    let owning = FileIdentityV3 {
        device: owning_metadata.dev(),
        inode: owning_metadata.ino(),
    };
    if owning.device == 0 || owning.inode == 0 || current != owning {
        bail!("Pool migration controller/worker is not in the host user namespace");
    }
    let uid_map = std::fs::read_to_string("/proc/self/uid_map")
        .context("read Pool migration controller/worker user-namespace UID map")?;
    let gid_map = std::fs::read_to_string("/proc/self/gid_map")
        .context("read Pool migration controller/worker user-namespace GID map")?;
    for (label, map) in [("UID", uid_map), ("GID", gid_map)] {
        let fields = map.split_ascii_whitespace().collect::<Vec<_>>();
        if fields != ["0", "0", "4294967295"] {
            bail!(
                "Pool migration controller/worker is not in the initial full-range {label} namespace"
            );
        }
    }
    require_namespace_without_parent(Path::new("/proc/self/ns/user"), "user")
}

#[cfg(target_os = "linux")]
fn require_effective_capability(bit: u32, name: &str) -> Result<()> {
    let status = std::fs::read_to_string("/proc/self/status")
        .context("read Pool migration controller capability status")?;
    let value = status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:\t"))
        .context("process status has no CapEff field")?;
    let capabilities =
        u64::from_str_radix(value, 16).context("parse effective Linux capability mask")?;
    if capabilities & (1u64 << bit) == 0 {
        bail!("Pool migration controller lacks required host {name}");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn require_namespace_without_parent(path: &Path, label: &str) -> Result<()> {
    let file = File::open(path).with_context(|| format!("open {label} namespace"))?;
    let parent = unsafe { libc::ioctl(file.as_raw_fd(), libc::NS_GET_PARENT) };
    if parent >= 0 {
        drop(unsafe { File::from_raw_fd(parent) });
        bail!("Pool migration controller is in a nested {label} namespace");
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() != Some(libc::EPERM) {
        return Err(error).with_context(|| format!("query {label} namespace parent"));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_host_systemd_runtime_identity() -> Result<()> {
    let local = Path::new("/run/systemd/system");
    let host = Path::new("/proc/1/root/run/systemd/system");
    let local_metadata =
        std::fs::metadata(local).context("inspect local systemd runtime directory")?;
    let host_metadata =
        std::fs::metadata(host).context("inspect host-root systemd runtime directory")?;
    if local_metadata.dev() != host_metadata.dev()
        || local_metadata.ino() != host_metadata.ino()
        || statx_mount_id(local, "local systemd runtime")?
            != statx_mount_id(host, "host-root systemd runtime")?
    {
        bail!("/run/systemd/system is not the exact host PID1 root directory and mount identity");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_local_host_filesystem(path: &Path, label: &str) -> Result<()> {
    let mount_id = statx_mount_id(path, label)?;
    let mounts = read_mountinfo()?;
    let mount = mounts
        .iter()
        .filter(|mount| path.starts_with(&mount.mount_point))
        .max_by_key(|mount| mount.mount_point.components().count())
        .with_context(|| format!("{label} has no covering host mount"))?;
    if mount.mount_id != mount_id {
        bail!("{label} statx mount ID differs from host mountinfo");
    }
    let path_c = c_string(path.as_os_str(), label)?;
    let mut statfs = std::mem::MaybeUninit::<libc::statfs>::zeroed();
    if unsafe { libc::statfs(path_c.as_ptr(), statfs.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("statfs {label}"));
    }
    let filesystem_magic = unsafe { statfs.assume_init() }.f_type as u64;
    const FUSE_SUPER_MAGIC: u64 = 0x6573_5546;
    const OVERLAYFS_SUPER_MAGIC: u64 = 0x794c_7630;
    const NFS_SUPER_MAGIC: u64 = 0x0000_6969;
    const CIFS_SUPER_MAGIC: u64 = 0xff53_4d42;
    const NINEP_SUPER_MAGIC: u64 = 0x0102_1997;
    const CEPH_SUPER_MAGIC: u64 = 0x00c3_64_00;
    const CODA_SUPER_MAGIC: u64 = 0x7375_7245;
    const AFS_SUPER_MAGIC: u64 = 0x5346_414f;
    if matches!(
        filesystem_magic,
        FUSE_SUPER_MAGIC
            | OVERLAYFS_SUPER_MAGIC
            | NFS_SUPER_MAGIC
            | CIFS_SUPER_MAGIC
            | NINEP_SUPER_MAGIC
            | CEPH_SUPER_MAGIC
            | CODA_SUPER_MAGIC
            | AFS_SUPER_MAGIC
    ) || matches!(
        mount.filesystem_type.as_str(),
        "fuse" | "fuseblk" | "overlay" | "nfs" | "nfs4" | "cifs" | "9p" | "ceph" | "afs"
    ) {
        bail!("{label} is on a remote, FUSE, or overlay filesystem");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn statx_mount_id(path: &Path, label: &str) -> Result<u64> {
    const STATX_MNT_ID: u32 = 0x0000_1000;
    let path = c_string(path.as_os_str(), label)?;
    let mut statx = std::mem::MaybeUninit::<LinuxStatx>::zeroed();
    let status = unsafe {
        libc::syscall(
            libc::SYS_statx,
            libc::AT_FDCWD,
            path.as_ptr(),
            libc::AT_NO_AUTOMOUNT | libc::AT_SYMLINK_NOFOLLOW,
            STATX_MNT_ID,
            statx.as_mut_ptr(),
        )
    };
    if status != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("statx {label}"));
    }
    let statx = unsafe { statx.assume_init() };
    if statx.stx_mask & STATX_MNT_ID == 0 || statx.stx_mnt_id == 0 {
        bail!("{label} statx did not return a stable mount ID");
    }
    Ok(statx.stx_mnt_id)
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct LinuxStatxTimestamp {
    seconds: i64,
    nanoseconds: u32,
    reserved: i32,
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct LinuxStatx {
    stx_mask: u32,
    stx_blksize: u32,
    stx_attributes: u64,
    stx_nlink: u32,
    stx_uid: u32,
    stx_gid: u32,
    stx_mode: u16,
    spare0: u16,
    stx_ino: u64,
    stx_size: u64,
    stx_blocks: u64,
    stx_attributes_mask: u64,
    stx_atime: LinuxStatxTimestamp,
    stx_btime: LinuxStatxTimestamp,
    stx_ctime: LinuxStatxTimestamp,
    stx_mtime: LinuxStatxTimestamp,
    stx_rdev_major: u32,
    stx_rdev_minor: u32,
    stx_dev_major: u32,
    stx_dev_minor: u32,
    stx_mnt_id: u64,
    stx_dio_mem_align: u32,
    stx_dio_offset_align: u32,
    spare3: [u64; 12],
}

#[cfg(target_os = "linux")]
fn namespace_identity(path: &Path) -> Result<FileIdentityV3> {
    let metadata =
        std::fs::metadata(path).with_context(|| format!("inspect {}", path.display()))?;
    if metadata.dev() == 0 || metadata.ino() == 0 {
        bail!("mount namespace has an invalid zero device/inode identity");
    }
    Ok(FileIdentityV3 {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(target_os = "linux")]
#[derive(Clone, Debug)]
struct MountInfo {
    mount_id: u64,
    parent_mount_id: u64,
    device_major: u64,
    device_minor: u64,
    root: PathBuf,
    mount_point: PathBuf,
    mount_options: Vec<String>,
    optional_fields: Vec<String>,
    filesystem_type: String,
    mount_source: String,
    super_options: Vec<String>,
}

#[cfg(target_os = "linux")]
fn read_mountinfo() -> Result<Vec<MountInfo>> {
    let text = std::fs::read_to_string("/proc/self/mountinfo")
        .context("read controller mount namespace mountinfo")?;
    text.lines()
        .enumerate()
        .map(|(index, line)| parse_mountinfo_line(line, index + 1))
        .collect()
}

#[cfg(target_os = "linux")]
fn parse_mountinfo_line(line: &str, line_number: usize) -> Result<MountInfo> {
    let (left, right) = line
        .split_once(" - ")
        .with_context(|| format!("mountinfo line {line_number} has no separator"))?;
    let mut left = left.split_ascii_whitespace();
    let mount_id = parse_u64(left.next(), "mount ID", line_number)?;
    let parent_mount_id = parse_u64(left.next(), "parent mount ID", line_number)?;
    let device = left
        .next()
        .with_context(|| format!("mountinfo line {line_number} has no device"))?;
    let (major, minor) = device
        .split_once(':')
        .with_context(|| format!("mountinfo line {line_number} has malformed device"))?;
    let device_major = major
        .parse::<u64>()
        .with_context(|| format!("parse mountinfo line {line_number} device major"))?;
    let device_minor = minor
        .parse::<u64>()
        .with_context(|| format!("parse mountinfo line {line_number} device minor"))?;
    let root = decode_mount_path(
        left.next()
            .with_context(|| format!("mountinfo line {line_number} has no root"))?,
    )?;
    let mount_point = decode_mount_path(
        left.next()
            .with_context(|| format!("mountinfo line {line_number} has no mount point"))?,
    )?;
    let mount_options = split_options(
        left.next()
            .with_context(|| format!("mountinfo line {line_number} has no mount options"))?,
    );
    let optional_fields = left.map(str::to_string).collect::<Vec<_>>();

    let mut right = right.split_ascii_whitespace();
    let filesystem_type = right
        .next()
        .with_context(|| format!("mountinfo line {line_number} has no filesystem type"))?
        .to_string();
    let mount_source = right
        .next()
        .with_context(|| format!("mountinfo line {line_number} has no mount source"))?
        .to_string();
    let super_options = split_options(
        right
            .next()
            .with_context(|| format!("mountinfo line {line_number} has no super options"))?,
    );
    if right.next().is_some() {
        bail!("mountinfo line {line_number} has unexpected trailing fields");
    }
    Ok(MountInfo {
        mount_id,
        parent_mount_id,
        device_major,
        device_minor,
        root,
        mount_point,
        mount_options,
        optional_fields,
        filesystem_type,
        mount_source,
        super_options,
    })
}

#[cfg(target_os = "linux")]
fn parse_u64(value: Option<&str>, label: &str, line: usize) -> Result<u64> {
    value
        .with_context(|| format!("mountinfo line {line} has no {label}"))?
        .parse::<u64>()
        .with_context(|| format!("parse mountinfo line {line} {label}"))
}

#[cfg(target_os = "linux")]
fn decode_mount_path(value: &str) -> Result<PathBuf> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            if index + 3 >= bytes.len()
                || !bytes[index + 1..=index + 3]
                    .iter()
                    .all(|byte| matches!(byte, b'0'..=b'7'))
            {
                bail!("mountinfo path contains malformed escaping");
            }
            let value = (bytes[index + 1] - b'0') * 64
                + (bytes[index + 2] - b'0') * 8
                + (bytes[index + 3] - b'0');
            decoded.push(value);
            index += 4;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    Ok(PathBuf::from(OsStr::from_bytes(&decoded)))
}

#[cfg(target_os = "linux")]
fn split_options(value: &str) -> Vec<String> {
    value.split(',').map(str::to_string).collect()
}

#[cfg(target_os = "linux")]
fn exact_mount_for_path<'a>(mounts: &'a [MountInfo], path: &Path) -> Option<&'a MountInfo> {
    mounts
        .iter()
        .filter(|mount| mount.mount_point == path)
        .max_by_key(|mount| mount.mount_id)
}

#[cfg(target_os = "linux")]
fn covering_mount_for_path<'a>(mounts: &'a [MountInfo], path: &Path) -> Option<&'a MountInfo> {
    mounts
        .iter()
        .filter(|mount| path.starts_with(&mount.mount_point))
        .max_by_key(|mount| mount.mount_point.components().count())
}

#[cfg(target_os = "linux")]
fn normalize_absolute_mount_root(root: &Path, relative: &Path) -> Result<PathBuf> {
    if !root.is_absolute() || relative.is_absolute() {
        bail!("mount root planning requires an absolute root and relative suffix");
    }
    let mut combined = root.to_path_buf();
    for component in relative.components() {
        match component {
            std::path::Component::Normal(value) => combined.push(value),
            std::path::Component::CurDir => {}
            _ => bail!("mount root planning rejected a non-normal relative component"),
        }
    }
    Ok(combined)
}

#[cfg(target_os = "linux")]
fn existing_mount_ids(mounts: &[MountInfo]) -> std::collections::HashSet<u64> {
    mounts.iter().map(|mount| mount.mount_id).collect()
}

#[cfg(target_os = "linux")]
fn mount_is_strictly_read_only(mount: &MountInfo) -> bool {
    ["ro", "nosuid", "nodev", "noexec"]
        .into_iter()
        .all(|option| mount.mount_options.iter().any(|value| value == option))
}

#[cfg(target_os = "linux")]
fn authority_from_mount(
    mount: &MountInfo,
    path: &Path,
    identity: FileIdentityV3,
) -> ReadOnlyBindMountAuthorityV3 {
    ReadOnlyBindMountAuthorityV3 {
        path: path.to_path_buf(),
        path_identity: identity,
        mount_id: mount.mount_id,
        parent_mount_id: mount.parent_mount_id,
        device_major: mount.device_major,
        device_minor: mount.device_minor,
        root: mount.root.clone(),
        mount_options: mount.mount_options.clone(),
        optional_fields: mount.optional_fields.clone(),
        filesystem_type: mount.filesystem_type.clone(),
        mount_source: mount.mount_source.clone(),
        super_options: mount.super_options.clone(),
    }
}

#[cfg(target_os = "linux")]
fn c_string(value: &OsStr, label: &str) -> Result<CString> {
    CString::new(value.as_bytes()).with_context(|| format!("{label} path contains a NUL byte"))
}

#[cfg(target_os = "linux")]
fn linux_device_major(device: u64) -> u64 {
    ((device >> 8) & 0xfff) | ((device >> 32) & !0xfff)
}

#[cfg(target_os = "linux")]
fn linux_device_minor(device: u64) -> u64 {
    (device & 0xff) | ((device >> 12) & !0xff)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::app::pool_migration_teardown::{
        validate_teardown_intent, BoundedFileAuthorityV3, MountTeardownIntentV3,
        MOUNT_TEARDOWN_INTENT_SCHEMA,
    };

    struct ExactMountCleanup(PathBuf);

    impl Drop for ExactMountCleanup {
        fn drop(&mut self) {
            if let Ok(target) = c_string(self.0.as_os_str(), "test mount cleanup") {
                unsafe {
                    libc::umount2(target.as_ptr(), 0);
                }
            }
        }
    }

    #[test]
    fn generated_read_only_self_bind_has_exact_non_lazy_teardown() {
        if unsafe { libc::geteuid() } != 0
            || require_effective_capability(21, "CAP_SYS_ADMIN").is_err()
        {
            eprintln!("skip: generated mount teardown test requires host CAP_SYS_ADMIN");
            return;
        }
        let root = tempfile::tempdir().expect("generated mount teardown root");
        let source = root.path().join("source");
        std::fs::create_dir(&source).expect("generated source directory");
        let data = source.join("data.mdb");
        std::fs::write(&data, b"generated-real-data\n").expect("generated source data");
        let metadata = std::fs::metadata(&data).expect("generated source metadata");
        let source_identity = LmdbIdentityV3 {
            directory: FileIdentityV3 {
                device: std::fs::metadata(&source).expect("source metadata").dev(),
                inode: std::fs::metadata(&source).expect("source metadata").ino(),
            },
            data: FileIdentityV3 {
                device: metadata.dev(),
                inode: metadata.ino(),
            },
            // The mount helper only consumes data.mdb identity. Keep this
            // generated value nonzero so the authority is structurally real.
            lock: FileIdentityV3 {
                device: metadata.dev(),
                inode: metadata.ino(),
            },
        };
        let external = root.path().join("external");
        std::fs::create_dir(&external).expect("generated external directory");
        let external_metadata = std::fs::metadata(&external).expect("generated external metadata");
        let pre_mount_plan =
            plan_source_read_only_mount_authority(&source, source_identity, None, None)
                .expect("durably plan generated source mount");
        let data_c = c_string(data.as_os_str(), "generated partial bind")
            .expect("encode generated partial bind path");
        assert_eq!(
            unsafe {
                libc::mount(
                    data_c.as_ptr(),
                    data_c.as_ptr(),
                    std::ptr::null(),
                    libc::MS_BIND,
                    std::ptr::null(),
                )
            },
            0,
            "simulate controller death after MS_BIND and before RO remount"
        );
        let partial_mounts = read_mountinfo().expect("mountinfo for partial bind");
        let partial = exact_mount_for_path(&partial_mounts, &data).expect("partial RW bind exists");
        assert!(
            !mount_is_strictly_read_only(partial),
            "partial-bind regression did not leave the intended RW crash boundary"
        );
        assert_eq!(
            cleanup_planned_source_mounts(&pre_mount_plan)
                .expect("recover exact partial RW bind")
                .len(),
            1
        );
        assert!(
            exact_mount_for_path(
                &read_mountinfo().expect("mountinfo after partial cleanup"),
                &data
            )
            .is_none(),
            "partial RW bind survived durable-plan cleanup"
        );
        let setup_error = ensure_source_read_only_mount_authority(
            &source,
            source_identity,
            Some(&external),
            Some(FileIdentityV3 {
                device: external_metadata.dev(),
                inode: external_metadata.ino().saturating_add(1),
            }),
        )
        .expect_err("invalid second mount authority must fail before any mount mutation");
        assert!(
            setup_error
                .to_string()
                .contains("differs from its pinned authority"),
            "setup failure did not reject the invalid pre-mount authority: {setup_error:#}"
        );
        assert!(
            exact_mount_for_path(&read_mountinfo().expect("mountinfo after rollback"), &data)
                .is_none(),
            "invalid pre-mount authority mutated the source mount table"
        );
        OpenOptions::new()
            .write(true)
            .open(&data)
            .expect("source data is writable after ordinary setup rollback");
        let authority =
            ensure_source_read_only_mount_authority(&source, source_identity, None, None)
                .expect("establish generated source fence");
        let _cleanup = ExactMountCleanup(data.clone());
        assert!(
            OpenOptions::new().write(true).open(&data).is_err(),
            "generated source fence accepted a write open"
        );
        let adoption_error =
            ensure_source_read_only_mount_authority(&source, source_identity, None, None)
                .expect_err("a later attempt must not adopt a preexisting exact RO mount");
        assert!(
            format!("{adoption_error:#}").contains("refusing to plan adoption"),
            "unexpected adoption error: {adoption_error:#}"
        );
        validate_cached_source_read_only_mount_authorities(&[&authority, &authority])
            .expect("batched cached mount fence accepts one exact deduplicated authority");
        let mut conflicting_authority = authority.clone();
        conflicting_authority.data.path = source.join("conflicting-data.mdb");
        validate_cached_source_read_only_mount_authorities(&[&authority, &conflicting_authority])
            .expect_err("batched cached mount fence rejects a conflicting duplicate mount ID");
        let plan = plan_source_read_only_mount_teardown(&[authority.clone()])
            .expect("build generated controller teardown plan");
        assert!(
            plan[0]
                .mount
                .super_options
                .iter()
                .any(|option| option == "rw"),
            "generated writable backing superblock did not exercise VFS-only read-only intent"
        );
        let nonce = "a".repeat(64);
        let attempt = root.path().join(&nonce);
        std::fs::create_dir(&attempt).expect("generated controller attempt");
        let intent = MountTeardownIntentV3 {
            schema: MOUNT_TEARDOWN_INTENT_SCHEMA.to_string(),
            status: "authorized".to_string(),
            boot_id: "00000000-0000-0000-0000-000000000001".to_string(),
            rollout_id: "generated-controller".to_string(),
            attempt_nonce: nonce,
            launch_request_sha256: "1".repeat(64),
            terminal_audit_path: attempt.join("terminal-audit.json"),
            terminal_audit_sha256: "2".repeat(64),
            terminal_audit_authority: BoundedFileAuthorityV3 {
                path: attempt.join("terminal-audit.json"),
                sha256: "2".repeat(64),
                identity: FileIdentityV3 {
                    device: 10,
                    inode: 11,
                },
                len: 1,
                uid: 65_534,
                gid: 65_534,
                mode: 0o600,
                links: 1,
            },
            non_lazy: true,
            mount_namespace_identity: authority.mount_namespace_identity,
            mounts: plan,
        };
        validate_teardown_intent(&intent, &attempt, "generated-controller")
            .expect("typed controller teardown intent accepts RO VFS over RW superblock");
        let removed = teardown_source_read_only_mount_authorities(&[authority.clone()])
            .expect("exact non-lazy generated teardown");
        assert_eq!(removed, vec![authority.data.clone()]);
        assert!(
            read_mountinfo()
                .expect("mountinfo after teardown")
                .iter()
                .all(|mount| mount.mount_id != authority.data.mount_id),
            "removed mount identity survived exact teardown"
        );
        OpenOptions::new()
            .write(true)
            .open(&data)
            .expect("source is writable after exact teardown");
    }
}
