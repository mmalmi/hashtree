use super::*;
use hashtree_core::store::MemoryStore;
use std::{fs, io, path::PathBuf, thread};

struct Mounted {
    unmounter: fuser::SessionUnmounter,
    worker: Option<thread::JoinHandle<io::Result<()>>>,
    filesystem: HashtreeFuse<MemoryStore>,
}

impl Mounted {
    fn new(filesystem: HashtreeFuse<MemoryStore>, path: &std::path::Path) -> Self {
        let mut session = fuser::Session::new(filesystem.clone(), path, &[]).unwrap();
        filesystem.set_notifier(session.notifier());
        let unmounter = session.unmount_callable();
        let worker = Some(thread::spawn(move || session.run()));
        Self {
            unmounter,
            worker,
            filesystem,
        }
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        self.unmounter
            .unmount()
            .expect("unmount owned test filesystem");
        self.worker.take().unwrap().join().unwrap().unwrap();
        self.filesystem.clear_notifier();
    }
}

struct MountDirectory(PathBuf);

impl Drop for MountDirectory {
    fn drop(&mut self) {
        // Never recursively remove a path that might still be mounted.
        let _ = fs::remove_dir(&self.0);
    }
}

#[test]
#[ignore = "requires an admitted non-root Linux runner with /dev/fuse and fusermount3"]
fn real_mount_crud_remote_root_and_remount() {
    assert_ne!(
        unsafe { libc::geteuid() },
        0,
        "must exercise unprivileged mounting"
    );
    assert!(std::path::Path::new("/dev/fuse").exists());
    let path = std::env::temp_dir().join(format!("hashtree-fuse-native-{}", std::process::id()));
    fs::create_dir(&path).expect("create exclusive mount directory");
    let directory = MountDirectory(path);
    let store = Arc::new(MemoryStore::new());
    let tree = HashTree::new(HashTreeConfig::new(store.clone()));
    let root = block_on(tree.put_directory(Vec::new())).unwrap();
    let filesystem = HashtreeFuse::new(store.clone(), root).unwrap();
    let mounted = Mounted::new(filesystem.clone(), &directory.0);

    fs::create_dir(directory.0.join("docs")).unwrap();
    let draft = directory.0.join("docs/draft.txt");
    fs::write(&draft, b"first").unwrap();
    assert_eq!(fs::read(&draft).unwrap(), b"first");
    fs::write(&draft, b"updated").unwrap();
    let renamed = directory.0.join("docs/renamed.txt");
    fs::rename(&draft, &renamed).unwrap();
    assert_eq!(fs::read(&renamed).unwrap(), b"updated");
    assert_eq!(
        fs::read(&draft).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    assert!(fs::remove_dir(directory.0.join("docs")).is_err());
    fs::remove_file(&renamed).unwrap();
    fs::remove_dir(directory.0.join("docs")).unwrap();

    fs::write(directory.0.join("cached.txt"), b"old").unwrap();
    fs::write(directory.0.join("removed.txt"), b"gone").unwrap();
    assert_eq!(fs::read(directory.0.join("cached.txt")).unwrap(), b"old");
    assert!(fs::metadata(directory.0.join("removed.txt")).is_ok());
    assert!(fs::metadata(directory.0.join("added.txt")).is_err());
    let remote = HashtreeFuse::new(store, filesystem.current_root()).unwrap();
    let cached = remote.lookup_child(ROOT_INODE, "cached.txt").unwrap();
    remote.write_file(cached.inode, 0, b"new").unwrap();
    remote.unlink(ROOT_INODE, "removed.txt").unwrap();
    let added = remote.create_file(ROOT_INODE, "added.txt").unwrap();
    remote.write_file(added.inode, 0, b"remote").unwrap();
    filesystem.replace_root(remote.current_root()).unwrap();
    assert_eq!(fs::read(directory.0.join("cached.txt")).unwrap(), b"new");
    assert_eq!(fs::read(directory.0.join("added.txt")).unwrap(), b"remote");
    assert_eq!(
        fs::metadata(directory.0.join("removed.txt"))
            .unwrap_err()
            .kind(),
        io::ErrorKind::NotFound
    );
    drop(mounted);
    assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 0);

    let remounted = Mounted::new(filesystem.clone(), &directory.0);
    assert_eq!(fs::read(directory.0.join("added.txt")).unwrap(), b"remote");
    drop(remounted);
    assert!(fuser::Session::new(filesystem, &directory.0.join("missing"), &[]).is_err());
    fs::remove_dir(&directory.0).expect("owned mount directory removed after normal unmount");
    println!("FUSE_NATIVE_ACCEPTANCE: crud=pass remote_root=pass remount=pass errors=pass normal_unmount=pass owned_mount_directory_removed=true");
}
