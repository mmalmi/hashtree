use anyhow::{bail, Context, Result};
use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

use super::pool_migration_protocol::FileIdentityV3;

const ATTEMPT_NAMESPACE_NAME: &str = "attempts-v3";

pub(super) struct PinnedDirectory {
    file: File,
    pub(super) path: PathBuf,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

pub(super) struct PinnedRegularEntry {
    pub(super) file: File,
    pub(super) name: OsString,
    #[cfg(unix)]
    pub(super) device: u64,
    #[cfg(unix)]
    pub(super) inode: u64,
}

pub(super) struct PinnedStagedFile {
    directory: File,
    directory_path: PathBuf,
    pub(super) file: File,
    name: OsString,
    identity: FileIdentityV3,
    published: bool,
}

impl PinnedDirectory {
    pub(super) fn open_exact(path: &Path, label: &str) -> Result<Self> {
        require_absolute(path, label)?;
        let canonical = path
            .canonicalize()
            .with_context(|| format!("canonicalize {label} {}", path.display()))?;
        if canonical != path {
            bail!(
                "{label} must be an exact canonical path (got {}, canonical {})",
                path.display(),
                canonical.display()
            );
        }
        #[cfg(unix)]
        let file = open_absolute_directory_without_symlinks(path, label)?;
        #[cfg(not(unix))]
        let file = File::open(path).with_context(|| format!("open {label} {}", path.display()))?;
        let metadata = file
            .metadata()
            .with_context(|| format!("inspect open {label} {}", path.display()))?;
        if !metadata.is_dir() {
            bail!("{label} {} is not a directory", path.display());
        }
        let pinned = Self {
            file,
            path: path.to_path_buf(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
        };
        pinned.ensure_path_identity(label)?;
        Ok(pinned)
    }

    pub(super) fn ensure_path_identity(&self, label: &str) -> Result<()> {
        let current = std::fs::symlink_metadata(&self.path)
            .with_context(|| format!("reinspect {label} {}", self.path.display()))?;
        if !current.file_type().is_dir() {
            bail!("{label} {} is no longer a directory", self.path.display());
        }
        #[cfg(unix)]
        if current.dev() != self.device || current.ino() != self.inode {
            bail!("{label} path changed while the launch was being validated");
        }
        Ok(())
    }

    pub(super) fn metadata(&self, label: &str) -> Result<std::fs::Metadata> {
        self.file
            .metadata()
            .with_context(|| format!("inspect open {label} {}", self.path.display()))
    }

    pub(super) fn same_object(&self, other: &Self) -> bool {
        #[cfg(unix)]
        {
            self.device == other.device && self.inode == other.inode
        }
        #[cfg(not(unix))]
        {
            self.path == other.path
        }
    }

    pub(super) fn authority_identity(&self) -> FileIdentityV3 {
        #[cfg(unix)]
        {
            FileIdentityV3 {
                device: self.device,
                inode: self.inode,
            }
        }
        #[cfg(not(unix))]
        {
            FileIdentityV3 {
                device: 0,
                inode: 0,
            }
        }
    }

    pub(super) fn require_authority_identity(
        &self,
        expected: FileIdentityV3,
        label: &str,
    ) -> Result<()> {
        if self.authority_identity() != expected {
            bail!("{label} device/inode differs from controller authority");
        }
        Ok(())
    }

    pub(super) fn runtime_path(&self) -> PathBuf {
        #[cfg(target_os = "linux")]
        {
            PathBuf::from(format!("/proc/self/fd/{}", self.file.as_raw_fd()))
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.path.clone()
        }
    }

    pub(super) fn open_regular_optional(&self, name: &OsStr, label: &str) -> Result<Option<File>> {
        validate_leaf(name, label)?;
        #[cfg(target_os = "linux")]
        {
            match openat2_file(
                self.file.as_raw_fd(),
                name,
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NONBLOCK,
                0,
                label,
            ) {
                Ok(file) => {
                    if !file.metadata()?.is_file() {
                        bail!("{label} is not a regular file");
                    }
                    Ok(Some(file))
                }
                Err(error) if root_io_kind(&error) == Some(ErrorKind::NotFound) => Ok(None),
                Err(error) => Err(error),
            }
        }
        #[cfg(all(unix, not(target_os = "linux")))]
        {
            let name = os_str_to_c_string(name, label)?;
            let raw = unsafe {
                libc::openat(
                    self.file.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
                )
            };
            if raw < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == ErrorKind::NotFound {
                    return Ok(None);
                }
                return Err(error)
                    .with_context(|| format!("open {label} beneath {}", self.path.display()));
            }
            let file = unsafe { File::from_raw_fd(raw) };
            if !file.metadata()?.is_file() {
                bail!("{label} is not a regular file");
            }
            Ok(Some(file))
        }
        #[cfg(not(unix))]
        {
            let path = self.path.join(name);
            match OpenOptions::new().read(true).open(&path) {
                Ok(file) if file.metadata()?.is_file() => Ok(Some(file)),
                Ok(_) => bail!("{label} {} is not a regular file", path.display()),
                Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error.into()),
            }
        }
    }

    pub(super) fn open_regular_authority(
        &self,
        name: &OsStr,
        expected: FileIdentityV3,
        label: &str,
    ) -> Result<File> {
        let file = self
            .open_regular_optional(name, label)?
            .with_context(|| format!("{label} is absent beneath {}", self.path.display()))?;
        let metadata = file
            .metadata()
            .with_context(|| format!("inspect {label}"))?;
        if file_identity(&metadata) != expected {
            bail!("{label} opened-FD identity differs from its authority");
        }
        self.ensure_path_identity(&format!("{label} parent"))?;
        Ok(file)
    }

    pub(super) fn pin_regular(&self, name: &OsStr, label: &str) -> Result<PinnedRegularEntry> {
        let file = self
            .open_regular_optional(name, label)?
            .with_context(|| format!("{label} is absent beneath {}", self.path.display()))?;
        let metadata = file
            .metadata()
            .with_context(|| format!("inspect pinned {label}"))?;
        let entry = PinnedRegularEntry {
            file,
            name: name.to_os_string(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
        };
        entry.ensure_identity(self, label)?;
        Ok(entry)
    }

    pub(super) fn entry_exists(&self, name: &OsStr, label: &str) -> Result<bool> {
        validate_leaf(name, label)?;
        #[cfg(unix)]
        {
            let name = os_str_to_c_string(name, label)?;
            let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
            let status = unsafe {
                libc::fstatat(
                    self.file.as_raw_fd(),
                    name.as_ptr(),
                    stat.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            if status == 0 {
                return Ok(true);
            }
            let error = std::io::Error::last_os_error();
            if error.kind() == ErrorKind::NotFound {
                return Ok(false);
            }
            Err(error).with_context(|| format!("inspect {label} beneath {}", self.path.display()))
        }
        #[cfg(not(unix))]
        match std::fs::symlink_metadata(self.path.join(name)) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    pub(super) fn acquire_exclusive_migration_lease(&self) -> Result<()> {
        self.ensure_path_identity("migration cursor parent")?;
        #[cfg(target_os = "linux")]
        if unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            if error
                .raw_os_error()
                .is_some_and(|code| code == libc::EWOULDBLOCK || code == libc::EAGAIN)
            {
                bail!(
                    "another Pool migration process holds the cursor-parent lease; every attempt sharing this cursor authority must serialize"
                );
            }
            return Err(error).context("acquire Pool migration cursor-parent lease");
        }
        Ok(())
    }

    pub(super) fn create_staged_regular(
        &self,
        name: &OsStr,
        mode: u32,
        label: &str,
    ) -> Result<PinnedStagedFile> {
        validate_leaf(name, label)?;
        if mode & !0o777 != 0 {
            bail!("{label} requested an invalid file mode");
        }
        #[cfg(target_os = "linux")]
        let file = openat2_file(
            self.file.as_raw_fd(),
            name,
            libc::O_CREAT | libc::O_EXCL | libc::O_RDWR | libc::O_CLOEXEC,
            mode,
            label,
        )?;
        #[cfg(all(unix, not(target_os = "linux")))]
        let file = {
            let name_c = os_str_to_c_string(name, label)?;
            let raw = unsafe {
                libc::openat(
                    self.file.as_raw_fd(),
                    name_c.as_ptr(),
                    libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_RDWR
                        | libc::O_CLOEXEC
                        | libc::O_NOFOLLOW,
                    mode,
                )
            };
            if raw < 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("create {label} beneath {}", self.path.display()));
            }
            unsafe { File::from_raw_fd(raw) }
        };
        #[cfg(not(unix))]
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(self.path.join(name))
            .with_context(|| format!("create {label}"))?;
        let identity = file_identity(&file.metadata().context("inspect staged authority file")?);
        Ok(PinnedStagedFile {
            directory: self.file.try_clone().context("clone pinned directory fd")?,
            directory_path: self.path.clone(),
            file,
            name: name.to_os_string(),
            identity,
            published: false,
        })
    }

    pub(super) fn sync(&self, label: &str) -> Result<()> {
        self.file
            .sync_all()
            .with_context(|| format!("fsync pinned {label} directory"))
    }

    pub(super) fn create_durable_exclusive(
        &self,
        name: &OsStr,
        bytes: &[u8],
        label: &str,
    ) -> Result<()> {
        self.create_durable_exclusive_with_mode(name, bytes, label, 0o600)
    }

    pub(super) fn create_durable_exclusive_with_mode(
        &self,
        name: &OsStr,
        bytes: &[u8],
        label: &str,
        mode: u32,
    ) -> Result<()> {
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let temporary = OsString::from(format!(
            ".authority.{}.{}.tmp",
            std::process::id(),
            SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let mut staged = self.create_staged_regular(&temporary, mode, label)?;
        staged.file.write_all(bytes)?;
        staged.file.sync_all()?;
        #[cfg(unix)]
        if unsafe { libc::fchmod(staged.file.as_raw_fd(), mode as libc::mode_t) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("set staged {label} mode"));
        }
        staged.file.sync_all()?;
        staged.publish_noreplace(name, label)?;
        let mut published = self.open_regular_authority(name, staged.identity, label)?;
        let mut actual = Vec::with_capacity(bytes.len());
        Read::by_ref(&mut published)
            .take((bytes.len() as u64).saturating_add(1))
            .read_to_end(&mut actual)?;
        if actual != bytes {
            bail!("published {label} differs from intended bytes");
        }
        Ok(())
    }

    pub(super) fn durable_replace(&self, name: &OsStr, bytes: &[u8], label: &str) -> Result<()> {
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let temporary = OsString::from(format!(
            ".replace.{}.{}.tmp",
            std::process::id(),
            SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let mut staged = self.create_staged_regular(&temporary, 0o600, label)?;
        staged.file.write_all(bytes)?;
        staged.file.sync_all()?;
        #[cfg(unix)]
        if unsafe { libc::fchmod(staged.file.as_raw_fd(), 0o400) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("make staged {label} read-only"));
        }
        staged.file.sync_all()?;
        staged.replace(name, label)?;
        Ok(())
    }

    pub(super) fn durable_remove_regular(&self, name: &OsStr, label: &str) -> Result<()> {
        validate_leaf(name, label)?;
        let file = self
            .open_regular_optional(name, label)?
            .with_context(|| format!("{label} is absent beneath {}", self.path.display()))?;
        if !file.metadata()?.is_file() {
            bail!("{label} is not a regular file");
        }
        #[cfg(unix)]
        {
            let name = os_str_to_c_string(name, label)?;
            if unsafe { libc::unlinkat(self.file.as_raw_fd(), name.as_ptr(), 0) } != 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("remove {label}"));
            }
        }
        #[cfg(not(unix))]
        std::fs::remove_file(self.path.join(name))?;
        self.file
            .sync_all()
            .with_context(|| format!("fsync {label} parent after removal"))
    }
}

impl PinnedRegularEntry {
    pub(super) fn same_object(&self, other: &Self) -> bool {
        #[cfg(unix)]
        {
            self.device == other.device && self.inode == other.inode && self.name == other.name
        }
        #[cfg(not(unix))]
        {
            self.name == other.name
        }
    }

    pub(super) fn ensure_identity(&self, directory: &PinnedDirectory, label: &str) -> Result<()> {
        let opened = self
            .file
            .metadata()
            .with_context(|| format!("reinspect pinned {label}"))?;
        #[cfg(unix)]
        if opened.dev() != self.device || opened.ino() != self.inode {
            bail!("{label} open file identity changed");
        }
        let current = directory
            .open_regular_optional(&self.name, label)?
            .with_context(|| format!("{label} disappeared after it was pinned"))?
            .metadata()
            .with_context(|| format!("reinspect {label} directory entry"))?;
        #[cfg(unix)]
        if current.dev() != self.device || current.ino() != self.inode {
            bail!("{label} directory entry changed after it was pinned");
        }
        #[cfg(not(unix))]
        if !current.is_file() {
            bail!("{label} is no longer a regular file");
        }
        Ok(())
    }
}

impl PinnedStagedFile {
    pub(super) fn identity(&self) -> FileIdentityV3 {
        self.identity
    }

    pub(super) fn publish_noreplace(&mut self, target: &OsStr, label: &str) -> Result<()> {
        self.rename(target, true, label)
    }

    pub(super) fn replace(&mut self, target: &OsStr, label: &str) -> Result<()> {
        self.rename(target, false, label)
    }

    fn rename(&mut self, target: &OsStr, no_replace: bool, label: &str) -> Result<()> {
        validate_leaf(target, label)?;
        self.validate_staging_identity(label)?;
        #[cfg(target_os = "linux")]
        {
            let source = os_str_to_c_string(&self.name, label)?;
            let target = os_str_to_c_string(target, label)?;
            let flags = if no_replace {
                libc::RENAME_NOREPLACE
            } else {
                0
            };
            let status = unsafe {
                libc::syscall(
                    libc::SYS_renameat2,
                    self.directory.as_raw_fd(),
                    source.as_ptr(),
                    self.directory.as_raw_fd(),
                    target.as_ptr(),
                    flags,
                )
            };
            if status != 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("publish staged {label}"));
            }
        }
        #[cfg(all(unix, not(target_os = "linux")))]
        {
            let source = os_str_to_c_string(&self.name, label)?;
            let target_c = os_str_to_c_string(target, label)?;
            if no_replace {
                if unsafe {
                    libc::linkat(
                        self.directory.as_raw_fd(),
                        source.as_ptr(),
                        self.directory.as_raw_fd(),
                        target_c.as_ptr(),
                        0,
                    )
                } != 0
                {
                    return Err(std::io::Error::last_os_error())
                        .with_context(|| format!("link staged {label} without replacement"));
                }
                if unsafe { libc::unlinkat(self.directory.as_raw_fd(), source.as_ptr(), 0) } != 0 {
                    return Err(std::io::Error::last_os_error())
                        .with_context(|| format!("unlink staged {label} after publication"));
                }
            } else if unsafe {
                libc::renameat(
                    self.directory.as_raw_fd(),
                    source.as_ptr(),
                    self.directory.as_raw_fd(),
                    target_c.as_ptr(),
                )
            } != 0
            {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("replace staged {label}"));
            }
        }
        #[cfg(not(unix))]
        {
            let source = self.directory_path.join(&self.name);
            let target_path = self.directory_path.join(target);
            if no_replace {
                std::fs::hard_link(&source, &target_path)?;
                std::fs::remove_file(&source)?;
            } else {
                std::fs::rename(&source, &target_path)?;
            }
        }
        self.directory
            .sync_all()
            .with_context(|| format!("fsync {label} parent after publication"))?;
        self.published = true;
        Ok(())
    }

    fn validate_staging_identity(&self, label: &str) -> Result<()> {
        let current = entry_identity(&self.directory, &self.name, label)?
            .context("staged authority entry disappeared")?;
        if current != self.identity
            || file_identity(&self.file.metadata().context("inspect staged open file")?)
                != self.identity
        {
            bail!("staged {label} name no longer resolves to its exact created inode");
        }
        Ok(())
    }
}

impl Drop for PinnedStagedFile {
    fn drop(&mut self) {
        if self.published {
            return;
        }
        if entry_identity(&self.directory, &self.name, "staged cleanup")
            .ok()
            .flatten()
            != Some(self.identity)
        {
            return;
        }
        #[cfg(unix)]
        if let Ok(name) = os_str_to_c_string(&self.name, "staged cleanup") {
            unsafe {
                libc::unlinkat(self.directory.as_raw_fd(), name.as_ptr(), 0);
            }
        }
        #[cfg(not(unix))]
        {
            let _ = std::fs::remove_file(self.directory_path.join(&self.name));
        }
    }
}

fn validate_leaf(name: &OsStr, label: &str) -> Result<()> {
    if name.is_empty() || Path::new(name).components().count() != 1 {
        bail!("{label} name must be one nonempty path component");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn openat2_file(dirfd: i32, name: &OsStr, flags: i32, mode: u32, label: &str) -> Result<File> {
    let name = os_str_to_c_string(name, label)?;
    let mut how = unsafe { std::mem::zeroed::<libc::open_how>() };
    how.flags = flags as u64;
    how.mode = mode as u64;
    how.resolve =
        (libc::RESOLVE_BENEATH | libc::RESOLVE_NO_SYMLINKS | libc::RESOLVE_NO_MAGICLINKS) as u64;
    let raw = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            dirfd,
            name.as_ptr(),
            &how,
            std::mem::size_of::<libc::open_how>(),
        )
    } as i32;
    if raw < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("openat2 {label} beneath pinned directory"));
    }
    Ok(unsafe { File::from_raw_fd(raw) })
}

#[cfg(unix)]
fn entry_identity(directory: &File, name: &OsStr, label: &str) -> Result<Option<FileIdentityV3>> {
    let name = os_str_to_c_string(name, label)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
    let status = unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if status != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(error).with_context(|| format!("inspect staged {label} entry"));
    }
    let stat = unsafe { stat.assume_init() };
    Ok(Some(FileIdentityV3 {
        device: stat.st_dev as u64,
        inode: stat.st_ino as u64,
    }))
}

#[cfg(not(unix))]
fn entry_identity(directory: &File, name: &OsStr, label: &str) -> Result<Option<FileIdentityV3>> {
    let _ = (directory, name, label);
    Ok(None)
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

fn root_io_kind(error: &anyhow::Error) -> Option<ErrorKind> {
    error.chain().find_map(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .map(std::io::Error::kind)
    })
}

fn require_absolute(path: &Path, label: &str) -> Result<()> {
    if !path.is_absolute() {
        bail!("{label} must be absolute: {}", path.display());
    }
    Ok(())
}

#[cfg(unix)]
fn open_absolute_directory_without_symlinks(path: &Path, label: &str) -> Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW);
    let mut directory = options
        .open(Path::new("/"))
        .with_context(|| format!("open filesystem root while resolving {label}"))?;
    for component in path.components() {
        match component {
            std::path::Component::RootDir => {}
            std::path::Component::Normal(name) => {
                let name = os_str_to_c_string(name, label)?;
                let raw = unsafe {
                    libc::openat(
                        directory.as_raw_fd(),
                        name.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                    )
                };
                if raw < 0 {
                    return Err(std::io::Error::last_os_error()).with_context(|| {
                        format!("open trusted {label} component {}", path.display())
                    });
                }
                directory = unsafe { File::from_raw_fd(raw) };
            }
            _ => bail!("{label} path contains a non-canonical component"),
        }
    }
    Ok(directory)
}

#[cfg(unix)]
fn os_str_to_c_string(value: &OsStr, label: &str) -> Result<std::ffi::CString> {
    std::ffi::CString::new(value.as_bytes())
        .with_context(|| format!("{label} path component contains NUL"))
}
