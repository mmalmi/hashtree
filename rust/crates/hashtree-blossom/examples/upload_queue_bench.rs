use hashtree_blossom::{BatchUploadItem, BlossomClient};
use nostr::Keys;
use sha2::{Digest, Sha256};
use std::error::Error;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

#[derive(Clone, Debug)]
struct Config {
    server: String,
    requests: usize,
    batch_size: usize,
    concurrency: usize,
    size: usize,
    seed: String,
    timeout_secs: u64,
    mode: Mode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Raw,
    Read,
    Batch,
    BatchJson,
    BatchBinary,
}

#[derive(Debug)]
struct UploadSample {
    elapsed: Duration,
    blobs: usize,
    error: Option<String>,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let config = Config::parse()?;
    let client = Arc::new(
        BlossomClient::new_empty(Keys::generate())
            .with_servers(vec![config.server.clone()])
            .with_timeout(Duration::from_secs(config.timeout_secs)),
    );
    let semaphore = Arc::new(Semaphore::new(config.concurrency));
    let started = Instant::now();
    let mut handles = Vec::new();

    match config.mode {
        Mode::Raw => {
            handles.reserve(config.requests);
            for index in 0..config.requests {
                let permit = semaphore.clone().acquire_owned().await?;
                let client = client.clone();
                let config = config.clone();
                handles.push(tokio::spawn(async move {
                    let _permit = permit;
                    let data = deterministic_payload(&config.seed, index, config.size);
                    let started = Instant::now();
                    let result = client.upload(&data).await;
                    UploadSample {
                        elapsed: started.elapsed(),
                        blobs: 1,
                        error: result.err().map(|error| error.to_string()),
                    }
                }));
            }
        }
        Mode::Read => {
            handles.reserve(config.requests);
            for index in 0..config.requests {
                let permit = semaphore.clone().acquire_owned().await?;
                let client = client.clone();
                let config = config.clone();
                handles.push(tokio::spawn(async move {
                    let _permit = permit;
                    let data = deterministic_payload(&config.seed, index, config.size);
                    let hash = hex::encode(Sha256::digest(&data));
                    let started = Instant::now();
                    let result = client.download(&hash).await.and_then(|downloaded| {
                        if downloaded.len() == config.size {
                            Ok(downloaded)
                        } else {
                            Err(hashtree_blossom::BlossomError::DownloadFailed(format!(
                                "downloaded {} bytes, expected {}",
                                downloaded.len(),
                                config.size
                            )))
                        }
                    });
                    UploadSample {
                        elapsed: started.elapsed(),
                        blobs: 1,
                        error: result.err().map(|error| error.to_string()),
                    }
                }));
            }
        }
        Mode::Batch | Mode::BatchJson | Mode::BatchBinary => {
            handles.reserve(config.requests.div_ceil(config.batch_size));
            let mut index = 0usize;
            while index < config.requests {
                let end = (index + config.batch_size).min(config.requests);
                let permit = semaphore.clone().acquire_owned().await?;
                let client = client.clone();
                let config = config.clone();
                handles.push(tokio::spawn(async move {
                    let _permit = permit;
                    let mut items = Vec::with_capacity(end - index);
                    for item_index in index..end {
                        let data = deterministic_payload(&config.seed, item_index, config.size);
                        let hash = hex::encode(Sha256::digest(&data));
                        items.push(BatchUploadItem::new(hash, data));
                    }
                    let started = Instant::now();
                    let result = match config.mode {
                        Mode::Batch => client.upload_batch_to_server(&config.server, &items).await,
                        Mode::BatchJson => {
                            client
                                .upload_json_batch_to_server(&config.server, &items)
                                .await
                        }
                        Mode::BatchBinary => {
                            client
                                .upload_binary_batch_to_server(&config.server, &items)
                                .await
                        }
                        Mode::Raw => unreachable!("raw mode handled separately"),
                        Mode::Read => unreachable!("read mode handled separately"),
                    };
                    UploadSample {
                        elapsed: started.elapsed(),
                        blobs: items.len(),
                        error: match result {
                            Ok(Some(_)) => None,
                            Ok(None) => Some("batch upload unsupported".to_string()),
                            Err(error) => Some(error.to_string()),
                        },
                    }
                }));
                index = end;
            }
        }
    }

    let mut samples = Vec::with_capacity(handles.len());
    for handle in handles {
        samples.push(handle.await?);
    }

    let wall = started.elapsed();
    let successes: usize = samples
        .iter()
        .filter(|sample| sample.error.is_none())
        .map(|sample| sample.blobs)
        .sum();
    let failures: usize = samples
        .iter()
        .filter(|sample| sample.error.is_some())
        .map(|sample| sample.blobs)
        .sum();
    let total_mib = successes as f64 * config.size as f64 / 1024.0 / 1024.0;
    let throughput_mib_s = if wall.as_secs_f64() > 0.0 {
        total_mib / wall.as_secs_f64()
    } else {
        0.0
    };

    let mut latencies: Vec<_> = samples
        .iter()
        .filter(|sample| sample.error.is_none())
        .map(|sample| sample.elapsed)
        .collect();
    latencies.sort_unstable();

    println!("server={}", config.server);
    println!(
        "mode={:?} requests={} batch_size={} concurrency={} size={} timeout_secs={} seed={}",
        config.mode,
        config.requests,
        config.batch_size,
        config.concurrency,
        config.size,
        config.timeout_secs,
        config.seed
    );
    println!(
        "success={} failed={} wall_ms={} throughput_mib_s={:.2}",
        successes,
        failures,
        wall.as_millis(),
        throughput_mib_s
    );
    if !latencies.is_empty() {
        println!(
            "latency_ms p50={} p95={} p99={} max={}",
            percentile_ms(&latencies, 50),
            percentile_ms(&latencies, 95),
            percentile_ms(&latencies, 99),
            latencies.last().unwrap().as_millis()
        );
    }

    for error in samples
        .iter()
        .filter_map(|sample| sample.error.as_deref())
        .take(5)
    {
        eprintln!("error: {error}");
    }

    if failures > 0 {
        std::process::exit(1);
    }
    Ok(())
}

impl Config {
    fn parse() -> Result<Self, Box<dyn Error>> {
        let mut config = Self {
            server: "http://127.0.0.1:8080".to_string(),
            requests: 128,
            batch_size: 32,
            concurrency: 32,
            size: 256 * 1024,
            seed: "upload-queue-bench".to_string(),
            timeout_secs: 120,
            mode: Mode::Raw,
        };

        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--server" => config.server = required_value(&mut args, "--server")?,
                "--requests" => {
                    config.requests = required_value(&mut args, "--requests")?.parse()?
                }
                "--batch-size" => {
                    config.batch_size = required_value(&mut args, "--batch-size")?.parse()?
                }
                "--concurrency" => {
                    config.concurrency = required_value(&mut args, "--concurrency")?.parse()?
                }
                "--size" => config.size = required_value(&mut args, "--size")?.parse()?,
                "--seed" => config.seed = required_value(&mut args, "--seed")?,
                "--timeout-secs" => {
                    config.timeout_secs = required_value(&mut args, "--timeout-secs")?.parse()?
                }
                "--mode" => {
                    config.mode = match required_value(&mut args, "--mode")?.as_str() {
                        "raw" => Mode::Raw,
                        "read" => Mode::Read,
                        "batch" => Mode::Batch,
                        "batch-json" => Mode::BatchJson,
                        "batch-binary" => Mode::BatchBinary,
                        other => return Err(format!("unknown mode: {other}").into()),
                    }
                }
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
        if config.concurrency == 0 {
            return Err("--concurrency must be greater than zero".into());
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
        "usage: cargo run -p hashtree-blossom --example upload_queue_bench -- \\
  --server http://127.0.0.1:8080 --mode raw|read|batch|batch-json|batch-binary --requests 128 --concurrency 32 --size 262144"
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
