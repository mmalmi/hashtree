//! Hashtree-backed git object and ref storage with configurable persistence
//!
//! Stores git objects and refs in a hashtree merkle tree:
//!   root/
//!     .git/
//!       HEAD -> "ref: refs/heads/main"
//!       refs/heads/main -> <commit-sha1>
//!       info/refs -> dumb-HTTP ref advertisement
//!       objects/XX/YYYY... -> zlib-compressed loose object (standard git layout)
//!       objects/info/packs -> standard Git pack list
//!
//! The root hash (SHA-256) is the content-addressed identifier for the entire repo state.

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use hashtree_config::{Config, StorageBackend};
use hashtree_core::store::{Store, StoreError, StoreStats};
use hashtree_core::types::Hash;
use hashtree_core::{Cid, DirEntry, HashTree, HashTreeConfig, LinkType};
use hashtree_fs::FsBlobStore;
#[cfg(feature = "lmdb")]
use hashtree_lmdb::LmdbBlobStore;
use sha1::{Digest, Sha1};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;
use tokio::runtime::{Handle, Runtime};
use tracing::{debug, info, warn};

use super::object::{parse_tree, GitObject, ObjectId, ObjectType};
use super::progress::{RepoTreeBuildPhase, RepoTreeBuildProgress};
use super::refs::{validate_ref_name, Ref};
use super::{Error, Result};

/// Box type for async recursion
type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

#[derive(Default)]
struct RefDirectory {
    files: BTreeMap<String, String>,
    dirs: BTreeMap<String, RefDirectory>,
}

impl RefDirectory {
    fn insert(&mut self, parts: &[&str], value: String) {
        let Some((name, rest)) = parts.split_first() else {
            return;
        };

        if rest.is_empty() {
            self.files.insert((*name).to_string(), value);
        } else {
            self.dirs
                .entry((*name).to_string())
                .or_default()
                .insert(rest, value);
        }
    }
}

/// Runtime executor - either owns a runtime or reuses an existing one
enum RuntimeExecutor {
    Owned(Runtime),
    Handle(Handle),
}

impl RuntimeExecutor {
    fn block_on<F: std::future::Future>(&self, f: F) -> F::Output {
        match self {
            RuntimeExecutor::Owned(rt) => rt.block_on(f),
            RuntimeExecutor::Handle(handle) => tokio::task::block_in_place(|| handle.block_on(f)),
        }
    }
}

/// Local blob store - wraps either FsBlobStore or LmdbBlobStore
pub enum LocalStore {
    Fs(FsBlobStore),
    #[cfg(feature = "lmdb")]
    Lmdb(LmdbBlobStore),
}

impl LocalStore {
    pub(crate) fn new_for_backend<P: AsRef<Path>>(
        path: P,
        backend: StorageBackend,
        max_bytes: u64,
    ) -> std::result::Result<Self, StoreError> {
        let path = path.as_ref();
        #[cfg(feature = "lmdb")]
        {
            return Self::new_for_backend_with_openers(
                path,
                backend,
                max_bytes,
                Self::open_fs_store,
                Self::open_lmdb_store,
            );
        }

        #[cfg(not(feature = "lmdb"))]
        match backend {
            StorageBackend::Fs => Self::open_fs_store(path, max_bytes),
            #[cfg(not(feature = "lmdb"))]
            StorageBackend::Lmdb => {
                warn!(
                    "LMDB backend requested but lmdb feature not enabled, using filesystem storage"
                );
                Self::open_fs_store(path, max_bytes)
            }
        }
    }

    fn open_fs_store(path: &Path, max_bytes: u64) -> std::result::Result<Self, StoreError> {
        if max_bytes > 0 {
            Ok(LocalStore::Fs(FsBlobStore::with_max_bytes(
                path, max_bytes,
            )?))
        } else {
            Ok(LocalStore::Fs(FsBlobStore::new(path)?))
        }
    }

    #[cfg(feature = "lmdb")]
    fn open_lmdb_store(path: &Path, max_bytes: u64) -> std::result::Result<Self, StoreError> {
        if max_bytes > 0 {
            Ok(LocalStore::Lmdb(LmdbBlobStore::with_max_bytes(
                path, max_bytes,
            )?))
        } else {
            Ok(LocalStore::Lmdb(LmdbBlobStore::new(path)?))
        }
    }

    #[cfg(feature = "lmdb")]
    fn new_for_backend_with_openers<FS, LMDB>(
        path: &Path,
        backend: StorageBackend,
        max_bytes: u64,
        fs_open: FS,
        lmdb_open: LMDB,
    ) -> std::result::Result<Self, StoreError>
    where
        FS: Fn(&Path, u64) -> std::result::Result<Self, StoreError>,
        LMDB: Fn(&Path, u64) -> std::result::Result<Self, StoreError>,
    {
        match backend {
            StorageBackend::Fs => fs_open(path, max_bytes),
            StorageBackend::Lmdb => match lmdb_open(path, max_bytes) {
                Ok(store) => Ok(store),
                Err(err) if should_fallback_from_lmdb_error(&err) => {
                    warn!(
                        path = %path.display(),
                        "LMDB backend is unsupported in this environment, falling back to filesystem storage"
                    );
                    fs_open(path, max_bytes)
                }
                Err(err) => Err(err),
            },
        }
    }

    /// Create a new local store based on config
    pub fn new<P: AsRef<Path>>(path: P) -> std::result::Result<Self, StoreError> {
        Self::new_with_max_bytes(path, 0)
    }

    /// Create a new local store based on config with an optional byte limit.
    pub fn new_with_max_bytes<P: AsRef<Path>>(
        path: P,
        max_bytes: u64,
    ) -> std::result::Result<Self, StoreError> {
        let config = Config::load_or_default();
        Self::new_for_backend(path, config.storage.backend, max_bytes)
    }

    /// List all hashes in the store
    pub fn list(&self) -> std::result::Result<Vec<Hash>, StoreError> {
        match self {
            LocalStore::Fs(store) => store.list(),
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.list(),
        }
    }

    /// Sync get operation
    pub fn get_sync(&self, hash: &Hash) -> std::result::Result<Option<Vec<u8>>, StoreError> {
        match self {
            LocalStore::Fs(store) => store.get_sync(hash),
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.get_sync(hash),
        }
    }
}

#[cfg(feature = "lmdb")]
pub(crate) fn should_fallback_from_lmdb_error(error: &StoreError) -> bool {
    matches!(
        error,
        StoreError::Io(io_error) if io_error.raw_os_error() == Some(libc::ENOSYS)
    )
}

#[async_trait::async_trait]
impl Store for LocalStore {
    async fn put(&self, hash: Hash, data: Vec<u8>) -> std::result::Result<bool, StoreError> {
        match self {
            LocalStore::Fs(store) => store.put(hash, data).await,
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.put(hash, data).await,
        }
    }

    async fn get(&self, hash: &Hash) -> std::result::Result<Option<Vec<u8>>, StoreError> {
        match self {
            LocalStore::Fs(store) => store.get(hash).await,
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.get(hash).await,
        }
    }

    async fn has(&self, hash: &Hash) -> std::result::Result<bool, StoreError> {
        match self {
            LocalStore::Fs(store) => store.has(hash).await,
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.has(hash).await,
        }
    }

    async fn delete(&self, hash: &Hash) -> std::result::Result<bool, StoreError> {
        match self {
            LocalStore::Fs(store) => store.delete(hash).await,
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.delete(hash).await,
        }
    }

    fn set_max_bytes(&self, max: u64) {
        match self {
            LocalStore::Fs(store) => store.set_max_bytes(max),
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.set_max_bytes(max),
        }
    }

    fn max_bytes(&self) -> Option<u64> {
        match self {
            LocalStore::Fs(store) => store.max_bytes(),
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.max_bytes(),
        }
    }

    async fn stats(&self) -> StoreStats {
        match self {
            LocalStore::Fs(store) => match store.stats() {
                Ok(stats) => StoreStats {
                    count: stats.count as u64,
                    bytes: stats.total_bytes,
                    pinned_count: stats.pinned_count as u64,
                    pinned_bytes: stats.pinned_bytes,
                },
                Err(_) => StoreStats::default(),
            },
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => match store.stats() {
                Ok(stats) => StoreStats {
                    count: stats.count as u64,
                    bytes: stats.total_bytes,
                    pinned_count: 0,
                    pinned_bytes: 0,
                },
                Err(_) => StoreStats::default(),
            },
        }
    }

    async fn evict_if_needed(&self) -> std::result::Result<u64, StoreError> {
        match self {
            LocalStore::Fs(store) => store.evict_if_needed().await,
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.evict_if_needed().await,
        }
    }
}

/// Git storage backed by HashTree with configurable persistence
pub struct GitStorage {
    store: Arc<LocalStore>,
    tree: HashTree<LocalStore>,
    runtime: RuntimeExecutor,
    /// In-memory state for the current session
    objects: std::sync::RwLock<HashMap<String, Vec<u8>>>,
    refs: std::sync::RwLock<HashMap<String, String>>,
    pack_files: std::sync::RwLock<BTreeMap<String, Vec<u8>>>,
    packed_object_ids: std::sync::RwLock<HashSet<String>>,
    /// Cached root CID (hash + encryption key)
    root_cid: std::sync::RwLock<Option<Cid>>,
}

impl GitStorage {
    /// Open or create a git storage at the given path
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let config = Config::load_or_default();
        let max_size_bytes = config
            .storage
            .max_size_gb
            .saturating_mul(1024 * 1024 * 1024);
        Self::open_with_max_bytes(path, max_size_bytes)
    }

    /// Open or create a git storage at the given path with an explicit byte limit.
    pub fn open_with_max_bytes(path: impl AsRef<Path>, max_size_bytes: u64) -> Result<Self> {
        let config = Config::load_or_default();
        Self::open_with_backend_and_max_bytes(path, config.storage.backend, max_size_bytes)
    }

    pub fn open_with_backend_and_max_bytes(
        path: impl AsRef<Path>,
        backend: StorageBackend,
        max_size_bytes: u64,
    ) -> Result<Self> {
        let runtime = match Handle::try_current() {
            Ok(handle) => RuntimeExecutor::Handle(handle),
            Err(_) => {
                let rt = Runtime::new()
                    .map_err(|e| Error::StorageError(format!("tokio runtime: {}", e)))?;
                RuntimeExecutor::Owned(rt)
            }
        };

        let store_path = path.as_ref().join("blobs");
        let store = Arc::new(
            LocalStore::new_for_backend(&store_path, backend, max_size_bytes)
                .map_err(|e| Error::StorageError(format!("local store: {}", e)))?,
        );

        // Use encrypted mode (default) - blossom servers require encrypted data
        let tree = HashTree::new(HashTreeConfig::new(store.clone()));

        Ok(Self {
            store,
            tree,
            runtime,
            objects: std::sync::RwLock::new(HashMap::new()),
            refs: std::sync::RwLock::new(HashMap::new()),
            pack_files: std::sync::RwLock::new(BTreeMap::new()),
            packed_object_ids: std::sync::RwLock::new(HashSet::new()),
            root_cid: std::sync::RwLock::new(None),
        })
    }

    /// Evict old local blobs if storage is over the configured limit.
    pub fn evict_if_needed(&self) -> Result<u64> {
        self.runtime
            .block_on(self.store.evict_if_needed())
            .map_err(|e| Error::StorageError(format!("evict: {}", e)))
    }

    /// Write an object, returning its ID
    fn write_object(&self, obj: &GitObject) -> Result<ObjectId> {
        let oid = obj.id();
        let key = oid.to_hex();

        let loose = obj.to_loose_format();
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&loose)?;
        let compressed = encoder.finish()?;

        let mut objects = self
            .objects
            .write()
            .map_err(|e| Error::StorageError(format!("lock: {}", e)))?;
        objects.insert(key, compressed);

        // Invalidate cached root
        if let Ok(mut root) = self.root_cid.write() {
            *root = None;
        }

        Ok(oid)
    }

    /// Write raw object data (type + content already parsed)
    pub fn write_raw_object(&self, obj_type: ObjectType, content: &[u8]) -> Result<ObjectId> {
        let obj = GitObject::new(obj_type, content.to_vec());
        self.write_object(&obj)
    }

    /// Read an object by ID from in-memory cache
    #[allow(dead_code)]
    fn read_object(&self, oid: &ObjectId) -> Result<GitObject> {
        let key = oid.to_hex();
        let objects = self
            .objects
            .read()
            .map_err(|e| Error::StorageError(format!("lock: {}", e)))?;
        let compressed = objects
            .get(&key)
            .ok_or_else(|| Error::ObjectNotFound(key.clone()))?;

        let mut decoder = ZlibDecoder::new(compressed.as_slice());
        let mut data = Vec::new();
        decoder.read_to_end(&mut data)?;

        GitObject::from_loose_format(&data)
    }

    /// Write a ref
    pub fn write_ref(&self, name: &str, target: &Ref) -> Result<()> {
        validate_ref_name(name)?;

        let value = match target {
            Ref::Direct(oid) => oid.to_hex(),
            Ref::Symbolic(target) => format!("ref: {}", target),
        };

        let mut refs = self
            .refs
            .write()
            .map_err(|e| Error::StorageError(format!("lock: {}", e)))?;
        refs.insert(name.to_string(), value);

        // Invalidate cached root
        if let Ok(mut root) = self.root_cid.write() {
            *root = None;
        }

        Ok(())
    }

    /// Read a ref
    #[allow(dead_code)]
    pub fn read_ref(&self, name: &str) -> Result<Option<Ref>> {
        let refs = self
            .refs
            .read()
            .map_err(|e| Error::StorageError(format!("lock: {}", e)))?;

        match refs.get(name) {
            Some(value) => {
                if let Some(target) = value.strip_prefix("ref: ") {
                    Ok(Some(Ref::Symbolic(target.to_string())))
                } else {
                    let oid = ObjectId::from_hex(value)
                        .ok_or_else(|| Error::StorageError(format!("invalid ref: {}", value)))?;
                    Ok(Some(Ref::Direct(oid)))
                }
            }
            None => Ok(None),
        }
    }

    /// List all refs
    #[allow(dead_code)]
    pub fn list_refs(&self) -> Result<HashMap<String, String>> {
        let refs = self
            .refs
            .read()
            .map_err(|e| Error::StorageError(format!("lock: {}", e)))?;
        Ok(refs.clone())
    }

    /// Delete a ref
    pub fn delete_ref(&self, name: &str) -> Result<bool> {
        let mut refs = self
            .refs
            .write()
            .map_err(|e| Error::StorageError(format!("lock: {}", e)))?;
        let existed = refs.remove(name).is_some();

        // Invalidate cached root
        if let Ok(mut root) = self.root_cid.write() {
            *root = None;
        }

        Ok(existed)
    }

    /// Import a raw git object (already in loose format, zlib compressed)
    /// Used when fetching existing objects from remote before push
    pub fn import_compressed_object(&self, oid: &str, compressed_data: Vec<u8>) -> Result<()> {
        let mut objects = self
            .objects
            .write()
            .map_err(|e| Error::StorageError(format!("lock: {}", e)))?;
        objects.insert(oid.to_string(), compressed_data);

        // Invalidate cached root
        if let Ok(mut root) = self.root_cid.write() {
            *root = None;
        }

        Ok(())
    }

    /// Import a ref directly (used when loading existing refs from remote)
    pub fn import_ref(&self, name: &str, value: &str) -> Result<()> {
        let mut refs = self
            .refs
            .write()
            .map_err(|e| Error::StorageError(format!("lock: {}", e)))?;
        refs.insert(name.to_string(), value.to_string());

        // Invalidate cached root
        if let Ok(mut root) = self.root_cid.write() {
            *root = None;
        }

        Ok(())
    }

    pub fn set_pack_files(&self, files: BTreeMap<String, Vec<u8>>) -> Result<()> {
        self.set_pack_checkpoint_files(files, HashSet::new())
    }

    pub fn set_pack_checkpoint_files(
        &self,
        files: BTreeMap<String, Vec<u8>>,
        covered_objects: HashSet<String>,
    ) -> Result<()> {
        let mut pack_files = self
            .pack_files
            .write()
            .map_err(|e| Error::StorageError(format!("lock: {}", e)))?;
        *pack_files = files;
        let mut packed_object_ids = self
            .packed_object_ids
            .write()
            .map_err(|e| Error::StorageError(format!("lock: {}", e)))?;
        *packed_object_ids = covered_objects;

        if let Ok(mut root) = self.root_cid.write() {
            *root = None;
        }

        Ok(())
    }

    pub fn add_pack_covered_objects(&self, covered_objects: HashSet<String>) -> Result<()> {
        let mut packed_object_ids = self
            .packed_object_ids
            .write()
            .map_err(|e| Error::StorageError(format!("lock: {}", e)))?;
        packed_object_ids.extend(covered_objects);

        if let Ok(mut root) = self.root_cid.write() {
            *root = None;
        }

        Ok(())
    }

    /// Check if a ref exists
    #[cfg(test)]
    pub fn has_ref(&self, name: &str) -> Result<bool> {
        let refs = self
            .refs
            .read()
            .map_err(|e| Error::StorageError(format!("lock: {}", e)))?;
        Ok(refs.contains_key(name))
    }

    /// Get count of objects in storage
    #[cfg(test)]
    pub fn object_count(&self) -> Result<usize> {
        let objects = self
            .objects
            .read()
            .map_err(|e| Error::StorageError(format!("lock: {}", e)))?;
        Ok(objects.len())
    }

    /// Get the cached root CID (returns None if tree hasn't been built)
    #[allow(dead_code)]
    pub fn get_root_cid(&self) -> Result<Option<Cid>> {
        let root = self
            .root_cid
            .read()
            .map_err(|e| Error::StorageError(format!("lock: {}", e)))?;
        Ok(root.clone())
    }

    /// Get the default branch name
    #[allow(dead_code)]
    pub fn default_branch(&self) -> Result<Option<String>> {
        let refs = self
            .refs
            .read()
            .map_err(|e| Error::StorageError(format!("lock: {}", e)))?;

        if let Some(head) = refs.get("HEAD") {
            if let Some(target) = head.strip_prefix("ref: ") {
                return Ok(Some(target.to_string()));
            }
        }
        Ok(None)
    }

    /// Get the tree SHA from a commit object
    fn get_commit_tree(
        &self,
        commit_oid: &str,
        objects: &HashMap<String, Vec<u8>>,
    ) -> Option<String> {
        let compressed = objects.get(commit_oid)?;

        // Decompress the object
        let mut decoder = ZlibDecoder::new(&compressed[..]);
        let mut decompressed = Vec::new();
        decoder.read_to_end(&mut decompressed).ok()?;

        // Parse git object format: "type size\0content"
        let null_pos = decompressed.iter().position(|&b| b == 0)?;
        let content = &decompressed[null_pos + 1..];

        // Parse commit content - first line is "tree <sha>"
        let content_str = std::str::from_utf8(content).ok()?;
        let first_line = content_str.lines().next()?;
        first_line
            .strip_prefix("tree ")
            .map(|tree_hash| tree_hash.to_string())
    }

    /// Get git object content (decompressed, without header)
    fn get_object_content(
        &self,
        oid: &str,
        objects: &HashMap<String, Vec<u8>>,
    ) -> Option<(ObjectType, Vec<u8>)> {
        let compressed = objects.get(oid)?;
        Self::parse_compressed_object(compressed)
    }

    fn parse_compressed_object(compressed: &[u8]) -> Option<(ObjectType, Vec<u8>)> {
        let mut decoder = ZlibDecoder::new(compressed);
        let mut decompressed = Vec::new();
        decoder.read_to_end(&mut decompressed).ok()?;

        // Parse git object format: "type size\0content"
        let null_pos = decompressed.iter().position(|&b| b == 0)?;
        let header = std::str::from_utf8(&decompressed[..null_pos]).ok()?;
        let obj_type = if header.starts_with("blob") {
            ObjectType::Blob
        } else if header.starts_with("tree") {
            ObjectType::Tree
        } else if header.starts_with("commit") {
            ObjectType::Commit
        } else if header.starts_with("tag") {
            ObjectType::Tag
        } else {
            return None;
        };
        let content = decompressed[null_pos + 1..].to_vec();
        Some((obj_type, content))
    }

    async fn load_base_compressed_object<S: Store>(
        &self,
        oid: &str,
        base_tree: &HashTree<S>,
        base_objects_cid: &Cid,
    ) -> Result<Option<Vec<u8>>> {
        if oid.len() != 40 {
            return Err(Error::ObjectNotFound(oid.to_string()));
        }

        let path = format!("{}/{}", &oid[..2], &oid[2..]);
        let Some(object_cid) = base_tree
            .resolve_path(base_objects_cid, &path)
            .await
            .map_err(|e| Error::StorageError(format!("resolve {} in base objects: {}", oid, e)))?
        else {
            return Ok(None);
        };

        base_tree
            .get(&object_cid, None)
            .await
            .map_err(|e| Error::StorageError(format!("read {} from base objects: {}", oid, e)))
    }

    async fn get_object_content_from_base<S: Store>(
        &self,
        oid: &str,
        base_tree: &HashTree<S>,
        base_objects_cid: &Cid,
    ) -> Result<Option<(ObjectType, Vec<u8>)>> {
        let Some(compressed) = self
            .load_base_compressed_object(oid, base_tree, base_objects_cid)
            .await?
        else {
            return Ok(None);
        };

        Ok(Self::parse_compressed_object(&compressed))
    }

    async fn get_tree_entries_from_sources<S: Store>(
        &self,
        tree_oid: &str,
        objects: &HashMap<String, Vec<u8>>,
        base_tree: Option<&HashTree<S>>,
        base_objects_cid: Option<&Cid>,
    ) -> Result<Vec<super::object::TreeEntry>> {
        let object = match self.get_object_content(tree_oid, objects) {
            Some(object) => Some(object),
            None => {
                if let (Some(base_tree), Some(base_objects_cid)) = (base_tree, base_objects_cid) {
                    self.get_object_content_from_base(tree_oid, base_tree, base_objects_cid)
                        .await?
                } else {
                    None
                }
            }
        }
        .ok_or_else(|| Error::ObjectNotFound(tree_oid.to_string()))?;

        if object.0 != ObjectType::Tree {
            return Err(Error::InvalidObjectType(format!(
                "expected tree, got {:?}",
                object.0
            )));
        }

        parse_tree(&object.1)
    }

    fn tree_entry_to_dir_entry(entry: &hashtree_core::TreeEntry) -> DirEntry {
        DirEntry::from_cid(
            &entry.name,
            &Cid {
                hash: entry.hash,
                key: entry.key,
            },
        )
        .with_size(entry.size)
        .with_link_type(entry.link_type)
    }

    async fn import_object_from_base<S: Store>(
        &self,
        oid: &str,
        objects: &mut HashMap<String, Vec<u8>>,
        base_tree: &HashTree<S>,
        base_objects_cid: &Cid,
    ) -> Result<bool> {
        if objects.contains_key(oid) {
            return Ok(true);
        }
        let Some(compressed) = self
            .load_base_compressed_object(oid, base_tree, base_objects_cid)
            .await?
        else {
            return Ok(false);
        };

        objects.insert(oid.to_string(), compressed);
        Ok(true)
    }

    fn seed_missing_object_from_base_boxed<'a, S: Store + 'a>(
        &'a self,
        oid: &'a str,
        objects: &'a mut HashMap<String, Vec<u8>>,
        base_tree: &'a HashTree<S>,
        base_objects_cid: &'a Cid,
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(self.import_object_from_base(oid, objects, base_tree, base_objects_cid))
    }

    fn peel_tag_target(&self, oid: &str, objects: &HashMap<String, Vec<u8>>) -> Option<String> {
        let (obj_type, content) = self.get_object_content(oid, objects)?;
        if obj_type != ObjectType::Tag {
            return Some(oid.to_string());
        }

        let target = std::str::from_utf8(&content)
            .ok()?
            .lines()
            .find_map(|line| line.strip_prefix("object "))
            .map(str::trim)?
            .to_string();

        match self.get_object_content(&target, objects)?.0 {
            ObjectType::Tag => self.peel_tag_target(&target, objects),
            _ => Some(target),
        }
    }

    fn build_info_refs_content(
        &self,
        refs: &HashMap<String, String>,
        objects: &HashMap<String, Vec<u8>>,
    ) -> String {
        let mut lines = Vec::new();

        for (name, value) in refs {
            if name == "HEAD" {
                continue;
            }

            let oid = value.trim().to_string();
            lines.push((name.clone(), oid.clone()));

            if name.starts_with("refs/tags/") {
                if let Some(peeled) = self.peel_tag_target(&oid, objects) {
                    if peeled != oid {
                        lines.push((format!("{}^{{}}", name), peeled));
                    }
                }
            }
        }

        lines.sort_by(|a, b| a.0.cmp(&b.0));

        let mut content = String::new();
        for (name, oid) in lines {
            content.push_str(&oid);
            content.push('\t');
            content.push_str(&name);
            content.push('\n');
        }
        content
    }

    async fn build_info_dir(
        &self,
        refs: &HashMap<String, String>,
        objects: &HashMap<String, Vec<u8>>,
    ) -> Result<Cid> {
        let info_refs = self.build_info_refs_content(refs, objects);
        let (info_refs_cid, info_refs_size) = self
            .tree
            .put(info_refs.as_bytes())
            .await
            .map_err(|e| Error::StorageError(format!("put info/refs: {}", e)))?;

        self.tree
            .put_directory(vec![
                DirEntry::from_cid("refs", &info_refs_cid).with_size(info_refs_size)
            ])
            .await
            .map_err(|e| Error::StorageError(format!("put info dir: {}", e)))
    }

    /// Build the hashtree and return the root CID (hash + encryption key)
    pub fn build_tree(&self) -> Result<Cid> {
        self.build_tree_with_base_objects::<LocalStore>(None, None, None)
    }

    pub fn build_tree_with_progress(&self, progress: &RepoTreeBuildProgress) -> Result<Cid> {
        self.build_tree_with_base_objects_internal::<LocalStore>(None, None, None, Some(progress))
    }

    fn is_safe_pack_name(name: &str) -> bool {
        name.len() == "pack-".len() + 40 + ".pack".len()
            && name.starts_with("pack-")
            && name.ends_with(".pack")
            && name["pack-".len()..name.len() - ".pack".len()]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
    }

    pub fn tree_root_has_git_pack_checkpoint<S: Store>(
        &self,
        tree: &HashTree<S>,
        root_cid: &Cid,
    ) -> Result<bool> {
        let Some(info_packs_cid) = self
            .runtime
            .block_on(tree.resolve_path(root_cid, ".git/objects/info/packs"))
            .map_err(|e| Error::StorageError(format!("resolve .git/objects/info/packs: {}", e)))?
        else {
            return Ok(false);
        };

        let Some(info_packs_bytes) = self
            .runtime
            .block_on(tree.get(&info_packs_cid, None))
            .map_err(|e| Error::StorageError(format!("read .git/objects/info/packs: {}", e)))?
        else {
            return Ok(false);
        };

        let info_packs = String::from_utf8_lossy(&info_packs_bytes);
        for line in info_packs.lines().map(str::trim) {
            let Some(pack_name) = line.strip_prefix("P ") else {
                continue;
            };
            if !Self::is_safe_pack_name(pack_name) {
                continue;
            }

            let idx_name = format!("{}.idx", pack_name.trim_end_matches(".pack"));
            let pack_path = format!(".git/objects/pack/{pack_name}");
            let idx_path = format!(".git/objects/pack/{idx_name}");
            let pack_exists = self
                .runtime
                .block_on(tree.resolve_path(root_cid, &pack_path))
                .map_err(|e| Error::StorageError(format!("resolve {}: {}", pack_path, e)))?
                .is_some();
            let idx_exists = self
                .runtime
                .block_on(tree.resolve_path(root_cid, &idx_path))
                .map_err(|e| Error::StorageError(format!("resolve {}: {}", idx_path, e)))?
                .is_some();
            if pack_exists && idx_exists {
                return Ok(true);
            }
        }

        Ok(false)
    }

    fn root_has_git_pack_checkpoint(&self, root_cid: &Cid) -> Result<bool> {
        self.tree_root_has_git_pack_checkpoint(&self.tree, root_cid)
    }

    pub fn validate_root_contains_direct_refs(&self, root_cid: &Cid) -> Result<()> {
        let direct_refs: Vec<String> = {
            let refs = self
                .refs
                .read()
                .map_err(|e| Error::StorageError(format!("lock: {}", e)))?;
            refs.values()
                .filter(|value| {
                    !value.starts_with("ref: ")
                        && value.len() == 40
                        && value.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
                .cloned()
                .collect()
        };

        if direct_refs.is_empty() {
            return Ok(());
        }

        let objects_dir = self
            .runtime
            .block_on(self.tree.resolve_path(root_cid, ".git/objects"))
            .map_err(|e| Error::StorageError(format!("resolve .git/objects: {}", e)))?;
        if objects_dir.is_none() {
            return Err(Error::StorageError(
                "built root is missing .git/objects".to_string(),
            ));
        }

        let pack_checkpoint_available = self.root_has_git_pack_checkpoint(root_cid)?;
        for oid in direct_refs {
            let object_path = format!(".git/objects/{}/{}", &oid[..2], &oid[2..]);
            let object_cid = self
                .runtime
                .block_on(self.tree.resolve_path(root_cid, &object_path))
                .map_err(|e| Error::StorageError(format!("resolve {}: {}", object_path, e)))?;
            if object_cid.is_none() && !pack_checkpoint_available {
                return Err(Error::ObjectNotFound(oid));
            }
        }

        Ok(())
    }

    pub fn build_tree_with_base_objects<S: Store>(
        &self,
        base_tree: Option<&HashTree<S>>,
        base_root: Option<&Cid>,
        base_tree_sha: Option<&str>,
    ) -> Result<Cid> {
        self.build_tree_with_base_objects_internal(base_tree, base_root, base_tree_sha, None)
    }

    pub fn build_tree_with_base_objects_with_progress<S: Store>(
        &self,
        base_tree: Option<&HashTree<S>>,
        base_root: Option<&Cid>,
        base_tree_sha: Option<&str>,
        progress: &RepoTreeBuildProgress,
    ) -> Result<Cid> {
        self.build_tree_with_base_objects_internal(
            base_tree,
            base_root,
            base_tree_sha,
            Some(progress),
        )
    }

    fn build_tree_with_base_objects_internal<S: Store>(
        &self,
        base_tree: Option<&HashTree<S>>,
        base_root: Option<&Cid>,
        base_tree_sha: Option<&str>,
        progress: Option<&RepoTreeBuildProgress>,
    ) -> Result<Cid> {
        // Check if we have a cached root
        if let Ok(root) = self.root_cid.read() {
            if let Some(ref cid) = *root {
                if let Some(progress) = progress {
                    progress.mark_done();
                }
                return Ok(cid.clone());
            }
        }

        if let Err(err) = self.evict_if_needed() {
            debug!("pre-build eviction skipped: {}", err);
        }

        let objects = self
            .objects
            .read()
            .map_err(|e| Error::StorageError(format!("lock: {}", e)))?;
        let refs = self
            .refs
            .read()
            .map_err(|e| Error::StorageError(format!("lock: {}", e)))?;

        // Get default branch from HEAD or find first branch ref
        let (default_branch, commit_sha) = if let Some(head) = refs.get("HEAD") {
            let branch = head.strip_prefix("ref: ").map(String::from);
            let sha = branch.as_ref().and_then(|b| refs.get(b)).cloned();
            (branch, sha)
        } else {
            // No HEAD ref - find first refs/heads/* ref directly
            let mut branch_info: Option<(String, String)> = None;
            for (ref_name, sha) in refs.iter() {
                if ref_name.starts_with("refs/heads/") {
                    branch_info = Some((ref_name.clone(), sha.clone()));
                    break;
                }
            }
            match branch_info {
                Some((branch, sha)) => (Some(branch), Some(sha)),
                None => (None, None),
            }
        };

        // Get tree SHA from commit
        let tree_sha = commit_sha
            .as_ref()
            .and_then(|sha| self.get_commit_tree(sha, &objects));

        // Clone objects for async block
        let mut objects_clone = objects.clone();

        let base_objects_cid = if let (Some(base_tree), Some(base_root)) = (base_tree, base_root) {
            self.runtime
                .block_on(base_tree.resolve_path(base_root, ".git/objects"))
                .map_err(|e| Error::StorageError(format!("resolve base .git/objects: {}", e)))?
        } else {
            None
        };

        let base_tree_sha = base_tree_sha.map(str::to_string);

        let base_root_entries = if let (Some(base_tree), Some(base_root)) = (base_tree, base_root) {
            Some(
                self.runtime
                    .block_on(base_tree.list_directory(base_root))
                    .map_err(|e| Error::StorageError(format!("list base root: {}", e)))?,
            )
        } else {
            None
        };

        let root_cid = loop {
            let build_result = self.runtime.block_on(async {
                // Build objects directory
                let objects_cid = self
                    .build_objects_dir_with_base(
                        &objects_clone,
                        base_tree,
                        base_objects_cid.as_ref(),
                        progress,
                    )
                    .await?;

                // Build refs directory
                if let Some(progress) = progress {
                    progress.start_phase(RepoTreeBuildPhase::Refs, Some(refs.len()));
                }
                let refs_cid = self.build_refs_dir(&refs, progress).await?;

                // Build dumb-HTTP info directory
                if let Some(progress) = progress {
                    progress.start_phase(RepoTreeBuildPhase::Info, Some(1));
                }
                let info_cid = self.build_info_dir(&refs, &objects_clone).await?;
                if let Some(progress) = progress {
                    progress.increment_processed();
                }

                // Build HEAD file - use default_branch if no explicit HEAD
                // Git expects HEAD to end with newline, so add it if missing
                if let Some(progress) = progress {
                    progress.start_phase(RepoTreeBuildPhase::Head, Some(1));
                }
                let head_content = refs.get("HEAD")
                    .map(|h| if h.ends_with('\n') { h.clone() } else { format!("{}\n", h) })
                    .or_else(|| default_branch.as_ref().map(|b| format!("ref: {}\n", b)))
                    .unwrap_or_else(|| "ref: refs/heads/main\n".to_string());
                debug!("HEAD content: {:?}", head_content);
                let (head_cid, head_size) = self.tree.put(head_content.as_bytes()).await
                    .map_err(|e| Error::StorageError(format!("put HEAD: {}", e)))?;
                debug!("HEAD hash: {}", hex::encode(head_cid.hash));
                if let Some(progress) = progress {
                    progress.increment_processed();
                }

                // Build .git directory - use from_cid to preserve encryption keys
                let mut git_entries = vec![
                    DirEntry::from_cid("HEAD", &head_cid).with_size(head_size),
                    DirEntry::from_cid("info", &info_cid).with_link_type(LinkType::Dir),
                    DirEntry::from_cid("objects", &objects_cid).with_link_type(LinkType::Dir),
                    DirEntry::from_cid("refs", &refs_cid).with_link_type(LinkType::Dir),
                ];

                // Add config if we have a default branch
                if let Some(ref branch) = default_branch {
                    let config = format!(
                        "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = true\n[init]\n\tdefaultBranch = {}\n",
                        branch.trim_start_matches("refs/heads/")
                    );
                    let (config_cid, config_size) = self.tree.put(config.as_bytes()).await
                        .map_err(|e| Error::StorageError(format!("put config: {}", e)))?;
                    git_entries.push(DirEntry::from_cid("config", &config_cid).with_size(config_size));
                }

                // Build and add index file if we have a tree SHA
                if let Some(ref tree_oid) = tree_sha {
                    if let Some(progress) = progress {
                        progress.start_phase(RepoTreeBuildPhase::Index, None);
                    }
                    let index_result = if let (Some(base_tree), Some(base_objects_cid), Some(base_tree_sha)) =
                        (base_tree, base_objects_cid.as_ref(), base_tree_sha.as_deref())
                    {
                        self.build_index_file_with_base(
                            tree_oid,
                            &objects_clone,
                            base_tree,
                            base_objects_cid,
                            base_tree_sha,
                            base_root_entries.as_ref(),
                            progress,
                        )
                        .await
                    } else {
                        self.build_index_file(tree_oid, &objects_clone, progress)
                    };
                    match index_result {
                        Ok(index_data) => {
                            let (index_cid, index_size) = self.tree.put(&index_data).await
                                .map_err(|e| Error::StorageError(format!("put index: {}", e)))?;
                            git_entries.push(DirEntry::from_cid("index", &index_cid).with_size(index_size));
                            info!("Added git index file ({} bytes)", index_data.len());
                        }
                        Err(e) => {
                            debug!("Failed to build git index file: {} - continuing without index", e);
                        }
                    }
                }

                if let Some(progress) = progress {
                    progress.start_phase(RepoTreeBuildPhase::GitDir, Some(1));
                }
                let git_cid = self.tree.put_directory(git_entries).await
                    .map_err(|e| Error::StorageError(format!("put .git: {}", e)))?;
                if let Some(progress) = progress {
                    progress.increment_processed();
                }

                // Build root entries starting with .git
                // Use from_cid to preserve the encryption key
                let mut root_entries = vec![DirEntry::from_cid(".git", &git_cid).with_link_type(LinkType::Dir)];

                // Add working tree files if we have a tree SHA
                if let Some(ref tree_oid) = tree_sha {
                    if let Some(progress) = progress {
                        progress.start_phase(RepoTreeBuildPhase::WorkingTree, None);
                    }
                    let working_tree_entries = if let (Some(base_tree), Some(base_objects_cid), Some(base_tree_sha)) =
                        (base_tree, base_objects_cid.as_ref(), base_tree_sha.as_deref())
                    {
                        self.build_working_tree_entries_with_base(
                            tree_oid,
                            &objects_clone,
                            base_tree,
                            base_objects_cid,
                            base_tree_sha,
                            base_root_entries.as_ref(),
                            progress,
                        )
                        .await?
                    } else {
                        self.build_working_tree_entries(tree_oid, &objects_clone, progress).await?
                    };
                    root_entries.extend(working_tree_entries);
                    info!("Added {} working tree entries to root", root_entries.len() - 1);
                }

                // Sort entries for deterministic ordering
                root_entries.sort_by(|a, b| a.name.cmp(&b.name));

                if let Some(progress) = progress {
                    progress.start_phase(RepoTreeBuildPhase::Root, Some(1));
                }
                let root_cid = self.tree.put_directory(root_entries).await
                    .map_err(|e| Error::StorageError(format!("put root: {}", e)))?;
                if let Some(progress) = progress {
                    progress.increment_processed();
                }

                info!("Built hashtree root: {} (encrypted: {}) (.git dir: {})",
                    hex::encode(root_cid.hash),
                    root_cid.key.is_some(),
                    hex::encode(git_cid.hash));

                Ok::<Cid, Error>(root_cid)
            });

            match build_result {
                Ok(root_cid) => break root_cid,
                Err(Error::ObjectNotFound(oid))
                    if base_tree.is_some() && base_objects_cid.is_some() =>
                {
                    let imported =
                        self.runtime
                            .block_on(self.seed_missing_object_from_base_boxed(
                                &oid,
                                &mut objects_clone,
                                base_tree.expect("checked is_some"),
                                base_objects_cid.as_ref().expect("checked is_some"),
                            ))?;
                    if imported {
                        continue;
                    }
                    return Err(Error::ObjectNotFound(oid).into());
                }
                Err(err) => return Err(err.into()),
            }
        };

        // Cache the root CID
        if let Ok(mut root) = self.root_cid.write() {
            *root = Some(root_cid.clone());
        }
        if let Some(progress) = progress {
            progress.mark_done();
        }

        Ok(root_cid)
    }

    /// Build working tree entries from a git tree object
    async fn build_working_tree_entries(
        &self,
        tree_oid: &str,
        objects: &HashMap<String, Vec<u8>>,
        progress: Option<&RepoTreeBuildProgress>,
    ) -> Result<Vec<DirEntry>> {
        let mut entries = Vec::new();

        // Get tree content
        let (obj_type, content) = self
            .get_object_content(tree_oid, objects)
            .ok_or_else(|| Error::ObjectNotFound(tree_oid.to_string()))?;

        if obj_type != ObjectType::Tree {
            return Err(Error::InvalidObjectType(format!(
                "expected tree, got {:?}",
                obj_type
            )));
        }

        // Parse tree entries
        let tree_entries = parse_tree(&content)?;

        for entry in tree_entries {
            let oid_hex = entry.oid.to_hex();

            if entry.is_tree() {
                // Recursively build subdirectory
                let sub_entries = self
                    .build_working_tree_entries_boxed(&oid_hex, objects, progress)
                    .await?;

                // Create subdirectory in hashtree
                let dir_cid =
                    self.tree.put_directory(sub_entries).await.map_err(|e| {
                        Error::StorageError(format!("put dir {}: {}", entry.name, e))
                    })?;

                // Use from_cid to preserve encryption key
                entries
                    .push(DirEntry::from_cid(&entry.name, &dir_cid).with_link_type(LinkType::Dir));
                if let Some(progress) = progress {
                    progress.record_working_dir(false);
                }
            } else {
                // Get blob content
                if let Some((ObjectType::Blob, blob_content)) =
                    self.get_object_content(&oid_hex, objects)
                {
                    // Use put() instead of put_blob() to chunk large files
                    let (cid, size) = self.tree.put(&blob_content).await.map_err(|e| {
                        Error::StorageError(format!("put blob {}: {}", entry.name, e))
                    })?;

                    // Use from_cid to preserve encryption key
                    entries.push(DirEntry::from_cid(&entry.name, &cid).with_size(size));
                    if let Some(progress) = progress {
                        progress.record_working_file(false);
                    }
                }
            }
        }

        // Sort for deterministic ordering
        entries.sort_by(|a, b| a.name.cmp(&b.name));

        Ok(entries)
    }

    /// Boxed version for async recursion
    fn build_working_tree_entries_boxed<'a>(
        &'a self,
        tree_oid: &'a str,
        objects: &'a HashMap<String, Vec<u8>>,
        progress: Option<&'a RepoTreeBuildProgress>,
    ) -> BoxFuture<'a, Result<Vec<DirEntry>>> {
        Box::pin(self.build_working_tree_entries(tree_oid, objects, progress))
    }

    fn build_working_tree_entries_with_base_recursive_boxed<'a, S: Store + 'a>(
        &'a self,
        tree_oid: &'a str,
        objects: &'a HashMap<String, Vec<u8>>,
        base_tree: &'a HashTree<S>,
        base_objects_cid: &'a Cid,
        old_tree_oid: Option<&'a str>,
        old_dir_entries: Option<&'a Vec<hashtree_core::TreeEntry>>,
        progress: Option<&'a RepoTreeBuildProgress>,
    ) -> BoxFuture<'a, Result<Vec<DirEntry>>> {
        Box::pin(async move {
            let tree_entries = self
                .get_tree_entries_from_sources(
                    tree_oid,
                    objects,
                    Some(base_tree),
                    Some(base_objects_cid),
                )
                .await?;
            let old_tree_entries = if let Some(old_tree_oid) = old_tree_oid {
                match self
                    .get_tree_entries_from_sources(
                        old_tree_oid,
                        objects,
                        Some(base_tree),
                        Some(base_objects_cid),
                    )
                    .await
                {
                    Ok(entries) => Some(entries),
                    Err(Error::ObjectNotFound(_)) if old_dir_entries.is_some() => None,
                    Err(err) => return Err(err),
                }
            } else {
                None
            };
            let old_tree_available = old_tree_entries.is_some();

            let mut old_tree_entry_map = old_tree_entries
                .unwrap_or_default()
                .into_iter()
                .map(|entry| (entry.name.clone(), entry))
                .collect::<HashMap<_, _>>();
            let mut old_dir_entry_map = old_dir_entries
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|entry| (entry.name.clone(), entry))
                .collect::<HashMap<_, _>>();

            let mut entries = Vec::new();
            for entry in tree_entries {
                let oid_hex = entry.oid.to_hex();
                let old_tree_entry = old_tree_entry_map.remove(&entry.name);
                let old_dir_entry = old_dir_entry_map.remove(&entry.name);

                if entry.is_tree() {
                    if let (Some(old_tree_entry), Some(old_dir_entry)) =
                        (old_tree_entry.as_ref(), old_dir_entry.as_ref())
                    {
                        if old_tree_entry.is_tree()
                            && old_tree_entry.oid == entry.oid
                            && old_dir_entry.link_type == LinkType::Dir
                        {
                            entries.push(Self::tree_entry_to_dir_entry(old_dir_entry));
                            if let Some(progress) = progress {
                                progress.record_working_dir(true);
                            }
                            continue;
                        }
                    }

                    let old_subtree_oid = old_tree_entry
                        .as_ref()
                        .filter(|old| old.is_tree())
                        .map(|old| old.oid.to_hex());
                    let old_subdir_entries = if let Some(old_dir_entry) = old_dir_entry.as_ref() {
                        if old_dir_entry.link_type == LinkType::Dir {
                            Some(
                                base_tree
                                    .list_directory(&Cid {
                                        hash: old_dir_entry.hash,
                                        key: old_dir_entry.key,
                                    })
                                    .await
                                    .map_err(|e| {
                                        Error::StorageError(format!(
                                            "list base working dir {}: {}",
                                            entry.name, e
                                        ))
                                    })?,
                            )
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    let sub_entries = self
                        .build_working_tree_entries_with_base_recursive_boxed(
                            &oid_hex,
                            objects,
                            base_tree,
                            base_objects_cid,
                            old_subtree_oid.as_deref(),
                            old_subdir_entries.as_ref(),
                            progress,
                        )
                        .await?;
                    let dir_cid = self.tree.put_directory(sub_entries).await.map_err(|e| {
                        Error::StorageError(format!("put dir {}: {}", entry.name, e))
                    })?;
                    entries.push(
                        DirEntry::from_cid(&entry.name, &dir_cid).with_link_type(LinkType::Dir),
                    );
                    if let Some(progress) = progress {
                        progress.record_working_dir(false);
                    }
                    continue;
                }

                if let (Some(old_tree_entry), Some(old_dir_entry)) =
                    (old_tree_entry.as_ref(), old_dir_entry.as_ref())
                {
                    if !old_tree_entry.is_tree()
                        && old_tree_entry.oid == entry.oid
                        && old_dir_entry.link_type != LinkType::Dir
                    {
                        entries.push(Self::tree_entry_to_dir_entry(old_dir_entry));
                        if let Some(progress) = progress {
                            progress.record_working_file(true);
                        }
                        continue;
                    }
                }

                let blob_content = match self.get_object_content(&oid_hex, objects) {
                    Some((ObjectType::Blob, blob_content)) => blob_content,
                    Some((obj_type, _)) => {
                        return Err(Error::InvalidObjectType(format!(
                            "expected blob, got {:?}",
                            obj_type
                        )));
                    }
                    None if !old_tree_available => {
                        if let Some(old_dir_entry) = old_dir_entry.as_ref() {
                            if old_dir_entry.link_type != LinkType::Dir {
                                entries.push(Self::tree_entry_to_dir_entry(old_dir_entry));
                                if let Some(progress) = progress {
                                    progress.record_working_file(true);
                                }
                                continue;
                            }
                        }
                        return Err(Error::ObjectNotFound(oid_hex));
                    }
                    None => match self
                        .get_object_content_from_base(&oid_hex, base_tree, base_objects_cid)
                        .await?
                    {
                        Some((ObjectType::Blob, blob_content)) => blob_content,
                        Some((obj_type, _)) => {
                            return Err(Error::InvalidObjectType(format!(
                                "expected blob, got {:?}",
                                obj_type
                            )));
                        }
                        None => return Err(Error::ObjectNotFound(oid_hex)),
                    },
                };

                let (cid, size) =
                    self.tree.put(&blob_content).await.map_err(|e| {
                        Error::StorageError(format!("put blob {}: {}", entry.name, e))
                    })?;
                entries.push(DirEntry::from_cid(&entry.name, &cid).with_size(size));
                if let Some(progress) = progress {
                    progress.record_working_file(false);
                }
            }

            entries.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(entries)
        })
    }

    async fn build_working_tree_entries_with_base<S: Store>(
        &self,
        tree_oid: &str,
        objects: &HashMap<String, Vec<u8>>,
        base_tree: &HashTree<S>,
        base_objects_cid: &Cid,
        base_tree_oid: &str,
        base_root_entries: Option<&Vec<hashtree_core::TreeEntry>>,
        progress: Option<&RepoTreeBuildProgress>,
    ) -> Result<Vec<DirEntry>> {
        let root_entries = base_root_entries.map(|entries| {
            entries
                .iter()
                .filter(|entry| entry.name != ".git")
                .cloned()
                .collect::<Vec<_>>()
        });

        self.build_working_tree_entries_with_base_recursive_boxed(
            tree_oid,
            objects,
            base_tree,
            base_objects_cid,
            Some(base_tree_oid),
            root_entries.as_ref(),
            progress,
        )
        .await
    }

    async fn build_objects_prefix_dir(
        &self,
        prefix: &str,
        old_entries: Option<Vec<hashtree_core::TreeEntry>>,
        new_objects: &[(String, Vec<u8>)],
        progress: Option<&RepoTreeBuildProgress>,
    ) -> Result<Cid> {
        let mut sub_entries: BTreeMap<String, DirEntry> = old_entries
            .unwrap_or_default()
            .into_iter()
            .map(|entry| (entry.name.clone(), Self::tree_entry_to_dir_entry(&entry)))
            .collect();

        for (suffix, data) in new_objects {
            let (cid, size) = self.tree.put(data).await.map_err(|e| {
                Error::StorageError(format!("put object {}{}: {}", prefix, suffix, e))
            })?;
            sub_entries.insert(
                suffix.clone(),
                DirEntry::from_cid(suffix, &cid).with_size(size),
            );
            if let Some(progress) = progress {
                progress.record_object_blob();
            }
        }

        let cid = self
            .tree
            .put_directory(sub_entries.into_values().collect())
            .await
            .map_err(|e| Error::StorageError(format!("put objects/{}: {}", prefix, e)))?;
        if let Some(progress) = progress {
            progress.record_object_prefix();
        }
        Ok(cid)
    }

    /// Build the objects directory using HashTree, reusing unchanged prefix directories
    /// from an older root when available.
    async fn build_objects_dir_with_base<S: Store>(
        &self,
        objects: &HashMap<String, Vec<u8>>,
        base_tree: Option<&HashTree<S>>,
        base_objects_cid: Option<&Cid>,
        progress: Option<&RepoTreeBuildProgress>,
    ) -> Result<Cid> {
        if let Some(progress) = progress {
            progress.start_phase(RepoTreeBuildPhase::Objects, Some(objects.len()));
        }

        let pack_files = self
            .pack_files
            .read()
            .map_err(|e| Error::StorageError(format!("lock: {}", e)))?
            .clone();
        let packed_object_ids = self
            .packed_object_ids
            .read()
            .map_err(|e| Error::StorageError(format!("lock: {}", e)))?
            .clone();
        let packs_replace_loose_objects =
            !packed_object_ids.is_empty() && (!pack_files.is_empty() || base_objects_cid.is_some());
        let mut buckets: BTreeMap<String, Vec<(String, Vec<u8>)>> = BTreeMap::new();
        for (oid, data) in objects {
            if packs_replace_loose_objects && packed_object_ids.contains(oid) {
                continue;
            }
            let prefix = &oid[..2];
            let suffix = &oid[2..];
            buckets
                .entry(prefix.to_string())
                .or_default()
                .push((suffix.to_string(), data.clone()));
        }

        let mut top_entries: BTreeMap<String, DirEntry> = BTreeMap::new();
        let mut merged_prefixes = std::collections::HashSet::new();
        let mut inherited_pack_entries: BTreeMap<String, DirEntry> = BTreeMap::new();
        let empty_objects: Vec<(String, Vec<u8>)> = Vec::new();

        if let (Some(base_tree), Some(base_objects_cid)) = (base_tree, base_objects_cid) {
            for entry in base_tree
                .list_directory(base_objects_cid)
                .await
                .map_err(|e| Error::StorageError(format!("list base objects dir: {}", e)))?
            {
                if entry.name == "info" || entry.name == "pack" {
                    if pack_files.is_empty() {
                        top_entries
                            .insert(entry.name.clone(), Self::tree_entry_to_dir_entry(&entry));
                    } else if entry.name == "pack" && entry.link_type == LinkType::Dir {
                        let pack_cid = Cid {
                            hash: entry.hash,
                            key: entry.key,
                        };
                        for pack_entry in
                            base_tree.list_directory(&pack_cid).await.map_err(|e| {
                                Error::StorageError(format!("list base objects/pack dir: {}", e))
                            })?
                        {
                            inherited_pack_entries.insert(
                                pack_entry.name.clone(),
                                Self::tree_entry_to_dir_entry(&pack_entry),
                            );
                        }
                    }
                    continue;
                }

                let Some(delta_objects) = buckets.get(&entry.name) else {
                    if !packs_replace_loose_objects {
                        top_entries
                            .insert(entry.name.clone(), Self::tree_entry_to_dir_entry(&entry));
                        continue;
                    }

                    if entry.link_type != LinkType::Dir || entry.name.len() != 2 {
                        top_entries
                            .insert(entry.name.clone(), Self::tree_entry_to_dir_entry(&entry));
                        continue;
                    }

                    let prefix_cid = Cid {
                        hash: entry.hash,
                        key: entry.key,
                    };
                    let old_prefix_entries = base_tree
                        .list_directory(&prefix_cid)
                        .await
                        .map_err(|e| {
                            Error::StorageError(format!(
                                "list base objects/{} dir: {}",
                                entry.name, e
                            ))
                        })?
                        .into_iter()
                        .filter(|old_entry| {
                            !packed_object_ids
                                .contains(&format!("{}{}", entry.name, old_entry.name))
                        })
                        .collect::<Vec<_>>();
                    if old_prefix_entries.is_empty() {
                        continue;
                    }

                    let merged_cid = self
                        .build_objects_prefix_dir(
                            &entry.name,
                            Some(old_prefix_entries),
                            &empty_objects,
                            progress,
                        )
                        .await?;
                    top_entries.insert(
                        entry.name.clone(),
                        DirEntry::from_cid(&entry.name, &merged_cid).with_link_type(LinkType::Dir),
                    );
                    merged_prefixes.insert(entry.name);
                    continue;
                };

                let prefix_cid = Cid {
                    hash: entry.hash,
                    key: entry.key,
                };
                let old_prefix_entries = if entry.link_type == LinkType::Dir {
                    let mut entries = base_tree.list_directory(&prefix_cid).await.map_err(|e| {
                        Error::StorageError(format!("list base objects/{} dir: {}", entry.name, e))
                    })?;
                    if packs_replace_loose_objects {
                        entries.retain(|old_entry| {
                            !packed_object_ids
                                .contains(&format!("{}{}", entry.name, old_entry.name))
                        });
                    }
                    Some(entries)
                } else {
                    None
                };
                let merged_cid = self
                    .build_objects_prefix_dir(
                        &entry.name,
                        old_prefix_entries,
                        delta_objects,
                        progress,
                    )
                    .await?;
                top_entries.insert(
                    entry.name.clone(),
                    DirEntry::from_cid(&entry.name, &merged_cid).with_link_type(LinkType::Dir),
                );
                merged_prefixes.insert(entry.name);
            }
        }

        for (prefix, objs) in &buckets {
            if merged_prefixes.contains(prefix) {
                continue;
            }
            let sub_cid = self
                .build_objects_prefix_dir(prefix, None, objs, progress)
                .await?;
            top_entries.insert(
                prefix.clone(),
                DirEntry::from_cid(prefix, &sub_cid).with_link_type(LinkType::Dir),
            );
        }

        if !pack_files.is_empty() {
            let mut pack_entries_by_name = inherited_pack_entries;
            for (name, data) in pack_files {
                let (cid, size) = self.tree.put(&data).await.map_err(|e| {
                    Error::StorageError(format!("put objects/pack/{}: {}", name, e))
                })?;
                pack_entries_by_name.insert(
                    name.clone(),
                    DirEntry::from_cid(&name, &cid).with_size(size),
                );
            }
            let mut pack_names_for_info_packs = pack_entries_by_name
                .keys()
                .filter(|name| name.ends_with(".pack"))
                .cloned()
                .collect::<Vec<_>>();
            pack_names_for_info_packs.sort();

            let pack_dir_cid = self
                .tree
                .put_directory(pack_entries_by_name.into_values().collect())
                .await
                .map_err(|e| Error::StorageError(format!("put objects/pack: {}", e)))?;
            top_entries.insert(
                "pack".to_string(),
                DirEntry::from_cid("pack", &pack_dir_cid).with_link_type(LinkType::Dir),
            );

            let packs_content = pack_names_for_info_packs
                .iter()
                .map(|name| format!("P {}\n", name))
                .collect::<String>();
            let (packs_cid, packs_size) = self
                .tree
                .put(packs_content.as_bytes())
                .await
                .map_err(|e| Error::StorageError(format!("put objects/info/packs: {}", e)))?;
            let info_cid = self
                .tree
                .put_directory(vec![
                    DirEntry::from_cid("packs", &packs_cid).with_size(packs_size)
                ])
                .await
                .map_err(|e| Error::StorageError(format!("put objects/info: {}", e)))?;
            top_entries.insert(
                "info".to_string(),
                DirEntry::from_cid("info", &info_cid).with_link_type(LinkType::Dir),
            );
        }

        if !top_entries.contains_key("info") {
            let (packs_cid, packs_size) = self
                .tree
                .put(b"")
                .await
                .map_err(|e| Error::StorageError(format!("put objects/info/packs: {}", e)))?;
            let info_cid = self
                .tree
                .put_directory(vec![
                    DirEntry::from_cid("packs", &packs_cid).with_size(packs_size)
                ])
                .await
                .map_err(|e| Error::StorageError(format!("put objects/info: {}", e)))?;
            top_entries.insert(
                "info".to_string(),
                DirEntry::from_cid("info", &info_cid).with_link_type(LinkType::Dir),
            );
        }

        let entry_count = top_entries.len();
        let cid = self
            .tree
            .put_directory(top_entries.into_values().collect())
            .await
            .map_err(|e| Error::StorageError(format!("put objects dir: {}", e)))?;

        debug!(
            "Built objects dir with {} entries: {}",
            entry_count,
            hex::encode(cid.hash)
        );
        Ok(cid)
    }

    /// Build the refs directory using HashTree
    async fn build_refs_dir(
        &self,
        refs: &HashMap<String, String>,
        progress: Option<&RepoTreeBuildProgress>,
    ) -> Result<Cid> {
        let mut root = RefDirectory::default();

        for (ref_name, value) in refs {
            let parts: Vec<&str> = ref_name.split('/').collect();
            if parts.len() >= 3 && parts[0] == "refs" {
                root.insert(&parts[1..], value.clone());
            }
        }

        let mut ref_entries = self
            .build_ref_entries_recursive(&root, "refs", progress)
            .await?;

        if ref_entries.is_empty() {
            // Return empty directory Cid
            let empty_cid = self
                .tree
                .put_directory(vec![])
                .await
                .map_err(|e| Error::StorageError(format!("put empty refs: {}", e)))?;
            return Ok(empty_cid);
        }

        ref_entries.sort_by(|a, b| a.name.cmp(&b.name));

        let refs_cid = self
            .tree
            .put_directory(ref_entries)
            .await
            .map_err(|e| Error::StorageError(format!("put refs dir: {}", e)))?;
        debug!("refs dir -> {}", hex::encode(refs_cid.hash));
        Ok(refs_cid)
    }

    fn build_ref_entries_recursive<'a>(
        &'a self,
        dir: &'a RefDirectory,
        prefix: &'a str,
        progress: Option<&'a RepoTreeBuildProgress>,
    ) -> BoxFuture<'a, Result<Vec<DirEntry>>> {
        Box::pin(async move {
            let mut entries = Vec::new();

            for (name, value) in &dir.files {
                let (cid, size) = self
                    .tree
                    .put(value.as_bytes())
                    .await
                    .map_err(|e| Error::StorageError(format!("put ref: {}", e)))?;
                debug!("{}/{} -> blob {}", prefix, name, hex::encode(cid.hash));
                entries.push(DirEntry::from_cid(name, &cid).with_size(size));
                if let Some(progress) = progress {
                    progress.increment_processed();
                }
            }

            for (name, child) in &dir.dirs {
                let child_prefix = format!("{prefix}/{name}");
                let child_entries = self
                    .build_ref_entries_recursive(child, &child_prefix, progress)
                    .await?;
                let child_cid =
                    self.tree.put_directory(child_entries).await.map_err(|e| {
                        Error::StorageError(format!("put {child_prefix} dir: {}", e))
                    })?;
                debug!("{} dir -> {}", child_prefix, hex::encode(child_cid.hash));
                entries.push(DirEntry::from_cid(name, &child_cid).with_link_type(LinkType::Dir));
            }

            entries.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(entries)
        })
    }

    /// Build git index file from tree entries
    /// Returns the raw binary content of the index file
    fn build_index_file(
        &self,
        tree_oid: &str,
        objects: &HashMap<String, Vec<u8>>,
        progress: Option<&RepoTreeBuildProgress>,
    ) -> Result<Vec<u8>> {
        // Collect all file entries from the tree (recursively)
        let mut entries: Vec<(String, [u8; 20], u32, u32)> = Vec::new(); // (path, sha1, mode, size)
        self.collect_tree_entries_for_index(tree_oid, objects, "", &mut entries, progress)?;

        self.build_index_bytes(entries)
    }

    async fn build_index_file_with_base<S: Store>(
        &self,
        tree_oid: &str,
        objects: &HashMap<String, Vec<u8>>,
        base_tree: &HashTree<S>,
        base_objects_cid: &Cid,
        base_tree_oid: &str,
        base_root_entries: Option<&Vec<hashtree_core::TreeEntry>>,
        progress: Option<&RepoTreeBuildProgress>,
    ) -> Result<Vec<u8>> {
        let mut entries: Vec<(String, [u8; 20], u32, u32)> = Vec::new();
        let root_entries = base_root_entries.map(|entries| {
            entries
                .iter()
                .filter(|entry| entry.name != ".git")
                .cloned()
                .collect::<Vec<_>>()
        });

        self.collect_tree_entries_for_index_with_base_boxed(
            tree_oid,
            objects,
            base_tree,
            base_objects_cid,
            Some(base_tree_oid),
            root_entries.as_ref(),
            "",
            &mut entries,
            progress,
        )
        .await?;

        self.build_index_bytes(entries)
    }

    fn build_index_bytes(&self, mut entries: Vec<(String, [u8; 20], u32, u32)>) -> Result<Vec<u8>> {
        // Sort entries by path (git index requirement)
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        let entry_count = entries.len() as u32;
        debug!("Building git index with {} entries", entry_count);

        // Build index content
        let mut index_data = Vec::new();

        // Header: DIRC + version 2 + entry count
        index_data.extend_from_slice(b"DIRC");
        index_data.extend_from_slice(&2u32.to_be_bytes()); // version 2
        index_data.extend_from_slice(&entry_count.to_be_bytes());

        // Current time for ctime/mtime (doesn't matter much for our use case)
        let now_sec = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as u32;

        for (path, sha1, mode, size) in &entries {
            let entry_start = index_data.len();

            // ctime sec, nsec
            index_data.extend_from_slice(&now_sec.to_be_bytes());
            index_data.extend_from_slice(&0u32.to_be_bytes());
            // mtime sec, nsec
            index_data.extend_from_slice(&now_sec.to_be_bytes());
            index_data.extend_from_slice(&0u32.to_be_bytes());
            // dev, ino (use 0)
            index_data.extend_from_slice(&0u32.to_be_bytes());
            index_data.extend_from_slice(&0u32.to_be_bytes());
            // mode
            index_data.extend_from_slice(&mode.to_be_bytes());
            // uid, gid (use 0)
            index_data.extend_from_slice(&0u32.to_be_bytes());
            index_data.extend_from_slice(&0u32.to_be_bytes());
            // file size
            index_data.extend_from_slice(&size.to_be_bytes());
            // SHA-1
            index_data.extend_from_slice(sha1);
            // flags: path length (max 0xFFF) in low 12 bits
            let path_len = std::cmp::min(path.len(), 0xFFF) as u16;
            index_data.extend_from_slice(&path_len.to_be_bytes());
            // path (NUL-terminated)
            index_data.extend_from_slice(path.as_bytes());
            index_data.push(0); // NUL terminator

            // Pad to 8-byte boundary relative to entry start
            let entry_len = index_data.len() - entry_start;
            let padding = (8 - (entry_len % 8)) % 8;
            index_data.extend(std::iter::repeat_n(0, padding));
        }

        // Calculate SHA-1 checksum of everything and append
        let mut hasher = Sha1::new();
        hasher.update(&index_data);
        let checksum = hasher.finalize();
        index_data.extend_from_slice(&checksum);

        debug!(
            "Built git index: {} bytes, {} entries",
            index_data.len(),
            entry_count
        );
        Ok(index_data)
    }

    /// Collect file entries from a git tree for building the index
    fn collect_tree_entries_for_index(
        &self,
        tree_oid: &str,
        objects: &HashMap<String, Vec<u8>>,
        prefix: &str,
        entries: &mut Vec<(String, [u8; 20], u32, u32)>,
        progress: Option<&RepoTreeBuildProgress>,
    ) -> Result<()> {
        let (obj_type, content) = self
            .get_object_content(tree_oid, objects)
            .ok_or_else(|| Error::ObjectNotFound(tree_oid.to_string()))?;

        if obj_type != ObjectType::Tree {
            return Err(Error::InvalidObjectType(format!(
                "expected tree, got {:?}",
                obj_type
            )));
        }

        let tree_entries = parse_tree(&content)?;

        for entry in tree_entries {
            let path = if prefix.is_empty() {
                entry.name.clone()
            } else {
                format!("{}/{}", prefix, entry.name)
            };

            let oid_hex = entry.oid.to_hex();

            if entry.is_tree() {
                // Recursively process subdirectory
                self.collect_tree_entries_for_index(&oid_hex, objects, &path, entries, progress)?;
            } else {
                // Get blob content for size and SHA-1
                if let Some((ObjectType::Blob, blob_content)) =
                    self.get_object_content(&oid_hex, objects)
                {
                    // Convert hex SHA to bytes
                    let mut sha1_bytes = [0u8; 20];
                    if let Ok(bytes) = hex::decode(&oid_hex) {
                        if bytes.len() == 20 {
                            sha1_bytes.copy_from_slice(&bytes);
                        }
                    }

                    // Mode: use entry.mode or default to regular file
                    let mode = entry.mode;
                    let size = blob_content.len() as u32;

                    entries.push((path, sha1_bytes, mode, size));
                    if let Some(progress) = progress {
                        progress.record_index_entry();
                    }
                }
            }
        }

        Ok(())
    }

    fn collect_tree_entries_for_index_with_base_boxed<'a, S: Store + 'a>(
        &'a self,
        tree_oid: &'a str,
        objects: &'a HashMap<String, Vec<u8>>,
        base_tree: &'a HashTree<S>,
        base_objects_cid: &'a Cid,
        old_tree_oid: Option<&'a str>,
        old_dir_entries: Option<&'a Vec<hashtree_core::TreeEntry>>,
        prefix: &'a str,
        entries: &'a mut Vec<(String, [u8; 20], u32, u32)>,
        progress: Option<&'a RepoTreeBuildProgress>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let tree_entries = self
                .get_tree_entries_from_sources(
                    tree_oid,
                    objects,
                    Some(base_tree),
                    Some(base_objects_cid),
                )
                .await?;
            let old_tree_entries = if let Some(old_tree_oid) = old_tree_oid {
                Some(
                    self.get_tree_entries_from_sources(
                        old_tree_oid,
                        objects,
                        Some(base_tree),
                        Some(base_objects_cid),
                    )
                    .await?,
                )
            } else {
                None
            };
            let mut old_tree_entry_map = old_tree_entries
                .unwrap_or_default()
                .into_iter()
                .map(|entry| (entry.name.clone(), entry))
                .collect::<HashMap<_, _>>();
            let mut old_dir_entry_map = old_dir_entries
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|entry| (entry.name.clone(), entry))
                .collect::<HashMap<_, _>>();

            for entry in tree_entries {
                let path = if prefix.is_empty() {
                    entry.name.clone()
                } else {
                    format!("{}/{}", prefix, entry.name)
                };
                let oid_hex = entry.oid.to_hex();
                let old_tree_entry = old_tree_entry_map.remove(&entry.name);
                let old_dir_entry = old_dir_entry_map.remove(&entry.name);

                if entry.is_tree() {
                    let old_subtree_oid = old_tree_entry
                        .as_ref()
                        .filter(|old| old.is_tree())
                        .map(|old| old.oid.to_hex());
                    let old_subdir_entries = if let Some(old_dir_entry) = old_dir_entry.as_ref() {
                        if old_dir_entry.link_type == LinkType::Dir {
                            Some(
                                base_tree
                                    .list_directory(&Cid {
                                        hash: old_dir_entry.hash,
                                        key: old_dir_entry.key,
                                    })
                                    .await
                                    .map_err(|e| {
                                        Error::StorageError(format!(
                                            "list base working dir {}: {}",
                                            path, e
                                        ))
                                    })?,
                            )
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    self.collect_tree_entries_for_index_with_base_boxed(
                        &oid_hex,
                        objects,
                        base_tree,
                        base_objects_cid,
                        old_subtree_oid.as_deref(),
                        old_subdir_entries.as_ref(),
                        &path,
                        entries,
                        progress,
                    )
                    .await?;
                    continue;
                }

                let mut sha1_bytes = [0u8; 20];
                if let Ok(bytes) = hex::decode(&oid_hex) {
                    if bytes.len() == 20 {
                        sha1_bytes.copy_from_slice(&bytes);
                    }
                }

                let size = if let (Some(old_tree_entry), Some(old_dir_entry)) =
                    (old_tree_entry.as_ref(), old_dir_entry.as_ref())
                {
                    if !old_tree_entry.is_tree()
                        && old_tree_entry.oid == entry.oid
                        && old_dir_entry.link_type != LinkType::Dir
                    {
                        old_dir_entry.size as u32
                    } else {
                        self.blob_size_from_sources(
                            &oid_hex,
                            objects,
                            Some(base_tree),
                            Some(base_objects_cid),
                        )
                        .await? as u32
                    }
                } else {
                    self.blob_size_from_sources(
                        &oid_hex,
                        objects,
                        Some(base_tree),
                        Some(base_objects_cid),
                    )
                    .await? as u32
                };

                entries.push((path, sha1_bytes, entry.mode, size));
                if let Some(progress) = progress {
                    progress.record_index_entry();
                }
            }

            Ok(())
        })
    }

    async fn blob_size_from_sources<S: Store>(
        &self,
        oid: &str,
        objects: &HashMap<String, Vec<u8>>,
        base_tree: Option<&HashTree<S>>,
        base_objects_cid: Option<&Cid>,
    ) -> Result<usize> {
        let object = match self.get_object_content(oid, objects) {
            Some(object) => Some(object),
            None => {
                if let (Some(base_tree), Some(base_objects_cid)) = (base_tree, base_objects_cid) {
                    self.get_object_content_from_base(oid, base_tree, base_objects_cid)
                        .await?
                } else {
                    None
                }
            }
        }
        .ok_or_else(|| Error::ObjectNotFound(oid.to_string()))?;

        if object.0 != ObjectType::Blob {
            return Err(Error::InvalidObjectType(format!(
                "expected blob, got {:?}",
                object.0
            )));
        }

        Ok(object.1.len())
    }

    /// Get the underlying store
    pub fn store(&self) -> &Arc<LocalStore> {
        &self.store
    }

    /// Get the HashTree for direct access
    #[allow(dead_code)]
    pub fn hashtree(&self) -> &HashTree<LocalStore> {
        &self.tree
    }

    /// Push all blobs to file servers
    #[allow(dead_code)]
    pub fn push_to_file_servers(
        &self,
        blossom: &hashtree_blossom::BlossomClient,
    ) -> Result<(usize, usize)> {
        let hashes = self
            .store
            .list()
            .map_err(|e| Error::StorageError(format!("list hashes: {}", e)))?;

        info!("Pushing {} blobs to file servers", hashes.len());

        let mut uploaded = 0;
        let mut existed = 0;

        self.runtime.block_on(async {
            for hash in &hashes {
                let hex_hash = hex::encode(hash);
                let data = match self.store.get_sync(hash) {
                    Ok(Some(d)) => d,
                    _ => continue,
                };

                match blossom.upload_if_missing(&data).await {
                    Ok((_, true)) => {
                        debug!("Uploaded {}", &hex_hash[..12]);
                        uploaded += 1;
                    }
                    Ok((_, false)) => {
                        existed += 1;
                    }
                    Err(e) => {
                        debug!("Failed to upload {}: {}", &hex_hash[..12], e);
                    }
                }
            }
        });

        info!(
            "Upload complete: {} new, {} already existed",
            uploaded, existed
        );
        Ok((uploaded, existed))
    }

    /// Clear all state (for testing or re-initialization)
    #[allow(dead_code)]
    pub fn clear(&self) -> Result<()> {
        let mut objects = self
            .objects
            .write()
            .map_err(|e| Error::StorageError(format!("lock: {}", e)))?;
        let mut refs = self
            .refs
            .write()
            .map_err(|e| Error::StorageError(format!("lock: {}", e)))?;
        let mut root = self
            .root_cid
            .write()
            .map_err(|e| Error::StorageError(format!("lock: {}", e)))?;

        objects.clear();
        refs.clear();
        *root = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashtree_core::store::Store;
    use hashtree_core::{sha256, LinkType};
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::Path;
    use std::process::{Child, Command, Stdio};
    #[cfg(feature = "lmdb")]
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    fn create_test_storage() -> (GitStorage, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let storage =
            GitStorage::open_with_backend_and_max_bytes(temp_dir.path(), StorageBackend::Fs, 0)
                .unwrap();
        (storage, temp_dir)
    }

    fn create_test_storage_with_limit(max_size_bytes: u64) -> (GitStorage, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let storage = GitStorage::open_with_backend_and_max_bytes(
            temp_dir.path(),
            StorageBackend::Fs,
            max_size_bytes,
        )
        .unwrap();
        (storage, temp_dir)
    }

    fn local_total_bytes(storage: &GitStorage) -> u64 {
        match storage.store().as_ref() {
            LocalStore::Fs(store) => store.stats().unwrap().total_bytes,
            #[cfg(feature = "lmdb")]
            LocalStore::Lmdb(store) => store.stats().unwrap().total_bytes,
        }
    }

    fn write_test_commit(storage: &GitStorage) -> ObjectId {
        let blob_oid = storage
            .write_raw_object(ObjectType::Blob, b"hello from hashtree\n")
            .unwrap();

        let mut tree_content = Vec::new();
        tree_content.extend_from_slice(b"100644 README.md\0");
        tree_content.extend_from_slice(&hex::decode(blob_oid.to_hex()).unwrap());
        let tree_oid = storage
            .write_raw_object(ObjectType::Tree, &tree_content)
            .unwrap();

        let commit_content = format!(
            "tree {}\nauthor Test User <test@example.com> 0 +0000\ncommitter Test User <test@example.com> 0 +0000\n\nInitial commit\n",
            tree_oid.to_hex()
        );
        storage
            .write_raw_object(ObjectType::Commit, commit_content.as_bytes())
            .unwrap()
    }

    fn write_root_tree(storage: &GitStorage, blobs: &[(&str, ObjectId)]) -> ObjectId {
        let mut sorted = blobs.to_vec();
        sorted.sort_by(|a, b| a.0.cmp(b.0));

        let mut tree_content = Vec::new();
        for (name, oid) in sorted {
            tree_content.extend_from_slice(format!("100644 {name}\0").as_bytes());
            tree_content.extend_from_slice(&hex::decode(oid.to_hex()).unwrap());
        }

        storage
            .write_raw_object(ObjectType::Tree, &tree_content)
            .unwrap()
    }

    fn write_commit_for_tree(storage: &GitStorage, tree_oid: ObjectId, message: &str) -> ObjectId {
        let commit_content = format!(
            "tree {}\nauthor Test User <test@example.com> 0 +0000\ncommitter Test User <test@example.com> 0 +0000\n\n{}\n",
            tree_oid.to_hex(),
            message
        );
        storage
            .write_raw_object(ObjectType::Commit, commit_content.as_bytes())
            .unwrap()
    }

    fn export_tree_to_fs<S: Store>(
        runtime: &RuntimeExecutor,
        tree: &HashTree<S>,
        cid: &Cid,
        dst: &Path,
    ) {
        std::fs::create_dir_all(dst).unwrap();
        let entries = runtime.block_on(tree.list_directory(cid)).unwrap();
        for entry in entries {
            let entry_cid = Cid {
                hash: entry.hash,
                key: entry.key,
            };
            let path = dst.join(&entry.name);
            match entry.link_type {
                LinkType::Dir => export_tree_to_fs(runtime, tree, &entry_cid, &path),
                LinkType::Blob | LinkType::File => {
                    let data = runtime
                        .block_on(tree.get(&entry_cid, None))
                        .unwrap()
                        .unwrap();
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent).unwrap();
                    }
                    std::fs::write(path, data).unwrap();
                }
            }
        }
    }

    fn spawn_http_server(root: &Path, port: u16) -> Child {
        Command::new("python3")
            .args([
                "-m",
                "http.server",
                &port.to_string(),
                "--bind",
                "127.0.0.1",
            ])
            .current_dir(root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn python http server")
    }

    fn wait_for_http_server(server: &mut Child, port: u16, path: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);

        loop {
            if let Some(status) = server.try_wait().expect("check http server status") {
                panic!("python http server exited before becoming ready: {status}");
            }

            if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
                stream
                    .set_read_timeout(Some(Duration::from_millis(200)))
                    .expect("set read timeout");
                stream
                    .set_write_timeout(Some(Duration::from_millis(200)))
                    .expect("set write timeout");
                let request =
                    format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
                if stream.write_all(request.as_bytes()).is_ok() {
                    let mut response = String::new();
                    if stream.read_to_string(&mut response).is_ok()
                        && response.starts_with("HTTP/1.0 200")
                    {
                        return;
                    }
                }
            }

            if Instant::now() >= deadline {
                panic!("python http server did not become ready on port {port}");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn test_import_ref() {
        let (storage, _temp) = create_test_storage();

        // Import a ref
        storage
            .import_ref("refs/heads/main", "abc123def456")
            .unwrap();

        // Check it exists
        assert!(storage.has_ref("refs/heads/main").unwrap());

        // Check value via list_refs
        let refs = storage.list_refs().unwrap();
        assert_eq!(
            refs.get("refs/heads/main"),
            Some(&"abc123def456".to_string())
        );
    }

    #[cfg(feature = "lmdb")]
    #[test]
    fn test_local_store_falls_back_to_fs_when_lmdb_open_returns_enosys() {
        let temp_dir = TempDir::new().unwrap();
        let fs_calls = AtomicUsize::new(0);
        let lmdb_calls = AtomicUsize::new(0);

        let store = LocalStore::new_for_backend_with_openers(
            temp_dir.path(),
            StorageBackend::Lmdb,
            0,
            |path, max_bytes| {
                fs_calls.fetch_add(1, Ordering::SeqCst);
                LocalStore::open_fs_store(path, max_bytes)
            },
            |_path, _max_bytes| {
                lmdb_calls.fetch_add(1, Ordering::SeqCst);
                Err(StoreError::Io(std::io::Error::from_raw_os_error(
                    libc::ENOSYS,
                )))
            },
        )
        .unwrap();

        assert!(matches!(store, LocalStore::Fs(_)));
        assert_eq!(lmdb_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fs_calls.load(Ordering::SeqCst), 1);
    }

    #[cfg(feature = "lmdb")]
    #[test]
    fn test_local_store_does_not_fallback_on_unrelated_lmdb_errors() {
        let temp_dir = TempDir::new().unwrap();
        let fs_calls = AtomicUsize::new(0);

        let result = LocalStore::new_for_backend_with_openers(
            temp_dir.path(),
            StorageBackend::Lmdb,
            0,
            |path, max_bytes| {
                fs_calls.fetch_add(1, Ordering::SeqCst);
                LocalStore::open_fs_store(path, max_bytes)
            },
            |_path, _max_bytes| {
                Err(StoreError::Io(std::io::Error::from_raw_os_error(
                    libc::EACCES,
                )))
            },
        );

        assert!(
            matches!(result, Err(StoreError::Io(io_error)) if io_error.raw_os_error() == Some(libc::EACCES))
        );
        assert_eq!(fs_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn test_import_multiple_refs_preserves_all() {
        let (storage, _temp) = create_test_storage();

        // Import multiple refs (simulating loading from remote)
        storage.import_ref("refs/heads/main", "sha_main").unwrap();
        storage.import_ref("refs/heads/dev", "sha_dev").unwrap();
        storage
            .import_ref("refs/heads/feature", "sha_feature")
            .unwrap();

        // All should exist
        assert!(storage.has_ref("refs/heads/main").unwrap());
        assert!(storage.has_ref("refs/heads/dev").unwrap());
        assert!(storage.has_ref("refs/heads/feature").unwrap());

        // Now write a new ref (simulating push)
        storage
            .write_ref(
                "refs/heads/new-branch",
                &Ref::Direct(
                    ObjectId::from_hex("0123456789abcdef0123456789abcdef01234567").unwrap(),
                ),
            )
            .unwrap();

        // Original refs should still exist
        let refs = storage.list_refs().unwrap();
        assert_eq!(refs.len(), 4);
        assert!(refs.contains_key("refs/heads/main"));
        assert!(refs.contains_key("refs/heads/dev"));
        assert!(refs.contains_key("refs/heads/feature"));
        assert!(refs.contains_key("refs/heads/new-branch"));
    }

    #[test]
    fn test_import_compressed_object() {
        let (storage, _temp) = create_test_storage();

        // Create a fake compressed object
        let fake_compressed = vec![0x78, 0x9c, 0x01, 0x02, 0x03]; // fake zlib data

        storage
            .import_compressed_object("abc123def456", fake_compressed.clone())
            .unwrap();

        // Check object count
        assert_eq!(storage.object_count().unwrap(), 1);
    }

    #[test]
    fn test_write_ref_overwrites_imported() {
        let (storage, _temp) = create_test_storage();

        // Import a ref
        storage.import_ref("refs/heads/main", "old_sha").unwrap();

        // Write same ref with new value
        storage
            .write_ref(
                "refs/heads/main",
                &Ref::Direct(
                    ObjectId::from_hex("0123456789abcdef0123456789abcdef01234567").unwrap(),
                ),
            )
            .unwrap();

        // Should have new value
        let refs = storage.list_refs().unwrap();
        assert_eq!(
            refs.get("refs/heads/main"),
            Some(&"0123456789abcdef0123456789abcdef01234567".to_string())
        );
    }

    #[test]
    fn test_delete_ref_preserves_others() {
        let (storage, _temp) = create_test_storage();

        // Import multiple refs
        storage.import_ref("refs/heads/main", "sha_main").unwrap();
        storage.import_ref("refs/heads/dev", "sha_dev").unwrap();

        // Delete one
        storage.delete_ref("refs/heads/dev").unwrap();

        // Other should still exist
        assert!(storage.has_ref("refs/heads/main").unwrap());
        assert!(!storage.has_ref("refs/heads/dev").unwrap());
    }

    #[test]
    fn test_clear_removes_all() {
        let (storage, _temp) = create_test_storage();

        // Import refs and objects
        storage.import_ref("refs/heads/main", "sha_main").unwrap();
        storage
            .import_compressed_object("obj1", vec![1, 2, 3])
            .unwrap();

        // Clear
        storage.clear().unwrap();

        // All gone
        assert!(!storage.has_ref("refs/heads/main").unwrap());
        assert_eq!(storage.object_count().unwrap(), 0);
    }

    #[test]
    fn test_evict_if_needed_respects_configured_limit() {
        let (storage, _temp) = create_test_storage_with_limit(1_024);

        storage
            .write_raw_object(ObjectType::Blob, &vec![b'a'; 900])
            .unwrap();
        storage
            .write_raw_object(ObjectType::Blob, &vec![b'b'; 900])
            .unwrap();
        storage
            .write_ref(
                "refs/heads/main",
                &Ref::Direct(
                    ObjectId::from_hex("0123456789abcdef0123456789abcdef01234567").unwrap(),
                ),
            )
            .unwrap();

        storage.build_tree().unwrap();

        let before = local_total_bytes(&storage);
        assert!(before > 1_024);

        let freed = storage.evict_if_needed().unwrap();
        assert!(freed > 0);

        let after = local_total_bytes(&storage);
        assert!(after <= 1_024);
    }

    #[test]
    fn test_build_tree_evicts_stale_blobs_before_writing_new_tree() {
        let max_size_bytes = 16 * 1024;
        let (storage, _temp) = create_test_storage_with_limit(max_size_bytes);

        let stale_blobs = vec![
            vec![b'x'; 7 * 1024],
            vec![b'y'; 7 * 1024],
            vec![b'z'; 7 * 1024],
        ];
        let stale_hashes: Vec<Hash> = stale_blobs.iter().map(|blob| sha256(blob)).collect();

        for (hash, blob) in stale_hashes.iter().zip(stale_blobs) {
            storage
                .runtime
                .block_on(storage.store().put(*hash, blob))
                .unwrap();
        }

        let before = local_total_bytes(&storage);
        assert!(before > max_size_bytes);

        let commit_oid = write_test_commit(&storage);
        storage
            .write_ref("refs/heads/main", &Ref::Direct(commit_oid))
            .unwrap();
        storage
            .write_ref("HEAD", &Ref::Symbolic("refs/heads/main".to_string()))
            .unwrap();

        storage.build_tree().unwrap();

        let evicted_stale = stale_hashes
            .iter()
            .filter(|hash| !storage.runtime.block_on(storage.store().has(hash)).unwrap())
            .count();

        assert!(
            evicted_stale > 0,
            "expected build_tree preflight eviction to remove stale blobs before writing"
        );
    }

    #[test]
    fn test_build_tree_progress_tracks_repo_tree_work() {
        let (storage, _temp) = create_test_storage();
        let commit_oid = write_test_commit(&storage);
        storage
            .write_ref("refs/heads/main", &Ref::Direct(commit_oid))
            .unwrap();
        storage
            .write_ref("HEAD", &Ref::Symbolic("refs/heads/main".to_string()))
            .unwrap();

        let progress = RepoTreeBuildProgress::new();
        storage.build_tree_with_progress(&progress).unwrap();

        let snapshot = progress.snapshot();
        assert_eq!(snapshot.phase, RepoTreeBuildPhase::Done);
        assert_eq!(snapshot.object_blobs, 3);
        assert_eq!(snapshot.files, 1);
        assert_eq!(snapshot.dirs, 0);
        assert_eq!(snapshot.reused, 0);
        assert_eq!(
            snapshot.format_for_label("Building repo tree"),
            "  Building repo tree: done (3 object blobs, 1 files, 0 dirs, 0 reused)"
        );
    }

    #[test]
    fn test_build_tree_adds_dumb_http_metadata() {
        let (storage, _temp) = create_test_storage();
        let commit_oid = write_test_commit(&storage);
        let tag_content = format!(
            "object {}\ntype commit\ntag v1.0.0\ntagger Test User <test@example.com> 0 +0000\n\nrelease\n",
            commit_oid.to_hex()
        );
        let tag_oid = storage
            .write_raw_object(ObjectType::Tag, tag_content.as_bytes())
            .unwrap();

        storage
            .write_ref("refs/heads/main", &Ref::Direct(commit_oid))
            .unwrap();
        storage
            .write_ref("refs/tags/v1.0.0", &Ref::Direct(tag_oid))
            .unwrap();
        storage
            .write_ref("HEAD", &Ref::Symbolic("refs/heads/main".to_string()))
            .unwrap();

        let root_cid = storage.build_tree().unwrap();

        let info_refs_cid = storage
            .runtime
            .block_on(storage.tree.resolve_path(&root_cid, ".git/info/refs"))
            .unwrap()
            .expect("info/refs exists");
        let info_refs = storage
            .runtime
            .block_on(storage.tree.get(&info_refs_cid, None))
            .unwrap()
            .unwrap();
        let info_refs = String::from_utf8(info_refs).unwrap();

        assert_eq!(
            info_refs,
            format!(
                "{commit}\trefs/heads/main\n{tag}\trefs/tags/v1.0.0\n{commit}\trefs/tags/v1.0.0^{{}}\n",
                commit = commit_oid.to_hex(),
                tag = tag_oid.to_hex()
            )
        );

        let packs_cid = storage
            .runtime
            .block_on(
                storage
                    .tree
                    .resolve_path(&root_cid, ".git/objects/info/packs"),
            )
            .unwrap()
            .expect("objects/info/packs exists");
        let packs = storage
            .runtime
            .block_on(storage.tree.get(&packs_cid, None))
            .unwrap()
            .unwrap();
        assert!(packs.is_empty(), "objects/info/packs should be empty");
    }

    #[test]
    fn test_build_tree_writes_git_objects_info_packs_for_pack_files() {
        let (storage, _temp) = create_test_storage();
        let commit_oid = write_test_commit(&storage);
        storage
            .write_ref("refs/heads/main", &Ref::Direct(commit_oid))
            .unwrap();
        storage
            .write_ref("HEAD", &Ref::Symbolic("refs/heads/main".to_string()))
            .unwrap();

        let pack_hash = "0123456789abcdef0123456789abcdef01234567";
        let pack_name = format!("pack-{pack_hash}.pack");
        let idx_name = format!("pack-{pack_hash}.idx");
        let mut pack_files = BTreeMap::new();
        pack_files.insert(pack_name.clone(), b"pack bytes".to_vec());
        pack_files.insert(idx_name.clone(), b"idx bytes".to_vec());
        storage.set_pack_files(pack_files).unwrap();

        let root_cid = storage.build_tree().unwrap();

        let packs_cid = storage
            .runtime
            .block_on(
                storage
                    .tree
                    .resolve_path(&root_cid, ".git/objects/info/packs"),
            )
            .unwrap()
            .expect("objects/info/packs exists");
        let packs = storage
            .runtime
            .block_on(storage.tree.get(&packs_cid, None))
            .unwrap()
            .unwrap();
        assert_eq!(
            String::from_utf8(packs).unwrap(),
            format!("P {pack_name}\n")
        );

        for path in [
            format!(".git/objects/pack/{pack_name}"),
            format!(".git/objects/pack/{idx_name}"),
        ] {
            let cid = storage
                .runtime
                .block_on(storage.tree.resolve_path(&root_cid, &path))
                .unwrap()
                .unwrap_or_else(|| panic!("{path} should exist"));
            let content = storage
                .runtime
                .block_on(storage.tree.get(&cid, None))
                .unwrap()
                .unwrap();
            assert!(!content.is_empty(), "{path} should have content");
        }
    }

    #[test]
    fn test_pack_checkpoint_can_replace_loose_git_objects() {
        let (storage, _temp) = create_test_storage();
        let commit_oid = write_test_commit(&storage);
        storage
            .write_ref("refs/heads/main", &Ref::Direct(commit_oid))
            .unwrap();
        storage
            .write_ref("HEAD", &Ref::Symbolic("refs/heads/main".to_string()))
            .unwrap();

        let pack_hash = "0123456789abcdef0123456789abcdef01234567";
        let pack_name = format!("pack-{pack_hash}.pack");
        let idx_name = format!("pack-{pack_hash}.idx");
        let mut pack_files = BTreeMap::new();
        pack_files.insert(pack_name.clone(), b"pack bytes".to_vec());
        pack_files.insert(idx_name, b"idx bytes".to_vec());
        storage
            .set_pack_checkpoint_files(pack_files, HashSet::from([commit_oid.to_hex()]))
            .unwrap();

        let root_cid = storage.build_tree().unwrap();
        let loose_path = format!(
            ".git/objects/{}/{}",
            &commit_oid.to_hex()[..2],
            &commit_oid.to_hex()[2..]
        );
        let loose_cid = storage
            .runtime
            .block_on(storage.tree.resolve_path(&root_cid, &loose_path))
            .unwrap();
        assert!(
            loose_cid.is_none(),
            "pack checkpoint should omit duplicated loose objects"
        );

        storage
            .validate_root_contains_direct_refs(&root_cid)
            .expect("pack checkpoint should satisfy ref validation");

        let packs_cid = storage
            .runtime
            .block_on(
                storage
                    .tree
                    .resolve_path(&root_cid, ".git/objects/info/packs"),
            )
            .unwrap()
            .expect("objects/info/packs exists");
        let packs = storage
            .runtime
            .block_on(storage.tree.get(&packs_cid, None))
            .unwrap()
            .unwrap();
        assert_eq!(
            String::from_utf8(packs).unwrap(),
            format!("P {pack_name}\n")
        );
    }

    #[test]
    fn test_inherited_pack_checkpoint_can_replace_base_loose_git_objects() {
        let (storage, _temp) = create_test_storage();
        let commit_oid = write_test_commit(&storage);
        storage
            .write_ref("refs/heads/main", &Ref::Direct(commit_oid))
            .unwrap();
        storage
            .write_ref("HEAD", &Ref::Symbolic("refs/heads/main".to_string()))
            .unwrap();

        let pack_hash = "0123456789abcdef0123456789abcdef01234567";
        let pack_name = format!("pack-{pack_hash}.pack");
        let idx_name = format!("pack-{pack_hash}.idx");
        let mut pack_files = BTreeMap::new();
        pack_files.insert(pack_name.clone(), b"pack bytes".to_vec());
        pack_files.insert(idx_name, b"idx bytes".to_vec());
        storage
            .set_pack_checkpoint_files(pack_files, HashSet::from([commit_oid.to_hex()]))
            .unwrap();
        let base_root_cid = storage.build_tree().unwrap();

        storage
            .set_pack_checkpoint_files(BTreeMap::new(), HashSet::from([commit_oid.to_hex()]))
            .unwrap();

        let root_cid = storage
            .build_tree_with_base_objects(Some(&storage.tree), Some(&base_root_cid), None)
            .unwrap();
        let loose_path = format!(
            ".git/objects/{}/{}",
            &commit_oid.to_hex()[..2],
            &commit_oid.to_hex()[2..]
        );
        let loose_cid = storage
            .runtime
            .block_on(storage.tree.resolve_path(&root_cid, &loose_path))
            .unwrap();
        assert!(
            loose_cid.is_none(),
            "inherited pack-covered objects should not be rewritten loose"
        );

        let packs_cid = storage
            .runtime
            .block_on(
                storage
                    .tree
                    .resolve_path(&root_cid, ".git/objects/info/packs"),
            )
            .unwrap()
            .expect("inherited objects/info/packs exists");
        let packs = storage
            .runtime
            .block_on(storage.tree.get(&packs_cid, None))
            .unwrap()
            .unwrap();
        assert_eq!(
            String::from_utf8(packs).unwrap(),
            format!("P {pack_name}\n")
        );
    }

    #[test]
    fn test_pack_only_base_reuses_visible_working_files_without_loose_old_tree() {
        let (storage, _temp) = create_test_storage();
        let readme_oid = storage
            .write_raw_object(ObjectType::Blob, b"base readme\n")
            .unwrap();
        let base_tree_oid = write_root_tree(&storage, &[("README.md", readme_oid)]);
        let base_commit_oid = write_commit_for_tree(&storage, base_tree_oid, "base");
        storage
            .write_ref("refs/heads/main", &Ref::Direct(base_commit_oid))
            .unwrap();
        storage
            .write_ref("HEAD", &Ref::Symbolic("refs/heads/main".to_string()))
            .unwrap();

        let pack_hash = "fedcba9876543210fedcba9876543210fedcba98";
        let pack_name = format!("pack-{pack_hash}.pack");
        let idx_name = format!("pack-{pack_hash}.idx");
        let mut pack_files = BTreeMap::new();
        pack_files.insert(pack_name, b"pack bytes".to_vec());
        pack_files.insert(idx_name, b"idx bytes".to_vec());
        storage
            .set_pack_checkpoint_files(
                pack_files,
                HashSet::from([
                    base_commit_oid.to_hex(),
                    base_tree_oid.to_hex(),
                    readme_oid.to_hex(),
                ]),
            )
            .unwrap();
        let base_root_cid = storage.build_tree().unwrap();

        let new_blob_oid = storage
            .write_raw_object(ObjectType::Blob, b"new file\n")
            .unwrap();
        let current_tree_oid = write_root_tree(
            &storage,
            &[("README.md", readme_oid), ("new.txt", new_blob_oid)],
        );
        let current_commit_oid = write_commit_for_tree(&storage, current_tree_oid, "delta");
        storage
            .write_ref("refs/heads/main", &Ref::Direct(current_commit_oid))
            .unwrap();
        storage
            .set_pack_checkpoint_files(BTreeMap::new(), HashSet::new())
            .unwrap();

        {
            let mut objects = storage.objects.write().unwrap();
            objects.remove(&base_commit_oid.to_hex());
            objects.remove(&base_tree_oid.to_hex());
            objects.remove(&readme_oid.to_hex());
        }

        let root_cid = storage
            .build_tree_with_base_objects(
                Some(&storage.tree),
                Some(&base_root_cid),
                Some(&base_tree_oid.to_hex()),
            )
            .unwrap();

        for (path, expected) in [("README.md", "base readme\n"), ("new.txt", "new file\n")] {
            let cid = storage
                .runtime
                .block_on(storage.tree.resolve_path(&root_cid, path))
                .unwrap()
                .unwrap_or_else(|| panic!("{path} should exist"));
            let content = storage
                .runtime
                .block_on(storage.tree.get(&cid, None))
                .unwrap()
                .unwrap();
            assert_eq!(String::from_utf8(content).unwrap(), expected, "{path}");
        }
    }

    #[test]
    fn test_build_tree_materializes_loose_refs_at_git_paths() {
        let (storage, _temp) = create_test_storage();
        let commit_oid = write_test_commit(&storage);

        storage
            .write_ref("refs/heads/master", &Ref::Direct(commit_oid))
            .unwrap();
        storage
            .write_ref("refs/heads/codex/meshrouter-prod", &Ref::Direct(commit_oid))
            .unwrap();
        storage
            .write_ref("refs/tags/v1.0.0", &Ref::Direct(commit_oid))
            .unwrap();
        storage
            .write_ref("HEAD", &Ref::Symbolic("refs/heads/master".to_string()))
            .unwrap();

        let root_cid = storage.build_tree().unwrap();

        for path in [
            ".git/refs/heads/master",
            ".git/refs/heads/codex/meshrouter-prod",
            ".git/refs/tags/v1.0.0",
        ] {
            let ref_cid = storage
                .runtime
                .block_on(storage.tree.resolve_path(&root_cid, path))
                .unwrap()
                .unwrap_or_else(|| panic!("{path} should exist"));
            let ref_value = storage
                .runtime
                .block_on(storage.tree.get(&ref_cid, None))
                .unwrap()
                .unwrap();
            assert_eq!(
                String::from_utf8(ref_value).unwrap(),
                commit_oid.to_hex(),
                "{path} should contain the ref target",
            );
        }
    }

    #[test]
    fn test_materialized_tree_supports_static_http_clone_from_git_dir() {
        let (storage, _temp) = create_test_storage();
        let commit_oid = write_test_commit(&storage);
        storage
            .write_ref("refs/heads/main", &Ref::Direct(commit_oid))
            .unwrap();
        storage
            .write_ref("HEAD", &Ref::Symbolic("refs/heads/main".to_string()))
            .unwrap();

        let root_cid = storage.build_tree().unwrap();
        let export_dir = TempDir::new().unwrap();
        let repo_dir = export_dir.path().join("repo");
        export_tree_to_fs(&storage.runtime, &storage.tree, &root_cid, &repo_dir);

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let mut server = spawn_http_server(export_dir.path(), port);
        wait_for_http_server(&mut server, port, "/repo/.git/HEAD");

        let clone_dir = TempDir::new().unwrap();
        let clone_path = clone_dir.path().join("clone");
        let output = Command::new("git")
            .current_dir(clone_dir.path())
            .args([
                "clone",
                &format!("http://127.0.0.1:{port}/repo/.git", port = port),
                clone_path.to_str().unwrap(),
            ])
            .output()
            .unwrap();

        let _ = server.kill();
        let _ = server.wait();

        assert!(
            output.status.success(),
            "git clone failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(clone_path.join("README.md")).unwrap(),
            "hello from hashtree\n"
        );
    }

    #[test]
    fn test_validate_root_contains_direct_refs_rejects_missing_tip_object() {
        let (storage, _temp) = create_test_storage();
        let commit_oid = write_test_commit(&storage);
        storage
            .write_ref("refs/heads/main", &Ref::Direct(commit_oid))
            .unwrap();
        storage
            .write_ref("HEAD", &Ref::Symbolic("refs/heads/main".to_string()))
            .unwrap();

        let empty_objects_dir = storage
            .runtime
            .block_on(storage.tree.put_directory(vec![]))
            .unwrap();
        let refs_dir = storage
            .runtime
            .block_on(storage.tree.put_directory(vec![]))
            .unwrap();
        let info_dir = storage
            .runtime
            .block_on(storage.tree.put_directory(vec![]))
            .unwrap();
        let git_dir = storage
            .runtime
            .block_on(storage.tree.put_directory(vec![
                DirEntry::from_cid("HEAD", &info_dir).with_size(0),
                DirEntry::from_cid("info", &info_dir).with_link_type(LinkType::Dir),
                DirEntry::from_cid("objects", &empty_objects_dir).with_link_type(LinkType::Dir),
                DirEntry::from_cid("refs", &refs_dir).with_link_type(LinkType::Dir),
            ]))
            .unwrap();
        let root_cid = storage
            .runtime
            .block_on(storage.tree.put_directory(vec![
                DirEntry::from_cid(".git", &git_dir).with_link_type(LinkType::Dir),
            ]))
            .unwrap();

        let err = storage
            .validate_root_contains_direct_refs(&root_cid)
            .expect_err("missing ref-tip object should fail validation");
        assert!(
            err.to_string().contains(&commit_oid.to_hex()),
            "validation error should mention missing commit oid: {}",
            err
        );
    }
}
