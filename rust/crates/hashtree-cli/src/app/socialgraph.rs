use anyhow::{Context, Result};
use hashtree_cli::blossom_push::background_blossom_push;
use hashtree_cli::{Config, HashtreeStore, NostrKeys, NostrResolverConfig, NostrRootResolver};
use hashtree_core::{nhash_encode_full, Cid, NHashData};
use nostr::nips::nip19::ToBech32;
use nostr::PublicKey;
use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hashtree_cli::config::{ensure_keys, ensure_keys_string};
use hashtree_cli::socialgraph::{
    sync_local_list_files_force, SocialGraphBackend, SocialGraphCrawler,
};
use hashtree_cli::RootResolver;

const SOCIALGRAPH_SYNC_CURSOR_FILE: &str = "socialgraph-last-sync.txt";
const PUBLISHED_EVENT_TREE_NAME: &str = "nostr-event-index";
const PUBLISHED_PROFILE_SEARCH_TREE_NAME: &str = "profile-search";
const PUBLISHED_PROFILES_BY_PUBKEY_TREE_NAME: &str = "profiles-by-pubkey";
const MIRROR_PUBLISH_RELAY_PRIORITY: &[&str] = &[
    "wss://temp.iris.to",
    "wss://vault.iris.to",
    "wss://relay.damus.io",
    "wss://relay.primal.net",
];
const MIRROR_PUBLISH_RELAY_BLOCKLIST: &[&str] =
    &["wss://graph-relay.iris.to", "wss://upload.iris.to/nostr"];

fn parse_pubkey_hex(hex_str: &str) -> Option<[u8; 32]> {
    if hex_str.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    if hex::decode_to_slice(hex_str, &mut bytes).is_err() {
        return None;
    }
    Some(bytes)
}

fn init_socialgraph(
    data_dir: &Path,
    config: &Config,
) -> Result<(Arc<hashtree_cli::socialgraph::SocialGraphStore>, [u8; 32])> {
    use hashtree_cli::config::{ensure_keys, parse_npub, pubkey_bytes};

    let nostr_db_max_bytes = config
        .nostr
        .db_max_size_gb
        .saturating_mul(1024 * 1024 * 1024);

    let (keys, _was_generated) = ensure_keys()?;
    let pk_bytes = pubkey_bytes(&keys);

    let social_graph_root_bytes = if let Some(ref root_npub) = config.nostr.socialgraph_root {
        match parse_npub(root_npub) {
            Ok(pk) => pk,
            Err(_) => {
                tracing::warn!("Invalid npub in socialgraph_root: {}", root_npub);
                pk_bytes
            }
        }
    } else {
        pk_bytes
    };

    let graph_store = hashtree_cli::socialgraph::open_social_graph_store_with_mapsize(
        data_dir,
        Some(nostr_db_max_bytes),
    )
    .context("Failed to initialize social graph store")?;
    graph_store.set_profile_index_overmute_threshold(config.nostr.overmute_threshold);
    hashtree_cli::socialgraph::set_social_graph_root(&graph_store, &social_graph_root_bytes);
    sync_local_list_files_force(graph_store.as_ref(), data_dir, &keys)
        .context("Failed to sync local social graph lists")?;

    Ok((graph_store, social_graph_root_bytes))
}

fn init_socialgraph_with_shared_storage(
    data_dir: &Path,
    config: &Config,
) -> Result<(Arc<hashtree_cli::socialgraph::SocialGraphStore>, [u8; 32])> {
    use hashtree_cli::config::{ensure_keys, parse_npub, pubkey_bytes};

    let nostr_db_max_bytes = config
        .nostr
        .db_max_size_gb
        .saturating_mul(1024 * 1024 * 1024);
    let max_size_bytes = config.storage.max_size_gb * 1024 * 1024 * 1024;

    let (keys, _was_generated) = ensure_keys()?;
    let pk_bytes = pubkey_bytes(&keys);

    let social_graph_root_bytes = if let Some(ref root_npub) = config.nostr.socialgraph_root {
        match parse_npub(root_npub) {
            Ok(pk) => pk,
            Err(_) => {
                tracing::warn!("Invalid npub in socialgraph_root: {}", root_npub);
                pk_bytes
            }
        }
    } else {
        pk_bytes
    };

    let store = Arc::new(HashtreeStore::with_options(
        data_dir,
        config.storage.s3.as_ref(),
        max_size_bytes,
    )?);
    let graph_store = hashtree_cli::socialgraph::open_social_graph_store_with_storage(
        data_dir,
        store.store_arc(),
        Some(nostr_db_max_bytes),
    )
    .context("Failed to initialize shared social graph store")?;
    graph_store.set_profile_index_overmute_threshold(config.nostr.overmute_threshold);
    hashtree_cli::socialgraph::set_social_graph_root(&graph_store, &social_graph_root_bytes);
    sync_local_list_files_force(graph_store.as_ref(), data_dir, &keys)
        .context("Failed to sync local social graph lists")?;

    Ok((graph_store, social_graph_root_bytes))
}

fn sync_cursor_path(data_dir: &Path) -> PathBuf {
    data_dir.join(SOCIALGRAPH_SYNC_CURSOR_FILE)
}

fn load_sync_cursor(data_dir: &Path) -> Result<Option<u64>> {
    let path = sync_cursor_path(data_dir);
    if !path.exists() {
        return Ok(None);
    }

    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    trimmed
        .parse::<u64>()
        .map(Some)
        .with_context(|| format!("Failed to parse {}", path.display()))
}

fn save_sync_cursor(data_dir: &Path, cursor: u64) -> Result<()> {
    let path = sync_cursor_path(data_dir);
    std::fs::write(&path, format!("{cursor}\n"))
        .with_context(|| format!("Failed to write {}", path.display()))
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn format_socialgraph_root(root: Option<&str>) -> String {
    let Some(root) = root else {
        return "unknown".to_string();
    };

    PublicKey::from_hex(root)
        .ok()
        .and_then(|pubkey| pubkey.to_bech32().ok())
        .unwrap_or_else(|| root.to_string())
}

fn mirror_publish_relays(active_relays: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let active_relays = active_relays
        .iter()
        .filter(|relay| seen.insert((*relay).clone()))
        .cloned()
        .collect::<Vec<_>>();
    if active_relays.is_empty() {
        return Vec::new();
    }

    let active_relay_set = active_relays.iter().cloned().collect::<HashSet<_>>();
    let preferred = MIRROR_PUBLISH_RELAY_PRIORITY
        .iter()
        .filter(|relay| active_relay_set.contains(**relay))
        .map(|relay| (*relay).to_string())
        .collect::<Vec<_>>();
    if !preferred.is_empty() {
        return preferred;
    }

    let filtered = active_relays
        .iter()
        .filter(|relay| !MIRROR_PUBLISH_RELAY_BLOCKLIST.contains(&relay.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if !filtered.is_empty() {
        return filtered;
    }

    active_relays
}

async fn publish_socialgraph_roots(
    data_dir: &Path,
    config: &Config,
    roots: &[(&str, Option<Cid>)],
) -> Result<()> {
    let roots = roots
        .iter()
        .filter_map(|(tree_name, root)| root.clone().map(|cid| ((*tree_name).to_string(), cid)))
        .collect::<Vec<_>>();
    if roots.is_empty() {
        return Ok(());
    }

    let write_servers = config.blossom.all_write_servers();
    for (tree_name, root) in &roots {
        if write_servers.is_empty() {
            continue;
        }
        background_blossom_push(data_dir, &root.to_string(), &write_servers)
            .await
            .with_context(|| format!("push rebuilt {tree_name} DAG to Blossom"))?;
    }

    let publish_relays = mirror_publish_relays(&config.nostr.active_relays());
    if publish_relays.is_empty() {
        return Ok(());
    }

    let (nsec_str, _was_generated) = ensure_keys_string()?;
    let keys = NostrKeys::parse(&nsec_str).context("Failed to parse nsec for publishing")?;
    let npub = keys
        .public_key()
        .to_bech32()
        .context("Failed to encode npub for publishing")?;
    let resolver = NostrRootResolver::new(NostrResolverConfig {
        relays: publish_relays,
        resolve_timeout: Duration::from_secs(5),
        secret_key: Some(keys),
    })
    .await
    .context("Failed to create Nostr resolver for rebuilt socialgraph roots")?;

    for (tree_name, root) in &roots {
        let nostr_key = format!("{npub}/{tree_name}");
        match resolver.publish(&nostr_key, root).await {
            Ok(true) => {}
            Ok(false) => {
                let _ = resolver.stop().await;
                anyhow::bail!("no relay accepted rebuilt {tree_name} root");
            }
            Err(err) => {
                let _ = resolver.stop().await;
                return Err(err)
                    .with_context(|| format!("publish rebuilt {tree_name} root to relays"));
            }
        }
    }

    let _ = resolver.stop().await;
    Ok(())
}

pub(crate) fn run_socialgraph_filter(
    data_dir: PathBuf,
    max_distance: Option<u32>,
    overmute_threshold: f64,
) -> Result<()> {
    let config = Config::load()?;
    let max_distance = max_distance.unwrap_or(config.nostr.max_write_distance);

    let (graph_store, social_graph_root_bytes) = init_socialgraph(&data_dir, &config)?;

    let mut distance_cache: HashMap<[u8; 32], Option<u32>> = HashMap::new();
    let mut overmute_cache: HashMap<[u8; 32], bool> = HashMap::new();

    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let event_value: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(err) => {
                eprintln!("Skipping invalid JSON line: {}", err);
                continue;
            }
        };

        let event_obj = match &event_value {
            serde_json::Value::Object(obj) => Some(obj),
            serde_json::Value::Array(items) if items.len() >= 3 => match &items[2] {
                serde_json::Value::Object(obj) => Some(obj),
                _ => None,
            },
            _ => None,
        };
        let Some(event_obj) = event_obj else {
            eprintln!("Skipping JSON line without event object");
            continue;
        };

        let Some(pubkey_hex) = event_obj.get("pubkey").and_then(|value| value.as_str()) else {
            eprintln!("Skipping JSON line without pubkey");
            continue;
        };
        let Some(pk_bytes) = parse_pubkey_hex(pubkey_hex) else {
            eprintln!("Skipping invalid pubkey hex: {}", pubkey_hex);
            continue;
        };

        let distance = *distance_cache.entry(pk_bytes).or_insert_with(|| {
            hashtree_cli::socialgraph::get_follow_distance(&graph_store, &pk_bytes)
        });
        let Some(distance) = distance else {
            continue;
        };
        if distance > max_distance {
            continue;
        }

        if overmute_threshold > 0.0 {
            let overmuted = *overmute_cache.entry(pk_bytes).or_insert_with(|| {
                hashtree_cli::socialgraph::is_overmuted(
                    &graph_store,
                    &social_graph_root_bytes,
                    &pk_bytes,
                    overmute_threshold,
                )
            });
            if overmuted {
                continue;
            }
        }

        stdout.write_all(trimmed.as_bytes())?;
        stdout.write_all(b"\n")?;
    }

    Ok(())
}

pub(crate) fn run_socialgraph_snapshot(
    data_dir: PathBuf,
    out: PathBuf,
    max_nodes: Option<usize>,
    max_edges: Option<usize>,
    max_distance: Option<u32>,
    max_edges_per_node: Option<usize>,
) -> Result<()> {
    let config = Config::load()?;

    let (graph_store, social_graph_root_bytes) = init_socialgraph(&data_dir, &config)?;

    let options = hashtree_cli::socialgraph::snapshot::SnapshotOptions {
        max_nodes,
        max_edges,
        max_distance,
        max_edges_per_node,
    };

    let chunks = hashtree_cli::socialgraph::snapshot::build_snapshot_chunks(
        &graph_store,
        &social_graph_root_bytes,
        &options,
    )
    .context("Failed to build social graph snapshot")?;

    let mut writer: Box<dyn Write> = if out.as_os_str() == "-" {
        Box::new(io::stdout())
    } else {
        Box::new(std::fs::File::create(&out).context("Failed to create output file")?)
    };

    for chunk in chunks {
        writer.write_all(&chunk)?;
    }
    writer.flush()?;

    Ok(())
}

pub(crate) fn run_socialgraph_stats(data_dir: PathBuf) -> Result<()> {
    let config = Config::load()?;
    let (graph_store, _social_graph_root_bytes) = init_socialgraph(&data_dir, &config)?;
    let stats = graph_store.stats().context("read social graph stats")?;

    println!("Social graph:");
    println!("  Root: {}", format_socialgraph_root(stats.root.as_deref()));
    println!("  Reachable users: {}", stats.total_users);
    println!("  Follow edges: {}", stats.total_follows);
    println!("  Max depth: {}", stats.max_depth);
    if stats.size_by_distance.is_empty() {
        println!("  Distances: none");
    } else {
        println!("  Distances:");
        for (distance, count) in stats.size_by_distance {
            println!("    {}: {}", distance, count);
        }
    }

    Ok(())
}

pub(crate) async fn run_socialgraph_rebuild_profile_index(data_dir: PathBuf) -> Result<()> {
    let config = Config::load()?;
    let (graph_store, _social_graph_root_bytes) =
        init_socialgraph_with_shared_storage(&data_dir, &config)?;
    let rebuilt = graph_store
        .rebuild_profile_index_from_stored_events_async()
        .await
        .context("rebuild profile search index from stored metadata events")?;
    let root = graph_store.profile_search_root()?;
    let by_pubkey_root = graph_store.profiles_by_pubkey_root()?;
    publish_socialgraph_roots(
        &data_dir,
        &config,
        &[
            (PUBLISHED_PROFILE_SEARCH_TREE_NAME, root.clone()),
            (PUBLISHED_PROFILES_BY_PUBKEY_TREE_NAME, by_pubkey_root),
        ],
    )
    .await?;

    println!(
        "Rebuilt profile search index from {} latest profiles",
        rebuilt
    );
    match root {
        Some(root) => {
            let nhash = nhash_encode_full(&NHashData {
                hash: root.hash,
                decrypt_key: root.key,
            })
            .context("encode profile search root nhash")?;
            println!("  hash:  {}", hex::encode(root.hash));
            if let Some(key) = root.key {
                println!("  key:   {}", hex::encode(key));
            }
            println!("  nhash: {}", nhash);
        }
        None => {
            println!("  root:  none");
        }
    }

    Ok(())
}

pub(crate) async fn run_socialgraph_rebuild_event_index(data_dir: PathBuf) -> Result<()> {
    let config = Config::load()?;
    let (graph_store, _social_graph_root_bytes) =
        init_socialgraph_with_shared_storage(&data_dir, &config)?;
    let (public_count, ambient_count) = graph_store
        .rebuild_event_indexes_from_stored_events_async()
        .await
        .context("rebuild event indexes from stored events")?;
    let public_root = graph_store.public_events_root()?;
    let profile_search_root = graph_store.profile_search_root()?;
    let profiles_by_pubkey_root = graph_store.profiles_by_pubkey_root()?;
    publish_socialgraph_roots(
        &data_dir,
        &config,
        &[
            (PUBLISHED_EVENT_TREE_NAME, public_root.clone()),
            (PUBLISHED_PROFILE_SEARCH_TREE_NAME, profile_search_root),
            (
                PUBLISHED_PROFILES_BY_PUBKEY_TREE_NAME,
                profiles_by_pubkey_root,
            ),
        ],
    )
    .await?;

    println!(
        "Rebuilt event indexes from stored events: public={}, ambient={}",
        public_count, ambient_count
    );
    match public_root {
        Some(root) => {
            let nhash = nhash_encode_full(&NHashData {
                hash: root.hash,
                decrypt_key: root.key,
            })
            .context("encode public events root nhash")?;
            println!("  public hash:  {}", hex::encode(root.hash));
            if let Some(key) = root.key {
                println!("  public key:   {}", hex::encode(key));
            }
            println!("  public nhash: {}", nhash);
        }
        None => {
            println!("  public root:  none");
        }
    }

    Ok(())
}

pub(crate) async fn run_socialgraph_warm(
    data_dir: PathBuf,
    secs: u64,
    crawl_depth: Option<u32>,
    full_graph_recrawl: bool,
    relays: Vec<String>,
    author_batch_size: usize,
    concurrent_batches: usize,
) -> Result<()> {
    let config = Config::load()?;
    let (keys, _) = ensure_keys()?;
    let (graph_store, _social_graph_root_bytes) = init_socialgraph(&data_dir, &config)?;

    let before = graph_store
        .stats()
        .context("read social graph stats before warm")?;
    let effective_crawl_depth = crawl_depth.unwrap_or(config.nostr.social_graph_crawl_depth);
    let effective_relays = if relays.is_empty() {
        config.nostr.relays.clone()
    } else {
        relays
    };
    let stored_known_since = if full_graph_recrawl {
        None
    } else {
        load_sync_cursor(&data_dir)?
    };
    let known_since = if full_graph_recrawl {
        None
    } else {
        Some(stored_known_since.unwrap_or(0))
    };
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut completed_cycles = 0usize;
    let mut cycle_known_since = known_since;

    while secs > 0 && (completed_cycles == 0 || Instant::now() < deadline) {
        let cycle_started_at = unix_timestamp();
        let crawler = SocialGraphCrawler::new(
            graph_store.clone() as Arc<dyn SocialGraphBackend>,
            keys.clone(),
            effective_relays.clone(),
            effective_crawl_depth,
        )
        .with_author_batch_size(author_batch_size)
        .with_concurrent_batches(concurrent_batches)
        .with_full_recrawl(full_graph_recrawl)
        .with_known_since(cycle_known_since);

        crawler.warm_once().await;
        save_sync_cursor(&data_dir, cycle_started_at.saturating_sub(1))?;
        completed_cycles = completed_cycles.saturating_add(1);

        if !full_graph_recrawl {
            cycle_known_since = Some(cycle_started_at.saturating_sub(1));
        }
    }

    let after = graph_store
        .stats()
        .context("read social graph stats after warm")?;
    println!(
        "Warmed social graph for {}s at depth {} ({}, {} complete cycle{})",
        secs,
        effective_crawl_depth,
        if full_graph_recrawl {
            "full recrawl"
        } else {
            "incremental"
        },
        completed_cycles,
        if completed_cycles == 1 { "" } else { "s" }
    );
    println!(
        "Known-author refresh: {}",
        known_since
            .map(|value| {
                if value == 0 {
                    "full fetch for known authors".to_string()
                } else {
                    format!("since {value}")
                }
            })
            .unwrap_or_else(|| "full fetch for known authors".to_string())
    );
    println!(
        "Reachable users: {} -> {} (delta {})",
        before.total_users,
        after.total_users,
        after.total_users.saturating_sub(before.total_users)
    );
    println!(
        "Follow edges: {} -> {} (delta {})",
        before.total_follows,
        after.total_follows,
        after.total_follows.saturating_sub(before.total_follows)
    );
    println!("Max depth: {} -> {}", before.max_depth, after.max_depth);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::format_socialgraph_root;
    use nostr::nips::nip19::ToBech32;
    use nostr::PublicKey;

    #[test]
    fn formats_hex_socialgraph_root_as_npub() {
        let hex_root = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
        let expected = PublicKey::from_hex(hex_root)
            .expect("valid hex pubkey")
            .to_bech32()
            .expect("npub encoding");

        assert_eq!(format_socialgraph_root(Some(hex_root)), expected);
    }

    #[test]
    fn preserves_unknown_and_invalid_socialgraph_root_values() {
        assert_eq!(format_socialgraph_root(None), "unknown");
        assert_eq!(
            format_socialgraph_root(Some("not-a-valid-pubkey")),
            "not-a-valid-pubkey"
        );
    }
}
