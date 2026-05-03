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
        kind @ (AssetKind::Deb
        | AssetKind::Rpm
        | AssetKind::Nsis
        | AssetKind::Msi
        | AssetKind::Archive) => Err(UpdateError::UnsupportedKind {
            kind: kind.as_str().to_string(),
        }),
    }
}

/// Write `bytes` to `path` via a temp file + atomic rename.
pub fn install_binary(path: &Path, bytes: &[u8], executable: bool) -> Result<(), UpdateError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let temp_path = temp_install_path(path);
    std::fs::write(&temp_path, bytes)?;
    if executable {
        set_executable(&temp_path)?;
    }
    std::fs::rename(&temp_path, path)?;
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

    let staging = parent.join(format!(
        ".{}.{}.staging",
        destination
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("app"),
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;

    let cursor = Cursor::new(bytes);
    let gz = GzDecoder::new(cursor);
    let mut archive = tar::Archive::new(gz);
    archive.unpack(&staging).map_err(|err| {
        let _ = std::fs::remove_dir_all(&staging);
        UpdateError::Install(format!("failed to unpack app bundle: {err}"))
    })?;

    let new_app = find_app_dir(&staging).ok_or_else(|| {
        let _ = std::fs::remove_dir_all(&staging);
        UpdateError::Install("no .app directory found in archive".to_string())
    })?;

    let backup = parent.join(format!(
        ".{}.{}.backup",
        destination
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("app"),
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&backup);

    let backed_up = destination.exists();
    if backed_up {
        if let Err(err) = std::fs::rename(destination, &backup) {
            // Permission denied: ask the user via AppleScript with admin privs.
            return swap_app_with_privs(&new_app, destination, &backup, &staging, err);
        }
    }

    if let Err(err) = std::fs::rename(&new_app, destination) {
        if backed_up {
            let _ = std::fs::rename(&backup, destination);
        }
        let _ = std::fs::remove_dir_all(&staging);
        return Err(UpdateError::Install(format!(
            "failed to install new app bundle: {err}"
        )));
    }

    let _ = std::fs::remove_dir_all(&backup);
    let _ = std::fs::remove_dir_all(&staging);
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
    _backup: &Path,
    staging: &Path,
    original: std::io::Error,
) -> Result<(), UpdateError> {
    if original.kind() != std::io::ErrorKind::PermissionDenied {
        let _ = std::fs::remove_dir_all(staging);
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
    let script = format!(
        "do shell script \"rm -rf '{dst}' && mv -f '{src}' '{dst}'\" with administrator privileges",
        dst = dst.replace('\\', "\\\\").replace('"', "\\\""),
        src = src.replace('\\', "\\\\").replace('"', "\\\""),
    );
    let status = std::process::Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .status()
        .map_err(|err| UpdateError::Install(format!("failed to launch osascript: {err}")))?;
    let _ = std::fs::remove_dir_all(staging);
    if !status.success() {
        return Err(UpdateError::Install(format!(
            "elevated install failed (exit status {status})"
        )));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn find_app_dir(root: &Path) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
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

    let temp_path = temp_install_path(destination);
    std::fs::write(&temp_path, &payload)?;
    let mut perms = std::fs::metadata(&temp_path)?.permissions();
    perms.set_mode(mode | 0o111);
    std::fs::set_permissions(&temp_path, perms)?;
    std::fs::rename(&temp_path, destination)?;
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

fn temp_install_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("update");
    let temp_name = format!(".{file_name}.{}.tmp", std::process::id());
    path.with_file_name(temp_name)
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), UpdateError> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<(), UpdateError> {
    Ok(())
}
