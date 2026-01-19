//! Git remote helper for hashtree
//!
//! This crate provides a git remote helper that allows pushing/pulling
//! git repositories via nostr and hashtree.
//!
//! ## Usage
//!
//! ```bash
//! git remote add origin htree://<pubkey>/<repo-name>
//! git remote add origin htree://<petname>/<repo-name>
//! git push origin main
//! git pull origin main
//! ```
//!
//! ## Encryption Modes
//!
//! - **Unencrypted**: No CHK, just hash - anyone with hash can read
//! - **Public**: CHK encrypted, `["key", "<hex>"]` in event - anyone can decrypt
//! - **Link-visible**: CHK + XOR mask, `["encryptedKey", XOR(key,secret)]` - need `#k=<secret>` URL
//! - **Private**: CHK + NIP-44 to self, `["selfEncryptedKey", "..."]` - author only
//!
//! Default is **Public** (CHK encrypted, key in nostr event).
//!
//! ## Creating Link-Visible Repos
//!
//! To create a link-visible repo, use `#link-visible` to auto-generate a key:
//! ```bash
//! git remote add origin htree://self/repo#link-visible
//! git push origin main
//! # After push, you'll see instructions to update the remote URL with the generated key
//! ```
//!
//! Or specify an explicit key with `#k=<secret>`:
//! ```bash
//! git remote add origin htree://npub/repo#k=<64-hex-chars>
//! ```

use anyhow::{bail, Context, Result};
use nostr_sdk::ToBech32;
use std::io::{BufRead, Write};
use tracing::{debug, info, warn};

pub mod git;
mod helper;
mod nostr_client;

use hashtree_config::Config;
use helper::RemoteHelper;
use nostr_client::resolve_identity;

/// Entry point for the git remote helper
/// Call this from main() to run the helper
pub fn main_entry() {
    // Install TLS crypto provider for reqwest/rustls
    let _ = rustls::crypto::ring::default_provider().install_default();

    if let Err(e) = run() {
        eprintln!("Error: {:#}", e);
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    // Suppress broken pipe panics - git may close the pipe early
    #[cfg(unix)]
    {
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
    }

    // Initialize logging - only show errors by default
    // Set RUST_LOG=debug for verbose output
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("git_remote_htree=error".parse().unwrap())
                .add_directive("nostr_relay_pool=off".parse().unwrap()),
        )
        .with_writer(std::io::stderr)
        .init();

    let args: Vec<String> = std::env::args().collect();
    debug!("git-remote-htree called with args: {:?}", args);

    // Git calls us as: git-remote-htree <remote-name> <url>
    if args.len() < 3 {
        bail!("Usage: git-remote-htree <remote-name> <url>");
    }

    let remote_name = &args[1];
    let url = &args[2];

    info!("Remote: {}, URL: {}", remote_name, url);

    // Parse URL: htree://<identifier>/<repo-name>#k=<secret>
    let parsed = parse_htree_url(url)?;
    let identifier = parsed.identifier;
    let repo_name = parsed.repo_name;
    let is_private = parsed.is_private; // Self-only visibility

    // Handle link-visible mode: either explicit key from URL or fail with setup instructions
    let url_secret = if let Some(key) = parsed.secret_key {
        // Explicit key from #k=<hex>
        Some(key)
    } else if parsed.auto_generate_secret {
        // #link-visible - generate key and fail with setup instructions
        let key = generate_secret_key();
        let secret_hex = hex::encode(key);

        // We need npub for the shareable URL, resolve identity first
        let npub = match resolve_identity(&identifier) {
            Ok((pubkey, _)) => {
                hex::decode(&pubkey)
                    .ok()
                    .filter(|b| b.len() == 32)
                    .and_then(|pk_bytes| nostr_sdk::PublicKey::from_slice(&pk_bytes).ok())
                    .and_then(|pk| pk.to_bech32().ok())
                    .unwrap_or(pubkey)
            }
            Err(_) => identifier.clone(),
        };

        let local_url = format!("htree://{}/{}#k={}", identifier, repo_name, secret_hex);
        let share_url = format!("htree://{}/{}#k={}", npub, repo_name, secret_hex);

        eprintln!();
        eprintln!("=== Link-Visible Repository Setup ===");
        eprintln!();
        eprintln!("A secret key has been generated for this link-visible repository.");
        eprintln!();
        eprintln!("Step 1: Update your remote URL with the generated key:");
        eprintln!("  git remote set-url {} {}", remote_name, local_url);
        eprintln!();
        eprintln!("Step 2: Push again (same command you just ran)");
        eprintln!();
        eprintln!("Shareable URL (for others to clone):");
        eprintln!("  {}", share_url);
        eprintln!();

        // Exit without error code so git doesn't show confusing messages
        std::process::exit(0);
    } else {
        None
    };

    if is_private {
        info!("Private repo mode: only author can decrypt");
    } else if url_secret.is_some() {
        info!("Link-visible repo mode: using secret key from URL");
    }

    // Resolve identifier to pubkey
    // If "self" is used and no keys exist, auto-generate
    let (pubkey, signing_key) = match resolve_identity(&identifier) {
        Ok(result) => result,
        Err(e) => {
            // If resolution failed and user intended "self", suggest using htree://self/repo
            warn!("Failed to resolve identity '{}': {}", identifier, e);
            info!("Tip: Use htree://self/<repo> to auto-generate identity on first use");
            return Err(e);
        }
    };

    if signing_key.is_some() {
        debug!("Found signing key for {}", identifier);
    } else {
        debug!("No signing key for {} (read-only)", identifier);
    }

    // Convert pubkey to npub for display and shareable URLs
    let npub = hex::decode(&pubkey)
        .ok()
        .filter(|b| b.len() == 32)
        .and_then(|pk_bytes| nostr_sdk::PublicKey::from_slice(&pk_bytes).ok())
        .and_then(|pk| pk.to_bech32().ok())
        .unwrap_or_else(|| pubkey.clone());

    info!("Using identity: {}", npub);

    // Load config
    let mut config = Config::load_or_default();
    debug!("Loaded config with {} read servers, {} write servers",
           config.blossom.read_servers.len(),
           config.blossom.write_servers.len());

    // Check for local daemon and use it if available
    let daemon_url = detect_local_daemon(Some(&config.server.bind_address));
    if let Some(ref url) = daemon_url {
        debug!("Local daemon detected at {}", url);
        // Prepend local daemon to read servers for cascade fetching
        config.blossom.read_servers.insert(0, url.clone());
    } else {
        // Show hint once per session (git may call us multiple times)
        static HINT_SHOWN: std::sync::Once = std::sync::Once::new();
        HINT_SHOWN.call_once(|| {
            eprintln!("Tip: run 'htree start' for P2P sharing");
        });
    }

    // Create helper and run protocol
    let mut helper =
        RemoteHelper::new(&pubkey, &repo_name, signing_key, url_secret, is_private, config)?;

    // Read commands from stdin, write responses to stdout
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                if e.kind() == std::io::ErrorKind::BrokenPipe {
                    break;
                }
                return Err(e.into());
            }
        };

        debug!("< {}", line);

        match helper.handle_command(&line) {
            Ok(Some(responses)) => {
                for response in responses {
                    debug!("> {}", response);
                    if let Err(e) = writeln!(stdout, "{}", response) {
                        if e.kind() == std::io::ErrorKind::BrokenPipe {
                            break;
                        }
                        return Err(e.into());
                    }
                }
                if let Err(e) = stdout.flush() {
                    if e.kind() == std::io::ErrorKind::BrokenPipe {
                        break;
                    }
                    return Err(e.into());
                }
            }
            Ok(None) => {}
            Err(e) => {
                warn!("Command error: {}", e);
                // Exit on error to avoid hanging
                return Err(e);
            }
        }

        if helper.should_exit() {
            break;
        }
    }

    Ok(())
}

/// Parsed htree URL components
pub struct ParsedUrl {
    pub identifier: String,
    pub repo_name: String,
    /// Secret key from #k=<hex> fragment (for link-visible repos)
    pub secret_key: Option<[u8; 32]>,
    /// Whether this is a private (self-only) repo from #private fragment
    pub is_private: bool,
    /// Whether to auto-generate a secret key (from #link-visible fragment)
    pub auto_generate_secret: bool,
}

/// Parse htree:// URL into components
/// Supports:
/// - htree://identifier/repo - public repo
/// - htree://identifier/repo#k=<hex> - link-visible repo with explicit key
/// - htree://identifier/repo#link-visible - link-visible repo (auto-generate key)
/// - htree://identifier/repo#private - private (self-only) repo
fn parse_htree_url(url: &str) -> Result<ParsedUrl> {
    let url = url
        .strip_prefix("htree://")
        .context("URL must start with htree://")?;

    // Split off fragment (#k=secret, #link-visible, or #private) if present
    let (url_path, secret_key, is_private, auto_generate_secret) = if let Some((path, fragment)) = url.split_once('#') {
        if fragment == "private" {
            // #private - self-only visibility
            (path, None, true, false)
        } else if fragment == "link-visible" {
            // #link-visible - auto-generate key on push
            (path, None, false, true)
        } else if let Some(key_hex) = fragment.strip_prefix("k=") {
            // #k=<hex> - link-visible with explicit key
            let bytes = hex::decode(key_hex)
                .context("Invalid secret key hex in URL fragment")?;
            if bytes.len() != 32 {
                bail!("Secret key must be 32 bytes (64 hex chars)");
            }
            let mut key = [0u8; 32];
            key.copy_from_slice(&bytes);
            (path, Some(key), false, false)
        } else {
            // Unknown fragment - error to prevent accidental public push
            bail!(
                "Unknown URL fragment '#{}'. Valid options:\n\
                 - #k=<64-hex-chars>  Link-visible with explicit key\n\
                 - #link-visible      Link-visible with auto-generated key\n\
                 - #private           Author-only (NIP-44 encrypted)\n\
                 - (no fragment)      Public",
                fragment
            );
        }
    } else {
        (url, None, false, false)
    };

    // Split on first /
    let (identifier, repo) = url_path
        .split_once('/')
        .context("URL must be htree://<identifier>/<repo>")?;

    // Handle repo paths like "repo/subpath" - keep full path as repo name
    let repo_name = repo.to_string();

    if identifier.is_empty() {
        bail!("Identifier cannot be empty");
    }
    if repo_name.is_empty() {
        bail!("Repository name cannot be empty");
    }

    Ok(ParsedUrl {
        identifier: identifier.to_string(),
        repo_name,
        secret_key,
        is_private,
        auto_generate_secret,
    })
}

/// Generate a new random secret key for private repos
pub fn generate_secret_key() -> [u8; 32] {
    let mut key = [0u8; 32];
    getrandom::fill(&mut key).expect("Failed to generate random bytes");
    key
}

/// Detect if local htree daemon is running
/// Returns the daemon URL if available
fn detect_local_daemon(bind_address: Option<&str>) -> Option<String> {
    hashtree_config::detect_local_daemon_url(bind_address)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_htree_url_pubkey() {
        let parsed = parse_htree_url(
            "htree://a9a91ed5f1c405618f63fdd393f9055ab8bac281102cff6b1ac3c74094562dd8/myrepo",
        )
        .unwrap();
        assert_eq!(
            parsed.identifier,
            "a9a91ed5f1c405618f63fdd393f9055ab8bac281102cff6b1ac3c74094562dd8"
        );
        assert_eq!(parsed.repo_name, "myrepo");
        assert!(parsed.secret_key.is_none());
    }

    #[test]
    fn test_parse_htree_url_npub() {
        let parsed =
            parse_htree_url("htree://npub1qvmu0aru530g6yu3kmlhw33fh68r75wf3wuml3vk4ekg0p4m4t6s7fuhxx/test")
                .unwrap();
        assert!(parsed.identifier.starts_with("npub1"));
        assert_eq!(parsed.repo_name, "test");
        assert!(parsed.secret_key.is_none());
    }

    #[test]
    fn test_parse_htree_url_petname() {
        let parsed = parse_htree_url("htree://alice/project").unwrap();
        assert_eq!(parsed.identifier, "alice");
        assert_eq!(parsed.repo_name, "project");
        assert!(parsed.secret_key.is_none());
    }

    #[test]
    fn test_parse_htree_url_self() {
        let parsed = parse_htree_url("htree://self/myrepo").unwrap();
        assert_eq!(parsed.identifier, "self");
        assert_eq!(parsed.repo_name, "myrepo");
        assert!(parsed.secret_key.is_none());
    }

    #[test]
    fn test_parse_htree_url_with_subpath() {
        let parsed = parse_htree_url("htree://test/repo/some/path").unwrap();
        assert_eq!(parsed.identifier, "test");
        assert_eq!(parsed.repo_name, "repo/some/path");
        assert!(parsed.secret_key.is_none());
    }

    #[test]
    fn test_parse_htree_url_with_secret() {
        let secret_hex = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let url = format!("htree://test/repo#k={}", secret_hex);
        let parsed = parse_htree_url(&url).unwrap();
        assert_eq!(parsed.identifier, "test");
        assert_eq!(parsed.repo_name, "repo");
        assert!(parsed.secret_key.is_some());
        let key = parsed.secret_key.unwrap();
        assert_eq!(hex::encode(key), secret_hex);
    }

    #[test]
    fn test_parse_htree_url_invalid_secret_length() {
        // Secret too short
        let url = "htree://test/repo#k=0123456789abcdef";
        assert!(parse_htree_url(url).is_err());
    }

    #[test]
    fn test_parse_htree_url_invalid_secret_hex() {
        // Invalid hex characters
        let url = "htree://test/repo#k=ghij456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert!(parse_htree_url(url).is_err());
    }

    #[test]
    fn test_parse_htree_url_invalid_scheme() {
        assert!(parse_htree_url("https://example.com/repo").is_err());
    }

    #[test]
    fn test_parse_htree_url_no_repo() {
        assert!(parse_htree_url("htree://pubkey").is_err());
    }

    #[test]
    fn test_parse_htree_url_empty_identifier() {
        assert!(parse_htree_url("htree:///repo").is_err());
    }

    #[test]
    fn test_parse_htree_url_colon() {
        // Some git versions may pass URL with : instead of /
        let result = parse_htree_url("htree://test:repo");
        assert!(result.is_err()); // We don't support : syntax
    }

    #[test]
    fn test_parse_htree_url_private() {
        let parsed = parse_htree_url("htree://self/myrepo#private").unwrap();
        assert_eq!(parsed.identifier, "self");
        assert_eq!(parsed.repo_name, "myrepo");
        assert!(parsed.is_private);
        assert!(parsed.secret_key.is_none());
    }

    #[test]
    fn test_parse_htree_url_secret_not_private() {
        // #k= is link-visible, not private
        let secret_hex = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let url = format!("htree://test/repo#k={}", secret_hex);
        let parsed = parse_htree_url(&url).unwrap();
        assert!(!parsed.is_private);
        assert!(parsed.secret_key.is_some());
    }

    #[test]
    fn test_parse_htree_url_public() {
        // No fragment = public
        let parsed = parse_htree_url("htree://test/repo").unwrap();
        assert!(!parsed.is_private);
        assert!(parsed.secret_key.is_none());
        assert!(!parsed.auto_generate_secret);
    }

    #[test]
    fn test_parse_htree_url_link_visible_auto() {
        // #link-visible = auto-generate key
        let parsed = parse_htree_url("htree://self/myrepo#link-visible").unwrap();
        assert_eq!(parsed.identifier, "self");
        assert_eq!(parsed.repo_name, "myrepo");
        assert!(!parsed.is_private);
        assert!(parsed.secret_key.is_none()); // Key will be generated at runtime
        assert!(parsed.auto_generate_secret);
    }

    #[test]
    fn test_parse_htree_url_link_visible_explicit_key() {
        // #k=<hex> = explicit key, not auto-generate
        let secret_hex = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let url = format!("htree://test/repo#k={}", secret_hex);
        let parsed = parse_htree_url(&url).unwrap();
        assert!(parsed.secret_key.is_some());
        assert!(!parsed.auto_generate_secret); // Not auto-generated
    }

    #[test]
    fn test_parse_htree_url_private_not_auto_generate() {
        // #private is not auto_generate_secret
        let parsed = parse_htree_url("htree://self/myrepo#private").unwrap();
        assert!(parsed.is_private);
        assert!(!parsed.auto_generate_secret);
    }

    #[test]
    fn test_detect_local_daemon_not_running() {
        // When no daemon is running on port 8080, should return None
        // This test assumes port 8080 is not in use during testing
        let result = detect_local_daemon(None);
        // Can't assert None because a daemon might be running
        // Just verify it doesn't panic and returns valid result
        if let Some(url) = result {
            assert!(url.starts_with("http://"));
            assert!(url.contains("8080"));
        }
    }

    #[test]
    fn test_detect_local_daemon_with_listener() {
        use std::net::TcpListener;

        // Bind to a random port
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        drop(listener);
        let addr = format!("127.0.0.1:{}", port);
        let result = detect_local_daemon(Some(&addr));
        assert!(result.is_none());
    }
}
