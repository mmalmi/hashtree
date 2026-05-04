use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Plugin configuration deserialized from `tauri.conf.json` under
/// `plugins.hashtree-updater`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Config {
    /// `htree://npub.../<tree>/<channel>/latest` reference to the release
    /// directory. Required.
    pub reference: Option<String>,
    /// Manifest filename within the release directory. Default: `release.json`
    /// (matches what `htree release publish` writes).
    pub manifest_path: Option<String>,
    /// Default install destination. Apps that override `kind` per-platform
    /// should leave this empty and pass a destination at install time.
    pub destination: Option<PathBuf>,
    /// Nostr relays for resolving the mutable release root. If empty, falls
    /// back to the resolver's default relay set.
    pub relays: Vec<String>,
    /// Blossom servers to download asset chunks from. If empty, the
    /// resolver/keys' configured defaults are used.
    pub blossom_servers: Vec<String>,
}
