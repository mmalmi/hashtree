use std::collections::HashMap;
use std::hash::{Hash as StdHash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use futures::executor::block_on;
use hashtree_core::{Cid, HashTree, HashTreeConfig, HashTreeError, LinkType, Store};
use thiserror::Error;

pub const ROOT_INODE: u64 = 1;

#[derive(Debug, Error)]
pub enum FsError {
    #[error("root hash is not a directory")]
    InvalidRoot,
    #[error("entry not found")]
    NotFound,
    #[error("not a directory")]
    NotDir,
    #[error("is a directory")]
    IsDir,
    #[error("entry already exists")]
    AlreadyExists,
    #[error("directory not empty")]
    NotEmpty,
    #[error("invalid entry name")]
    InvalidName,
    #[error("tree error: {0}")]
    Tree(String),
    #[error("publish error: {0}")]
    Publish(String),
}

impl From<HashTreeError> for FsError {
    fn from(err: HashTreeError) -> Self {
        FsError::Tree(err.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryAttr {
    pub inode: u64,
    pub size: u64,
    pub kind: EntryKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub inode: u64,
    pub name: String,
    pub kind: EntryKind,
}

pub trait RootPublisher: Send + Sync {
    fn publish(&self, cid: &Cid) -> Result<(), FsError>;
}

#[derive(Debug, Clone, Eq)]
struct ChildKey {
    parent: u64,
    name: String,
}

impl PartialEq for ChildKey {
    fn eq(&self, other: &Self) -> bool {
        self.parent == other.parent && self.name == other.name
    }
}

impl StdHash for ChildKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.parent.hash(state);
        self.name.hash(state);
    }
}

struct ResolvedEntry {
    cid: Cid,
    link_type: LinkType,
    size: u64,
}

pub struct HashtreeFuseInner<S: Store> {
    tree: HashTree<S>,
    root: RwLock<Cid>,
    paths: RwLock<HashMap<u64, Vec<String>>>,
    children: RwLock<HashMap<ChildKey, u64>>,
    parents: RwLock<HashMap<u64, u64>>,
    next_inode: AtomicU64,
    publisher: Option<Arc<dyn RootPublisher>>,
    modify_lock: Mutex<()>,
    #[cfg(feature = "fuse")]
    notifier: Mutex<Option<fuser::Notifier>>,
}

pub struct HashtreeFuse<S: Store> {
    inner: Arc<HashtreeFuseInner<S>>,
}

impl<S: Store> Clone for HashtreeFuse<S> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<S: Store> std::ops::Deref for HashtreeFuse<S> {
    type Target = HashtreeFuseInner<S>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<S: Store> HashtreeFuse<S> {
    pub fn new(store: Arc<S>, root: Cid) -> Result<Self, FsError> {
        Self::new_with_publisher(store, root, None)
    }

    pub fn new_with_publisher(
        store: Arc<S>,
        root: Cid,
        publisher: Option<Arc<dyn RootPublisher>>,
    ) -> Result<Self, FsError> {
        let mut config = HashTreeConfig::new(store);
        if root.key.is_none() {
            config = config.public();
        }
        let tree = HashTree::new(config);

        let is_dir = block_on(tree.get_directory_node(&root))?.is_some();
        if !is_dir {
            return Err(FsError::InvalidRoot);
        }

        let mut paths = HashMap::new();
        paths.insert(ROOT_INODE, Vec::new());
        let mut parents = HashMap::new();
        parents.insert(ROOT_INODE, ROOT_INODE);

        Ok(Self {
            inner: Arc::new(HashtreeFuseInner {
                tree,
                root: RwLock::new(root),
                paths: RwLock::new(paths),
                children: RwLock::new(HashMap::new()),
                parents: RwLock::new(parents),
                next_inode: AtomicU64::new(ROOT_INODE + 1),
                publisher,
                modify_lock: Mutex::new(()),
                #[cfg(feature = "fuse")]
                notifier: Mutex::new(None),
            }),
        })
    }

    pub fn current_root(&self) -> Cid {
        self.root.read().unwrap().clone()
    }

    pub fn replace_root(&self, root: Cid) -> Result<(), FsError> {
        let _guard = self.modify_lock.lock().unwrap();
        let is_dir = block_on(self.tree.get_directory_node(&root))?.is_some();
        if !is_dir {
            return Err(FsError::InvalidRoot);
        }

        #[cfg(feature = "fuse")]
        self.notify_known_caches_invalidated();

        *self.root.write().unwrap() = root;

        let mut paths = HashMap::new();
        paths.insert(ROOT_INODE, Vec::new());
        *self.paths.write().unwrap() = paths;

        self.children.write().unwrap().clear();

        let mut parents = HashMap::new();
        parents.insert(ROOT_INODE, ROOT_INODE);
        *self.parents.write().unwrap() = parents;

        Ok(())
    }

    #[cfg(feature = "fuse")]
    fn set_notifier(&self, notifier: fuser::Notifier) {
        *self.notifier.lock().unwrap() = Some(notifier);
    }

    #[cfg(feature = "fuse")]
    fn clear_notifier(&self) {
        *self.notifier.lock().unwrap() = None;
    }

    #[cfg(feature = "fuse")]
    fn notify_known_caches_invalidated(&self) {
        let inodes: Vec<u64> = self.paths.read().unwrap().keys().copied().collect();
        let entries: Vec<(u64, String)> = self
            .children
            .read()
            .unwrap()
            .keys()
            .map(|key| (key.parent, key.name.clone()))
            .collect();
        let guard = self.notifier.lock().unwrap();
        let Some(notifier) = guard.as_ref() else {
            return;
        };
        for inode in inodes {
            let _ = notifier.inval_inode(inode, 0, 0);
        }
        for (parent, name) in entries {
            let _ = notifier.inval_entry(parent, std::ffi::OsStr::new(&name));
        }
    }

    pub fn lookup_child(&self, parent: u64, name: &str) -> Result<EntryAttr, FsError> {
        if name.is_empty() {
            return Err(FsError::NotFound);
        }
        if name == "." {
            return self.get_attr(parent);
        }
        if name == ".." {
            let parent_inode = self.parent_inode(parent);
            return self.get_attr(parent_inode);
        }

        let child_inode = self.get_or_create_child_inode(parent, name)?;
        let path = self.path_for_inode(child_inode)?;

        match self.resolve_entry(&path) {
            Ok(entry) => self.entry_attr_from_resolved(child_inode, entry),
            Err(FsError::NotFound) => {
                self.drop_inode(child_inode);
                Err(FsError::NotFound)
            }
            Err(err) => Err(err),
        }
    }

    pub fn get_attr(&self, inode: u64) -> Result<EntryAttr, FsError> {
        if inode == ROOT_INODE {
            return Ok(EntryAttr {
                inode,
                size: 0,
                kind: EntryKind::Directory,
            });
        }

        let path = self.path_for_inode(inode)?;
        let entry = self.resolve_entry(&path)?;
        self.entry_attr_from_resolved(inode, entry)
    }

    pub fn read_file(&self, inode: u64, offset: u64, size: u32) -> Result<Vec<u8>, FsError> {
        let path = self.path_for_inode(inode)?;
        let entry = self.resolve_entry(&path)?;
        if entry.link_type == LinkType::Dir {
            return Err(FsError::IsDir);
        }

        let file_size = self.entry_size(&entry)?;
        if offset >= file_size {
            return Ok(vec![]);
        }
        let read_len = (size as u64).min(file_size - offset);
        if read_len == 0 {
            return Ok(vec![]);
        }

        if entry.cid.key.is_some() {
            let data = block_on(self.tree.get(&entry.cid, None))?.ok_or(FsError::NotFound)?;
            let start = usize::try_from(offset).unwrap_or(usize::MAX);
            if start >= data.len() {
                return Ok(vec![]);
            }
            let end_u64 = offset.saturating_add(read_len);
            let mut end = usize::try_from(end_u64).unwrap_or(data.len());
            if end > data.len() {
                end = data.len();
            }
            return Ok(data[start..end].to_vec());
        }

        let end = offset.saturating_add(read_len);
        let data = block_on(
            self.tree
                .read_file_range(&entry.cid.hash, offset, Some(end)),
        )?
        .ok_or(FsError::NotFound)?;
        Ok(data)
    }

    pub fn read_dir(&self, inode: u64) -> Result<Vec<DirEntry>, FsError> {
        let path = self.path_for_inode(inode)?;
        let dir_cid = self.resolve_dir_cid(&path)?;

        let entries = block_on(self.tree.list_directory(&dir_cid))?;
        let mut out = Vec::with_capacity(entries.len());

        for entry in entries {
            let child_inode = self.get_or_create_child_inode(inode, &entry.name)?;
            out.push(DirEntry {
                inode: child_inode,
                name: entry.name,
                kind: Self::kind_from_link(entry.link_type),
            });
        }

        Ok(out)
    }

    pub fn create_file(&self, parent: u64, name: &str) -> Result<EntryAttr, FsError> {
        self.ensure_valid_name(name)?;
        let _guard = self.modify_lock.lock().unwrap();

        let parent_path = self.path_for_inode(parent)?;
        let mut child_path = parent_path.clone();
        child_path.push(name.to_string());

        if self.resolve_entry(&child_path).is_ok() {
            return Err(FsError::AlreadyExists);
        }

        let (cid, size) = block_on(self.tree.put(&[]))?;
        let link_type = self.link_type_for_size(size);
        let new_root = block_on(self.tree.set_entry(
            &self.current_root(),
            &self.path_refs(&parent_path),
            name,
            &cid,
            size,
            link_type,
        ))?;

        self.apply_root_update(new_root)?;

        let inode = self.insert_path(parent, name.to_string(), child_path);
        Ok(EntryAttr {
            inode,
            size,
            kind: EntryKind::File,
        })
    }

    pub fn mkdir(&self, parent: u64, name: &str) -> Result<EntryAttr, FsError> {
        self.ensure_valid_name(name)?;
        let _guard = self.modify_lock.lock().unwrap();

        let parent_path = self.path_for_inode(parent)?;
        let mut child_path = parent_path.clone();
        child_path.push(name.to_string());

        if self.resolve_entry(&child_path).is_ok() {
            return Err(FsError::AlreadyExists);
        }

        let dir_cid = block_on(self.tree.put_directory(Vec::new()))?;
        let new_root = block_on(self.tree.set_entry(
            &self.current_root(),
            &self.path_refs(&parent_path),
            name,
            &dir_cid,
            0,
            LinkType::Dir,
        ))?;

        self.apply_root_update(new_root)?;

        let inode = self.insert_path(parent, name.to_string(), child_path);
        Ok(EntryAttr {
            inode,
            size: 0,
            kind: EntryKind::Directory,
        })
    }

    pub fn write_file(&self, inode: u64, offset: u64, data: &[u8]) -> Result<u32, FsError> {
        let _guard = self.modify_lock.lock().unwrap();
        let path = self.path_for_inode(inode)?;
        let existing = self.read_file_full(&path)?;
        let new_data = Self::apply_write(existing, offset, data);
        self.update_file_at_path(&path, new_data)?;
        Ok(data.len() as u32)
    }

    pub fn truncate_file(&self, inode: u64, size: u64) -> Result<(), FsError> {
        let _guard = self.modify_lock.lock().unwrap();
        let path = self.path_for_inode(inode)?;
        let existing = self.read_file_full(&path)?;
        let new_data = Self::apply_truncate(existing, size);
        self.update_file_at_path(&path, new_data)?;
        Ok(())
    }

    pub fn unlink(&self, parent: u64, name: &str) -> Result<(), FsError> {
        self.ensure_valid_name(name)?;
        let _guard = self.modify_lock.lock().unwrap();

        let parent_path = self.path_for_inode(parent)?;
        let mut child_path = parent_path.clone();
        child_path.push(name.to_string());
        let entry = self.resolve_entry(&child_path)?;
        if entry.link_type == LinkType::Dir {
            return Err(FsError::IsDir);
        }

        let new_root = block_on(self.tree.remove_entry(
            &self.current_root(),
            &self.path_refs(&parent_path),
            name,
        ))?;

        self.apply_root_update(new_root)?;
        self.remove_paths_prefix(&child_path);
        self.children.write().unwrap().remove(&ChildKey {
            parent,
            name: name.to_string(),
        });

        Ok(())
    }

    pub fn rmdir(&self, parent: u64, name: &str) -> Result<(), FsError> {
        self.ensure_valid_name(name)?;
        let _guard = self.modify_lock.lock().unwrap();

        let parent_path = self.path_for_inode(parent)?;
        let mut child_path = parent_path.clone();
        child_path.push(name.to_string());
        let entry = self.resolve_entry(&child_path)?;
        if entry.link_type != LinkType::Dir {
            return Err(FsError::NotDir);
        }

        let dir_entries = block_on(self.tree.list_directory(&entry.cid))?;
        if !dir_entries.is_empty() {
            return Err(FsError::NotEmpty);
        }

        let new_root = block_on(self.tree.remove_entry(
            &self.current_root(),
            &self.path_refs(&parent_path),
            name,
        ))?;

        self.apply_root_update(new_root)?;
        self.remove_paths_prefix(&child_path);
        self.children.write().unwrap().remove(&ChildKey {
            parent,
            name: name.to_string(),
        });

        Ok(())
    }

    pub fn rename(
        &self,
        parent: u64,
        name: &str,
        new_parent: u64,
        new_name: &str,
    ) -> Result<(), FsError> {
        self.ensure_valid_name(name)?;
        self.ensure_valid_name(new_name)?;
        let _guard = self.modify_lock.lock().unwrap();

        if parent == new_parent && name == new_name {
            return Ok(());
        }

        let parent_path = self.path_for_inode(parent)?;
        let new_parent_path = self.path_for_inode(new_parent)?;

        let mut old_path = parent_path.clone();
        old_path.push(name.to_string());
        let entry = self.resolve_entry(&old_path)?;

        let new_root = block_on(self.tree.set_entry(
            &self.current_root(),
            &self.path_refs(&new_parent_path),
            new_name,
            &entry.cid,
            entry.size,
            entry.link_type,
        ))?;
        let new_root = block_on(self.tree.remove_entry(
            &new_root,
            &self.path_refs(&parent_path),
            name,
        ))?;

        self.apply_root_update(new_root)?;

        let inode = self.get_or_create_child_inode(parent, name)?;
        let mut new_path = new_parent_path.clone();
        new_path.push(new_name.to_string());

        self.children.write().unwrap().remove(&ChildKey {
            parent,
            name: name.to_string(),
        });
        self.children.write().unwrap().insert(
            ChildKey {
                parent: new_parent,
                name: new_name.to_string(),
            },
            inode,
        );

        self.parents.write().unwrap().insert(inode, new_parent);
        self.update_paths_prefix(&old_path, &new_path);

        Ok(())
    }

    fn ensure_valid_name(&self, name: &str) -> Result<(), FsError> {
        if name.is_empty() || name.contains('/') {
            return Err(FsError::InvalidName);
        }
        Ok(())
    }

    fn path_for_inode(&self, inode: u64) -> Result<Vec<String>, FsError> {
        self.paths
            .read()
            .unwrap()
            .get(&inode)
            .cloned()
            .ok_or(FsError::NotFound)
    }

    fn resolve_entry(&self, path: &[String]) -> Result<ResolvedEntry, FsError> {
        if path.is_empty() {
            return Ok(ResolvedEntry {
                cid: self.current_root(),
                link_type: LinkType::Dir,
                size: 0,
            });
        }

        let (parent_path, name) = path.split_at(path.len() - 1);
        let parent_cid = self.resolve_dir_cid(parent_path)?;
        let entries = block_on(self.tree.list_directory(&parent_cid))?;
        let entry = entries
            .into_iter()
            .find(|e| e.name == name[0])
            .ok_or(FsError::NotFound)?;

        Ok(ResolvedEntry {
            cid: Cid {
                hash: entry.hash,
                key: entry.key,
            },
            link_type: entry.link_type,
            size: entry.size,
        })
    }

    fn resolve_dir_cid(&self, path: &[String]) -> Result<Cid, FsError> {
        if path.is_empty() {
            return Ok(self.current_root());
        }

        let root = self.current_root();
        let path_str = path.join("/");
        let cid = block_on(self.tree.resolve(&root, &path_str))?.ok_or(FsError::NotFound)?;

        let is_dir = block_on(self.tree.is_dir(&cid))?;
        if !is_dir {
            return Err(FsError::NotDir);
        }

        Ok(cid)
    }

    fn entry_attr_from_resolved(
        &self,
        inode: u64,
        entry: ResolvedEntry,
    ) -> Result<EntryAttr, FsError> {
        let kind = Self::kind_from_link(entry.link_type);
        let size = if kind == EntryKind::Directory {
            0
        } else {
            self.entry_size(&entry)?
        };

        Ok(EntryAttr { inode, size, kind })
    }

    fn entry_size(&self, entry: &ResolvedEntry) -> Result<u64, FsError> {
        if entry.link_type == LinkType::Dir {
            return Ok(0);
        }
        if entry.size > 0 {
            return Ok(entry.size);
        }

        let data = block_on(self.tree.get(&entry.cid, None))?.ok_or(FsError::NotFound)?;
        Ok(data.len() as u64)
    }

    fn read_file_full(&self, path: &[String]) -> Result<Vec<u8>, FsError> {
        let entry = self.resolve_entry(path)?;
        if entry.link_type == LinkType::Dir {
            return Err(FsError::IsDir);
        }
        let data = block_on(self.tree.get(&entry.cid, None))?.ok_or(FsError::NotFound)?;
        Ok(data)
    }

    fn update_file_at_path(&self, path: &[String], data: Vec<u8>) -> Result<(), FsError> {
        let (parent_path, name) = path.split_at(path.len() - 1);
        let (cid, size) = block_on(self.tree.put(&data))?;
        let link_type = self.link_type_for_size(size);

        let new_root = block_on(self.tree.set_entry(
            &self.current_root(),
            &self.path_refs(parent_path),
            name[0].as_str(),
            &cid,
            size,
            link_type,
        ))?;

        self.apply_root_update(new_root)
    }

    fn apply_root_update(&self, new_root: Cid) -> Result<(), FsError> {
        if let Some(publisher) = &self.publisher {
            publisher.publish(&new_root)?;
        }
        *self.root.write().unwrap() = new_root;
        Ok(())
    }

    fn link_type_for_size(&self, size: u64) -> LinkType {
        if size as usize > self.tree.chunk_size() {
            LinkType::File
        } else {
            LinkType::Blob
        }
    }

    fn get_or_create_child_inode(&self, parent: u64, name: &str) -> Result<u64, FsError> {
        let key = ChildKey {
            parent,
            name: name.to_string(),
        };
        if let Some(inode) = self.children.read().unwrap().get(&key).copied() {
            return Ok(inode);
        }

        let parent_path = self.path_for_inode(parent)?;
        let mut child_path = parent_path.clone();
        child_path.push(name.to_string());

        if let Some(existing) = self.find_inode_by_path(&child_path) {
            self.children.write().unwrap().insert(key, existing);
            return Ok(existing);
        }

        Ok(self.insert_path(parent, name.to_string(), child_path))
    }

    fn insert_path(&self, parent: u64, name: String, path: Vec<String>) -> u64 {
        let inode = self.next_inode.fetch_add(1, Ordering::Relaxed);
        self.paths.write().unwrap().insert(inode, path);
        self.parents.write().unwrap().insert(inode, parent);
        self.children
            .write()
            .unwrap()
            .insert(ChildKey { parent, name }, inode);
        inode
    }

    fn find_inode_by_path(&self, path: &[String]) -> Option<u64> {
        self.paths
            .read()
            .unwrap()
            .iter()
            .find_map(|(inode, inode_path)| {
                if inode_path == path {
                    Some(*inode)
                } else {
                    None
                }
            })
    }

    fn update_paths_prefix(&self, old_prefix: &[String], new_prefix: &[String]) {
        let mut paths = self.paths.write().unwrap();
        for path in paths.values_mut() {
            if Self::path_has_prefix(path, old_prefix) {
                let mut updated = new_prefix.to_vec();
                updated.extend_from_slice(&path[old_prefix.len()..]);
                *path = updated;
            }
        }
    }

    fn remove_paths_prefix(&self, prefix: &[String]) {
        let mut to_remove = Vec::new();
        {
            let paths = self.paths.read().unwrap();
            for (inode, path) in paths.iter() {
                if *inode == ROOT_INODE {
                    continue;
                }
                if Self::path_has_prefix(path, prefix) {
                    to_remove.push(*inode);
                }
            }
        }

        if to_remove.is_empty() {
            return;
        }

        let remove_set: std::collections::HashSet<u64> = to_remove.into_iter().collect();
        self.paths
            .write()
            .unwrap()
            .retain(|inode, _| !remove_set.contains(inode));
        self.parents
            .write()
            .unwrap()
            .retain(|inode, _| !remove_set.contains(inode));
        self.children
            .write()
            .unwrap()
            .retain(|_, inode| !remove_set.contains(inode));
    }

    fn drop_inode(&self, inode: u64) {
        if inode == ROOT_INODE {
            return;
        }
        let mut paths = self.paths.write().unwrap();
        let removed_path = paths.remove(&inode);
        drop(paths);
        self.parents.write().unwrap().remove(&inode);
        if let Some(path) = removed_path {
            if let Some((name, parent_path)) = path.split_last() {
                if let Some(parent_inode) = self.find_inode_by_path(parent_path) {
                    self.children.write().unwrap().remove(&ChildKey {
                        parent: parent_inode,
                        name: name.to_string(),
                    });
                }
            }
        }
    }

    fn parent_inode(&self, inode: u64) -> u64 {
        self.parents
            .read()
            .unwrap()
            .get(&inode)
            .copied()
            .unwrap_or(ROOT_INODE)
    }

    fn kind_from_link(link_type: LinkType) -> EntryKind {
        match link_type {
            LinkType::Dir => EntryKind::Directory,
            LinkType::Blob | LinkType::File => EntryKind::File,
        }
    }

    fn apply_write(mut existing: Vec<u8>, offset: u64, data: &[u8]) -> Vec<u8> {
        let offset_usize = offset as usize;
        if existing.len() < offset_usize {
            existing.resize(offset_usize, 0);
        }
        if existing.len() < offset_usize + data.len() {
            existing.resize(offset_usize + data.len(), 0);
        }
        existing[offset_usize..offset_usize + data.len()].copy_from_slice(data);
        existing
    }

    fn apply_truncate(mut existing: Vec<u8>, size: u64) -> Vec<u8> {
        let size = size as usize;
        if existing.len() > size {
            existing.truncate(size);
        } else if existing.len() < size {
            existing.resize(size, 0);
        }
        existing
    }

    fn path_refs<'a>(&self, path: &'a [String]) -> Vec<&'a str> {
        path.iter().map(|p| p.as_str()).collect()
    }

    fn path_has_prefix(path: &[String], prefix: &[String]) -> bool {
        if prefix.len() > path.len() {
            return false;
        }
        path.iter().zip(prefix.iter()).all(|(a, b)| a == b)
    }
}

#[cfg(feature = "fuse")]
mod fuse_impl {
    use super::*;
    use fuser::{
        FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate, ReplyData,
        ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyStatfs, ReplyWrite, Request,
    };
    use std::ffi::CString;
    use std::ffi::OsStr;
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    const TTL: Duration = Duration::from_secs(1);
    const FALLBACK_BLOCK_SIZE: u32 = 4096;
    const FALLBACK_TOTAL_BYTES: u64 = 1 << 40;
    const FALLBACK_FREE_BYTES: u64 = 1 << 39;
    const FALLBACK_TOTAL_FILES: u64 = 1_000_000;
    const FALLBACK_FREE_FILES: u64 = 900_000;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct FsStats {
        pub(crate) blocks: u64,
        pub(crate) bfree: u64,
        pub(crate) bavail: u64,
        pub(crate) files: u64,
        pub(crate) ffree: u64,
        pub(crate) bsize: u32,
        pub(crate) namelen: u32,
        pub(crate) frsize: u32,
    }

    impl FsError {
        fn errno(&self) -> i32 {
            match self {
                FsError::InvalidRoot | FsError::InvalidName => libc::EINVAL,
                FsError::NotFound => libc::ENOENT,
                FsError::NotDir => libc::ENOTDIR,
                FsError::IsDir => libc::EISDIR,
                FsError::AlreadyExists => libc::EEXIST,
                FsError::NotEmpty => libc::ENOTEMPTY,
                FsError::Tree(_) | FsError::Publish(_) => libc::EIO,
            }
        }
    }

    pub(crate) fn fallback_fs_stats() -> FsStats {
        let blocks = FALLBACK_TOTAL_BYTES / u64::from(FALLBACK_BLOCK_SIZE);
        let free_blocks = FALLBACK_FREE_BYTES / u64::from(FALLBACK_BLOCK_SIZE);
        FsStats {
            blocks,
            bfree: free_blocks,
            bavail: free_blocks,
            files: FALLBACK_TOTAL_FILES,
            ffree: FALLBACK_FREE_FILES,
            bsize: FALLBACK_BLOCK_SIZE,
            namelen: 255,
            frsize: FALLBACK_BLOCK_SIZE,
        }
    }

    fn host_fs_stats(path: &Path) -> Option<FsStats> {
        let path = CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
        let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
        let result = unsafe { libc::statfs(path.as_ptr(), stat.as_mut_ptr()) };
        if result != 0 {
            return None;
        }

        let stat = unsafe { stat.assume_init() };
        let bsize = u32::try_from(stat.f_bsize).ok()?;
        Some(FsStats {
            blocks: stat.f_blocks,
            bfree: stat.f_bfree,
            bavail: stat.f_bavail,
            files: stat.f_files,
            ffree: stat.f_ffree,
            bsize,
            namelen: 255,
            frsize: bsize,
        })
    }

    pub(crate) fn current_fs_stats() -> FsStats {
        host_fs_stats(Path::new("/")).unwrap_or_else(fallback_fs_stats)
    }

    impl<S: Store + Send + Sync + 'static> HashtreeFuse<S> {
        pub fn mount(
            self,
            mountpoint: impl AsRef<Path>,
            options: &[MountOption],
        ) -> std::io::Result<()> {
            let mut session = fuser::Session::new(self.clone(), mountpoint.as_ref(), options)?;
            self.set_notifier(session.notifier());
            let result = session.run();
            self.clear_notifier();
            result
        }

        fn file_attr(&self, attr: &EntryAttr) -> FileAttr {
            let (kind, perm, nlink) = match attr.kind {
                EntryKind::Directory => (FileType::Directory, 0o755, 2),
                EntryKind::File => (FileType::RegularFile, 0o644, 1),
            };
            let uid = unsafe { libc::geteuid() };
            let gid = unsafe { libc::getegid() };
            let blocks = attr.size.div_ceil(512);

            FileAttr {
                ino: attr.inode,
                size: attr.size,
                blocks,
                atime: SystemTime::UNIX_EPOCH,
                mtime: SystemTime::UNIX_EPOCH,
                ctime: SystemTime::UNIX_EPOCH,
                crtime: SystemTime::UNIX_EPOCH,
                kind,
                perm,
                nlink,
                uid,
                gid,
                rdev: 0,
                blksize: 512,
                flags: 0,
            }
        }

        fn file_type(kind: EntryKind) -> FileType {
            match kind {
                EntryKind::Directory => FileType::Directory,
                EntryKind::File => FileType::RegularFile,
            }
        }
    }

    impl<S: Store + Send + Sync + 'static> Filesystem for HashtreeFuse<S> {
        fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
            let name = match name.to_str() {
                Some(value) => value,
                None => {
                    reply.error(libc::ENOENT);
                    return;
                }
            };

            match self.lookup_child(parent, name) {
                Ok(attr) => reply.entry(&TTL, &self.file_attr(&attr), 0),
                Err(err) => reply.error(err.errno()),
            }
        }

        fn getattr(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyAttr) {
            match self.get_attr(ino) {
                Ok(attr) => reply.attr(&TTL, &self.file_attr(&attr)),
                Err(err) => reply.error(err.errno()),
            }
        }

        fn open(&mut self, _req: &Request<'_>, _ino: u64, _flags: i32, reply: fuser::ReplyOpen) {
            reply.opened(0, 0);
        }

        fn read(
            &mut self,
            _req: &Request<'_>,
            ino: u64,
            _fh: u64,
            offset: i64,
            size: u32,
            _flags: i32,
            _lock_owner: Option<u64>,
            reply: ReplyData,
        ) {
            let offset = if offset < 0 { 0 } else { offset as u64 };
            match self.read_file(ino, offset, size) {
                Ok(data) => reply.data(&data),
                Err(err) => reply.error(err.errno()),
            }
        }

        fn write(
            &mut self,
            _req: &Request<'_>,
            ino: u64,
            _fh: u64,
            offset: i64,
            data: &[u8],
            _write_flags: u32,
            _flags: i32,
            _lock_owner: Option<u64>,
            reply: ReplyWrite,
        ) {
            let offset = if offset < 0 { 0 } else { offset as u64 };
            match self.write_file(ino, offset, data) {
                Ok(written) => reply.written(written),
                Err(err) => reply.error(err.errno()),
            }
        }

        fn create(
            &mut self,
            _req: &Request<'_>,
            parent: u64,
            name: &OsStr,
            _mode: u32,
            _umask: u32,
            _flags: i32,
            reply: ReplyCreate,
        ) {
            let name = match name.to_str() {
                Some(value) => value,
                None => {
                    reply.error(libc::EINVAL);
                    return;
                }
            };

            match self.create_file(parent, name) {
                Ok(attr) => reply.created(&TTL, &self.file_attr(&attr), 0, 0, 0),
                Err(err) => reply.error(err.errno()),
            }
        }

        fn mkdir(
            &mut self,
            _req: &Request<'_>,
            parent: u64,
            name: &OsStr,
            _mode: u32,
            _umask: u32,
            reply: ReplyEntry,
        ) {
            let name = match name.to_str() {
                Some(value) => value,
                None => {
                    reply.error(libc::EINVAL);
                    return;
                }
            };

            match HashtreeFuse::mkdir(self, parent, name) {
                Ok(attr) => reply.entry(&TTL, &self.file_attr(&attr), 0),
                Err(err) => reply.error(err.errno()),
            }
        }

        fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
            let name = match name.to_str() {
                Some(value) => value,
                None => {
                    reply.error(libc::EINVAL);
                    return;
                }
            };

            match HashtreeFuse::unlink(self, parent, name) {
                Ok(()) => reply.ok(),
                Err(err) => reply.error(err.errno()),
            }
        }

        fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
            let name = match name.to_str() {
                Some(value) => value,
                None => {
                    reply.error(libc::EINVAL);
                    return;
                }
            };

            match HashtreeFuse::rmdir(self, parent, name) {
                Ok(()) => reply.ok(),
                Err(err) => reply.error(err.errno()),
            }
        }

        fn rename(
            &mut self,
            _req: &Request<'_>,
            parent: u64,
            name: &OsStr,
            newparent: u64,
            newname: &OsStr,
            _flags: u32,
            reply: ReplyEmpty,
        ) {
            let name = match name.to_str() {
                Some(value) => value,
                None => {
                    reply.error(libc::EINVAL);
                    return;
                }
            };
            let newname = match newname.to_str() {
                Some(value) => value,
                None => {
                    reply.error(libc::EINVAL);
                    return;
                }
            };

            match HashtreeFuse::rename(self, parent, name, newparent, newname) {
                Ok(()) => reply.ok(),
                Err(err) => reply.error(err.errno()),
            }
        }

        fn setattr(
            &mut self,
            _req: &Request<'_>,
            ino: u64,
            _mode: Option<u32>,
            _uid: Option<u32>,
            _gid: Option<u32>,
            size: Option<u64>,
            _atime: Option<fuser::TimeOrNow>,
            _mtime: Option<fuser::TimeOrNow>,
            _ctime: Option<SystemTime>,
            _fh: Option<u64>,
            _crtime: Option<SystemTime>,
            _chgtime: Option<SystemTime>,
            _bkuptime: Option<SystemTime>,
            _flags: Option<u32>,
            reply: ReplyAttr,
        ) {
            if let Some(size) = size {
                match self.truncate_file(ino, size) {
                    Ok(()) => {
                        if let Ok(attr) = self.get_attr(ino) {
                            reply.attr(&TTL, &self.file_attr(&attr));
                        } else {
                            reply.error(libc::EIO);
                        }
                    }
                    Err(err) => reply.error(err.errno()),
                }
            } else {
                match self.get_attr(ino) {
                    Ok(attr) => reply.attr(&TTL, &self.file_attr(&attr)),
                    Err(err) => reply.error(err.errno()),
                }
            }
        }

        fn readdir(
            &mut self,
            _req: &Request<'_>,
            ino: u64,
            _fh: u64,
            offset: i64,
            mut reply: ReplyDirectory,
        ) {
            let mut entries = Vec::new();
            entries.push((ino, EntryKind::Directory, ".".to_string()));
            let parent = self.parent_inode(ino);
            entries.push((parent, EntryKind::Directory, "..".to_string()));

            match self.read_dir(ino) {
                Ok(children) => {
                    for entry in children {
                        entries.push((entry.inode, entry.kind, entry.name));
                    }
                }
                Err(err) => {
                    reply.error(err.errno());
                    return;
                }
            }

            let start = if offset < 0 { 0 } else { offset as usize };
            for (index, (inode, kind, name)) in entries.into_iter().enumerate().skip(start) {
                let next_offset = (index + 1) as i64;
                let full = reply.add(inode, next_offset, Self::file_type(kind), name);
                if full {
                    break;
                }
            }

            reply.ok();
        }

        fn statfs(&mut self, _req: &Request<'_>, _ino: u64, reply: ReplyStatfs) {
            let stats = current_fs_stats();
            reply.statfs(
                stats.blocks,
                stats.bfree,
                stats.bavail,
                stats.files,
                stats.ffree,
                stats.bsize,
                stats.namelen,
                stats.frsize,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashtree_core::store::MemoryStore;

    #[cfg(feature = "fuse")]
    use super::fuse_impl::{current_fs_stats, fallback_fs_stats};

    struct RecordingPublisher {
        updates: Mutex<Vec<Cid>>,
    }

    impl RecordingPublisher {
        fn new() -> Self {
            Self {
                updates: Mutex::new(Vec::new()),
            }
        }

        fn updates(&self) -> Vec<Cid> {
            self.updates.lock().unwrap().clone()
        }
    }

    impl RootPublisher for RecordingPublisher {
        fn publish(&self, cid: &Cid) -> Result<(), FsError> {
            self.updates.lock().unwrap().push(cid.clone());
            Ok(())
        }
    }

    async fn empty_root(store: Arc<MemoryStore>) -> Cid {
        let tree = HashTree::new(HashTreeConfig::new(store.clone()));
        tree.put_directory(Vec::new()).await.unwrap()
    }

    #[tokio::test]
    async fn test_create_write_read_file() {
        let store = Arc::new(MemoryStore::new());
        let root = empty_root(store.clone()).await;
        let fs = HashtreeFuse::new(store, root).unwrap();

        let attr = fs.create_file(ROOT_INODE, "hello.txt").unwrap();
        assert_eq!(attr.kind, EntryKind::File);

        fs.write_file(attr.inode, 0, b"hello").unwrap();
        let read = fs.read_file(attr.inode, 0, 5).unwrap();
        assert_eq!(read, b"hello");
    }

    #[tokio::test]
    async fn test_mkdir_and_rename() {
        let store = Arc::new(MemoryStore::new());
        let root = empty_root(store.clone()).await;
        let fs = HashtreeFuse::new(store, root).unwrap();

        let dir = fs.mkdir(ROOT_INODE, "docs").unwrap();
        let file = fs.create_file(dir.inode, "draft.txt").unwrap();
        fs.write_file(file.inode, 0, b"data").unwrap();

        fs.rename(dir.inode, "draft.txt", dir.inode, "final.txt")
            .unwrap();
        let entries = fs.read_dir(dir.inode).unwrap();
        let names: Vec<String> = entries.into_iter().map(|e| e.name).collect();
        assert!(names.contains(&"final.txt".to_string()));
        assert!(!names.contains(&"draft.txt".to_string()));
    }

    #[tokio::test]
    async fn test_truncate_file() {
        let store = Arc::new(MemoryStore::new());
        let root = empty_root(store.clone()).await;
        let fs = HashtreeFuse::new(store, root).unwrap();

        let file = fs.create_file(ROOT_INODE, "file.bin").unwrap();
        fs.write_file(file.inode, 0, b"abcdef").unwrap();
        fs.truncate_file(file.inode, 3).unwrap();
        let read = fs.read_file(file.inode, 0, 10).unwrap();
        assert_eq!(read, b"abc");
    }

    #[tokio::test]
    async fn test_publisher_invoked() {
        let store = Arc::new(MemoryStore::new());
        let root = empty_root(store.clone()).await;
        let publisher = Arc::new(RecordingPublisher::new());
        let fs = HashtreeFuse::new_with_publisher(store, root, Some(publisher.clone())).unwrap();

        let file = fs.create_file(ROOT_INODE, "note.txt").unwrap();
        fs.write_file(file.inode, 0, b"note").unwrap();

        let updates = publisher.updates();
        assert!(!updates.is_empty());
        assert_eq!(updates.last().unwrap(), &fs.current_root());
    }

    #[tokio::test]
    async fn test_replace_root_refreshes_visible_tree_without_publishing() {
        let store = Arc::new(MemoryStore::new());
        let tree = HashTree::new(HashTreeConfig::new(store.clone()));
        let root = empty_root(store.clone()).await;
        let publisher = Arc::new(RecordingPublisher::new());
        let fs = HashtreeFuse::new_with_publisher(store, root, Some(publisher.clone())).unwrap();

        let old_file = fs.create_file(ROOT_INODE, "old.txt").unwrap();
        fs.write_file(old_file.inode, 0, b"old").unwrap();
        assert!(!publisher.updates().is_empty());
        publisher.updates.lock().unwrap().clear();

        let (new_blob, new_size) = tree.put(b"new").await.unwrap();
        let new_root = tree
            .put_directory(vec![hashtree_core::DirEntry::from_cid(
                "new.txt", &new_blob,
            )
            .with_size(new_size)
            .with_link_type(LinkType::Blob)])
            .await
            .unwrap();

        let old_lookup = fs.lookup_child(ROOT_INODE, "old.txt").unwrap();
        fs.replace_root(new_root.clone()).unwrap();

        assert_eq!(fs.current_root(), new_root);
        assert!(fs.lookup_child(ROOT_INODE, "old.txt").is_err());
        let new_lookup = fs.lookup_child(ROOT_INODE, "new.txt").unwrap();
        assert_ne!(old_lookup.inode, new_lookup.inode);
        assert_eq!(fs.read_file(new_lookup.inode, 0, 3).unwrap(), b"new");
        assert!(publisher.updates().is_empty());
    }

    #[cfg(feature = "fuse")]
    #[test]
    fn test_fallback_fs_stats_reports_free_space() {
        let stats = fallback_fs_stats();
        assert!(stats.blocks > 0);
        assert!(stats.bfree > 0);
        assert!(stats.bavail > 0);
        assert!(stats.bsize > 0);
        assert_eq!(stats.bfree, stats.bavail);
    }

    #[cfg(feature = "fuse")]
    #[test]
    fn test_current_fs_stats_reports_free_space() {
        let stats = current_fs_stats();
        assert!(stats.blocks > 0);
        assert!(stats.bfree > 0);
        assert!(stats.bavail > 0);
        assert!(stats.bsize > 0);
    }
}
