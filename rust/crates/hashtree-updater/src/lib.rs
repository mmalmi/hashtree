//! Hashtree-based update discovery and artifact installation helpers.
//!
//! This crate treats a signed hashtree mutable root as the update authority.
//! Apps bake an `npub` + release tree + channel/path, resolve it to an
//! immutable release directory, read `manifest.json`, select the asset for the
//! current platform, then download or install that asset locally.
//!
//! Asset bytes are authenticated end-to-end by hashtree itself: the resolved
//! root CID transitively pins every directory entry and chunk, so no extra
//! per-asset hash or signature is required.

mod error;
mod install;
mod manifest;
mod progress;
mod reference;
mod target;
mod updater;

pub use error::UpdateError;
pub use install::{install, install_appimage, install_app_bundle, install_binary, install_file, InstallTarget};
pub use manifest::{AssetKind, UpdateAsset, UpdateManifest};
pub use progress::{DownloadCallback, DownloadEvent};
pub use reference::UpdateRef;
pub use target::UpdateTarget;
pub use updater::{
    DownloadOptions, DownloadedAsset, HashtreeUpdater, UpdateCheck, UpdateCheckOptions,
};
