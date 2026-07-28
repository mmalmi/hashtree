use hashtree_core::lmdb_runtime::{
    acquire_lmdb_write_permit, with_lmdb_lock_acquisition, LmdbWritePermit,
};
use heed::{Env, EnvOpenOptions, PinnedLmdbIdentity, RoTxn, RwTxn};
use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

static ENV_REFERENCES: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();

fn env_references() -> &'static Mutex<HashMap<PathBuf, usize>> {
    ENV_REFERENCES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Owns one process-local reference to a Heed environment.
///
/// Heed retains a strong global reference after ordinary `Env` handles are
/// dropped. The last managed owner must explicitly prepare the environment for
/// closing so LMDB releases file descriptors and, on macOS, named semaphores.
pub(crate) struct ManagedEnv {
    env: Option<Env>,
    path: PathBuf,
}

impl ManagedEnv {
    /// Open and register an environment as one atomic managed operation.
    ///
    /// # Safety
    ///
    /// This has the same safety requirements as [`EnvOpenOptions::open`].
    pub(crate) unsafe fn open<P: AsRef<Path>>(
        options: &EnvOpenOptions,
        path: P,
    ) -> Result<Self, heed::Error> {
        let mut references = env_references()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let path = path.as_ref();
        let env = with_lmdb_lock_acquisition(|| unsafe { options.open(path) })?;
        Self::register_open_env(&mut references, env)
    }

    /// Open through a retained Linux `/proc/self/fd/N` directory while
    /// requiring LMDB's actual data and lock descriptors to match identities
    /// captured by the authority-establishing caller.
    ///
    /// On non-Linux development hosts the normal exact path is used; the
    /// production migration launcher is Linux-only.
    ///
    /// # Safety
    ///
    /// This has the same safety requirements as [`EnvOpenOptions::open`].
    pub(crate) unsafe fn open_pinned<P: AsRef<Path>>(
        options: &EnvOpenOptions,
        path: P,
        identity: PinnedLmdbIdentity,
    ) -> Result<Self, heed::Error> {
        let mut references = env_references()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let path = path.as_ref();
        #[cfg(target_os = "linux")]
        let env = {
            if !is_linux_proc_fd_directory(path) {
                return Err(heed::Error::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "pinned LMDB open requires an exact /proc/self/fd/N directory",
                )));
            }
            with_lmdb_lock_acquisition(|| unsafe { options.open_from_pinned_path(path, identity) })?
        };
        #[cfg(not(target_os = "linux"))]
        let env = {
            let _ = identity;
            with_lmdb_lock_acquisition(|| unsafe { options.open(path) })?
        };
        Self::register_open_env(&mut references, env)
    }

    fn register_open_env(
        references: &mut HashMap<PathBuf, usize>,
        env: Env,
    ) -> Result<Self, heed::Error> {
        let path = env.path().to_path_buf();
        let count = references.entry(path.clone()).or_default();
        *count = count
            .checked_add(1)
            .expect("managed LMDB environment reference count overflow");
        Ok(Self {
            env: Some(env),
            path,
        })
    }

    pub(crate) fn read_txn(&self) -> heed::Result<RoTxn<'_>> {
        with_lmdb_lock_acquisition(|| self.deref().read_txn())
    }

    pub(crate) fn write_txn(&self) -> heed::Result<ManagedRwTxn<'_>> {
        let permit = acquire_lmdb_write_permit();
        let txn = self.deref().write_txn()?;
        Ok(ManagedRwTxn {
            txn: Some(txn),
            _permit: permit,
        })
    }
}

#[cfg(target_os = "linux")]
fn is_linux_proc_fd_directory(path: &Path) -> bool {
    let mut components = path.components();
    if components.next() != Some(std::path::Component::RootDir) {
        return false;
    }
    for expected in ["proc", "self", "fd"] {
        if components
            .next()
            .and_then(|component| component.as_os_str().to_str())
            != Some(expected)
        {
            return false;
        }
    }
    let Some(descriptor) = components
        .next()
        .and_then(|component| component.as_os_str().to_str())
    else {
        return false;
    };
    !descriptor.is_empty()
        && descriptor.bytes().all(|byte| byte.is_ascii_digit())
        && components.next().is_none()
}

pub(crate) struct ManagedRwTxn<'env> {
    txn: Option<RwTxn<'env>>,
    _permit: LmdbWritePermit,
}

impl ManagedRwTxn<'_> {
    pub(crate) fn commit(mut self) -> heed::Result<()> {
        self.txn
            .take()
            .expect("managed LMDB transaction accessed after completion")
            .commit()
    }
}

impl<'env> Deref for ManagedRwTxn<'env> {
    type Target = RwTxn<'env>;

    fn deref(&self) -> &Self::Target {
        self.txn
            .as_ref()
            .expect("managed LMDB transaction accessed after completion")
    }
}

impl DerefMut for ManagedRwTxn<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.txn
            .as_mut()
            .expect("managed LMDB transaction accessed after completion")
    }
}

impl Deref for ManagedEnv {
    type Target = Env;

    fn deref(&self) -> &Self::Target {
        self.env
            .as_ref()
            .expect("managed LMDB environment accessed after drop")
    }
}

impl Drop for ManagedEnv {
    fn drop(&mut self) {
        let Some(env) = self.env.take() else {
            return;
        };
        let mut references = env_references()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let count = references
            .get_mut(&self.path)
            .expect("managed LMDB environment reference was not registered");
        *count = count
            .checked_sub(1)
            .expect("managed LMDB environment reference count underflow");
        if *count == 0 {
            references.remove(&self.path);
            // Keep the registry locked until Heed has removed its cached strong
            // reference, preventing a managed reopen from racing the final close.
            let _closing = env.prepare_for_closing();
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;
    use std::process::Command;
    use std::thread;
    use std::time::{Duration, Instant};

    const RACE_HELPER_PATH: &str = "HASHTREE_PINNED_OPEN_RACE_PATH";
    const RACE_HELPER_LEAF: &str = "HASHTREE_PINNED_OPEN_RACE_LEAF";
    const RACE_READY_PATH: &str = "HASHTREE_PINNED_OPEN_RACE_READY";
    const RACE_GO_PATH: &str = "HASHTREE_PINNED_OPEN_RACE_GO";

    fn options() -> EnvOpenOptions {
        let mut options = EnvOpenOptions::new();
        options.map_size(16 * 1024 * 1024).max_dbs(1);
        options
    }

    fn initialize(path: &Path) {
        fs::create_dir(path).expect("create environment");
        let env = unsafe { ManagedEnv::open(&options(), path) }.expect("initialize LMDB");
        drop(env);
    }

    fn identity(path: &Path) -> PinnedLmdbIdentity {
        let data = fs::metadata(path.join("data.mdb")).expect("data metadata");
        let lock = fs::metadata(path.join("lock.mdb")).expect("lock metadata");
        PinnedLmdbIdentity {
            data: heed::PinnedLmdbFileIdentity {
                device: data.dev(),
                inode: data.ino(),
            },
            lock: heed::PinnedLmdbFileIdentity {
                device: lock.dev(),
                inode: lock.ino(),
            },
        }
    }

    #[test]
    fn pinned_proc_fd_open_survives_directory_path_replacement() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().canonicalize().expect("canonical tempdir");
        let original = root.join("environment");
        let retained = root.join("retained-environment");
        initialize(&original);
        let expected = identity(&original);
        let directory = fs::File::open(&original).expect("pin environment directory");
        let proc_path = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));

        fs::rename(&original, &retained).expect("rename pinned environment");
        fs::create_dir(&original).expect("create pathname replacement");

        let env = unsafe { ManagedEnv::open_pinned(&options(), &proc_path, expected) }
            .expect("open LMDB through retained procfd path");
        drop(env);

        assert!(
            retained.join("data.mdb").is_file(),
            "LMDB data must be opened beneath the retained directory"
        );
        assert!(
            retained.join("lock.mdb").is_file(),
            "LMDB lock must be opened beneath the retained directory"
        );
        assert!(
            !original.join("data.mdb").exists() && !original.join("lock.mdb").exists(),
            "the replacement pathname must remain untouched"
        );
    }

    #[test]
    fn pinned_open_rejects_replaced_data_and_lock_inodes() {
        for name in ["data.mdb", "lock.mdb"] {
            let temp = tempfile::tempdir().expect("tempdir");
            let path = temp.path().join("environment");
            initialize(&path);
            let expected = identity(&path);
            let directory = fs::File::open(&path).expect("pin environment directory");
            let proc_path = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
            let original = path.join(name);
            let retained = path.join(format!("{name}.retained"));
            fs::rename(&original, &retained).expect("retain original LMDB file");
            fs::copy(&retained, &original).expect("install different regular inode");

            let error = unsafe { ManagedEnv::open_pinned(&options(), &proc_path, expected) }
                .err()
                .expect("replaced LMDB inode must fail closed");
            assert!(
                error.to_string().contains("caller-pinned identity"),
                "unexpected error for {name}: {error}"
            );
        }
    }

    #[test]
    fn c_open_rejects_data_and_lock_swapped_after_rust_precheck() {
        let harness = tempfile::tempdir().expect("harness tempdir");
        let shim_source = harness.path().join("open-race.c");
        let shim_library = harness.path().join("open-race.so");
        fs::write(
            &shim_source,
            br#"
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

static volatile int blocked = 0;

static int should_block(const char *path) {
    const char *leaf = getenv("HASHTREE_PINNED_OPEN_RACE_LEAF");
    if (!path || !leaf || !*leaf) return 0;
    size_t path_len = strlen(path);
    size_t leaf_len = strlen(leaf);
    return path_len >= leaf_len &&
        strcmp(path + path_len - leaf_len, leaf) == 0 &&
        __sync_bool_compare_and_swap(&blocked, 0, 1);
}

static void rendezvous(void) {
    const char *ready = getenv("HASHTREE_PINNED_OPEN_RACE_READY");
    const char *go = getenv("HASHTREE_PINNED_OPEN_RACE_GO");
    int fd = syscall(SYS_openat, AT_FDCWD, ready,
        O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC, 0600);
    if (fd >= 0) {
        syscall(SYS_write, fd, "ready\n", 6);
        syscall(SYS_close, fd);
    }
    for (int i = 0; i < 3000 && access(go, F_OK) != 0; ++i) {
        usleep(10000);
    }
}

static mode_t open_mode(int flags, va_list args) {
    return (flags & O_CREAT) ? (mode_t)va_arg(args, int) : 0;
}

int open(const char *path, int flags, ...) {
    static int (*real_open)(const char *, int, ...) = NULL;
    if (!real_open) real_open = dlsym(RTLD_NEXT, "open");
    va_list args;
    va_start(args, flags);
    mode_t mode = open_mode(flags, args);
    va_end(args);
    if (should_block(path)) rendezvous();
    return (flags & O_CREAT) ? real_open(path, flags, mode) : real_open(path, flags);
}

int open64(const char *path, int flags, ...) {
    static int (*real_open64)(const char *, int, ...) = NULL;
    if (!real_open64) real_open64 = dlsym(RTLD_NEXT, "open64");
    va_list args;
    va_start(args, flags);
    mode_t mode = open_mode(flags, args);
    va_end(args);
    if (should_block(path)) rendezvous();
    return (flags & O_CREAT) ? real_open64(path, flags, mode) : real_open64(path, flags);
}
"#,
        )
        .expect("write generated preload shim");
        let compile = Command::new("cc")
            .args(["-shared", "-fPIC", "-O2"])
            .arg(&shim_source)
            .args(["-ldl", "-o"])
            .arg(&shim_library)
            .output()
            .expect("compile generated preload shim");
        assert!(
            compile.status.success(),
            "compile preload shim failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&compile.stdout),
            String::from_utf8_lossy(&compile.stderr)
        );

        for leaf in ["data.mdb", "lock.mdb"] {
            let case = tempfile::tempdir().expect("case tempdir");
            let path = case.path().join("environment");
            initialize(&path);
            let ready = case.path().join("race.ready");
            let go = case.path().join("race.go");
            let mut child = Command::new(std::env::current_exe().expect("test executable"))
                .arg("--ignored")
                .arg("--exact")
                .arg("managed_env::tests::pinned_open_c_enforcement_helper")
                .env("LD_PRELOAD", &shim_library)
                .env(RACE_HELPER_PATH, &path)
                .env(RACE_HELPER_LEAF, leaf)
                .env(RACE_READY_PATH, &ready)
                .env(RACE_GO_PATH, &go)
                .spawn()
                .expect("spawn pinned-open race helper");

            let deadline = Instant::now() + Duration::from_secs(10);
            while !ready.exists() {
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for C open rendezvous for {leaf}"
                );
                thread::sleep(Duration::from_millis(10));
            }

            let original = path.join(leaf);
            let retained = path.join(format!("{leaf}.retained"));
            fs::rename(&original, &retained).expect("retain authorized LMDB leaf");
            fs::copy(&retained, &original).expect("install unauthorized LMDB leaf inode");
            let before = fs::read(&original).expect("snapshot unauthorized LMDB leaf");
            fs::write(&go, b"go\n").expect("release C open rendezvous");

            let status = child.wait().expect("wait for pinned-open race helper");
            assert!(status.success(), "C enforcement helper failed for {leaf}");
            assert_eq!(
                fs::read(&original).expect("reinspect unauthorized LMDB leaf"),
                before,
                "LMDB mutated unauthorized {leaf} before rejecting its identity"
            );
        }
    }

    #[test]
    #[ignore = "subprocess helper for c_open_rejects_data_and_lock_swapped_after_rust_precheck"]
    fn pinned_open_c_enforcement_helper() {
        let Some(path) = std::env::var_os(RACE_HELPER_PATH) else {
            return;
        };
        let path = PathBuf::from(path);
        let expected = identity(&path);
        let directory = fs::File::open(&path).expect("pin helper LMDB directory");
        let proc_path = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
        let error = unsafe { ManagedEnv::open_pinned(&options(), &proc_path, expected) }
            .err()
            .expect("C open accepted an LMDB leaf swapped after the Rust precheck");
        assert!(
            error.to_string().contains("identity")
                || error.to_string().contains("Invalid argument"),
            "unexpected C pinned-open rejection: {error}"
        );
    }

    #[test]
    fn pinned_open_rejects_data_and_lock_symlinks() {
        use std::os::unix::fs::symlink;

        for name in ["data.mdb", "lock.mdb"] {
            let temp = tempfile::tempdir().expect("tempdir");
            let path = temp.path().join("environment");
            initialize(&path);
            let expected = identity(&path);
            let directory = fs::File::open(&path).expect("pin environment directory");
            let proc_path = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
            let original = path.join(name);
            let retained = path.join(format!("{name}.retained"));
            fs::rename(&original, &retained).expect("retain original LMDB file");
            symlink(&retained, &original).expect("install LMDB symlink");

            unsafe { ManagedEnv::open_pinned(&options(), &proc_path, expected) }
                .err()
                .expect("LMDB symlink must fail closed");
        }
    }

    #[test]
    fn ordinary_non_migration_open_remains_compatible() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("ordinary");
        fs::create_dir(&path).expect("create ordinary environment");
        let env = unsafe { ManagedEnv::open(&options(), &path) }.expect("ordinary LMDB open");
        drop(env);
        assert!(path.join("data.mdb").is_file());
        assert!(path.join("lock.mdb").is_file());
    }
}
