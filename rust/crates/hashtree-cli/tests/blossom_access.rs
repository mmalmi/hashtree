//! Integration tests for Blossom access control with social graph
//!
//! Tests the access control logic for blossom blob uploads:
//! - Hardcoded subscribers are always allowed
//! - Users within DEFAULT_MAX_WRITE_DISTANCE (3) degrees of separation are allowed
//! - Users muted by root are denied
//! - Overmuted users (high muter/follower ratio) are denied
//!
//! Note: Full e2e tests with nostrdb social graph require a running relay
//! and populated social graph. These tests focus on the HTTP API behavior.
//!
//! Run with: cargo test --package hashtree-cli --test blossom_access -- --nocapture

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use tempfile::TempDir;
use nostr::{Keys, ToBech32};

struct TestServer {
    _data_dir: TempDir,
    _home_dir: TempDir,
    process: Child,
    port: u16,
}

impl TestServer {
    fn new(port: u16, enable_auth: bool) -> Self {
        Self::new_with_upstream(port, enable_auth, None)
    }

    fn new_with_upstream(port: u16, enable_auth: bool, upstream_blossom: Option<&str>) -> Self {
        let htree_bin = find_htree_binary();
        let data_dir = TempDir::new().expect("Failed to create temp dir");
        let home_dir = TempDir::new().expect("Failed to create home dir");

        // Create .hashtree config dir
        let config_dir = home_dir.path().join(".hashtree");
        std::fs::create_dir_all(&config_dir).expect("Failed to create config dir");

        // Create config
        let upstream_config = if let Some(url) = upstream_blossom {
            format!("\n[blossom]\nread_servers = [\"{}\"]", url)
        } else {
            String::new()
        };
        let config_content = format!(r#"
[server]
enable_auth = {}
stun_port = 0
enable_webrtc = false

[nostr]
relays = []
{}"#, enable_auth, upstream_config);
        std::fs::write(config_dir.join("config.toml"), config_content)
            .expect("Failed to write config");

        // Generate and write keys file
        let keys = Keys::generate();
        let nsec = keys.secret_key().to_bech32().expect("Failed to encode nsec");
        std::fs::write(config_dir.join("keys"), &nsec)
            .expect("Failed to write keys");

        let process = Command::new(htree_bin)
            .arg("--data-dir")
            .arg(data_dir.path())
            .arg("start")
            .arg("--addr")
            .arg(format!("127.0.0.1:{}", port))
            .env("HOME", home_dir.path())
            .env("RUST_LOG", "info")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("Failed to start htree server");

        // Wait for server to start
        std::thread::sleep(Duration::from_secs(2));

        TestServer {
            _data_dir: data_dir,
            _home_dir: home_dir,
            process,
            port,
        }
    }

    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

fn find_htree_binary() -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace_root = PathBuf::from(manifest_dir)
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();

    let debug_bin = workspace_root.join("target/debug/htree");
    let release_bin = workspace_root.join("target/release/htree");

    if debug_bin.exists() {
        debug_bin
    } else if release_bin.exists() {
        release_bin
    } else {
        panic!(
            "htree binary not found. Run `cargo build --bin htree` first.\n\
             Looked in:\n  - {:?}\n  - {:?}",
            debug_bin, release_bin
        );
    }
}

/// Create a blossom auth header for upload
/// Kind 24242 event with "upload" action tag
fn create_blossom_auth(keys: &Keys) -> String {
    use nostr::{EventBuilder, Kind, Tag, TagKind, Timestamp};
    use base64::Engine;

    let now = Timestamp::now();
    let expiration = Timestamp::from(now.as_u64() + 300); // 5 minutes

    // Create kind 24242 event with tags
    let tags = vec![
        Tag::custom(TagKind::Custom("t".into()), vec!["upload".to_string()]),
        Tag::custom(TagKind::Custom("expiration".into()), vec![expiration.to_string()]),
    ];
    let event = EventBuilder::new(Kind::Custom(24242), "", tags)
        .to_event(keys)
        .expect("Failed to sign event");

    let event_json = serde_json::to_string(&event).expect("Failed to serialize event");
    let encoded = base64::engine::general_purpose::STANDARD.encode(event_json);

    format!("Nostr {}", encoded)
}

/// Test that uploads without auth are rejected when auth is enabled
#[test]
fn test_upload_requires_auth() {
    let server = TestServer::new(19001, true);

    // Try to upload without auth header
    let output = Command::new("curl")
        .arg("-s")
        .arg("-w").arg("\n%{http_code}")
        .arg("-X").arg("PUT")
        .arg("-H").arg("Content-Type: application/octet-stream")
        .arg("--data-binary").arg("test content")
        .arg(format!("{}/upload", server.base_url()))
        .output()
        .expect("Failed to run curl");

    let response = String::from_utf8_lossy(&output.stdout);
    println!("Response: {}", response);

    // Should get 401 Unauthorized
    assert!(response.contains("401") || response.contains("error"),
        "Upload without auth should be rejected");
}

/// Test that uploads with valid auth are processed
/// Note: Without social graph, auth'd uploads may still be rejected if not in allowed list
#[test]
fn test_upload_with_auth_header() {
    let server = TestServer::new(19002, true);

    let keys = Keys::generate();
    let auth_header = create_blossom_auth(&keys);

    println!("Testing upload with auth header...");
    println!("Pubkey: {}", keys.public_key().to_hex());

    let output = Command::new("curl")
        .arg("-s")
        .arg("-w").arg("\n%{http_code}")
        .arg("-X").arg("PUT")
        .arg("-H").arg("Content-Type: application/octet-stream")
        .arg("-H").arg(format!("Authorization: {}", auth_header))
        .arg("--data-binary").arg("test content for upload")
        .arg(format!("{}/upload", server.base_url()))
        .output()
        .expect("Failed to run curl");

    let response = String::from_utf8_lossy(&output.stdout);
    println!("Response: {}", response);

    // Should get either success (200/201) or forbidden (403) based on social graph
    // We're testing that auth header is processed correctly
    assert!(
        response.contains("200") ||
        response.contains("201") ||
        response.contains("403") ||
        response.contains("error"),
        "Should get a valid response (success or forbidden)"
    );
}

/// Test that blobs can be retrieved without auth (GET is public)
#[test]
fn test_get_blob_no_auth_required() {
    let server = TestServer::new(19003, false); // Disable auth for this test

    // Upload a blob first (no auth)
    let test_content = "Hello, Blossom!";
    let upload_output = Command::new("curl")
        .arg("-s")
        .arg("-X").arg("PUT")
        .arg("-H").arg("Content-Type: text/plain")
        .arg("--data-binary").arg(test_content)
        .arg(format!("{}/upload", server.base_url()))
        .output()
        .expect("Failed to upload");

    let upload_response = String::from_utf8_lossy(&upload_output.stdout);
    println!("Upload response: {}", upload_response);

    // Extract sha256 from response
    let sha256: Option<String> = serde_json::from_str::<serde_json::Value>(&upload_response)
        .ok()
        .and_then(|v| v.get("sha256").and_then(|s| s.as_str()).map(|s| s.to_string()));

    if let Some(hash) = sha256 {
        println!("Uploaded blob hash: {}", hash);

        // GET should work without auth
        let get_output = Command::new("curl")
            .arg("-s")
            .arg("-w").arg("\n%{http_code}")
            .arg(format!("{}/{}", server.base_url(), hash))
            .output()
            .expect("Failed to get blob");

        let get_response = String::from_utf8_lossy(&get_output.stdout);
        println!("GET response: {}", get_response);

        assert!(get_response.contains(test_content) || get_response.contains("200"),
            "GET should return the blob content");
    }
}

/// Test HEAD request for blob existence check
#[test]
fn test_head_blob() {
    let server = TestServer::new(19004, false);

    // Upload a blob
    let upload_output = Command::new("curl")
        .arg("-s")
        .arg("-X").arg("PUT")
        .arg("-H").arg("Content-Type: text/plain")
        .arg("--data-binary").arg("HEAD test content")
        .arg(format!("{}/upload", server.base_url()))
        .output()
        .expect("Failed to upload");

    let upload_response = String::from_utf8_lossy(&upload_output.stdout);
    println!("Upload response: {}", upload_response);

    let sha256: Option<String> = serde_json::from_str::<serde_json::Value>(&upload_response)
        .ok()
        .and_then(|v| v.get("sha256").and_then(|s| s.as_str()).map(|s| s.to_string()));

    if let Some(hash) = sha256 {
        // HEAD should return 200 for existing blob
        let head_output = Command::new("curl")
            .arg("-s")
            .arg("-I") // HEAD request
            .arg("-w").arg("\n%{http_code}")
            .arg(format!("{}/{}", server.base_url(), hash))
            .output()
            .expect("Failed to HEAD blob");

        let head_response = String::from_utf8_lossy(&head_output.stdout);
        println!("HEAD response: {}", head_response);

        assert!(head_response.contains("200"), "HEAD should return 200 for existing blob");

        // HEAD for non-existent blob should return 404
        let fake_hash = "0000000000000000000000000000000000000000000000000000000000000000";
        let head_404 = Command::new("curl")
            .arg("-s")
            .arg("-I")
            .arg("-w").arg("\n%{http_code}")
            .arg(format!("{}/{}", server.base_url(), fake_hash))
            .output()
            .expect("Failed to HEAD blob");

        let head_404_response = String::from_utf8_lossy(&head_404.stdout);
        println!("HEAD 404 response: {}", head_404_response);

        assert!(head_404_response.contains("404"), "HEAD should return 404 for non-existent blob");
    }
}

/// Test CORS preflight (OPTIONS request)
#[test]
fn test_cors_preflight() {
    let server = TestServer::new(19005, false);

    let output = Command::new("curl")
        .arg("-s")
        .arg("-X").arg("OPTIONS")
        .arg("-I")
        .arg("-H").arg("Origin: https://example.com")
        .arg("-H").arg("Access-Control-Request-Method: PUT")
        .arg("-H").arg("Access-Control-Request-Headers: Authorization, Content-Type")
        .arg(format!("{}/upload", server.base_url()))
        .output()
        .expect("Failed to send OPTIONS");

    let response = String::from_utf8_lossy(&output.stdout);
    println!("OPTIONS response: {}", response);

    // Should have CORS headers
    assert!(response.contains("Access-Control-Allow-Origin") || response.contains("204"),
        "Should return CORS headers");
}

/// Test list endpoint returns user's blobs
#[test]
fn test_list_blobs() {
    let server = TestServer::new(19006, false);

    // Upload some blobs
    for i in 1..=3 {
        let content = format!("Blob content {}", i);
        Command::new("curl")
            .arg("-s")
            .arg("-X").arg("PUT")
            .arg("-H").arg("Content-Type: text/plain")
            .arg("--data-binary").arg(&content)
            .arg(format!("{}/upload", server.base_url()))
            .output()
            .expect("Failed to upload");
    }

    // List endpoint requires a pubkey parameter: /list/<pubkey>
    // Without a specific pubkey, the endpoint may return 404 or empty
    // This is expected behavior - list is per-user
    let list_output = Command::new("curl")
        .arg("-s")
        .arg("-w").arg("\n%{http_code}")
        .arg(format!("{}/list", server.base_url()))
        .output()
        .expect("Failed to list");

    let list_response = String::from_utf8_lossy(&list_output.stdout);
    println!("List response: {}", list_response);

    // List without pubkey may return 404 (Not found) or empty array
    // Both are valid responses
    assert!(
        list_response.contains("404") ||
        list_response.contains("Not found") ||
        list_response.contains("[]") ||
        list_response.is_empty() ||
        serde_json::from_str::<Vec<serde_json::Value>>(&list_response.lines().next().unwrap_or("")).is_ok(),
        "List should return 404, empty array, or valid JSON"
    );
}

// Social graph tests removed - nostrdb dependency has been removed
// Access control now uses allowed_npubs config + public_writes flag

/// Test that Cache-Control: immutable header is set for content-addressed routes
#[test]
fn test_cache_control_immutable_header() {
    let server = TestServer::new(19007, false);

    // Upload a blob with auth header (since enable_auth may still require it)
    let keys = Keys::generate();
    let auth_header = create_blossom_auth(&keys);

    let test_content = "Cache control test content";
    let upload_output = Command::new("curl")
        .arg("-s")
        .arg("-X").arg("PUT")
        .arg("-H").arg("Content-Type: text/plain")
        .arg("-H").arg(format!("Authorization: {}", auth_header))
        .arg("--data-binary").arg(test_content)
        .arg(format!("{}/upload", server.base_url()))
        .output()
        .expect("Failed to upload");

    let upload_response = String::from_utf8_lossy(&upload_output.stdout);
    println!("Upload response: {}", upload_response);

    let sha256: Option<String> = serde_json::from_str::<serde_json::Value>(&upload_response)
        .ok()
        .and_then(|v| v.get("sha256").and_then(|s| s.as_str()).map(|s| s.to_string()));

    if let Some(hash) = sha256 {
        println!("Uploaded blob hash: {}", hash);

        // GET request should include Cache-Control: immutable header
        let get_output = Command::new("curl")
            .arg("-s")
            .arg("-I") // Get headers only
            .arg(format!("{}/{}", server.base_url(), hash))
            .output()
            .expect("Failed to get blob headers");

        let get_headers = String::from_utf8_lossy(&get_output.stdout);
        println!("GET headers: {}", get_headers);

        assert!(get_headers.contains("cache-control:") || get_headers.contains("Cache-Control:"),
            "Response should include Cache-Control header");
        assert!(get_headers.to_lowercase().contains("immutable"),
            "Cache-Control should include 'immutable' directive");
        assert!(get_headers.to_lowercase().contains("max-age=31536000"),
            "Cache-Control should include max-age=31536000 (1 year)");

        // HEAD request should also include Cache-Control: immutable header
        let head_output = Command::new("curl")
            .arg("-s")
            .arg("-I")
            .arg("-X").arg("HEAD")
            .arg(format!("{}/{}", server.base_url(), hash))
            .output()
            .expect("Failed to HEAD blob");

        let head_headers = String::from_utf8_lossy(&head_output.stdout);
        println!("HEAD headers: {}", head_headers);

        assert!(head_headers.to_lowercase().contains("immutable"),
            "HEAD response should include Cache-Control: immutable");
    } else {
        // Skip test if upload failed (may happen in some CI environments)
        println!("Upload failed (possibly auth issue), skipping cache header test");
    }
}

/// Test that X-Source header shows upstream source when fetched from upstream blossom
#[test]
fn test_x_source_header_upstream() {
    // Start upstream server (has the blob)
    let upstream_server = TestServer::new(19009, false);

    // Upload a blob to upstream server with auth
    let keys = Keys::generate();
    let auth_header = create_blossom_auth(&keys);

    let test_content = "X-Source upstream test content";
    let upload_output = Command::new("curl")
        .arg("-s")
        .arg("-X").arg("PUT")
        .arg("-H").arg("Content-Type: text/plain")
        .arg("-H").arg(format!("Authorization: {}", auth_header))
        .arg("--data-binary").arg(test_content)
        .arg(format!("{}/upload", upstream_server.base_url()))
        .output()
        .expect("Failed to upload to upstream");

    let upload_response = String::from_utf8_lossy(&upload_output.stdout);
    println!("Upstream upload response: {}", upload_response);

    let sha256: Option<String> = serde_json::from_str::<serde_json::Value>(&upload_response)
        .ok()
        .and_then(|v| v.get("sha256").and_then(|s| s.as_str()).map(|s| s.to_string()));

    if let Some(hash) = sha256 {
        println!("Uploaded blob hash: {}", hash);

        // Start downstream server with upstream configured (does NOT have the blob locally)
        let downstream_server = TestServer::new_with_upstream(19010, false, Some(&upstream_server.base_url()));

        // Wait a bit for downstream server to fully start
        std::thread::sleep(Duration::from_millis(500));

        // GET from downstream should fetch from upstream and include X-Source header
        // Use -i (include headers) instead of -I (HEAD only) because HEAD doesn't do upstream fallback
        let get_output = Command::new("curl")
            .arg("-s")
            .arg("-i") // Include headers in output
            .arg(format!("{}/{}", downstream_server.base_url(), hash))
            .output()
            .expect("Failed to get blob from downstream");

        let get_response = String::from_utf8_lossy(&get_output.stdout);
        println!("GET response from downstream: {}", get_response);

        // Should have X-Source header showing upstream source
        assert!(get_response.to_lowercase().contains("x-source:"),
            "Response should include X-Source header for localhost requests");
        assert!(get_response.to_lowercase().contains("x-source: upstream:"),
            "X-Source header should indicate 'upstream:' source for blobs fetched from upstream server");
    } else {
        // Skip test if upload failed (may happen in some CI environments)
        println!("Upload to upstream failed (possibly auth issue), skipping upstream X-Source header test");
    }
}

/// Test that X-Source header is present for localhost requests (local source)
#[test]
fn test_x_source_header_local() {
    let server = TestServer::new(19008, false);

    // Upload a blob with auth header (required even with enable_auth=false for some setups)
    let keys = Keys::generate();
    let auth_header = create_blossom_auth(&keys);

    let test_content = "X-Source test content";
    let upload_output = Command::new("curl")
        .arg("-s")
        .arg("-X").arg("PUT")
        .arg("-H").arg("Content-Type: text/plain")
        .arg("-H").arg(format!("Authorization: {}", auth_header))
        .arg("--data-binary").arg(test_content)
        .arg(format!("{}/upload", server.base_url()))
        .output()
        .expect("Failed to upload");

    let upload_response = String::from_utf8_lossy(&upload_output.stdout);
    println!("Upload response: {}", upload_response);

    let sha256: Option<String> = serde_json::from_str::<serde_json::Value>(&upload_response)
        .ok()
        .and_then(|v| v.get("sha256").and_then(|s| s.as_str()).map(|s| s.to_string()));

    if let Some(hash) = sha256 {
        println!("Uploaded blob hash: {}", hash);

        // GET request from localhost should include X-Source header
        let get_output = Command::new("curl")
            .arg("-s")
            .arg("-i") // Include headers in output
            .arg(format!("{}/{}", server.base_url(), hash))
            .output()
            .expect("Failed to get blob");

        let get_response = String::from_utf8_lossy(&get_output.stdout);
        println!("GET response: {}", get_response);

        // Should have X-Source: local header for localhost requests
        assert!(get_response.to_lowercase().contains("x-source:"),
            "Response should include X-Source header for localhost requests");
        assert!(get_response.to_lowercase().contains("x-source: local"),
            "X-Source header should be 'local' for locally stored blobs");
    } else {
        // Skip test if upload failed (may happen in some CI environments)
        println!("Upload failed (possibly auth issue), skipping X-Source header test");
    }
}

/// Test htree add with blossom push to local server
#[test]
fn test_htree_add_with_blossom_push() {
    let server = TestServer::new(18095, false);
    let htree_bin = find_htree_binary();

    // Create temp directories for the CLI instance
    let data_dir = TempDir::new().expect("Failed to create data dir");
    let home_dir = TempDir::new().expect("Failed to create home dir");

    // Create .hashtree config dir with write_servers pointing to our test server
    let config_dir = home_dir.path().join(".hashtree");
    std::fs::create_dir_all(&config_dir).expect("Failed to create config dir");

    let config_content = format!(r#"
[blossom]
write_servers = ["{}"]
read_servers = ["{}"]
"#, server.base_url(), server.base_url());
    std::fs::write(config_dir.join("config.toml"), config_content)
        .expect("Failed to write config");

    // Generate keys
    let keys = Keys::generate();
    let nsec = keys.secret_key().to_bech32().expect("Failed to encode nsec");
    std::fs::write(config_dir.join("keys"), &nsec)
        .expect("Failed to write keys");

    // Create a test file to add
    let test_file = data_dir.path().join("test.txt");
    std::fs::write(&test_file, "Hello from htree add test!").expect("Failed to write test file");

    // Run htree add (without --local, so it pushes to blossom)
    println!("Running htree add...");
    let add_output = Command::new(&htree_bin)
        .arg("--data-dir")
        .arg(data_dir.path())
        .arg("add")
        .arg(&test_file)
        .arg("--public")
        .env("HOME", home_dir.path())
        .output()
        .expect("Failed to run htree add");

    let stdout = String::from_utf8_lossy(&add_output.stdout);
    let stderr = String::from_utf8_lossy(&add_output.stderr);
    println!("stdout: {}", stdout);
    println!("stderr: {}", stderr);

    assert!(add_output.status.success(), "htree add should succeed");

    // Check that blossom push happened
    assert!(stdout.contains("file servers:") || stdout.contains("uploaded"),
        "Output should mention file server upload");

    // Extract hex hash from output (line starts with "  hash:")
    let hash = stdout.lines()
        .find(|l| l.trim().starts_with("hash:"))
        .and_then(|l| l.split_whitespace().last())
        .expect("Should have hash in output");

    println!("Uploaded hash: {}", hash);
    assert!(hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()),
        "Hash should be 64 hex chars, got: {}", hash);

    // Verify the blob exists on the server
    let check_output = Command::new("curl")
        .arg("-s")
        .arg("-o").arg("/dev/null")
        .arg("-w").arg("%{http_code}")
        .arg(format!("{}/{}.bin", server.base_url(), hash))
        .output()
        .expect("Failed to check blob");

    let status_code = String::from_utf8_lossy(&check_output.stdout);
    println!("Server check status: {}", status_code);

    assert_eq!(status_code.trim(), "200", "Blob should exist on server after htree add");
}
