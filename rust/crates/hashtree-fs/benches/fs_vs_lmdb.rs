//! Benchmark comparing FsBlobStore vs LmdbBlobStore performance
//!
//! Run with: cargo bench -p hashtree-fs --features lmdb

use hashtree_core::store::Store;
use hashtree_core::sha256;
use std::path::Path;
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn collect_repo_files(repo_path: &Path) -> Vec<(String, Vec<u8>)> {
    let mut files = Vec::new();

    fn visit_dir(dir: &Path, files: &mut Vec<(String, Vec<u8>)>) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let name = path.file_name().unwrap().to_string_lossy().to_string();

                // Skip .git, target, and other non-essential dirs
                if name == ".git" || name == "target" || name == ".github" {
                    continue;
                }

                if path.is_dir() {
                    visit_dir(&path, files);
                } else if path.is_file() {
                    if let Ok(data) = std::fs::read(&path) {
                        let rel_path = path.to_string_lossy().to_string();
                        files.push((rel_path, data));
                    }
                }
            }
        }
    }

    visit_dir(repo_path, &mut files);
    files
}

fn format_duration(d: Duration) -> String {
    if d.as_secs() > 0 {
        format!("{:.2}s", d.as_secs_f64())
    } else if d.as_millis() > 0 {
        format!("{}ms", d.as_millis())
    } else {
        format!("{}µs", d.as_micros())
    }
}

fn format_throughput(bytes: usize, duration: Duration) -> String {
    let mb = bytes as f64 / (1024.0 * 1024.0);
    let secs = duration.as_secs_f64();
    if secs > 0.0 {
        format!("{:.1} MB/s", mb / secs)
    } else {
        "∞".to_string()
    }
}

async fn benchmark_store<S: Store>(store: &S, files: &[(String, Vec<u8>)], name: &str) -> (Duration, Duration, usize) {
    let mut total_bytes = 0usize;
    let mut hashes = Vec::new();

    // Benchmark writes
    let write_start = Instant::now();
    for (_path, data) in files {
        let hash = sha256(data);
        hashes.push(hash);
        store.put(hash, data.clone()).await.unwrap();
        total_bytes += data.len();
    }
    let write_duration = write_start.elapsed();

    // Benchmark reads
    let read_start = Instant::now();
    for hash in &hashes {
        let _ = store.get(hash).await.unwrap();
    }
    let read_duration = read_start.elapsed();

    println!("\n{}:", name);
    println!("  Files: {}", files.len());
    println!("  Total size: {:.2} MB", total_bytes as f64 / (1024.0 * 1024.0));
    println!("  Write: {} ({})", format_duration(write_duration), format_throughput(total_bytes, write_duration));
    println!("  Read:  {} ({})", format_duration(read_duration), format_throughput(total_bytes, read_duration));

    (write_duration, read_duration, total_bytes)
}

#[tokio::main]
async fn main() {
    println!("=== Hashtree Storage Backend Benchmark ===\n");

    // Find repo root
    let repo_path = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap();
    println!("Benchmarking with: {}", repo_path.display());

    // Collect files
    let files = collect_repo_files(repo_path);
    println!("Collected {} files", files.len());

    // Create temp directories
    let fs_temp = TempDir::new().unwrap();
    let lmdb_temp = TempDir::new().unwrap();

    // Benchmark FsBlobStore
    let fs_store = hashtree_fs::FsBlobStore::new(fs_temp.path().join("blobs")).unwrap();
    let (fs_write, fs_read, total_bytes) = benchmark_store(&fs_store, &files, "FsBlobStore (filesystem)").await;

    // Benchmark LmdbBlobStore
    let lmdb_store = hashtree_lmdb::LmdbBlobStore::new(lmdb_temp.path().join("blobs")).unwrap();
    let (lmdb_write, lmdb_read, _) = benchmark_store(&lmdb_store, &files, "LmdbBlobStore").await;

    // Summary
    println!("\n=== Summary ===");
    println!("Write speedup: {:.2}x (FS {} vs LMDB {})",
        lmdb_write.as_secs_f64() / fs_write.as_secs_f64().max(0.001),
        format_duration(fs_write),
        format_duration(lmdb_write));
    println!("Read speedup:  {:.2}x (FS {} vs LMDB {})",
        lmdb_read.as_secs_f64() / fs_read.as_secs_f64().max(0.001),
        format_duration(fs_read),
        format_duration(lmdb_read));

    // Random access benchmark
    println!("\n=== Random Access Benchmark (1000 reads) ===");

    let hashes: Vec<_> = files.iter().map(|(_, data)| sha256(data)).collect();
    let iterations = 1000;

    // FS random reads
    let start = Instant::now();
    for i in 0..iterations {
        let hash = &hashes[i % hashes.len()];
        let _ = fs_store.get(hash).await.unwrap();
    }
    let fs_random = start.elapsed();

    // LMDB random reads
    let start = Instant::now();
    for i in 0..iterations {
        let hash = &hashes[i % hashes.len()];
        let _ = lmdb_store.get(hash).await.unwrap();
    }
    let lmdb_random = start.elapsed();

    println!("FS:   {} ({:.0} ops/sec)", format_duration(fs_random), iterations as f64 / fs_random.as_secs_f64());
    println!("LMDB: {} ({:.0} ops/sec)", format_duration(lmdb_random), iterations as f64 / lmdb_random.as_secs_f64());
    println!("Random read speedup: {:.2}x", lmdb_random.as_secs_f64() / fs_random.as_secs_f64().max(0.001));
}
