//! Benchmark git storage operations with FS vs LMDB backends
//!
//! Run with: cargo bench -p git-remote-htree

use git_remote_htree::git::storage::GitStorage;
use git_remote_htree::git::object::ObjectType;
use std::path::Path;
use std::time::Instant;
use tempfile::TempDir;

fn format_duration(ms: u128) -> String {
    if ms >= 1000 {
        format!("{:.2}s", ms as f64 / 1000.0)
    } else {
        format!("{}ms", ms)
    }
}

fn collect_source_files(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut files = Vec::new();

    fn visit(dir: &Path, files: &mut Vec<(String, Vec<u8>)>, base: &Path) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let name = path.file_name().unwrap().to_string_lossy().to_string();

                if name == ".git" || name == "target" || name.starts_with('.') {
                    continue;
                }

                if path.is_dir() {
                    visit(&path, files, base);
                } else if path.is_file() {
                    if let Ok(data) = std::fs::read(&path) {
                        let rel = path.strip_prefix(base).unwrap_or(&path);
                        files.push((rel.to_string_lossy().to_string(), data));
                    }
                }
            }
        }
    }

    visit(dir, &mut files, dir);
    files
}

fn run_benchmark(backend: &str, files: &[(String, Vec<u8>)]) -> (u128, u128, u128, u64) {
    let temp = TempDir::new().unwrap();
    let storage_dir = temp.path().join("storage");
    std::fs::create_dir_all(&storage_dir).unwrap();

    // Set up config for this backend
    let config_dir = temp.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();

    let config_content = format!(
        r#"[storage]
backend = "{}"
"#,
        backend
    );
    std::fs::write(config_dir.join("config.toml"), config_content).unwrap();
    std::env::set_var("HTREE_CONFIG_DIR", &config_dir);

    // Benchmark: Open storage
    let start = Instant::now();
    let storage = GitStorage::open(&storage_dir).unwrap();
    let open_time = start.elapsed().as_millis();

    // Benchmark: Write all files as blob objects
    let start = Instant::now();
    let mut oids = Vec::new();
    for (_path, data) in files {
        let oid = storage.write_raw_object(ObjectType::Blob, data).unwrap();
        oids.push(oid);
    }

    // Build the merkle tree
    let _root = storage.build_tree().unwrap();
    let write_time = start.elapsed().as_millis();

    // Benchmark: Read all objects back from blob store
    let start = Instant::now();
    let store = storage.store();
    let hashes = store.list().unwrap();
    for hash in &hashes {
        let _ = store.get_sync(hash);
    }
    let read_time = start.elapsed().as_millis();

    // Get storage size
    let storage_size = dir_size(&storage_dir);

    (open_time, write_time, read_time, storage_size)
}

fn dir_size(path: &Path) -> u64 {
    let mut size = 0;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                size += dir_size(&p);
            } else {
                size += entry.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    size
}

fn main() {
    println!("=== Git Storage Backend Benchmark ===");

    // Collect source files from hashtree-rs
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();

    println!("Source: {}", repo_root.display());

    let files = collect_source_files(repo_root);
    let total_size: usize = files.iter().map(|(_, d)| d.len()).sum();

    println!("Files: {}", files.len());
    println!("Total: {:.2} MB\n", total_size as f64 / (1024.0 * 1024.0));

    // Run FS benchmark
    println!("--- Filesystem Backend ---");
    let (fs_open, fs_write, fs_read, fs_size) = run_benchmark("fs", &files);
    println!("  Open:  {}", format_duration(fs_open));
    println!("  Write: {} ({:.1} MB/s)",
        format_duration(fs_write),
        total_size as f64 / (1024.0 * 1024.0) / (fs_write.max(1) as f64 / 1000.0));
    println!("  Read:  {} ({:.1} MB/s)",
        format_duration(fs_read),
        total_size as f64 / (1024.0 * 1024.0) / (fs_read.max(1) as f64 / 1000.0));
    println!("  Size:  {:.2} MB", fs_size as f64 / (1024.0 * 1024.0));

    // Run LMDB benchmark
    println!("\n--- LMDB Backend ---");
    let (lmdb_open, lmdb_write, lmdb_read, lmdb_size) = run_benchmark("lmdb", &files);
    println!("  Open:  {}", format_duration(lmdb_open));
    println!("  Write: {} ({:.1} MB/s)",
        format_duration(lmdb_write),
        total_size as f64 / (1024.0 * 1024.0) / (lmdb_write.max(1) as f64 / 1000.0));
    println!("  Read:  {} ({:.1} MB/s)",
        format_duration(lmdb_read),
        total_size as f64 / (1024.0 * 1024.0) / (lmdb_read.max(1) as f64 / 1000.0));
    println!("  Size:  {:.2} MB", lmdb_size as f64 / (1024.0 * 1024.0));

    // Summary
    println!("\n=== Summary ===");
    println!("Write: FS {} vs LMDB {} ({:.2}x)",
        format_duration(fs_write),
        format_duration(lmdb_write),
        lmdb_write as f64 / fs_write.max(1) as f64);
    println!("Read:  FS {} vs LMDB {} ({:.2}x)",
        format_duration(fs_read),
        format_duration(lmdb_read),
        lmdb_read as f64 / fs_read.max(1) as f64);
    println!("Size:  FS {:.2} MB vs LMDB {:.2} MB",
        fs_size as f64 / (1024.0 * 1024.0),
        lmdb_size as f64 / (1024.0 * 1024.0));
}
