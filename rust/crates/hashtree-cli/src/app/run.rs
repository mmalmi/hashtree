use anyhow::{bail, Context, Result};
use clap::Parser;
use hashtree_cli::config::{
    ensure_auth_cookie, ensure_keys, ensure_keys_string, parse_npub, pubkey_bytes,
};
use hashtree_cli::{
    spawn_background_eviction_task, Config, FetchConfig, FetchProgress, Fetcher, HashtreeServer,
    HashtreeStore, NostrKeys, NostrResolverConfig, NostrRootResolver, NostrToBech32, RootResolver,
    BACKGROUND_EVICTION_INTERVAL, PRIORITY_OTHER,
};
use hashtree_core::{
    from_hex, nhash_decode, Cid, HashTree, HashTreeConfig, HashTreeError, NHashData,
};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::future::Future;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::add::{run_add, AddOptions};
use super::args::{
    Cli, Commands, MirrorCommands, NostrIndexCommands, PoolCommands, PoolMigrationControllerArgs,
    PrCommands, PwaCommands, ReleaseCommands, SocialGraphCommands, SocialGraphIndexArgs,
    StorageCommands,
};
use super::blossom::push_to_blossom;
use super::cashu_delegate::run_cashu_helper;
use super::daemonize::{format_daemon_status, reload_daemon, spawn_daemon, stop_daemon};
use super::lists::{follow_user, list_following, list_muted, mute_user, update_profile};
#[cfg(feature = "fuse")]
use super::mount::{mount_fuse, MountFuseOptions};
#[cfg(feature = "fuse")]
use super::mount_registry::list_active_mounts;
#[cfg(feature = "fuse")]
use super::mount_target::{
    prepare_explicit_mountpoint, reject_local_mount_target, ExplicitMountpointDisposition,
};
use super::mounts::print_active_mounts;
use super::nostr_index::{
    run_nostr_bulk_event_blob_repair, run_nostr_bulk_profile_repair,
    run_nostr_bulk_projection_audit, run_nostr_bulk_tranche_append, run_nostr_bulk_tranche_build,
    run_nostr_bulk_tranche_freeze, run_nostr_bulk_tranche_prepare, run_nostr_index_import,
    run_nostr_index_query, run_nostr_replaceable_repair, run_nostr_time_repair_preparation,
    run_socialgraph_index_from_cli, BulkEventBlobRepairOptions, BulkProfileRepairOptions,
    BulkProjectionAuditOptions, BulkTrancheAppendOptions, BulkTrancheBuildOptions,
    BulkTrancheFreezeOptions, BulkTranchePrepareOptions, NostrIndexImportOptions,
    NostrIndexQueryOptions, NostrReplaceableRepairOptions, NostrTimeRepairPreparationOptions,
    SocialGraphIndexOptions,
};
use super::peers::list_peers;
use super::pool_migration_controller::{
    run_pool_migration_controller, PoolMigrationControllerOptions,
};
#[cfg(feature = "lmdb")]
use super::pool_migration_evidence::validate_terminal_catalog_target_evidence;
use super::pool_migration_evidence::{SourceEvidenceManifestReaderV3, SourceEvidenceUnionReaderV3};
#[cfg(test)]
use super::pool_migration_launch::write_durable_pool_migration_cursor;
use super::pool_migration_launch::{
    acknowledge_pool_migration_launch, validate_source_read_concurrency,
    validate_stopped_final_batch_size, PoolMigrationLaunchContext, PoolMigrationSourceUnionAuditV3,
    MAX_FINAL_REOPEN_BATCHES,
};
use super::pool_migration_receipt::{
    validate_frozen_source_generation, PoolMigrationSourceTerminalReceiptV3, SourceContentAuditV3,
};
use super::pwa::run_export;
use super::release::publish_release_version;
use super::resolve::{
    parse_published_target, resolve_cid_input, resolve_cid_input_with_opts, ResolveOptions,
    ResolvedCid,
};
use super::socialgraph::{
    run_socialgraph_filter, run_socialgraph_publish_profile_indexes,
    run_socialgraph_rebuild_event_index, run_socialgraph_rebuild_profile_index,
    run_socialgraph_snapshot, run_socialgraph_stats, run_socialgraph_warm,
};
use super::storage_stats::print_storage_inventory;
use super::user::show_user_identity;
use super::util::{chrono_humanize_timestamp, format_bytes};
#[cfg(feature = "fuse")]
use std::io;
#[cfg(feature = "fuse")]
use std::process::Command;

pub(crate) const ALLOW_ROOT_DAEMON_ENV: &str = "HTREE_ALLOW_ROOT_DAEMON";

pub(crate) fn root_daemon_override_enabled(value: Option<&str>) -> bool {
    let Some(value) = value else {
        return false;
    };
    let value = value.trim();
    if value.is_empty() {
        return false;
    }
    !matches!(
        value.to_ascii_lowercase().as_str(),
        "0" | "false" | "no" | "off"
    )
}

#[cfg(unix)]
fn ensure_daemon_not_root() -> Result<()> {
    let allow_root = std::env::var(ALLOW_ROOT_DAEMON_ENV).ok();
    if unsafe { libc::geteuid() } == 0 && !root_daemon_override_enabled(allow_root.as_deref()) {
        bail!(
            "Refusing to run htree daemon as root. Run it under a dedicated user, \
             for example systemd User=hashtree, or set {ALLOW_ROOT_DAEMON_ENV}=1 \
             for an intentional test/container root daemon."
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_daemon_not_root() -> Result<()> {
    Ok(())
}

#[cfg(feature = "fuse")]
pub(crate) fn find_existing_active_mount<'a>(
    mounts: &'a [super::mount_registry::ActiveMount],
    mountpoint: &std::path::Path,
) -> Option<&'a super::mount_registry::ActiveMount> {
    mounts.iter().find(|mount| mount.mountpoint == mountpoint)
}

#[cfg(feature = "fuse")]
pub(crate) fn should_warn_for_temporary_mountpoint(path: &std::path::Path) -> bool {
    let temp_root = std::env::temp_dir();
    path.starts_with(&temp_root)
        || path.starts_with(std::path::Path::new("/tmp"))
        || path.starts_with(std::path::Path::new("/private/tmp"))
}

#[cfg(feature = "fuse")]
pub(crate) fn is_stale_mount_io_error(error: &io::Error) -> bool {
    error.raw_os_error() == Some(6)
}

#[cfg(feature = "fuse")]
fn probe_mountpoint(path: &std::path::Path) -> io::Result<()> {
    let mut entries = std::fs::read_dir(path)?;
    if let Some(entry) = entries.next() {
        entry?;
    }
    Ok(())
}

#[cfg(feature = "fuse")]
fn clear_stale_mountpoint(path: &std::path::Path) -> Result<bool> {
    let probe_error = match probe_mountpoint(path) {
        Ok(()) => return Ok(false),
        Err(error) if is_stale_mount_io_error(&error) => error,
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to access {}", path.display()))
        }
    };

    let umount = Command::new("umount").arg(path).status();
    let unmounted = matches!(umount, Ok(status) if status.success());

    let diskutil_unmounted = if unmounted {
        true
    } else {
        matches!(
            Command::new("diskutil")
                .args(["unmount", "force"])
                .arg(path)
                .status(),
            Ok(status) if status.success()
        )
    };

    if !diskutil_unmounted {
        return Err(probe_error).with_context(|| {
            format!(
                "Detected stale mountpoint at {} but automatic unmount failed",
                path.display()
            )
        });
    }

    match probe_mountpoint(path) {
        Ok(()) => Ok(true),
        Err(error) if is_stale_mount_io_error(&error) => Err(error).with_context(|| {
            format!(
                "Detected stale mountpoint at {} but it is still not accessible after unmount",
                path.display()
            )
        }),
        Err(error) => Err(error).with_context(|| {
            format!(
                "Failed to verify recovered mountpoint {} after unmount",
                path.display()
            )
        }),
    }
}

fn normalized_pin_label(input: &str) -> String {
    input
        .strip_prefix("htree://")
        .unwrap_or(input)
        .split('#')
        .next()
        .unwrap_or(input)
        .split('?')
        .next()
        .unwrap_or(input)
        .trim_matches('/')
        .to_string()
}

fn ensure_supported_pin_target(input: &str) -> Result<()> {
    let normalized = normalized_pin_label(input);
    if normalized.starts_with("npub1") && !normalized.contains('/') {
        anyhow::bail!("Author-wide mirroring is a mirror policy. Use `htree mirror add <npub>`.");
    }
    Ok(())
}

async fn resolve_cid_input_for_pin(input: &str, data_dir: &Path) -> Result<ResolvedCid> {
    let opts = ResolveOptions {
        data_dir: Some(data_dir.to_path_buf()),
        ..ResolveOptions::default()
    };
    resolve_cid_input_with_opts(input, &opts).await
}

fn pinned_ref_key_for_input(input: &str) -> Option<String> {
    let parsed_target = parse_published_target(input)?;
    if parsed_target.path.is_some() {
        return None;
    }
    Some(parsed_target.resolver_key())
}

pub(crate) fn stored_published_pin_hash(
    store: &HashtreeStore,
    input: &str,
) -> Result<Option<[u8; 32]>> {
    let Some(ref_key) = pinned_ref_key_for_input(input) else {
        return Ok(None);
    };
    store.get_tree_ref(&ref_key)
}

pub(crate) async fn pin_input_target(
    store: &Arc<HashtreeStore>,
    fetcher: &Fetcher,
    input: &str,
    resolved: &ResolvedCid,
) -> Result<Cid> {
    let target_cid = resolve_load_target_cid(fetcher, store, resolved, None).await?;
    let normalized_input = normalized_pin_label(input);
    let parsed_target = parse_published_target(input);

    let (owner, name, ref_key) = if let Some(parsed_target) = parsed_target.as_ref() {
        let name = parsed_target
            .path
            .as_deref()
            .map(|path| format!("{}/{}", parsed_target.tree_name, path))
            .unwrap_or_else(|| parsed_target.tree_name.clone());
        let ref_key = parsed_target
            .path
            .is_none()
            .then(|| parsed_target.resolver_key());
        (parsed_target.npub.clone(), Some(name), ref_key)
    } else {
        ("pinned".to_string(), Some(normalized_input), None)
    };

    store.pin(&target_cid.hash)?;
    store.index_tree(
        &target_cid.hash,
        &owner,
        name.as_deref(),
        PRIORITY_OTHER,
        ref_key.as_deref(),
    )?;
    if let Some(ref_key) = ref_key.as_deref() {
        store.add_pinned_ref(ref_key)?;
    }

    if let Some(parsed_target) = parsed_target.as_ref() {
        let pubkey_hex = hex::encode(parse_npub(&parsed_target.npub)?);
        let root_hash = hashtree_core::to_hex(&resolved.cid.hash);
        let root_key = resolved.cid.key.map(hex::encode);
        let updated_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        store.set_cached_root(
            &pubkey_hex,
            &parsed_target.tree_name,
            &root_hash,
            root_key.as_deref(),
            "public",
            updated_at,
        )?;
    }

    if let Err(error) = store.evict_if_needed() {
        tracing::warn!("Post-pin eviction check failed: {}", error);
    }

    Ok(target_cid)
}

pub(crate) fn should_spawn_background_update(cli: &Cli) -> bool {
    if matches!(&cli.command, Commands::Update { .. } | Commands::BgCheck) {
        return false;
    }
    !matches!(
        &cli.command,
        Commands::NostrIndex { command }
            if matches!(
                command.as_ref(),
                NostrIndexCommands::RepairBulkProjectionProfiles { .. }
                    | NostrIndexCommands::RepairBulkProjectionEventBlobs { .. }
            )
    ) && !matches!(
        &cli.command,
        Commands::Storage {
            command: StorageCommands::Pool {
                command: PoolCommands::MigrateLmdb { .. } | PoolCommands::LaunchMigrateLmdbV3(_),
            },
        }
    )
}

pub(crate) async fn run() -> Result<()> {
    // Install rustls crypto provider (required for TLS connections)
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Initialize tracing (respects RUST_LOG env var)
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    // Get data_dir early to avoid borrow issues in match arms
    let data_dir = cli.data_dir();

    if matches!(cli.command, Commands::Start { .. }) {
        ensure_daemon_not_root()?;
    }

    // Background self-update flow:
    //   1) Print any pending notification from the previous bg check.
    //   2) If interval elapsed, fork a detached child that writes a fresh
    //      result. Detached so it survives short commands like `htree user`.
    // Skipped for `htree update` (user already has it covered) and the
    // internal `__bg_check` subcommand itself (don't recursively fork). The
    // profile repair also skips it: its pinned, single-writer recovery
    // boundary must not create a concurrent process before locks and
    // authority checks are established.
    if should_spawn_background_update(&cli) {
        super::update::print_cached_update_notification(&data_dir);
        super::update::spawn_detached_bg_check(&data_dir);
    }

    match cli.command {
        Commands::Start {
            addr,
            relays: relays_override,
            mode: mode_override,
            daemon,
            log_file,
            pid_file,
        } => {
            if daemon && std::env::var_os("HTREE_DAEMONIZED").is_none() {
                spawn_daemon(
                    addr.as_deref(),
                    relays_override.as_deref(),
                    mode_override.map(Into::into),
                    cli.data_dir.clone(),
                    log_file.as_ref(),
                    pid_file.as_ref(),
                )?;
                return Ok(());
            }
            // Load or create config
            let mut config = Config::load()?;

            // Override relays if specified on command line
            if let Some(relays_str) = relays_override.as_deref() {
                config.nostr.relays = relays_str
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .collect();
                println!("Using relays from CLI: {:?}", config.nostr.relays);
            }
            if let Some(mode) = mode_override {
                config.server.mode = mode.into();
                println!("Using mode from CLI: {}", config.server.mode.as_str());
            }
            if let Some(addr) = addr.as_deref() {
                config.server.bind_address = addr.to_string();
                println!(
                    "Using bind address from CLI: {}",
                    config.server.bind_address
                );
            }
            let bind_address = config.server.bind_address.clone();

            // Use CLI data_dir if provided, otherwise use config's data_dir
            let data_dir = cli
                .data_dir
                .clone()
                .unwrap_or_else(|| PathBuf::from(&config.storage.data_dir));

            // Convert max_size_gb to bytes
            let max_size_bytes = config.storage.max_size_gb * 1024 * 1024 * 1024;
            let nostr_db_max_bytes = config
                .nostr
                .db_max_size_gb
                .saturating_mul(1024 * 1024 * 1024);
            let spambox_db_max_bytes = config
                .nostr
                .spambox_max_size_gb
                .saturating_mul(1024 * 1024 * 1024);
            let store = Arc::new(HashtreeStore::with_options(
                &data_dir,
                config.storage.s3.as_ref(),
                max_size_bytes,
            )?);

            // Ensure nsec exists (generate if needed)
            let (keys, was_generated) = ensure_keys()?;
            let pk_bytes = pubkey_bytes(&keys);
            let npub = keys
                .public_key()
                .to_bech32()
                .context("Failed to encode npub")?;

            // Convert allowed_npubs to hex pubkeys for blossom access control
            let mut allowed_pubkeys: HashSet<String> = HashSet::new();
            // Always allow own pubkey
            allowed_pubkeys.insert(hex::encode(pk_bytes));
            // Add configured allowed npubs
            for npub_str in &config.nostr.allowed_npubs {
                if let Ok(pk) = parse_npub(npub_str) {
                    allowed_pubkeys.insert(hex::encode(pk));
                } else {
                    tracing::warn!("Invalid npub in allowed_npubs: {}", npub_str);
                }
            }

            // Set social graph root (configured npub or own key)
            let social_graph_root_bytes = if let Some(ref root_npub) = config.nostr.socialgraph_root
            {
                parse_npub(root_npub).unwrap_or(pk_bytes)
            } else {
                pk_bytes
            };
            let nostr_relay_config = hashtree_cli::nostr_relay::NostrRelayConfig {
                spambox_db_max_bytes,
                ..Default::default()
            };
            let pool_audit_read_only = store.is_pool_audit_read_only();
            let graph_store;
            let social_graph_store;
            let social_graph;
            let fips_peer_ids;
            let nostr_relay;
            let crawler_spambox_backend;
            if pool_audit_read_only {
                tracing::warn!(
                    "Pool audit-serving read-only mode: social graph and durable Nostr relay writers remain unopened"
                );
                graph_store = None;
                social_graph_store = None;
                social_graph = None;
                fips_peer_ids = Vec::new();
                nostr_relay = config.nostr.enabled.then(|| {
                    Arc::new(hashtree_cli::nostr_relay::NostrRelay::new_read_only(
                        data_dir.clone(),
                        nostr_relay_config,
                    ))
                });
                crawler_spambox_backend = None;
            } else {
                // Initialize the local social graph store.
                let opened_graph_store =
                    hashtree_cli::socialgraph::open_social_graph_store_with_storage(
                        &data_dir,
                        store.store_arc(),
                        Some(nostr_db_max_bytes),
                    )
                    .context("Failed to initialize social graph store")?;
                opened_graph_store
                    .set_profile_index_overmute_threshold(config.nostr.overmute_threshold);
                hashtree_cli::socialgraph::set_social_graph_root(
                    &opened_graph_store,
                    &social_graph_root_bytes,
                );
                hashtree_cli::socialgraph::sync_local_list_files_force(
                    opened_graph_store.as_ref(),
                    &data_dir,
                    &keys,
                )
                .context("Failed to sync local social graph lists")?;
                fips_peer_ids = hashtree_cli::fips_transport::fips_peer_ids_from_pubkeys(
                    hashtree_cli::socialgraph::get_follows(opened_graph_store.as_ref(), &pk_bytes),
                );
                let opened_social_graph_store: Arc<
                    dyn hashtree_cli::socialgraph::SocialGraphBackend,
                > = opened_graph_store.clone();
                let opened_social_graph =
                    Arc::new(hashtree_cli::socialgraph::SocialGraphAccessControl::new(
                        Arc::clone(&opened_social_graph_store),
                        config.nostr.max_write_distance,
                        allowed_pubkeys.clone(),
                    ));
                nostr_relay = if config.nostr.enabled {
                    let mut public_event_pubkeys = HashSet::new();
                    public_event_pubkeys.insert(hex::encode(pk_bytes));
                    Some(Arc::new(
                        hashtree_cli::nostr_relay::NostrRelay::new(
                            Arc::clone(&opened_social_graph_store),
                            data_dir.clone(),
                            public_event_pubkeys,
                            Some(opened_social_graph.clone()),
                            nostr_relay_config,
                        )
                        .map(|relay| {
                            relay.with_historical_nostr_index(store.store_arc(), data_dir.clone())
                        })
                        .context("Failed to initialize Nostr relay")?,
                    ))
                } else {
                    None
                };
                let crawler_spambox = if config.nostr.enabled && spambox_db_max_bytes != 0 {
                    let spam_dir = data_dir.join("socialgraph_spambox");
                    match hashtree_cli::socialgraph::open_social_graph_store_at_path(
                        &spam_dir,
                        Some(spambox_db_max_bytes),
                    ) {
                        Ok(store) => Some(store),
                        Err(err) => {
                            tracing::warn!(
                                "Failed to open social graph spambox for crawler: {}",
                                err
                            );
                            None
                        }
                    }
                } else {
                    None
                };
                crawler_spambox_backend = crawler_spambox
                    .map(|store| store as Arc<dyn hashtree_cli::socialgraph::SocialGraphBackend>);
                graph_store = Some(opened_graph_store);
                social_graph_store = Some(opened_social_graph_store);
                social_graph = Some(opened_social_graph);
            }

            // Keep client-visible Blossom endpoints available to the daemon,
            // except for its own listener, which would recurse on a miss.
            let upstream_blossom = config.blossom.upstream_read_servers(&bind_address);
            let blossom_replica_queue_bytes = hashtree_cli::server::bounded_upload_queue_bytes(
                config
                    .blossom
                    .replicate_queue_mb
                    .saturating_mul(1024 * 1024),
            );
            let active_nostr_relays = config.nostr.active_relays();
            let active_nostr_relay_count = active_nostr_relays.len();
            let fips_handle = hashtree_cli::fips_transport::start_daemon_fips_transport(
                &config,
                &keys,
                Arc::clone(&store),
                fips_peer_ids,
            )
            .await?;
            let nostr_cache =
                hashtree_cli::fips_transport::new_daemon_nostr_cache(store.store_arc());
            let nostr_provider = hashtree_cli::fips_transport::start_daemon_nostr_provider(
                &config,
                fips_handle.as_ref(),
                Some(Arc::clone(&nostr_cache)),
            )
            .await?;
            #[cfg(feature = "experimental-decentralized-pubsub")]
            let nostr_pubsub_handle = hashtree_cli::fips_transport::start_daemon_nostr_pubsub(
                &config,
                fips_handle.as_ref(),
                nostr_relay.clone(),
                nostr_cache,
            )
            .await?;

            // Set up server with allowed pubkeys for blossom write access
            let mut server = HashtreeServer::new(Arc::clone(&store), bind_address.clone())
                .with_server_mode(config.server.mode)
                .with_hash_get_enabled(config.server.mode.hash_get_enabled())
                .with_fetch_from_fips_peers(config.server.fetch_from_fips_peers)
                .with_allowed_pubkeys(allowed_pubkeys.clone())
                .with_max_upload_bytes((config.blossom.max_upload_mb as usize) * 1024 * 1024)
                .with_public_writes(config.server.public_writes)
                .with_public_plaintext_reads(config.server.public_plaintext_reads)
                .with_require_random_untrusted_ingest(
                    config.blossom.require_random_untrusted_ingest,
                )
                .with_optimistic_blossom_uploads(config.blossom.optimistic_uploads)
                .with_upstream_blossom(upstream_blossom)
                .with_blossom_upload_replicas(
                    config.blossom.replicate_servers.clone(),
                    blossom_replica_queue_bytes,
                    keys.clone(),
                )
                .with_nostr_relay_urls(active_nostr_relays);

            // Add social graph to server when its durable stores are writable.
            if let Some(social_graph) = social_graph {
                server = server.with_social_graph(social_graph);
            }
            if let Some(social_graph_store) = social_graph_store.as_ref() {
                server = server.with_socialgraph_snapshot(
                    Arc::clone(social_graph_store),
                    social_graph_root_bytes,
                    config.server.socialgraph_snapshot_public,
                );
            }
            if let Some(nostr_relay) = nostr_relay.clone() {
                server = server.with_nostr_relay(nostr_relay);
            }
            if let Some(provider) = nostr_provider {
                server = server.with_nostr_provider(provider);
            }

            if let Some(ref fips_handle) = fips_handle {
                server = server
                    .with_fips_endpoint(fips_handle.endpoint.clone())
                    .with_fips_blob_resolver(fips_handle.blob_resolver.clone());
            }

            let background_services_controller = match (graph_store, social_graph_store.as_ref()) {
                (Some(graph_store), Some(social_graph_store)) => Some(Arc::new(
                    hashtree_cli::daemon::EmbeddedBackgroundServicesController::new(
                        keys.clone(),
                        data_dir.clone(),
                        Arc::clone(&store),
                        graph_store,
                        Arc::clone(social_graph_store),
                        crawler_spambox_backend,
                    ),
                )),
                _ => None,
            };

            // Print startup info
            println!("Starting hashtree daemon on {}", bind_address);
            println!("Data directory: {}", data_dir.display());
            if was_generated {
                println!("Identity: {} (new)", npub);
            } else {
                println!("Identity: {}", npub);
            }
            println!("Mode: {}", config.server.mode.as_str());
            println!(
                "Hash Get: {}",
                if config.server.mode.hash_get_enabled() {
                    "enabled"
                } else {
                    "disabled"
                }
            );
            if !config.nostr.allowed_npubs.is_empty() {
                println!(
                    "Allowed writers: {} npubs",
                    config.nostr.allowed_npubs.len()
                );
            }
            if config.server.public_writes {
                println!("Public writes: enabled");
            }
            if !config.server.public_plaintext_reads {
                println!("Public plaintext reads: allowlist only");
            }
            println!("Relays: {} configured", active_nostr_relay_count);
            if let Some(ref fips_handle) = fips_handle {
                println!(
                    "FIPS: enabled (scope {}, endpoint {}, UDP {}, WebRTC {})",
                    fips_handle.discovery_scope,
                    fips_handle.endpoint_npub,
                    if config.server.enable_fips_udp {
                        "enabled"
                    } else {
                        "disabled"
                    },
                    if config.server.enable_fips_webrtc {
                        "enabled"
                    } else {
                        "disabled"
                    }
                );
            } else if config.server.enable_fips {
                println!("FIPS: disabled in this server mode");
            }
            #[cfg(feature = "experimental-decentralized-pubsub")]
            if nostr_pubsub_handle.is_some() {
                println!("Nostr decentralized pubsub: enabled (authenticated FIPS pubsub)");
            } else if config.nostr.decentralized_pubsub {
                println!(
                    "Nostr decentralized pubsub: disabled (requires local Nostr relay and FIPS)"
                );
            }
            println!("Git remote: http://{}/git/<pubkey>/<repo>", bind_address);
            if pool_audit_read_only {
                println!("Social graph: read-only maintenance projection");
            } else {
                println!(
                    "Social graph: enabled (social_graph_crawl_depth={}, max_write_distance={})",
                    config.nostr.social_graph_crawl_depth, config.nostr.max_write_distance
                );
            }
            println!("Storage limit: {} GB", config.storage.max_size_gb);
            if !config.cashu.accepted_mints.is_empty() {
                println!(
                    "Cashu accepted mints: {}",
                    config.cashu.accepted_mints.len()
                );
                if let Some(default_mint) = &config.cashu.default_mint {
                    println!("Cashu default mint: {}", default_mint);
                }
            }
            if config.sync.enabled {
                let mut sync_features = Vec::new();
                if config.sync.sync_own {
                    sync_features.push("own trees");
                }
                if config.sync.sync_followed {
                    sync_features.push("followed trees");
                }
                if sync_features.is_empty() {
                    println!("Background sync: enabled");
                } else {
                    println!("Background sync: enabled ({})", sync_features.join(", "));
                }
            }

            if config.server.enable_auth {
                let (username, password) = ensure_auth_cookie()?;
                println!();
                println!("Web UI: http://{}/#{}:{}", bind_address, username, password);
                server = server.with_auth(username, password);
            } else {
                println!("Web UI: http://{}", bind_address);
                println!("Auth: disabled");
            }

            let listener = tokio::net::TcpListener::bind(&bind_address)
                .await
                .with_context(|| format!("Failed to bind daemon listener {}", bind_address))?;
            let server_handle =
                tokio::spawn(async move { server.run_with_listener(listener).await });

            if let Some(controller) = background_services_controller.as_ref() {
                controller
                    .apply_config(&config)
                    .await
                    .context("Failed to start background services")?;
            }

            // Start background eviction task (runs every 5 minutes), except
            // while the Pool catalog and member files are being audited.
            let eviction_handle = if store.is_pool_audit_read_only() {
                tracing::warn!(
                    "Pool audit-serving read-only mode: daemon background eviction remains stopped"
                );
                None
            } else {
                Some(spawn_background_eviction_task(
                    Arc::clone(&store),
                    BACKGROUND_EVICTION_INTERVAL,
                    "daemon",
                ))
            };

            match server_handle.await {
                Ok(Ok(_)) => {}
                Ok(Err(err)) => return Err(err),
                Err(err) => anyhow::bail!("Daemon server task failed: {}", err),
            }

            // Shutdown social graph crawler
            // Shutdown background eviction
            if let Some(eviction_handle) = eviction_handle {
                eviction_handle.abort();
            }

            #[cfg(feature = "experimental-decentralized-pubsub")]
            if let Some(ref handle) = nostr_pubsub_handle {
                handle.shutdown();
            }

            if let Some(ref fips_handle) = fips_handle {
                fips_handle.shutdown().await;
            }

            if let Some(controller) = background_services_controller {
                controller.shutdown().await;
            }
        }
        #[cfg(feature = "fuse")]
        Commands::Mount {
            target,
            mountpoint,
            visibility,
            link_key,
            private,
            relays,
            allow_other,
        } => {
            let current_dir = std::env::current_dir()?;
            reject_local_mount_target(&target, &current_dir)
                .context("Failed to validate mount target")?;

            let mountpoint = if let Some(path) = mountpoint {
                let path = if path.is_relative() {
                    current_dir.join(path)
                } else {
                    path
                };

                if should_warn_for_temporary_mountpoint(&path) {
                    eprintln!(
                        "warning: mounting under {} may be less reliable for long-lived published mounts; prefer a persistent path under your home directory",
                        std::env::temp_dir().display()
                    );
                }
                if path.exists() && clear_stale_mountpoint(&path)? {
                    eprintln!("Recovered stale mountpoint at {}", path.display());
                }
                let active_mounts = list_active_mounts(&data_dir)?;
                if let Some(existing) = find_existing_active_mount(&active_mounts, &path) {
                    println!("already mounted {}", path.display());
                    println!("  target: {}", existing.target);
                    println!("  cid: {}", existing.mounted_cid);
                    if let Some(published) = existing.published_key.as_deref() {
                        println!("  published: {}", published);
                    }
                    return Ok(());
                }

                match prepare_explicit_mountpoint(&path)? {
                    ExplicitMountpointDisposition::CreateDir => {
                        std::fs::create_dir(&path).with_context(|| {
                            format!("Failed to create mountpoint {}", path.display())
                        })?;
                    }
                    ExplicitMountpointDisposition::UseExistingEmptyDir => {}
                }

                Some(path)
            } else {
                None
            };

            mount_fuse(
                target,
                mountpoint,
                data_dir,
                MountFuseOptions {
                    visibility,
                    link_key,
                    private,
                    relays,
                    allow_other,
                },
            )
            .await?;
        }
        Commands::Mounts { json } => {
            print_active_mounts(&data_dir, json)?;
        }
        Commands::Add {
            path,
            only_hash,
            unencrypted,
            no_ignore,
            publish,
            chunk_size,
            local,
        } => {
            run_add(
                data_dir.clone(),
                path,
                AddOptions {
                    only_hash,
                    unencrypted,
                    no_ignore,
                    publish,
                    chunk_size,
                    local,
                },
            )
            .await?
        }
        Commands::Pwa { command } => match command {
            PwaCommands::Export { url, json } => run_export(data_dir.clone(), url, json).await?,
        },
        Commands::Load { cid: cid_input } => {
            let resolved = resolve_cid_input(&cid_input).await?;
            let store = Arc::new(HashtreeStore::new(&data_dir)?);
            let fetcher = Fetcher::new(FetchConfig::default());
            let progress = Arc::new(FetchProgress::new());
            let target_cid = run_with_fetch_progress("Loading", Arc::clone(&progress), async {
                resolve_load_target_cid(&fetcher, &store, &resolved, Some(progress.as_ref())).await
            })
            .await?;
            ensure_loaded_target_present(&store, &target_cid)?;

            let fetched = progress.snapshot();
            if fetched.chunks_fetched > 0 {
                println!(
                    "Loaded {} into local storage ({})",
                    format_cid_for_display(&target_cid),
                    format_fetch_summary(fetched)
                );
            } else {
                println!(
                    "Already available locally: {}",
                    format_cid_for_display(&target_cid)
                );
            }
        }
        Commands::Get {
            cid: cid_input,
            output,
        } => {
            use hashtree_core::{to_hex, Cid};

            // Resolve to Cid (raw bytes, no hex conversion needed for nhash)
            let resolved = resolve_cid_input(&cid_input).await?;
            let cid = resolved.cid.clone();

            let store = Arc::new(HashtreeStore::new(&data_dir)?);
            let fetcher = Fetcher::new(FetchConfig::default());
            let progress = Arc::new(FetchProgress::new());
            let target_cid = run_with_fetch_progress("Fetching", Arc::clone(&progress), async {
                resolve_load_target_cid(&fetcher, &store, &resolved, Some(progress.as_ref())).await
            })
            .await?;
            ensure_loaded_target_present(&store, &target_cid)?;

            // Check if it's a directory
            let listing = store.get_directory_listing_by_cid(&cid)?;

            // Handle path: nhash/path/to/file.ext
            if let Some(path) = resolved.path.as_deref() {
                let filename = path.rsplit('/').next().unwrap_or(path);
                let out_path = output.unwrap_or_else(|| PathBuf::from(filename));

                store.write_file_by_cid(&target_cid, &out_path)?;
                println!("{} -> {}", to_hex(&target_cid.hash), out_path.display());
            } else if listing.is_some() {
                // It's a directory - create it and download contents
                let hash_hex = to_hex(&cid.hash);
                let out_dir = output.unwrap_or_else(|| PathBuf::from(&hash_hex));
                std::fs::create_dir_all(&out_dir)?;

                async fn download_dir(
                    store: &Arc<HashtreeStore>,
                    cid: &Cid,
                    dir: &std::path::Path,
                ) -> Result<()> {
                    // Get listing
                    let listing = store.get_directory_listing_by_cid(cid)?;
                    if let Some(listing) = listing {
                        for entry in listing.entries {
                            let entry_path = dir.join(&entry.name);
                            let entry_cid = Cid::parse(&entry.cid)
                                .map_err(|e| anyhow::anyhow!("Invalid CID: {}", e))?;
                            if entry.is_directory {
                                std::fs::create_dir_all(&entry_path)?;
                                Box::pin(download_dir(store, &entry_cid, &entry_path)).await?;
                            } else {
                                store.write_file_by_cid(&entry_cid, &entry_path)?;
                                println!("  {} -> {}", entry.cid, entry_path.display());
                            }
                        }
                    }
                    Ok(())
                }

                println!("Downloading directory to {}", out_dir.display());
                download_dir(&store, &cid, &out_dir).await?;
                println!("Done.");
            } else {
                // Try as a file - stream from store to output path with decryption support.
                let hash_hex = to_hex(&target_cid.hash);
                let out_path = output.unwrap_or_else(|| PathBuf::from(&hash_hex));
                store.write_file_by_cid(&target_cid, &out_path)?;
                println!("{} -> {}", hash_hex, out_path.display());
            }
        }
        Commands::Cat { cid: cid_input } => {
            use std::io::Write;

            // Resolve npub/repo or htree:// URLs to CID
            let resolved = resolve_cid_input(&cid_input).await?;

            let store = Arc::new(HashtreeStore::new(&data_dir)?);
            let fetcher = Fetcher::new(FetchConfig::default());
            let target_cid = resolve_cat_target_cid(&fetcher, &store, &resolved).await?;
            let mut stdout = std::io::stdout().lock();
            store.write_file_by_cid_to_writer(&target_cid, &mut stdout)?;
            stdout.flush()?;
        }
        Commands::Pins => {
            let store = HashtreeStore::new(&data_dir)?;
            let pins = store.list_pins_with_names()?;
            if pins.is_empty() {
                println!("No pinned CIDs");
            } else {
                println!("Pinned items ({}):", pins.len());
                for pin in pins {
                    let icon = if pin.is_directory { "dir" } else { "file" };
                    println!(
                        "  [{}] {} - {} ({})",
                        icon,
                        pin.name,
                        format_bytes(pin.size_bytes),
                        pin.cid
                    );
                }
            }
        }
        Commands::Pin { cid: cid_input } => {
            ensure_supported_pin_target(&cid_input)?;
            let resolved = resolve_cid_input_for_pin(&cid_input, &data_dir).await?;
            let store = Arc::new(HashtreeStore::new(&data_dir)?);
            let fetcher = Fetcher::new(FetchConfig::default());
            let pinned = pin_input_target(&store, &fetcher, &cid_input, &resolved).await?;
            println!("Pinned: {}", format_cid_for_display(&pinned));
        }
        Commands::Unpin { cid: cid_input } => {
            ensure_supported_pin_target(&cid_input)?;
            let store = HashtreeStore::new(&data_dir)?;
            if let Some(hash) = stored_published_pin_hash(&store, &cid_input)? {
                store.unpin(&hash)?;
                if let Some(ref_key) = pinned_ref_key_for_input(&cid_input) {
                    store.remove_pinned_ref(&ref_key)?;
                }
                println!("Unpinned: {}", hashtree_core::to_hex(&hash));
            } else {
                let resolved = resolve_cid_input_for_pin(&cid_input, &data_dir).await?;
                let store = Arc::new(store);
                let fetcher = Fetcher::new(FetchConfig::default());
                let target = resolve_load_target_cid(&fetcher, &store, &resolved, None).await?;
                store.unpin(&target.hash)?;
                if let Some(ref_key) = pinned_ref_key_for_input(&cid_input) {
                    store.remove_pinned_ref(&ref_key)?;
                }
                println!("Unpinned: {}", format_cid_for_display(&target));
            }
        }
        Commands::Mirror { command } => match command {
            MirrorCommands::Add { npub } => {
                parse_npub(&npub).context("Invalid npub")?;
                let store = HashtreeStore::new(&data_dir)?;
                if store.add_tracked_author(&npub)? {
                    println!("Mirroring author: {}", npub);
                } else {
                    println!("Already mirroring author: {}", npub);
                }
            }
            MirrorCommands::Rm { npub } => {
                parse_npub(&npub).context("Invalid npub")?;
                let store = HashtreeStore::new(&data_dir)?;
                if store.remove_tracked_author(&npub)? {
                    println!("Stopped mirroring author: {}", npub);
                } else {
                    println!("Not mirroring author: {}", npub);
                }
            }
            MirrorCommands::Ls => {
                let store = HashtreeStore::new(&data_dir)?;
                let authors = store.list_tracked_authors()?;
                if authors.is_empty() {
                    println!("No mirrored authors");
                } else {
                    println!("Mirrored authors ({}):", authors.len());
                    for npub in authors {
                        println!("  {}", npub);
                    }
                }
            }
        },
        Commands::NostrIndex { command } => match *command {
            NostrIndexCommands::Import {
                root,
                events_file,
                out,
            } => {
                run_nostr_index_import(
                    data_dir,
                    NostrIndexImportOptions {
                        root,
                        events_file,
                        out,
                    },
                )
                .await?;
            }
            NostrIndexCommands::Query {
                root,
                filter,
                filter_file,
                limit,
                out,
            } => {
                let filter_json = match (filter, filter_file) {
                    (Some(filter), None) => filter,
                    (None, Some(path)) => std::fs::read_to_string(&path).with_context(|| {
                        format!("read Nostr filter JSON from {}", path.display())
                    })?,
                    (Some(_), Some(_)) => {
                        bail!("--filter and --filter-file cannot be used together")
                    }
                    (None, None) => bail!("missing --filter or --filter-file"),
                };
                run_nostr_index_query(
                    data_dir,
                    NostrIndexQueryOptions {
                        root,
                        filter_json,
                        limit,
                        out,
                    },
                )
                .await?;
            }
            NostrIndexCommands::AuditBulkProjection {
                staging_data_dir,
                expected_state_sha256,
                v3_candidate,
                expected_stage_state_sha256,
                expected_policy_sha256,
                expected_profile_distance_seal_sha256,
                profile_rank_decisions_file,
                expected_profile_rank_decisions_file_sha256,
                profile_rank_decisions_report,
                expected_profile_rank_decisions_report_sha256,
                expected_full_author_count,
                allow_recovery_tranche,
                btree_order,
                page_size,
                query_limit,
                out,
            } => {
                run_nostr_bulk_projection_audit(
                    data_dir,
                    BulkProjectionAuditOptions {
                        staging_data_dir,
                        expected_state_sha256,
                        v3_candidate,
                        expected_stage_state_sha256,
                        expected_policy_sha256,
                        expected_profile_distance_seal_sha256,
                        profile_rank_decisions_file,
                        expected_profile_rank_decisions_file_sha256,
                        profile_rank_decisions_report,
                        expected_profile_rank_decisions_report_sha256,
                        expected_full_author_count,
                        allow_recovery_tranche,
                        btree_order,
                        page_size,
                        query_limit,
                        out,
                    },
                )
                .await?;
            }
            NostrIndexCommands::RepairBulkProjectionProfiles {
                staging_data_dir,
                expected_state_sha256,
                expected_stage_state_sha256,
                expected_policy_sha256,
                expected_spool_data_sha256,
                profile_rank_decisions_file,
                expected_profile_rank_decisions_file_sha256,
                profile_rank_decisions_report,
                expected_profile_rank_decisions_report_sha256,
                expected_replayed_author_count,
                expected_full_author_count,
                expected_profiles_by_pubkey_root_file_sha256,
                expected_profile_search_root_file_sha256,
                required_profile_pubkeys,
                btree_order,
                out,
            } => {
                run_nostr_bulk_profile_repair(
                    data_dir,
                    BulkProfileRepairOptions {
                        staging_data_dir,
                        expected_state_sha256,
                        expected_stage_state_sha256,
                        expected_policy_sha256,
                        expected_spool_data_sha256,
                        profile_rank_decisions_file,
                        expected_profile_rank_decisions_file_sha256,
                        profile_rank_decisions_report,
                        expected_profile_rank_decisions_report_sha256,
                        expected_replayed_author_count,
                        expected_full_author_count,
                        expected_profiles_by_pubkey_root_file_sha256,
                        expected_profile_search_root_file_sha256,
                        required_profile_pubkeys,
                        btree_order,
                        out,
                    },
                )
                .await?;
            }
            NostrIndexCommands::RepairBulkProjectionEventBlobs {
                staging_data_dir,
                expected_state_sha256,
                expected_stage_state_sha256,
                expected_policy_sha256,
                expected_spool_data_sha256,
                expected_profile_repair_retention_lease_sha256,
                expected_replayed_author_count,
                expected_full_author_count,
                btree_order,
                page_size,
                apply,
                out,
            } => {
                run_nostr_bulk_event_blob_repair(
                    data_dir,
                    BulkEventBlobRepairOptions {
                        staging_data_dir,
                        expected_state_sha256,
                        expected_stage_state_sha256,
                        expected_policy_sha256,
                        expected_spool_data_sha256,
                        expected_profile_repair_retention_lease_sha256,
                        expected_replayed_author_count,
                        expected_full_author_count,
                        btree_order,
                        page_size,
                        apply,
                        out,
                    },
                )
                .await?;
            }
            NostrIndexCommands::PrepareBulkTranche {
                staging_data_dir,
                eligible_authors,
                expected_v2_state_sha256,
                expected_stage_state_sha256,
                audit_evidence,
                profile_rank_decisions_file,
                expected_profile_rank_decisions_file_sha256,
                profile_rank_decisions_report,
                expected_profile_rank_decisions_report_sha256,
                serving_root,
                serving_event,
                serving_event_id,
                serving_publisher_pubkey,
                serving_tree_name,
                btree_order,
                btree_update_concurrency,
                index_commit_batch_size,
                out,
            } => {
                run_nostr_bulk_tranche_prepare(
                    data_dir,
                    BulkTranchePrepareOptions {
                        staging_data_dir,
                        eligible_authors,
                        expected_v2_state_sha256,
                        expected_stage_state_sha256,
                        audit_evidence,
                        profile_rank_decisions_file,
                        expected_profile_rank_decisions_file_sha256,
                        profile_rank_decisions_report,
                        expected_profile_rank_decisions_report_sha256,
                        serving_root,
                        serving_event,
                        serving_event_id,
                        serving_publisher_pubkey,
                        serving_tree_name,
                        btree_order,
                        btree_update_concurrency,
                        index_commit_batch_size,
                        out,
                    },
                )?;
            }
            NostrIndexCommands::AppendBulkTranche {
                staging_data_dir,
                expected_state_sha256,
                max_segments,
                out,
            } => {
                run_nostr_bulk_tranche_append(
                    data_dir,
                    BulkTrancheAppendOptions {
                        staging_data_dir,
                        expected_state_sha256,
                        max_segments,
                        out,
                    },
                )
                .await?;
            }
            NostrIndexCommands::FreezeBulkTranche {
                staging_data_dir,
                expected_state_sha256,
                through_author,
                out,
            } => {
                run_nostr_bulk_tranche_freeze(
                    data_dir,
                    BulkTrancheFreezeOptions {
                        staging_data_dir,
                        expected_state_sha256,
                        through_author,
                        out,
                    },
                )?;
            }
            NostrIndexCommands::BuildBulkTranche {
                staging_data_dir,
                expected_state_sha256,
                max_indexes,
                out,
            } => {
                run_nostr_bulk_tranche_build(
                    data_dir,
                    BulkTrancheBuildOptions {
                        staging_data_dir,
                        expected_state_sha256,
                        max_indexes,
                        out,
                    },
                )
                .await?;
            }
            NostrIndexCommands::PrepareTimeRepair { state_file, apply } => {
                let output = run_nostr_time_repair_preparation(
                    data_dir,
                    NostrTimeRepairPreparationOptions { state_file, apply },
                )
                .await?;
                println!("{}", serde_json::to_string_pretty(&output)?);
            }
            NostrIndexCommands::RepairReplaceable {
                state_file,
                staging_data_dir,
                eligible_authors,
                page_size,
                btree_order,
                apply,
            } => {
                let output = run_nostr_replaceable_repair(
                    data_dir,
                    NostrReplaceableRepairOptions {
                        state_file,
                        staging_data_dir,
                        eligible_authors,
                        page_size,
                        btree_order,
                        apply,
                    },
                )
                .await?;
                println!("{}", serde_json::to_string_pretty(&output)?);
            }
        },
        Commands::Info { cid: cid_input } => {
            // Resolve npub/repo or htree:// URLs to CID
            let resolved = resolve_cid_input(&cid_input).await?;
            let store = Arc::new(HashtreeStore::new(&data_dir)?);
            let fetcher = Fetcher::new(FetchConfig::default());
            let target_cid =
                resolve_info_target(&store, &fetcher, &resolved.cid, resolved.path.as_deref())
                    .await?;

            if !print_info_for_cid(&store, &target_cid).await? {
                println!("Hash not found: {}", format_cid_for_display(&target_cid));
            }
        }
        Commands::Stats { addr } => {
            let store = HashtreeStore::new(&data_dir)?;
            let stats = store.get_storage_stats()?;
            println!("Storage Statistics:");
            println!("  Stored objects: {}", stats.total_dags);
            println!("  Pinned items: {}", stats.pinned_dags);
            println!("  Total size: {}", format_bytes(stats.total_bytes));
            print_storage_inventory(&store, &data_dir)?;
            if let Some(status) = fetch_daemon_status_quietly(&addr).await {
                print_network_stats(&status);
            }
        }
        Commands::Status { addr } => {
            let url = format!("http://{}/api/status", addr);
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(3))
                .build()
                .context("Failed to build HTTP client")?;
            match client.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    let status: serde_json::Value = resp.json().await?;
                    println!("{}", format_daemon_status(&status, true));
                }
                Ok(resp) => {
                    eprintln!("Daemon returned error: {}", resp.status());
                }
                Err(err) if err.is_timeout() => {
                    eprintln!(
                        "Daemon at {} did not respond before the status timeout",
                        addr
                    );
                    eprintln!("Check daemon logs or try again after load subsides");
                }
                Err(_) => {
                    eprintln!("Daemon not running at {}", addr);
                    eprintln!("Start with: htree start");
                }
            }
        }
        Commands::Stop { pid_file } => {
            stop_daemon(pid_file.as_ref())?;
        }
        Commands::Reload { pid_file } => {
            reload_daemon(pid_file.as_ref())?;
        }
        Commands::Gc => {
            let store = HashtreeStore::new(&data_dir)?;
            println!("Running garbage collection...");
            let gc_stats = store.gc()?;
            println!("Deleted {} DAGs", gc_stats.deleted_dags);
            println!(
                "Freed {} bytes ({:.2} KB)",
                gc_stats.freed_bytes,
                gc_stats.freed_bytes as f64 / 1024.0
            );
        }
        Commands::User { identity } => {
            use hashtree_cli::config::get_keys_path;
            use nostr::nips::nip19::FromBech32;
            use std::fs;

            match identity {
                None => {
                    show_user_identity()?;
                }
                Some(id) => {
                    // Set identity - accept nsec or derive from input
                    let nsec = if id.starts_with("nsec1") {
                        // Validate it's a valid nsec
                        nostr::SecretKey::from_bech32(&id).context("Invalid nsec")?;
                        id
                    } else {
                        anyhow::bail!("Identity must be an nsec (secret key). Use 'htree user' to see your current npub.");
                    };

                    // Save to keys file
                    let keys_path = get_keys_path();
                    if let Some(parent) = keys_path.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::write(&keys_path, &nsec)?;

                    // Set permissions to 0600
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        fs::set_permissions(&keys_path, fs::Permissions::from_mode(0o600))?;
                    }

                    // Show the new npub
                    let secret_key = nostr::SecretKey::from_bech32(&nsec)?;
                    let keys = nostr::Keys::new(secret_key);
                    let npub = keys.public_key().to_bech32()?;
                    println!("{}", npub);
                }
            }
        }
        Commands::Publish {
            ref_name,
            hash,
            key,
        } => {
            use hashtree_core::{from_hex, key_from_hex, Cid};

            // Load config for relay list
            let config = Config::load()?;

            // Ensure nsec exists (generate if needed)
            let (nsec_str, was_generated) = ensure_keys_string()?;

            // Create Keys using nostr-sdk's version
            let keys = NostrKeys::parse(&nsec_str).context("Failed to parse nsec")?;
            let npub =
                NostrToBech32::to_bech32(&keys.public_key()).context("Failed to encode npub")?;

            if was_generated {
                println!("Identity: {} (new)", npub);
            }

            // Parse hash and optional key
            let hash_bytes = from_hex(&hash).context("Invalid hash (expected hex)")?;
            let key_bytes = key
                .as_ref()
                .map(|k| key_from_hex(k))
                .transpose()
                .map_err(|e| anyhow::anyhow!("Invalid key: {}", e))?;

            let cid = Cid {
                hash: hash_bytes,
                key: key_bytes,
            };

            // Create resolver config with secret key for publishing
            let resolver_config = NostrResolverConfig {
                relays: config.nostr.relays.clone(),
                resolve_timeout: Duration::from_secs(5),
                secret_key: Some(keys),
            };

            // Create resolver
            let resolver = NostrRootResolver::new(resolver_config)
                .await
                .context("Failed to create Nostr resolver")?;

            // Build Nostr key: "npub.../ref_name"
            let nostr_key = format!("{}/{}", npub, ref_name);

            // Publish
            match resolver.publish(&nostr_key, &cid).await {
                Ok(true) => {
                    println!("Published: {}", nostr_key);
                    println!("  hash: {}", hash);
                    if let Some(k) = key {
                        println!("  key:  {}", k);
                    }
                }
                Ok(false) => {
                    eprintln!("Publish failed: no relay accepted the event");
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("Publish failed: {}", e);
                    std::process::exit(1);
                }
            }

            // Clean up
            let _ = resolver.stop().await;
        }
        Commands::Release { command } => match command {
            ReleaseCommands::Publish {
                tree_name,
                version_path,
                cid,
                draft,
                local,
            } => {
                let published = publish_release_version(
                    &data_dir,
                    &tree_name,
                    &version_path,
                    &cid,
                    local,
                    draft,
                )
                .await?;

                println!(
                    "Published release: htree://{}/{}/{}",
                    published.npub, published.tree_name, published.version_path
                );
                println!("Release tree root: {}", published.root);
                if let Some(latest_path) = published.latest_path {
                    println!(
                        "Latest release:    htree://{}/{}/{}",
                        published.npub, published.tree_name, latest_path
                    );
                }
                if let Some(draft_path) = published.draft_path {
                    println!(
                        "Draft release:     htree://{}/{}/{}",
                        published.npub, published.tree_name, draft_path
                    );
                }
            }
        },
        Commands::Install {
            reference,
            to,
            check,
            download_only,
            current_version,
            target,
            manifest_path,
            kind,
            executable,
            archive_entry,
            only_if_newer,
        } => {
            super::update::run_install(
                &data_dir,
                reference,
                to,
                check,
                download_only,
                current_version,
                target,
                manifest_path,
                kind,
                executable,
                archive_entry,
                only_if_newer,
            )
            .await?;
        }
        Commands::Update { check, force } => {
            super::update::run_self_update(&data_dir, check, force).await?;
        }
        Commands::BgCheck => {
            super::update::run_bg_check(&data_dir).await?;
        }
        Commands::Follow { npub } => {
            follow_user(&data_dir, &npub, true).await?;
        }
        Commands::Unfollow { npub } => {
            follow_user(&data_dir, &npub, false).await?;
        }
        Commands::Mute { npub, reason } => {
            mute_user(&data_dir, &npub, reason.as_deref(), true).await?;
        }
        Commands::Unmute { npub } => {
            mute_user(&data_dir, &npub, None, false).await?;
        }
        Commands::Following => {
            list_following(&data_dir).await?;
        }
        Commands::Muted => {
            list_muted(&data_dir).await?;
        }
        Commands::Socialgraph { command } => match command {
            SocialGraphCommands::Filter {
                max_distance,
                overmute_threshold,
            } => {
                run_socialgraph_filter(data_dir, max_distance, overmute_threshold)?;
            }
            SocialGraphCommands::Stats => {
                run_socialgraph_stats(data_dir)?;
            }
            SocialGraphCommands::Warm {
                secs,
                crawl_depth,
                full_graph_recrawl,
                relays,
                author_batch_size,
                concurrent_batches,
            } => {
                run_socialgraph_warm(
                    data_dir,
                    secs,
                    crawl_depth,
                    full_graph_recrawl,
                    relays,
                    author_batch_size,
                    concurrent_batches,
                )
                .await?;
            }
            SocialGraphCommands::Snapshot {
                out,
                max_nodes,
                max_edges,
                max_distance,
                max_edges_per_node,
            } => {
                run_socialgraph_snapshot(
                    data_dir,
                    out,
                    max_nodes,
                    max_edges,
                    max_distance,
                    max_edges_per_node,
                )?;
            }
            SocialGraphCommands::RebuildProfileIndex => {
                run_socialgraph_rebuild_profile_index(data_dir).await?;
            }
            SocialGraphCommands::PublishProfileIndexes => {
                run_socialgraph_publish_profile_indexes(data_dir).await?;
            }
            SocialGraphCommands::RebuildEventIndex => {
                run_socialgraph_rebuild_event_index(data_dir).await?;
            }
            SocialGraphCommands::Index { options } => {
                let SocialGraphIndexArgs {
                    warm_secs,
                    crawl_depth,
                    full_graph_recrawl,
                    max_follow_distance,
                    max_authors,
                    max_authors_per_run,
                    max_live_mb,
                    per_author_event_limit,
                    per_author_kind_event_limit,
                    per_author_live_bytes,
                    author_batch_size,
                    checkpoint_authors,
                    index_commit_batch_size,
                    stage_only,
                    project_staged,
                    bulk_project_staged,
                    staging_data_dir,
                    projection_authors,
                    projection_event_limit,
                    projection_follow,
                    btree_order,
                    btree_update_concurrency,
                    concurrent_batches,
                    fetch_timeout_secs,
                    relay_event_max_bytes,
                    global_relay_scan,
                    full_author_history,
                    author_allowlist_url,
                    negentropy_only,
                    relay_page_size,
                    max_relay_pages,
                    max_events_seen,
                    kinds,
                    relays,
                } = *options;
                let config = Config::load()?;
                let effective_crawl_depth =
                    crawl_depth.unwrap_or(config.nostr.social_graph_crawl_depth);
                let effective_max_follow_distance =
                    max_follow_distance.or(Some(config.nostr.social_graph_crawl_depth));
                run_socialgraph_index_from_cli(
                    data_dir,
                    SocialGraphIndexOptions {
                        warm_graph_for: Duration::from_secs(warm_secs),
                        graph_crawl_depth: effective_crawl_depth,
                        full_graph_recrawl,
                        max_events_seen,
                        max_authors,
                        max_authors_per_run,
                        max_follow_distance: effective_max_follow_distance,
                        max_live_bytes: max_live_mb.saturating_mul(1024 * 1024),
                        author_batch_size,
                        checkpoint_authors,
                        index_commit_batch_size,
                        stage_only,
                        project_staged,
                        bulk_project_staged,
                        staging_data_dir,
                        projection_authors,
                        projection_event_limit,
                        projection_follow,
                        btree_order,
                        btree_update_concurrency,
                        concurrent_batches,
                        per_author_event_limit,
                        per_author_kind_event_limit,
                        per_author_live_bytes,
                        fetch_timeout: Duration::from_secs(fetch_timeout_secs),
                        relay_event_max_bytes,
                        global_relay_scan,
                        full_author_history,
                        author_allowlist_url,
                        negentropy_only,
                        relay_page_size,
                        max_relay_pages,
                        kinds: (!kinds.is_empty()).then_some(kinds),
                        relays: (!relays.is_empty()).then_some(relays),
                    },
                )
                .await?;
            }
        },
        Commands::Profile {
            name,
            about,
            picture,
        } => {
            update_profile(name, about, picture).await?;
        }
        Commands::Push {
            cid: cid_input,
            server,
            force,
            shallow,
        } => {
            // Resolve npub/repo or htree:// URLs to CID
            let resolved = resolve_cid_input(&cid_input).await?;
            let cid = resolved.cid.to_string();
            push_to_blossom(&data_dir, &cid, server, force, shallow).await?;
        }
        Commands::Storage { command } => {
            // The migration launcher is an inert rendezvous until its
            // invocation-bound request has been durably acknowledged. In
            // particular, do not call Config::load here: it creates a default
            // config file when one is absent. A controlled migration must
            // therefore name its data directory explicitly and dispatch
            // before any config read/create path.
            let command = match command {
                StorageCommands::Pool {
                    command: PoolCommands::LaunchMigrateLmdbV3(arguments),
                } => {
                    let PoolMigrationControllerArgs {
                        preflight,
                        rollout_dir,
                        rollout_id,
                        phase,
                        controller_executable,
                        controller_systemd_unit,
                        controller_systemd_fragment,
                        controller_systemd_environment_file,
                        controller_state_input,
                        source_baseline_input,
                        pool_topology_input,
                        additional_cas,
                        writer_units,
                        systemd_unit,
                        systemctl,
                        systemd_fragment,
                        systemd_environment_file,
                        service_gid,
                        migration_binary,
                        target_data_dir,
                        pool,
                        source,
                        source_external_dir,
                        state_file,
                        batch_size,
                        max_buffer_mib,
                        source_read_concurrency,
                        reopen_batches,
                        max_items,
                        launch_request_wait_seconds,
                        acknowledgement_wait_seconds,
                    } = *arguments;
                    run_pool_migration_controller(PoolMigrationControllerOptions {
                        preflight,
                        rollout_dir,
                        rollout_id,
                        phase,
                        controller_executable,
                        controller_systemd_unit,
                        controller_systemd_fragment,
                        controller_systemd_environment_file,
                        controller_state_input,
                        source_baseline_input,
                        pool_topology_input,
                        additional_cas,
                        writer_units,
                        systemd_unit,
                        systemctl,
                        systemd_fragment,
                        systemd_environment_file,
                        service_gid,
                        migration_binary,
                        target_data_dir,
                        pool,
                        source,
                        source_external_dir,
                        state_file,
                        batch_size,
                        max_buffer_mib,
                        source_read_concurrency,
                        reopen_batches,
                        max_items,
                        launch_request_wait: Duration::from_secs(launch_request_wait_seconds),
                        acknowledgement_wait: Duration::from_secs(acknowledgement_wait_seconds),
                    })?;
                    return Ok(());
                }
                StorageCommands::Pool {
                    command: command @ PoolCommands::MigrateLmdb { .. },
                } => {
                    let data_dir = cli.data_dir.clone().context(
                        "controlled Pool migration requires an explicit --data-dir before storage pool migrate-lmdb",
                    )?;
                    if !data_dir.is_absolute() {
                        bail!("controlled Pool migration --data-dir must be absolute");
                    }
                    run_pool_command(&data_dir, command)?;
                    return Ok(());
                }
                command => command,
            };

            // Load config
            let config = Config::load()?;

            // Use CLI data_dir if provided, otherwise use config's data_dir
            let data_dir = cli
                .data_dir
                .clone()
                .unwrap_or_else(|| PathBuf::from(&config.storage.data_dir));

            match command {
                StorageCommands::TrimLmdb { env_dir, max_gb } => {
                    #[cfg(feature = "lmdb")]
                    {
                        use hashtree_core::store::Store;
                        use hashtree_lmdb::LmdbBlobStore;

                        let env_dir = if env_dir.is_absolute() {
                            env_dir
                        } else {
                            data_dir.join(env_dir)
                        };
                        let max_bytes = max_gb.saturating_mul(1024 * 1024 * 1024);
                        let file_bytes_before = std::fs::metadata(env_dir.join("data.mdb"))
                            .map(|metadata| metadata.len())
                            .unwrap_or(0);
                        let blob_store = LmdbBlobStore::with_max_bytes(&env_dir, max_bytes)?;
                        let freed = blob_store.evict_if_needed().await?;
                        let stats = blob_store.stats()?;

                        println!("Trimmed {}", env_dir.display());
                        println!(
                            "  File bytes before: {} ({:.2} GB)",
                            file_bytes_before,
                            file_bytes_before as f64 / 1024.0 / 1024.0 / 1024.0
                        );
                        println!(
                            "  Logical bytes after: {} ({:.2} GB)",
                            stats.total_bytes,
                            stats.total_bytes as f64 / 1024.0 / 1024.0 / 1024.0
                        );
                        println!(
                            "  Logical limit: {} ({:.2} GB)",
                            max_bytes,
                            max_bytes as f64 / 1024.0 / 1024.0 / 1024.0
                        );
                        println!(
                            "  Additional bytes freed on explicit pass: {} ({:.2} GB)",
                            freed,
                            freed as f64 / 1024.0 / 1024.0 / 1024.0
                        );
                        println!("  Blobs remaining: {}", stats.count);
                    }
                    #[cfg(not(feature = "lmdb"))]
                    {
                        let _ = (env_dir, max_gb);
                        anyhow::bail!("LMDB support not enabled in this build");
                    }
                }
                StorageCommands::Stats => {
                    let max_size_bytes = config.storage.max_size_gb * 1024 * 1024 * 1024;
                    let store = HashtreeStore::with_options(
                        &data_dir,
                        config.storage.s3.as_ref(),
                        max_size_bytes,
                    )?;
                    let stats = store.get_storage_stats()?;
                    let tracked = store.tracked_size()?;
                    let trees = store.list_indexed_trees()?;

                    println!("Storage Statistics:");
                    println!(
                        "  Max size:     {} GB ({} bytes)",
                        config.storage.max_size_gb, max_size_bytes
                    );
                    println!("  Total size:   {}", format_bytes(stats.total_bytes));
                    println!(
                        "  Tracked:      {} ({:.2} GB)",
                        tracked,
                        tracked as f64 / 1024.0 / 1024.0 / 1024.0
                    );
                    println!("  Stored objects: {}", stats.total_dags);
                    println!("  Pinned items: {}", stats.pinned_dags);
                    println!("  Indexed trees: {}", trees.len());
                    print_storage_inventory(&store, &data_dir)?;

                    let utilization = if max_size_bytes > 0 {
                        (tracked as f64 / max_size_bytes as f64) * 100.0
                    } else {
                        0.0
                    };
                    println!();
                    println!("Utilization: {:.1}%", utilization);
                }
                StorageCommands::Trees => {
                    use hashtree_core::to_hex;
                    let max_size_bytes = config.storage.max_size_gb * 1024 * 1024 * 1024;
                    let store = HashtreeStore::with_options(
                        &data_dir,
                        config.storage.s3.as_ref(),
                        max_size_bytes,
                    )?;
                    let trees = store.list_indexed_trees()?;

                    if trees.is_empty() {
                        println!("No indexed trees");
                    } else {
                        println!("Indexed trees ({}):", trees.len());
                        for (root_hash, meta) in trees {
                            let root_hex = to_hex(&root_hash);
                            let priority_str = match meta.priority {
                                255 => "own",
                                128 => "followed",
                                _ => "other",
                            };
                            let name = meta.name.as_deref().unwrap_or("<unnamed>");
                            let synced = chrono_humanize_timestamp(meta.synced_at);
                            println!(
                                "  {}... {} ({}) - {} - {} - synced {}",
                                &root_hex[..12],
                                name,
                                priority_str,
                                &meta.owner[..12.min(meta.owner.len())],
                                format_bytes(meta.total_size),
                                synced
                            );
                        }
                    }
                }
                StorageCommands::Evict => {
                    let max_size_bytes = config.storage.max_size_gb * 1024 * 1024 * 1024;
                    let store = HashtreeStore::with_options(
                        &data_dir,
                        config.storage.s3.as_ref(),
                        max_size_bytes,
                    )?;
                    println!("Running eviction...");
                    let freed = store.evict_if_needed()?;
                    if freed > 0 {
                        println!(
                            "Evicted {} bytes ({:.2} MB)",
                            freed,
                            freed as f64 / 1024.0 / 1024.0
                        );
                    } else {
                        println!("No eviction needed (storage under limit)");
                    }
                }
                StorageCommands::Compact {
                    env_dirs,
                    scratch_dir,
                    keep_backup,
                } => {
                    let results = hashtree_cli::storage::compact_lmdb_environments_under(
                        &data_dir,
                        &env_dirs,
                        scratch_dir.as_deref(),
                        keep_backup,
                    )?;
                    if results.is_empty() {
                        println!("No LMDB environments found under {}", data_dir.display());
                    } else {
                        for result in &results {
                            let saved_bytes =
                                result.before_bytes.saturating_sub(result.after_bytes);
                            println!(
                                "{}: {} -> {} bytes (saved {} / {:.2} GB)",
                                result.env_dir.display(),
                                result.before_bytes,
                                result.after_bytes,
                                saved_bytes,
                                saved_bytes as f64 / 1024.0 / 1024.0 / 1024.0,
                            );
                            if let Some(backup_path) = &result.backup_path {
                                println!("  recovery backup: {}", backup_path.display());
                            }
                        }
                    }
                }
                StorageCommands::RetainNostrRoot { state_file, apply } => {
                    let state_bytes = std::fs::read(&state_file).with_context(|| {
                        format!("read durable crawl state {}", state_file.display())
                    })?;
                    let state: serde_json::Value = serde_json::from_slice(&state_bytes)
                        .with_context(|| {
                            format!("parse durable crawl state {}", state_file.display())
                        })?;
                    let root_text = state
                        .get("root")
                        .and_then(serde_json::Value::as_str)
                        .filter(|root| !root.is_empty())
                        .with_context(|| {
                            format!("durable crawl state {} has no root", state_file.display())
                        })?;
                    let root = if root_text.starts_with("nhash1") {
                        let decoded =
                            nhash_decode(root_text).context("decode durable root nhash")?;
                        Cid {
                            hash: decoded.hash,
                            key: decoded.decrypt_key,
                        }
                    } else {
                        Cid::parse(root_text).context("parse durable root CID")?
                    };
                    // This closed-writer reachability operation must not invoke
                    // quota eviction while opening an intentionally large index.
                    let store =
                        HashtreeStore::with_options(&data_dir, config.storage.s3.as_ref(), 0)?;
                    if !apply {
                        println!("Dry run; pass --apply to delete unreachable blobs.");
                    }
                    let report = store.retain_nostr_root(&root, apply)?;
                    println!("Retained Nostr root from {}", state_file.display());
                    println!("  Stored hashes: {}", report.total_hashes);
                    println!("  Reachable hashes: {}", report.reachable_hashes);
                    println!("  Explicitly pinned hashes: {}", report.pinned_hashes);
                    println!("  Unreachable hashes: {}", report.candidate_hashes);
                    println!("  Deleted hashes: {}", report.deleted_hashes);
                    println!(
                        "  Logical bytes: {} -> {}",
                        report.logical_bytes_before, report.logical_bytes_after
                    );
                }
                StorageCommands::Verify { delete, r2 } => {
                    let max_size_bytes = config.storage.max_size_gb * 1024 * 1024 * 1024;
                    let store = HashtreeStore::with_options(
                        &data_dir,
                        config.storage.s3.as_ref(),
                        max_size_bytes,
                    )?;
                    println!("Verifying blob integrity...");
                    if !delete {
                        println!(
                            "(dry-run mode - use --delete to actually remove corrupted entries)"
                        );
                    }
                    println!();

                    // Verify LMDB
                    let lmdb_result = store.verify_lmdb_integrity(delete)?;
                    println!("LMDB verification:");
                    println!("  Total blobs:     {}", lmdb_result.total);
                    println!("  Valid:           {}", lmdb_result.valid);
                    println!("  Corrupted:       {}", lmdb_result.corrupted);
                    if delete {
                        println!("  Deleted:         {}", lmdb_result.deleted);
                    }
                    println!();

                    // Verify R2 if requested
                    if r2 {
                        println!("Verifying R2 storage (this may take a while)...");
                        match store.verify_r2_integrity(delete).await {
                            Ok(r2_result) => {
                                println!("R2 verification:");
                                println!("  Total objects:   {}", r2_result.total);
                                println!("  Valid:           {}", r2_result.valid);
                                println!("  Corrupted:       {}", r2_result.corrupted);
                                if delete {
                                    println!("  Deleted:         {}", r2_result.deleted);
                                }
                            }
                            Err(e) => {
                                println!("R2 verification failed: {}", e);
                            }
                        }
                    }

                    let total_corrupted = lmdb_result.corrupted;
                    if total_corrupted > 0 {
                        println!();
                        if delete {
                            println!(
                                "Cleanup complete. Removed {} corrupted entries.",
                                total_corrupted
                            );
                        } else {
                            println!(
                                "Found {} corrupted entries. Run with --delete to remove them.",
                                total_corrupted
                            );
                        }
                    } else {
                        println!("All blobs verified successfully!");
                    }
                }
                StorageCommands::ImportR2 {
                    concurrency,
                    check_only,
                    resume,
                    fast_list,
                    stream_merge,
                    keys,
                    keys_file,
                    start_after,
                    scan_prefix,
                    state_file,
                    max_objects,
                    progress_every,
                    scan_delay_ms,
                } => {
                    let max_size_bytes = config.storage.max_size_gb * 1024 * 1024 * 1024;
                    let store = HashtreeStore::with_options(
                        &data_dir,
                        config.storage.s3.as_ref(),
                        max_size_bytes,
                    )?;
                    #[cfg(feature = "s3")]
                    {
                        let result = store
                            .import_r2_to_local(hashtree_cli::storage::R2ImportOptions {
                                concurrency,
                                check_only,
                                resume,
                                fast_list,
                                stream_merge,
                                keys,
                                keys_file,
                                start_after,
                                scan_prefix,
                                state_file,
                                max_objects,
                                progress_every,
                                scan_delay_ms,
                            })
                            .await?;
                        println!(
                            "R2 import complete: {} listed, {} skipped, {} missing, {} imported, {} corrupted, {} failed, {:.2} GB imported",
                            result.listed,
                            result.skipped,
                            result.missing,
                            result.imported,
                            result.corrupted,
                            result.failed,
                            result.bytes_imported as f64 / 1024.0 / 1024.0 / 1024.0,
                        );
                    }
                    #[cfg(not(feature = "s3"))]
                    {
                        let _ = (
                            concurrency,
                            check_only,
                            resume,
                            fast_list,
                            stream_merge,
                            keys,
                            keys_file,
                            start_after,
                            scan_prefix,
                            state_file,
                            max_objects,
                            progress_every,
                            scan_delay_ms,
                            store,
                        );
                        anyhow::bail!("R2 import requires building htree with the s3 feature");
                    }
                }
                StorageCommands::Pool { command } => {
                    run_pool_command(&data_dir, command)?;
                }
            }
        }
        Commands::Peer { addr } => {
            list_peers(&addr).await?;
        }
        Commands::Cashu { command } => {
            run_cashu_helper(&data_dir, &command)?;
        }
        Commands::Pr { command } => match command {
            PrCommands::Create {
                repo,
                title,
                description,
                branch,
                target_branch,
                clone_url,
            } => {
                super::pr::create_pr(
                    repo.as_deref(),
                    &title,
                    description.as_deref(),
                    branch.as_deref(),
                    &target_branch,
                    clone_url.as_deref(),
                )
                .await?;
            }
            PrCommands::List { repo, state } => {
                super::pr::list_prs(repo.as_deref(), state).await?;
            }
        },
        Commands::Repos { owner } => {
            super::repos::list_repos(owner.as_deref()).await?;
        }
    }

    Ok(())
}

fn run_pool_command(data_dir: &Path, command: PoolCommands) -> Result<()> {
    #[cfg(feature = "lmdb")]
    {
        use hashtree_lmdb::{
            PoolMemberConfig, PoolMemberId, PoolMemberRuntimePaths, PoolStore, PoolStoreConfig,
            SHARED_BLOB_POOL_DIR_NAME,
        };

        let pool_path = data_dir.join(SHARED_BLOB_POOL_DIR_NAME);
        let open_existing = || -> Result<PoolStore> {
            if !pool_path.join("data.mdb").exists() {
                bail!(
                    "no storage pool exists at {}; add a member first",
                    pool_path.display()
                );
            }
            PoolStore::open(&pool_path, PoolStoreConfig::default()).map_err(Into::into)
        };
        let resolve_path = |path: PathBuf| {
            if path.is_absolute() {
                path
            } else {
                data_dir.join(path)
            }
        };
        let gib = 1024u64 * 1024 * 1024;

        match command {
            PoolCommands::Status => {
                let pool = open_existing()?;
                let members = pool.members()?;
                println!("Storage pool: {}", pool_path.display());
                println!("Members: {}", members.len());
                for member in members {
                    println!("  {}", member.id);
                    println!("    state: {:?}", member.state);
                    println!("    path: {}", member.path.display());
                    println!(
                        "    usage: {} / {} bytes ({} blobs)",
                        member.logical_bytes, member.capacity_bytes, member.located_blobs
                    );
                    println!("    LMDB map: {} bytes", member.map_size_bytes);
                    if let Some(external) = member.external_blob_dir {
                        println!("    external path: {}", external.display());
                        println!(
                            "    external threshold: {} bytes, sync: {}, pack target: {:?}",
                            member.external_blob_min_bytes.unwrap_or(0),
                            member.external_blob_sync,
                            member.external_pack_target_bytes
                        );
                    }
                    println!(
                        "    concurrency: {} reads, {} writes per process",
                        member.max_read_concurrency, member.max_write_concurrency
                    );
                    println!(
                        "    temperature watermarks: {}% low / {}% high",
                        member.temperature_low_watermark_percent,
                        member.temperature_high_watermark_percent
                    );
                    println!("    available: {}", member.available);
                    if let Some(error) = member.last_error {
                        println!("    error: {error}");
                    }
                }
            }
            PoolCommands::Add {
                path,
                capacity_gb,
                map_size_gb,
                external_dir,
                external_min_bytes,
                external_pack_mib,
                external_no_sync,
                max_reads,
                max_writes,
                temperature_high_percent,
                temperature_low_percent,
            } => {
                if capacity_gb == 0 || max_reads == 0 || max_writes == 0 {
                    bail!("capacity and concurrency limits must be non-zero");
                }
                let pool = PoolStore::open(&pool_path, PoolStoreConfig::default())?;
                let capacity_bytes = capacity_gb.saturating_mul(gib);
                let mut member = PoolMemberConfig::new(resolve_path(path), capacity_bytes)
                    .with_map_size_bytes(map_size_gb.unwrap_or(capacity_gb).saturating_mul(gib));
                member.max_read_concurrency = max_reads;
                member.max_write_concurrency = max_writes;
                member = member
                    .with_temperature_watermarks(temperature_low_percent, temperature_high_percent);
                if let Some(external_dir) = external_dir {
                    member = member.with_external_blobs(
                        resolve_path(external_dir),
                        external_min_bytes,
                        !external_no_sync,
                        external_pack_mib.map(|mib| mib.saturating_mul(1024 * 1024)),
                    );
                }
                let id = pool.add_member(member)?;
                println!("Added pool member {id}");
            }
            PoolCommands::Configure {
                id,
                capacity_gb,
                max_reads,
                max_writes,
                temperature_high_percent,
                temperature_low_percent,
            } => {
                let pool = open_existing()?;
                let id: PoolMemberId = id.parse()?;
                let member = pool.member(id)?;
                pool.update_member_limits(
                    id,
                    capacity_gb
                        .map(|value| value.saturating_mul(gib))
                        .unwrap_or(member.capacity_bytes),
                    max_reads.unwrap_or(member.max_read_concurrency),
                    max_writes.unwrap_or(member.max_write_concurrency),
                )?;
                if temperature_high_percent.is_some() || temperature_low_percent.is_some() {
                    pool.update_member_temperature_watermarks(
                        id,
                        temperature_low_percent.unwrap_or(member.temperature_low_watermark_percent),
                        temperature_high_percent
                            .unwrap_or(member.temperature_high_watermark_percent),
                    )?;
                }
                println!("Updated pool member {id}");
            }
            PoolCommands::Drain { id } => {
                let pool = open_existing()?;
                let id: PoolMemberId = id.parse()?;
                pool.begin_drain(id)?;
                println!("Pool member {id} is draining");
            }
            PoolCommands::Maintain {
                max_items,
                batch_items,
            } => {
                let pool = open_existing()?;
                let report = pool.maintain_with_batch_items(max_items, batch_items)?;
                println!(
                    "Examined {}, moved {} blobs / {} bytes, {} failures",
                    report.examined,
                    report.moved,
                    report.bytes_moved,
                    report.failed.len()
                );
                for failure in &report.failed {
                    eprintln!("  {failure}");
                }
                if !report.failed.is_empty() {
                    bail!("pool maintenance completed with failures");
                }
            }
            PoolCommands::BalanceTemperature {
                max_moves,
                max_bytes_gb,
                max_concurrency,
            } => {
                let mut config = PoolStoreConfig::default();
                if let Some(max_moves) = max_moves {
                    config.temperature.max_moves_per_cycle = max_moves;
                }
                if let Some(max_bytes_gb) = max_bytes_gb {
                    config.temperature.max_bytes_per_cycle = max_bytes_gb.saturating_mul(gib);
                }
                if let Some(max_concurrency) = max_concurrency {
                    config.temperature.max_concurrent_moves = max_concurrency;
                }
                let pool = PoolStore::open(&pool_path, config)?;
                let report = pool.balance_temperature()?;
                println!(
                    "Scanned {}, considered {} candidates, attempted {} / moved {} blobs and {} bytes (peak concurrency {}, resumed {}, throttled {}, lease {})",
                    report.scanned,
                    report.candidates,
                    report.attempted_moves,
                    report.moved,
                    report.bytes_moved,
                    report.peak_concurrent_moves,
                    report.resumed,
                    report.throttled,
                    report.lease_acquired
                );
                for failure in &report.failed {
                    eprintln!("  {failure}");
                }
                if !report.failed.is_empty() {
                    bail!("pool temperature balance completed with failures");
                }
            }
            PoolCommands::MigrateLmdb {
                launch_request,
                launch_request_wait_seconds,
                source,
                source_external_dir,
                state_file,
                batch_size,
                max_buffer_mib,
                source_read_concurrency,
                reopen_batches,
                max_items,
                resume,
            } => {
                use hashtree_lmdb::{
                    migrate_lmdb_hashes_with_max_buffer_bytes_and_authorizer, ExternalBlobOptions,
                    LmdbBlobReader, PoolMigrationAuditStore,
                };

                if batch_size == 0
                    || max_buffer_mib == 0
                    || source_read_concurrency == 0
                    || reopen_batches == 0
                    || max_items == Some(0)
                {
                    bail!(
                        "migration batch size, max buffer MiB, source read concurrency, reopen batches, and max items must be non-zero"
                    );
                }
                validate_source_read_concurrency(source_read_concurrency)?;
                let max_buffer_bytes = max_buffer_mib
                    .checked_mul(1024 * 1024)
                    .and_then(|bytes| usize::try_from(bytes).ok())
                    .context("migration max buffer MiB is too large for this platform")?;
                let source = resolve_path(source);
                let state_file = resolve_path(state_file);
                let source_external_dir = source_external_dir.map(resolve_path);
                let launch = acknowledge_pool_migration_launch(PoolMigrationLaunchContext {
                    launch_request: &launch_request,
                    source: &source,
                    source_external_dir: source_external_dir.as_deref(),
                    pool: &pool_path,
                    state_file: &state_file,
                    resume,
                    max_items,
                    request_wait: Duration::from_secs(launch_request_wait_seconds),
                })?;
                if launch.final_stopped_pass && reopen_batches > MAX_FINAL_REOPEN_BATCHES {
                    bail!(
                        "stopped final migration requires --reopen-batches <= {MAX_FINAL_REOPEN_BATCHES} for bounded fence revalidation"
                    );
                }
                validate_stopped_final_batch_size(launch.final_stopped_pass, batch_size)?;
                if launch.final_stopped_source_pass {
                    run_final_stopped_source_audit(
                        &launch,
                        batch_size,
                        max_buffer_bytes,
                        source_read_concurrency,
                        reopen_batches,
                    )?;
                    println!("Cursor: {}", state_file.display());
                    return Ok(());
                }
                if launch.final_stopped_full_pass {
                    run_final_stopped_full_reconciliation(
                        &launch,
                        batch_size,
                        max_buffer_bytes,
                        source_read_concurrency,
                        reopen_batches,
                    )?;
                    println!("Cursor: {}", state_file.display());
                    return Ok(());
                }
                launch.ensure_store_paths()?;
                let online_audit_path = launch.online_audit_path()?;
                if online_audit_path.canonicalize().with_context(|| {
                    format!(
                        "canonicalize online migration audit directory {}",
                        online_audit_path.display()
                    )
                })? != online_audit_path
                {
                    bail!("online migration audit directory is not an exact canonical path");
                }
                let audit_metadata = std::fs::symlink_metadata(&online_audit_path)
                    .context("inspect online migration audit directory")?;
                if !audit_metadata.file_type().is_dir() {
                    bail!("online migration audit path is not a directory");
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    let service_gid = unsafe { libc::getegid() };
                    if audit_metadata.uid() != 0
                        || audit_metadata.gid() != service_gid
                        || audit_metadata.mode() & 0o7777 != 0o750
                    {
                        bail!(
                            "online migration audit directory must be root-owned by the service GID with mode 0750"
                        );
                    }
                    for (name, mode) in [("data.mdb", 0o640), ("lock.mdb", 0o660)] {
                        let metadata = std::fs::symlink_metadata(online_audit_path.join(name))
                            .with_context(|| format!("inspect online audit {name}"))?;
                        if !metadata.file_type().is_file()
                            || metadata.nlink() != 1
                            || metadata.uid() != 0
                            || metadata.gid() != service_gid
                            || metadata.mode() & 0o7777 != mode
                        {
                            bail!("online audit {name} differs from root-owned authority");
                        }
                    }
                }
                let online_audit = PoolMigrationAuditStore::open_read_only(
                    &online_audit_path,
                    launch.online_audit_binding()?,
                )
                .map_err(anyhow::Error::from)?;
                let mut cursor = launch.cursor;
                let mut verified = 0usize;
                let mut scanned = 0usize;
                let mut already_present = 0usize;
                let mut audit_reused = 0usize;
                let mut target_audit_reused = 0usize;
                let mut inserted = 0usize;
                let mut inserted_bytes = 0u64;
                let mut source_completed = false;
                'mapping_epochs: loop {
                    if max_items.is_some_and(|maximum| scanned >= maximum) {
                        break;
                    }

                    launch.ensure_store_paths()?;
                    launch.ensure_final_writer_fence()?;
                    let mut pool_config = PoolStoreConfig::default();
                    pool_config.temperature.enabled = false;
                    pool_config.catalog_lmdb_identity = Some(launch.pool_catalog_lmdb_identity());
                    pool_config.expected_manifest_sha256 = Some(launch.pool_manifest_sha256());
                    pool_config.member_runtime_paths = launch
                        .pool_member_runtime_paths()
                        .into_iter()
                        .map(|paths| {
                            Ok(PoolMemberRuntimePaths {
                                id: paths.id.parse()?,
                                configured_path: paths.configured_path,
                                runtime_path: paths.runtime_path,
                                configured_external_path: paths.configured_external_path,
                                runtime_external_path: paths.runtime_external_path,
                                lmdb_identity: paths.lmdb_identity,
                            })
                        })
                        .collect::<Result<Vec<_>>>()?;
                    let pool = PoolStore::open(launch.pool(), pool_config.clone())?;
                    let external = launch.source_external().map(|path| ExternalBlobOptions {
                        base_path: path.to_path_buf(),
                        min_bytes: 1,
                        sync: true,
                        pack_target_bytes: None,
                    });
                    let reader =
                        LmdbBlobReader::open_with_external_read_concurrency_and_pinned_identity(
                            launch.source(),
                            external,
                            source_read_concurrency,
                            launch.source_lmdb_identity(),
                        )?;
                    let mut epoch_batches = 0usize;

                    loop {
                        let remaining = max_items
                            .map(|maximum| maximum.saturating_sub(scanned))
                            .unwrap_or(batch_size);
                        if remaining == 0 {
                            break 'mapping_epochs;
                        }
                        let limit = batch_size.min(remaining);
                        launch.ensure_final_writer_masks()?;
                        let mut authorize_target_write = |batch_cursor, write_count| {
                            launch
                                .authorize_checkpoint(
                                    "migration-batch",
                                    batch_cursor,
                                    Some(write_count),
                                )
                                .map_err(|error| {
                                    hashtree_core::store::StoreError::Other(format!(
                                        "root checkpoint rejected target mutation: {error:#}"
                                    ))
                                })
                        };
                        let hashes = reader.scan_hashes_after(cursor, limit)?;
                        if hashes.is_empty() {
                            launch.ensure_checkpoint_broker_alive()?;
                            launch.ensure_final_writer_masks()?;
                            if let Some((missing, size)) =
                                online_audit.first_unverified_source(&reader, batch_size)?
                            {
                                launch.reset_online_cursor()?;
                                cursor = None;
                                println!(
                                    "Online audit catch-up: source {} / {} bytes is not yet in the durable verified set; restarting the metadata scan",
                                    hashtree_core::to_hex(&missing),
                                    size,
                                );
                                if max_items.is_some_and(|maximum| scanned >= maximum) {
                                    break 'mapping_epochs;
                                }
                                break;
                            }
                            source_completed = true;
                            break 'mapping_epochs;
                        }
                        let sizes = reader.sizes_for_sorted_hashes(&hashes)?;
                        let source_entries = hashes.iter().copied().zip(sizes).collect::<Vec<_>>();
                        let covered = online_audit.contains_source_exact_sorted(&source_entries)?;
                        let missing_hashes = source_entries
                            .iter()
                            .zip(&covered)
                            .filter_map(|((hash, _), present)| (!present).then_some(*hash))
                            .collect::<Vec<_>>();
                        let mut batch = migrate_lmdb_hashes_with_max_buffer_bytes_and_authorizer(
                            &reader,
                            &pool,
                            &missing_hashes,
                            cursor,
                            max_buffer_bytes,
                            &mut authorize_target_write,
                        )?;
                        batch.scanned = hashes.len();
                        batch.last_hash = hashes.last().copied();
                        batch.source_entries = source_entries;
                        launch.ensure_checkpoint_broker_alive()?;
                        launch.ensure_final_writer_masks()?;
                        scanned = scanned.saturating_add(batch.scanned);
                        audit_reused = audit_reused
                            .saturating_add(covered.iter().filter(|present| **present).count());
                        already_present = already_present.saturating_add(batch.already_present);
                        verified = verified.saturating_add(batch.verified);
                        inserted = inserted.saturating_add(batch.inserted);
                        inserted_bytes = inserted_bytes.saturating_add(batch.inserted_bytes);
                        cursor = batch.last_hash;
                        let cursor_hash = cursor.expect("non-empty migration batch has a cursor");
                        if !batch.verified_source_entries.is_empty() {
                            pool.validate_controlled_authority_and_sync()?;
                            launch.authorize_online_source_audit_batch(
                                cursor_hash,
                                batch.scanned,
                                &batch.verified_source_entries,
                            )?;
                        }
                        launch.ensure_store_paths()?;
                        launch.write_cursor(&hashtree_core::to_hex(&cursor_hash))?;
                        println!(
                            "Migration batch: scanned {}, audit reused {}, already present {}, verified {}, writes {}, peak buffered {} bytes, scan {} us, catalog probe {} us, source read {} us, source verify {} us, target write {} us",
                            batch.scanned,
                            covered.iter().filter(|present| **present).count(),
                            batch.already_present,
                            batch.verified,
                            batch.write_batches,
                            batch.peak_buffered_bytes,
                            batch.scan_micros,
                            batch.catalog_probe_micros,
                            batch.source_read_micros,
                            batch.source_verify_micros,
                            batch.target_write_micros,
                        );
                        epoch_batches = epoch_batches.saturating_add(1);
                        if epoch_batches >= reopen_batches {
                            println!(
                                "Migration mappings reopened after {epoch_batches} batches at cursor {}",
                                hashtree_core::to_hex(&cursor_hash)
                            );
                            break;
                        }
                    }
                }
                let mut target_completed = false;
                if source_completed && max_items.is_none_or(|maximum| scanned < maximum) {
                    let require_terminal_target = launch.target_writers_fenced();
                    let mut target_cursor = online_audit.target_cursor()?;
                    'target_epochs: loop {
                        if max_items.is_some_and(|maximum| scanned >= maximum) {
                            break;
                        }
                        launch.ensure_store_paths()?;
                        launch.ensure_final_writer_fence()?;
                        let mut pool_config = PoolStoreConfig::default();
                        pool_config.temperature.enabled = false;
                        pool_config.catalog_lmdb_identity =
                            Some(launch.pool_catalog_lmdb_identity());
                        pool_config.expected_manifest_sha256 = Some(launch.pool_manifest_sha256());
                        pool_config.member_runtime_paths = launch
                            .pool_member_runtime_paths()
                            .into_iter()
                            .map(|paths| {
                                Ok(PoolMemberRuntimePaths {
                                    id: paths.id.parse()?,
                                    configured_path: paths.configured_path,
                                    runtime_path: paths.runtime_path,
                                    configured_external_path: paths.configured_external_path,
                                    runtime_external_path: paths.runtime_external_path,
                                    lmdb_identity: paths.lmdb_identity,
                                })
                            })
                            .collect::<Result<Vec<_>>>()?;
                        let pool = PoolStore::open(launch.pool(), pool_config)?;
                        let mut epoch_batches = 0usize;
                        loop {
                            let remaining = max_items
                                .map(|maximum| maximum.saturating_sub(scanned))
                                .unwrap_or(batch_size);
                            if remaining == 0 {
                                break 'target_epochs;
                            }
                            let limit = batch_size.min(remaining);
                            launch.ensure_final_writer_masks()?;
                            let page = pool.scan_catalog_locations_after(target_cursor, limit)?;
                            if page.is_empty() {
                                launch.ensure_checkpoint_broker_alive()?;
                                launch.ensure_final_writer_masks()?;
                                if let Some((missing, size)) = first_unverified_pool_target(
                                    &online_audit,
                                    &pool,
                                    batch_size,
                                    require_terminal_target,
                                )? {
                                    launch.reset_online_target_audit_cursor()?;
                                    target_cursor = None;
                                    println!(
                                        "Online target audit catch-up: Pool {} / {} bytes is not yet in the root-owned target proof set; restarting the catalog scan",
                                        hashtree_core::to_hex(&missing),
                                        size,
                                    );
                                    break;
                                }
                                target_completed = true;
                                break 'target_epochs;
                            }
                            let stored_entries = page
                                .iter()
                                .filter_map(|(hash, location)| match location {
                                    hashtree_lmdb::PoolCatalogLocation::Stored { size, .. } => {
                                        Some(Ok((*hash, *size)))
                                    }
                                    hashtree_lmdb::PoolCatalogLocation::Pending { .. } => {
                                        require_terminal_target.then(|| {
                                            bail!(
                                                "target-fenced online audit found pending Pool location {}",
                                                hashtree_core::to_hex(hash)
                                            )
                                        })
                                    }
                                    hashtree_lmdb::PoolCatalogLocation::Moving { .. } => {
                                        require_terminal_target.then(|| {
                                            bail!(
                                                "target-fenced online audit found moving Pool location {}",
                                                hashtree_core::to_hex(hash)
                                            )
                                        })
                                    }
                                    hashtree_lmdb::PoolCatalogLocation::Missing => {
                                        require_terminal_target.then(|| {
                                            bail!(
                                                "target-fenced online audit found missing Pool location {}",
                                                hashtree_core::to_hex(hash)
                                            )
                                        })
                                    }
                                })
                                .collect::<Result<Vec<_>>>()?;
                            let covered =
                                online_audit.contains_target_exact_sorted(&stored_entries)?;
                            let missing = stored_entries
                                .iter()
                                .zip(&covered)
                                .filter_map(|(entry, present)| (!present).then_some(*entry))
                                .collect::<Vec<_>>();
                            for (hash, size) in &missing {
                                let data = pool.get_sync(hash)?.with_context(|| {
                                    format!(
                                        "target Pool lost {} during online content audit",
                                        hashtree_core::to_hex(hash)
                                    )
                                })?;
                                if data.len() as u64 != *size
                                    || hashtree_core::sha256(&data) != *hash
                                {
                                    bail!(
                                        "target Pool content differs from catalog hash/size for {}",
                                        hashtree_core::to_hex(hash)
                                    );
                                }
                            }
                            let page_cursor = page
                                .last()
                                .map(|(hash, _)| *hash)
                                .context("nonempty target catalog page has no cursor")?;
                            pool.validate_controlled_authority_and_sync()?;
                            launch.authorize_online_target_audit_batch(
                                page_cursor,
                                page.len(),
                                &missing,
                            )?;
                            launch.ensure_store_paths()?;
                            target_cursor = Some(page_cursor);
                            scanned = scanned.saturating_add(page.len());
                            target_audit_reused = target_audit_reused
                                .saturating_add(covered.iter().filter(|present| **present).count());
                            println!(
                                "Target audit batch: scanned {}, reused {}, newly root-verified {}",
                                page.len(),
                                covered.iter().filter(|present| **present).count(),
                                missing.len(),
                            );
                            epoch_batches = epoch_batches.saturating_add(1);
                            if epoch_batches >= reopen_batches {
                                println!(
                                    "Target audit mappings reopened after {epoch_batches} batches at cursor {}",
                                    hashtree_core::to_hex(&page_cursor)
                                );
                                break;
                            }
                        }
                    }
                }
                let completed =
                    source_completed && target_completed && launch.target_writers_fenced();
                if completed {
                    launch.authorize_checkpoint("online-evidence-publication", cursor, None)?;
                    let mut source_evidence = launch.create_source_evidence_writer()?;
                    let source_summary =
                        online_audit.for_each_source_verified_batch(batch_size, |entries| {
                            source_evidence.append(entries).map_err(|error| {
                                hashtree_core::store::StoreError::Other(format!(
                                    "append online source evidence: {error:#}"
                                ))
                            })
                        })?;
                    let source_evidence = source_evidence.finish()?;
                    let mut target_evidence = launch.create_online_target_evidence_writer()?;
                    let target_summary =
                        online_audit.for_each_target_verified_batch(batch_size, |entries| {
                            target_evidence.append(entries).map_err(|error| {
                                hashtree_core::store::StoreError::Other(format!(
                                    "append online target evidence: {error:#}"
                                ))
                            })
                        })?;
                    let target_evidence = target_evidence.finish()?;
                    launch.ensure_checkpoint_broker_alive()?;
                    launch.authorize_checkpoint("online-audit-publication", cursor, None)?;
                    launch.write_online_target_audit_receipt(
                        online_audit_path,
                        online_audit.binding(),
                        source_evidence,
                        &source_summary,
                        target_evidence,
                        &target_summary,
                    )?;
                    launch.ensure_checkpoint_broker_alive()?;
                    launch.authorize_checkpoint("online-readiness", cursor, None)?;
                }
                println!(
                    "Migration pass: scanned {scanned}, source audit reused {audit_reused}, target audit reused {target_audit_reused}, already present {already_present}, verified {verified}, inserted {inserted} blobs / {inserted_bytes} bytes, source covered: {source_completed}, target covered: {target_completed}, target fence held: {}, completed: {completed}",
                    launch.target_writers_fenced(),
                );
                println!("Cursor: {}", state_file.display());
            }
            PoolCommands::LaunchMigrateLmdbV3(_) => {
                bail!("Pool migration v3 controller must dispatch before configuration loading");
            }
            PoolCommands::Remove { id } => {
                let pool = open_existing()?;
                let id: PoolMemberId = id.parse()?;
                pool.remove_member(id)?;
                println!("Removed pool member {id}");
            }
        }
        Ok(())
    }

    #[cfg(not(feature = "lmdb"))]
    {
        let _ = (data_dir, command);
        bail!("LMDB support not enabled in this build");
    }
}

#[cfg(feature = "lmdb")]
fn first_unverified_pool_target(
    audit: &hashtree_lmdb::PoolMigrationAuditStore,
    pool: &hashtree_lmdb::PoolStore,
    page_size: usize,
    require_terminal: bool,
) -> Result<Option<([u8; 32], u64)>> {
    if page_size == 0 {
        bail!("online target audit coverage page size must be non-zero");
    }
    let mut cursor = None;
    loop {
        let page = pool.scan_catalog_locations_after(cursor, page_size)?;
        if page.is_empty() {
            return Ok(None);
        }
        let stored = page
            .iter()
            .filter_map(|(hash, location)| match location {
                hashtree_lmdb::PoolCatalogLocation::Stored { size, .. } => Some(Ok((*hash, *size))),
                hashtree_lmdb::PoolCatalogLocation::Pending { .. } => require_terminal.then(|| {
                    bail!(
                        "target-fenced coverage scan found pending Pool location {}",
                        hashtree_core::to_hex(hash)
                    )
                }),
                hashtree_lmdb::PoolCatalogLocation::Moving { .. } => require_terminal.then(|| {
                    bail!(
                        "target-fenced coverage scan found moving Pool location {}",
                        hashtree_core::to_hex(hash)
                    )
                }),
                hashtree_lmdb::PoolCatalogLocation::Missing => require_terminal.then(|| {
                    bail!(
                        "target-fenced coverage scan found missing Pool location {}",
                        hashtree_core::to_hex(hash)
                    )
                }),
            })
            .collect::<Result<Vec<_>>>()?;
        let covered = audit.contains_target_exact_sorted(&stored)?;
        if let Some((entry, _)) = stored.iter().zip(covered).find(|(_, present)| !present) {
            return Ok(Some(*entry));
        }
        cursor = page.last().map(|(hash, _)| *hash);
    }
}

#[cfg(feature = "lmdb")]
fn run_final_stopped_source_audit(
    launch: &super::pool_migration_launch::AcknowledgedPoolMigrationLaunch,
    batch_size: usize,
    _max_buffer_bytes: usize,
    source_read_concurrency: usize,
    reopen_batches: usize,
) -> Result<()> {
    use hashtree_lmdb::{ExternalBlobOptions, LmdbBlobReader, LmdbEnvironmentGeneration};

    let online = launch.online_target_audit()?.receipt.clone();
    let mut online_evidence = SourceEvidenceManifestReaderV3::open(&online.source_evidence)?;
    let mut next_online = online_evidence.next_entry()?;
    let mut cursor = None;
    let mut scanned = 0usize;
    let mut verified = 0usize;
    let mut verified_bytes = 0u64;
    let mut source_content_hasher = Sha256::new();
    source_content_hasher.update(b"hashtree-pool-migration-source-content/v3\0");
    let mut source_evidence = launch.create_source_evidence_writer()?;
    let mut generation: Option<LmdbEnvironmentGeneration> = None;
    loop {
        launch.ensure_source_paths()?;
        launch.ensure_final_writer_fence()?;
        let external = launch.source_external().map(|path| ExternalBlobOptions {
            base_path: path.to_path_buf(),
            min_bytes: 1,
            sync: true,
            pack_target_bytes: None,
        });
        let reader = LmdbBlobReader::open_with_external_read_concurrency_and_pinned_identity(
            launch.source(),
            external,
            source_read_concurrency,
            launch.source_lmdb_identity(),
        )?;
        let opened_generation = reader.environment_generation();
        if generation.is_some_and(|expected| expected != opened_generation) {
            bail!("source LMDB generation changed across mapping reopen");
        }
        generation = Some(opened_generation);
        let mut epoch_batches = 0usize;
        loop {
            launch.ensure_final_writer_masks()?;
            let hashes = reader.scan_hashes_after(cursor, batch_size)?;
            launch.ensure_checkpoint_broker_alive()?;
            launch.ensure_final_writer_masks()?;
            if hashes.is_empty() {
                launch.authorize_checkpoint("source-keyset-audit", cursor, None)?;
                let source_audit = reader.validate_terminal_migration_keyset()?;
                launch.ensure_checkpoint_broker_alive()?;
                if reader.environment_generation() != opened_generation {
                    bail!("source LMDB generation changed during terminal source audit");
                }
                while online_evidence.next_entry()?.is_some() {}
                let online_summary = online_evidence.validated_summary()?;
                if online_summary.entries != online.source_verified_entries
                    || online_summary.bytes != online.source_verified_bytes
                    || hashtree_core::to_hex(&online_summary.content_sha256)
                        != online.source_content_sha256
                {
                    bail!("online target audit evidence changed during stopped boundary scan");
                }
                drop(reader);
                launch.ensure_source_paths()?;
                launch.ensure_final_writer_fence()?;
                launch.authorize_checkpoint("source-evidence-publication", cursor, None)?;
                let source_evidence = source_evidence.finish()?;
                launch.ensure_checkpoint_broker_alive()?;
                launch.authorize_checkpoint("source-generation-fingerprint", cursor, None)?;
                let source_generation = launch.capture_source_generation(opened_generation)?;
                launch.ensure_checkpoint_broker_alive()?;
                let content = SourceContentAuditV3 {
                    verified_entries: verified as u64,
                    verified_bytes,
                    sha256: source_content_hasher.finalize().into(),
                };
                launch.authorize_checkpoint("source-terminal-publication", cursor, None)?;
                launch.write_source_terminal_receipt(
                    &source_audit,
                    &content,
                    source_evidence,
                    source_generation,
                )?;
                launch.ensure_checkpoint_broker_alive()?;
                launch.ensure_source_paths()?;
                launch.ensure_final_writer_fence()?;
                launch.authorize_checkpoint("terminal-readiness", cursor, None)?;
                println!(
                    "Terminal source boundary: {} raw blob keys / {} metadata keys / {} blob-only / {} inline / {} loose external / {} packed external / {} online-verified bytes, keyset {}, locations {}, content {}",
                    source_audit.blob_entries,
                    source_audit.metadata_entries,
                    source_audit.blob_only_entries,
                    source_audit.inline_entries,
                    source_audit.loose_external_entries,
                    source_audit.packed_external_entries,
                    content.verified_bytes,
                    hashtree_core::to_hex(&source_audit.sha256),
                    hashtree_core::to_hex(&source_audit.catalog_location_sha256),
                    hashtree_core::to_hex(&content.sha256),
                );
                println!(
                    "Source boundary pass: scanned {scanned}, reused online proof {verified}, target writes 0, completed: true"
                );
                return Ok(());
            }
            let sizes = reader.sizes_for_sorted_hashes(&hashes)?;
            let entries = hashes.iter().copied().zip(sizes).collect::<Vec<_>>();
            for (hash, bytes) in &entries {
                while next_online.is_some_and(|(online_hash, _)| online_hash < *hash) {
                    next_online = online_evidence.next_entry()?;
                }
                match next_online {
                    Some((online_hash, online_size))
                        if online_hash == *hash && online_size == *bytes =>
                    {
                        next_online = online_evidence.next_entry()?;
                    }
                    Some((online_hash, online_size)) if online_hash == *hash => {
                        bail!(
                            "stopped source size {} for {} differs from online target audit size {}",
                            bytes,
                            hashtree_core::to_hex(hash),
                            online_size,
                        )
                    }
                    _ => bail!(
                        "stopped source {} / {} bytes is absent from the certified online target audit; rerun online-bounded catch-up",
                        hashtree_core::to_hex(hash),
                        bytes,
                    ),
                }
                source_content_hasher.update(hash);
                source_content_hasher.update(bytes.to_be_bytes());
            }
            source_evidence.append(&entries)?;
            let page_bytes = entries.iter().try_fold(0u64, |total, (_, size)| {
                total
                    .checked_add(*size)
                    .context("source page byte total overflow")
            })?;
            verified_bytes = verified_bytes
                .checked_add(page_bytes)
                .context("source audit byte total overflow")?;
            scanned = scanned
                .checked_add(entries.len())
                .context("source audit scan count overflow")?;
            verified = verified
                .checked_add(entries.len())
                .context("source audit verified count overflow")?;
            cursor = hashes.last().copied();
            let cursor_hash = cursor.context("non-empty source audit batch has no cursor")?;
            println!(
                "Source boundary batch: scanned {}, reused online proof {}, payload bytes read 0",
                entries.len(),
                entries.len(),
            );
            epoch_batches = epoch_batches.saturating_add(1);
            if epoch_batches >= reopen_batches {
                println!(
                    "Source audit mappings reopened after {epoch_batches} batches at cursor {}",
                    hashtree_core::to_hex(&cursor_hash)
                );
                break;
            }
        }
    }
}

#[cfg(feature = "lmdb")]
fn run_final_stopped_full_reconciliation(
    launch: &super::pool_migration_launch::AcknowledgedPoolMigrationLaunch,
    batch_size: usize,
    max_buffer_bytes: usize,
    source_read_concurrency: usize,
    reopen_batches: usize,
) -> Result<()> {
    use hashtree_lmdb::{
        reconcile_lmdb_source_union_page_with_max_buffer_bytes_and_authorizer, ExternalBlobOptions,
        LmdbBlobReader, PinnedLmdbFileIdentity, PinnedLmdbIdentity, PoolMemberRuntimePaths,
        PoolStore, PoolStoreConfig, PoolStoreReader,
    };

    struct SourcePlan {
        receipt: PoolMigrationSourceTerminalReceiptV3,
        runtime_path: PathBuf,
        runtime_external_path: Option<PathBuf>,
    }

    let source_plans = launch.source_terminal_runtime_plans()?;
    if source_plans.is_empty() {
        bail!("final-stopped-full requires a nonempty exact source-terminal receipt set");
    }
    let receipt_sha256 = source_plans
        .iter()
        .map(|plan| plan.validated.authority_sha256.clone())
        .collect::<Vec<_>>();
    let mut sources = Vec::with_capacity(source_plans.len());
    for plan in source_plans {
        let receipt = plan.validated.receipt;
        sources.push(SourcePlan {
            receipt,
            runtime_path: plan.runtime_path,
            runtime_external_path: plan.runtime_external_path,
        });
    }
    let evidence_authorities = sources
        .iter()
        .map(|source| source.receipt.source_evidence.clone())
        .collect::<Vec<_>>();
    let mut union = SourceEvidenceUnionReaderV3::open(&evidence_authorities)?;
    let target_evidence_authorities = sources
        .iter()
        .map(|source| source.receipt.online_target_evidence.clone())
        .collect::<Vec<_>>();

    let mut pool_config = PoolStoreConfig::default();
    pool_config.temperature.enabled = false;
    pool_config.catalog_lmdb_identity = Some(launch.pool_catalog_lmdb_identity());
    pool_config.expected_manifest_sha256 = Some(launch.pool_manifest_sha256());
    pool_config.member_runtime_paths = launch
        .pool_member_runtime_paths()
        .into_iter()
        .map(|paths| {
            Ok(PoolMemberRuntimePaths {
                id: paths.id.parse()?,
                configured_path: paths.configured_path,
                runtime_path: paths.runtime_path,
                configured_external_path: paths.configured_external_path,
                runtime_external_path: paths.runtime_external_path,
                lmdb_identity: paths.lmdb_identity,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    launch.ensure_store_paths()?;
    launch.ensure_final_writer_fence()?;

    let mut source_exhausted = false;
    loop {
        let body_readers = sources
            .iter()
            .map(|source| {
                let identity = PinnedLmdbIdentity {
                    data: PinnedLmdbFileIdentity {
                        device: source.receipt.source_lmdb_identity.data.device,
                        inode: source.receipt.source_lmdb_identity.data.inode,
                    },
                    lock: PinnedLmdbFileIdentity {
                        device: source.receipt.source_lmdb_identity.lock.device,
                        inode: source.receipt.source_lmdb_identity.lock.inode,
                    },
                };
                let external =
                    source
                        .runtime_external_path
                        .as_ref()
                        .map(|path| ExternalBlobOptions {
                            base_path: path.clone(),
                            min_bytes: 1,
                            sync: true,
                            pack_target_bytes: None,
                        });
                LmdbBlobReader::open_with_external_read_concurrency_and_pinned_identity(
                    &source.runtime_path,
                    external,
                    source_read_concurrency,
                    identity,
                )
                .map_err(anyhow::Error::from)
            })
            .collect::<Result<Vec<_>>>()?;
        let pool = PoolStore::open(launch.pool(), pool_config.clone())?;
        let mut epoch_pages = 0usize;
        while epoch_pages < reopen_batches {
            let mut page = Vec::with_capacity(batch_size);
            while page.len() < batch_size {
                let Some(entry) = union.next_entry()? else {
                    source_exhausted = true;
                    break;
                };
                page.push(entry);
            }
            if page.is_empty() {
                break;
            }

            let entries = page
                .iter()
                .map(|entry| (entry.hash, entry.size, entry.body_source))
                .collect::<Vec<_>>();
            let source_readers = body_readers.iter().collect::<Vec<_>>();
            launch.ensure_final_writer_masks()?;
            let mut authorize_target_write = |batch_cursor, write_count| {
                launch
                    .authorize_checkpoint("migration-batch", batch_cursor, Some(write_count))
                    .map_err(|error| {
                        hashtree_core::store::StoreError::Other(format!(
                            "root checkpoint rejected target mutation: {error:#}"
                        ))
                    })
            };
            let batch = reconcile_lmdb_source_union_page_with_max_buffer_bytes_and_authorizer(
                &source_readers,
                &pool,
                &entries,
                max_buffer_bytes,
                &mut authorize_target_write,
            )?;
            launch.ensure_checkpoint_broker_alive()?;
            launch.ensure_final_writer_masks()?;
            println!(
                "Final union page: {} keys, exact Stored {}, {} source read groups, source bodies read {}, inserted {} blobs / {} bytes",
                batch.scanned,
                batch.already_present,
                batch.source_read_groups,
                batch.verified,
                batch.inserted,
                batch.inserted_bytes,
            );
            epoch_pages = epoch_pages.saturating_add(1);
        }
        pool.validate_controlled_authority_and_sync()?;
        drop(pool);
        drop(body_readers);
        launch.ensure_store_paths()?;
        launch.ensure_final_writer_fence()?;
        if source_exhausted {
            break;
        }
        println!("Final reconciliation mappings reopened after {epoch_pages} pages");
    }

    let source_summaries = union.validated_source_summaries()?;
    let union_summary = union.validated_union_summary()?;
    for (source, evidence) in sources.iter().zip(source_summaries) {
        if evidence.entries != source.receipt.source_verified_entries
            || evidence.bytes != source.receipt.source_verified_bytes
            || hashtree_core::to_hex(&evidence.content_sha256)
                != source.receipt.source_content_sha256
        {
            bail!(
                "source evidence manifest {} differs from its terminal receipt",
                source.receipt.source_evidence.path.display()
            );
        }
        validate_frozen_source_generation(
            &source.receipt,
            &source.runtime_path,
            source.runtime_external_path.as_deref(),
        )?;
        launch.authorize_checkpoint("source-evidence-consumed", None, None)?;
        launch.ensure_checkpoint_broker_alive()?;
    }
    let revalidated_receipts = launch.source_terminal_receipts(false)?;
    let revalidated_sha256 = revalidated_receipts
        .into_iter()
        .map(|validated| validated.authority_sha256)
        .collect::<Vec<_>>();
    if revalidated_sha256 != receipt_sha256 {
        bail!("source-terminal receipt authority set changed during final reconciliation");
    }

    launch.ensure_store_paths()?;
    launch.ensure_final_writer_fence()?;
    launch.authorize_checkpoint("target-terminal-audit", None, None)?;
    let terminal_reader = PoolStoreReader::open(launch.pool(), pool_config)?;
    let target_content = validate_terminal_catalog_target_evidence(
        &terminal_reader,
        &target_evidence_authorities,
        batch_size,
        || {
            launch.ensure_checkpoint_broker_alive()?;
            launch.ensure_final_writer_masks()
        },
    )?;
    for (source, evidence) in sources.iter().zip(&target_content.evidence) {
        if evidence.entries != source.receipt.online_target_verified_entries
            || evidence.bytes != source.receipt.online_target_verified_bytes
            || hashtree_core::to_hex(&evidence.content_sha256)
                != source.receipt.online_target_content_sha256
        {
            bail!(
                "certified target evidence manifest {} differs from its source-terminal receipt",
                source.receipt.online_target_evidence.path.display()
            );
        }
    }
    launch.ensure_checkpoint_broker_alive()?;
    launch.ensure_final_writer_masks()?;
    let terminal_audit = terminal_reader.validate_terminal_catalog_and_physical_state()?;
    launch.ensure_checkpoint_broker_alive()?;
    if terminal_audit.manifest_sha256 != launch.pool_manifest_sha256() {
        bail!("terminal Pool audit manifest differs from launch authority");
    }
    if target_content.catalog.entries != terminal_audit.stored_locations
        || target_content.catalog.bytes != terminal_audit.stored_bytes
    {
        bail!("terminal Pool catalog changed between target content proof and physical audit");
    }
    drop(terminal_reader);

    let source_union = PoolMigrationSourceUnionAuditV3 {
        receipt_sha256,
        source_count: sources.len() as u64,
        entries: union_summary.entries,
        bytes: union_summary.bytes,
        sha256: union_summary.content_sha256,
    };
    launch.ensure_store_paths()?;
    launch.ensure_final_writer_fence()?;
    launch.authorize_checkpoint("terminal-receipt-publication", None, None)?;
    launch.write_terminal_audit_receipt(&source_union, &target_content.catalog, &terminal_audit)?;
    launch.ensure_checkpoint_broker_alive()?;
    launch.ensure_store_paths()?;
    launch.ensure_final_writer_fence()?;
    launch.authorize_checkpoint("terminal-readiness", None, None)?;

    println!(
        "Final source union: {} sources / {} entries / {} bytes, reconciliation {}",
        source_union.source_count,
        source_union.entries,
        source_union.bytes,
        hashtree_core::to_hex(&source_union.sha256),
    );
    println!(
        "Single terminal Pool physical audit: stored {} blobs / {} bytes, catalog {}, physical {}",
        terminal_audit.stored_locations,
        terminal_audit.stored_bytes,
        hashtree_core::to_hex(&terminal_audit.catalog_sha256),
        hashtree_core::to_hex(&terminal_audit.physical_sha256),
    );
    Ok(())
}

#[cfg(feature = "lmdb")]
#[cfg(test)]
pub(super) fn write_pool_migration_cursor(path: &Path, value: &str) -> Result<()> {
    write_durable_pool_migration_cursor(path, value)
}

pub(crate) fn format_cid_for_display(cid: &Cid) -> String {
    hashtree_core::nhash_encode_full(&NHashData {
        hash: cid.hash,
        decrypt_key: cid.key,
    })
    .unwrap_or_else(|_| cid.to_string())
}

pub(crate) async fn resolve_load_target_cid(
    fetcher: &Fetcher,
    store: &Arc<HashtreeStore>,
    resolved: &ResolvedCid,
    progress: Option<&FetchProgress>,
) -> Result<Cid> {
    let cid = resolved.cid.clone();
    if let Some(path) = resolved.path.as_deref() {
        if cid_is_directory_with_fetch(fetcher, store, &cid).await? {
            let resolved_cid = resolve_path_with_fetch(fetcher, store, &cid, path)
                .await?
                .ok_or_else(|| anyhow::anyhow!("Path not found in directory: {}", path))?;
            fetcher
                .fetch_cid_tree_with_progress(store, &resolved_cid, progress)
                .await?;
            return Ok(resolved_cid);
        }

        fetcher
            .fetch_cid_tree_with_progress(store, &cid, progress)
            .await?;
        return Ok(cid);
    }

    fetcher
        .fetch_cid_tree_with_progress(store, &cid, progress)
        .await?;
    Ok(cid)
}

pub(crate) async fn resolve_cat_target_cid(
    fetcher: &Fetcher,
    store: &Arc<HashtreeStore>,
    resolved: &ResolvedCid,
) -> Result<Cid> {
    let cid = resolved.cid.clone();
    if let Some(path) = resolved.path.as_deref() {
        if cid_is_directory_with_fetch(fetcher, store, &cid).await? {
            let resolved_cid = resolve_path_with_fetch(fetcher, store, &cid, path)
                .await?
                .ok_or_else(|| anyhow::anyhow!("Path not found in directory: {}", path))?;
            fetcher.fetch_cid_tree(store, &resolved_cid).await?;
            return Ok(resolved_cid);
        }

        fetcher.fetch_cid_tree(store, &cid).await?;
        return Ok(cid);
    }

    if cid_is_directory_with_fetch(fetcher, store, &cid).await? {
        anyhow::bail!("Cannot cat a directory; specify a file path or use `htree get`");
    }

    fetcher.fetch_cid_tree(store, &cid).await?;
    Ok(cid)
}

fn ensure_loaded_target_present(store: &HashtreeStore, cid: &Cid) -> Result<()> {
    if store.get_chunk(&cid.hash)?.is_some() {
        return Ok(());
    }
    anyhow::bail!("Hash not found: {}", format_cid_for_display(cid));
}

pub(crate) async fn resolve_info_target(
    store: &Arc<HashtreeStore>,
    fetcher: &Fetcher,
    root_cid: &Cid,
    path: Option<&str>,
) -> Result<Cid> {
    let Some(path) = path else {
        ensure_root_chunk_loaded(fetcher, store, root_cid).await?;
        return Ok(root_cid.clone());
    };

    if !cid_is_directory_with_fetch(fetcher, store, root_cid).await? {
        return Ok(root_cid.clone());
    }

    let target_cid = resolve_path_with_fetch(fetcher, store, root_cid, path)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Path not found in directory: {}", path))?;

    ensure_root_chunk_loaded(fetcher, store, &target_cid).await?;
    Ok(target_cid)
}

async fn ensure_root_chunk_loaded(
    fetcher: &Fetcher,
    store: &Arc<HashtreeStore>,
    cid: &Cid,
) -> Result<()> {
    fetcher
        .fetch_chunk_with_store(store, &cid.hash)
        .await
        .with_context(|| format!("Failed to fetch {}", format_cid_for_display(cid)))?;
    Ok(())
}

async fn fetch_missing_chunk(
    fetcher: &Fetcher,
    store: &Arc<HashtreeStore>,
    missing: &str,
    seen_missing: &mut HashSet<String>,
) -> Result<()> {
    if !seen_missing.insert(missing.to_string()) {
        anyhow::bail!("Repeated missing chunk {}", missing);
    }
    let hash =
        from_hex(missing).with_context(|| format!("Invalid missing chunk hash {}", missing))?;
    fetcher
        .fetch_chunk_with_store(store, &hash)
        .await
        .with_context(|| format!("Failed to fetch missing chunk {}", missing))?;
    Ok(())
}

async fn cid_is_directory_with_fetch(
    fetcher: &Fetcher,
    store: &Arc<HashtreeStore>,
    cid: &Cid,
) -> Result<bool> {
    ensure_root_chunk_loaded(fetcher, store, cid).await?;
    let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());
    let mut seen_missing = HashSet::new();

    loop {
        match tree.is_dir(cid).await {
            Ok(is_directory) => return Ok(is_directory),
            Err(HashTreeError::MissingChunk(missing)) => {
                fetch_missing_chunk(fetcher, store, &missing, &mut seen_missing).await?;
            }
            Err(error) => {
                return Err(anyhow::anyhow!("Failed to inspect directory: {}", error));
            }
        }
    }
}

async fn resolve_path_with_fetch(
    fetcher: &Fetcher,
    store: &Arc<HashtreeStore>,
    cid: &Cid,
    path: &str,
) -> Result<Option<Cid>> {
    ensure_root_chunk_loaded(fetcher, store, cid).await?;
    let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());
    let mut seen_missing = HashSet::new();

    loop {
        match tree.resolve_path(cid, path).await {
            Ok(resolved) => return Ok(resolved),
            Err(HashTreeError::MissingChunk(missing)) => {
                fetch_missing_chunk(fetcher, store, &missing, &mut seen_missing).await?;
            }
            Err(error) => {
                return Err(anyhow::anyhow!("Failed to resolve path: {}", error));
            }
        }
    }
}

async fn run_with_fetch_progress<T, F>(
    label: &'static str,
    progress: Arc<FetchProgress>,
    future: F,
) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    if !std::io::stderr().is_terminal() {
        return future.await;
    }

    let done = Arc::new(AtomicBool::new(false));
    let outcome = Arc::new(AtomicU8::new(0));
    let progress_task =
        spawn_fetch_progress_task(label, progress, Arc::clone(&done), Arc::clone(&outcome));
    let result = future.await;
    outcome.store(if result.is_ok() { 1 } else { 2 }, Ordering::Relaxed);
    done.store(true, Ordering::Relaxed);
    let _ = progress_task.await;
    result
}

fn spawn_fetch_progress_task(
    label: &'static str,
    progress: Arc<FetchProgress>,
    done: Arc<AtomicBool>,
    outcome: Arc<AtomicU8>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(200));
        let start = tokio::time::Instant::now();
        let mut shown = false;
        let mut previous_line_len = 0usize;
        let mut last_report_second = None;

        loop {
            interval.tick().await;
            let elapsed = start.elapsed();
            let snapshot = progress.snapshot();

            if done.load(Ordering::Relaxed) {
                if shown {
                    let status = match outcome.load(Ordering::Relaxed) {
                        2 => "stopped",
                        _ => "complete",
                    };
                    let final_line = format!(
                        "{} {}: {}",
                        label,
                        status,
                        format_fetch_progress_line(snapshot, elapsed)
                    );
                    print_progress_line(&final_line, &mut previous_line_len, true);
                }
                break;
            }

            if elapsed < Duration::from_secs(1) {
                continue;
            }

            let elapsed_seconds = elapsed.as_secs();
            if last_report_second == Some(elapsed_seconds) {
                continue;
            }
            last_report_second = Some(elapsed_seconds);
            shown = true;
            let line = format!(
                "{label}... {}",
                format_fetch_progress_line(snapshot, elapsed)
            );
            print_progress_line(&line, &mut previous_line_len, false);
        }
    })
}

fn print_progress_line(line: &str, previous_line_len: &mut usize, newline: bool) {
    let padding_len = previous_line_len.saturating_sub(line.len());
    let padding = " ".repeat(padding_len);
    if newline {
        eprintln!("\r{line}{padding}");
    } else {
        eprint!("\r{line}{padding}");
        let _ = std::io::stderr().flush();
    }
    *previous_line_len = line.len();
}

fn format_fetch_progress_line(
    snapshot: hashtree_cli::FetchProgressSnapshot,
    elapsed: Duration,
) -> String {
    if snapshot.chunks_fetched == 0 {
        return format!("waiting for data ({})", format_duration_compact(elapsed));
    }
    format!(
        "{} fetched in {}",
        format_fetch_summary(snapshot),
        format_duration_compact(elapsed)
    )
}

fn format_fetch_summary(snapshot: hashtree_cli::FetchProgressSnapshot) -> String {
    let chunk_label = if snapshot.chunks_fetched == 1 {
        "chunk"
    } else {
        "chunks"
    };
    format!(
        "{} {} ({})",
        snapshot.chunks_fetched,
        chunk_label,
        format_bytes(snapshot.bytes_fetched)
    )
}

async fn fetch_daemon_status_quietly(addr: &str) -> Option<serde_json::Value> {
    let url = format!("http://{}/api/status", addr);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(700))
        .build()
        .ok()?;
    let response = client.get(&url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json().await.ok()
}

fn status_u64(status: &serde_json::Value, section: &str, key: &str) -> u64 {
    status
        .get(section)
        .and_then(|value| value.get(key))
        .and_then(|value| value.as_u64())
        .unwrap_or(0)
}

fn print_network_stats(status: &serde_json::Value) {
    let mesh_enabled = status
        .get("mesh")
        .and_then(|mesh| mesh.get("enabled"))
        .and_then(|enabled| enabled.as_bool())
        .unwrap_or(false);
    let relay_enabled = status
        .get("relay")
        .and_then(|relay| relay.get("enabled"))
        .and_then(|enabled| enabled.as_bool())
        .unwrap_or(false);
    if !mesh_enabled && !relay_enabled {
        return;
    }

    let mesh_sent = status_u64(status, "mesh", "bytes_sent");
    let mesh_received = status_u64(status, "mesh", "bytes_received");
    let relay_sent = status_u64(status, "relay", "bytes_sent");
    let relay_received = status_u64(status, "relay", "bytes_received");

    println!();
    println!("Network:");
    if mesh_enabled {
        let total_peers = status_u64(status, "mesh", "total_peers");
        let connected = status_u64(status, "mesh", "connected");
        println!("  Peers: {connected}/{total_peers} connected");
    }
    let uptime = status
        .get("uptime_seconds")
        .and_then(|value| value.as_u64())
        .map(|seconds| {
            format!(
                " (uptime {})",
                format_duration_compact(Duration::from_secs(seconds))
            )
        })
        .unwrap_or_default();
    println!(
        "  Traffic since daemon start{}: up {}, down {}",
        uptime,
        format_bytes(mesh_sent.saturating_add(relay_sent)),
        format_bytes(mesh_received.saturating_add(relay_received))
    );
    if mesh_enabled {
        println!(
            "  Mesh traffic: up {}, down {}",
            format_bytes(mesh_sent),
            format_bytes(mesh_received)
        );
    }
    if relay_enabled {
        println!(
            "  Relay traffic: up {}, down {}",
            format_bytes(relay_sent),
            format_bytes(relay_received)
        );
    }
}

fn format_duration_compact(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 60 {
        return format!("{}m{:02}s", seconds / 60, seconds % 60);
    }
    if seconds > 0 {
        return format!("{seconds}s");
    }
    format!("{}ms", duration.as_millis())
}

async fn print_info_for_cid(store: &Arc<HashtreeStore>, cid: &Cid) -> Result<bool> {
    use hashtree_core::to_hex;

    if store.get_chunk(&cid.hash)?.is_none() {
        return Ok(false);
    }

    let tree = HashTree::new(HashTreeConfig::new(store.store_arc()).public());
    let total_size = tree
        .get_size_cid(cid)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to get size: {}", e))?;
    let is_directory = tree
        .is_dir(cid)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to inspect directory: {}", e))?;
    let node = if is_directory {
        tree.get_directory_node(cid)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get directory node: {}", e))?
    } else {
        tree.get_node(cid)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get tree node: {}", e))?
    };

    println!("Hash: {}", format_cid_for_display(cid));
    println!("Pinned: {}", store.is_pinned(&cid.hash)?);
    println!("Total size: {} bytes", total_size);

    if is_directory {
        println!("Directory: true");
        let entries = tree
            .list_directory(cid)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to list directory: {}", e))?;
        println!("\nDirectory contents:");
        for entry in entries {
            let type_str = if entry.link_type.is_tree() {
                "dir"
            } else {
                "file"
            };
            let entry_cid = Cid {
                hash: entry.hash,
                key: entry.key,
            };
            println!(
                "  [{}] {} -> {} ({} bytes)",
                type_str,
                entry.name,
                format_cid_for_display(&entry_cid),
                entry.size
            );
        }
    } else if let Some(node) = &node {
        let is_chunked = !node.links.is_empty();
        println!("Chunked: {}", is_chunked);

        if is_chunked {
            println!("Chunks: {}", node.links.len());
            println!("\nChunk details:");
            for (i, link) in node.links.iter().enumerate() {
                println!("  [{}] {} ({} bytes)", i, to_hex(&link.hash), link.size);
            }
        }
    } else {
        println!("Chunked: false");
    }

    if let Some(node) = node {
        println!("\nTree node info:");
        println!("  Links: {}", node.links.len());
        for (i, link) in node.links.iter().enumerate() {
            let name = link.name.as_deref().unwrap_or("<unnamed>");
            println!(
                "    [{}] {} -> {} ({} bytes)",
                i,
                name,
                to_hex(&link.hash),
                link.size
            );
        }
    }

    Ok(true)
}
