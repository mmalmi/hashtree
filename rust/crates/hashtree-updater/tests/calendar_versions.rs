use hashtree_updater::{UpdateAsset, UpdateManifest};

fn manifest(version: &str) -> UpdateManifest {
    UpdateManifest {
        version: version.to_string(),
        assets: vec![UpdateAsset {
            name: "app.tar.gz".to_string(),
            path: "assets/app.tar.gz".to_string(),
            ..Default::default()
        }],
        ..Default::default()
    }
}

#[test]
fn calendar_revision_versions_validate_and_compare_numerically() {
    let ordered = [
        "2026.9.10.1",
        "2026.9.22",
        "2026.9.22.1",
        "2026.9.22.2",
        "2026.9.22.10",
        "2026.9.23",
        "2026.10.1",
        "2027.1.1",
    ];
    for (index, candidate) in ordered.iter().enumerate() {
        let mut release = manifest(candidate);
        release.min_version = Some("2026.9.10.1".to_string());
        release.validate().expect("calendar release metadata");
        for (current_index, current) in ordered.iter().enumerate() {
            assert_eq!(
                release.is_newer_than(current).expect("calendar comparison"),
                index > current_index,
                "{candidate} compared with {current}"
            );
        }
    }
    let release = UpdateManifest {
        tag: Some("v2026.9.22.2".to_string()),
        ..manifest("")
    };
    release.validate().expect("tag-only calendar release");
    assert!(release.is_newer_than("v2026.9.22.1").unwrap());
}

#[test]
fn semantic_versions_keep_prerelease_ordering_and_invalid_versions_fail() {
    assert!(manifest("1.2.3").is_newer_than("1.2.3-rc.1").unwrap());
    assert!(!manifest("1.2.3-rc.1").is_newer_than("1.2.3").unwrap());
    for invalid in [
        "1.2.3.4",
        "2026.9.22.0",
        "2026.9.22.01",
        "2026.9.22.x",
        "2026.9.22.1.2",
    ] {
        assert!(manifest(invalid).validate().is_err(), "{invalid}");
    }
}
