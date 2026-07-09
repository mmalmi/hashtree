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

impl HashtreeStore {
    /// Upload a file as raw plaintext and return its CID, with auto-pin
    pub fn upload_file<P: AsRef<Path>>(&self, file_path: P) -> Result<String> {
        self.upload_file_with_chunk_size(file_path, None)
    }

    /// Upload a file without pinning (for blossom uploads that can be evicted)
    pub fn upload_file_no_pin<P: AsRef<Path>>(&self, file_path: P) -> Result<String> {
        self.upload_file_internal(file_path, false, None, None)
    }

    pub fn upload_file_with_chunk_size<P: AsRef<Path>>(
        &self,
        file_path: P,
        chunk_size: Option<usize>,
    ) -> Result<String> {
        self.upload_file_internal(file_path, true, chunk_size, None)
    }

    pub fn upload_file_with_chunk_size_and_progress<P: AsRef<Path>>(
        &self,
        file_path: P,
        chunk_size: Option<usize>,
        progress: &AddProgress,
    ) -> Result<String> {
        self.upload_file_internal(file_path, true, chunk_size, Some(progress))
    }

    fn upload_file_internal<P: AsRef<Path>>(
        &self,
        file_path: P,
        pin: bool,
        chunk_size: Option<usize>,
        progress: Option<&AddProgress>,
    ) -> Result<String> {
        let file_path = file_path.as_ref();
        let file = std::fs::File::open(file_path)
            .with_context(|| format!("Failed to open file {}", file_path.display()))?;
        if let Some(progress) = progress {
            if let Ok(metadata) = file.metadata() {
                progress.record_file_discovered(metadata.len());
            }
        }

        // Store raw plaintext blobs without CHK encryption, streaming from disk.
        let store = self.store_arc();
        let mut config = HashTreeConfig::new(store).public();
        if let Some(chunk_size) = chunk_size {
            config = config.with_chunk_size(chunk_size);
        }
        let tree = HashTree::new(config);

        let (cid, _size) = sync_block_on(async {
            tree.put_stream_with_progress(AllowStdIo::new(file), |bytes| {
                if let Some(progress) = progress {
                    progress.record_bytes(bytes);
                }
            })
            .await
        })
        .context("Failed to store file")?;
        if let Some(progress) = progress {
            progress.record_file_finished();
        }

        // Only pin if requested (htree add = pin, blossom upload = no pin)
        if pin {
            let mut wtxn = self.env.write_txn()?;
            self.pins.put(&mut wtxn, cid.hash.as_slice(), &())?;
            wtxn.commit()?;
        }

        Ok(to_hex(&cid.hash))
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
        // Use HashTree.put_stream for streaming upload without CHK encryption.
        let store = self.store_arc();
        let tree = HashTree::new(HashTreeConfig::new(store).public());

        let (cid, _size) = sync_block_on(async { tree.put_stream(AllowStdIo::new(reader)).await })
            .context("Failed to store file")?;

        let root_hex = to_hex(&cid.hash);
        callback(&root_hex);

        // Auto-pin on upload
        let mut wtxn = self.env.write_txn()?;
        self.pins.put(&mut wtxn, cid.hash.as_slice(), &())?;
        wtxn.commit()?;

        Ok(root_hex)
    }

    /// Upload a directory and return its root hash (hex)
    /// Respects .gitignore and ignores common OS junk files by default.
    pub fn upload_dir<P: AsRef<Path>>(&self, dir_path: P) -> Result<String> {
        self.upload_dir_with_options(dir_path, true)
    }

    /// Upload a directory with options as raw plaintext (no CHK encryption)
    pub fn upload_dir_with_options<P: AsRef<Path>>(
        &self,
        dir_path: P,
        respect_gitignore: bool,
    ) -> Result<String> {
        self.upload_dir_with_options_and_chunk_size(dir_path, respect_gitignore, None)
    }

    pub fn upload_dir_with_options_and_chunk_size<P: AsRef<Path>>(
        &self,
        dir_path: P,
        respect_gitignore: bool,
        chunk_size: Option<usize>,
    ) -> Result<String> {
        self.upload_dir_with_options_and_chunk_size_internal(
            dir_path,
            respect_gitignore,
            chunk_size,
            None,
        )
    }

    pub fn upload_dir_with_options_and_chunk_size_and_progress<P: AsRef<Path>>(
        &self,
        dir_path: P,
        respect_gitignore: bool,
        chunk_size: Option<usize>,
        progress: &AddProgress,
    ) -> Result<String> {
        self.upload_dir_with_options_and_chunk_size_internal(
            dir_path,
            respect_gitignore,
            chunk_size,
            Some(progress),
        )
    }

    fn upload_dir_with_options_and_chunk_size_internal<P: AsRef<Path>>(
        &self,
        dir_path: P,
        respect_gitignore: bool,
        chunk_size: Option<usize>,
        progress: Option<&AddProgress>,
    ) -> Result<String> {
        let dir_path = dir_path.as_ref();

        let store = self.store_arc();
        let mut config = HashTreeConfig::new(store).public();
        if let Some(chunk_size) = chunk_size {
            config = config.with_chunk_size(chunk_size);
        }
        let tree = HashTree::new(config);

        let root_cid = sync_block_on(async {
            self.upload_dir_recursive(&tree, dir_path, dir_path, respect_gitignore, progress)
                .await
        })
        .context("Failed to upload directory")?;

        let root_hex = to_hex(&root_cid.hash);

        let mut wtxn = self.env.write_txn()?;
        self.pins.put(&mut wtxn, root_cid.hash.as_slice(), &())?;
        wtxn.commit()?;

        Ok(root_hex)
    }

    async fn upload_dir_recursive<S: Store>(
        &self,
        tree: &HashTree<S>,
        _root_path: &Path,
        current_path: &Path,
        respect_gitignore: bool,
        progress: Option<&AddProgress>,
    ) -> Result<Cid> {
        // Build directory structure from flat file list - store full Cid with key
        let mut dir_contents: HashMap<String, Vec<(String, Cid)>> = HashMap::new();
        dir_contents.insert(String::new(), Vec::new()); // Root

        let walker = crate::ignore_rules::build_content_walker(current_path, respect_gitignore);

        for result in walker {
            let entry = result?;
            let path = entry.path();

            // Skip the root directory itself
            if path == current_path {
                continue;
            }

            let relative = path.strip_prefix(current_path).unwrap_or(path);

            if path.is_file() {
                let file = std::fs::File::open(path)
                    .with_context(|| format!("Failed to open file {}", path.display()))?;
                if let Some(progress) = progress {
                    if let Ok(metadata) = file.metadata() {
                        progress.record_file_discovered(metadata.len());
                    }
                }
                let (cid, _size) = tree
                    .put_stream_with_progress(AllowStdIo::new(file), |bytes| {
                        if let Some(progress) = progress {
                            progress.record_bytes(bytes);
                        }
                    })
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!("Failed to upload file {}: {}", path.display(), e)
                    })?;
                if let Some(progress) = progress {
                    progress.record_file_finished();
                }

                // Get parent directory path and file name
                let parent = relative
                    .parent()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default();
                let name = relative
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();

                dir_contents.entry(parent).or_default().push((name, cid));
            } else if path.is_dir() {
                // Ensure directory entry exists
                let dir_path = relative.to_string_lossy().to_string();
                dir_contents.entry(dir_path).or_default();
            }
        }

        // Build directory tree bottom-up
        self.build_directory_tree(tree, &mut dir_contents).await
    }

    async fn build_directory_tree<S: Store>(
        &self,
        tree: &HashTree<S>,
        dir_contents: &mut HashMap<String, Vec<(String, Cid)>>,
    ) -> Result<Cid> {
        // Sort directories by depth (deepest first) to build bottom-up
        let mut dirs: Vec<String> = dir_contents.keys().cloned().collect();
        dirs.sort_by(|a, b| {
            let depth_a = a.matches('/').count() + if a.is_empty() { 0 } else { 1 };
            let depth_b = b.matches('/').count() + if b.is_empty() { 0 } else { 1 };
            depth_b.cmp(&depth_a) // Deepest first
        });

        let mut dir_cids: HashMap<String, Cid> = HashMap::new();

        for dir_path in dirs {
            let files = dir_contents.get(&dir_path).cloned().unwrap_or_default();

            let mut entries: Vec<hashtree_core::DirEntry> = files
                .into_iter()
                .map(|(name, cid)| hashtree_core::DirEntry::from_cid(name, &cid))
                .collect();

            // Add subdirectory entries
            for (subdir_path, cid) in &dir_cids {
                let parent = Path::new(subdir_path)
                    .parent()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default();

                if parent == dir_path {
                    let name = Path::new(subdir_path)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    entries.push(
                        hashtree_core::DirEntry::from_cid(name, cid)
                            .with_link_type(hashtree_core::LinkType::Dir),
                    );
                }
            }

            let cid = tree
                .put_directory(entries)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to create directory node: {}", e))?;

            dir_cids.insert(dir_path, cid);
        }

        // Return root Cid
        dir_cids
            .get("")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("No root directory"))
    }

    /// Upload a file with CHK encryption, returns CID in format "hash:key"
    pub fn upload_file_encrypted<P: AsRef<Path>>(&self, file_path: P) -> Result<String> {
        self.upload_file_encrypted_with_chunk_size(file_path, None)
    }

    pub fn upload_file_encrypted_with_chunk_size<P: AsRef<Path>>(
        &self,
        file_path: P,
        chunk_size: Option<usize>,
    ) -> Result<String> {
        self.upload_file_encrypted_with_chunk_size_internal(file_path, chunk_size, None)
    }

    pub fn upload_file_encrypted_with_chunk_size_and_progress<P: AsRef<Path>>(
        &self,
        file_path: P,
        chunk_size: Option<usize>,
        progress: &AddProgress,
    ) -> Result<String> {
        self.upload_file_encrypted_with_chunk_size_internal(file_path, chunk_size, Some(progress))
    }

    fn upload_file_encrypted_with_chunk_size_internal<P: AsRef<Path>>(
        &self,
        file_path: P,
        chunk_size: Option<usize>,
        progress: Option<&AddProgress>,
    ) -> Result<String> {
        let file_path = file_path.as_ref();
        let file = std::fs::File::open(file_path)
            .with_context(|| format!("Failed to open file {}", file_path.display()))?;
        if let Some(progress) = progress {
            if let Ok(metadata) = file.metadata() {
                progress.record_file_discovered(metadata.len());
            }
        }

        // Use unified API with encryption enabled (default), streaming from disk.
        let store = self.store_arc();
        let mut config = HashTreeConfig::new(store);
        if let Some(chunk_size) = chunk_size {
            config = config.with_chunk_size(chunk_size);
        }
        let tree = HashTree::new(config);

        let (cid, _size) = sync_block_on(async {
            tree.put_stream_with_progress(AllowStdIo::new(file), |bytes| {
                if let Some(progress) = progress {
                    progress.record_bytes(bytes);
                }
            })
            .await
        })
        .map_err(|e| anyhow::anyhow!("Failed to encrypt file: {}", e))?;
        if let Some(progress) = progress {
            progress.record_file_finished();
        }

        let cid_str = cid.to_string();

        let mut wtxn = self.env.write_txn()?;
        self.pins.put(&mut wtxn, cid.hash.as_slice(), &())?;
        wtxn.commit()?;

        Ok(cid_str)
    }

    /// Upload a directory with CHK encryption, returns CID
    /// Respects .gitignore and ignores common OS junk files by default.
    pub fn upload_dir_encrypted<P: AsRef<Path>>(&self, dir_path: P) -> Result<String> {
        self.upload_dir_encrypted_with_options(dir_path, true)
    }

    /// Upload a directory with CHK encryption and options
    /// Returns CID as "hash:key" format for encrypted directories
    pub fn upload_dir_encrypted_with_options<P: AsRef<Path>>(
        &self,
        dir_path: P,
        respect_gitignore: bool,
    ) -> Result<String> {
        self.upload_dir_encrypted_with_options_and_chunk_size(dir_path, respect_gitignore, None)
    }

    pub fn upload_dir_encrypted_with_options_and_chunk_size<P: AsRef<Path>>(
        &self,
        dir_path: P,
        respect_gitignore: bool,
        chunk_size: Option<usize>,
    ) -> Result<String> {
        self.upload_dir_encrypted_with_options_and_chunk_size_internal(
            dir_path,
            respect_gitignore,
            chunk_size,
            None,
        )
    }

    pub fn upload_dir_encrypted_with_options_and_chunk_size_and_progress<P: AsRef<Path>>(
        &self,
        dir_path: P,
        respect_gitignore: bool,
        chunk_size: Option<usize>,
        progress: &AddProgress,
    ) -> Result<String> {
        self.upload_dir_encrypted_with_options_and_chunk_size_internal(
            dir_path,
            respect_gitignore,
            chunk_size,
            Some(progress),
        )
    }

    fn upload_dir_encrypted_with_options_and_chunk_size_internal<P: AsRef<Path>>(
        &self,
        dir_path: P,
        respect_gitignore: bool,
        chunk_size: Option<usize>,
        progress: Option<&AddProgress>,
    ) -> Result<String> {
        let dir_path = dir_path.as_ref();
        let store = self.store_arc();

        // Use unified API with encryption enabled (default)
        let mut config = HashTreeConfig::new(store);
        if let Some(chunk_size) = chunk_size {
            config = config.with_chunk_size(chunk_size);
        }
        let tree = HashTree::new(config);

        let root_cid = sync_block_on(async {
            self.upload_dir_recursive(&tree, dir_path, dir_path, respect_gitignore, progress)
                .await
        })
        .context("Failed to upload encrypted directory")?;

        let cid_str = root_cid.to_string(); // Returns "hash:key" or "hash"

        let mut wtxn = self.env.write_txn()?;
        // Pin by hash only (the key is for decryption, not identification)
        self.pins.put(&mut wtxn, root_cid.hash.as_slice(), &())?;
        wtxn.commit()?;

        Ok(cid_str)
    }
}
