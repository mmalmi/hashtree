use hashtree_client::ClientConfig;
use hashtree_config::Config;

// A dedicated test executable keeps environment changes isolated from the
// parallel transport/download tests. This executable contains only this test.
#[test]
fn configured_origins_are_readable_and_empty_lists_use_shared_defaults() {
    let temp = tempfile::tempdir().unwrap();
    std::env::set_var("HTREE_CONFIG_DIR", temp.path());
    std::env::set_var("HTREE_LOCAL_DAEMON_ONLY", "true");
    std::env::set_var("HTREE_DAEMON_URL", "http://127.0.0.1:8080");
    std::fs::write(
        temp.path().join("config.toml"),
        r#"
[blossom]
servers = ["https://legacy.example", "https://origin.example"]
read_servers = ["https://cdn.example", "https://origin.example"]
write_servers = ["https://origin.example"]
"#,
    )
    .unwrap();
    let actual = ClientConfig::from_env().unwrap();
    assert_eq!(
        actual.read_servers,
        [
            "https://cdn.example",
            "https://legacy.example",
            "https://origin.example"
        ]
    );
    assert!(actual.local_only);
    assert_eq!(actual.daemon_url.as_deref(), Some("http://127.0.0.1:8080"));
    std::fs::write(
        temp.path().join("config.toml"),
        "[blossom]\nservers=[]\nread_servers=[]\nwrite_servers=[]\n",
    )
    .unwrap();
    let actual = ClientConfig::from_env().unwrap();
    assert_eq!(
        actual.read_servers,
        Config::default().blossom.all_read_servers()
    );
    assert!(!actual.read_servers.is_empty());
    assert!(actual.local_only);
}
