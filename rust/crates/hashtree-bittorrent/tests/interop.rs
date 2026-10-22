#![cfg(feature = "libtorrent")]
use hashtree_bittorrent::{Format, Metadata, NetworkOptions, Seed, Snapshot, Torrent};
use hashtree_core::{DirEntry, HashTree, HashTreeConfig, LinkType, MemoryStore};
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Arc,
    time::Duration,
};

fn python() -> String {
    std::env::var("HASHTREE_BITTORRENT_PYTHON").unwrap_or_else(|_| "python3".into())
}
fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/reference_client.py")
}
fn payload() -> Vec<u8> {
    (0..180_013)
        .map(|i| ((i * 13 + i / 256) % 256) as u8)
        .collect()
}
struct Client {
    child: Child,
    log: PathBuf,
}
impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn client(mode: &str, root: &Path, torrent: &Path, ready: &Path, extra: &str) -> Client {
    let log = ready.with_extension("log");
    let output = std::fs::File::create(&log).unwrap();
    let child = Command::new(python())
        .arg(script())
        .arg(mode)
        .arg(root)
        .arg(torrent)
        .arg(ready)
        .arg(extra)
        .stdout(Stdio::from(output.try_clone().unwrap()))
        .stderr(Stdio::from(output))
        .spawn()
        .expect("install libtorrent's Python binding and set HASHTREE_BITTORRENT_PYTHON");
    Client { child, log }
}
async fn ready(client: &mut Client, path: &Path) -> serde_json::Value {
    tokio::time::timeout(Duration::from_secs(50), async {
        loop {
            if let Ok(bytes) = std::fs::read(path) {
                if let Ok(value) = serde_json::from_slice(&bytes) {
                    return value;
                }
            }
            if let Some(status) = client.child.try_wait().unwrap() {
                panic!(
                    "reference client exited {status}: {}",
                    std::fs::read_to_string(&client.log).unwrap()
                );
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "reference client timed out: {}",
            std::fs::read_to_string(&client.log).unwrap()
        )
    })
}
async fn snapshot() -> Snapshot {
    let store = Arc::new(MemoryStore::new());
    let tree = HashTree::new(HashTreeConfig::new(store.clone()).with_chunk_size(7777));
    let (a, a_size) = tree.put(&vec![17; 513]).await.unwrap();
    let (b, b_size) = tree.put(&payload()).await.unwrap();
    let (empty, _) = tree.put(b"").await.unwrap();
    let nested = tree
        .put_directory(vec![DirEntry::from_cid("b.bin", &b)
            .with_size(b_size)
            .with_link_type(LinkType::File)])
        .await
        .unwrap();
    let root = tree
        .put_directory(vec![
            DirEntry::from_cid("a.bin", &a).with_size(a_size),
            DirEntry::from_cid("empty", &empty),
            DirEntry::from_cid("nested", &nested).with_link_type(LinkType::Dir),
        ])
        .await
        .unwrap();
    Snapshot::new(store, root, "bundle", true).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn ordinary_clients_download_hashtree_snapshots_in_all_formats() {
    for format in [Format::V1, Format::V2, Format::Hybrid] {
        let dir = tempfile::tempdir().unwrap();
        let meta = Metadata::create(snapshot().await, format, 32768)
            .await
            .unwrap();
        let file = dir.path().join("snapshot.torrent");
        std::fs::write(&file, meta.torrent_bytes().unwrap()).unwrap();
        let seed = Seed::start(meta, NetworkOptions::default()).await.unwrap();
        let port = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let port = seed.stats().await.unwrap().port;
                if port != 0 {
                    return port;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let marker = dir.path().join("complete.json");
        let mut downloader = client("download", dir.path(), &file, &marker, &port.to_string());
        ready(&mut downloader, &marker).await;
        assert_eq!(
            std::fs::read(dir.path().join("bundle/a.bin")).unwrap(),
            vec![17; 513]
        );
        assert_eq!(
            std::fs::read(dir.path().join("bundle/nested/b.bin")).unwrap(),
            payload()
        );
        assert_eq!(
            std::fs::metadata(dir.path().join("bundle/empty"))
                .unwrap()
                .len(),
            0
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn hashtree_reads_ranges_from_ordinary_seeders_in_all_formats() {
    for format in ["v1", "v2", "hybrid"] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("bundle/nested")).unwrap();
        std::fs::write(dir.path().join("bundle/a.bin"), vec![17; 513]).unwrap();
        std::fs::write(dir.path().join("bundle/empty"), []).unwrap();
        std::fs::write(dir.path().join("bundle/nested/b.bin"), payload()).unwrap();
        let file = dir.path().join("reference.torrent");
        let marker = dir.path().join("ready.json");
        let mut creator = client("create", dir.path(), &file, &marker, format);
        assert!(
            creator.child.wait().unwrap().success(),
            "{}",
            std::fs::read_to_string(&creator.log).unwrap()
        );
        let mut seeder = client("seed", dir.path(), &file, &marker, "");
        let status = ready(&mut seeder, &marker).await;
        let meta = Metadata::parse(&std::fs::read(&file).unwrap()).unwrap();
        let index = meta
            .files()
            .iter()
            .find(|f| f.path.ends_with("nested/b.bin"))
            .unwrap()
            .index;
        let torrent = Torrent::open(
            meta,
            NetworkOptions {
                peers: vec![format!("127.0.0.1:{}", status["port"].as_u64().unwrap())
                    .parse()
                    .unwrap()],
                cache_bytes: 65536,
                timeout: Duration::from_secs(20),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let got = torrent.read_range(index, 32000, 2000).await.unwrap();
        assert_eq!(got, payload()[32000..34000], "{format}");
        let stats = torrent.stats().await.unwrap();
        assert!(stats.downloaded <= 65536, "{format}: {}", stats.downloaded);
        assert!(stats.cache_bytes <= 65536);
    }
}
