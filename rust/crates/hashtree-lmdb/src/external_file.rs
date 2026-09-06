use std::fs;
#[cfg(not(windows))]
use std::fs::File;
use std::io;
use std::path::Path;

/// Publish a closed temporary file whose contents have already been synchronized
/// when `sync` is enabled.
pub(super) fn publish(temp: &Path, path: &Path, sync: bool) -> io::Result<()> {
    #[cfg(windows)]
    if sync {
        return publish_windows_sync(temp, path);
    }

    fs::rename(temp, path)?;
    #[cfg(not(windows))]
    if sync {
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file has no parent"))?;
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(windows)]
fn publish_windows_sync(temp: &Path, path: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        #[link_name = "MoveFileExW"]
        fn move_file_ex(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }

    if temp.parent() != path.parent() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "external temporary file must share its destination directory",
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file has no parent"))?;
    // Windows canonicalization supplies an absolute verbatim path, preserving
    // Unicode and long paths without requiring the destination to exist.
    let parent = fs::canonicalize(parent)?;
    let wide_path = |path: &Path| -> io::Result<Vec<u16>> {
        let name = path
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file has no name"))?;
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
    let from = wide_path(temp)?;
    let to = wide_path(path)?;
    // WRITE_THROUGH waits for the move to reach disk. COPY_ALLOWED is omitted:
    // publication must remain a same-directory move, never a copy/delete.
    // https://learn.microsoft.com/windows/win32/api/winbase/nf-winbase-movefileexw
    // SAFETY: Both paths are live, NUL-terminated UTF-16 buffers without interior
    // NULs, and the flags are documented MoveFileExW values.
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

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn publication_preserves_long_unicode_paths_and_replaces_existing_files() -> io::Result<()> {
        for sync in [false, true] {
            let root = tempfile::tempdir()?;
            let mut parent = root.path().join("external-\u{e9}-\u{6811}");
            for _ in 0..4 {
                parent.push("segment".repeat(10));
            }
            fs::create_dir_all(&parent)?;
            let path = parent.join("blob-\u{e9}-\u{6811}");
            let temp = parent.join("temporary-\u{e9}-\u{6811}");
            fs::write(&path, b"old")?;
            fs::write(&temp, b"new")?;
            if sync {
                fs::OpenOptions::new().write(true).open(&temp)?.sync_all()?;
            }
            publish(&temp, &path, sync)?;
            assert_eq!(fs::read(&path)?, b"new");
            assert!(!temp.exists());
        }
        Ok(())
    }

    #[test]
    fn failed_publication_preserves_the_temporary_file() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("existing-directory");
        let temp = root.path().join("temporary");
        fs::create_dir(&path)?;
        fs::write(&temp, b"retained")?;
        fs::OpenOptions::new().write(true).open(&temp)?.sync_all()?;
        assert!(publish(&temp, &path, true).is_err());
        assert_eq!(fs::read(&temp)?, b"retained");
        assert!(path.is_dir());
        Ok(())
    }
}
