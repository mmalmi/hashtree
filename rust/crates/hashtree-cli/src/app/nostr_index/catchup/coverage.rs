//! Content-addressed evidence written before the checkpoint that references it.
//! No directory scans, cleanup, or network I/O are needed for append or resume.
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};
use hashtree_cli::HashtreeStore;
use hashtree_nostr::catchup::{
    CatchupSourceCoverage, CatchupSourceMode, CatchupSourceStatus, CatchupState,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const FORMAT: &str = "hashtree/nostr-index-catchup-coverage@1";
const MAX_BYTES: usize = 256 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    format: String,
    previous_head: Option<String>,
    policy_sha256: String,
    pass_since: u64,
    pass_until: u64,
    author: String,
    ordinal: usize,
    before_root: String,
    after_root: String,
    events_received: u64,
    sources: Vec<CatchupSourceCoverage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pubsub: Option<super::pubsub::Receipt>,
}

fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
fn is_sha(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn canonical(value: &serde_json::Value) -> Result<String> {
    Ok(match value {
        serde_json::Value::Object(values) => {
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort();
            let fields = keys
                .into_iter()
                .map(|key| {
                    Ok(format!(
                        "{}:{}",
                        serde_json::to_string(key)?,
                        canonical(&values[key])?
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            format!("{{{}}}", fields.join(","))
        }
        serde_json::Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(canonical)
                .collect::<Result<Vec<_>>>()?
                .join(",")
        ),
        _ => serde_json::to_string(value)?,
    })
}
fn receipt_path(directory: &Path, head: &str) -> Result<PathBuf> {
    ensure!(is_sha(head), "invalid catchup coverage head");
    Ok(directory.join(format!("{head}.json")))
}
fn read(path: &Path) -> Result<Vec<u8>> {
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.len() <= MAX_BYTES as u64,
        "invalid coverage receipt file"
    );
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX_BYTES, "coverage receipt exceeds limit");
    Ok(bytes)
}
fn check_directory(directory: &Path) -> Result<()> {
    match std::fs::symlink_metadata(directory) {
        Ok(metadata) => ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "invalid coverage directory"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

pub(super) fn validate_head(
    directory: &Path,
    state: &CatchupState,
    authors: &[String],
) -> Result<()> {
    let Some(head) = &state.coverage_head else {
        // Legacy relay-only checkpoints have no observation receipt. Peer
        // discovery is runtime state and does not alter relay resume identity.
        return Ok(());
    };
    check_directory(directory)?;
    let bytes = read(&receipt_path(directory, head)?)?;
    ensure!(
        sha(&bytes) == *head,
        "catchup coverage receipt hash mismatch"
    );
    let receipt: Receipt = serde_json::from_slice(&bytes)?;
    ensure!(
        receipt.format == FORMAT
            && receipt.policy_sha256
                == sha(canonical(&serde_json::to_value(&state.policy)?)?.as_bytes())
            && authors.get(receipt.ordinal) == Some(&receipt.author)
            && receipt
                .previous_head
                .as_ref()
                .is_none_or(|value| is_sha(value))
            && receipt.after_root == state.root,
        "coverage receipt does not bind checkpoint root"
    );
    let same_pass = receipt.pass_since == state.pass_since
        && receipt.pass_until == state.pass_until
        && receipt.ordinal.checked_add(1) == Some(state.next_author);
    let rollover = state.next_author == 0
        && receipt.ordinal.checked_add(1) == Some(state.policy.author_count)
        && receipt.pass_until < state.pass_until;
    ensure!(
        same_pass || rollover,
        "coverage receipt does not bind checkpoint frontier"
    );
    validate_sources(state, &receipt.sources)?;
    validate_pubsub(state, receipt.pubsub.as_ref())?;
    Ok(())
}
fn validate_sources(state: &CatchupState, sources: &[CatchupSourceCoverage]) -> Result<()> {
    ensure!(
        sources.len() == state.policy.relays.len()
            && sources
                .iter()
                .zip(&state.policy.relays)
                .all(|(source, relay)| source.relay == *relay)
            && sources
                .iter()
                .any(|source| source.status == CatchupSourceStatus::Complete),
        "coverage needs all sources and one completion"
    );
    if state.policy.source_mode == CatchupSourceMode::Strict {
        ensure!(
            sources
                .iter()
                .all(|source| source.status == CatchupSourceStatus::Complete),
            "strict coverage needs every relay completion"
        );
    }
    ensure!(
        sources.iter().all(|source| match source.status {
            CatchupSourceStatus::Complete => source.error.is_none(),
            CatchupSourceStatus::Failed | CatchupSourceStatus::Skipped => source
                .error
                .as_ref()
                .is_some_and(|error| !error.is_empty() && error.chars().count() <= 256),
        }),
        "invalid source coverage status"
    );
    Ok(())
}

fn persist_with_pubsub(
    store: &HashtreeStore,
    directory: &Path,
    before: &CatchupState,
    after: &CatchupState,
    author: &str,
    sources: Vec<CatchupSourceCoverage>,
    pubsub: Option<super::pubsub::Receipt>,
) -> Result<String> {
    validate_sources(before, &sources)?;
    validate_pubsub(before, pubsub.as_ref())?;
    ensure!(
        after.policy == before.policy
            && after.next_author == before.next_author + 1
            && after.pass_since == before.pass_since
            && after.pass_until == before.pass_until,
        "invalid coverage checkpoint transition"
    );
    let receipt = Receipt {
        format: FORMAT.into(),
        previous_head: before.coverage_head.clone(),
        policy_sha256: sha(canonical(&serde_json::to_value(&before.policy)?)?.as_bytes()),
        pass_since: before.pass_since,
        pass_until: before.pass_until,
        author: author.to_owned(),
        ordinal: before.next_author,
        before_root: before.root.clone(),
        after_root: after.root.clone(),
        events_received: after
            .events_received
            .checked_sub(before.events_received)
            .context("coverage event counter decreased")?,
        sources,
        pubsub,
    };
    let mut bytes = serde_json::to_vec(&receipt)?;
    bytes.push(b'\n');
    ensure!(bytes.len() <= MAX_BYTES, "coverage receipt exceeds limit");
    let head = sha(&bytes);
    let path = receipt_path(directory, &head)?;
    persist_bytes(store, directory, &path, &bytes)?;
    Ok(head)
}
pub(super) fn commit_with_pubsub(
    store: &HashtreeStore,
    directory: &Path,
    checkpoint: &Path,
    before: &CatchupState,
    mut after: CatchupState,
    author: &str,
    sources: Vec<CatchupSourceCoverage>,
    pubsub: Option<super::pubsub::Receipt>,
) -> Result<CatchupState> {
    after.coverage_head = Some(persist_with_pubsub(
        store, directory, before, &after, author, sources, pubsub,
    )?);
    store.admit_checkpoint_write(checkpoint, serde_json::to_vec(&after)?.len() + 1)?;
    super::super::persist_json_atomic(checkpoint, &after, "Nostr catchup checkpoint")?;
    Ok(after)
}
fn validate_pubsub(state: &CatchupState, receipt: Option<&super::pubsub::Receipt>) -> Result<()> {
    if let Some(receipt) = receipt {
        ensure!(
            is_sha(&receipt.event_ids_sha256)
                && receipt.added_events <= receipt.events
                && receipt.events <= state.policy.max_events_per_author
                && receipt.events <= super::pubsub::MAX_REPLAY_EVENTS
                && receipt
                    .sources
                    .iter()
                    .all(|(peer, count)| nostr::PublicKey::parse(peer).is_ok()
                        && *count > 0
                        && *count <= receipt.events)
                && receipt
                    .sources
                    .values()
                    .try_fold(0usize, |sum, count| sum.checked_add(*count))
                    == Some(receipt.events),
            "invalid pubsub observation receipt"
        );
        if receipt.status == super::pubsub::Status::Unavailable {
            ensure!(
                receipt.events == 0 && receipt.event_ids_sha256 == sha(&[]),
                "unavailable pubsub source has observations"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
fn persist(
    store: &HashtreeStore,
    directory: &Path,
    before: &CatchupState,
    after: &CatchupState,
    author: &str,
    sources: Vec<CatchupSourceCoverage>,
) -> Result<String> {
    persist_with_pubsub(store, directory, before, after, author, sources, None)
}

#[cfg(test)]
fn commit(
    store: &HashtreeStore,
    directory: &Path,
    checkpoint: &Path,
    before: &CatchupState,
    after: CatchupState,
    author: &str,
    sources: Vec<CatchupSourceCoverage>,
) -> Result<CatchupState> {
    commit_with_pubsub(
        store, directory, checkpoint, before, after, author, sources, None,
    )
}

fn persist_bytes(store: &HashtreeStore, directory: &Path, path: &Path, bytes: &[u8]) -> Result<()> {
    check_directory(directory)?;
    match std::fs::symlink_metadata(path) {
        Ok(_) => {
            ensure!(read(path)? == bytes, "existing coverage receipt differs");
            // A previous attempt may have synced the receipt but failed before
            // checkpoint replacement. Exact replay requires no new allocation.
            File::open(path)?.sync_all()?;
            File::open(directory)?.sync_all()?;
            File::open(
                directory
                    .parent()
                    .context("coverage directory has no parent")?,
            )?
            .sync_all()?;
            return Ok(());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    store.admit_checkpoint_write(path, bytes.len())?;
    std::fs::create_dir_all(directory)?;
    check_directory(directory)?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist_noclobber(path)
        .map_err(|error| error.error)
        .context("publish immutable catchup coverage")?;
    // Failure is fatal and the checkpoint remains unchanged. Retain an orphan
    // receipt on failure; it can be verified and resynced by an exact retry.
    File::open(directory)?
        .sync_all()
        .context("sync catchup coverage directory")?;
    File::open(
        directory
            .parent()
            .context("coverage directory has no parent")?,
    )?
    .sync_all()
    .context("sync catchup coverage directory parent")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashtree_nostr::catchup::{CatchupPolicy, CatchupSourceMode};
    fn state() -> CatchupState {
        CatchupState {
            version: 2,
            policy: CatchupPolicy {
                base_root: "old-root".into(),
                authors_sha256: "authors".into(),
                author_count: 2,
                initial_since: 10,
                overlap_secs: 10,
                relays: vec!["a".into(), "b".into()],
                source_mode: CatchupSourceMode::BestEffort,
                kinds: vec![1, 5],
                page_size: 4,
                max_pages_per_author: 100,
                max_events_per_author: 100,
                max_bytes_per_author: 1024,
                fetch_timeout_secs: 1,
                index_commit_batch_size: 4,
            },
            root: "old-root".into(),
            pass_since: 10,
            pass_until: 100,
            next_author: 0,
            events_received: 0,
            coverage_head: None,
        }
    }
    fn sources() -> Vec<CatchupSourceCoverage> {
        vec![
            CatchupSourceCoverage {
                relay: "a".into(),
                status: CatchupSourceStatus::Failed,
                error: Some("unavailable".into()),
            },
            CatchupSourceCoverage {
                relay: "b".into(),
                status: CatchupSourceStatus::Complete,
                error: None,
            },
        ]
    }
    fn store(path: &Path) -> HashtreeStore {
        HashtreeStore::with_options_and_backend(
            path,
            None,
            1024 * 1024 * 1024,
            false,
            &hashtree_config::StorageBackend::Fs,
        )
        .unwrap()
    }
    #[test]
    fn legacy_frontier_resumes_and_strict_policy_still_requires_all_relays() {
        let temp = tempfile::tempdir().unwrap();
        let mut current = state();
        current.next_author = 1;
        validate_head(temp.path(), &current, &["a".repeat(64), "b".repeat(64)]).unwrap();
        current.policy.source_mode = CatchupSourceMode::Strict;
        assert!(validate_sources(&current, &sources()).is_err());
    }

    #[test]
    fn checkpoint_write_failure_keeps_exact_previous_state_and_reuses_durable_orphan() {
        let temp = tempfile::tempdir().unwrap();
        let storage = store(&temp.path().join("store"));
        let directory = temp.path().join("catchup-coverage");
        let checkpoint = temp.path().join("catchup-state.json");
        let before = state();
        let mut after = before.clone();
        after.next_author = 1;
        after.root = "new-root".into();
        after.events_received = 2;
        let raw = serde_json::to_vec(&before).unwrap();
        std::fs::write(&checkpoint, &raw).unwrap();
        let blocked = temp.path().join(".catchup-state.json.tmp");
        std::fs::create_dir(&blocked).unwrap();
        assert!(commit(
            &storage,
            &directory,
            &checkpoint,
            &before,
            after.clone(),
            &"a".repeat(64),
            sources()
        )
        .is_err());
        assert_eq!(std::fs::read(&checkpoint).unwrap(), raw);
        assert!(before.coverage_head.is_none());
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
        std::fs::remove_dir(blocked).unwrap();
        let result = commit(
            &storage,
            &directory,
            &checkpoint,
            &before,
            after,
            &"a".repeat(64),
            sources(),
        )
        .unwrap();
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
        validate_head(&directory, &result, &["a".repeat(64), "b".repeat(64)]).unwrap();
        assert_eq!(
            serde_json::from_slice::<CatchupState>(&std::fs::read(checkpoint).unwrap()).unwrap(),
            result
        );
    }
    #[test]
    fn self_consistent_hash_cannot_substitute_wrong_policy_author_or_frontier() {
        let temp = tempfile::tempdir().unwrap();
        let storage = store(&temp.path().join("store"));
        let directory = temp.path().join("coverage");
        let before = state();
        let mut after = before.clone();
        after.next_author = 1;
        let head = persist(
            &storage,
            &directory,
            &before,
            &after,
            &"a".repeat(64),
            sources(),
        )
        .unwrap();
        after.coverage_head = Some(head.clone());
        let original = read(&receipt_path(&directory, &head).unwrap()).unwrap();
        for field in ["policy_sha256", "author", "ordinal", "after_root"] {
            let mut value: serde_json::Value = serde_json::from_slice(&original).unwrap();
            value[field] = if field == "ordinal" {
                serde_json::json!(1)
            } else {
                serde_json::json!("f".repeat(64))
            };
            let bytes = serde_json::to_vec(&value).unwrap();
            let alternate = sha(&bytes);
            std::fs::write(receipt_path(&directory, &alternate).unwrap(), bytes).unwrap();
            after.coverage_head = Some(alternate);
            assert!(
                validate_head(&directory, &after, &["a".repeat(64), "b".repeat(64)]).is_err(),
                "{field}"
            );
        }
    }
    #[test]
    fn coverage_collision_or_no_completed_source_cannot_write_checkpoint() {
        let temp = tempfile::tempdir().unwrap();
        let storage = store(&temp.path().join("store"));
        let directory = temp.path().join("coverage");
        let before = state();
        let mut after = before.clone();
        after.next_author = 1;
        let mut missing = sources();
        missing[1].status = CatchupSourceStatus::Skipped;
        missing[1].error = Some("quarantined".into());
        assert!(persist(
            &storage,
            &directory,
            &before,
            &after,
            &"a".repeat(64),
            missing
        )
        .is_err());
        assert!(!directory.exists());
        let head = persist(
            &storage,
            &directory,
            &before,
            &after,
            &"a".repeat(64),
            sources(),
        )
        .unwrap();
        let path = receipt_path(&directory, &head).unwrap();
        std::fs::write(&path, b"changed").unwrap();
        assert!(persist(
            &storage,
            &directory,
            &before,
            &after,
            &"a".repeat(64),
            sources()
        )
        .is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"changed");
    }
    #[test]
    fn canonical_policy_hash_uses_recursive_sorted_json() {
        let value = serde_json::json!({"z":[3,{"b":2,"a":1}],"a":"value"});
        assert_eq!(
            canonical(&value).unwrap(),
            r#"{"a":"value","z":[3,{"a":1,"b":2}]}"#
        );
    }
}
