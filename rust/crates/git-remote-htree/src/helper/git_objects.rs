use crate::git::object::{GitObject, ObjectId, ObjectType};
use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;
use tracing::debug;

use super::{fetch_progress_interval, RemoteHelper};

impl RemoteHelper {
    /// A failed pack is optional only when every requested history is complete
    /// and valid. Tip presence alone misses absent parents, trees, and blobs.
    pub(super) fn verify_requested_git_object_closure(&self) -> Result<()> {
        let mut shas: Vec<_> = self
            .fetch_specs
            .iter()
            .map(|spec| spec.sha.as_str())
            .collect();
        shas.sort_unstable();
        shas.dedup();
        if shas.is_empty() || shas.iter().any(|sha| ObjectId::from_hex(sha).is_none()) {
            bail!("Cannot verify pack recovery without valid requested object IDs");
        }
        let git = || {
            let mut command = Command::new("git");
            command
                .arg("--no-replace-objects")
                .env("GIT_NO_LAZY_FETCH", "1")
                .env("GIT_GRAFT_FILE", "");
            command
        };
        let shallow = git()
            .args(["rev-parse", "--is-shallow-repository"])
            .output()?;
        if !shallow.status.success() || String::from_utf8_lossy(&shallow.stdout).trim() != "false" {
            bail!("Cannot verify complete pack recovery in a shallow or unreadable repository");
        }
        // fsck accepts promised omissions, while traversal alone does not check
        // object hashes. Require both, without replacement/graft shortcuts.
        for args in [
            vec!["rev-list", "--quiet", "--objects", "--missing=error"],
            vec![
                "fsck",
                "--full",
                "--no-reflogs",
                "--no-dangling",
                "--no-progress",
            ],
        ] {
            let output = git()
                .args(&args)
                .args(&shas)
                .output()
                .with_context(|| format!("run git {} for requested history", args[0]))?;
            if !output.status.success() {
                bail!(
                    "Requested Git history failed {} validation: {}{}",
                    args[0],
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
        Ok(())
    }

    /// Recover a loose object from Git's ODB, including packed objects, without
    /// trusting the requested OID or cat-file's header as proof of its contents.
    pub(super) fn read_verified_compressed_git_object(&self, oid: &str) -> Result<Vec<u8>> {
        let mut objects = self.read_git_objects_batch(&[oid.to_string()])?;
        let (obj_type, content) = objects.pop().context("Missing local Git object")?;
        let object = GitObject::new(obj_type, content);
        let actual_oid = object.id().to_hex();
        if actual_oid != oid {
            bail!("Local Git object id mismatch: expected {oid}, got {actual_oid}");
        }

        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&object.to_loose_format())?;
        Ok(encoder.finish()?)
    }

    /// Batch check which objects git already has (returns set of existing oids)
    pub(super) fn git_batch_check_objects<'a>(
        &self,
        oids: impl Iterator<Item = &'a str>,
    ) -> Result<HashSet<String>> {
        let mut existing = HashSet::new();
        let oids: Vec<_> = oids.collect();

        // Process in chunks to avoid memory issues with huge repos
        const BATCH_SIZE: usize = 1000;
        for chunk in oids.chunks(BATCH_SIZE) {
            let mut child = Command::new("git")
                .args(["cat-file", "--batch-check=%(objectname)"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .context("Failed to spawn git cat-file")?;

            {
                let stdin = child.stdin.as_mut().context("Failed to open stdin")?;
                for oid in chunk {
                    writeln!(stdin, "{}", oid)?;
                }
            }

            let output = child
                .wait_with_output()
                .context("Failed to read git cat-file output")?;

            // Parse output - valid objects return just the oid, missing ones return "oid missing"
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                let line = line.trim();
                if line.len() == 40 && !line.contains(' ') {
                    existing.insert(line.to_string());
                }
            }
        }
        Ok(existing)
    }

    pub(super) fn git_dir_path() -> PathBuf {
        PathBuf::from(std::env::var("GIT_DIR").unwrap_or_else(|_| ".git".to_string()))
    }

    /// Write loose object to a git object store.
    /// The data is zlib-compressed loose object format.
    pub(super) fn write_git_object_to_dir(git_dir: &Path, oid: &str, data: &[u8]) -> Result<()> {
        // Git objects are stored as .git/objects/xx/yy... where xx is first 2 chars
        if oid.len() < 3 {
            bail!("Invalid object id: {}", oid);
        }

        let (dir_name, file_name) = oid.split_at(2);
        let obj_dir = git_dir.join("objects").join(dir_name);
        std::fs::create_dir_all(&obj_dir).context("Failed to create object directory")?;

        let obj_path = obj_dir.join(file_name);
        if obj_path.exists() {
            return Ok(());
        }

        std::fs::write(&obj_path, data).context("Failed to write git object")?;
        debug!("Wrote git object {} as loose object", oid);
        Ok(())
    }

    /// List objects that need to be pushed.
    pub(super) fn list_objects_to_push(
        &self,
        sha: &str,
        exclude: &[String],
    ) -> Result<Vec<String>> {
        self.list_objects_for_shas(&[sha.to_string()], exclude)
    }

    pub(super) fn list_objects_for_shas(
        &self,
        include: &[String],
        exclude: &[String],
    ) -> Result<Vec<String>> {
        if include.is_empty() {
            return Ok(Vec::new());
        }

        let mut command = Command::new("git");
        command.arg("rev-list").arg("--objects");
        for sha in include {
            command.arg(sha);
        }
        if !exclude.is_empty() {
            command.arg("--not");
            for sha in exclude {
                command.arg(sha);
            }
        }

        let output = command.output()?;
        if !output.status.success() {
            bail!("Failed to list objects");
        }

        let mut seen = HashSet::new();
        let mut objects = Vec::new();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            // Format: <sha> [path]
            if let Some(oid) = line.split_whitespace().next() {
                let oid = oid.to_string();
                if seen.insert(oid.clone()) {
                    objects.push(oid);
                }
            }
        }

        Ok(objects)
    }

    /// Read multiple git objects using git cat-file --batch
    /// Processes in batches to avoid pipe buffer deadlock
    pub(super) fn read_git_objects_batch(
        &self,
        oids: &[String],
    ) -> Result<Vec<(ObjectType, Vec<u8>)>> {
        if oids.is_empty() {
            return Ok(Vec::new());
        }

        let total = oids.len();
        let mut results = Vec::with_capacity(total);
        let progress_interval = fetch_progress_interval();
        let mut last_progress = Instant::now();

        // Process in batches of 100 to avoid pipe buffer issues
        const BATCH_SIZE: usize = 100;

        for (batch_idx, batch) in oids.chunks(BATCH_SIZE).enumerate() {
            let mut child = Command::new("git")
                .args(["cat-file", "--batch"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()?;

            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| anyhow::anyhow!("Failed to open stdin"))?;
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| anyhow::anyhow!("Failed to open stdout"))?;

            // Write batch OIDs to stdin
            for oid in batch {
                writeln!(stdin, "{}", oid)?;
            }
            drop(stdin);

            // Read responses
            let mut reader = BufReader::new(stdout);

            for (i, oid) in batch.iter().enumerate() {
                let mut header = String::new();
                reader.read_line(&mut header)?;
                let header = header.trim();

                let parts: Vec<&str> = header.split_whitespace().collect();
                if parts.len() < 3 {
                    bail!("Object not found or invalid header for {}: {}", oid, header);
                }

                let obj_type = match parts[1] {
                    "blob" => ObjectType::Blob,
                    "tree" => ObjectType::Tree,
                    "commit" => ObjectType::Commit,
                    "tag" => ObjectType::Tag,
                    _ => bail!("Unknown object type: {}", parts[1]),
                };

                let size: usize = parts[2].parse()?;
                let mut content = vec![0u8; size];
                reader.read_exact(&mut content)?;

                // Read trailing newline
                let mut newline = [0u8; 1];
                reader.read_exact(&mut newline)?;

                results.push((obj_type, content));

                // Progress indicator
                let done = batch_idx * BATCH_SIZE + i + 1;
                if done == total || last_progress.elapsed() >= progress_interval {
                    eprintln!("  Reading objects: {}/{}", done, total);
                    last_progress = Instant::now();
                }
            }

            child.wait()?;
        }

        Ok(results)
    }
}
