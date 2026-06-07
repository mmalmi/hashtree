use semver::Version;
use serde::{Deserialize, Serialize};

use crate::error::UpdateError;
use crate::target::UpdateTarget;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateManifest {
    pub schema: Option<String>,
    pub app: String,
    pub version: String,
    pub channel: Option<String>,
    pub notes: Option<String>,
    /// Either a unix-seconds integer (as the existing `release.json` writes
    /// today) or an ISO 8601 string. Use `published_at_string()` if you need
    /// a printable form.
    pub published_at: Option<PublishedAt>,
    pub min_version: Option<String>,
    pub assets: Vec<UpdateAsset>,
    /// Tag like `v0.3.12` from the existing git.iris.to-style `release.json`.
    /// When `version` is empty we derive it from this by stripping any `v`.
    pub tag: Option<String>,
    /// Some release scripts split the title (`title`) and the tag; we accept
    /// both so they can populate either field.
    pub title: Option<String>,
}

/// Accepts either an integer (unix seconds, as `release.json` already uses)
/// or a string (eg ISO 8601). Stays as-is for the consumer to format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PublishedAt {
    UnixSeconds(i64),
    Iso(String),
}

impl PublishedAt {
    pub fn as_string(&self) -> String {
        match self {
            PublishedAt::UnixSeconds(secs) => secs.to_string(),
            PublishedAt::Iso(s) => s.clone(),
        }
    }
}

impl UpdateManifest {
    pub fn published_at_string(&self) -> Option<String> {
        self.published_at.as_ref().map(PublishedAt::as_string)
    }

    /// Resolve the version string. Prefers explicit `version`, falls back to
    /// `tag` (with any leading `v` stripped) so existing git.iris.to-style
    /// `release.json` files work without modification.
    pub fn effective_version(&self) -> String {
        if !self.version.trim().is_empty() {
            return self.version.trim().to_string();
        }
        if let Some(tag) = self.tag.as_deref() {
            let trimmed = tag.trim();
            if !trimmed.is_empty() {
                return trimmed.strip_prefix('v').unwrap_or(trimmed).to_string();
            }
        }
        String::new()
    }

    pub fn validate(&self) -> Result<(), UpdateError> {
        let version = self.effective_version();
        if version.is_empty() {
            return Err(UpdateError::InvalidManifest(
                "version (or tag) is required".to_string(),
            ));
        }
        parse_version(&version)?;
        if let Some(min_version) = self.min_version.as_deref() {
            parse_version(min_version)?;
        }
        if self.assets.is_empty() {
            return Err(UpdateError::InvalidManifest(
                "at least one asset is required".to_string(),
            ));
        }
        for asset in &self.assets {
            asset.validate()?;
        }
        Ok(())
    }

    pub fn select_asset(&self, target: &UpdateTarget) -> Option<&UpdateAsset> {
        self.assets
            .iter()
            .find(|asset| asset.matches_target_with_inference(target))
            .or_else(|| {
                (self.assets.len() == 1 && self.assets[0].target_values().is_empty())
                    .then_some(&self.assets[0])
            })
    }

    pub fn is_newer_than(&self, current_version: &str) -> Result<bool, UpdateError> {
        Ok(parse_version(&self.effective_version())? > parse_version(current_version)?)
    }
}

/// Asset payload kind. Drives install strategy.
///
/// `Binary` is a plain file written into place, optionally executable.
/// `AppBundle` is a `tar.gz` containing a `*.app` (macOS only install).
/// `AppImage` is a Linux AppImage (optionally gzipped).
/// `BinaryArchive` is a `.tar.gz` containing one binary plus auxiliary
/// files; the manifest's `executable` field names the entry to extract
/// (eg `iris/iris`). Works cross-platform.
/// `Deb` / `Rpm` invoke the system package manager (requires elevation).
/// `Nsis` / `Msi` launch the installer and exit (Windows only).
/// `Archive` is an opaque tarball or zip the caller is expected to handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetKind {
    Binary,
    AppBundle,
    AppImage,
    BinaryArchive,
    Deb,
    Rpm,
    Nsis,
    Msi,
    Archive,
}

impl AssetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AssetKind::Binary => "binary",
            AssetKind::AppBundle => "app-bundle",
            AssetKind::AppImage => "appimage",
            AssetKind::BinaryArchive => "binary-archive",
            AssetKind::Deb => "deb",
            AssetKind::Rpm => "rpm",
            AssetKind::Nsis => "nsis",
            AssetKind::Msi => "msi",
            AssetKind::Archive => "archive",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "binary" | "raw" | "raw-binary" | "exe" => AssetKind::Binary,
            "app" | "app-bundle" | "macos-app" => AssetKind::AppBundle,
            "appimage" | "app-image" => AssetKind::AppImage,
            "binary-archive" | "tarball-binary" => AssetKind::BinaryArchive,
            "deb" => AssetKind::Deb,
            "rpm" => AssetKind::Rpm,
            "nsis" => AssetKind::Nsis,
            "msi" => AssetKind::Msi,
            "archive" | "tar" | "tar.gz" | "tgz" | "zip" => AssetKind::Archive,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct UpdateAsset {
    pub name: String,
    pub path: String,
    pub target: Option<String>,
    pub targets: Vec<String>,
    /// Asset kind hint. Recognised values: `binary`, `app-bundle`, `appimage`,
    /// `deb`, `rpm`, `nsis`, `msi`, `archive`. If unset, defaults to `binary`.
    pub kind: Option<String>,
    /// Optional display name for the executable entry inside an archive (eg
    /// the `.app` directory inside a tar.gz, or the AppImage filename inside
    /// a gzipped wrapper). Currently advisory.
    pub executable: Option<String>,
}

impl UpdateAsset {
    pub fn validate(&self) -> Result<(), UpdateError> {
        if self.name.trim().is_empty() {
            return Err(UpdateError::InvalidManifest(
                "asset name is required".to_string(),
            ));
        }
        if !is_safe_relative_path(&self.path) {
            return Err(UpdateError::InvalidManifest(format!(
                "asset path is not a safe relative path: {}",
                self.path
            )));
        }
        if let Some(kind) = self.kind.as_deref() {
            if AssetKind::parse(kind).is_none() {
                return Err(UpdateError::InvalidManifest(format!(
                    "unknown asset kind: {kind}"
                )));
            }
        }
        Ok(())
    }

    pub fn target_values(&self) -> Vec<&str> {
        self.target
            .as_deref()
            .into_iter()
            .chain(self.targets.iter().map(String::as_str))
            .filter(|value| !value.trim().is_empty())
            .collect()
    }

    pub fn matches_target(&self, target: &UpdateTarget) -> bool {
        self.target_values()
            .into_iter()
            .any(|candidate| target.matches(candidate))
    }

    /// Like `matches_target` but, when the asset has no explicit target
    /// metadata, falls back to inferring one from the filename. Lets us pick
    /// the right artifact out of a git.iris.to-style `release.json` whose
    /// assets only carry a `name` and `path`.
    pub fn matches_target_with_inference(&self, target: &UpdateTarget) -> bool {
        if self.matches_target(target) {
            return true;
        }
        if !self.target_values().is_empty() {
            return false;
        }
        infer_target_from_name(&self.name)
            .map(|inferred| target.matches(&inferred))
            .unwrap_or(false)
    }

    pub fn asset_kind(&self) -> AssetKind {
        if let Some(kind) = self.kind.as_deref().and_then(AssetKind::parse) {
            return kind;
        }
        let inferred = infer_kind_from_name(&self.name).unwrap_or(AssetKind::Binary);
        // A bare archive with an `executable` hint upgrades to BinaryArchive
        // so the install dispatcher knows which entry to extract.
        let has_executable_hint = self
            .executable
            .as_deref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        if matches!(inferred, AssetKind::Archive) && has_executable_hint {
            return AssetKind::BinaryArchive;
        }
        inferred
    }
}

/// Infer a target triple from common release artifact filenames.
/// Examples: `myapp-v1-linux-arm64.AppImage` → `linux-aarch64`,
/// `myapp-macos-x64.dmg` → `darwin-x86_64`,
/// `myapp-windows-x64.exe` → `windows-x86_64`.
pub fn infer_target_from_name(name: &str) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    let pairs: &[(&str, &str)] = &[
        ("linux-arm64", "linux-aarch64"),
        ("linux-aarch64", "linux-aarch64"),
        ("linux-x64", "linux-x86_64"),
        ("linux-x86_64", "linux-x86_64"),
        ("linux-amd64", "linux-x86_64"),
        ("macos-arm64", "darwin-aarch64"),
        ("macos-aarch64", "darwin-aarch64"),
        ("darwin-aarch64", "darwin-aarch64"),
        ("macos-x64", "darwin-x86_64"),
        ("macos-x86_64", "darwin-x86_64"),
        ("darwin-x86_64", "darwin-x86_64"),
        ("windows-arm64", "windows-aarch64"),
        ("windows-aarch64", "windows-aarch64"),
        ("windows-x64", "windows-x86_64"),
        ("windows-x86_64", "windows-x86_64"),
        ("aarch64-apple-darwin", "aarch64-apple-darwin"),
        ("x86_64-apple-darwin", "x86_64-apple-darwin"),
        ("aarch64-unknown-linux-musl", "aarch64-unknown-linux-musl"),
        ("x86_64-unknown-linux-musl", "x86_64-unknown-linux-musl"),
        (
            "arm-unknown-linux-musleabihf",
            "arm-unknown-linux-musleabihf",
        ),
        ("aarch64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"),
        ("x86_64-unknown-linux-gnu", "x86_64-unknown-linux-gnu"),
        ("x86_64-pc-windows-msvc", "x86_64-pc-windows-msvc"),
        ("aarch64-pc-windows-msvc", "aarch64-pc-windows-msvc"),
        ("android-arm64", "android-aarch64"),
        ("android-aarch64", "android-aarch64"),
        ("aarch64-linux-android", "aarch64-linux-android"),
    ];
    for (needle, target) in pairs {
        if lower.contains(needle) {
            return Some((*target).to_string());
        }
    }
    None
}

/// Infer the install kind from a release artifact filename.
pub fn infer_kind_from_name(name: &str) -> Option<AssetKind> {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".appimage") || lower.ends_with(".appimage.gz") {
        Some(AssetKind::AppImage)
    } else if lower.ends_with(".app.tar.gz") || lower.ends_with(".app.tgz") {
        Some(AssetKind::AppBundle)
    } else if lower.ends_with(".deb") {
        Some(AssetKind::Deb)
    } else if lower.ends_with(".rpm") {
        Some(AssetKind::Rpm)
    } else if lower.ends_with(".msi") {
        Some(AssetKind::Msi)
    } else if lower.ends_with(".exe") {
        Some(AssetKind::Nsis)
    } else if lower.ends_with(".dmg") {
        // We don't auto-install DMGs yet; surface as an unsupported archive
        // so callers can fall back to opening the file manually.
        Some(AssetKind::Archive)
    } else if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") || lower.ends_with(".zip") {
        Some(AssetKind::Archive)
    } else {
        None
    }
}

fn is_safe_relative_path(path: &str) -> bool {
    let trimmed = path.trim();
    !trimmed.is_empty()
        && !trimmed.starts_with('/')
        && trimmed
            .split('/')
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

pub(crate) fn parse_version(version: &str) -> Result<Version, semver::Error> {
    Version::parse(version.trim().strip_prefix('v').unwrap_or(version.trim()))
}
