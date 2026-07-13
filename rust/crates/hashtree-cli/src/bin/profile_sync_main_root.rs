use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use hashtree_cli::config::{ensure_keys, parse_npub, pubkey_bytes};
use hashtree_cli::socialgraph::{self, SocialGraphBackend};
use hashtree_cli::{Config, HashtreeStore};
use hashtree_core::{nhash_decode, nhash_encode_full, Cid, NHashData};
use hashtree_nostr::{ListEventsOptions, NostrEventStore};
use nostr::Kind;

#[derive(Debug, Parser)]
struct Args {
    #[arg(long)]
    data_dir: PathBuf,
    #[arg(long)]
    root: Option<String>,
}

fn resolve_root_text(data_dir: &Path, explicit: Option<String>) -> Result<String> {
    if let Some(root) = explicit {
        return Ok(root);
    }

    for path in [
        data_dir.join("nostr-index").join("latest-root.txt"),
        data_dir.join("nostr-index").join("checkpoint-root.txt"),
    ] {
        if let Ok(raw) = fs::read_to_string(&path) {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                return Ok(trimmed.to_string());
            }
        }
    }

    anyhow::bail!("no root provided and no latest/checkpoint root file found")
}

fn cid_to_nhash(cid: &Cid) -> Result<String> {
    nhash_encode_full(&NHashData {
        hash: cid.hash,
        decrypt_key: cid.key,
    })
    .context("encode nhash root")
}

fn main() -> Result<()> {
    let args = Args::parse();
    let config = Config::load()?;

    let max_size_bytes = config.storage.max_size_gb * 1024 * 1024 * 1024;
    let store = Arc::new(HashtreeStore::with_options(
        &args.data_dir,
        config.storage.s3.as_ref(),
        max_size_bytes,
    )?);

    let nostr_db_max_bytes = config
        .nostr
        .db_max_size_gb
        .saturating_mul(1024 * 1024 * 1024);

    let graph_store = socialgraph::open_social_graph_store_with_storage(
        &args.data_dir,
        store.store_arc(),
        Some(nostr_db_max_bytes),
    )
    .context("open shared social graph store")?;
    graph_store.set_profile_index_overmute_threshold(config.nostr.overmute_threshold);

    let (keys, _) = ensure_keys()?;
    let self_pubkey = pubkey_bytes(&keys);
    let social_graph_root_bytes = if let Some(ref root_npub) = config.nostr.socialgraph_root {
        parse_npub(root_npub).unwrap_or(self_pubkey)
    } else {
        self_pubkey
    };
    socialgraph::set_social_graph_root(&graph_store, &social_graph_root_bytes);

    let root_text = resolve_root_text(&args.data_dir, args.root)?;
    let decoded_root = nhash_decode(&root_text).context("decode root nhash")?;
    let root = Cid {
        hash: decoded_root.hash,
        key: decoded_root.decrypt_key,
    };

    let event_store = NostrEventStore::new(store.store_arc());
    let stored = futures::executor::block_on(event_store.list_by_kind_lossy(
        Some(&root),
        Kind::Metadata.as_u16() as u32,
        ListEventsOptions::default(),
    ))
    .context("list stored metadata events from root")?;

    let events = stored
        .into_iter()
        .map(|event| event.to_nostr_sdk_event())
        .collect::<std::result::Result<Vec<_>, _>>()?;

    graph_store
        .sync_profile_index_for_events(&events)
        .context("sync profile index from stored metadata events")?;

    let profile_search_root = graph_store
        .profile_search_root()?
        .context("profile search root missing after sync")?;

    println!("metadata_events={}", events.len());
    println!(
        "profile_search_root_hash={}",
        hex::encode(profile_search_root.hash)
    );
    if let Some(key) = profile_search_root.key {
        println!("profile_search_root_key={}", hex::encode(key));
    }
    println!(
        "profile_search_root={}",
        cid_to_nhash(&profile_search_root)?
    );

    Ok(())
}
