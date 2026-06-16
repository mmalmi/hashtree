use hashtree_cli::storage::HashtreeStore;
use hashtree_core::sha256;
use sha2::{Digest, Sha256};
use std::error::Error;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Cached,
    CachedBatch,
}

#[derive(Debug)]
struct Config {
    data_dir: PathBuf,
    requests: usize,
    batch_size: usize,
    size: usize,
    max_bytes: u64,
    seed: String,
    mode: Mode,
    delete_after: bool,
}

fn main() -> Result<(), Box<dyn Error>> {
    let config = Config::parse()?;
    let store = HashtreeStore::with_options(&config.data_dir, None, config.max_bytes)?;
    let started = Instant::now();
    let mut samples = Vec::new();
    let mut inserted = 0usize;
    let mut written_hashes = Vec::new();

    match config.mode {
        Mode::Cached => {
            for index in 0..config.requests {
                let data = deterministic_payload(&config.seed, index, config.size);
                let hash = sha256(&data);
                let sample_started = Instant::now();
                store.put_cached_blob(&data)?;
                samples.push(sample_started.elapsed());
                inserted += 1;
                written_hashes.push(hash);
            }
        }
        Mode::CachedBatch => {
            let mut index = 0usize;
            while index < config.requests {
                let end = (index + config.batch_size).min(config.requests);
                let mut batch = Vec::with_capacity(end - index);
                for item_index in index..end {
                    let data = deterministic_payload(&config.seed, item_index, config.size);
                    let hash = sha256(&data);
                    written_hashes.push(hash);
                    batch.push((hash, data));
                }
                let sample_started = Instant::now();
                inserted += store.put_cached_blobs(&batch)?;
                samples.push(sample_started.elapsed());
                index = end;
            }
        }
    }

    let wall = started.elapsed();
    let candidate_mib = config.requests as f64 * config.size as f64 / 1024.0 / 1024.0;
    let inserted_mib = inserted as f64 * config.size as f64 / 1024.0 / 1024.0;
    let (candidate_throughput_mib_s, inserted_throughput_mib_s) = if wall.as_secs_f64() > 0.0 {
        (
            candidate_mib / wall.as_secs_f64(),
            inserted_mib / wall.as_secs_f64(),
        )
    } else {
        (0.0, 0.0)
    };
    samples.sort_unstable();

    println!("data_dir={}", config.data_dir.display());
    println!(
        "mode={:?} requests={} batch_size={} size={} max_bytes={} seed={}",
        config.mode, config.requests, config.batch_size, config.size, config.max_bytes, config.seed
    );
    println!(
        "inserted={} wall_ms={} candidate_throughput_mib_s={:.2} inserted_throughput_mib_s={:.2}",
        inserted,
        wall.as_millis(),
        candidate_throughput_mib_s,
        inserted_throughput_mib_s
    );
    if !samples.is_empty() {
        println!(
            "sample_latency_ms p50={} p95={} p99={} max={}",
            percentile_ms(&samples, 50),
            percentile_ms(&samples, 95),
            percentile_ms(&samples, 99),
            samples.last().unwrap().as_millis()
        );
    }
    if config.delete_after {
        let cleanup_started = Instant::now();
        let mut deleted = 0usize;
        for hash in written_hashes {
            if store.router().delete_sync(&hash)? {
                deleted += 1;
            }
        }
        println!(
            "cleanup deleted={} cleanup_ms={}",
            deleted,
            cleanup_started.elapsed().as_millis()
        );
    }
    Ok(())
}

impl Config {
    fn parse() -> Result<Self, Box<dyn Error>> {
        let mut config = Self {
            data_dir: PathBuf::from("/tmp/hashtree-storage-write-bench"),
            requests: 128,
            batch_size: 32,
            size: 256 * 1024,
            max_bytes: 1024 * 1024 * 1024,
            seed: "storage-write-bench".to_string(),
            mode: Mode::Cached,
            delete_after: false,
        };

        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--data-dir" => {
                    config.data_dir = PathBuf::from(required_value(&mut args, "--data-dir")?)
                }
                "--requests" => {
                    config.requests = required_value(&mut args, "--requests")?.parse()?
                }
                "--batch-size" => {
                    config.batch_size = required_value(&mut args, "--batch-size")?.parse()?
                }
                "--size" => config.size = required_value(&mut args, "--size")?.parse()?,
                "--max-bytes" => {
                    config.max_bytes = required_value(&mut args, "--max-bytes")?.parse()?
                }
                "--seed" => config.seed = required_value(&mut args, "--seed")?,
                "--mode" => {
                    config.mode = match required_value(&mut args, "--mode")?.as_str() {
                        "cached" => Mode::Cached,
                        "cached-batch" => Mode::CachedBatch,
                        other => return Err(format!("unknown mode: {other}").into()),
                    }
                }
                "--delete-after" => config.delete_after = true,
                "--help" | "-h" => {
                    print_usage();
                    std::process::exit(0);
                }
                other => return Err(format!("unknown argument: {other}").into()),
            }
        }

        if config.requests == 0 {
            return Err("--requests must be greater than zero".into());
        }
        if config.batch_size == 0 {
            return Err("--batch-size must be greater than zero".into());
        }
        if config.size == 0 {
            return Err("--size must be greater than zero".into());
        }
        Ok(config)
    }
}

fn required_value(
    args: &mut impl Iterator<Item = String>,
    name: &str,
) -> Result<String, Box<dyn Error>> {
    args.next()
        .ok_or_else(|| format!("{name} requires a value").into())
}

fn print_usage() {
    println!(
        "usage: cargo run -p hashtree-cli --example storage_write_bench -- \\
  --data-dir /tmp/bench --mode cached-batch --requests 128 --batch-size 32 --size 262144"
    );
}

fn deterministic_payload(seed: &str, index: usize, size: usize) -> Vec<u8> {
    let mut output = Vec::with_capacity(size);
    let mut counter = 0u64;
    while output.len() < size {
        let mut hasher = Sha256::new();
        hasher.update(seed.as_bytes());
        hasher.update((index as u64).to_le_bytes());
        hasher.update(counter.to_le_bytes());
        output.extend_from_slice(&hasher.finalize());
        counter += 1;
    }
    output.truncate(size);
    output
}

fn percentile_ms(sorted: &[Duration], percentile: usize) -> u128 {
    let index = ((sorted.len() - 1) * percentile).div_ceil(100);
    sorted[index].as_millis()
}
