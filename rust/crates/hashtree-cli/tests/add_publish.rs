mod common;

use common::htree_bin;
use common::test_relay::TestRelay;
use nostr::{Keys, ToBech32};
use std::io::Write;
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;
use tempfile::TempDir;

struct HttpStatusServer {
    url: String,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl HttpStatusServer {
    fn new(status: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind HTTP fixture");
        let addr = listener.local_addr().expect("HTTP fixture address");
        listener.set_nonblocking(true).expect("nonblocking fixture");
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let response =
            format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        let worker = std::thread::spawn(move || {
            while !worker_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.write_all(response.as_bytes());
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            url: format!("http://{addr}"),
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for HttpStatusServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            worker.join().expect("join HTTP fixture");
        }
    }
}

fn write_config(config_dir: &Path, relay: &str, file_server: &str) {
    std::fs::create_dir_all(config_dir).expect("create config dir");
    std::fs::write(
        config_dir.join("config.toml"),
        format!(
            "[nostr]\nrelays = [\"{relay}\"]\n\n[blossom]\nwrite_servers = [\"{file_server}\"]\n"
        ),
    )
    .expect("write config");
    let keys = Keys::generate();
    let nsec = keys
        .secret_key()
        .to_bech32()
        .expect("encode test secret key");
    std::fs::write(config_dir.join("keys"), nsec).expect("write keys");
}

fn run_published_add(temp: &TempDir, relay: &str, file_server: &str) -> Output {
    let config_dir = temp.path().join("config");
    let data_dir = temp.path().join("data");
    let source = temp.path().join("index.html");
    std::fs::write(&source, "<h1>published</h1>").expect("write source");
    write_config(&config_dir, relay, file_server);

    Command::new(htree_bin())
        .arg("--data-dir")
        .arg(data_dir)
        .arg("add")
        .arg(source)
        .arg("--unencrypted")
        .arg("--publish")
        .arg("test-site")
        .env("HTREE_CONFIG_DIR", config_dir)
        .output()
        .expect("run published add")
}

#[test]
fn explicit_publish_fails_when_file_servers_reject_content() {
    let relay = TestRelay::new();
    let file_server = HttpStatusServer::new("403 Forbidden");
    let temp = TempDir::new().expect("temp dir");

    let output = run_published_add(&temp, &relay.url(), &file_server.url);

    assert!(
        !output.status.success(),
        "required file publication must fail\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("published:"));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Failed to push content to file servers")
    );
}

#[test]
fn explicit_publish_fails_when_no_nostr_relay_accepts_the_root() {
    let file_server = HttpStatusServer::new("201 Created");
    let temp = TempDir::new().expect("temp dir");

    let output = run_published_add(&temp, "ws://127.0.0.1:1", &file_server.url);

    assert!(
        !output.status.success(),
        "required Nostr publication must fail\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("published:"));
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("No configured Nostr relay accepted the published root"));
}

#[test]
fn explicit_publish_reports_success_after_both_stages_accept() {
    let relay = TestRelay::new();
    let file_server = HttpStatusServer::new("201 Created");
    let temp = TempDir::new().expect("temp dir");

    let output = run_published_add(&temp, &relay.url(), &file_server.url);

    assert!(
        output.status.success(),
        "published add failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("published:"));
}
