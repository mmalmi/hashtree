//! `htree get`/`cat` open link-visible trees with their link secret, decrypting
//! locally. The secret never reaches the daemon or a Blossom server.

mod common;

use common::{htree_bin, test_relay::TestRelay};
use hashtree_cli::{Config, HashtreeStore};
use hashtree_config::StorageBackend;
use hashtree_core::{mask_object_key, nhash_encode, Cid, LinkKeyBytes};
use nostr::{EventBuilder, Keys, Kind, Tag, ToBech32};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

/// Serves `GET /<sha256>.bin` from a store (or 404s) and records each request head.
struct BlobServer {
    url: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl BlobServer {
    fn new(store: Option<Arc<HashtreeStore>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (store, recorded) = (store.clone(), Arc::clone(&recorded));
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut head = String::new();
                    // Request lines end at the blank line (or EOF); GET/HEAD carry no body.
                    while reader.read_line(&mut head).unwrap_or(0) > 2 {}
                    recorded.lock().unwrap().push(head.clone());
                    let mut words = head.split_whitespace();
                    let method = words.next().unwrap_or_default();
                    let blob = words
                        .next()
                        .and_then(|path| path.trim_start_matches('/').get(..64))
                        .and_then(|hex| hashtree_core::from_hex(hex).ok())
                        .and_then(|hash| store.as_ref()?.get_chunk(&hash).ok().flatten());
                    let mut stream = stream;
                    let _ = match blob {
                        Some(body) => {
                            let head = format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: application/octet-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                                body.len()
                            );
                            stream.write_all(head.as_bytes()).and_then(|_| {
                                if method == "GET" {
                                    stream.write_all(&body)
                                } else {
                                    Ok(())
                                }
                            })
                        }
                        None => stream.write_all(
                            b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                        ),
                    };
                });
            }
        });
        Self { url, requests }
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

struct Fixture {
    temp: TempDir,
    author: Keys,
    secret: LinkKeyBytes,
    legacy_secret: LinkKeyBytes,
    public_root: Cid,
    _relay: TestRelay,
    blossom: BlobServer,
    daemon: BlobServer,
}

/// Publishes `photos` (33-byte secret, with keyId) and `legacy` (32-byte
/// secret, no keyId) as link-visible trees whose blobs live only on Blossom.
fn fixture() -> Fixture {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    std::fs::create_dir_all(source.join("docs")).unwrap();
    std::fs::write(source.join("marker"), "link-visible hello").unwrap();
    std::fs::write(source.join("docs/readme.txt"), "nested secret").unwrap();
    let writer = Arc::new(
        HashtreeStore::new_with_backend(
            temp.path().join("writer"),
            StorageBackend::Fs,
            64 * 1024 * 1024,
        )
        .unwrap(),
    );
    let root = Cid::parse(&writer.upload_dir_encrypted(&source).unwrap()).unwrap();
    let public_root = Cid::parse(&writer.upload_dir(&source).unwrap()).unwrap();

    let author = Keys::generate();
    let secret = LinkKeyBytes::from_bytes(&[0x5a; 33]).unwrap();
    let legacy_secret = LinkKeyBytes::from_bytes(&[0x3c; 32]).unwrap();
    let relay = TestRelay::with_events(vec![
        link_visible_root(&author, "photos", &root, &secret, true),
        link_visible_root(&author, "legacy", &root, &legacy_secret, false),
    ]);
    let blossom = BlobServer::new(Some(writer));
    let daemon = BlobServer::new(None);

    let mut config = Config::default();
    config.nostr.enabled = true;
    config.nostr.relays = vec![relay.url()];
    config.sync.enabled = false;
    config.updater.auto_check = false;
    config.server.bind_address = daemon.url.trim_start_matches("http://").to_string();
    config.blossom.servers = Vec::new();
    config.blossom.read_servers = vec![blossom.url.clone()];
    config.blossom.write_servers = Vec::new();
    let mut config = toml::Value::try_from(config).unwrap();
    config["storage"]
        .as_table_mut()
        .unwrap()
        .insert("backend".into(), toml::Value::String("fs".into()));
    std::fs::create_dir_all(temp.path().join("config")).unwrap();
    std::fs::write(
        temp.path().join("config/config.toml"),
        toml::to_string(&config).unwrap(),
    )
    .unwrap();

    Fixture {
        temp,
        author,
        secret,
        legacy_secret,
        public_root,
        _relay: relay,
        blossom,
        daemon,
    }
}

fn link_visible_root(
    author: &Keys,
    tree: &str,
    root: &Cid,
    secret: &LinkKeyBytes,
    key_id: bool,
) -> Value {
    let masked = mask_object_key(&root.key.unwrap(), secret.as_ref()).unwrap();
    let mut tags = vec![
        Tag::identifier(tree),
        Tag::parse(["l", "hashtree"]).unwrap(),
        Tag::parse(["hash", &hex::encode(root.hash)]).unwrap(),
        Tag::parse(["encryptedKey", &hex::encode(masked)]).unwrap(),
    ];
    if key_id {
        tags.push(Tag::parse(["keyId", &secret.key_id()]).unwrap());
    }
    let event = EventBuilder::new(Kind::Custom(30064), "")
        .tags(tags)
        .sign_with_keys(author)
        .unwrap();
    serde_json::to_value(event).unwrap()
}

impl Fixture {
    fn npub(&self) -> String {
        self.author.public_key().to_bech32().unwrap()
    }

    /// A fresh data dir per run, so every blob is fetched again.
    fn htree(&self, args: &[&str]) -> Output {
        let data = tempfile::tempdir_in(self.temp.path()).unwrap();
        Command::new(htree_bin())
            .arg("--data-dir")
            .arg(data.path())
            .args(args)
            .env("HTREE_CONFIG_DIR", self.temp.path().join("config"))
            .env("HTREE_DATA_DIR", data.path())
            .env("HTREE_PREFER_LOCAL_DAEMON", "1")
            .output()
            .unwrap()
    }

    fn cat(&self, target: &str, extra: &[&str]) -> Output {
        let mut args = vec!["cat", target];
        args.extend_from_slice(extra);
        self.htree(&args)
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_fails_with(output: &Output, message: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "unexpected success: {stderr}");
    assert!(output.stdout.is_empty(), "no plaintext on failure");
    assert!(
        stderr.contains(message),
        "expected {message:?} in: {stderr}"
    );
}

#[test]
fn get_and_cat_open_link_visible_trees_without_sending_the_secret() {
    let fx = fixture();
    let (npub, secret, legacy) = (
        fx.npub(),
        hex::encode(fx.secret),
        hex::encode(fx.legacy_secret),
    );

    let out = fx.temp.path().join("photos");
    let output = fx.htree(&[
        "get",
        &format!("htree://{npub}/photos#k={secret}"),
        "--output",
        out.to_str().unwrap(),
    ]);
    assert_success(&output);
    assert_eq!(
        std::fs::read_to_string(out.join("marker")).unwrap(),
        "link-visible hello"
    );
    assert_eq!(
        std::fs::read_to_string(out.join("docs/readme.txt")).unwrap(),
        "nested secret"
    );

    let output = fx.cat(&format!("{npub}/photos/docs/readme.txt?k={secret}"), &[]);
    assert_success(&output);
    assert_eq!(output.stdout, b"nested secret");

    // An existing 32-byte secret still opens the root it masked.
    let output = fx.cat(
        &format!("htree://{npub}/legacy/marker"),
        &["--link-key", &legacy],
    );
    assert_success(&output);
    assert_eq!(output.stdout, b"link-visible hello");

    let (blossom, daemon) = (fx.blossom.requests(), fx.daemon.requests());
    let fetched_blob = |heads: &[String]| {
        heads
            .iter()
            .any(|head| head.starts_with("GET /") && head.contains(".bin HTTP/1.1"))
    };
    assert!(
        fetched_blob(&blossom) && fetched_blob(&daemon),
        "both read paths fetched blobs"
    );
    for head in blossom.iter().chain(&daemon) {
        for leaked in [secret.as_str(), legacy.as_str(), "k=", "#"] {
            assert!(!head.contains(leaked), "{leaked:?} sent in:\n{head}");
        }
    }
}

#[test]
fn wrong_conflicting_or_misplaced_link_keys_fail_clearly() {
    let fx = fixture();
    let npub = fx.npub();
    let (a, b) = ("aa".repeat(33), "bb".repeat(33));

    // keyId rejects a wrong secret before any blob is read.
    let output = fx.cat(&format!("{npub}/photos/marker#k={a}"), &[]);
    assert_fails_with(&output, "link key does not match this tree");
    // Without keyId, authenticated decryption still refuses a near miss.
    let mut near_miss = fx.legacy_secret.as_ref().to_vec();
    near_miss[31] ^= 1;
    let output = fx.cat(
        &format!("{npub}/legacy/marker#k={}", hex::encode(near_miss)),
        &[],
    );
    assert_fails_with(&output, "ecrypt");

    let output = fx.cat(&format!("{npub}/photos/marker?k={a}#k={b}"), &[]);
    assert_fails_with(&output, "conflicting link keys");
    let output = fx.cat(&format!("{npub}/photos/marker#k={a}"), &["--link-key", &b]);
    assert_fails_with(&output, "conflicting link keys");
    let output = fx.cat(&format!("{npub}/photos/marker#k=abc"), &[]);
    assert_fails_with(&output, "invalid link key");
    let nhash = nhash_encode(&fx.public_root.hash).unwrap();
    let output = fx.cat(&format!("{nhash}/marker#k={a}"), &[]);
    assert_fails_with(&output, "A link key applies only to npub/tree references");
}
