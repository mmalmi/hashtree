//! Compare crypto backends through production tree writes and reads.
//!
//! Run the same release-built example before and after a backend change:
//! `cargo run --release -p hashtree-lmdb --example crypto_backend_bench -- 16 3`
//! Arguments are payload MiB and repetitions. Input generation and fresh-store
//! setup are excluded. LMDB imports include source reads and a final forced sync;
//! reads use warm OS caches. Each result is checked against the original bytes.

use futures::io::AllowStdIo;
use hashtree_core::{to_hex, HashTree, HashTreeConfig, MemoryStore, Store};
use hashtree_lmdb::LmdbBlobStore;
use serde_json::json;
use std::{error::Error, fs::File, path::Path, sync::Arc, time::Instant};
use tempfile::TempDir;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn payload(size: usize) -> Vec<u8> {
    let mut state = 0x63a9_7e21_d40b_f815u64;
    (0..size)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

async fn measure<S: Store>(
    store: Arc<S>,
    data: &[u8],
    source: Option<&Path>,
    encrypted: bool,
    iteration: usize,
    sync: impl FnOnce(&S) -> Result<()>,
) -> Result<()> {
    let config = HashTreeConfig::new(store.clone());
    let tree = HashTree::new(if encrypted { config } else { config.public() });
    let start = Instant::now();
    let (cid, size) = match source {
        Some(path) => tree.put_stream(AllowStdIo::new(File::open(path)?)).await?,
        None => tree.put(data).await?,
    };
    sync(&store)?;
    let put_ms = start.elapsed().as_secs_f64() * 1_000.0;
    assert_eq!(size, data.len() as u64);

    let start = Instant::now();
    let recovered = tree.get(&cid, Some(data.len() as u64)).await?;
    let get_ms = start.elapsed().as_secs_f64() * 1_000.0;
    assert_eq!(recovered.as_deref(), Some(data));
    println!(
        "{}",
        json!({
            "backend": if source.is_some() { "lmdb_import" } else { "memory" },
            "encrypted": encrypted,
            "iteration": iteration,
            "bytes": data.len(),
            "put_ms": put_ms,
            "get_ms": get_ms,
            "root": to_hex(&cid.hash),
        })
    );
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mib: usize = args.next().as_deref().unwrap_or("16").parse()?;
    let repetitions: usize = args.next().as_deref().unwrap_or("3").parse()?;
    if !(1..=256).contains(&mib) || !(1..=20).contains(&repetitions) || args.next().is_some() {
        return Err("usage: crypto_backend_bench [MiB: 1..256] [repetitions: 1..20]".into());
    }
    let data = payload(mib * 1024 * 1024);
    let input = TempDir::new()?;
    let source = input.path().join("input.bin");
    std::fs::write(&source, &data)?;

    // Warm up the crypto paths before measuring either backend.
    let warmup = HashTree::new(HashTreeConfig::new(Arc::new(MemoryStore::new())));
    let (cid, _) = warmup.put(&data[..1024 * 1024]).await?;
    assert_eq!(warmup.get(&cid, None).await?.unwrap(), data[..1024 * 1024]);
    drop(warmup);

    for iteration in 0..repetitions {
        for encrypted in [false, true] {
            measure(
                Arc::new(MemoryStore::new()),
                &data,
                None,
                encrypted,
                iteration,
                |_| Ok(()),
            )
            .await?;

            let directory = TempDir::new()?;
            let store = Arc::new(LmdbBlobStore::with_map_size_and_external_blob_options(
                directory.path(),
                (data.len() * 4).max(128 * 1024 * 1024),
                None,
            )?);
            measure(store, &data, Some(&source), encrypted, iteration, |store| {
                Ok(store.force_sync()?)
            })
            .await?;
        }
    }
    Ok(())
}
