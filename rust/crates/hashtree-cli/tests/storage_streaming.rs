use hashtree_cli::HashtreeStore;
use hashtree_core::{from_hex, is_tree_node, Cid};

#[test]
fn upload_and_download_streaming_roundtrip_preserves_bytes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = HashtreeStore::new(tmp.path().join("store")).expect("store");

    let mut source = Vec::with_capacity(2_750_000);
    for i in 0..2_750_000u32 {
        source.push((i % 251) as u8);
    }

    let source_path = tmp.path().join("source.bin");
    std::fs::write(&source_path, &source).expect("write source");

    let cid_str = store
        .upload_file_encrypted(&source_path)
        .expect("upload encrypted");
    let cid = Cid::parse(&cid_str).expect("parse cid");

    let out_path = tmp.path().join("restored.bin");
    let written = store
        .write_file_by_cid(&cid, &out_path)
        .expect("stream download");
    assert_eq!(written as usize, source.len());

    let restored = std::fs::read(&out_path).expect("read restored");
    assert_eq!(restored, source);
}

#[test]
fn upload_public_and_write_by_hash_streaming_roundtrip_preserves_bytes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = HashtreeStore::new(tmp.path().join("store")).expect("store");

    let source = vec![42u8; 1_600_000];
    let source_path = tmp.path().join("source-public.bin");
    std::fs::write(&source_path, &source).expect("write source");

    let hash_hex = store.upload_file(&source_path).expect("upload public");
    let hash = from_hex(&hash_hex).expect("hash");

    let out_path = tmp.path().join("restored-public.bin");
    let written = store.write_file(&hash, &out_path).expect("stream download");
    assert_eq!(written as usize, source.len());

    let restored = std::fs::read(&out_path).expect("read restored");
    assert_eq!(restored, source);
}

#[test]
fn upload_file_with_large_chunk_size_stores_single_blob() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = HashtreeStore::new(tmp.path().join("store")).expect("store");

    let source = vec![7u8; 3_000_000];
    let source_path = tmp.path().join("single-blob.bin");
    std::fs::write(&source_path, &source).expect("write source");

    let hash_hex = store
        .upload_file_with_chunk_size(&source_path, Some(8 * 1024 * 1024))
        .expect("upload public with large chunk size");
    let hash = from_hex(&hash_hex).expect("hash");
    let stored = store
        .get_chunk(&hash)
        .expect("chunk lookup")
        .expect("stored bytes");

    assert!(
        !is_tree_node(&stored),
        "expected direct blob storage when chunk size exceeds file size"
    );
    assert_eq!(stored, source);
}

#[test]
fn write_file_by_cid_errors_when_content_missing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = HashtreeStore::new(tmp.path().join("store")).expect("store");

    let missing_hash = [0x7fu8; 32];
    assert!(store
        .get_chunk(&missing_hash)
        .expect("chunk lookup")
        .is_none());
    let missing_cid = Cid::public(missing_hash);
    let out_path = tmp.path().join("missing.bin");

    let err = store
        .write_file_by_cid(&missing_cid, &out_path)
        .expect_err("missing cid should fail");
    assert!(
        err.to_string().contains("not found"),
        "expected not found error, got: {err}"
    );
    assert!(
        !out_path.exists(),
        "output file should not be created on missing cid"
    );
}

#[test]
fn encrypted_directory_with_underscore_file_roundtrips_by_cid() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = HashtreeStore::new(tmp.path().join("store")).expect("store");

    let site_dir = tmp.path().join("site");
    std::fs::create_dir_all(site_dir.join("assets")).expect("asset dir");
    std::fs::write(site_dir.join("_headers"), "cache-control: no-store\n").expect("_headers");
    std::fs::write(site_dir.join("index.html"), "<html></html>").expect("index");
    std::fs::write(site_dir.join("assets").join("asset.txt"), "asset").expect("asset");

    let cid_str = store
        .upload_dir_encrypted_with_options(&site_dir, true)
        .expect("upload encrypted dir");
    let cid = Cid::parse(&cid_str).expect("parse dir cid");

    let listing = store
        .get_directory_listing_by_cid(&cid)
        .expect("directory listing")
        .expect("encrypted root directory");
    let names: Vec<_> = listing
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    assert!(names.contains(&"_headers"));
    assert!(names.contains(&"assets"));
    assert!(names.contains(&"index.html"));

    let index_cid = store
        .resolve_path(&cid, "index.html")
        .expect("resolve path")
        .expect("resolved index");
    let out_path = tmp.path().join("restored-index.html");
    store
        .write_file_by_cid(&index_cid, &out_path)
        .expect("stream encrypted index");
    let restored = std::fs::read_to_string(&out_path).expect("read restored index");
    assert_eq!(restored, "<html></html>");
}
