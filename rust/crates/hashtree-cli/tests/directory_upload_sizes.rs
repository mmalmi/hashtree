use hashtree_cli::{AddProgress, HashtreeStore, UploadOptions};
use hashtree_config::StorageBackend;
use hashtree_core::Cid;
use std::path::Path;

fn check_files(store: &HashtreeStore, root: &Cid, source: &Path) {
    for name in ["empty", "small", "nested/chunked"] {
        let relative = Path::new(name);
        let parent = relative.parent().unwrap().to_str().unwrap();
        let directory = if parent.is_empty() {
            root.clone()
        } else {
            store.resolve_path(root, parent).unwrap().unwrap()
        };
        let listing = store
            .get_directory_listing_by_cid(&directory)
            .unwrap()
            .unwrap();
        let entry = listing
            .entries
            .iter()
            .find(|entry| entry.name == relative.file_name().unwrap().to_str().unwrap())
            .unwrap();
        let expected = std::fs::read(source.join(relative)).unwrap();
        assert_eq!(entry.size, expected.len() as u64, "stored size for {name}");
        let restored = tempfile::NamedTempFile::new().unwrap();
        let written = store
            .write_file_by_cid(&Cid::parse(&entry.cid).unwrap(), restored.path())
            .unwrap();
        assert_eq!(written, entry.size);
        assert_eq!(std::fs::read(restored.path()).unwrap(), expected);
    }
}

fn roundtrip(encrypted: bool, cached: bool) {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    std::fs::create_dir_all(source.join("nested")).unwrap();
    std::fs::write(source.join("empty"), []).unwrap();
    std::fs::write(source.join("small"), [1; 17]).unwrap();
    std::fs::write(source.join("nested/chunked"), [2; 257]).unwrap();
    let store =
        HashtreeStore::new_with_backend(temp.path().join("store"), StorageBackend::Fs, 0).unwrap();
    let upload = || {
        let progress = AddProgress::new();
        let root = if cached {
            #[cfg(unix)]
            {
                store.upload_dir_cached_with_progress(
                    &source,
                    false,
                    Some(64),
                    encrypted,
                    &progress,
                    &temp.path().join("cache/files.json"),
                )
            }
            #[cfg(not(unix))]
            unreachable!("cached uploads are Unix-only")
        } else {
            let options = if encrypted {
                UploadOptions::encrypted()
            } else {
                UploadOptions::public()
            };
            store.upload_dir_with(
                &source,
                options
                    .with_respect_gitignore(false)
                    .with_chunk_size(Some(64))
                    .with_progress(&progress),
            )
        }
        .unwrap();
        let root = Cid::parse(&root).unwrap();
        check_files(&store, &root, &source);
        (root, progress.snapshot().bytes_processed)
    };
    let first = upload();
    assert_eq!(first.1, 274);
    if cached {
        assert_eq!(upload(), (first.0, 0), "cache hit must retain file sizes");
        std::fs::write(source.join("small"), [3; 19]).unwrap();
        assert_eq!(upload().1, 19, "only the changed file needs rereading");
    }
}

#[test]
fn public_directory_preserves_file_sizes() {
    roundtrip(false, false);
}

#[test]
fn encrypted_directory_preserves_file_sizes() {
    roundtrip(true, false);
}

#[test]
#[cfg(unix)]
fn cached_public_directory_preserves_file_sizes() {
    roundtrip(false, true);
}

#[test]
#[cfg(unix)]
fn cached_encrypted_directory_preserves_file_sizes() {
    roundtrip(true, true);
}
