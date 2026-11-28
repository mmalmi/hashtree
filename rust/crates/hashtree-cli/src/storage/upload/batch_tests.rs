use super::*;

#[test]
fn directory_upload_coalesces_tiny_writes_and_bounds_the_item_count() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let store = HashtreeStore::new_with_backend(temp.path(), StorageBackend::Fs, 0)?;
    let (batch, _tree) = store.buffered_upload_tree(&UploadOptions::public());
    let items: Vec<_> = (0u32..1024)
        .map(|i| {
            let data = i.to_le_bytes().to_vec();
            (sha256(&data), data)
        })
        .collect();
    let (first, data) = items[0].clone();
    sync_block_on(batch.put(first, data.clone()))?;
    sync_block_on(batch.flush_pending_if_needed())?;
    assert_eq!(sync_block_on(batch.get(&first))?, Some(data));
    assert!(
        !store.router().exists(&first)?,
        "one tiny write should stay buffered"
    );
    // Include the first item again to exercise deduplication in the shared batch.
    assert_eq!(sync_block_on(batch.put_many(items))?, 1023);
    assert!(
        store.router().exists(&first)?,
        "tiny writes exceeded the item limit"
    );
    Ok(())
}

#[test]
fn directory_upload_flushes_chunks_before_a_large_file_finishes() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let store = HashtreeStore::new_with_backend(temp.path(), StorageBackend::Fs, 0)?;
    let chunk_size = 64 * 1024;
    let mut bytes = vec![0; DIR_UPLOAD_WRITE_BATCH_BYTES + chunk_size];
    for (i, chunk) in bytes.chunks_mut(chunk_size).enumerate() {
        chunk[..8].copy_from_slice(&(i as u64).to_le_bytes());
    }
    let first = sha256(&bytes[..chunk_size]);
    let (_batch, tree) =
        store.buffered_upload_tree(&UploadOptions::public().with_chunk_size(Some(chunk_size)));
    let mut processed = 0;
    let mut persisted_before_eof = false;
    let (_, size) = sync_block_on(tree.put_stream_with_progress(bytes.as_slice(), |count| {
        processed += count;
        if processed == DIR_UPLOAD_WRITE_BATCH_BYTES as u64 {
            persisted_before_eof = store.router().exists(&first).unwrap();
        }
    }))?;
    assert_eq!(size, bytes.len() as u64);
    assert!(
        persisted_before_eof,
        "the first write batch remained in memory past its byte limit"
    );
    Ok(())
}

#[cfg(feature = "lmdb")]
#[test]
fn ordinary_directory_reimport_repairs_a_corrupt_pool_chunk() -> Result<()> {
    use hashtree_lmdb::{LmdbBlobStore, PoolMemberConfig, PoolStore, PoolStoreConfig};

    let temp = tempfile::tempdir()?;
    let data = temp.path().join("data");
    let source = temp.path().join("source");
    let member = temp.path().join("member");
    std::fs::create_dir(&source)?;
    let bytes = b"first chunk should survive a repeated directory import".repeat(40);
    std::fs::write(source.join("file"), &bytes)?;
    let chunk_size = 256;
    let first = sha256(&bytes[..chunk_size]);
    let map_size = 16 * 1024 * 1024;
    let pool = PoolStore::open(
        data.join(SHARED_BLOB_POOL_DIR_NAME),
        PoolStoreConfig::default(),
    )?;
    pool.add_member(PoolMemberConfig::new(member.clone(), map_size).with_map_size_bytes(map_size))?;
    drop(pool);

    let open = || HashtreeStore::new_with_backend(&data, StorageBackend::Lmdb, 0);
    let add = |store: &HashtreeStore| {
        store.upload_dir_with(
            &source,
            UploadOptions::public().with_chunk_size(Some(chunk_size)),
        )
    };
    let root = add(&open()?)?;
    // Damage only the member payload. The pool still has its committed catalog entry.
    let raw = LmdbBlobStore::with_exact_map_size_and_external_blob_options(
        &member,
        map_size as usize,
        None,
    )?;
    assert!(raw.delete_sync(&first)?);
    assert!(raw.put_sync(first, b"damaged member payload")?);
    drop(raw);

    let store = open()?;
    assert!(store.get_blob(&first).is_err());
    assert_eq!(add(&store)?, root);
    assert_eq!(
        store.get_blob(&first)?.as_deref(),
        Some(&bytes[..chunk_size])
    );
    let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());
    let file = sync_block_on(tree.resolve(&Cid::parse(&root)?, "file"))?.unwrap();
    assert_eq!(sync_block_on(tree.get(&file, None))?, Some(bytes));
    Ok(())
}
