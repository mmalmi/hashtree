use super::*;
use futures::io::AllowStdIo;
use std::collections::HashMap;
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Debug, Default)]
pub struct AddProgress {
    bytes_processed: AtomicU64,
    bytes_total: AtomicU64,
    files_processed: AtomicU64,
    files_total: AtomicU64,
    totals_known: AtomicBool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AddProgressSnapshot {
    pub bytes_processed: u64,
    pub bytes_total: u64,
    pub files_processed: u64,
    pub files_total: u64,
    pub totals_known: bool,
}

impl AddProgress {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_file_discovered(&self, bytes: u64) {
        if self.totals_known.load(Ordering::Relaxed) {
            return;
        }
        self.files_total.fetch_add(1, Ordering::Relaxed);
        self.bytes_total.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn mark_totals_known(&self) {
        self.totals_known.store(true, Ordering::Relaxed);
    }

    pub fn record_bytes(&self, bytes: u64) {
        self.bytes_processed.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn record_file_finished(&self) {
        self.files_processed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> AddProgressSnapshot {
        AddProgressSnapshot {
            bytes_processed: self.bytes_processed.load(Ordering::Relaxed),
            bytes_total: self.bytes_total.load(Ordering::Relaxed),
            files_processed: self.files_processed.load(Ordering::Relaxed),
            files_total: self.files_total.load(Ordering::Relaxed),
            totals_known: self.totals_known.load(Ordering::Relaxed),
        }
    }
}

/// How to store a file or directory. The default is a plaintext, unpinned
/// upload with the default chunk size and no progress reporting.
#[derive(Clone, Copy, Debug, Default)]
pub struct UploadOptions<'a> {
    /// CHK-encrypt chunks and tree nodes. The returned CID then carries the key.
    pub encrypted: bool,
    /// Pin the root so it is not evicted. Directory uploads always pin.
    pub pin: bool,
    /// Chunk size in bytes; `None` uses the hashtree default.
    pub chunk_size: Option<usize>,
    /// For directories: honour .gitignore and skip common OS junk files.
    pub respect_gitignore: bool,
    /// Receives discovered and processed byte and file counts.
    pub progress: Option<&'a AddProgress>,
}

impl<'a> UploadOptions<'a> {
    /// Plaintext, pinned, ignore rules on: what `htree add --unencrypted` does.
    pub fn public() -> Self {
        Self {
            pin: true,
            respect_gitignore: true,
            ..Self::default()
        }
    }

    /// CHK-encrypted, pinned, ignore rules on: what `htree add` does.
    pub fn encrypted() -> Self {
        Self {
            encrypted: true,
            ..Self::public()
        }
    }

    pub fn with_chunk_size(mut self, chunk_size: Option<usize>) -> Self {
        self.chunk_size = chunk_size;
        self
    }

    pub fn with_respect_gitignore(mut self, respect_gitignore: bool) -> Self {
        self.respect_gitignore = respect_gitignore;
        self
    }

    pub fn with_pin(mut self, pin: bool) -> Self {
        self.pin = pin;
        self
    }

    pub fn with_progress(mut self, progress: &'a AddProgress) -> Self {
        self.progress = Some(progress);
        self
    }
}

/// Store one file from disk, streaming it chunk by chunk.
async fn add_file_to_tree<S: Store>(
    tree: &HashTree<S>,
    path: &Path,
    progress: Option<&AddProgress>,
) -> Result<(Cid, u64)> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("Failed to open file {}", path.display()))?;
    if let Some(progress) = progress {
        if let Ok(metadata) = file.metadata() {
            progress.record_file_discovered(metadata.len());
        }
    }
    let (cid, size) = tree
        .put_stream_with_progress(AllowStdIo::new(file), |bytes| {
            if let Some(progress) = progress {
                progress.record_bytes(bytes);
            }
        })
        .await
        .map_err(|e| anyhow::anyhow!("Failed to store file {}: {}", path.display(), e))?;
    if let Some(progress) = progress {
        progress.record_file_finished();
    }
    Ok((cid, size))
}

/// Store a directory tree from disk and return the root directory CID.
///
/// Respects .gitignore and skips common OS junk files when
/// `respect_gitignore` is set. Symlinks to files are stored as the files
/// they point at; the walker does not descend through symlinked directories.
pub async fn add_directory_to_tree<S: Store>(
    tree: &HashTree<S>,
    dir: &Path,
    respect_gitignore: bool,
    progress: Option<&AddProgress>,
) -> Result<Cid> {
    add_directory_to_tree_inner(tree, dir, respect_gitignore, progress, None).await
}

async fn add_directory_to_tree_inner<S: Store>(
    tree: &HashTree<S>,
    current_path: &Path,
    respect_gitignore: bool,
    progress: Option<&AddProgress>,
    #[cfg(unix)] mut cache: Option<&mut super::add_cache::DirectoryCache>,
    #[cfg(not(unix))] _cache: Option<()>,
) -> Result<Cid> {
    // Files of each directory with their plaintext sizes, keyed by the
    // directory's path relative to the root.
    let mut dir_contents: HashMap<PathBuf, Vec<(String, Cid, u64)>> = HashMap::new();
    dir_contents.insert(PathBuf::new(), Vec::new()); // Root
    let mut large_files: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut small_files: Vec<(PathBuf, PathBuf)> = Vec::new();

    let walker = crate::ignore_rules::build_content_walker(current_path, respect_gitignore);

    for result in walker {
        let entry = result?;
        let path = entry.path();

        // Skip the root directory itself
        if path == current_path {
            continue;
        }

        let relative = path.strip_prefix(current_path).unwrap_or(path);

        // The walker already knows the type of regular files and directories;
        // only a symlink needs a second look at what it points to.
        let kind = entry.file_type();
        #[cfg(unix)]
        if cache.is_some() {
            anyhow::ensure!(
                kind.is_some_and(|kind| kind.is_file() || kind.is_dir()),
                "cached add requires regular files and directories; symlinks are not followed"
            );
        }
        let (is_file, is_dir) = match kind {
            Some(kind) if kind.is_file() => (true, false),
            Some(kind) if kind.is_dir() => (false, true),
            _ => (path.is_file(), path.is_dir()),
        };

        if is_file {
            #[cfg(unix)]
            if let Some(cache) = cache.as_deref_mut() {
                // The cache is stateful, so cached adds stay sequential.
                let (cid, size) = cache.file(tree, relative, progress).await?;
                record_file(&mut dir_contents, relative, cid, size);
                continue;
            }
            let size = entry.metadata().map_or(0, |metadata| metadata.len());
            let pending = (path.to_path_buf(), relative.to_path_buf());
            if size >= PARALLEL_FILE_ADD_MIN_BYTES {
                large_files.push(pending);
            } else {
                small_files.push(pending);
            }
        } else if is_dir {
            // Ensure directory entry exists
            dir_contents.entry(relative.to_path_buf()).or_default();
        }
    }

    for (relative, cid, size) in add_files_to_tree(tree, large_files, progress).await? {
        record_file(&mut dir_contents, &relative, cid, size);
    }
    for (path, relative) in small_files {
        let (cid, size) = add_file_to_tree(tree, &path, progress).await?;
        record_file(&mut dir_contents, &relative, cid, size);
    }

    build_directory_tree(tree, dir_contents).await
}

fn record_file(
    dir_contents: &mut HashMap<PathBuf, Vec<(String, Cid, u64)>>,
    relative: &Path,
    cid: Cid,
    size: u64,
) {
    let parent = relative.parent().map(Path::to_path_buf).unwrap_or_default();
    let name = relative
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    dir_contents
        .entry(parent)
        .or_default()
        .push((name, cid, size));
}

/// Files below this size are stored one at a time. Their cost is the store
/// commit, which is serialized, so threads gave no measurable gain on a tree
/// of 2 KiB files, while trees of 64 KiB and larger files were added roughly
/// twice as fast on two cores.
const PARALLEL_FILE_ADD_MIN_BYTES: u64 = 32 * 1024;

/// Most files a directory add stores at once. Each in-flight file may hold a
/// write batch in memory, so this also bounds peak memory.
const MAX_PARALLEL_FILE_ADDS: usize = 4;

/// Store `files` (absolute path, path relative to the root) and return each
/// relative path with its CID and plaintext size. Hashing and encryption dominate an add and are
/// independent per file, so files are spread over a few threads. Content
/// addressing makes the result independent of the order files finish in.
async fn add_files_to_tree<S: Store>(
    tree: &HashTree<S>,
    files: Vec<(PathBuf, PathBuf)>,
    progress: Option<&AddProgress>,
) -> Result<Vec<(PathBuf, Cid, u64)>> {
    let workers = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(MAX_PARALLEL_FILE_ADDS)
        .min(files.len());

    if workers <= 1 {
        let mut added = Vec::with_capacity(files.len());
        for (path, relative) in files {
            let (cid, size) = add_file_to_tree(tree, &path, progress).await?;
            added.push((relative, cid, size));
        }
        return Ok(added);
    }

    // Each worker thread drives its own files to completion; the calling
    // thread only waits, so no executor is entered twice.
    let next = std::sync::atomic::AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let work = || -> Result<Vec<(PathBuf, Cid, u64)>> {
        let mut added = Vec::new();
        while !failed.load(Ordering::Relaxed) {
            let Some((path, relative)) = files.get(next.fetch_add(1, Ordering::Relaxed)) else {
                break;
            };
            match sync_block_on(add_file_to_tree(tree, path, progress)) {
                Ok((cid, size)) => added.push((relative.clone(), cid, size)),
                Err(error) => {
                    failed.store(true, Ordering::Relaxed);
                    return Err(error);
                }
            }
        }
        Ok(added)
    };

    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers).map(|_| scope.spawn(&work)).collect();
        let mut added = Vec::with_capacity(files.len());
        let mut first_error = None;
        for handle in handles {
            match handle.join() {
                Ok(Ok(part)) => added.extend(part),
                Ok(Err(error)) => {
                    first_error.get_or_insert(error);
                }
                Err(panic) => std::panic::resume_unwind(panic),
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(added),
        }
    })
}

/// Build directory nodes bottom-up. Each finished directory is handed to its
/// parent, so the work is linear in the number of directories.
async fn build_directory_tree<S: Store>(
    tree: &HashTree<S>,
    mut dir_contents: HashMap<PathBuf, Vec<(String, Cid, u64)>>,
) -> Result<Cid> {
    // Deepest first; the root (the empty path) sorts last.
    let mut dirs: Vec<PathBuf> = dir_contents.keys().cloned().collect();
    dirs.sort_by_key(|dir| std::cmp::Reverse(dir.components().count()));

    let mut finished_subdirs: HashMap<PathBuf, Vec<(String, Cid)>> = HashMap::new();

    for dir_path in dirs {
        let files = dir_contents.remove(&dir_path).unwrap_or_default();
        let subdirs = finished_subdirs.remove(&dir_path).unwrap_or_default();

        let mut entries: Vec<hashtree_core::DirEntry> =
            Vec::with_capacity(files.len() + subdirs.len());
        entries.extend(files.iter().map(|(name, cid, size)| {
            hashtree_core::DirEntry::from_cid(name.clone(), cid).with_size(*size)
        }));
        entries.extend(subdirs.iter().map(|(name, cid)| {
            hashtree_core::DirEntry::from_cid(name.clone(), cid)
                .with_link_type(hashtree_core::LinkType::Dir)
        }));

        let cid = tree
            .put_directory(entries)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to create directory node: {}", e))?;

        if dir_path.as_os_str().is_empty() {
            return Ok(cid);
        }
        let parent = dir_path.parent().map(Path::to_path_buf).unwrap_or_default();
        let name = dir_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        finished_subdirs
            .entry(parent)
            .or_default()
            .push((name, cid));
    }

    Err(anyhow::anyhow!("No root directory"))
}

impl HashtreeStore {
    fn upload_tree(&self, options: &UploadOptions<'_>) -> HashTree<StorageRouter> {
        let mut config = HashTreeConfig::new(self.store_arc());
        if !options.encrypted {
            config = config.public();
        }
        if let Some(chunk_size) = options.chunk_size {
            config = config.with_chunk_size(chunk_size);
        }
        HashTree::new(config)
    }

    fn pin_upload_root(&self, cid: &Cid) -> Result<()> {
        // Pin by hash only (the key is for decryption, not identification)
        let mut wtxn = self.env.write_txn()?;
        self.pins.put(&mut wtxn, cid.hash.as_slice(), &())?;
        wtxn.commit()?;
        Ok(())
    }

    /// Store a file, streaming from disk. Returns the CID as a string:
    /// the hash for plaintext, `hash:key` for encrypted content.
    pub fn upload_file_with<P: AsRef<Path>>(
        &self,
        file_path: P,
        options: UploadOptions<'_>,
    ) -> Result<String> {
        let tree = self.upload_tree(&options);
        let (cid, _size) = sync_block_on(add_file_to_tree(
            &tree,
            file_path.as_ref(),
            options.progress,
        ))?;
        // htree add pins; blossom uploads stay evictable.
        if options.pin {
            self.pin_upload_root(&cid)?;
        }
        Ok(cid.to_string())
    }

    /// Store a directory and pin its root. Returns the CID as a string:
    /// the hash for plaintext, `hash:key` for encrypted content.
    pub fn upload_dir_with<P: AsRef<Path>>(
        &self,
        dir_path: P,
        options: UploadOptions<'_>,
    ) -> Result<String> {
        let tree = self.upload_tree(&options);
        let root = sync_block_on(add_directory_to_tree(
            &tree,
            dir_path.as_ref(),
            options.respect_gitignore,
            options.progress,
        ))
        .context("Failed to upload directory")?;
        self.pin_upload_root(&root)?;
        Ok(root.to_string())
    }

    /// Upload a file as raw plaintext and return its hash (hex), with auto-pin
    pub fn upload_file<P: AsRef<Path>>(&self, file_path: P) -> Result<String> {
        self.upload_file_with(file_path, UploadOptions::public())
    }

    /// Upload a file without pinning (for blossom uploads that can be evicted)
    pub fn upload_file_no_pin<P: AsRef<Path>>(&self, file_path: P) -> Result<String> {
        self.upload_file_with(file_path, UploadOptions::public().with_pin(false))
    }

    /// Upload a file from a stream with progress callbacks
    pub fn upload_file_stream<R: Read, F>(
        &self,
        reader: R,
        _file_name: impl Into<String>,
        mut callback: F,
    ) -> Result<String>
    where
        F: FnMut(&str),
    {
        let tree = self.upload_tree(&UploadOptions::public());
        let (cid, _size) = sync_block_on(async { tree.put_stream(AllowStdIo::new(reader)).await })
            .context("Failed to store file")?;

        let root_hex = to_hex(&cid.hash);
        callback(&root_hex);

        // Auto-pin on upload
        self.pin_upload_root(&cid)?;
        Ok(root_hex)
    }

    /// Upload a directory as raw plaintext and return its root hash (hex).
    /// Respects .gitignore and ignores common OS junk files by default.
    pub fn upload_dir<P: AsRef<Path>>(&self, dir_path: P) -> Result<String> {
        self.upload_dir_with(dir_path, UploadOptions::public())
    }

    /// Upload a directory with options as raw plaintext (no CHK encryption)
    pub fn upload_dir_with_options<P: AsRef<Path>>(
        &self,
        dir_path: P,
        respect_gitignore: bool,
    ) -> Result<String> {
        self.upload_dir_with(
            dir_path,
            UploadOptions::public().with_respect_gitignore(respect_gitignore),
        )
    }

    /// Upload a file with CHK encryption, returns CID in format "hash:key"
    pub fn upload_file_encrypted<P: AsRef<Path>>(&self, file_path: P) -> Result<String> {
        self.upload_file_with(file_path, UploadOptions::encrypted())
    }

    /// Upload a directory with CHK encryption, returns CID
    /// Respects .gitignore and ignores common OS junk files by default.
    pub fn upload_dir_encrypted<P: AsRef<Path>>(&self, dir_path: P) -> Result<String> {
        self.upload_dir_with(dir_path, UploadOptions::encrypted())
    }

    /// Upload a directory with CHK encryption and options
    /// Returns CID as "hash:key" format for encrypted directories
    pub fn upload_dir_encrypted_with_options<P: AsRef<Path>>(
        &self,
        dir_path: P,
        respect_gitignore: bool,
    ) -> Result<String> {
        self.upload_dir_with(
            dir_path,
            UploadOptions::encrypted().with_respect_gitignore(respect_gitignore),
        )
    }

    #[deprecated(since = "0.4.3", note = "use `upload_file_with`")]
    pub fn upload_file_with_chunk_size<P: AsRef<Path>>(
        &self,
        file_path: P,
        chunk_size: Option<usize>,
    ) -> Result<String> {
        self.upload_file_with(
            file_path,
            UploadOptions::public().with_chunk_size(chunk_size),
        )
    }

    #[deprecated(since = "0.4.3", note = "use `upload_file_with`")]
    pub fn upload_file_with_chunk_size_and_progress<P: AsRef<Path>>(
        &self,
        file_path: P,
        chunk_size: Option<usize>,
        progress: &AddProgress,
    ) -> Result<String> {
        let options = UploadOptions::public().with_chunk_size(chunk_size);
        self.upload_file_with(file_path, options.with_progress(progress))
    }

    #[deprecated(since = "0.4.3", note = "use `upload_dir_with`")]
    pub fn upload_dir_with_options_and_chunk_size<P: AsRef<Path>>(
        &self,
        dir_path: P,
        respect_gitignore: bool,
        chunk_size: Option<usize>,
    ) -> Result<String> {
        let options = UploadOptions::public().with_respect_gitignore(respect_gitignore);
        self.upload_dir_with(dir_path, options.with_chunk_size(chunk_size))
    }

    #[deprecated(since = "0.4.3", note = "use `upload_dir_with`")]
    pub fn upload_dir_with_options_and_chunk_size_and_progress<P: AsRef<Path>>(
        &self,
        dir_path: P,
        respect_gitignore: bool,
        chunk_size: Option<usize>,
        progress: &AddProgress,
    ) -> Result<String> {
        let options = UploadOptions::public().with_respect_gitignore(respect_gitignore);
        self.upload_dir_with(
            dir_path,
            options.with_chunk_size(chunk_size).with_progress(progress),
        )
    }

    #[deprecated(since = "0.4.3", note = "use `upload_file_with`")]
    pub fn upload_file_encrypted_with_chunk_size<P: AsRef<Path>>(
        &self,
        file_path: P,
        chunk_size: Option<usize>,
    ) -> Result<String> {
        self.upload_file_with(
            file_path,
            UploadOptions::encrypted().with_chunk_size(chunk_size),
        )
    }

    #[deprecated(since = "0.4.3", note = "use `upload_file_with`")]
    pub fn upload_file_encrypted_with_chunk_size_and_progress<P: AsRef<Path>>(
        &self,
        file_path: P,
        chunk_size: Option<usize>,
        progress: &AddProgress,
    ) -> Result<String> {
        let options = UploadOptions::encrypted().with_chunk_size(chunk_size);
        self.upload_file_with(file_path, options.with_progress(progress))
    }

    #[deprecated(since = "0.4.3", note = "use `upload_dir_with`")]
    pub fn upload_dir_encrypted_with_options_and_chunk_size<P: AsRef<Path>>(
        &self,
        dir_path: P,
        respect_gitignore: bool,
        chunk_size: Option<usize>,
    ) -> Result<String> {
        let options = UploadOptions::encrypted().with_respect_gitignore(respect_gitignore);
        self.upload_dir_with(dir_path, options.with_chunk_size(chunk_size))
    }

    #[deprecated(since = "0.4.3", note = "use `upload_dir_with`")]
    pub fn upload_dir_encrypted_with_options_and_chunk_size_and_progress<P: AsRef<Path>>(
        &self,
        dir_path: P,
        respect_gitignore: bool,
        chunk_size: Option<usize>,
        progress: &AddProgress,
    ) -> Result<String> {
        let options = UploadOptions::encrypted().with_respect_gitignore(respect_gitignore);
        self.upload_dir_with(
            dir_path,
            options.with_chunk_size(chunk_size).with_progress(progress),
        )
    }

    /// Cache unchanged source-file CIDs outside the public directory. Unix file
    /// identity/change metadata and CAS retention are trusted; this is not a
    /// substitute for a full stored-payload bitrot audit.
    #[cfg(unix)]
    pub fn upload_dir_cached_with_progress(
        &self,
        source: &Path,
        respect_ignore: bool,
        chunk_size: Option<usize>,
        encrypted: bool,
        progress: &AddProgress,
        cache_path: &Path,
    ) -> Result<String> {
        let mut config = HashTreeConfig::new(self.store_arc());
        if !encrypted {
            config = config.public();
        }
        if let Some(chunk_size) = chunk_size {
            config = config.with_chunk_size(chunk_size);
        }
        let tree = HashTree::new(config);
        let root = sync_block_on(async {
            let mut cache = super::add_cache::DirectoryCache::open(
                &tree,
                source,
                &self.base_path,
                respect_ignore,
                cache_path,
            )
            .await?;
            let root = add_directory_to_tree_inner(
                &tree,
                source,
                respect_ignore,
                Some(progress),
                Some(&mut cache),
            )
            .await?;
            cache.save(&root)?;
            Ok::<_, anyhow::Error>(root)
        })?;
        let mut transaction = self.env.write_txn()?;
        self.pins.put(&mut transaction, root.hash.as_slice(), &())?;
        transaction.commit()?;
        Ok(root.to_string())
    }
}
