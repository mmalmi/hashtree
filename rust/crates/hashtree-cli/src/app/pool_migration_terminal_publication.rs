use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

use super::pool_migration_launch::{CursorAuthorityV3, FileAuthorityV3, FileIdentityV3};
use super::pool_migration_mount_lifecycle::{
    recover_full_mount_lifecycle_closed, recover_source_mounts_retained,
    validated_terminal_completion_authority, PreparedMountLifecycleV3, MOUNT_LIFECYCLE_INTENT_FILE,
};
use super::pool_migration_pinned::PinnedDirectory;
use super::pool_migration_teardown::{
    durable_create_root_receipt, parse_strict, read_bounded_file_authority,
    read_exact_root_receipt, require_boot_id, require_safe_component, serialize_json_line,
    sha256_bytes, BoundedFileAuthorityV3,
};

pub(super) const TERMINAL_PUBLICATION_INTENT_FILE: &str = "terminal-publication-intent.json";
pub(super) const TERMINAL_PUBLICATION_READY_FILE: &str = "terminal-publication-ready.json";
pub(super) const TERMINAL_PUBLICATION_RECEIPT_FILE: &str = "terminal-publication.json";
pub(super) const SOURCE_TERMINAL_CERTIFICATION_FILE: &str = "source-terminal-certification.json";
const TERMINAL_PUBLICATION_INTENT_SCHEMA: &str =
    "hashtree-pool-migration-terminal-publication-intent/v3";
const TERMINAL_PUBLICATION_READY_SCHEMA: &str =
    "hashtree-pool-migration-terminal-publication-ready/v3";
const TERMINAL_PUBLICATION_RECEIPT_SCHEMA: &str = "hashtree-pool-migration-terminal-publication/v3";
const SOURCE_TERMINAL_CERTIFICATION_SCHEMA: &str =
    "hashtree-pool-migration-source-terminal-certification/v3";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct TerminalPublicationIntentV3 {
    pub(super) schema: String,
    pub(super) status: String,
    pub(super) boot_id: String,
    pub(super) rollout_id: String,
    pub(super) attempt_nonce: String,
    pub(super) phase: String,
    pub(super) launch_request_sha256: String,
    pub(super) cursor: CursorAuthorityV3,
    pub(super) cursor_value: String,
    pub(super) cursor_gid: u32,
    pub(super) terminal_authority: BoundedFileAuthorityV3,
    pub(super) mount_lifecycle_intent: FileAuthorityV3,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct TerminalPublicationReadyV3 {
    pub(super) schema: String,
    pub(super) status: String,
    pub(super) boot_id: String,
    pub(super) rollout_id: String,
    pub(super) attempt_nonce: String,
    pub(super) intent: FileAuthorityV3,
    pub(super) lifecycle_completion: FileAuthorityV3,
    pub(super) source_certification: Option<FileAuthorityV3>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct SourceTerminalCertificationV3 {
    pub(super) schema: String,
    pub(super) status: String,
    pub(super) boot_id: String,
    pub(super) rollout_id: String,
    pub(super) attempt_nonce: String,
    pub(super) source_terminal: BoundedFileAuthorityV3,
    pub(super) mount_retention: FileAuthorityV3,
    pub(super) terminal_publication_intent: FileAuthorityV3,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct PublishedCursorAuthorityV3 {
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
pub(super) struct TerminalPublicationReceiptV3 {
    pub(super) schema: String,
    pub(super) status: String,
    pub(super) boot_id: String,
    pub(super) rollout_id: String,
    pub(super) attempt_nonce: String,
    pub(super) intent: FileAuthorityV3,
    pub(super) ready: FileAuthorityV3,
    pub(super) cursor: PublishedCursorAuthorityV3,
}

#[derive(Clone)]
pub(super) struct PreparedTerminalPublicationV3 {
    pub(super) intent: TerminalPublicationIntentV3,
    pub(super) authority: FileAuthorityV3,
}

pub(super) struct CompletedTerminalPublicationV3 {
    pub(super) receipt: FileAuthorityV3,
    pub(super) source_certification: Option<FileAuthorityV3>,
}

pub(super) fn create_terminal_publication_intent(
    attempt_dir: &Path,
    boot_id: &str,
    rollout_id: &str,
    attempt_nonce: &str,
    launch_request_sha256: &str,
    lifecycle: &PreparedMountLifecycleV3,
    cursor: CursorAuthorityV3,
    cursor_gid: u32,
    terminal_authority: BoundedFileAuthorityV3,
) -> Result<PreparedTerminalPublicationV3> {
    let cursor_value = match lifecycle.intent.phase.as_str() {
        "final-stopped-source" => "source-complete",
        "final-stopped-full" => "complete",
        _ => bail!("terminal publication requires a stopped mount lifecycle"),
    };
    let intent = TerminalPublicationIntentV3 {
        schema: TERMINAL_PUBLICATION_INTENT_SCHEMA.to_string(),
        status: "authorized".to_string(),
        boot_id: boot_id.to_string(),
        rollout_id: rollout_id.to_string(),
        attempt_nonce: attempt_nonce.to_string(),
        phase: lifecycle.intent.phase.clone(),
        launch_request_sha256: launch_request_sha256.to_string(),
        cursor,
        cursor_value: cursor_value.to_string(),
        cursor_gid,
        terminal_authority,
        mount_lifecycle_intent: FileAuthorityV3 {
            path: lifecycle.intent_path.clone(),
            sha256: lifecycle.intent_sha256.clone(),
        },
    };
    validate_intent(&intent, attempt_dir, rollout_id)?;
    let bytes = serialize_json_line(&intent, "terminal publication intent")?;
    let path = attempt_dir.join(TERMINAL_PUBLICATION_INTENT_FILE);
    durable_create_root_receipt(&path, &bytes, attempt_nonce)?;
    Ok(PreparedTerminalPublicationV3 {
        intent,
        authority: FileAuthorityV3 {
            path,
            sha256: sha256_bytes(&bytes),
        },
    })
}

pub(super) fn complete_terminal_publication(
    attempt_dir: &Path,
    publication: &PreparedTerminalPublicationV3,
    lifecycle_completion: FileAuthorityV3,
    current_boot_id: &str,
) -> Result<CompletedTerminalPublicationV3> {
    complete_terminal_publication_authorized(
        attempt_dir,
        publication,
        lifecycle_completion,
        current_boot_id,
        || Ok(()),
    )
}

pub(super) fn complete_terminal_publication_authorized<F>(
    attempt_dir: &Path,
    publication: &PreparedTerminalPublicationV3,
    lifecycle_completion: FileAuthorityV3,
    current_boot_id: &str,
    authorize_cursor_publication: F,
) -> Result<CompletedTerminalPublicationV3>
where
    F: FnOnce() -> Result<()>,
{
    authorize_cursor_publication()
        .context("root rejected terminal cursor publication after lifecycle completion")?;
    let ready = publish_ready(
        attempt_dir,
        publication,
        lifecycle_completion,
        current_boot_id,
    )?;
    let cursor = publish_or_validate_cursor(&publication.intent)?;
    let receipt = publish_receipt(attempt_dir, publication, ready, cursor)?;
    let source_certification = if publication.intent.phase == "final-stopped-source" {
        let path = attempt_dir.join(SOURCE_TERMINAL_CERTIFICATION_FILE);
        let bytes = read_exact_service_file(
            &path,
            publication.intent.cursor_gid,
            "source terminal certification",
        )?;
        Some(FileAuthorityV3 {
            path,
            sha256: sha256_bytes(&bytes),
        })
    } else {
        None
    };
    Ok(CompletedTerminalPublicationV3 {
        receipt,
        source_certification,
    })
}

pub(super) fn recover_rollout_terminal_publications<F>(
    attempts_dir: &Path,
    rollout_id: &str,
    current_boot_id: &str,
    mut authorize_cursor_publication: F,
) -> Result<usize>
where
    F: FnMut(&Path, &PreparedTerminalPublicationV3) -> Result<()>,
{
    require_boot_id(current_boot_id)?;
    let entries = match std::fs::read_dir(attempts_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error).context("scan terminal publication attempts"),
    };
    let mut attempts = Vec::new();
    for entry in entries {
        let entry = entry.context("read terminal publication attempt")?;
        if entry
            .file_type()
            .context("inspect terminal publication attempt type")?
            .is_dir()
            && entry.path().join(TERMINAL_PUBLICATION_INTENT_FILE).exists()
        {
            attempts.push(entry.path());
        }
    }
    attempts.sort();
    let unfinished = attempts
        .iter()
        .filter(|attempt| !attempt.join(TERMINAL_PUBLICATION_RECEIPT_FILE).exists())
        .count();
    if unfinished > 1 {
        bail!(
            "rollout has {unfinished} unfinished terminal publications; refusing ambiguous recovery"
        );
    }
    let mut recovered = 0usize;
    for attempt_dir in &attempts {
        if recover_attempt(
            attempt_dir,
            rollout_id,
            current_boot_id,
            &mut authorize_cursor_publication,
        )
        .with_context(|| {
            format!(
                "recover terminal publication attempt {}",
                attempt_dir.display()
            )
        })? {
            recovered = recovered
                .checked_add(1)
                .context("terminal publication recovery count overflow")?;
        }
    }
    let remaining = attempts
        .iter()
        .filter(|attempt| {
            attempt.join(TERMINAL_PUBLICATION_INTENT_FILE).exists()
                && !attempt.join(TERMINAL_PUBLICATION_RECEIPT_FILE).exists()
        })
        .count();
    if remaining != 0 {
        bail!(
            "rollout retains {remaining} unfinished terminal publication after recovery; refusing a new attempt"
        );
    }
    validate_live_cursor_heads(&attempts, rollout_id)?;
    Ok(recovered)
}

fn recover_attempt<F>(
    attempt_dir: &Path,
    rollout_id: &str,
    current_boot_id: &str,
    authorize_cursor_publication: &mut F,
) -> Result<bool>
where
    F: FnMut(&Path, &PreparedTerminalPublicationV3) -> Result<()>,
{
    let publication = load_intent(attempt_dir, rollout_id)?;
    let receipt_path = attempt_dir.join(TERMINAL_PUBLICATION_RECEIPT_FILE);
    if receipt_path.exists() {
        load_receipt(attempt_dir, &publication)?;
        return Ok(false);
    }

    if publication.intent.boot_id != current_boot_id {
        bail!(
            "refusing cross-boot terminal publication recovery for {}",
            publication.intent.phase
        );
    }

    let lifecycle_completion = match load_ready_optional(attempt_dir, &publication)? {
        Some((ready, _)) => {
            validate_file_authority(&ready.lifecycle_completion, "terminal lifecycle completion")?;
            ready.lifecycle_completion
        }
        None => {
            let mut authority = validated_terminal_completion_authority(
                attempt_dir,
                rollout_id,
                current_boot_id,
                &publication.intent.phase,
            )?;
            if authority.is_none() && publication.intent.phase == "final-stopped-source" {
                let recovered = recover_source_mounts_retained(
                    attempt_dir,
                    rollout_id,
                    current_boot_id,
                    publication.intent.terminal_authority.clone(),
                )?;
                let validated = validated_terminal_completion_authority(
                    attempt_dir,
                    rollout_id,
                    current_boot_id,
                    &publication.intent.phase,
                )?
                .context("recovered source mount retention is not a terminal authority")?;
                if validated != recovered {
                    bail!("recovered source mount retention changed during validation");
                }
                authority = Some(validated);
            }
            if authority.is_none() && publication.intent.phase == "final-stopped-full" {
                authorize_cursor_publication(attempt_dir, &publication)
                    .context("root rejected recovery before reconstructed full-final teardown")?;
                let recovered = recover_full_mount_lifecycle_closed(
                    attempt_dir,
                    rollout_id,
                    current_boot_id,
                    &publication.intent.launch_request_sha256,
                    publication.intent.terminal_authority.clone(),
                )?;
                let validated = validated_terminal_completion_authority(
                    attempt_dir,
                    rollout_id,
                    current_boot_id,
                    &publication.intent.phase,
                )?
                .context("recovered full mount teardown is not a terminal authority")?;
                if validated != recovered {
                    bail!("recovered full mount lifecycle changed during validation");
                }
                authority = Some(validated);
            }
            let Some(authority) = authority else {
                return Ok(false);
            };
            authority
        }
    };
    complete_terminal_publication_authorized(
        attempt_dir,
        &publication,
        lifecycle_completion,
        current_boot_id,
        || authorize_cursor_publication(attempt_dir, &publication),
    )?;
    Ok(true)
}

fn validate_live_cursor_heads(attempts: &[PathBuf], rollout_id: &str) -> Result<()> {
    let mut heads = HashMap::<
        PathBuf,
        (
            u8,
            PreparedTerminalPublicationV3,
            TerminalPublicationReceiptV3,
        ),
    >::new();
    for attempt_dir in attempts {
        if !attempt_dir.join(TERMINAL_PUBLICATION_RECEIPT_FILE).exists() {
            continue;
        }
        let publication = load_intent(attempt_dir, rollout_id)?;
        let receipt = load_receipt(attempt_dir, &publication)?;
        let rank = match publication.intent.phase.as_str() {
            "final-stopped-source" => 1,
            "final-stopped-full" => 2,
            _ => bail!("completed terminal publication has an unsupported phase"),
        };
        match heads.entry(publication.intent.cursor.path.clone()) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert((rank, publication, receipt));
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                if rank > entry.get().0 {
                    entry.insert((rank, publication, receipt));
                } else if rank == entry.get().0 {
                    bail!(
                        "multiple completed terminal publications ambiguously own the same cursor phase"
                    );
                }
            }
        }
    }
    for (_, publication, receipt) in heads.into_values() {
        let current = validate_published_cursor(&publication.intent)?;
        if receipt.cursor != current {
            bail!("latest terminal publication cursor changed after its durable receipt");
        }
    }
    Ok(())
}

fn publish_ready(
    attempt_dir: &Path,
    publication: &PreparedTerminalPublicationV3,
    lifecycle_completion: FileAuthorityV3,
    current_boot_id: &str,
) -> Result<FileAuthorityV3> {
    let expected = validated_terminal_completion_authority(
        attempt_dir,
        &publication.intent.rollout_id,
        current_boot_id,
        &publication.intent.phase,
    )?
    .context("terminal publication lifecycle is not durably complete")?;
    if lifecycle_completion != expected {
        bail!("terminal publication does not bind the exact lifecycle completion authority");
    }
    let source_certification = if publication.intent.phase == "final-stopped-source" {
        Some(publish_source_certification(
            attempt_dir,
            publication,
            lifecycle_completion.clone(),
        )?)
    } else {
        None
    };
    let ready = TerminalPublicationReadyV3 {
        schema: TERMINAL_PUBLICATION_READY_SCHEMA.to_string(),
        status: "ready".to_string(),
        boot_id: publication.intent.boot_id.clone(),
        rollout_id: publication.intent.rollout_id.clone(),
        attempt_nonce: publication.intent.attempt_nonce.clone(),
        intent: publication.authority.clone(),
        lifecycle_completion,
        source_certification,
    };
    validate_ready(&ready, publication)?;
    let bytes = serialize_json_line(&ready, "terminal publication ready receipt")?;
    let path = attempt_dir.join(TERMINAL_PUBLICATION_READY_FILE);
    match read_exact_root_receipt(&path, "terminal publication ready receipt") {
        Ok(existing) => {
            if existing != bytes {
                bail!("terminal publication ready receipt already exists with different bytes");
            }
        }
        Err(error) if root_io_kind(&error) == Some(ErrorKind::NotFound) => {
            durable_create_root_receipt(&path, &bytes, &publication.intent.attempt_nonce)?;
        }
        Err(error) => return Err(error),
    }
    Ok(FileAuthorityV3 {
        path,
        sha256: sha256_bytes(&bytes),
    })
}

fn publish_source_certification(
    attempt_dir: &Path,
    publication: &PreparedTerminalPublicationV3,
    mount_retention: FileAuthorityV3,
) -> Result<FileAuthorityV3> {
    let certification = SourceTerminalCertificationV3 {
        schema: SOURCE_TERMINAL_CERTIFICATION_SCHEMA.to_string(),
        status: "certified".to_string(),
        boot_id: publication.intent.boot_id.clone(),
        rollout_id: publication.intent.rollout_id.clone(),
        attempt_nonce: publication.intent.attempt_nonce.clone(),
        source_terminal: publication.intent.terminal_authority.clone(),
        mount_retention,
        terminal_publication_intent: publication.authority.clone(),
    };
    validate_source_certification(&certification, attempt_dir, publication)?;
    let bytes = serialize_json_line(&certification, "source terminal certification")?;
    let path = attempt_dir.join(SOURCE_TERMINAL_CERTIFICATION_FILE);
    publish_or_validate_service_file(
        &path,
        &bytes,
        publication.intent.cursor_gid,
        &publication.intent.attempt_nonce,
        "source terminal certification",
    )?;
    Ok(FileAuthorityV3 {
        path,
        sha256: sha256_bytes(&bytes),
    })
}

pub(super) fn read_validated_source_terminal_certification(
    authority: &FileAuthorityV3,
    expected_boot_id: &str,
    expected_service_gid: u32,
) -> Result<(BoundedFileAuthorityV3, Vec<u8>)> {
    require_boot_id(expected_boot_id)?;
    let bytes = read_exact_service_file(
        &authority.path,
        expected_service_gid,
        "source terminal certification",
    )?;
    if sha256_bytes(&bytes) != authority.sha256 {
        bail!("source terminal certification differs from its CAS authority");
    }
    let certification: SourceTerminalCertificationV3 =
        parse_strict(&bytes, "source terminal certification")?;
    let attempt_dir = authority
        .path
        .parent()
        .context("source terminal certification has no attempt directory")?;
    let publication = load_intent(attempt_dir, &certification.rollout_id)?;
    validate_source_certification(&certification, attempt_dir, &publication)?;
    if certification.boot_id != expected_boot_id {
        bail!("source terminal certification belongs to a different boot");
    }
    let expected_retention = validated_terminal_completion_authority(
        attempt_dir,
        &certification.rollout_id,
        expected_boot_id,
        "final-stopped-source",
    )?
    .context("source terminal certification has no retained mount lifecycle")?;
    if certification.mount_retention != expected_retention {
        bail!("source terminal certification retention authority changed");
    }
    let source_bytes = read_bounded_file_authority(
        &certification.source_terminal,
        "certified source terminal evidence",
    )?;
    Ok((certification.source_terminal, source_bytes))
}

fn validate_source_certification(
    certification: &SourceTerminalCertificationV3,
    attempt_dir: &Path,
    publication: &PreparedTerminalPublicationV3,
) -> Result<()> {
    if certification.schema != SOURCE_TERMINAL_CERTIFICATION_SCHEMA
        || certification.status != "certified"
        || certification.boot_id != publication.intent.boot_id
        || certification.rollout_id != publication.intent.rollout_id
        || certification.attempt_nonce != publication.intent.attempt_nonce
        || certification.source_terminal != publication.intent.terminal_authority
        || certification.terminal_publication_intent != publication.authority
        || certification.source_terminal.path != attempt_dir.join("source-terminal.json")
        || certification.mount_retention.path != attempt_dir.join("mount-lifecycle-retained.json")
        || publication.intent.phase != "final-stopped-source"
        || attempt_dir.file_name().and_then(|name| name.to_str())
            != Some(certification.attempt_nonce.as_str())
    {
        bail!("source terminal certification breaks its exact authority chain");
    }
    validate_file_authority(
        &certification.mount_retention,
        "source mount retention authority",
    )?;
    read_bounded_file_authority(
        &certification.source_terminal,
        "source terminal certification evidence",
    )?;
    Ok(())
}

fn publish_receipt(
    attempt_dir: &Path,
    publication: &PreparedTerminalPublicationV3,
    ready: FileAuthorityV3,
    cursor: PublishedCursorAuthorityV3,
) -> Result<FileAuthorityV3> {
    let receipt = TerminalPublicationReceiptV3 {
        schema: TERMINAL_PUBLICATION_RECEIPT_SCHEMA.to_string(),
        status: "published".to_string(),
        boot_id: publication.intent.boot_id.clone(),
        rollout_id: publication.intent.rollout_id.clone(),
        attempt_nonce: publication.intent.attempt_nonce.clone(),
        intent: publication.authority.clone(),
        ready,
        cursor,
    };
    validate_receipt(&receipt, publication)?;
    let bytes = serialize_json_line(&receipt, "terminal publication receipt")?;
    let path = attempt_dir.join(TERMINAL_PUBLICATION_RECEIPT_FILE);
    match read_exact_root_receipt(&path, "terminal publication receipt") {
        Ok(existing) => {
            if existing != bytes {
                bail!("terminal publication receipt already exists with different bytes");
            }
        }
        Err(error) if root_io_kind(&error) == Some(ErrorKind::NotFound) => {
            durable_create_root_receipt(&path, &bytes, &publication.intent.attempt_nonce)?;
        }
        Err(error) => return Err(error),
    }
    Ok(FileAuthorityV3 {
        path,
        sha256: sha256_bytes(&bytes),
    })
}

fn publish_or_validate_cursor(
    intent: &TerminalPublicationIntentV3,
) -> Result<PublishedCursorAuthorityV3> {
    match validate_published_cursor(intent) {
        Ok(authority) => return Ok(authority),
        Err(error) if root_io_kind(&error) != Some(ErrorKind::NotFound) => return Err(error),
        Err(_) => {}
    }
    let parent_path = intent
        .cursor
        .path
        .parent()
        .context("terminal cursor has no parent directory")?;
    let parent = PinnedDirectory::open_exact(parent_path, "terminal cursor parent")?;
    parent.require_authority_identity(intent.cursor.parent_identity, "terminal cursor parent")?;
    parent.acquire_exclusive_migration_lease()?;
    let target = intent
        .cursor
        .path
        .file_name()
        .context("terminal cursor has no leaf name")?;
    if parent.entry_exists(target, "terminal cursor")? {
        return validate_published_cursor(intent);
    }
    let temporary = format!(
        ".terminal-cursor.{}.{}.tmp",
        std::process::id(),
        &intent.attempt_nonce[..16]
    );
    let mut staged =
        parent.create_staged_regular(temporary.as_ref(), 0o600, "terminal cursor staging")?;
    let bytes = format!("{}\n", intent.cursor_value).into_bytes();
    staged
        .file
        .write_all(&bytes)
        .context("write terminal cursor staging file")?;
    #[cfg(target_os = "linux")]
    {
        if unsafe { libc::fchown(staged.file.as_raw_fd(), 0, intent.cursor_gid) } != 0 {
            return Err(std::io::Error::last_os_error()).context("set terminal cursor ownership");
        }
        if unsafe { libc::fchmod(staged.file.as_raw_fd(), 0o440) } != 0 {
            return Err(std::io::Error::last_os_error()).context("set terminal cursor mode");
        }
    }
    staged
        .file
        .sync_all()
        .context("fsync terminal cursor staging file")?;
    staged.publish_noreplace(target, "terminal cursor")?;
    parent.sync("terminal cursor parent")?;
    validate_published_cursor(intent)
}

fn publish_or_validate_service_file(
    path: &Path,
    bytes: &[u8],
    service_gid: u32,
    nonce: &str,
    label: &str,
) -> Result<()> {
    match read_exact_service_file(path, service_gid, label) {
        Ok(existing) => {
            if existing != bytes {
                bail!("{label} already exists with different bytes");
            }
            return Ok(());
        }
        Err(error) if root_io_kind(&error) != Some(ErrorKind::NotFound) => return Err(error),
        Err(_) => {}
    }
    let parent_path = path
        .parent()
        .context("service file has no parent directory")?;
    let parent = PinnedDirectory::open_exact(parent_path, "service file parent")?;
    let target = path.file_name().context("service file has no leaf name")?;
    if parent.entry_exists(target, label)? {
        bail!("{label} appeared before exclusive publication");
    }
    let temporary = format!(
        ".service-authority.{}.{}.tmp",
        std::process::id(),
        &nonce[..16]
    );
    let mut staged = parent.create_staged_regular(temporary.as_ref(), 0o600, label)?;
    staged
        .file
        .write_all(bytes)
        .with_context(|| format!("write staged {label}"))?;
    #[cfg(target_os = "linux")]
    {
        if unsafe { libc::fchown(staged.file.as_raw_fd(), 0, service_gid) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("set staged {label} ownership"));
        }
        if unsafe { libc::fchmod(staged.file.as_raw_fd(), 0o440) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("set staged {label} mode"));
        }
    }
    staged
        .file
        .sync_all()
        .with_context(|| format!("fsync staged {label}"))?;
    staged.publish_noreplace(target, label)?;
    parent.sync("service file parent")?;
    let published = read_exact_service_file(path, service_gid, label)?;
    if published != bytes {
        bail!("published {label} differs from intended bytes");
    }
    Ok(())
}

fn read_exact_service_file(path: &Path, service_gid: u32, label: &str) -> Result<Vec<u8>> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("open {label} {}", path.display()))?;
    let before = file
        .metadata()
        .with_context(|| format!("inspect open {label}"))?;
    let named =
        std::fs::symlink_metadata(path).with_context(|| format!("inspect named {label}"))?;
    #[cfg(unix)]
    if !before.file_type().is_file()
        || before.dev() != named.dev()
        || before.ino() != named.ino()
        || before.uid() != 0
        || before.gid() != service_gid
        || before.mode() & 0o7777 != 0o440
        || before.nlink() != 1
        || before.len() == 0
        || before.len() > 4 * 1024 * 1024
    {
        bail!("{label} is not an exact bounded root:service mode 0440 single-link file");
    }
    let mut bytes = Vec::with_capacity(before.len() as usize);
    file.read_to_end(&mut bytes)
        .with_context(|| format!("read {label}"))?;
    let after = file
        .metadata()
        .with_context(|| format!("reinspect open {label}"))?;
    #[cfg(unix)]
    if before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
        || !bytes.ends_with(b"\n")
    {
        bail!("{label} changed while read or lacks its terminal newline");
    }
    Ok(bytes)
}

fn validate_published_cursor(
    intent: &TerminalPublicationIntentV3,
) -> Result<PublishedCursorAuthorityV3> {
    let parent_path = intent
        .cursor
        .path
        .parent()
        .context("terminal cursor has no parent directory")?;
    let parent = PinnedDirectory::open_exact(parent_path, "terminal cursor parent")?;
    parent.require_authority_identity(intent.cursor.parent_identity, "terminal cursor parent")?;
    let name = intent
        .cursor
        .path
        .file_name()
        .context("terminal cursor has no leaf name")?;
    let mut file = parent
        .open_regular_optional(name, "terminal cursor")?
        .ok_or_else(|| std::io::Error::from(ErrorKind::NotFound))?;
    let before = file.metadata().context("inspect terminal cursor")?;
    let expected = format!("{}\n", intent.cursor_value).into_bytes();
    let mut bytes = Vec::with_capacity(expected.len());
    Read::by_ref(&mut file)
        .take(1025)
        .read_to_end(&mut bytes)
        .context("read terminal cursor")?;
    let after = file.metadata().context("reinspect terminal cursor")?;
    if bytes != expected || before.len() != after.len() || before.len() != bytes.len() as u64 {
        bail!("terminal cursor bytes or length differ from publication intent");
    }
    #[cfg(unix)]
    {
        if before.dev() != after.dev()
            || before.ino() != after.ino()
            || before.uid() != 0
            || before.gid() != intent.cursor_gid
            || before.mode() & 0o7777 != 0o440
            || before.nlink() != 1
        {
            bail!("terminal cursor inode/ownership/mode authority is invalid");
        }
        Ok(PublishedCursorAuthorityV3 {
            path: intent.cursor.path.clone(),
            sha256: sha256_bytes(&bytes),
            identity: FileIdentityV3 {
                device: before.dev(),
                inode: before.ino(),
            },
            len: before.len(),
            uid: before.uid(),
            gid: before.gid(),
            mode: before.mode() & 0o7777,
            links: before.nlink(),
        })
    }
    #[cfg(not(unix))]
    {
        bail!("terminal publication is supported only on Unix")
    }
}

fn load_intent(attempt_dir: &Path, rollout_id: &str) -> Result<PreparedTerminalPublicationV3> {
    let path = attempt_dir.join(TERMINAL_PUBLICATION_INTENT_FILE);
    let bytes = read_exact_root_receipt(&path, "terminal publication intent")?;
    let intent: TerminalPublicationIntentV3 = parse_strict(&bytes, "terminal publication intent")?;
    validate_intent(&intent, attempt_dir, rollout_id)?;
    Ok(PreparedTerminalPublicationV3 {
        intent,
        authority: FileAuthorityV3 {
            path,
            sha256: sha256_bytes(&bytes),
        },
    })
}

fn load_ready_optional(
    attempt_dir: &Path,
    publication: &PreparedTerminalPublicationV3,
) -> Result<Option<(TerminalPublicationReadyV3, FileAuthorityV3)>> {
    let path = attempt_dir.join(TERMINAL_PUBLICATION_READY_FILE);
    let bytes = match read_exact_root_receipt(&path, "terminal publication ready receipt") {
        Ok(bytes) => bytes,
        Err(error) if root_io_kind(&error) == Some(ErrorKind::NotFound) => return Ok(None),
        Err(error) => return Err(error),
    };
    let ready: TerminalPublicationReadyV3 =
        parse_strict(&bytes, "terminal publication ready receipt")?;
    validate_ready(&ready, publication)?;
    let authority = FileAuthorityV3 {
        path,
        sha256: sha256_bytes(&bytes),
    };
    Ok(Some((ready, authority)))
}

fn load_receipt(
    attempt_dir: &Path,
    publication: &PreparedTerminalPublicationV3,
) -> Result<TerminalPublicationReceiptV3> {
    let path = attempt_dir.join(TERMINAL_PUBLICATION_RECEIPT_FILE);
    let bytes = read_exact_root_receipt(&path, "terminal publication receipt")?;
    let receipt: TerminalPublicationReceiptV3 =
        parse_strict(&bytes, "terminal publication receipt")?;
    validate_receipt(&receipt, publication)?;
    let (_, ready_authority) = load_ready_optional(attempt_dir, publication)?
        .context("published terminal receipt has no validated readiness receipt")?;
    if receipt.ready != ready_authority {
        bail!("terminal publication receipt binds a different readiness authority");
    }
    Ok(receipt)
}

fn validate_intent(
    intent: &TerminalPublicationIntentV3,
    attempt_dir: &Path,
    rollout_id: &str,
) -> Result<()> {
    require_boot_id(&intent.boot_id)?;
    require_safe_component("terminal publication rollout ID", &intent.rollout_id, 128)?;
    require_safe_component(
        "terminal publication attempt nonce",
        &intent.attempt_nonce,
        128,
    )?;
    let expected_value = match intent.phase.as_str() {
        "final-stopped-source" => "source-complete",
        "final-stopped-full" => "complete",
        _ => bail!("terminal publication intent has an unsupported phase"),
    };
    if intent.schema != TERMINAL_PUBLICATION_INTENT_SCHEMA
        || intent.status != "authorized"
        || intent.rollout_id != rollout_id
        || intent.cursor_value != expected_value
        || intent.cursor_gid == 0
        || intent.cursor.exists
        || intent.cursor.value.is_some()
        || intent.cursor.sha256.is_some()
        || intent.cursor.parent_identity.device == 0
        || intent.cursor.parent_identity.inode == 0
        || intent.launch_request_sha256.len() != 64
        || !intent
            .launch_request_sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        || attempt_dir.file_name().and_then(|name| name.to_str())
            != Some(intent.attempt_nonce.as_str())
        || intent.mount_lifecycle_intent.path != attempt_dir.join(MOUNT_LIFECYCLE_INTENT_FILE)
    {
        bail!("terminal publication intent has invalid authority fields");
    }
    validate_file_authority(
        &intent.mount_lifecycle_intent,
        "terminal mount lifecycle intent",
    )?;
    read_bounded_file_authority(&intent.terminal_authority, "terminal publication evidence")?;
    Ok(())
}

fn validate_ready(
    ready: &TerminalPublicationReadyV3,
    publication: &PreparedTerminalPublicationV3,
) -> Result<()> {
    if ready.schema != TERMINAL_PUBLICATION_READY_SCHEMA
        || ready.status != "ready"
        || ready.boot_id != publication.intent.boot_id
        || ready.rollout_id != publication.intent.rollout_id
        || ready.attempt_nonce != publication.intent.attempt_nonce
        || ready.intent != publication.authority
    {
        bail!("terminal publication ready receipt breaks its exact intent chain");
    }
    validate_file_authority(&ready.lifecycle_completion, "terminal lifecycle completion")?;
    match (
        publication.intent.phase.as_str(),
        ready.source_certification.as_ref(),
    ) {
        ("final-stopped-source", Some(authority)) => {
            let bytes = read_exact_service_file(
                &authority.path,
                publication.intent.cursor_gid,
                "source terminal certification",
            )?;
            if sha256_bytes(&bytes) != authority.sha256 {
                bail!("source terminal certification changed after terminal readiness");
            }
            let certification: SourceTerminalCertificationV3 =
                parse_strict(&bytes, "source terminal certification")?;
            let attempt_dir = publication
                .authority
                .path
                .parent()
                .context("terminal publication intent has no attempt directory")?;
            validate_source_certification(&certification, attempt_dir, publication)?;
            if certification.mount_retention != ready.lifecycle_completion {
                bail!("source terminal certification binds a different mount retention");
            }
        }
        ("final-stopped-full", None) => {}
        _ => bail!("terminal publication ready certification is inconsistent with its phase"),
    }
    Ok(())
}

fn validate_receipt(
    receipt: &TerminalPublicationReceiptV3,
    publication: &PreparedTerminalPublicationV3,
) -> Result<()> {
    if receipt.schema != TERMINAL_PUBLICATION_RECEIPT_SCHEMA
        || receipt.status != "published"
        || receipt.boot_id != publication.intent.boot_id
        || receipt.rollout_id != publication.intent.rollout_id
        || receipt.attempt_nonce != publication.intent.attempt_nonce
        || receipt.intent != publication.authority
        || receipt.ready.path
            != publication
                .authority
                .path
                .parent()
                .context("terminal publication intent has no attempt directory")?
                .join(TERMINAL_PUBLICATION_READY_FILE)
        || receipt.cursor.path != publication.intent.cursor.path
        || receipt.cursor.sha256
            != sha256_bytes(format!("{}\n", publication.intent.cursor_value).as_bytes())
        || receipt.cursor.identity.device == 0
        || receipt.cursor.identity.inode == 0
        || receipt.cursor.len != publication.intent.cursor_value.len() as u64 + 1
        || receipt.cursor.uid != 0
        || receipt.cursor.gid != publication.intent.cursor_gid
        || receipt.cursor.mode != 0o440
        || receipt.cursor.links != 1
    {
        bail!("terminal publication receipt breaks its exact authority chain");
    }
    validate_file_authority(&receipt.ready, "terminal publication ready authority")?;
    Ok(())
}

fn validate_file_authority(authority: &FileAuthorityV3, label: &str) -> Result<()> {
    let bytes = read_exact_root_receipt(&authority.path, label)?;
    if sha256_bytes(&bytes) != authority.sha256 {
        bail!("{label} differs from its SHA-256 authority");
    }
    Ok(())
}

fn root_io_kind(error: &anyhow::Error) -> Option<ErrorKind> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<std::io::Error>())
        .map(std::io::Error::kind)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::app::pool_migration_launch::LmdbIdentityV3;
    use crate::app::pool_migration_mount::{
        cleanup_planned_source_mounts, ensure_source_read_only_mount_authority_from_plan,
        plan_source_read_only_mount_authority,
    };
    use crate::app::pool_migration_mount_lifecycle::{
        create_full_mount_lifecycle, create_source_mount_lifecycle, record_source_mounts_created,
        record_source_mounts_retained, recover_rollout_mount_lifecycle_state,
    };
    use crate::app::pool_migration_teardown::capture_bounded_worker_file_authority;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    struct ExactMountCleanup(PathBuf);

    impl Drop for ExactMountCleanup {
        fn drop(&mut self) {
            if let Ok(path) = CString::new(self.0.as_os_str().as_bytes()) {
                unsafe {
                    libc::umount2(path.as_ptr(), 0);
                }
            }
        }
    }

    fn identity(path: &Path) -> FileIdentityV3 {
        let metadata = std::fs::metadata(path).expect("inspect generated authority path");
        FileIdentityV3 {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }

    fn current_boot_id() -> String {
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .expect("read generated test boot ID")
            .trim()
            .to_string()
    }

    fn has_mount_authority() -> bool {
        if unsafe { libc::geteuid() } != 0 {
            return false;
        }
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        let Some(value) = status
            .lines()
            .find_map(|line| line.strip_prefix("CapEff:\t"))
        else {
            return false;
        };
        u64::from_str_radix(value, 16).is_ok_and(|capabilities| capabilities & (1u64 << 21) != 0)
    }

    #[test]
    fn generated_recovery_finishes_every_terminal_cursor_crash_boundary() {
        if !has_mount_authority() {
            eprintln!("skip: terminal publication recovery test requires host CAP_SYS_ADMIN");
            return;
        }
        for stage in 0..4 {
            let temp = tempfile::tempdir().expect("create generated rollout");
            let rollout = temp.path().join("generated-rollout");
            let attempts = rollout.join("attempts-v3");
            let nonce = format!("{:x}", stage + 10).repeat(64);
            let attempt = attempts.join(&nonce);
            let cursor_parent = temp.path().join(format!("cursor-parent-{stage}"));
            std::fs::create_dir_all(&attempt).expect("create generated attempt");
            std::fs::create_dir(&cursor_parent).expect("create generated cursor parent");

            let source = temp.path().join(format!("source-{stage}"));
            std::fs::create_dir(&source).expect("create generated source");
            let data = source.join("data.mdb");
            let lock = source.join("lock.mdb");
            std::fs::write(&data, b"generated terminal publication source\n")
                .expect("write generated data");
            std::fs::write(&lock, b"generated lock\n").expect("write generated lock");
            let source_identity = LmdbIdentityV3 {
                directory: identity(&source),
                data: identity(&data),
                lock: identity(&lock),
            };
            let plan = plan_source_read_only_mount_authority(&source, source_identity, None, None)
                .expect("plan generated source mount");
            let lifecycle = create_source_mount_lifecycle(
                &attempt,
                &current_boot_id(),
                "generated-rollout",
                &nonce,
                &"1".repeat(64),
                &source,
                source_identity,
                None,
                None,
                plan.clone(),
            )
            .expect("publish generated mount lifecycle");
            let mounts = ensure_source_read_only_mount_authority_from_plan(
                &plan,
                &source,
                source_identity,
                None,
                None,
            )
            .expect("mount generated source read-only");
            let _cleanup = ExactMountCleanup(data.clone());
            let mounted =
                record_source_mounts_created(&attempt, &lifecycle, mounts).expect("record mounts");

            let terminal_path = attempt.join("source-terminal.json");
            std::fs::write(
                &terminal_path,
                b"{\"schema\":\"generated-source-terminal\"}\n",
            )
            .expect("write generated terminal evidence");
            std::fs::set_permissions(&terminal_path, std::fs::Permissions::from_mode(0o640))
                .expect("protect generated terminal evidence");
            let terminal_path_c =
                CString::new(terminal_path.as_os_str().as_bytes()).expect("encode terminal path");
            assert_eq!(
                unsafe { libc::chown(terminal_path_c.as_ptr(), 65_534, 65_534) },
                0,
                "chown generated terminal evidence"
            );
            let terminal = capture_bounded_worker_file_authority(
                &terminal_path,
                65_534,
                65_534,
                0o640,
                "generated source terminal",
            )
            .expect("capture generated terminal authority")
            .0;
            let cursor = CursorAuthorityV3 {
                path: cursor_parent.join(format!("cursor-{stage}")),
                parent_identity: identity(&cursor_parent),
                exists: false,
                value: None,
                sha256: None,
            };
            let publication = create_terminal_publication_intent(
                &attempt,
                &current_boot_id(),
                "generated-rollout",
                &nonce,
                &"2".repeat(64),
                &lifecycle,
                cursor,
                65_534,
                terminal.clone(),
            )
            .expect("publish generated terminal intent");
            let completion = if stage >= 1 {
                Some(
                    record_source_mounts_retained(&attempt, &lifecycle, &mounted, terminal.clone())
                        .expect("retain generated source mounts"),
                )
            } else {
                None
            };
            if stage >= 2 {
                publish_ready(
                    &attempt,
                    &publication,
                    completion.clone().expect("prepared source retention"),
                    &current_boot_id(),
                )
                .expect("simulate crash after terminal ready receipt");
            }
            if stage >= 3 {
                publish_or_validate_cursor(&publication.intent)
                    .expect("simulate crash after terminal cursor");
            }

            let cross_boot_error = recover_rollout_terminal_publications(
                &attempts,
                "generated-rollout",
                "00000000-0000-0000-0000-000000000001",
                |_, _| Ok(()),
            )
            .expect_err("cross-boot terminal recovery must be refused");
            assert!(
                format!("{cross_boot_error:#}").contains("cross-boot"),
                "unexpected cross-boot recovery error: {cross_boot_error:#}"
            );
            recover_rollout_mount_lifecycle_state(
                &attempts,
                "generated-rollout",
                &current_boot_id(),
            )
            .expect("same-boot lifecycle recovery preserves terminal-ready source mounts");
            if stage < 3 {
                let denied = recover_rollout_terminal_publications(
                    &attempts,
                    "generated-rollout",
                    &current_boot_id(),
                    |_, _| bail!("generated writer mask disappeared"),
                )
                .expect_err("missing writer mask must block cursor recovery");
                assert!(
                    format!("{denied:#}").contains("writer mask disappeared"),
                    "unexpected authorization error: {denied:#}"
                );
                assert!(!publication.intent.cursor.path.exists());

                let mutated = recover_rollout_terminal_publications(
                    &attempts,
                    "generated-rollout",
                    &current_boot_id(),
                    |_, _| bail!("generated target generation changed"),
                )
                .expect_err("target mutation must block cursor recovery");
                assert!(
                    format!("{mutated:#}").contains("target generation changed"),
                    "unexpected mutation error: {mutated:#}"
                );
                assert!(!publication.intent.cursor.path.exists());
            }

            recover_rollout_terminal_publications(
                &attempts,
                "generated-rollout",
                &current_boot_id(),
                |_, _| Ok(()),
            )
            .expect("recover generated terminal publication");
            let receipt = load_receipt(&attempt, &publication)
                .expect("validate recovered terminal publication receipt");
            assert_eq!(receipt.cursor.path, publication.intent.cursor.path);
            let certification_path = attempt.join(SOURCE_TERMINAL_CERTIFICATION_FILE);
            let certification_bytes = read_exact_service_file(
                &certification_path,
                65_534,
                "generated source terminal certification",
            )
            .expect("read generated source certification");
            let certification_authority = FileAuthorityV3 {
                path: certification_path.clone(),
                sha256: sha256_bytes(&certification_bytes),
            };
            let (certified_terminal, certified_bytes) =
                read_validated_source_terminal_certification(
                    &certification_authority,
                    &current_boot_id(),
                    65_534,
                )
                .expect("validate generated root certification handoff");
            assert_eq!(certified_terminal.path, terminal_path);
            assert_eq!(
                certified_bytes,
                b"{\"schema\":\"generated-source-terminal\"}\n"
            );
            recover_rollout_terminal_publications(
                &attempts,
                "generated-rollout",
                &current_boot_id(),
                |_, _| Ok(()),
            )
            .expect("terminal publication recovery is idempotent");
            if stage == 3 {
                std::fs::set_permissions(
                    &certification_path,
                    std::fs::Permissions::from_mode(0o640),
                )
                .expect("tamper generated certification mode");
                assert!(
                    read_validated_source_terminal_certification(
                        &certification_authority,
                        &current_boot_id(),
                        65_534,
                    )
                    .is_err(),
                    "certification mode tampering was accepted"
                );
            }
            cleanup_planned_source_mounts(&plan).expect("clean generated retained source mount");
        }
    }

    #[test]
    fn generated_full_recovery_reconstructs_inter_intent_teardown() {
        if !has_mount_authority() {
            eprintln!("skip: full terminal reconstruction test requires host CAP_SYS_ADMIN");
            return;
        }
        let temp = tempfile::tempdir().expect("create generated full rollout");
        let rollout = temp.path().join("generated-full-rollout");
        let attempts = rollout.join("attempts-v3");
        let source_nonce = "e".repeat(64);
        let source_attempt = attempts.join(&source_nonce);
        let nonce = "f".repeat(64);
        let attempt = attempts.join(&nonce);
        let cursor_parent = temp.path().join("full-cursor-parent");
        std::fs::create_dir_all(&source_attempt).expect("create generated source attempt");
        std::fs::create_dir(&attempt).expect("create generated full attempt");
        std::fs::create_dir(&cursor_parent).expect("create generated full cursor parent");

        let source = temp.path().join("full-source");
        std::fs::create_dir(&source).expect("create generated full source");
        let data = source.join("data.mdb");
        let lock = source.join("lock.mdb");
        std::fs::write(&data, b"generated full terminal source\n")
            .expect("write generated full data");
        std::fs::write(&lock, b"generated full lock\n").expect("write generated full lock");
        let source_identity = LmdbIdentityV3 {
            directory: identity(&source),
            data: identity(&data),
            lock: identity(&lock),
        };
        let plan = plan_source_read_only_mount_authority(&source, source_identity, None, None)
            .expect("plan generated full source mount");
        let mounts = ensure_source_read_only_mount_authority_from_plan(
            &plan,
            &source,
            source_identity,
            None,
            None,
        )
        .expect("mount generated full source read-only");
        let _cleanup = ExactMountCleanup(data);
        let boot_id = current_boot_id();
        let source_lifecycle = create_source_mount_lifecycle(
            &source_attempt,
            &boot_id,
            "generated-full-rollout",
            &source_nonce,
            &"0".repeat(64),
            &source,
            source_identity,
            None,
            None,
            plan.clone(),
        )
        .expect("publish generated source lifecycle");
        let source_mounted =
            record_source_mounts_created(&source_attempt, &source_lifecycle, mounts.clone())
                .expect("record generated source mounts");
        let source_terminal_path = source_attempt.join("source-terminal.json");
        std::fs::write(
            &source_terminal_path,
            b"{\"schema\":\"generated-source-before-full\"}\n",
        )
        .expect("write generated source terminal evidence");
        std::fs::set_permissions(
            &source_terminal_path,
            std::fs::Permissions::from_mode(0o640),
        )
        .expect("protect generated source terminal evidence");
        let source_terminal_path_c = CString::new(source_terminal_path.as_os_str().as_bytes())
            .expect("encode source terminal path");
        assert_eq!(
            unsafe { libc::chown(source_terminal_path_c.as_ptr(), 65_534, 65_534) },
            0,
            "chown generated source terminal evidence"
        );
        let source_terminal = capture_bounded_worker_file_authority(
            &source_terminal_path,
            65_534,
            65_534,
            0o640,
            "generated source terminal before full",
        )
        .expect("capture generated source terminal authority")
        .0;
        let shared_cursor_path = cursor_parent.join("shared-cursor");
        let source_publication = create_terminal_publication_intent(
            &source_attempt,
            &boot_id,
            "generated-full-rollout",
            &source_nonce,
            &"1".repeat(64),
            &source_lifecycle,
            CursorAuthorityV3 {
                path: shared_cursor_path.clone(),
                parent_identity: identity(&cursor_parent),
                exists: false,
                value: None,
                sha256: None,
            },
            65_534,
            source_terminal.clone(),
        )
        .expect("publish generated source terminal intent");
        let source_retention = record_source_mounts_retained(
            &source_attempt,
            &source_lifecycle,
            &source_mounted,
            source_terminal,
        )
        .expect("retain generated source mounts before full");
        complete_terminal_publication(
            &source_attempt,
            &source_publication,
            source_retention,
            &boot_id,
        )
        .expect("publish generated source cursor before full");
        std::fs::remove_file(&shared_cursor_path)
            .expect("simulate controlled phase handoff to a fresh full cursor");

        let lifecycle = create_full_mount_lifecycle(
            &attempt,
            &boot_id,
            "generated-full-rollout",
            &nonce,
            &"1".repeat(64),
            vec![mounts],
        )
        .expect("publish generated full lifecycle");

        let terminal_path = attempt.join("terminal-audit.json");
        std::fs::write(
            &terminal_path,
            b"{\"schema\":\"generated-full-terminal\"}\n",
        )
        .expect("write generated full terminal evidence");
        std::fs::set_permissions(&terminal_path, std::fs::Permissions::from_mode(0o600))
            .expect("protect generated full terminal evidence");
        let terminal_path_c =
            CString::new(terminal_path.as_os_str().as_bytes()).expect("encode terminal path");
        assert_eq!(
            unsafe { libc::chown(terminal_path_c.as_ptr(), 65_534, 65_534) },
            0,
            "chown generated full terminal evidence"
        );
        let terminal = capture_bounded_worker_file_authority(
            &terminal_path,
            65_534,
            65_534,
            0o600,
            "generated full terminal",
        )
        .expect("capture generated full terminal authority")
        .0;
        let cursor = CursorAuthorityV3 {
            path: shared_cursor_path,
            parent_identity: identity(&cursor_parent),
            exists: false,
            value: None,
            sha256: None,
        };
        let publication = create_terminal_publication_intent(
            &attempt,
            &boot_id,
            "generated-full-rollout",
            &nonce,
            &"2".repeat(64),
            &lifecycle,
            cursor,
            65_534,
            terminal,
        )
        .expect("publish generated full terminal intent");

        recover_rollout_mount_lifecycle_state(&attempts, "generated-full-rollout", &boot_id)
            .expect("inter-intent recovery preserves full source mounts");
        assert!(
            !attempt.join("mount-lifecycle-closed.json").exists(),
            "inter-intent lifecycle closed before authorized teardown reconstruction"
        );

        let authorizations = std::cell::Cell::new(0usize);
        let recovered = recover_rollout_terminal_publications(
            &attempts,
            "generated-full-rollout",
            &boot_id,
            |_, _| {
                authorizations.set(authorizations.get() + 1);
                Ok(())
            },
        )
        .expect("recover full terminal publication from the inter-intent boundary");
        assert_eq!(recovered, 1);
        assert_eq!(
            authorizations.get(),
            2,
            "full recovery must authorize before teardown and immediately before cursor"
        );
        assert!(attempt.join("mount-teardown-intent.json").exists());
        assert!(attempt.join("mount-teardown.json").exists());
        assert!(attempt.join("mount-lifecycle-closed.json").exists());
        assert!(attempt.join(TERMINAL_PUBLICATION_RECEIPT_FILE).exists());
        assert!(publication.intent.cursor.path.exists());

        std::fs::remove_file(attempt.join(TERMINAL_PUBLICATION_RECEIPT_FILE))
            .expect("simulate full cursor publication before its terminal receipt");
        authorizations.set(0);
        assert_eq!(
            recover_rollout_terminal_publications(
                &attempts,
                "generated-full-rollout",
                &boot_id,
                |_, _| {
                    authorizations.set(authorizations.get() + 1);
                    Ok(())
                },
            )
            .expect("recover full cursor-before-receipt after completed source publication"),
            1
        );
        assert_eq!(
            authorizations.get(),
            1,
            "cursor-before-receipt recovery must reauthorize exactly once"
        );

        assert_eq!(
            recover_rollout_terminal_publications(
                &attempts,
                "generated-full-rollout",
                &boot_id,
                |_, _| bail!("completed publication must not reauthorize"),
            )
            .expect("full terminal recovery is idempotent"),
            0
        );
        cleanup_planned_source_mounts(&plan).expect("full reconstructed teardown is complete");
    }
}
