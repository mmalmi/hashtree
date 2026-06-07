use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hashtree_core::{Cid, DirEntry, HashTree, HashTreeConfig, LinkType, MemoryStore};
use hashtree_resolver::nostr::{NostrResolverConfig, NostrRootResolver};
use hashtree_resolver::{Keys, RootResolver, ToBech32};
use hashtree_sim::WsRelay;
use hashtree_updater::{
    archive_extension_for_target, current_archive_target, install, install_file,
    platform_app_asset_suffixes, preferred_product_asset, safe_download_file_name, AssetKind,
    DownloadEvent, DownloadOptions, HashtreeUpdater, InstallTarget, ProductAssetPolicy,
    ProductUpdateMode, UpdateAsset, UpdateAutoCheckPolicy, UpdateCheckOptions, UpdateManifest,
    UpdateRef, UpdateTarget,
};

async fn file_entry(tree: &HashTree<MemoryStore>, name: &str, bytes: &[u8]) -> DirEntry {
    let (cid, size) = tree.put_file(bytes).await.expect("put file");
    DirEntry::from_cid(name, &cid)
        .with_size(size)
        .with_link_type(LinkType::File)
}

async fn make_release_tree(
    tree: &HashTree<MemoryStore>,
    manifest: &str,
    asset_path: &str,
    asset_bytes: &[u8],
) -> Cid {
    let (manifest_cid, manifest_size) = tree
        .put_file(manifest.as_bytes())
        .await
        .expect("put manifest");
    let manifest_entry = DirEntry::from_cid("release.json", &manifest_cid)
        .with_size(manifest_size)
        .with_link_type(LinkType::File);

    let path = Path::new(asset_path);
    let asset_name = path.file_name().unwrap().to_string_lossy().to_string();
    let asset_entry = file_entry(tree, &asset_name, asset_bytes).await;
    let assets_dir = tree
        .put_directory(vec![asset_entry])
        .await
        .expect("put assets dir");
    let assets_entry = DirEntry::from_cid("assets", &assets_dir).with_link_type(LinkType::Dir);

    tree.put_directory(vec![manifest_entry, assets_entry])
        .await
        .expect("put release dir")
}

async fn make_releases_root(tree: &HashTree<MemoryStore>, release_cid: &Cid, version: &str) -> Cid {
    let version_entry = DirEntry::from_cid(version, release_cid).with_link_type(LinkType::Dir);
    let latest_entry = DirEntry::from_cid("latest", release_cid).with_link_type(LinkType::Dir);
    tree.put_directory(vec![version_entry, latest_entry])
        .await
        .expect("put releases root")
}

#[test]
fn parses_encoded_release_ref_with_tree_path_split() {
    let parsed = UpdateRef::parse("htree://npub1owner/releases%2Fnostr-vpn/stable/latest")
        .expect("parse ref");

    assert_eq!(parsed.npub, "npub1owner");
    assert_eq!(parsed.tree_name, "releases/nostr-vpn");
    assert_eq!(parsed.path.as_deref(), Some("stable/latest"));
    assert_eq!(parsed.resolver_key(), "npub1owner/releases/nostr-vpn");
}

#[test]
fn manifest_selects_current_target_alias_and_rejects_unsafe_paths() {
    let manifest: UpdateManifest = serde_json::from_str(
        r#"{
          "schema": "hashtree.update.v1",
          "app": "nostr-vpn",
          "version": "v1.2.3",
          "channel": "stable",
          "assets": [
            {
              "name": "nostr-vpn-macos-arm64.tar.gz",
              "path": "assets/nostr-vpn-macos-arm64.tar.gz",
              "targets": ["aarch64-apple-darwin"]
            }
          ]
        }"#,
    )
    .expect("manifest");

    manifest.validate().expect("valid manifest");
    let target = UpdateTarget::new("darwin-aarch64");
    assert_eq!(
        manifest.select_asset(&target).expect("selected asset").name,
        "nostr-vpn-macos-arm64.tar.gz"
    );

    let unsafe_manifest: UpdateManifest = serde_json::from_str(
        r#"{
          "app": "bad",
          "version": "1.0.0",
          "assets": [{ "name": "bad", "path": "../bad", "target": "x86_64-unknown-linux-gnu" }]
        }"#,
    )
    .expect("unsafe manifest");
    assert!(unsafe_manifest.validate().is_err());
}

#[tokio::test]
async fn check_resolves_release_manifest_and_downloads_selected_asset() {
    let store = Arc::new(MemoryStore::new());
    let tree = HashTree::new(HashTreeConfig::new(store).public());
    let manifest = r#"{
      "schema": "hashtree.update.v1",
      "app": "squirreldisk",
      "version": "1.2.0",
      "channel": "stable",
      "assets": [
        {
          "name": "squirreldisk-linux-x64",
          "path": "assets/squirreldisk-linux-x64",
          "target": "x86_64-unknown-linux-gnu"
        }
      ]
    }"#;
    let release_cid = make_release_tree(
        &tree,
        manifest,
        "assets/squirreldisk-linux-x64",
        b"new-binary\n",
    )
    .await;
    let releases_root = make_releases_root(&tree, &release_cid, "v1.2.0").await;
    let resolver = SingleRootResolver::new("npub1publisher/releases/squirreldisk", releases_root);
    let updater = HashtreeUpdater::new(resolver, tree);

    let check = updater
        .check(UpdateCheckOptions {
            reference: UpdateRef::parse("htree://npub1publisher/releases%2Fsquirreldisk/latest")
                .expect("ref"),
            current_version: "1.1.0".to_string(),
            target: UpdateTarget::new("linux-x86_64"),
            ..UpdateCheckOptions::default()
        })
        .await
        .expect("check");

    assert!(check.update_available);
    assert_eq!(check.manifest.app, "squirreldisk");
    assert_eq!(
        check.asset.as_ref().expect("asset").name,
        "squirreldisk-linux-x64"
    );

    let downloaded = updater
        .download_asset(&check, None)
        .await
        .expect("download");
    assert_eq!(downloaded.bytes, b"new-binary\n");
}

#[test]
fn install_file_replaces_destination_atomically_and_marks_executable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let destination = dir.path().join("app-bin");
    std::fs::write(&destination, b"old").expect("write old");

    install_file(&destination, b"new", true).expect("install");

    assert_eq!(std::fs::read(&destination).expect("read installed"), b"new");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&destination)
            .expect("metadata")
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0, "installed file should be executable");
    }
}

#[tokio::test]
async fn e2e_resolves_latest_update_through_nostr_root_event() {
    let mut relay = WsRelay::new();
    relay.start().await.expect("start relay");
    let relay_url = relay.url().expect("relay url");

    let store = Arc::new(MemoryStore::new());
    let tree = HashTree::new(HashTreeConfig::new(store).public());
    let manifest = r#"{
      "schema": "hashtree.update.v1",
      "app": "nostr-vpn",
      "version": "2.0.0",
      "assets": [
        {
          "name": "nostr-vpn-aarch64-apple-darwin.tar.gz",
          "path": "assets/nostr-vpn-aarch64-apple-darwin.tar.gz",
          "targets": ["aarch64-apple-darwin"]
        }
      ]
    }"#;
    let release_cid = make_release_tree(
        &tree,
        manifest,
        "assets/nostr-vpn-aarch64-apple-darwin.tar.gz",
        b"archive bytes",
    )
    .await;
    let stable_root = make_releases_root(&tree, &release_cid, "v2.0.0").await;
    let stable_entry = DirEntry::from_cid("stable", &stable_root).with_link_type(LinkType::Dir);
    let releases_root = tree
        .put_directory(vec![stable_entry])
        .await
        .expect("put channel root");

    let keys = Keys::generate();
    let npub = keys.public_key().to_bech32().expect("npub");
    let publish_resolver = NostrRootResolver::new(NostrResolverConfig {
        relays: vec![relay_url.clone()],
        resolve_timeout: Duration::from_secs(2),
        secret_key: Some(keys),
    })
    .await
    .expect("publisher resolver");
    publish_resolver
        .publish(&format!("{npub}/releases/nostr-vpn"), &releases_root)
        .await
        .expect("publish");

    let read_resolver = NostrRootResolver::new(NostrResolverConfig {
        relays: vec![relay_url],
        resolve_timeout: Duration::from_secs(2),
        secret_key: None,
    })
    .await
    .expect("read resolver");
    let updater = HashtreeUpdater::new(read_resolver, tree);

    let check = updater
        .check(UpdateCheckOptions {
            reference: UpdateRef::parse(&format!(
                "htree://{npub}/releases%2Fnostr-vpn/stable/latest"
            ))
            .expect("ref"),
            current_version: "1.9.0".to_string(),
            target: UpdateTarget::new("darwin-aarch64"),
            ..UpdateCheckOptions::default()
        })
        .await
        .expect("check");

    assert!(check.update_available);
    assert_eq!(check.manifest.app, "nostr-vpn");
    assert_eq!(
        updater
            .download_asset(&check, None)
            .await
            .expect("download")
            .bytes,
        b"archive bytes"
    );

    relay.stop().await;
}

#[tokio::test]
async fn check_parses_existing_release_json_schema_with_filename_inference() {
    let store = Arc::new(MemoryStore::new());
    let tree = HashTree::new(HashTreeConfig::new(store).public());
    // Match the schema squirreldisk's release.mjs already writes today —
    // tag/commit/notes_file/assets without per-asset target or kind.
    let release_json = r#"{
      "id": "v0.3.12",
      "title": "v0.3.12",
      "tag": "v0.3.12",
      "commit": "deadbeef",
      "created_at": 1777820414,
      "published_at": 1777820414,
      "draft": false,
      "prerelease": false,
      "notes_file": "notes.md",
      "assets": [
        {
          "name": "squirreldisk-v0.3.12-linux-arm64.AppImage",
          "path": "assets/squirreldisk-v0.3.12-linux-arm64.AppImage",
          "size": 112144768
        },
        {
          "name": "squirreldisk-v0.3.12-linux-arm64.deb",
          "path": "assets/squirreldisk-v0.3.12-linux-arm64.deb",
          "size": 12003144
        },
        {
          "name": "squirreldisk-v0.3.12-macos-arm64.dmg",
          "path": "assets/squirreldisk-v0.3.12-macos-arm64.dmg",
          "size": 13738512
        },
        {
          "name": "squirreldisk-v0.3.12-windows-x64.exe",
          "path": "assets/squirreldisk-v0.3.12-windows-x64.exe",
          "size": 8801067
        }
      ]
    }"#;
    let release_cid = make_release_tree(
        &tree,
        release_json,
        "assets/squirreldisk-v0.3.12-linux-arm64.AppImage",
        b"appimage bytes",
    )
    .await;
    let releases_root = make_releases_root(&tree, &release_cid, "v0.3.12").await;
    let resolver = SingleRootResolver::new("npub1publisher/releases/squirreldisk", releases_root);
    let updater = HashtreeUpdater::new(resolver, tree);

    let check = updater
        .check(UpdateCheckOptions {
            reference: UpdateRef::parse("htree://npub1publisher/releases%2Fsquirreldisk/latest")
                .expect("ref"),
            current_version: "0.3.11".to_string(),
            target: UpdateTarget::new("linux-aarch64"),
            ..UpdateCheckOptions::default()
        })
        .await
        .expect("check");

    assert!(check.update_available);
    assert_eq!(check.manifest.effective_version(), "0.3.12");
    let asset = check.asset.as_ref().expect("asset");
    assert!(
        asset.name.contains("linux-arm64"),
        "expected linux asset, got {}",
        asset.name,
    );
    assert!(
        asset.name.ends_with(".AppImage"),
        "expected appimage to win over deb, got {}",
        asset.name,
    );
    assert_eq!(asset.asset_kind(), AssetKind::AppImage);
}

#[test]
fn filename_inference_covers_common_release_artifacts() {
    use hashtree_updater::{infer_kind_from_name, infer_target_from_name};

    let cases = [
        (
            "squirreldisk-v0.3.12-linux-arm64.AppImage",
            "linux-aarch64",
            AssetKind::AppImage,
        ),
        (
            "squirreldisk-v0.3.12-linux-x64.deb",
            "linux-x86_64",
            AssetKind::Deb,
        ),
        (
            "nostr-vpn-v1.0-macos-arm64.app.tar.gz",
            "darwin-aarch64",
            AssetKind::AppBundle,
        ),
        (
            "nostr-vpn-v1.0-windows-x64.msi",
            "windows-x86_64",
            AssetKind::Msi,
        ),
        (
            "nostr-vpn-v1.0-windows-x64.exe",
            "windows-x86_64",
            AssetKind::Nsis,
        ),
    ];
    for (name, expected_target, expected_kind) in cases {
        assert_eq!(
            infer_target_from_name(name).as_deref(),
            Some(expected_target),
            "target inference for {name}",
        );
        assert_eq!(
            infer_kind_from_name(name),
            Some(expected_kind),
            "kind inference for {name}",
        );
    }
}

#[test]
fn product_cli_asset_selection_ignores_app_packages_for_same_target() {
    let target = current_archive_target();
    let archive_ext = archive_extension_for_target(target);
    let policy = ProductAssetPolicy::new("democtl", "Demo CLI", "Demo App")
        .with_app_asset_suffixes(["-linux-x64.appimage", "-linux-x64.deb"]);
    let manifest = UpdateManifest {
        tag: Some("v1.2.3".to_string()),
        assets: vec![
            UpdateAsset {
                name: "demo-v1.2.3-linux-x64.deb".to_string(),
                path: "assets/demo.deb".to_string(),
                ..UpdateAsset::default()
            },
            UpdateAsset {
                name: format!("democtl-v1.2.3-{target}{archive_ext}"),
                path: "assets/democtl.tar.gz".to_string(),
                ..UpdateAsset::default()
            },
        ],
        ..UpdateManifest::default()
    };

    let selected =
        preferred_product_asset(&manifest, ProductUpdateMode::Cli, &policy).expect("CLI asset");

    assert!(selected.name.starts_with("democtl-v1.2.3-"));
}

#[test]
fn product_app_asset_selection_uses_platform_suffixes() {
    let suffixes = platform_app_asset_suffixes();
    if suffixes.is_empty() {
        return;
    }
    let wanted = format!("demo-v1.2.3{}", suffixes[0]);
    let policy = ProductAssetPolicy::new("democtl", "Demo CLI", "Demo App")
        .with_app_asset_suffixes(suffixes.iter().copied());
    let manifest = UpdateManifest {
        tag: Some("v1.2.3".to_string()),
        assets: vec![
            UpdateAsset {
                name: format!(
                    "democtl-v1.2.3-{}{}",
                    current_archive_target(),
                    archive_extension_for_target(current_archive_target())
                ),
                path: "assets/democtl.tar.gz".to_string(),
                ..UpdateAsset::default()
            },
            UpdateAsset {
                name: wanted.clone(),
                path: format!("assets/{wanted}"),
                ..UpdateAsset::default()
            },
        ],
        ..UpdateManifest::default()
    };

    let selected =
        preferred_product_asset(&manifest, ProductUpdateMode::App, &policy).expect("app asset");

    assert_eq!(selected.name, wanted);
}

#[test]
fn product_download_file_name_is_sanitized() {
    assert_eq!(
        safe_download_file_name("../bad name.tar.gz", "fallback"),
        ".._bad_name.tar.gz"
    );
    assert_eq!(safe_download_file_name("", "fallback"), "fallback");
}

#[test]
fn update_auto_check_policy_handles_startup_interval_and_manual_reset() {
    let start = std::time::Instant::now();
    let mut policy = UpdateAutoCheckPolicy::new(std::time::Duration::from_secs(60));

    assert!(policy.should_start_check(true, start));
    assert!(!policy.should_start_check(true, start + std::time::Duration::from_secs(59)));
    policy.note_manual_check_started(start + std::time::Duration::from_secs(50));
    assert!(!policy.should_start_check(true, start + std::time::Duration::from_secs(100)));
    assert!(policy.should_start_check(true, start + std::time::Duration::from_secs(110)));
    assert!(!policy.should_start_check(false, start + std::time::Duration::from_secs(200)));
}

#[tokio::test]
async fn download_emits_started_progress_finished_with_total_size() {
    let store = Arc::new(MemoryStore::new());
    // Small chunk size forces the asset into a multi-chunk tree so the
    // progress callback fires several times.
    let tree = HashTree::new(HashTreeConfig::new(store).public().with_chunk_size(64));
    let asset_bytes = vec![7u8; 4096];
    let manifest = format!(
        r#"{{
          "schema": "hashtree.update.v1",
          "app": "iris-chat",
          "version": "0.5.0",
          "assets": [
            {{
              "name": "iris-chat",
              "path": "assets/iris-chat",
              "target": "{}",
              "kind": "binary"
            }}
          ]
        }}"#,
        UpdateTarget::current().as_str(),
    );
    let release_cid = make_release_tree(&tree, &manifest, "assets/iris-chat", &asset_bytes).await;
    let releases_root = make_releases_root(&tree, &release_cid, "v0.5.0").await;
    let resolver = SingleRootResolver::new("npub1publisher/releases/iris-chat", releases_root);
    let updater = HashtreeUpdater::new(resolver, tree);

    let check = updater
        .check(UpdateCheckOptions {
            reference: UpdateRef::parse("htree://npub1publisher/releases%2Firis-chat/latest")
                .expect("ref"),
            current_version: "0.4.0".to_string(),
            target: UpdateTarget::current(),
            ..UpdateCheckOptions::default()
        })
        .await
        .expect("check");

    let events: Arc<Mutex<Vec<DownloadEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let cb: hashtree_updater::DownloadCallback = Arc::new(move |event| {
        sink.lock().unwrap().push(event);
    });

    let downloaded = updater
        .download(
            &check,
            DownloadOptions {
                progress_chunk: Some(512),
                ..Default::default()
            },
            Some(cb),
        )
        .await
        .expect("download");
    assert_eq!(downloaded.bytes, asset_bytes);

    let events = events.lock().unwrap();
    assert!(matches!(
        events.first(),
        Some(DownloadEvent::Started {
            content_length: Some(4096)
        })
    ));
    assert!(matches!(
        events.last(),
        Some(DownloadEvent::Finished { total: 4096 })
    ));
    let progress_total: u64 = events
        .iter()
        .filter_map(|event| match event {
            DownloadEvent::Progress { chunk_len, .. } => Some(*chunk_len),
            _ => None,
        })
        .sum();
    assert_eq!(progress_total, 4096);
    let progress_count = events
        .iter()
        .filter(|event| matches!(event, DownloadEvent::Progress { .. }))
        .count();
    assert!(
        progress_count >= 4,
        "expected several progress events, got {progress_count}",
    );
}

#[test]
fn install_dispatcher_handles_binary_kind_with_atomic_swap() {
    let dir = tempfile::tempdir().expect("tempdir");
    let dest = dir.path().join("iris-chat");
    std::fs::write(&dest, b"old").unwrap();

    let asset = UpdateAsset {
        name: "iris-chat".into(),
        path: "assets/iris-chat".into(),
        kind: Some("binary".into()),
        ..Default::default()
    };
    let target = InstallTarget::new(&dest).executable(true);

    install(&asset, b"new-binary", &target).expect("install");

    assert_eq!(std::fs::read(&dest).unwrap(), b"new-binary");
    assert_eq!(asset.asset_kind(), AssetKind::Binary);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&dest).unwrap().permissions().mode();
        assert_ne!(mode & 0o111, 0);
    }
}

#[test]
fn install_dispatcher_rejects_unsupported_kinds() {
    let dir = tempfile::tempdir().expect("tempdir");
    let dest = dir.path().join("payload");

    for kind in ["deb", "rpm", "nsis", "msi", "archive"] {
        let asset = UpdateAsset {
            name: "x".into(),
            path: "assets/x".into(),
            kind: Some(kind.into()),
            ..Default::default()
        };
        let result = install(&asset, b"data", &InstallTarget::new(&dest));
        match result {
            Err(hashtree_updater::UpdateError::UnsupportedKind { kind: reported }) => {
                assert_eq!(reported, kind, "unsupported kind round-trip");
            }
            other => panic!("expected UnsupportedKind for {kind}, got {other:?}"),
        }
    }
}

#[test]
fn install_binary_archive_extracts_named_entry_to_destination() {
    use flate2::write::GzEncoder;
    use flate2::Compression;

    let dir = tempfile::tempdir().expect("tempdir");
    let dest = dir.path().join("iris");
    std::fs::write(&dest, b"old").unwrap();

    // tar.gz layout: iris/iris (binary), iris/install.sh (ignored)
    let payload = b"#!/bin/sh\necho hello from iris\n";
    let install_sh = b"#!/bin/sh\necho stale installer\n";
    let mut buffer = Vec::new();
    {
        let gz = GzEncoder::new(&mut buffer, Compression::fast());
        let mut tar = tar::Builder::new(gz);
        let mut h1 = tar::Header::new_gnu();
        h1.set_size(payload.len() as u64);
        h1.set_mode(0o755);
        h1.set_cksum();
        tar.append_data(&mut h1, "iris/iris", &payload[..]).unwrap();
        let mut h2 = tar::Header::new_gnu();
        h2.set_size(install_sh.len() as u64);
        h2.set_mode(0o755);
        h2.set_cksum();
        tar.append_data(&mut h2, "iris/install.sh", &install_sh[..])
            .unwrap();
        tar.into_inner().unwrap().finish().unwrap();
    }

    let asset = UpdateAsset {
        name: "iris-aarch64-apple-darwin.tar.gz".into(),
        path: "assets/iris-aarch64-apple-darwin.tar.gz".into(),
        // No explicit kind — relies on inference from .tar.gz + executable hint.
        executable: Some("iris/iris".into()),
        ..Default::default()
    };
    assert_eq!(asset.asset_kind(), AssetKind::BinaryArchive);

    install(&asset, &buffer, &InstallTarget::new(&dest)).expect("install");

    assert_eq!(std::fs::read(&dest).unwrap(), payload);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&dest).unwrap().permissions().mode();
        assert_ne!(mode & 0o111, 0, "extracted binary should be executable");
    }
}

#[test]
fn install_binary_archive_errors_when_entry_not_in_tar() {
    use flate2::write::GzEncoder;
    use flate2::Compression;

    let dir = tempfile::tempdir().expect("tempdir");
    let dest = dir.path().join("iris");
    let mut buffer = Vec::new();
    {
        let gz = GzEncoder::new(&mut buffer, Compression::fast());
        let mut tar = tar::Builder::new(gz);
        let mut h = tar::Header::new_gnu();
        let payload = b"unrelated";
        h.set_size(payload.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        tar.append_data(&mut h, "other/file", &payload[..]).unwrap();
        tar.into_inner().unwrap().finish().unwrap();
    }

    let asset = UpdateAsset {
        name: "iris.tar.gz".into(),
        path: "assets/iris.tar.gz".into(),
        executable: Some("iris/iris".into()),
        ..Default::default()
    };
    let err = install(&asset, &buffer, &InstallTarget::new(&dest)).unwrap_err();
    assert!(
        matches!(err, hashtree_updater::UpdateError::Install(ref m) if m.contains("not found")),
        "expected Install error, got {err:?}",
    );
}

#[cfg(target_os = "macos")]
#[test]
fn install_app_bundle_unpacks_tar_gz_and_swaps_dot_app() {
    use flate2::write::GzEncoder;
    use flate2::Compression;

    let dir = tempfile::tempdir().expect("tempdir");

    // Build a tar.gz containing MyApp.app/Contents/MacOS/MyApp
    let mut buffer = Vec::new();
    {
        let gz = GzEncoder::new(&mut buffer, Compression::fast());
        let mut tar = tar::Builder::new(gz);
        let mut header = tar::Header::new_gnu();
        let payload = b"#!/bin/sh\necho hi\n";
        header.set_size(payload.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        tar.append_data(&mut header, "MyApp.app/Contents/MacOS/MyApp", &payload[..])
            .unwrap();
        tar.into_inner().unwrap().finish().unwrap();
    }

    let dest = dir.path().join("MyApp.app");
    let asset = UpdateAsset {
        name: "MyApp.tar.gz".into(),
        path: "assets/MyApp.tar.gz".into(),
        kind: Some("app-bundle".into()),
        ..Default::default()
    };
    let target = InstallTarget::new(&dest);
    install(&asset, &buffer, &target).expect("install app bundle");

    let installed = dest.join("Contents/MacOS/MyApp");
    assert!(installed.exists(), "binary missing inside installed .app");
    assert_eq!(std::fs::read(&installed).unwrap(), b"#!/bin/sh\necho hi\n");
}

#[cfg(target_os = "linux")]
#[test]
fn install_appimage_decompresses_gzip_and_preserves_executable_bit() {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("tempdir");
    let dest = dir.path().join("MyApp.AppImage");
    std::fs::write(&dest, b"old").unwrap();
    let mut perms = std::fs::metadata(&dest).unwrap().permissions();
    perms.set_mode(0o644);
    std::fs::set_permissions(&dest, perms).unwrap();

    let payload = b"new appimage bytes";
    let mut gzipped = Vec::new();
    {
        let mut enc = GzEncoder::new(&mut gzipped, Compression::fast());
        enc.write_all(payload).unwrap();
        enc.finish().unwrap();
    }

    let asset = UpdateAsset {
        name: "MyApp.AppImage.gz".into(),
        path: "assets/MyApp.AppImage.gz".into(),
        kind: Some("appimage".into()),
        ..Default::default()
    };
    install(&asset, &gzipped, &InstallTarget::new(&dest)).expect("install");

    assert_eq!(std::fs::read(&dest).unwrap(), payload);
    let mode = std::fs::metadata(&dest).unwrap().permissions().mode();
    assert_ne!(
        mode & 0o111,
        0,
        "appimage should be executable after install"
    );
}

struct SingleRootResolver {
    key: String,
    cid: Cid,
}

impl SingleRootResolver {
    fn new(key: impl Into<String>, cid: Cid) -> Self {
        Self {
            key: key.into(),
            cid,
        }
    }
}

#[async_trait::async_trait]
impl RootResolver for SingleRootResolver {
    async fn resolve(&self, key: &str) -> Result<Option<Cid>, hashtree_resolver::ResolverError> {
        Ok((key == self.key).then(|| self.cid.clone()))
    }

    async fn subscribe(
        &self,
        _key: &str,
    ) -> Result<tokio::sync::mpsc::Receiver<Option<Cid>>, hashtree_resolver::ResolverError> {
        Err(hashtree_resolver::ResolverError::Other(
            "test resolver does not subscribe".to_string(),
        ))
    }
}
