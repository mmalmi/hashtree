//! Cross-language peer test: hashtree-rs exposing content for hashtree-ts to sync
//!
//! This test starts an htree server with known test content and outputs markers
//! that the TypeScript E2E test can capture to discover and sync from it.
//!
//! Run with: cargo test --package hashtree-cli --test crosslang_peer -- --nocapture
//!
//! The test outputs:
//! - CROSSLANG_NPUB:<npub1...> - The server's Nostr pubkey in bech32
//! - CROSSLANG_PUBKEY:<hex> - The server's Nostr pubkey in hex
//! - CROSSLANG_HASH:<hex> - The SHA256 hash of test content
//! - CROSSLANG_READY - Indicates server is ready for connections
//! - CROSSLANG_CONNECTED:<pubkey> - When a peer connects

use nostr::{Keys, ToBech32};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use tempfile::TempDir;

struct CrosslangServer {
    _data_dir: TempDir,
    process: Child,
    #[allow(dead_code)]
    pubkey_hex: String,
}

impl CrosslangServer {
    fn new(port: u16, htree_bin: &str, keys: &Keys, test_content: &[u8], follow_pubkeys: &[String]) -> Self {
        let data_dir = TempDir::new().expect("Failed to create temp dir");
        let data_path = data_dir.path();
        let home_dir = data_dir.path();

        // Create .hashtree config dir
        let config_dir = home_dir.join(".hashtree");
        std::fs::create_dir_all(&config_dir).expect("Failed to create config dir");

        // Use local test relay (started by playwright) for reliable testing
        // Falls back to public relays if LOCAL_RELAY env var not set
        let relay_url = std::env::var("LOCAL_RELAY")
            .unwrap_or_else(|_| "wss://temp.iris.to".to_string());
        println!("CROSSLANG_RELAY:{}", relay_url);
        println!("CROSSLANG_CONFIG_DIR:{}", config_dir.display());
        let config_content = format!(r#"
[server]
enable_auth = false
stun_port = 0

[nostr]
relays = ["{}"]
crawl_depth = 0
"#, relay_url);
        let config_path = config_dir.join("config.toml");
        std::fs::write(&config_path, &config_content)
            .expect("Failed to write config");
        println!("CROSSLANG_CONFIG_WRITTEN:{}", config_path.display());
        println!("CROSSLANG_CONFIG_CONTENT:{}", config_content.replace('\n', " "));

        // Write pre-generated keys file
        let nsec = keys.secret_key().to_bech32().expect("Failed to encode nsec");
        std::fs::write(config_dir.join("keys"), &nsec)
            .expect("Failed to write keys");

        // Write contacts.json with pubkeys to follow (for WebRTC peer classification)
        if !follow_pubkeys.is_empty() {
            let contacts_json = serde_json::to_string(&follow_pubkeys)
                .expect("Failed to serialize contacts");
            std::fs::write(data_path.join("contacts.json"), &contacts_json)
                .expect("Failed to write contacts.json");
            println!("Following pubkeys: {:?}", follow_pubkeys);
        }

        let pubkey_hex = keys.public_key().to_hex();

        // Create test content file
        let content_file = data_path.join("test-content.txt");
        std::fs::write(&content_file, test_content).expect("Failed to write test content");

        // Start the htree server
        let process = Command::new(htree_bin)
            .arg("--data-dir")
            .arg(data_path)
            .arg("start")
            .arg("--addr")
            .arg(format!("127.0.0.1:{}", port))
            .arg("--relays")
            .arg(&relay_url)
            .env("HOME", home_dir)
            .env("RUST_LOG", std::env::var("RUST_LOG").unwrap_or_else(|_| "warn,hashtree_cli::webrtc=info".to_string()))
            .stdout(Stdio::inherit())  // Forward stdout to test output
            .stderr(Stdio::inherit())  // Forward stderr to test output
            .spawn()
            .expect("Failed to start htree server");

        CrosslangServer {
            _data_dir: data_dir,
            process,
            pubkey_hex,
        }
    }
}

impl Drop for CrosslangServer {
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

#[test]
#[ignore = "long-running network test (120s) - run manually with --ignored"]
fn test_crosslang_peer() {
    let htree_bin = find_htree_binary();
    let htree_bin_str = htree_bin.to_str().unwrap();

    // Use pre-generated key from env if available, otherwise generate
    let keys = if let Ok(secret_hex) = std::env::var("CROSSLANG_SECRET_KEY") {
        Keys::parse(&secret_hex).expect("Failed to parse CROSSLANG_SECRET_KEY")
    } else {
        Keys::generate()
    };
    let pubkey_hex = keys.public_key().to_hex();
    let npub = keys.public_key().to_bech32().expect("Failed to encode npub");

    // Test content that will be synced
    let test_content = b"Hello from hashtree-rs! This content was synced cross-language.";

    // Check if we should follow a specific pubkey (from TS test)
    let follow_pubkeys: Vec<String> = std::env::var("CROSSLANG_FOLLOW_PUBKEY")
        .ok()
        .map(|pk| vec![pk])
        .unwrap_or_default();

    println!("\n=== Cross-Language Peer Test ===");
    println!("CROSSLANG_NPUB:{}", npub);
    println!("CROSSLANG_PUBKEY:{}", pubkey_hex);
    if !follow_pubkeys.is_empty() {
        println!("CROSSLANG_FOLLOWING:{}", follow_pubkeys[0]);
    }

    let port: u16 = std::env::var("CROSSLANG_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(18090);
    println!("CROSSLANG_PORT:{}", port);

    // Start the server
    let server = CrosslangServer::new(port, htree_bin_str, &keys, test_content, &follow_pubkeys);
    println!("Server started with pubkey: {}", &server.pubkey_hex[..16]);

    // Wait for server to start
    std::thread::sleep(Duration::from_secs(3));

    // Upload test content via HTTP
    let upload_output = Command::new("curl")
        .arg("-s")
        .arg("-X").arg("POST")
        .arg("-F").arg(format!("file=@{}", server._data_dir.path().join("test-content.txt").display()))
        .arg(format!("http://127.0.0.1:{}/upload", port))
        .output()
        .expect("Failed to upload file");

    let upload_stdout = String::from_utf8_lossy(&upload_output.stdout);
    println!("Upload response: {}", upload_stdout);

    // Extract hash from response
    let hash = upload_stdout
        .split('"')
        .find(|s| s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit()))
        .map(|s| s.to_string());

    let hash = match hash {
        Some(h) => h,
        None => {
            println!("Could not extract hash from upload response");
            panic!("Upload failed: {}", upload_stdout);
        }
    };

    println!("CROSSLANG_HASH:{}", hash);

    // Pin the content
    let pin_output = Command::new("curl")
        .arg("-s")
        .arg("-X").arg("POST")
        .arg(format!("http://127.0.0.1:{}/api/pin/{}", port, hash))
        .output()
        .expect("Failed to pin");
    println!("Pin response: {}", String::from_utf8_lossy(&pin_output.stdout));

    // Signal ready
    println!("CROSSLANG_READY");
    println!("\nServer running at http://127.0.0.1:{}", port);
    println!("Waiting for cross-language peer connections...\n");

    // Run for 2 minutes to allow TS test to connect and sync
    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(120);

    while start.elapsed() < timeout {
        std::thread::sleep(Duration::from_secs(5));

        // Check peers via API
        let peers_output = Command::new("curl")
            .arg("-s")
            .arg(format!("http://127.0.0.1:{}/api/peers", port))
            .output();

        if let Ok(output) = peers_output {
            let peers_json = String::from_utf8_lossy(&output.stdout);
            if peers_json.contains("\"pubkey\"") {
                println!("Peers: {}", peers_json);

                // Parse and report connected peers
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&peers_json) {
                    if let Some(peers) = json.get("peers").and_then(|p| p.as_array()) {
                        for peer in peers {
                            if let Some(pk) = peer.get("pubkey").and_then(|p| p.as_str()) {
                                let has_dc = peer.get("has_data_channel")
                                    .and_then(|d| d.as_bool())
                                    .unwrap_or(false);
                                if has_dc {
                                    println!("CROSSLANG_CONNECTED:{}", pk);
                                }
                            }
                        }
                    }
                }
            }
        }

        println!("  {} seconds elapsed...", start.elapsed().as_secs());
    }

    println!("\n=== Cross-Language Peer Test Complete ===");
}
