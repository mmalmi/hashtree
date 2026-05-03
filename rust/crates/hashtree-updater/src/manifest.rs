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
    pub published_at: Option<String>,
    pub min_version: Option<String>,
    pub assets: Vec<UpdateAsset>,
}

impl UpdateManifest {
    pub fn validate(&self) -> Result<(), UpdateError> {
        if self.app.trim().is_empty() {
            return Err(UpdateError::InvalidManifest("app is required".to_string()));
        }
        if self.version.trim().is_empty() {
            return Err(UpdateError::InvalidManifest(
                "version is required".to_string(),
            ));
        }
        parse_version(&self.version)?;
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
            .find(|asset| asset.matches_target(target))
            .or_else(|| {
                (self.assets.len() == 1 && self.assets[0].target_values().is_empty())
                    .then_some(&self.assets[0])
            })
    }

    pub fn is_newer_than(&self, current_version: &str) -> Result<bool, UpdateError> {
        Ok(parse_version(&self.version)? > parse_version(current_version)?)
    }
}

/// Asset payload kind. Drives install strategy.
///
/// `Binary` is a plain file written into place, optionally executable.
/// `AppBundle` is a `tar.gz` containing a `*.app` (macOS only install).
/// `AppImage` is a Linux AppImage (optionally gzipped).
/// `Deb` / `Rpm` invoke the system package manager (requires elevation).
/// `Nsis` / `Msi` launch the installer and exit (Windows only).
/// `Archive` is a tarball or zip the caller is expected to handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetKind {
    Binary,
    AppBundle,
    AppImage,
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

    pub fn asset_kind(&self) -> AssetKind {
        self.kind
            .as_deref()
            .and_then(AssetKind::parse)
            .unwrap_or(AssetKind::Binary)
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
