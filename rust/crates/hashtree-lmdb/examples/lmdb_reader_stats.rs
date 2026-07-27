use hashtree_lmdb::{ExternalBlobOptions, LmdbBlobReader};
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().collect::<Vec<_>>();
    if !(args.len() == 2 || args.len() == 3) {
        return Err("usage: lmdb_reader_stats LMDB_DIR [EXTERNAL_DIR]".into());
    }
    let external = args.get(2).map(|path| ExternalBlobOptions {
        base_path: PathBuf::from(path),
        min_bytes: 1,
        sync: false,
        pack_target_bytes: None,
    });
    let reader = LmdbBlobReader::open(&args[1], external)?;
    let stats = reader.stats()?;
    let (blob_entries, metadata_entries) = reader.database_entry_counts()?;
    println!("counter_count={}", stats.count);
    println!("counter_total_bytes={}", stats.total_bytes);
    println!("blob_entries={blob_entries}");
    println!("metadata_entries={metadata_entries}");
    Ok(())
}
