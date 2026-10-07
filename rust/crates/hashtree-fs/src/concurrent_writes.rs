use super::*;
use std::sync::{Arc, Barrier};

#[test]
fn independent_stores_concurrently_put_the_same_blob_without_losing_writes() {
    const WRITERS: usize = 8;
    const ROUNDS: usize = 16;
    let directory = tempfile::tempdir().unwrap();
    let blobs: Vec<_> = (0..ROUNDS)
        .map(|round| {
            let bytes = vec![round as u8; 256 * 1024];
            (hashtree_core::sha256(&bytes), bytes)
        })
        .collect();
    let barrier = Arc::new(Barrier::new(WRITERS));
    let errors = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..WRITERS)
            .map(|_| {
                let store = FsBlobStore::new(directory.path()).unwrap();
                let barrier = barrier.clone();
                let blobs = &blobs;
                scope.spawn(move || {
                    let mut errors = Vec::new();
                    for (round, (hash, bytes)) in blobs.iter().enumerate() {
                        barrier.wait();
                        if let Err(error) = store.put_sync(*hash, bytes) {
                            errors.push(format!("round {round}: {error}"));
                        }
                        barrier.wait();
                    }
                    errors
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|worker| worker.join().expect("concurrent writer"))
            .collect::<Vec<_>>()
    });
    assert!(
        errors.is_empty(),
        "concurrent writes failed {} times: {:?}",
        errors.len(),
        &errors[..errors.len().min(3)]
    );
    let reopened = FsBlobStore::new(directory.path()).unwrap();
    for (hash, expected) in &blobs {
        assert_eq!(reopened.get_sync(hash).unwrap().as_ref(), Some(expected));
        let parent = reopened.blob_path(hash).parent().unwrap().to_path_buf();
        assert!(
            fs::read_dir(parent).unwrap().all(|entry| {
                entry
                    .unwrap()
                    .path()
                    .extension()
                    .and_then(|part| part.to_str())
                    != Some("tmp")
            }),
            "successful concurrent writes must leave no temporary files"
        );
    }
}

#[test]
fn failed_atomic_blob_write_removes_only_its_own_temporary_file() {
    let directory = tempfile::tempdir().unwrap();
    let destination = directory.path().join("completed");
    fs::create_dir(&destination).unwrap();
    let completed = destination.join("existing-blob");
    fs::write(&completed, b"preserved completed data").unwrap();
    let other_writer = directory.path().join("other-writer.tmp");
    fs::write(&other_writer, b"another writer owns this").unwrap();

    assert!(write_blob_atomically(&destination, b"new data").is_err());
    assert_eq!(fs::read(completed).unwrap(), b"preserved completed data");
    assert_eq!(fs::read(other_writer).unwrap(), b"another writer owns this");
    assert_eq!(
        fs::read_dir(directory.path()).unwrap().count(),
        2,
        "the failed writer must remove its own temporary file"
    );
}
