//! Platform install dispatchers.
//!
//! Each `install_*` helper takes the downloaded bytes and writes them into
//! place atomically. Strategy is selected from `UpdateAsset::asset_kind()`
//! (or the `kind` argument here for callers that already decoded it).

use std::path::{Path, PathBuf};

use crate::error::UpdateError;
use crate::manifest::{AssetKind, UpdateAsset};

#[derive(Debug, Clone)]
pub struct InstallTarget {
    /// Final path the new binary/bundle should occupy after install.
    /// For `Binary` this is the full file path. For `AppBundle` (macOS) this
    /// is the path to the existing `.app` directory. For `AppImage` this is
    /// the AppImage file.
    pub destination: PathBuf,
    /// If `true`, set the unix executable bit (0o755) on the installed file.
    /// Ignored for kinds where it's implicit (`AppImage`, `AppBundle`).
    pub executable: bool,
}

impl InstallTarget {
    pub fn new(destination: impl Into<PathBuf>) -> Self {
        Self {
            destination: destination.into(),
            executable: false,
        }
    }

    pub fn executable(mut self, value: bool) -> Self {
        self.executable = value;
        self
    }
}

/// Dispatch the install based on the asset's declared kind.
pub fn install(
    asset: &UpdateAsset,
    bytes: &[u8],
    target: &InstallTarget,
) -> Result<(), UpdateError> {
    match asset.asset_kind() {
        AssetKind::Binary => install_binary(&target.destination, bytes, target.executable),
        AssetKind::AppBundle => install_app_bundle(&target.destination, bytes),
        AssetKind::AppImage => install_appimage(&target.destination, bytes),
        AssetKind::BinaryArchive => {
            let entry = asset.executable.as_deref().ok_or_else(|| {
                UpdateError::Install(
                    "binary-archive kind requires asset.executable to name the entry to extract"
                        .to_string(),
                )
            })?;
            install_binary_archive(&target.destination, bytes, entry)
        }
        kind @ (AssetKind::Deb
        | AssetKind::Rpm
        | AssetKind::Nsis
        | AssetKind::Msi
        | AssetKind::Archive) => Err(UpdateError::UnsupportedKind {
            kind: kind.as_str().to_string(),
        }),
    }
}

/// Decompress a `.tar.gz` (or raw tar — auto-detected by gzip magic),
/// find the entry whose path matches `entry_name`, and atomically write
/// its bytes to `destination` with the executable bit set. Cross-platform.
pub fn install_binary_archive(
    destination: &Path,
    bytes: &[u8],
    entry_name: &str,
) -> Result<(), UpdateError> {
    use std::io::{Cursor, Read};

    let payload: Box<dyn Read> = if bytes.starts_with(&[0x1f, 0x8b]) {
        Box::new(flate2::read::GzDecoder::new(Cursor::new(bytes)))
    } else {
        Box::new(Cursor::new(bytes))
    };
    let mut archive = tar::Archive::new(payload);

    for entry in archive
        .entries()
        .map_err(|err| UpdateError::Install(format!("failed to read tar archive: {err}")))?
    {
        let mut entry = entry
            .map_err(|err| UpdateError::Install(format!("failed to read tar entry: {err}")))?;
        let path = entry
            .path()
            .map_err(|err| UpdateError::Install(format!("entry has invalid path: {err}")))?;
        if path.to_string_lossy() == entry_name {
            let mut buf = Vec::with_capacity(entry.header().size().unwrap_or(0) as usize);
            entry.read_to_end(&mut buf).map_err(|err| {
                UpdateError::Install(format!("failed to read entry bytes: {err}"))
            })?;
            return install_binary(destination, &buf, true);
        }
    }
    Err(UpdateError::Install(format!(
        "entry {entry_name} not found in archive"
    )))
}

/// Write `bytes` to `path` via a temp file + atomic rename.
pub fn install_binary(path: &Path, bytes: &[u8], executable: bool) -> Result<(), UpdateError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let staged = stage_file(parent, bytes)?;
    if executable {
        set_executable(staged.as_file())?;
    }
    staged.persist(path).map_err(|err| err.error)?;
    Ok(())
}

/// macOS-only. Decompress `bytes` (expected to be a `.tar.gz` produced by
/// `tauri-bundler` or our own release tooling) into a temp dir, find the
/// `*.app` inside, then atomically swap it into `destination`.
///
/// On non-macOS platforms this returns `UnsupportedKind`.
#[cfg(target_os = "macos")]
pub fn install_app_bundle(destination: &Path, bytes: &[u8]) -> Result<(), UpdateError> {
    use flate2::read::GzDecoder;
    use std::io::Cursor;

    let parent = destination
        .parent()
        .ok_or_else(|| UpdateError::Install("app bundle destination has no parent".to_string()))?;
    std::fs::create_dir_all(parent)?;

    let staging = tempfile::Builder::new()
        .prefix(".hashtree-update-")
        .tempdir_in(parent)?;
    let payload_dir = staging.path().join("payload");
    std::fs::create_dir(&payload_dir)?;

    let cursor = Cursor::new(bytes);
    let gz = GzDecoder::new(cursor);
    let mut archive = tar::Archive::new(gz);
    archive
        .unpack(&payload_dir)
        .map_err(|err| UpdateError::Install(format!("failed to unpack app bundle: {err}")))?;

    let new_app = find_app_dir(&payload_dir)
        .ok_or_else(|| UpdateError::Install("no .app directory found in archive".to_string()))?;

    let backup = staging.path().join("previous.app");

    let backed_up = destination.exists();
    if backed_up {
        if let Err(err) = std::fs::rename(destination, &backup) {
            // Permission denied: ask the user via AppleScript with admin privs.
            return swap_app_with_privs(&new_app, destination, err);
        }
    }

    if let Err(err) = std::fs::rename(&new_app, destination) {
        if backed_up {
            if let Err(restore_err) = std::fs::rename(&backup, destination) {
                // Keep the original bundle recoverable even when rollback fails.
                let recovery_dir = staging.keep();
                return Err(UpdateError::Install(format!(
                    "failed to install new app bundle: {err}; failed to restore original: \
                     {restore_err}; original bundle retained at {}",
                    recovery_dir.join("previous.app").display()
                )));
            }
        }
        return Err(UpdateError::Install(format!(
            "failed to install new app bundle: {err}"
        )));
    }

    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn install_app_bundle(_destination: &Path, _bytes: &[u8]) -> Result<(), UpdateError> {
    Err(UpdateError::UnsupportedKind {
        kind: "app-bundle".to_string(),
    })
}

#[cfg(target_os = "macos")]
fn swap_app_with_privs(
    new_app: &Path,
    destination: &Path,
    original: std::io::Error,
) -> Result<(), UpdateError> {
    if original.kind() != std::io::ErrorKind::PermissionDenied {
        return Err(UpdateError::Install(format!(
            "failed to back up existing app bundle: {original}"
        )));
    }
    let dst = destination
        .to_str()
        .ok_or_else(|| UpdateError::Install("non-utf8 destination path".to_string()))?;
    let src = new_app
        .to_str()
        .ok_or_else(|| UpdateError::Install("non-utf8 staging path".to_string()))?;
    let command = app_swap_shell_command(src, dst);
    let script = format!(
        "do shell script \"{}\" with administrator privileges",
        command.replace('\\', "\\\\").replace('"', "\\\""),
    );
    let status = std::process::Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .status()
        .map_err(|err| UpdateError::Install(format!("failed to launch osascript: {err}")))?;
    if !status.success() {
        return Err(UpdateError::Install(format!(
            "elevated install failed (exit status {status})"
        )));
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn app_swap_shell_command(src: &str, dst: &str) -> String {
    // Paths pass through both AppleScript and the shell. Quote for the shell
    // first; the caller separately escapes the resulting AppleScript string.
    let quote = |path: &str| format!("'{}'", path.replace('\'', "'\\''"));
    let src = quote(src);
    let dst = quote(dst);
    format!("/bin/rm -rf -- {dst} && /bin/mv -f -- {src} {dst}")
}

#[cfg(target_os = "macos")]
fn find_app_dir(root: &Path) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                if path.extension().and_then(|s| s.to_str()) == Some("app") {
                    return Some(path);
                }
                stack.push(path);
            }
        }
    }
    None
}

/// Linux. Replace an AppImage at `destination` with the new bytes. Detects
/// whether `bytes` is gzipped and transparently decompresses if so. Existing
/// permissions are preserved (so the executable bit survives).
#[cfg(target_os = "linux")]
pub fn install_appimage(destination: &Path, bytes: &[u8]) -> Result<(), UpdateError> {
    use flate2::read::GzDecoder;
    use std::io::{Cursor, Read};
    use std::os::unix::fs::PermissionsExt;

    let parent = destination
        .parent()
        .ok_or_else(|| UpdateError::Install("AppImage destination has no parent".to_string()))?;
    std::fs::create_dir_all(parent)?;

    let mut payload = Vec::new();
    if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut gz = GzDecoder::new(Cursor::new(bytes));
        gz.read_to_end(&mut payload)?;
    } else {
        payload = bytes.to_vec();
    }

    let mode = std::fs::metadata(destination)
        .map(|m| m.permissions().mode())
        .unwrap_or(0o755);

    let staged = stage_file(parent, &payload)?;
    let mut perms = staged.as_file().metadata()?.permissions();
    perms.set_mode(mode | 0o111);
    staged.as_file().set_permissions(perms)?;
    staged.persist(destination).map_err(|err| err.error)?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn install_appimage(_destination: &Path, _bytes: &[u8]) -> Result<(), UpdateError> {
    Err(UpdateError::UnsupportedKind {
        kind: "appimage".to_string(),
    })
}

/// Backwards-compatible alias for [`install_binary`] retained for callers
/// that don't go through the kind dispatcher.
pub fn install_file(
    path: impl AsRef<Path>,
    bytes: &[u8],
    executable: bool,
) -> Result<(), UpdateError> {
    install_binary(path.as_ref(), bytes, executable)
}

fn stage_file(parent: &Path, bytes: &[u8]) -> Result<tempfile::NamedTempFile, UpdateError> {
    use std::io::Write;

    // Exclusive creation prevents a pre-existing file or symlink from being
    // followed. Keep the file open for writes and permission changes.
    let mut staged = tempfile::Builder::new()
        .prefix(".hashtree-update-")
        .tempfile_in(parent)?;
    staged.write_all(bytes)?;
    Ok(staged)
}

#[cfg(unix)]
fn set_executable(file: &std::fs::File) -> Result<(), UpdateError> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = file.metadata()?.permissions();
    permissions.set_mode(0o755);
    file.set_permissions(permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_file: &std::fs::File) -> Result<(), UpdateError> {
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::app_swap_shell_command;

    #[test]
    fn elevated_app_swap_treats_paths_as_data() {
        for source_is_hostile in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let hostile_name = "App'; touch injected; #.app";
            let (src_name, dst_name) = if source_is_hostile {
                (hostile_name, "Installed.app")
            } else {
                ("Staged.app", hostile_name)
            };
            let src = temp.path().join(src_name);
            let dst = temp.path().join(dst_name);
            std::fs::create_dir(&src).unwrap();
            std::fs::write(src.join("payload"), b"new app").unwrap();
            std::fs::create_dir(&dst).unwrap();
            std::fs::write(dst.join("payload"), b"old app").unwrap();

            // Exercise the exact shell command used by the privileged fallback,
            // without requesting administrator access in a test.
            let status = std::process::Command::new("/bin/sh")
                .args([
                    "-c",
                    &app_swap_shell_command(src.to_str().unwrap(), dst.to_str().unwrap()),
                ])
                .current_dir(temp.path())
                .status()
                .unwrap();

            assert!(
                !temp.path().join("injected").exists(),
                "path executed shell code"
            );
            assert!(status.success());
            assert_eq!(std::fs::read(dst.join("payload")).unwrap(), b"new app");
        }
    }
}
