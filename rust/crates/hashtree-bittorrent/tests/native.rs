#![cfg(feature = "libtorrent")]
use hashtree_bittorrent::{
    BlobMapping, Error, Format, Metadata, NetworkOptions, Seed, Snapshot, Torrent, TorrentRoute,
};
use hashtree_core::{
    sha256, BlobReply, BlobRequest, BlobRoute, HashTree, HashTreeConfig, MemoryStore,
};
use std::{sync::Arc, time::Duration};

async fn fixture(format: Format, cache: usize) -> (Vec<u8>, Metadata, Seed, NetworkOptions) {
    let bytes: Vec<_> = (0..196_733usize)
        .map(|i| ((i * 31 + i / 251) % 256) as u8)
        .collect();
    let store = Arc::new(MemoryStore::new());
    let tree = HashTree::new(HashTreeConfig::new(store.clone()).with_chunk_size(29_000));
    let (cid, _) = tree.put(&bytes).await.unwrap();
    let snapshot = Snapshot::new(store, cid, "payload.bin", true)
        .await
        .unwrap();
    let meta = Metadata::create(snapshot, format, 32768).await.unwrap();
    let seed = Seed::start(meta.clone(), NetworkOptions::default())
        .await
        .unwrap();
    let port = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let port = seed.stats().await.unwrap().port;
            if port != 0 {
                break port;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let options = NetworkOptions {
        peers: vec![format!("127.0.0.1:{port}").parse().unwrap()],
        cache_bytes: cache,
        timeout: Duration::from_secs(15),
        ..Default::default()
    };
    (bytes, meta, seed, options)
}

#[tokio::test(flavor = "multi_thread")]
async fn v1_v2_and_hybrid_range_reads_and_bounded_eviction() {
    for format in [Format::V1, Format::V2, Format::Hybrid] {
        let (bytes, meta, _seed, options) = fixture(format, 32768).await;
        let torrent = Torrent::open(
            Metadata::parse(&meta.torrent_bytes().unwrap()).unwrap(),
            options,
        )
        .await
        .unwrap();
        for (start, len) in [(100, 99), (65520, 80), (0, 100), (196_700, 33)] {
            let got = torrent.read_range(0, start, len).await.unwrap();
            assert_eq!(
                got,
                bytes[start as usize..start as usize + len],
                "{format:?}"
            );
            assert!(torrent.stats().await.unwrap().cache_bytes <= 32768);
        }
        assert!(torrent.stats().await.unwrap().downloaded < bytes.len() as u64);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn magnets_materialization_and_hash_routes() {
    let (bytes, meta, _seed, options) = fixture(Format::Hybrid, 262144).await;
    let torrent = Torrent::magnet(&meta.magnet().unwrap(), options)
        .await
        .unwrap();
    let hash = sha256(&bytes[20..80]);
    let route = TorrentRoute::new(
        torrent.clone(),
        vec![BlobMapping {
            hash,
            file: 0,
            offset: 20,
            length: 60,
        }],
    )
    .unwrap();
    assert_eq!(
        route.route(BlobRequest { hash, htl: 3 }).await.unwrap(),
        BlobReply::Data(bytes[20..80].to_vec())
    );
    assert_eq!(
        route
            .route(BlobRequest {
                hash: [0; 32],
                htl: 3
            })
            .await
            .unwrap(),
        BlobReply::NoResult
    );
    let bad = TorrentRoute::new(
        torrent.clone(),
        vec![BlobMapping {
            hash: [9; 32],
            file: 0,
            offset: 20,
            length: 60,
        }],
    )
    .unwrap();
    assert!(bad
        .route(BlobRequest {
            hash: [9; 32],
            htl: 3
        })
        .await
        .is_err());
    let store = Arc::new(MemoryStore::new());
    let config = HashTreeConfig::new(store.clone());
    let (cid, size) = torrent.materialize(0, config).await.unwrap();
    let direct = HashTree::new(HashTreeConfig::new(store))
        .put(&bytes)
        .await
        .unwrap();
    assert_eq!((cid, size), direct);
}

#[tokio::test(flavor = "multi_thread")]
async fn timeouts_are_errors_and_cancelled_reads_release_the_session() {
    let (bytes, meta, _seed, peers) = fixture(Format::V2, 32768).await;
    let options = NetworkOptions {
        timeout: Duration::from_secs(2),
        ..Default::default()
    };
    let torrent = Torrent::open(meta, options).await.unwrap();
    assert!(matches!(
        torrent.read_range(0, 0, 1).await,
        Err(Error::Timeout)
    ));
    let cancel = tokio::time::timeout(Duration::from_millis(10), torrent.read_range(0, 0, 1)).await;
    assert!(cancel.is_err());
    tokio::time::timeout(Duration::from_secs(2), torrent.stats())
        .await
        .unwrap()
        .unwrap();
    torrent.connect(peers.peers[0]).await.unwrap();
    assert_eq!(torrent.read_range(0, 0, 100).await.unwrap(), bytes[..100]);
}

#[tokio::test(flavor = "multi_thread")]
async fn metadata_validation_and_seed_permission() {
    assert!(Metadata::parse(b"not bencode").is_err());
    let (_, meta, _seed, _) = fixture(Format::Hybrid, 32768).await;
    let parsed = Metadata::parse(&meta.torrent_bytes().unwrap()).unwrap();
    assert_eq!(parsed.magnet().unwrap(), meta.magnet().unwrap());
    assert!(Seed::start(parsed, NetworkOptions::default())
        .await
        .is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn cancellation_during_eviction_remains_recoverable() {
    let (bytes, meta, _seed, mut options) = fixture(Format::V2, 32768).await;
    options.timeout = Duration::from_secs(3);
    let torrent = Torrent::open(meta, options).await.unwrap();
    torrent.read_range(0, 0, 100).await.unwrap();
    let mut cancelled = 0;
    for piece in 1..6 {
        let offset = piece * 32768;
        if tokio::time::timeout(Duration::from_millis(1), torrent.read_range(0, offset, 100))
            .await
            .is_err()
        {
            cancelled += 1;
        }
        assert_eq!(
            torrent.read_range(0, offset, 100).await.unwrap(),
            bytes[offset as usize..offset as usize + 100]
        );
        assert!(torrent.stats().await.unwrap().cache_bytes <= 32768);
    }
    assert!(cancelled > 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn metadata_cannot_enable_trackers_or_web_seeds() {
    let (_, meta, _seed, _) = fixture(Format::V2, 32768).await;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/announce", listener.local_addr().unwrap());
    let value = format!("{}:{url}", url.len());
    let mut bytes =
        format!("d8:announce{value}9:httpseedsl{value}e8:url-listl{value}e").into_bytes();
    bytes.extend_from_slice(&meta.torrent_bytes().unwrap()[1..]);
    let metadata = Metadata::parse(&bytes).unwrap();
    assert_eq!(metadata.magnet().unwrap(), meta.magnet().unwrap());
    let options = NetworkOptions {
        timeout: Duration::from_secs(2),
        ..Default::default()
    };
    let torrent = Torrent::open(metadata.clone(), options.clone())
        .await
        .unwrap();
    assert!(matches!(
        torrent.read_range(0, 0, 1).await,
        Err(Error::Timeout)
    ));
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    drop(torrent);

    // Positive control: the same embedded tracker is contacted when allowed.
    let enabled = Torrent::open(
        metadata,
        NetworkOptions {
            trackers: true,
            ..options
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        enabled.read_range(0, 0, 1).await,
        Err(Error::Timeout)
    ));
    assert!(listener.accept().is_ok());
}
