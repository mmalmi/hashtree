//! Opt-in source-file addressing cache; never part of the published directory.
use anyhow::{ensure, Context, Result};
use hashtree_core::{sha256, Cid, HashTree, Store};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Stamp {
    size: u64,
    device: u64,
    inode: u64,
    mode: u32,
    modified: (i64, i64),
    changed: (i64, i64),
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashtree_core::{DirEntry, HashTreeConfig, MemoryStore};
    use std::sync::Arc;

    async fn build(
        tree: &HashTree<MemoryStore>,
        source: &Path,
        store: &Path,
        cache: &Path,
    ) -> Result<(Cid, u64)> {
        let progress = super::super::AddProgress::new();
        let mut cache = DirectoryCache::open(tree, source, store, true, cache).await?;
        let mut entries = Vec::new();
        for name in ["old", "new"] {
            let cid = cache.file(tree, Path::new(name), Some(&progress)).await?;
            entries.push(DirEntry::from_cid(name, &cid));
        }
        let root = tree.put_directory(entries).await?;
        cache.save(&root)?;
        Ok((root, progress.snapshot().bytes_processed))
    }

    #[tokio::test]
    async fn unchanged_cache_reads_zero_source_bytes_and_changed_file_only_reads_itself(
    ) -> Result<()> {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source");
        fs::create_dir(&source)?;
        fs::write(source.join("old"), vec![1; 8192])?;
        fs::write(source.join("new"), vec![2; 4096])?;
        let store = Arc::new(MemoryStore::new());
        let tree = HashTree::new(HashTreeConfig::new(store.clone()));
        let path = temp.path().join("private-cache.json");
        let (original, bytes) = build(&tree, &source, temp.path(), &path).await?;
        assert_eq!(bytes, 12288);
        let (unchanged, bytes) = build(&tree, &source, temp.path(), &path).await?;
        assert_eq!(original, unchanged);
        assert_eq!(bytes, 0, "a no-op must not reread retained payloads");
        fs::write(source.join("new"), vec![3; 4096])?;
        let (changed, bytes) = build(&tree, &source, temp.path(), &path).await?;
        assert_ne!(original, changed);
        assert_eq!(bytes, 4096, "only the changed asset may be reread");
        assert_eq!(
            tree.get(&tree.resolve_path(&original, "old").await?.unwrap(), None)
                .await?
                .unwrap(),
            vec![1; 8192]
        );
        assert_eq!(
            tree.get(&tree.resolve_path(&original, "new").await?.unwrap(), None)
                .await?
                .unwrap(),
            vec![2; 4096]
        );
        let other = temp.path().join("other-store");
        fs::create_dir(&other)?;
        assert_eq!(build(&tree, &source, &other, &path).await?.1, 12288);
        fs::write(&path, "invalid cache")?;
        assert_eq!(build(&tree, &source, temp.path(), &path).await?.1, 12288);
        store.delete(&changed.hash).await?;
        // Last cache stores this same root; an absent root cannot authorize reuse.
        assert_eq!(build(&tree, &source, temp.path(), &path).await?.1, 12288);
        store.delete(&changed.hash).await?;
        store.put(changed.hash, vec![0]).await?;
        assert_eq!(build(&tree, &source, temp.path(), &path).await?.1, 12288);
        Ok(())
    }

    #[tokio::test]
    async fn cache_rejects_symlinks_parent_swaps_and_input_cache_paths() -> Result<()> {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source");
        fs::create_dir_all(source.join("nested"))?;
        fs::write(source.join("nested/file"), "safe")?;
        let tree = HashTree::new(HashTreeConfig::new(Arc::new(MemoryStore::new())));
        let mut cache = DirectoryCache::open(
            &tree,
            &source,
            temp.path(),
            true,
            &temp.path().join("cache"),
        )
        .await?;
        cache.file(&tree, Path::new("nested/file"), None).await?;
        fs::rename(source.join("nested"), source.join("original"))?;
        symlink(source.join("original"), source.join("nested"))?;
        assert!(cache
            .file(&tree, Path::new("nested/file"), None)
            .await
            .is_err());
        symlink(source.join("original/file"), source.join("link"))?;
        assert!(cache.file(&tree, Path::new("link"), None).await.is_err());
        assert!(
            DirectoryCache::open(&tree, &source, temp.path(), true, &source.join("cache"))
                .await
                .is_err()
        );
        assert!(cache
            .file(&tree, Path::new("../source/original/file"), None)
            .await
            .is_err());
        let forbidden = source.join("new-private-parent/cache");
        assert!(
            DirectoryCache::open(&tree, &source, temp.path(), true, &forbidden)
                .await
                .is_err()
        );
        assert!(
            !source.join("new-private-parent").exists(),
            "rejected cache paths must not mutate the input"
        );
        Ok(())
    }

    #[test]
    fn actual_cached_directory_keeps_format_and_only_addresses_changed_sources() -> Result<()> {
        use super::super::{AddProgress, HashtreeStore};
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source");
        fs::create_dir(&source)?;
        fs::write(source.join("old"), vec![1; 8192])?;
        fs::write(source.join("new"), vec![2; 4096])?;
        let store = HashtreeStore::new_with_backend(
            temp.path().join("data"),
            hashtree_config::StorageBackend::Fs,
            0,
        )?;
        let path = temp.path().join("cache/files.json");
        let run = || {
            let progress = AddProgress::new();
            let root = store
                .upload_dir_cached_with_progress(&source, true, None, true, &progress, &path)?;
            Ok::<_, anyhow::Error>((root, progress.snapshot().bytes_processed))
        };
        let (original, bytes) = run()?;
        assert_eq!(bytes, 12288);
        assert_eq!(original, store.upload_dir_encrypted(&source)?);
        assert_eq!(run()?, (original.clone(), 0));
        fs::write(source.join("new"), vec![3; 4096])?;
        let (changed, bytes) = run()?;
        assert_ne!(original, changed);
        assert_eq!(bytes, 4096);
        assert_eq!(changed, store.upload_dir_encrypted(&source)?);
        let cached: CacheFile = serde_json::from_slice(&fs::read(&path)?)?;
        let old = Cid::parse(&cached.state.files[Path::new("old")].cid)?;
        futures::executor::block_on(store.store_arc().delete(&old.hash))?;
        assert_eq!(run()?, (changed.clone(), 8192));
        fs::write(source.join(".ignore"), "new\n")?;
        let (ignored, bytes) = run()?;
        assert_ne!(ignored, changed);
        assert_eq!(bytes, 4);
        assert_eq!(ignored, store.upload_dir_encrypted(&source)?);
        Ok(())
    }
}
impl Stamp {
    fn of(metadata: &fs::Metadata) -> Result<Self> {
        ensure!(metadata.is_file(), "add cache requires regular files");
        use std::os::unix::fs::MetadataExt;
        Ok(Self {
            size: metadata.len(),
            device: metadata.dev(),
            inode: metadata.ino(),
            mode: metadata.mode(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        })
    }
}

#[derive(Serialize, Deserialize)]
struct Entry {
    stamp: Stamp,
    cid: String,
}
#[derive(Default, Serialize, Deserialize)]
struct State {
    policy: String,
    root: Option<String>,
    files: BTreeMap<PathBuf, Entry>,
}

#[derive(Serialize, Deserialize)]
struct CacheFile {
    checksum: String,
    state: State,
}

pub(super) struct DirectoryCache {
    path: PathBuf,
    source: fs::File,
    previous: State,
    next: State,
}

impl DirectoryCache {
    pub(super) async fn open<S: Store>(
        tree: &HashTree<S>,
        source: &Path,
        store: &Path,
        respect_ignore: bool,
        path: &Path,
    ) -> Result<Self> {
        let source = source.canonicalize()?;
        let path = std::path::absolute(path)?;
        let parent = path
            .parent()
            .context("add cache needs a parent directory")?;
        let mut existing = parent.to_path_buf();
        let mut missing = Vec::new();
        while !existing.exists() {
            missing.push(
                existing
                    .file_name()
                    .context("invalid add cache parent")?
                    .to_owned(),
            );
            existing = existing
                .parent()
                .context("invalid add cache parent")?
                .to_path_buf();
        }
        let mut parent = existing.canonicalize()?;
        for component in missing.into_iter().rev() {
            parent.push(component);
        }
        let path = parent.join(path.file_name().context("invalid add cache path")?);
        ensure!(
            !path.starts_with(&source),
            "add cache must be outside the input directory"
        );
        fs::create_dir_all(parent)?;
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "add cache must be a regular file"
            );
        }
        let policy = serde_json::to_string(&(
            1,
            &source,
            store.canonicalize()?,
            tree.is_encrypted(),
            tree.chunk_size(),
            tree.max_links(),
            respect_ignore,
        ))?;
        let mut previous: State = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<CacheFile>(&bytes).ok())
            .filter(|cache| {
                serde_json::to_vec(&cache.state)
                    .is_ok_and(|bytes| hex::encode(sha256(&bytes)) == cache.checksum)
            })
            .map(|cache| cache.state)
            .unwrap_or_default();
        // Verify the small saved directory root, not every retained payload byte.
        // Chunk integrity/retention remains the content-addressed store's invariant.
        let root_valid = if previous.policy == policy {
            if let Some(root) = previous.root.as_deref().and_then(|s| Cid::parse(s).ok()) {
                tree.get_store()
                    .get(&root.hash)
                    .await
                    .ok()
                    .flatten()
                    .is_some_and(|bytes| sha256(&bytes) == root.hash)
                    && tree.list_directory(&root).await.is_ok()
            } else {
                false
            }
        } else {
            false
        };
        if !root_valid {
            previous = State::default();
        }
        use std::os::unix::fs::OpenOptionsExt;
        let source = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(source)?;
        Ok(Self {
            path,
            source,
            previous,
            next: State {
                policy,
                ..State::default()
            },
        })
    }

    // Resolve each component relative to a held root descriptor. O_NOFOLLOW on
    // the final filename alone would still follow a swapped parent directory.
    fn open_file(&self, relative: &Path) -> Result<fs::File> {
        use std::os::{
            fd::{AsRawFd, FromRawFd},
            unix::ffi::OsStrExt,
        };
        let mut directory = self.source.try_clone()?;
        let components: Vec<_> = relative.components().collect();
        ensure!(!components.is_empty(), "invalid cached file path");
        for (index, component) in components.iter().enumerate() {
            let std::path::Component::Normal(name) = component else {
                anyhow::bail!("invalid cached file path")
            };
            let name = std::ffi::CString::new(name.as_bytes())?;
            let flags = libc::O_RDONLY
                | libc::O_NOFOLLOW
                | libc::O_CLOEXEC
                | libc::O_NONBLOCK
                | if index + 1 < components.len() {
                    libc::O_DIRECTORY
                } else {
                    0
                };
            let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
            ensure!(
                fd >= 0,
                "cannot safely open cached source: {}",
                std::io::Error::last_os_error()
            );
            directory = unsafe { fs::File::from_raw_fd(fd) };
        }
        Ok(directory)
    }

    pub(super) async fn file<S: Store>(
        &mut self,
        tree: &HashTree<S>,
        relative: &Path,
        progress: Option<&super::AddProgress>,
    ) -> Result<Cid> {
        let file = self.open_file(relative)?;
        let stamp = Stamp::of(&file.metadata()?)?;
        if let Some(progress) = progress {
            progress.record_file_discovered(stamp.size);
        }
        let cached = self
            .previous
            .files
            .get(relative)
            .filter(|entry| entry.stamp == stamp)
            .and_then(|entry| Cid::parse(&entry.cid).ok());
        let cached = match cached {
            Some(cid) if tree.get_store().has(&cid.hash).await? => Some(cid),
            _ => None,
        };
        let cid = if let Some(cid) = cached {
            cid
        } else {
            let check = file.try_clone()?;
            let (cid, _) = tree
                .put_stream_with_progress(futures::io::AllowStdIo::new(file), |bytes| {
                    if let Some(progress) = progress {
                        progress.record_bytes(bytes);
                    }
                })
                .await?;
            ensure!(
                stamp == Stamp::of(&check.metadata()?)?,
                "source changed during cached add"
            );
            cid
        };
        ensure!(
            stamp == Stamp::of(&self.open_file(relative)?.metadata()?)?,
            "source path changed during cached add"
        );
        self.next.files.insert(
            relative.to_path_buf(),
            Entry {
                stamp,
                cid: cid.to_string(),
            },
        );
        if let Some(progress) = progress {
            progress.record_file_finished();
        }
        Ok(cid)
    }

    pub(super) fn save(mut self, root: &Cid) -> Result<()> {
        use std::io::Write;
        self.next.root = Some(root.to_string());
        let mut temporary = tempfile::NamedTempFile::new_in(self.path.parent().unwrap())?;
        let checksum = hex::encode(sha256(&serde_json::to_vec(&self.next)?));
        serde_json::to_writer(
            &mut temporary,
            &CacheFile {
                checksum,
                state: self.next,
            },
        )?;
        temporary.flush()?;
        temporary.as_file().sync_all()?;
        temporary.persist(&self.path)?;
        Ok(())
    }
}
