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
mod product;
mod progress;
mod reference;
mod target;
mod update_policy;
mod updater;

pub use error::UpdateError;
pub use install::{
    install, install_app_bundle, install_appimage, install_binary, install_binary_archive,
    install_file, InstallTarget,
};
pub use manifest::{
    infer_kind_from_name, infer_target_from_name, AssetKind, PublishedAt, UpdateAsset,
    UpdateManifest,
};
pub use product::{
    archive_extension_for_target, current_archive_target, dedupe_nonempty, display_manifest_tag,
    download_product_selection, env_csv, platform_app_asset_suffixes,
    preferred_app_asset_for_suffixes, preferred_cli_asset_for_target, preferred_product_asset,
    product_result_from_selection, safe_download_file_name, select_product_update,
    selected_download_path, split_csv, update_ref_from_override, write_downloaded_asset,
    ProductAssetPolicy, ProductUpdateMode, ProductUpdateResult, ProductUpdateSelection,
    SECURE_SOURCE_NAME,
};
#[cfg(feature = "secure-nostr-blossom")]
pub use product::{
    build_secure_nostr_blossom_updater, SecureNostrBlossomConfig, SecureNostrBlossomSelection,
    SecureNostrBlossomUpdater,
};
pub use progress::{DownloadCallback, DownloadEvent};
pub use reference::UpdateRef;
pub use target::UpdateTarget;
pub use update_policy::UpdateAutoCheckPolicy;
pub use updater::{
    DownloadOptions, DownloadedAsset, HashtreeUpdater, UpdateCheck, UpdateCheckOptions,
};
