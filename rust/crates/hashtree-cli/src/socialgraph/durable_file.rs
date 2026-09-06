//! Windows metadata publication under the profile root-pair transaction lock.

use std::fs;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// Synchronize a same-directory rename. Replacement callers have already
/// synchronized and closed the source file; deletion callers move an existing
/// authoritative file out of its canonical name before discarding its bytes.
pub(super) fn rename(source: &Path, destination: &Path) -> io::Result<()> {
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        #[link_name = "MoveFileExW"]
        fn move_file_ex(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }

    if source.parent() != destination.parent() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "durable metadata rename must stay in the same directory",
        ));
    }
    let parent = destination.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "metadata path has no parent")
    })?;
    // Canonicalize only the parent: the destination need not exist. The verbatim
    // absolute prefix and UTF-16 preserve Windows long paths and Unicode names.
    let parent = fs::canonicalize(parent)?;
    let wide_path = |path: &Path| -> io::Result<Vec<u16>> {
        let name = path.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "metadata path has no name")
        })?;
        let mut wide: Vec<u16> = parent.join(name).as_os_str().encode_wide().collect();
        if wide.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path contains NUL",
            ));
        }
        wide.push(0);
        Ok(wide)
    };
    let from = wide_path(source)?;
    let to = wide_path(destination)?;
    // WRITE_THROUGH waits for the move to reach disk. COPY_ALLOWED and deferred
    // deletion are deliberately absent: this is one same-directory rename.
    // https://learn.microsoft.com/windows/win32/api/winbase/nf-winbase-movefileexw
    // SAFETY: Both buffers live through the call, contain terminated UTF-16 with
    // no interior NUL, and the flags are documented MoveFileExW values.
    if unsafe {
        move_file_ex(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn tombstone_path(path: &Path) -> io::Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "metadata path has no name"))?;
    let mut tombstone = std::ffi::OsString::from(".");
    tombstone.push(name);
    tombstone.push(".deleted");
    Ok(path.with_file_name(tombstone))
}

pub(super) fn remove(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
        Ok(metadata) if metadata.is_dir() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot remove a metadata directory as a file",
            ));
        }
        Ok(_) => {}
    }
    let tombstone = tombstone_path(path)?;
    // The transaction lock serializes this deterministic sidecar. Replacing a
    // leftover tombstone is safe: recovery reads only the canonical filenames.
    rename(path, &tombstone)?;
    // The canonical name is now durably absent. A crash during this cleanup may
    // leave an ignored tombstone, but cannot restore a committed journal marker.
    // DeleteFile alone has no write-through flag; do not use it as the durable
    // transition. Errors still propagate so a failed cleanup is visible.
    fs::remove_file(tombstone)
}
