use std::process::Command;

#[test]
fn publisher_capabilities_need_no_identity_store_or_network() {
    let empty = tempfile::tempdir().unwrap();
    let command = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_git-remote-htree"));
        command
            .current_dir(empty.path())
            .env("HOME", empty.path().join("home"))
            .env("HTREE_CONFIG_DIR", empty.path().join("config"))
            .env("HTREE_DATA_DIR", empty.path().join("data"))
            .env("HTREE_NSEC", "invalid-key-must-not-be-read")
            .env("PATH", "");
        command
    };
    let output = command().arg("--publisher-capabilities").output().unwrap();
    assert!(
        output.status.success(),
        "capability discovery must succeed without configured tools or identity: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        value,
        serde_json::json!({
            "schema": 1,
            "helper": "git-remote-htree",
            "version": env!("CARGO_PKG_VERSION"),
            "capabilities": ["complete-blob-dag-v1"]
        })
    );
    assert_eq!(std::fs::read_dir(empty.path()).unwrap().count(), 0);

    let extra = command()
        .args(["--publisher-capabilities", "unexpected"])
        .output()
        .unwrap();
    assert!(!extra.status.success());
    assert!(extra.stdout.is_empty());
    assert_eq!(std::fs::read_dir(empty.path()).unwrap().count(), 0);
}
