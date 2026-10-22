use crate::{
    config::{validate_component, validate_path, MAX_FILES},
    Error, Result, MAX_READ_BYTES,
};
use async_trait::async_trait;
use hashtree_core::{sha256, Cid, Hash, HashTree, HashTreeConfig, Store, StoreError};
use serde::Serialize;
use std::{
    collections::{HashSet, VecDeque},
    sync::Arc,
};
use tokio::sync::Mutex;

/// Public file information contains neither CIDs nor decryption capabilities.
#[derive(Clone, Debug, Serialize)]
pub struct File {
    pub index: usize,
    pub path: String,
    pub size: u64,
}

#[async_trait]
// async-trait adds must_use to a method returning an already must-use Future.
#[allow(clippy::double_must_use)]
pub(crate) trait Source: Send + Sync {
    async fn read(&self, file: usize, offset: u64, len: usize) -> Result<Vec<u8>>;
}

#[derive(Clone)]
pub struct Snapshot {
    pub(crate) source: Arc<dyn Source>,
    files: Arc<Vec<File>>,
}

struct TreeSource<S: Store> {
    tree: HashTree<VerifiedStore<S>>,
    files: Vec<(Cid, u64)>,
    windows: Mutex<VecDeque<ReadWindow>>,
}

// Peer requests are normally 16 KiB. Reuse a few immutable 2 MiB windows
// instead of reading, hashing and decrypting the same hashtree chunk 128 times.
const WINDOW_BYTES: u64 = 2 * 1024 * 1024;
const MAX_WINDOWS: usize = 4;
struct ReadWindow {
    file: usize,
    start: u64,
    bytes: Vec<u8>,
}

#[async_trait]
impl<S: Store + 'static> Source for TreeSource<S> {
    async fn read(&self, file: usize, offset: u64, len: usize) -> Result<Vec<u8>> {
        let (cid, size) = self
            .files
            .get(file)
            .ok_or_else(|| Error::Invalid("unknown file".into()))?;
        let end = offset
            .checked_add(len as u64)
            .ok_or_else(|| Error::Invalid("range overflow".into()))?;
        if end > *size || len > MAX_READ_BYTES {
            return Err(Error::Invalid("range exceeds file or read limit".into()));
        }
        if len == 0 {
            return Ok(Vec::new());
        }
        let mut windows = self.windows.lock().await;
        let mut position = offset;
        let mut result = Vec::with_capacity(len);
        while position < end {
            let start = position / WINDOW_BYTES * WINDOW_BYTES;
            let window_end = start.saturating_add(WINDOW_BYTES).min(*size);
            let entry = if let Some(index) = windows
                .iter()
                .position(|w| w.file == file && w.start == start)
            {
                windows.remove(index).expect("existing window")
            } else {
                let bytes = self
                    .tree
                    .read_file_range_cid(cid, start, Some(window_end))
                    .await?
                    .ok_or(Error::Content)?;
                if bytes.len() as u64 != window_end - start {
                    return Err(Error::Content);
                }
                ReadWindow { file, start, bytes }
            };
            let take_until = end.min(window_end);
            result.extend_from_slice(
                &entry.bytes[(position - start) as usize..(take_until - start) as usize],
            );
            windows.push_front(entry);
            windows.truncate(MAX_WINDOWS);
            position = take_until;
        }
        Ok(result)
    }
}

impl Snapshot {
    /// Resolve an immutable snapshot. Encrypted parents, children and file chunks
    /// all require permission; a public directory cannot bypass this check.
    pub async fn new<S: Store + 'static>(
        store: Arc<S>,
        root: Cid,
        name: &str,
        allow_plaintext: bool,
    ) -> Result<Self> {
        validate_component(name)?;
        let verified = Arc::new(VerifiedStore(store));
        let tree = HashTree::new(HashTreeConfig::new(verified.clone()));
        let mut pending = vec![(root, name.to_owned(), 0usize)];
        let mut files = Vec::new();
        while let Some((cid, path, depth)) = pending.pop() {
            if depth > 128 || files.len() + pending.len() >= MAX_FILES {
                return Err(Error::Invalid("snapshot exceeds file/depth limit".into()));
            }
            if cid.key.is_some() && !allow_plaintext {
                return Err(Error::PlaintextPermission);
            }
            if verified.get(&cid.hash).await?.is_none() {
                return Err(Error::Content);
            }
            validate_path(&path)?;
            if tree.is_dir(&cid).await? {
                let entries = tree.list_directory_required(&cid).await?;
                let mut names = HashSet::new();
                for entry in entries {
                    validate_component(&entry.name)?;
                    if !names.insert(entry.name.clone()) {
                        return Err(Error::Invalid("duplicate directory entry".into()));
                    }
                    pending.push((
                        Cid {
                            hash: entry.hash,
                            key: entry.key,
                        },
                        format!("{path}/{}", entry.name),
                        depth + 1,
                    ));
                }
            } else {
                if !allow_plaintext {
                    check_public_file(&tree, &cid).await?;
                }
                let size = tree.get_size_cid(&cid).await?;
                files.push((path, cid, size));
            }
        }
        files.sort_by(|a, b| a.0.cmp(&b.0));
        if files.is_empty() {
            return Err(Error::Invalid(
                "BitTorrent snapshots need at least one file".into(),
            ));
        }
        let public_files = files
            .iter()
            .enumerate()
            .map(|(index, (path, _, size))| File {
                index,
                path: path.clone(),
                size: *size,
            })
            .collect();
        let source = TreeSource {
            tree,
            windows: Mutex::new(VecDeque::new()),
            files: files
                .into_iter()
                .map(|(_, cid, size)| (cid, size))
                .collect(),
        };
        Ok(Self {
            source: Arc::new(source),
            files: Arc::new(public_files),
        })
    }

    pub fn files(&self) -> &[File] {
        &self.files
    }

    pub async fn read(&self, file: usize, offset: u64, len: usize) -> Result<Vec<u8>> {
        self.source.read(file, offset, len).await
    }
}

async fn check_public_file<S: Store>(tree: &HashTree<S>, root: &Cid) -> Result<()> {
    let mut pending = vec![(root.clone(), 0)];
    let mut seen = HashSet::new();
    while let Some((cid, depth)) = pending.pop() {
        if cid.key.is_some() {
            return Err(Error::PlaintextPermission);
        }
        if depth > 128 {
            return Err(Error::Invalid("file tree is too deep".into()));
        }
        if !seen.insert(cid.hash) {
            continue;
        }
        if let Some(node) = tree.get_node(&cid).await? {
            for link in node.links {
                if link.key.is_some() {
                    return Err(Error::PlaintextPermission);
                }
                if link.link_type.is_tree() {
                    pending.push((
                        Cid {
                            hash: link.hash,
                            key: link.key,
                        },
                        depth + 1,
                    ));
                }
            }
        }
    }
    Ok(())
}

struct VerifiedStore<S>(Arc<S>);

#[async_trait]
impl<S: Store> Store for VerifiedStore<S> {
    async fn get(&self, hash: &Hash) -> std::result::Result<Option<Vec<u8>>, StoreError> {
        let data = self.0.get(hash).await?;
        if data.as_ref().is_some_and(|data| sha256(data) != *hash) {
            return Err(StoreError::Other(
                "snapshot blob failed SHA-256 verification".into(),
            ));
        }
        Ok(data)
    }
    async fn put(&self, _: Hash, _: Vec<u8>) -> std::result::Result<bool, StoreError> {
        Err(StoreError::Other("snapshot is read-only".into()))
    }
    async fn delete(&self, _: &Hash) -> std::result::Result<bool, StoreError> {
        Err(StoreError::Other("snapshot is read-only".into()))
    }
    async fn has(&self, hash: &Hash) -> std::result::Result<bool, StoreError> {
        Ok(self.get(hash).await?.is_some())
    }
}
