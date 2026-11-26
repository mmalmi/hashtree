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
        let shas: Vec<_> = self
            .fetch_specs
            .iter()
            .map(|spec| spec.sha.as_str())
            .collect();
        Self::verify_git_object_closure(&shas)
    }

    pub(super) fn strict_git_command() -> Command {
        let mut command = Command::new("git");
        command
            .arg("--no-replace-objects")
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_GRAFT_FILE", "");
        command
    }

    pub(super) fn git_command_for_push() -> Command {
        if Self::local_rebuild_requested() {
            Self::strict_git_command()
        } else {
            Command::new("git")
        }
    }

    pub(super) fn verify_git_object_closure(shas: &[&str]) -> Result<()> {
        let mut shas = shas.to_vec();
        shas.sort_unstable();
        shas.dedup();
        if shas.is_empty() || shas.iter().any(|sha| ObjectId::from_hex(sha).is_none()) {
            bail!("Cannot verify pack recovery without valid requested object IDs");
        }
        let shallow = Self::strict_git_command()
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
            let output = Self::strict_git_command()
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
        let oids: Vec<&'a str> = oids.collect();
        if oids.is_empty() {
            return Ok(HashSet::new());
        }

        let mut child = Command::new("git")
            .args(["cat-file", "--batch-check=%(objectname)"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("Failed to spawn git cat-file")?;

        let stdin = child.stdin.take().context("Failed to open stdin")?;
        let stdout = child.stdout.take().context("Failed to open stdout")?;

        let existing = std::thread::scope(|scope| -> Result<HashSet<String>> {
            let writer = scope.spawn(move || -> std::io::Result<()> {
                let mut w = std::io::BufWriter::new(stdin);
                for oid in &oids {
                    w.write_all(oid.as_bytes())?;
                    w.write_all(b"\n")?;
                }
                w.flush()
            });

            let mut existing = HashSet::new();
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            while reader.read_line(&mut line)? > 0 {
                let trimmed = line.trim();
                if trimmed.len() == 40 && !trimmed.contains(' ') {
                    existing.insert(trimmed.to_string());
                }
                line.clear();
            }

            writer
                .join()
                .map_err(|_| anyhow::anyhow!("git cat-file stdin writer panicked"))??;
            Ok(existing)
        })?;

        let _ = child.wait();
        Ok(existing)
    }

    pub(super) fn git_dir_path() -> PathBuf {
        PathBuf::from(std::env::var("GIT_DIR").unwrap_or_else(|_| ".git".to_string()))
    }

    /// Write loose object to a git object store.
    /// The data is zlib-compressed loose object format.
    #[cfg(test)]
    pub(super) fn write_git_object_to_dir(git_dir: &Path, oid: &str, data: &[u8]) -> Result<()> {
        let mut created_dirs = HashSet::new();
        Self::write_git_object_to_dir_cached(git_dir, oid, data, &mut created_dirs)
    }

    pub(super) fn write_git_object_to_dir_cached(
        git_dir: &Path,
        oid: &str,
        data: &[u8],
        created_dirs: &mut HashSet<[u8; 2]>,
    ) -> Result<()> {
        // Git objects are stored as .git/objects/xx/yy... where xx is first 2 chars
        if oid.len() < 3 {
            bail!("Invalid object id: {}", oid);
        }

        let (dir_name, file_name) = oid.split_at(2);
        let obj_dir = git_dir.join("objects").join(dir_name);
        let prefix_bytes = dir_name.as_bytes();
        let dir_key = if prefix_bytes.len() == 2 {
            Some([prefix_bytes[0], prefix_bytes[1]])
        } else {
            None
        };
        if dir_key.is_none_or(|k| !created_dirs.contains(&k)) {
            std::fs::create_dir_all(&obj_dir).context("Failed to create object directory")?;
            if let Some(k) = dir_key {
                created_dirs.insert(k);
            }
        }

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

        let mut command = Self::git_command_for_push();
        command.arg("rev-list").arg("--objects");
        if Self::local_rebuild_requested() {
            command.arg("--missing=error");
        }
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

    /// Read multiple git objects using a single pipelined `git cat-file --batch`
    /// process with a scoped stdin writer thread to prevent pipe buffer deadlock.
    pub(super) fn read_git_objects_batch(
        &self,
        oids: &[String],
    ) -> Result<Vec<(ObjectType, Vec<u8>)>> {
        if oids.is_empty() {
            return Ok(Vec::new());
        }

        let total = oids.len();
        let progress_interval = fetch_progress_interval();

        let mut child = Self::git_command_for_push()
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("Failed to open stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("Failed to open stdout"))?;

        let results = std::thread::scope(|scope| -> Result<Vec<(ObjectType, Vec<u8>)>> {
            let writer = scope.spawn(move || -> std::io::Result<()> {
                let mut w = std::io::BufWriter::new(stdin);
                for oid in oids {
                    w.write_all(oid.as_bytes())?;
                    w.write_all(b"\n")?;
                }
                w.flush()
            });

            let mut results = Vec::with_capacity(total);
            let mut reader = BufReader::new(stdout);
            let mut header = String::new();
            let mut last_progress = Instant::now();

            for (i, oid) in oids.iter().enumerate() {
                header.clear();
                reader.read_line(&mut header)?;
                let trimmed = header.trim();

                let mut parts = trimmed.split_whitespace();
                let _returned_oid = parts.next();
                let obj_type_str = parts.next();
                let size_str = parts.next();
                let (Some(obj_type_str), Some(size_str)) = (obj_type_str, size_str) else {
                    bail!(
                        "Object not found or invalid header for {}: {}",
                        oid,
                        trimmed
                    );
                };

                let obj_type = match obj_type_str {
                    "blob" => ObjectType::Blob,
                    "tree" => ObjectType::Tree,
                    "commit" => ObjectType::Commit,
                    "tag" => ObjectType::Tag,
                    _ => bail!("Unknown object type: {}", obj_type_str),
                };

                let size: usize = size_str.parse()?;
                let mut content = vec![0u8; size];
                reader.read_exact(&mut content)?;

                // Read trailing newline
                let mut newline = [0u8; 1];
                reader.read_exact(&mut newline)?;

                results.push((obj_type, content));

                let done = i + 1;
                if done == total || last_progress.elapsed() >= progress_interval {
                    eprintln!("  Reading objects: {}/{}", done, total);
                    last_progress = Instant::now();
                }
            }

            let _ = writer.join();
            Ok(results)
        })?;

        let _ = child.wait();
        Ok(results)
    }
}
