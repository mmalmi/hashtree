use super::{
    run::format_cid_for_display,
    torrent_args::{NetworkArgs, SnapshotArgs, TorrentCommand, TorrentFormat},
};
use anyhow::{bail, Context, Result};
use hashtree_bittorrent::{Format, Metadata, Seed, Snapshot, Torrent, MAX_METADATA_BYTES};
use hashtree_cli::{HashtreeStore, PRIORITY_OTHER};
use hashtree_core::{from_hex, nhash_decode, Cid, HashTreeConfig};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

pub(crate) async fn run(data_dir: PathBuf, command: TorrentCommand) -> Result<()> {
    match command {
        TorrentCommand::Inspect { torrent } => {
            let metadata = read_metadata(&torrent)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "magnet": metadata.magnet()?, "piece_length": metadata.piece_length(), "files": metadata.files(),
                }))?
            );
        }
        TorrentCommand::Export { snapshot, output } => {
            let (metadata, _, _) = prepare(&data_dir, snapshot).await?;
            write_new(&output, &metadata.torrent_bytes()?)?;
            println!("{}", metadata.magnet()?);
        }
        TorrentCommand::Seed {
            snapshot,
            output,
            network,
        } => {
            let options = network.options();
            options.validate()?;
            let (metadata, store, cid) = prepare(&data_dir, snapshot).await?;
            if let Some(output) = output {
                write_new(&output, &metadata.torrent_bytes()?)?;
            }
            // Retain the verified snapshot across restarts. Existing pins are
            // preserved; stopping a seed never unpins user content.
            pin(&store, &cid)?;
            let seed = Seed::start(metadata, options).await?;
            let ready = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let stats = seed.stats().await?;
                    if stats.port != 0 {
                        return Ok::<_, hashtree_bittorrent::Error>(stats);
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            })
            .await
            .context("torrent listener did not become ready")??;
            println!("{}", seed.metadata().magnet()?);
            eprintln!(
                "Seeding on port {}. Snapshot pinned. Press Ctrl-C to stop.",
                ready.port
            );
            let mut interval = tokio::time::interval(Duration::from_secs(2));
            loop {
                tokio::select! {
                    signal = tokio::signal::ctrl_c() => { signal?; break; }
                    _ = interval.tick() => { seed.stats().await?; }
                }
            }
        }
        TorrentCommand::Get {
            source,
            file,
            offset,
            length,
            output,
            network,
        } => {
            let torrent = open(&source, network).await?;
            let index = file_index(&torrent, &file)?;
            let size = torrent.metadata().file(index)?.size;
            let length = length.unwrap_or_else(|| size.saturating_sub(offset));
            if offset > size || length > size - offset {
                bail!("range exceeds file size");
            }
            if let Some(output) = output {
                let parent = output
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new("."));
                let mut temp = tempfile::NamedTempFile::new_in(parent)?;
                copy(&torrent, index, offset, length, temp.as_file_mut()).await?;
                temp.as_file().sync_all()?;
                temp.persist_noclobber(&output)
                    .map_err(|e| e.error)
                    .context("output already exists or could not be saved")?;
            } else {
                let mut stdout = std::io::stdout().lock();
                copy(&torrent, index, offset, length, &mut stdout).await?;
                stdout.flush()?;
            }
        }
        TorrentCommand::Import {
            source,
            file,
            unencrypted,
            network,
        } => {
            let torrent = open(&source, network).await?;
            let index = file_index(&torrent, &file)?;
            let store = Arc::new(HashtreeStore::new(&data_dir)?);
            let mut config = HashTreeConfig::new(store.store_arc());
            if unencrypted {
                config = config.public();
            }
            let (cid, size) = torrent.materialize(index, config).await?;
            pin(&store, &cid)?;
            println!("{}", format_cid_for_display(&cid));
            eprintln!("Imported and pinned {size} bytes.");
        }
    }
    Ok(())
}

fn read_metadata(path: &Path) -> Result<Metadata> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_METADATA_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    Ok(Metadata::parse(&bytes)?)
}
fn immutable_cid(input: &str) -> Result<Cid> {
    let input = input.strip_prefix("htree://").unwrap_or(input);
    if input.starts_with("nhash1") {
        let data = nhash_decode(input)?;
        Ok(Cid {
            hash: data.hash,
            key: data.decrypt_key,
        })
    } else {
        Ok(Cid {
            hash: from_hex(input).context("use an immutable nhash or 64-character hash")?,
            key: None,
        })
    }
}
async fn prepare(
    data_dir: &Path,
    args: SnapshotArgs,
) -> Result<(Metadata, Arc<HashtreeStore>, Cid)> {
    let cid = immutable_cid(&args.cid)?;
    let store = Arc::new(HashtreeStore::new(data_dir)?);
    let snapshot = Snapshot::new(
        store.store_arc(),
        cid.clone(),
        &args.name,
        args.allow_plaintext,
    )
    .await?;
    let format = match args.format {
        TorrentFormat::V1 => Format::V1,
        TorrentFormat::V2 => Format::V2,
        TorrentFormat::Hybrid => Format::Hybrid,
    };
    let metadata = Metadata::create(snapshot, format, args.piece_size).await?;
    Ok((metadata, store, cid))
}
fn pin(store: &HashtreeStore, cid: &Cid) -> Result<()> {
    if store.is_pinned(&cid.hash)? {
        return Ok(());
    }
    let existing = store.get_tree_meta(&cid.hash)?;
    store.pin_and_index_tree(
        cid,
        existing.as_ref().map_or("local", |m| &m.owner),
        existing.as_ref().and_then(|m| m.name.as_deref()),
        existing.as_ref().map_or(PRIORITY_OTHER, |m| m.priority),
        store.tree_index_limits(),
    )?;
    Ok(())
}
fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist_noclobber(path)
        .map_err(|e| e.error)
        .context("output already exists or could not be saved")?;
    Ok(())
}
async fn open(source: &str, network: NetworkArgs) -> Result<Torrent> {
    let options = network.options();
    if source.starts_with("magnet:?") {
        Ok(Torrent::magnet(source, options).await?)
    } else {
        Ok(Torrent::open(read_metadata(Path::new(source))?, options).await?)
    }
}
fn file_index(torrent: &Torrent, input: &str) -> Result<usize> {
    if let Some(file) = torrent.metadata().files().iter().find(|f| f.path == input) {
        return Ok(file.index);
    }
    let index: usize = input
        .parse()
        .context("file must be an exact torrent path or index")?;
    torrent.metadata().file(index)?;
    Ok(index)
}
async fn copy(
    torrent: &Torrent,
    file: usize,
    mut offset: u64,
    mut length: u64,
    out: &mut impl Write,
) -> Result<()> {
    while length > 0 {
        let len = length.min(2 * 1024 * 1024) as usize;
        let bytes = torrent.read_range(file, offset, len).await?;
        out.write_all(&bytes)?;
        offset += len as u64;
        length -= len as u64;
    }
    Ok(())
}
