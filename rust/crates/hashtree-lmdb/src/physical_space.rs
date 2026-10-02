//! Opt-in admission at actual local write boundaries, independent of blob size.
//!
//! The full filesystem-block-rounded write range is charged, even for writes
//! that reuse allocated pages. A 16 MiB margin covers ordinary allocation and
//! namespace metadata beyond that data range. This is a fail-closed operational
//! guard, not a filesystem quota: unrelated processes and filesystem-specific
//! allocation internals cannot be reserved by statvfs. No guarded data syscall
//! exceeds 64 MiB; external files are admitted in at most 1 MiB chunks.

use std::fs::File;
use std::io::{self, Seek, Write};
use std::path::Path;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;

pub const PHYSICAL_SPACE_METADATA_MARGIN: u64 = 16 * 1024 * 1024;
pub const MAX_GUARDED_WRITE_BYTES: usize = 64 * 1024 * 1024;
const EXTERNAL_WRITE_BYTES: usize = 1024 * 1024;

#[derive(Clone)]
pub struct PhysicalSpaceGuard {
    inner: Arc<GuardInner>,
    #[cfg(unix)]
    admission: heed::WriteAdmission,
}

struct GuardInner {
    minimum_free_bytes: u64,
    refusal: AtomicI32,
}

impl std::fmt::Debug for PhysicalSpaceGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhysicalSpaceGuard")
            .field("minimum_free_bytes", &self.inner.minimum_free_bytes)
            .finish()
    }
}

impl PhysicalSpaceGuard {
    pub fn new(minimum_free_bytes: u64) -> io::Result<Self> {
        #[cfg(unix)]
        {
            let inner = Arc::new(GuardInner {
                minimum_free_bytes,
                refusal: AtomicI32::new(0),
            });
            let callback = Arc::clone(&inner);
            let admission = heed::WriteAdmission::new(move |fd, offset, length| {
                callback.admit(fd, offset, length)
            });
            Ok(Self { inner, admission })
        }
        #[cfg(not(unix))]
        {
            let _ = minimum_free_bytes;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "physical write admission requires Unix descriptors",
            ))
        }
    }

    /// Any admission failure ends this guarded writer; a fresh process may retry.
    pub fn has_refused(&self) -> bool {
        self.inner.refusal.load(Ordering::Acquire) != 0
    }

    pub fn refusal_error(&self) -> io::Error {
        io::Error::from_raw_os_error(self.inner.refusal.load(Ordering::Acquire).max(1))
    }

    pub(crate) fn stop_after_write_error(&self, error: &hashtree_core::store::StoreError) -> bool {
        if let hashtree_core::store::StoreError::Io(error) = error {
            self.inner
                .refusal
                .compare_exchange(
                    0,
                    error.raw_os_error().unwrap_or(libc::EIO),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .ok();
        }
        self.has_refused()
    }

    pub fn status(&self) -> String {
        format!("physical-space admission: floor={} metadata_margin={} max_write_quantum={} latched_errno={}",
            self.inner.minimum_free_bytes, PHYSICAL_SPACE_METADATA_MARGIN, MAX_GUARDED_WRITE_BYTES,
            self.inner.refusal.load(Ordering::Acquire))
    }

    pub fn install(&self, options: &mut heed::EnvOpenOptions) -> io::Result<()> {
        #[cfg(unix)]
        {
            options.write_admission(self.admission.clone());
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = options;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "physical write admission requires Unix descriptors",
            ))
        }
    }

    /// Admit each namespace change on its actual parent filesystem. Existing
    /// directories require no admission, preserving duplicate/no-op operation.
    pub fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        if path.is_dir() {
            return Ok(());
        }
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        self.create_dir_all(parent)?;
        self.admit_file(&File::open(parent)?, 0, 1)?;
        match std::fs::create_dir(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub fn admit_file(&self, file: &File, offset: u64, length: usize) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            self.inner.admit(file.as_raw_fd(), offset, length)
        }
        #[cfg(not(unix))]
        {
            let _ = (file, offset, length);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "physical write admission requires Unix descriptors",
            ))
        }
    }

    pub fn write_all(&self, file: &mut File, data: &[u8]) -> io::Result<()> {
        for chunk in data.chunks(EXTERNAL_WRITE_BYTES) {
            let offset = file.stream_position()?;
            self.admit_file(file, offset, chunk.len())?;
            file.write_all(chunk)?;
        }
        Ok(())
    }
}

impl GuardInner {
    #[cfg(unix)]
    fn admit(&self, fd: std::os::fd::RawFd, offset: u64, length: usize) -> io::Result<()> {
        let refused = self.refusal.load(Ordering::Acquire);
        if refused != 0 {
            return Err(io::Error::from_raw_os_error(refused));
        }
        let result = self.admit_checked(fd, offset, length);
        if let Err(error) = &result {
            self.refusal
                .compare_exchange(
                    0,
                    error.raw_os_error().unwrap_or(libc::EIO),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .ok();
        }
        result
    }

    #[cfg(unix)]
    fn admit_checked(&self, fd: std::os::fd::RawFd, offset: u64, length: usize) -> io::Result<()> {
        if length == 0 {
            return Ok(());
        }
        if length > MAX_GUARDED_WRITE_BYTES {
            return Err(io::Error::from_raw_os_error(libc::EFBIG));
        }
        let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        // fd is owned by the active LMDB/file write and remains valid here.
        if unsafe { libc::fstatvfs(fd, stats.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let stats = unsafe { stats.assume_init() };
        let block = stats.f_frsize as u64;
        if block == 0 {
            return Err(io::Error::from_raw_os_error(libc::EIO));
        }
        let available = (stats.f_bavail as u64).saturating_mul(block);
        let touched = (offset % block)
            .checked_add(length as u64)
            .and_then(|n| n.checked_add(block - 1))
            .map(|n| n / block * block)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EFBIG))?;
        let required = self
            .minimum_free_bytes
            .checked_add(PHYSICAL_SPACE_METADATA_MARGIN)
            .and_then(|n| n.checked_add(touched))
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOSPC))?;
        if available < required {
            return Err(io::Error::from_raw_os_error(libc::ENOSPC));
        }
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    #[test]
    fn failed_filesystem_query_latches_and_cannot_be_retried_as_a_write() {
        let guard = PhysicalSpaceGuard::new(0).unwrap();
        let error = guard.inner.admit(-1, 0, 1).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EBADF));
        let temp = tempfile::tempfile().unwrap();
        assert_eq!(
            guard
                .inner
                .admit(temp.as_raw_fd(), 0, 1)
                .unwrap_err()
                .raw_os_error(),
            Some(libc::EBADF)
        );
        assert!(guard.has_refused());
        assert!(guard.status().contains("latched_errno=9"));
    }

    #[test]
    fn unsupported_write_quantum_is_rejected_before_any_write() {
        let guard = PhysicalSpaceGuard::new(0).unwrap();
        let file = tempfile::tempfile().unwrap();
        let error = guard
            .admit_file(&file, 0, MAX_GUARDED_WRITE_BYTES + 1)
            .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EFBIG));
        assert_eq!(file.metadata().unwrap().len(), 0);
    }
}
