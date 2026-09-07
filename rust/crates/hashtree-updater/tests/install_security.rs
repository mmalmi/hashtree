#![cfg(unix)]

use std::os::unix::fs::symlink;

use hashtree_updater::install_binary;

#[test]
fn downloaded_asset_does_not_follow_a_precreated_destination_symlink() {
    let dir = tempfile::tempdir().unwrap();
    let destination = dir.path().join("release.tar.gz");
    let unrelated = dir.path().join("unrelated");
    std::fs::write(&unrelated, b"keep this file").unwrap();
    symlink(&unrelated, &destination).unwrap();

    hashtree_updater::write_downloaded_asset(&destination, b"new archive").unwrap();

    assert_eq!(std::fs::read(&unrelated).unwrap(), b"keep this file");
    assert_eq!(std::fs::read(&destination).unwrap(), b"new archive");
    assert!(!std::fs::symlink_metadata(&destination)
        .unwrap()
        .file_type()
        .is_symlink());
}

#[test]
fn install_binary_does_not_follow_a_precreated_temporary_symlink() {
    let dir = tempfile::tempdir().unwrap();
    let destination = dir.path().join("app");
    let unrelated = dir.path().join("unrelated");
    std::fs::write(&unrelated, b"keep this file").unwrap();
    let predictable_temp = dir.path().join(format!(".app.{}.tmp", std::process::id()));
    symlink(&unrelated, &predictable_temp).unwrap();

    install_binary(&destination, b"new binary", true).unwrap();

    assert_eq!(std::fs::read(&unrelated).unwrap(), b"keep this file");
    assert_eq!(std::fs::read(&destination).unwrap(), b"new binary");
    assert!(!std::fs::symlink_metadata(&destination)
        .unwrap()
        .file_type()
        .is_symlink());
}

#[cfg(target_os = "linux")]
#[test]
fn install_appimage_does_not_follow_a_precreated_temporary_symlink() {
    let dir = tempfile::tempdir().unwrap();
    let destination = dir.path().join("app.AppImage");
    let unrelated = dir.path().join("unrelated");
    std::fs::write(&unrelated, b"keep this file").unwrap();
    let predictable_temp = dir
        .path()
        .join(format!(".app.AppImage.{}.tmp", std::process::id()));
    symlink(&unrelated, &predictable_temp).unwrap();

    hashtree_updater::install_appimage(&destination, b"new binary").unwrap();

    assert_eq!(std::fs::read(&unrelated).unwrap(), b"keep this file");
    assert_eq!(std::fs::read(&destination).unwrap(), b"new binary");
    assert!(!std::fs::symlink_metadata(&destination)
        .unwrap()
        .file_type()
        .is_symlink());
}

#[cfg(target_os = "macos")]
#[test]
fn install_app_bundle_preserves_unrelated_staging_and_backup_paths() {
    use flate2::write::GzEncoder;
    use flate2::Compression;

    let dir = tempfile::tempdir().unwrap();
    let destination = dir.path().join("App.app");
    std::fs::create_dir(&destination).unwrap();
    std::fs::write(destination.join("payload"), b"old app").unwrap();
    let mut unrelated_paths = Vec::new();
    for suffix in ["staging", "backup"] {
        let unrelated = dir
            .path()
            .join(format!(".App.app.{}.{suffix}", std::process::id()));
        std::fs::create_dir(&unrelated).unwrap();
        std::fs::write(unrelated.join("keep"), b"keep this file").unwrap();
        unrelated_paths.push(unrelated);
    }
    let mut archive = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
    let mut header = tar::Header::new_gnu();
    header.set_size(7);
    header.set_mode(0o755);
    archive
        .append_data(&mut header, "App.app/payload", &b"new app"[..])
        .unwrap();
    let bytes = archive.into_inner().unwrap().finish().unwrap();

    hashtree_updater::install_app_bundle(&destination, &bytes).unwrap();

    assert_eq!(
        std::fs::read(destination.join("payload")).unwrap(),
        b"new app"
    );
    for unrelated in unrelated_paths {
        assert_eq!(
            std::fs::read(unrelated.join("keep")).unwrap(),
            b"keep this file"
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn install_app_bundle_rejects_a_symlink_as_the_app() {
    use flate2::write::GzEncoder;
    use flate2::Compression;

    let dir = tempfile::tempdir().unwrap();
    let destination = dir.path().join("App.app");
    let unrelated = dir.path().join("unrelated");
    std::fs::create_dir(&unrelated).unwrap();
    std::fs::write(unrelated.join("payload"), b"unverified app").unwrap();
    let mut archive = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
    let mut header = tar::Header::new_gnu();
    header.set_size(0);
    header.set_mode(0o755);
    header.set_entry_type(tar::EntryType::Symlink);
    archive
        .append_link(&mut header, "App.app", &unrelated)
        .unwrap();
    let bytes = archive.into_inner().unwrap().finish().unwrap();

    assert!(hashtree_updater::install_app_bundle(&destination, &bytes).is_err());

    assert!(!destination.exists());
    assert_eq!(
        std::fs::read(unrelated.join("payload")).unwrap(),
        b"unverified app"
    );
}
