use super::ProfileRootPairLockMode;
use std::fs::{File, TryLockError};
use std::io;

#[cfg(not(target_os = "android"))]
pub(super) fn try_lock(file: &File, mode: ProfileRootPairLockMode) -> Result<(), TryLockError> {
    match mode {
        ProfileRootPairLockMode::Shared => file.try_lock_shared(),
        ProfileRootPairLockMode::Exclusive => file.try_lock(),
    }
}

#[cfg(not(target_os = "android"))]
pub(super) fn unlock(file: &File) -> io::Result<()> {
    file.unlock()
}

// Rust 1.95's std file locks return Unsupported on Android. Use the same flock
// operations as std on Linux until our minimum toolchain includes rust#157038.
#[cfg(target_os = "android")]
pub(super) fn try_lock(file: &File, mode: ProfileRootPairLockMode) -> Result<(), TryLockError> {
    let operation = match mode {
        ProfileRootPairLockMode::Shared => libc::LOCK_SH,
        ProfileRootPairLockMode::Exclusive => libc::LOCK_EX,
    };
    flock(file, operation | libc::LOCK_NB).map_err(|error| {
        if error.kind() == io::ErrorKind::WouldBlock {
            TryLockError::WouldBlock
        } else {
            TryLockError::Error(error)
        }
    })
}

#[cfg(target_os = "android")]
pub(super) fn unlock(file: &File) -> io::Result<()> {
    flock(file, libc::LOCK_UN)
}

#[cfg(target_os = "android")]
fn flock(file: &File, operation: libc::c_int) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    // SAFETY: the borrowed File keeps this descriptor valid for the call. flock
    // does not take ownership, and operation is a LOCK_* combination above.
    if unsafe { libc::flock(file.as_raw_fd(), operation) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
