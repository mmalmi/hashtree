use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

use super::pool_migration_launch::FileIdentityV3;
use super::pool_migration_pinned::{PinnedDirectory, PinnedStagedFile};

pub(super) const SOURCE_EVIDENCE_FILE_NAME: &str = "source-hash-size.manifest";
const SOURCE_EVIDENCE_MAGIC: &[u8] = b"HTREE-SOURCE-HASH-SIZE-V3\n";
const SOURCE_EVIDENCE_RECORD_BYTES: u64 = 40;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct SourceEvidenceManifestAuthorityV3 {
    pub(super) path: PathBuf,
    pub(super) parent_identity: FileIdentityV3,
    pub(super) identity: FileIdentityV3,
    pub(super) len: u64,
    pub(super) entries: u64,
    pub(super) sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SourceEvidenceSummaryV3 {
    pub(super) entries: u64,
    pub(super) bytes: u64,
    pub(super) content_sha256: [u8; 32],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SourceEvidenceUnionEntryV3 {
    pub(super) hash: [u8; 32],
    pub(super) size: u64,
    pub(super) body_source: usize,
}

pub(super) struct SourceEvidenceManifestWriterV3 {
    staged: PinnedStagedFile,
    final_path: PathBuf,
    parent_identity: FileIdentityV3,
    hasher: Sha256,
    entries: u64,
    previous_hash: Option<[u8; 32]>,
}

impl SourceEvidenceManifestWriterV3 {
    pub(super) fn create(attempt_path: &Path) -> Result<Self> {
        let attempt =
            PinnedDirectory::open_exact(attempt_path, "source evidence attempt directory")?;
        let final_path = attempt.path.join(SOURCE_EVIDENCE_FILE_NAME);
        if attempt.entry_exists(
            OsStr::new(SOURCE_EVIDENCE_FILE_NAME),
            "source evidence manifest",
        )? {
            bail!(
                "source evidence manifest already exists at {}",
                final_path.display()
            );
        }
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let staging_name = OsString::from(format!(
            ".source-hash-size.{}.{}.tmp",
            std::process::id(),
            SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let mut staged =
            attempt.create_staged_regular(&staging_name, 0o600, "source evidence staging file")?;
        let parent_identity = attempt.authority_identity();
        staged
            .file
            .write_all(SOURCE_EVIDENCE_MAGIC)
            .context("write source evidence header")?;
        let mut hasher = Sha256::new();
        hasher.update(SOURCE_EVIDENCE_MAGIC);
        Ok(Self {
            staged,
            final_path,
            parent_identity,
            hasher,
            entries: 0,
            previous_hash: None,
        })
    }

    pub(super) fn append(&mut self, entries: &[([u8; 32], u64)]) -> Result<()> {
        for (hash, size) in entries {
            if self.previous_hash.is_some_and(|previous| previous >= *hash) {
                bail!("source evidence hashes must be globally unique and strictly sorted");
            }
            let mut record = [0u8; SOURCE_EVIDENCE_RECORD_BYTES as usize];
            record[..32].copy_from_slice(hash);
            record[32..].copy_from_slice(&size.to_be_bytes());
            self.staged
                .file
                .write_all(&record)
                .context("append source evidence record")?;
            self.hasher.update(record);
            self.entries = self
                .entries
                .checked_add(1)
                .context("source evidence entry count overflow")?;
            self.previous_hash = Some(*hash);
        }
        Ok(())
    }

    pub(super) fn finish(mut self) -> Result<SourceEvidenceManifestAuthorityV3> {
        self.staged
            .file
            .sync_all()
            .context("fsync source evidence staging file")?;
        #[cfg(unix)]
        if unsafe { libc::fchmod(self.staged.file.as_raw_fd(), 0o440) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("make source evidence manifest read-only");
        }
        self.staged
            .file
            .sync_all()
            .context("fsync source evidence inode after chmod")?;
        let expected_len = evidence_len(self.entries)?;
        let before = self
            .staged
            .file
            .metadata()
            .context("inspect source evidence staging file")?;
        #[cfg(unix)]
        if before.len() != expected_len || before.nlink() != 1 {
            bail!("source evidence staging file has an invalid length/link count");
        }
        #[cfg(not(unix))]
        if before.len() != expected_len {
            bail!("source evidence staging file has an invalid length");
        }
        let identity = self.staged.identity();
        self.staged.publish_noreplace(
            OsStr::new(SOURCE_EVIDENCE_FILE_NAME),
            "source evidence manifest",
        )?;
        let after = self
            .staged
            .file
            .metadata()
            .context("inspect published source evidence manifest")?;
        let open_after = self
            .staged
            .file
            .metadata()
            .context("reinspect open published source evidence manifest")?;
        #[cfg(unix)]
        let original_identity_changed = before.dev() != after.dev()
            || before.ino() != after.ino()
            || before.len() != after.len();
        #[cfg(not(unix))]
        let original_identity_changed = before.len() != after.len();
        if original_identity_changed || !same_metadata(&open_after, &after) {
            bail!("source evidence manifest changed during publication");
        }
        Ok(SourceEvidenceManifestAuthorityV3 {
            path: self.final_path.clone(),
            parent_identity: self.parent_identity,
            identity,
            len: after.len(),
            entries: self.entries,
            sha256: hex::encode(self.hasher.clone().finalize()),
        })
    }
}

pub(super) struct SourceEvidenceManifestReaderV3 {
    parent: PinnedDirectory,
    file: File,
    authority: SourceEvidenceManifestAuthorityV3,
    manifest_hasher: Sha256,
    content_hasher: Sha256,
    entries: u64,
    bytes: u64,
    previous_hash: Option<[u8; 32]>,
    finished: bool,
}

impl SourceEvidenceManifestReaderV3 {
    pub(super) fn open(authority: &SourceEvidenceManifestAuthorityV3) -> Result<Self> {
        let (parent, mut file) = open_source_evidence(authority, None)?;
        let mut magic = vec![0u8; SOURCE_EVIDENCE_MAGIC.len()];
        file.read_exact(&mut magic)
            .context("read source evidence header")?;
        if magic != SOURCE_EVIDENCE_MAGIC {
            bail!("source evidence manifest has an invalid header");
        }
        let mut manifest_hasher = Sha256::new();
        manifest_hasher.update(&magic);
        let mut content_hasher = Sha256::new();
        content_hasher.update(b"hashtree-pool-migration-source-content/v3\0");
        Ok(Self {
            file,
            parent,
            authority: authority.clone(),
            manifest_hasher,
            content_hasher,
            entries: 0,
            bytes: 0,
            previous_hash: None,
            finished: false,
        })
    }

    pub(super) fn next_entry(&mut self) -> Result<Option<([u8; 32], u64)>> {
        if self.finished {
            return Ok(None);
        }
        let mut record = [0u8; SOURCE_EVIDENCE_RECORD_BYTES as usize];
        let mut read = 0usize;
        while read < record.len() {
            match self.file.read(&mut record[read..]) {
                Ok(0) if read == 0 => {
                    self.finish_validation()?;
                    return Ok(None);
                }
                Ok(0) => bail!("source evidence manifest ends in a partial record"),
                Ok(count) => read += count,
                Err(error) => return Err(error).context("read source evidence record"),
            }
        }
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&record[..32]);
        if self.previous_hash.is_some_and(|previous| previous >= hash) {
            bail!("source evidence manifest hashes are not strictly sorted");
        }
        let mut size_bytes = [0u8; 8];
        size_bytes.copy_from_slice(&record[32..]);
        let size = u64::from_be_bytes(size_bytes);
        self.manifest_hasher.update(record);
        self.content_hasher.update(hash);
        self.content_hasher.update(size.to_be_bytes());
        self.entries = self
            .entries
            .checked_add(1)
            .context("source evidence reader count overflow")?;
        self.bytes = self
            .bytes
            .checked_add(size)
            .context("source evidence reader byte count overflow")?;
        self.previous_hash = Some(hash);
        Ok(Some((hash, size)))
    }

    pub(super) fn validated_summary(&self) -> Result<SourceEvidenceSummaryV3> {
        if !self.finished {
            bail!("source evidence manifest summary requested before validated EOF");
        }
        Ok(SourceEvidenceSummaryV3 {
            entries: self.entries,
            bytes: self.bytes,
            content_sha256: self.content_hasher.clone().finalize().into(),
        })
    }

    fn finish_validation(&mut self) -> Result<()> {
        if self.entries != self.authority.entries
            || hex::encode(self.manifest_hasher.clone().finalize()) != self.authority.sha256
        {
            bail!("source evidence manifest count/SHA-256 differs from receipt authority");
        }
        let current = self.parent.open_regular_authority(
            OsStr::new(SOURCE_EVIDENCE_FILE_NAME),
            self.authority.identity,
            "source evidence manifest",
        )?;
        validate_open_source_evidence_metadata(&current, &self.authority, None)?;
        self.finished = true;
        Ok(())
    }
}

pub(super) struct SourceEvidenceUnionReaderV3 {
    readers: Vec<SourceEvidenceManifestReaderV3>,
    current: Vec<Option<([u8; 32], u64)>>,
    hasher: Sha256,
    entries: u64,
    bytes: u64,
    finished: bool,
}

impl SourceEvidenceUnionReaderV3 {
    pub(super) fn open(authorities: &[SourceEvidenceManifestAuthorityV3]) -> Result<Self> {
        if authorities.is_empty() {
            bail!("source evidence union requires at least one manifest");
        }
        let mut readers = authorities
            .iter()
            .map(SourceEvidenceManifestReaderV3::open)
            .collect::<Result<Vec<_>>>()?;
        let current = readers
            .iter_mut()
            .map(SourceEvidenceManifestReaderV3::next_entry)
            .collect::<Result<Vec<_>>>()?;
        let mut hasher = Sha256::new();
        hasher.update(b"hashtree-pool-migration-source-union/v3\0");
        Ok(Self {
            readers,
            current,
            hasher,
            entries: 0,
            bytes: 0,
            finished: false,
        })
    }

    pub(super) fn next_entry(&mut self) -> Result<Option<SourceEvidenceUnionEntryV3>> {
        if self.finished {
            return Ok(None);
        }
        let Some(next_hash) = self
            .current
            .iter()
            .filter_map(|entry| entry.map(|(hash, _)| hash))
            .min()
        else {
            self.finished = true;
            return Ok(None);
        };
        let matching = self
            .current
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                entry
                    .filter(|(hash, _)| *hash == next_hash)
                    .map(|(_, size)| (index, size))
            })
            .collect::<Vec<_>>();
        let expected_size = matching[0].1;
        if matching.iter().any(|(_, size)| *size != expected_size) {
            bail!(
                "source evidence manifests disagree on the size of {}",
                hashtree_core::to_hex(&next_hash)
            );
        }
        let body_source = matching[0].0;
        for (source_index, _) in matching {
            self.current[source_index] = self.readers[source_index].next_entry()?;
        }
        self.hasher.update(next_hash);
        self.hasher.update(expected_size.to_be_bytes());
        self.entries = self
            .entries
            .checked_add(1)
            .context("source evidence union entry count overflow")?;
        self.bytes = self
            .bytes
            .checked_add(expected_size)
            .context("source evidence union byte count overflow")?;
        Ok(Some(SourceEvidenceUnionEntryV3 {
            hash: next_hash,
            size: expected_size,
            body_source,
        }))
    }

    pub(super) fn validated_source_summaries(&self) -> Result<Vec<SourceEvidenceSummaryV3>> {
        if !self.finished {
            bail!("source evidence union summaries requested before validated EOF");
        }
        self.readers
            .iter()
            .map(SourceEvidenceManifestReaderV3::validated_summary)
            .collect()
    }

    pub(super) fn validated_union_summary(&self) -> Result<SourceEvidenceSummaryV3> {
        if !self.finished {
            bail!("source evidence union summary requested before validated EOF");
        }
        Ok(SourceEvidenceSummaryV3 {
            entries: self.entries,
            bytes: self.bytes,
            content_sha256: self.hasher.clone().finalize().into(),
        })
    }
}

pub(super) fn validate_source_evidence_metadata(
    authority: &SourceEvidenceManifestAuthorityV3,
    expected_service_gid: Option<u32>,
    hash_content: bool,
) -> Result<()> {
    let (_, mut file) = open_source_evidence(authority, expected_service_gid)?;
    if hash_content
        && hash_open_regular_file(&mut file, authority.len, "source evidence manifest")?
            != authority.sha256
    {
        bail!("source evidence manifest bytes differ from receipt authority");
    }
    Ok(())
}

fn open_source_evidence(
    authority: &SourceEvidenceManifestAuthorityV3,
    expected_service_gid: Option<u32>,
) -> Result<(PinnedDirectory, File)> {
    if authority.path.file_name().and_then(|name| name.to_str()) != Some(SOURCE_EVIDENCE_FILE_NAME)
    {
        bail!("source evidence authority has an unexpected leaf name");
    }
    let parent_path = authority
        .path
        .parent()
        .context("source evidence authority has no parent")?;
    let parent = PinnedDirectory::open_exact(parent_path, "source evidence parent")?;
    parent.require_authority_identity(
        authority.parent_identity,
        "source evidence parent authority",
    )?;
    let file = parent.open_regular_authority(
        OsStr::new(SOURCE_EVIDENCE_FILE_NAME),
        authority.identity,
        "source evidence manifest",
    )?;
    validate_open_source_evidence_metadata(&file, authority, expected_service_gid)?;
    Ok((parent, file))
}

fn validate_open_source_evidence_metadata(
    file: &File,
    authority: &SourceEvidenceManifestAuthorityV3,
    expected_service_gid: Option<u32>,
) -> Result<()> {
    let expected_len = evidence_len(authority.entries)?;
    if authority.len != expected_len {
        bail!("source evidence manifest length does not match its entry count");
    }
    let metadata = file
        .metadata()
        .context("inspect source evidence manifest authority")?;
    #[cfg(unix)]
    if !metadata.file_type().is_file()
        || metadata.len() != authority.len
        || metadata.nlink() != 1
        || metadata.mode() & 0o7777 != 0o440
        || metadata.dev() != authority.identity.device
        || metadata.ino() != authority.identity.inode
        || expected_service_gid.is_some_and(|gid| metadata.gid() != gid)
    {
        bail!("source evidence manifest metadata differs from receipt authority");
    }
    #[cfg(not(unix))]
    if !metadata.file_type().is_file() || metadata.len() != authority.len {
        let _ = expected_service_gid;
        bail!("source evidence manifest metadata differs from receipt authority");
    }
    Ok(())
}

fn evidence_len(entries: u64) -> Result<u64> {
    (SOURCE_EVIDENCE_MAGIC.len() as u64)
        .checked_add(
            entries
                .checked_mul(SOURCE_EVIDENCE_RECORD_BYTES)
                .context("source evidence authority length overflow")?,
        )
        .context("source evidence authority length overflow")
}

fn hash_open_regular_file(file: &mut File, expected_len: u64, label: &str) -> Result<String> {
    let before = file
        .metadata()
        .with_context(|| format!("inspect open {label}"))?;
    if before.len() != expected_len {
        bail!("{label} length changed before hashing");
    }
    let mut hasher = Sha256::new();
    std::io::copy(&mut *file, &mut hasher).with_context(|| format!("hash {label}"))?;
    let after = file
        .metadata()
        .with_context(|| format!("reinspect open {label}"))?;
    if !same_metadata(&before, &after) {
        bail!("{label} changed while hashing");
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(unix)]
fn file_identity(metadata: &std::fs::Metadata) -> FileIdentityV3 {
    FileIdentityV3 {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

#[cfg(not(unix))]
fn file_identity(_metadata: &std::fs::Metadata) -> FileIdentityV3 {
    FileIdentityV3 {
        device: 0,
        inode: 0,
    }
}

fn same_metadata(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        left.dev() == right.dev()
            && left.ino() == right.ino()
            && left.mode() == right.mode()
            && left.nlink() == right.nlink()
            && left.len() == right.len()
            && left.mtime() == right.mtime()
            && left.mtime_nsec() == right.mtime_nsec()
            && left.ctime() == right.ctime()
            && left.ctime_nsec() == right.ctime_nsec()
    }
    #[cfg(not(unix))]
    {
        left.len() == right.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn publish(directory: &Path, records: &[([u8; 32], u64)]) -> SourceEvidenceManifestAuthorityV3 {
        std::fs::create_dir(directory).expect("create generated evidence directory");
        let canonical = directory
            .canonicalize()
            .expect("canonicalize generated evidence directory");
        let mut writer =
            SourceEvidenceManifestWriterV3::create(&canonical).expect("create evidence writer");
        writer.append(records).expect("append generated evidence");
        writer.finish().expect("publish generated evidence")
    }

    #[test]
    fn k_way_union_deduplicates_equal_hash_sizes() {
        let temp = TempDir::new().expect("temp dir");
        let first_hash = [1u8; 32];
        let second_hash = [2u8; 32];
        let third_hash = [3u8; 32];
        let first = publish(
            &temp.path().join("first"),
            &[(first_hash, 11), (third_hash, 33)],
        );
        let second = publish(
            &temp.path().join("second"),
            &[(first_hash, 11), (second_hash, 22)],
        );
        let mut union = SourceEvidenceUnionReaderV3::open(&[first, second])
            .expect("open generated evidence union");
        let mut records = Vec::new();
        while let Some(record) = union.next_entry().expect("stream evidence union") {
            records.push((record.hash, record.size, record.body_source));
        }
        assert_eq!(
            records,
            vec![
                (first_hash, 11, 0),
                (second_hash, 22, 1),
                (third_hash, 33, 0)
            ]
        );
        assert_eq!(union.validated_source_summaries().unwrap().len(), 2);
        let summary = union.validated_union_summary().unwrap();
        assert_eq!(summary.entries, 3);
        assert_eq!(summary.bytes, 66);
    }

    #[test]
    fn k_way_union_rejects_equal_hash_size_disagreement() {
        let temp = TempDir::new().expect("temp dir");
        let hash = [7u8; 32];
        let first = publish(&temp.path().join("first"), &[(hash, 11)]);
        let second = publish(&temp.path().join("second"), &[(hash, 12)]);
        let mut union = SourceEvidenceUnionReaderV3::open(&[first, second])
            .expect("open generated evidence union");
        assert!(union.next_entry().is_err());
    }

    #[test]
    fn reader_rejects_swapped_parent_even_when_file_inode_is_relinked() {
        let temp = TempDir::new().expect("temp dir");
        let directory = temp.path().join("attempt");
        let authority = publish(&directory, &[([9u8; 32], 99)]);
        let original_parent = authority.path.parent().unwrap().to_path_buf();
        let moved_parent = original_parent.with_file_name("moved-attempt");
        std::fs::rename(&original_parent, &moved_parent).expect("move authority parent");
        std::fs::create_dir(&original_parent).expect("replace authority parent path");
        std::fs::hard_link(
            moved_parent.join(SOURCE_EVIDENCE_FILE_NAME),
            original_parent.join(SOURCE_EVIDENCE_FILE_NAME),
        )
        .expect("relink exact evidence inode beneath swapped parent");
        let error = SourceEvidenceManifestReaderV3::open(&authority)
            .err()
            .expect("swapped parent must be rejected");
        assert!(error.to_string().contains("parent authority"));
    }
}
