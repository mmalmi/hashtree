//! TreeBuilder benchmark for different chunk sizes and encryption modes.
//!
//! Run with: cargo bench -p hashtree --features encryption

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use hashtree::{
    HashTree, HashTreeConfig, MemoryStore,
    BEP52_CHUNK_SIZE, DEFAULT_CHUNK_SIZE,
};
use std::sync::Arc;

/// Generate random data
fn random_data(size: usize) -> Vec<u8> {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..size).map(|_| rng.gen()).collect()
}

/// Benchmark HashTree with different chunk sizes
fn bench_tree_builder(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("tree_builder");

    let sizes = [
        (1, "1MB"),
        (10, "10MB"),
        (50, "50MB"),
    ];

    let chunk_sizes = [
        ("256KB", DEFAULT_CHUNK_SIZE),
        ("16KB", BEP52_CHUNK_SIZE),
    ];

    for (size_mb, size_name) in sizes {
        let size = size_mb * 1024 * 1024;
        let data = random_data(size);
        group.throughput(Throughput::Bytes(size as u64));

        for (chunk_name, chunk_size) in &chunk_sizes {
            group.bench_with_input(
                BenchmarkId::new(*chunk_name, size_name),
                &data,
                |b, data| {
                    b.iter(|| {
                        rt.block_on(async {
                            let store = Arc::new(MemoryStore::new());
                            let config = HashTreeConfig::new(store)
                                .with_chunk_size(*chunk_size)
                                .public();
                            let tree = HashTree::new(config);
                            tree.put(black_box(data)).await.unwrap()
                        })
                    })
                },
            );
        }
    }

    group.finish();
}

/// Benchmark HashTree read performance
fn bench_tree_reader(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("tree_reader");

    let sizes = [
        (1, "1MB"),
        (10, "10MB"),
    ];

    let chunk_sizes = [
        ("256KB", DEFAULT_CHUNK_SIZE),
        ("16KB", BEP52_CHUNK_SIZE),
    ];

    for (size_mb, size_name) in sizes {
        let size = size_mb * 1024 * 1024;
        let data = random_data(size);
        group.throughput(Throughput::Bytes(size as u64));

        for (chunk_name, chunk_size) in &chunk_sizes {
            // Pre-build the tree
            let (tree, cid) = rt.block_on(async {
                let store = Arc::new(MemoryStore::new());
                let config = HashTreeConfig::new(store)
                    .with_chunk_size(*chunk_size)
                    .public();
                let tree = HashTree::new(config);
                let cid = tree.put(&data).await.unwrap();
                (tree, cid)
            });

            group.bench_with_input(
                BenchmarkId::new(*chunk_name, size_name),
                &(tree, cid),
                |b, (tree, cid)| {
                    b.iter(|| {
                        rt.block_on(async {
                            tree.get(black_box(cid)).await.unwrap()
                        })
                    })
                },
            );
        }
    }

    group.finish();
}

/// Benchmark write+read roundtrip
fn bench_roundtrip(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("roundtrip");

    let size = 10 * 1024 * 1024; // 10 MB
    let data = random_data(size);
    group.throughput(Throughput::Bytes(size as u64));

    for (name, chunk_size) in [("256KB", DEFAULT_CHUNK_SIZE), ("16KB", BEP52_CHUNK_SIZE)] {
        group.bench_with_input(
            BenchmarkId::new(name, "10MB"),
            &data,
            |b, data| {
                b.iter(|| {
                    rt.block_on(async {
                        let store = Arc::new(MemoryStore::new());
                        let config = HashTreeConfig::new(store)
                            .with_chunk_size(chunk_size)
                            .public();
                        let tree = HashTree::new(config);
                        let cid = tree.put(black_box(data)).await.unwrap();
                        tree.get(&cid).await.unwrap()
                    })
                })
            },
        );
    }

    group.finish();
}

/// Benchmark encrypted vs non-encrypted write performance
#[cfg(feature = "encryption")]
fn bench_encrypted_write(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("encrypted_write");

    let sizes = [
        (1, "1MB"),
        (10, "10MB"),
    ];

    for (size_mb, size_name) in sizes {
        let size = size_mb * 1024 * 1024;
        let data = random_data(size);
        group.throughput(Throughput::Bytes(size as u64));

        // Non-encrypted
        group.bench_with_input(
            BenchmarkId::new("plain", size_name),
            &data,
            |b, data| {
                b.iter(|| {
                    rt.block_on(async {
                        let store = Arc::new(MemoryStore::new());
                        let config = HashTreeConfig::new(store).public();
                        let tree = HashTree::new(config);
                        tree.put(black_box(data)).await.unwrap()
                    })
                })
            },
        );

        // Encrypted (default)
        group.bench_with_input(
            BenchmarkId::new("encrypted", size_name),
            &data,
            |b, data| {
                b.iter(|| {
                    rt.block_on(async {
                        let store = Arc::new(MemoryStore::new());
                        let config = HashTreeConfig::new(store); // encrypted by default
                        let tree = HashTree::new(config);
                        tree.put(black_box(data)).await.unwrap()
                    })
                })
            },
        );
    }

    group.finish();
}

/// Benchmark encrypted vs non-encrypted read performance
#[cfg(feature = "encryption")]
fn bench_encrypted_read(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("encrypted_read");

    let sizes = [
        (1, "1MB"),
        (10, "10MB"),
    ];

    for (size_mb, size_name) in sizes {
        let size = size_mb * 1024 * 1024;
        let data = random_data(size);
        group.throughput(Throughput::Bytes(size as u64));

        // Non-encrypted - pre-build
        let (plain_tree, plain_cid) = rt.block_on(async {
            let store = Arc::new(MemoryStore::new());
            let config = HashTreeConfig::new(store).public();
            let tree = HashTree::new(config);
            let cid = tree.put(&data).await.unwrap();
            (tree, cid)
        });

        group.bench_with_input(
            BenchmarkId::new("plain", size_name),
            &(plain_tree, plain_cid),
            |b, (tree, cid)| {
                b.iter(|| {
                    rt.block_on(async {
                        tree.get(black_box(cid)).await.unwrap()
                    })
                })
            },
        );

        // Encrypted - pre-build
        let (enc_tree, enc_cid) = rt.block_on(async {
            let store = Arc::new(MemoryStore::new());
            let config = HashTreeConfig::new(store); // encrypted by default
            let tree = HashTree::new(config);
            let cid = tree.put(&data).await.unwrap();
            (tree, cid)
        });

        group.bench_with_input(
            BenchmarkId::new("encrypted", size_name),
            &(enc_tree, enc_cid),
            |b, (tree, cid)| {
                b.iter(|| {
                    rt.block_on(async {
                        tree.get(black_box(cid)).await.unwrap()
                    })
                })
            },
        );
    }

    group.finish();
}

#[cfg(feature = "encryption")]
criterion_group!(
    benches,
    bench_tree_builder,
    bench_tree_reader,
    bench_roundtrip,
    bench_encrypted_write,
    bench_encrypted_read,
);

#[cfg(not(feature = "encryption"))]
criterion_group!(
    benches,
    bench_tree_builder,
    bench_tree_reader,
    bench_roundtrip,
);

criterion_main!(benches);
