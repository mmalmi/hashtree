use super::add::{
    build_drive_iris_to_url_for_add_route, build_drive_iris_to_url_for_published_ref,
    build_drive_iris_to_url_for_published_target, build_sites_iris_to_url_for_add_route,
    build_sites_iris_to_url_for_published_ref, detect_site_entry_for_path, render_add_output,
    PublishedAddSummary,
};
use super::daemonize::{
    build_daemon_args, format_daemon_status, parse_pid, read_pid_file, write_pid_file,
};
#[cfg(unix)]
use super::daemonize::{
    daemon_state_file_path, read_daemon_launch_state, write_daemon_launch_state, DaemonLaunchState,
};
use super::lists::{
    build_mute_list_event, load_mute_entries, update_hex_list_file,
    update_mute_list_file_with_status, MuteEntry, MuteUpdate,
};
use super::resolve::{
    parse_published_target, resolve_cid_input, resolve_cid_input_with_opts, ResolveOptions,
    ResolvedCid,
};
#[cfg(feature = "fuse")]
use super::run::{
    find_existing_active_mount, is_stale_mount_io_error, should_warn_for_temporary_mountpoint,
};
use super::run::{
    format_cid_for_display, pin_input_target, resolve_cat_target_cid, resolve_info_target,
    resolve_load_target_cid, root_daemon_override_enabled, stored_published_pin_hash,
};
use super::storage_stats::{
    classify_storage_bucket, render_storage_inventory, AuthorSummary, PinnedDetail, StorageBucket,
    StorageBucketSummary, StorageInventory, TreeDetail,
};
use super::util::format_bytes;
use crate::app::args::{
    CashuCommands, CashuMintCommands, MirrorCommands, NostrIndexCommands, ReleaseCommands,
    SocialGraphCommands, SocialGraphIndexArgs, StorageCommands,
};
use crate::app::args::{Cli, Commands, PoolCommands};
#[cfg(feature = "fuse")]
use crate::app::mount_registry::ActiveMount;
use clap::{CommandFactory, Parser};
use hashtree_cli::config::ServerMode;
use hashtree_cli::{FetchConfig, Fetcher, HashtreeStore, NostrToBech32};
use hashtree_core::{nhash_decode, Cid};
use hashtree_updater::UpdateRef;
use nostr::{Keys, Kind};
#[cfg(feature = "fuse")]
use std::io;
use std::path::PathBuf;

fn args_to_strings(args: Vec<std::ffi::OsString>) -> Vec<String> {
    args.into_iter()
        .map(|arg| arg.to_string_lossy().to_string())
        .collect()
}

#[test]
fn test_root_daemon_override_enabled_parses_false_values() {
    assert!(!root_daemon_override_enabled(None));
    assert!(!root_daemon_override_enabled(Some("")));
    assert!(!root_daemon_override_enabled(Some("0")));
    assert!(!root_daemon_override_enabled(Some("false")));
    assert!(!root_daemon_override_enabled(Some("NO")));
    assert!(!root_daemon_override_enabled(Some(" off ")));
}

#[test]
fn test_root_daemon_override_enabled_parses_true_values() {
    assert!(root_daemon_override_enabled(Some("1")));
    assert!(root_daemon_override_enabled(Some("true")));
    assert!(root_daemon_override_enabled(Some("yes")));
    assert!(root_daemon_override_enabled(Some("dev-container")));
}

#[test]
fn test_build_daemon_args_with_overrides() {
    let data_dir = PathBuf::from("data-dir");
    let args = args_to_strings(build_daemon_args(
        Some("127.0.0.1:8080"),
        Some("wss://relay.example"),
        Some(ServerMode::Assist),
        Some(&data_dir),
    ));

    assert_eq!(
        args,
        vec![
            "--addr",
            "127.0.0.1:8080",
            "--relays",
            "wss://relay.example",
            "--mode",
            "assist",
            "--data-dir",
            "data-dir",
        ]
    );
}

#[test]
fn test_build_daemon_args_minimal() {
    let args = args_to_strings(build_daemon_args(None, None, None, None));
    assert!(args.is_empty());
}

#[test]
fn test_nostr_index_query_args() {
    let cli = Cli::try_parse_from([
        "htree",
        "nostr-index",
        "query",
        "--filter",
        r##"{"kinds":[7368],"#i":["fips.peer"]}"##,
        "--limit",
        "25",
    ])
    .unwrap();

    let Commands::NostrIndex { command } = cli.command else {
        panic!("expected nostr-index command");
    };
    let NostrIndexCommands::Query {
        root,
        filter,
        filter_file,
        limit,
        out,
    } = command
    else {
        panic!("expected nostr-index query command");
    };
    assert!(root.is_none());
    assert_eq!(
        filter.as_deref(),
        Some(r##"{"kinds":[7368],"#i":["fips.peer"]}"##)
    );
    assert!(filter_file.is_none());
    assert_eq!(limit, 25);
    assert!(out.is_none());
}

#[test]
fn test_nostr_index_import_args() {
    let cli = Cli::try_parse_from([
        "htree",
        "nostr-index",
        "import",
        "--events",
        "ratings.json",
        "--out",
        "report.json",
    ])
    .unwrap();

    let Commands::NostrIndex { command } = cli.command else {
        panic!("expected nostr-index command");
    };
    let NostrIndexCommands::Import {
        root,
        events_file,
        out,
    } = command
    else {
        panic!("expected nostr-index import command");
    };
    assert!(root.is_none());
    assert_eq!(events_file, PathBuf::from("ratings.json"));
    assert_eq!(out, Some(PathBuf::from("report.json")));
}

#[test]
fn test_storage_pool_add_and_migration_args() {
    let cli = Cli::try_parse_from([
        "htree",
        "storage",
        "pool",
        "add",
        "/pool/member",
        "--capacity-gb",
        "24",
        "--map-size-gb",
        "4",
        "--external-dir",
        "/pool/packs",
        "--max-reads",
        "12",
        "--max-writes",
        "3",
        "--temperature-low-percent",
        "60",
        "--temperature-high-percent",
        "80",
    ])
    .unwrap();
    let Commands::Storage {
        command: StorageCommands::Pool { command },
    } = cli.command
    else {
        panic!("expected storage pool command");
    };
    let PoolCommands::Add {
        capacity_gb,
        map_size_gb,
        max_reads,
        max_writes,
        temperature_low_percent,
        temperature_high_percent,
        ..
    } = command
    else {
        panic!("expected storage pool add command");
    };
    assert_eq!(capacity_gb, 24);
    assert_eq!(map_size_gb, Some(4));
    assert_eq!(max_reads, 12);
    assert_eq!(max_writes, 3);
    assert_eq!(temperature_low_percent, 60);
    assert_eq!(temperature_high_percent, 80);

    let cli = Cli::try_parse_from([
        "htree",
        "storage",
        "pool",
        "balance-temperature",
        "--max-moves",
        "7",
        "--max-bytes-gb",
        "2",
        "--max-concurrency",
        "3",
    ])
    .unwrap();
    let Commands::Storage {
        command: StorageCommands::Pool { command },
    } = cli.command
    else {
        panic!("expected storage pool command");
    };
    assert!(matches!(
        command,
        PoolCommands::BalanceTemperature {
            max_moves: Some(7),
            max_bytes_gb: Some(2),
            max_concurrency: Some(3),
        }
    ));

    let cli = Cli::try_parse_from([
        "htree",
        "storage",
        "pool",
        "migrate-lmdb",
        "--source",
        "/old/blobs",
        "--state-file",
        "/state/legacy.cursor",
        "--resume",
    ])
    .unwrap();
    let Commands::Storage {
        command: StorageCommands::Pool { command },
    } = cli.command
    else {
        panic!("expected storage pool command");
    };
    assert!(matches!(
        command,
        PoolCommands::MigrateLmdb {
            batch_size: 256,
            max_buffer_mib: 64,
            reopen_batches: 256,
            max_items: None,
            resume: true,
            ..
        }
    ));
}

#[test]
fn test_build_daemon_args_with_addr_override() {
    let args = args_to_strings(build_daemon_args(Some("0.0.0.0:8080"), None, None, None));
    assert_eq!(args, vec!["--addr", "0.0.0.0:8080"]);
}

#[test]
fn test_parse_pid() {
    assert_eq!(parse_pid("123\n").unwrap(), 123);
    assert!(parse_pid("").is_err());
    assert!(parse_pid("abc").is_err());
}

#[test]
fn test_pid_file_roundtrip() {
    let temp_dir = tempfile::tempdir().unwrap();
    let path = temp_dir.path().join("htree.pid");
    write_pid_file(&path, 42).unwrap();
    let pid = read_pid_file(&path).unwrap();
    assert_eq!(pid, 42);
}

#[cfg(unix)]
#[test]
fn test_daemon_launch_state_roundtrip() {
    let temp_dir = tempfile::tempdir().unwrap();
    let path = temp_dir.path().join("htree.pid");
    let state_path = daemon_state_file_path(&path);
    let state = DaemonLaunchState {
        addr: Some("127.0.0.1:18080".to_string()),
        relays: Some("wss://relay.example,wss://relay.two".to_string()),
        mode: Some(ServerMode::Assist),
        data_dir: Some(PathBuf::from("/tmp/htree-data")),
        log_file: PathBuf::from("/tmp/htree.log"),
    };

    write_daemon_launch_state(&state_path, &state).unwrap();
    let reloaded = read_daemon_launch_state(&state_path).unwrap();
    assert_eq!(reloaded, state);
}

#[test]
fn test_format_bytes_uses_reasonable_binary_units() {
    assert_eq!(format_bytes(0), "0 B");
    assert_eq!(format_bytes(1023), "1023 B");
    assert_eq!(format_bytes(1024), "1.0 KiB");
    assert_eq!(format_bytes(222_944_845_229), "207.6 GiB");
}

#[test]
fn test_storage_bucket_classification_prefers_social_graph_distance() {
    assert_eq!(classify_storage_bucket(255, None), StorageBucket::Mine);
    assert_eq!(classify_storage_bucket(128, None), StorageBucket::Followed);
    assert_eq!(classify_storage_bucket(64, None), StorageBucket::Other);
    assert_eq!(
        classify_storage_bucket(255, Some(1)),
        StorageBucket::Followed
    );
    assert_eq!(
        classify_storage_bucket(64, Some(2)),
        StorageBucket::SocialGraph
    );
}

#[test]
fn test_storage_inventory_render_shows_bucket_details() {
    let mut inventory = StorageInventory {
        buckets: vec![
            empty_storage_bucket(StorageBucket::Mine),
            empty_storage_bucket(StorageBucket::Followed),
            empty_storage_bucket(StorageBucket::SocialGraph),
            empty_storage_bucket(StorageBucket::Other),
        ],
    };
    inventory.buckets[1].indexed_tree_count = 1;
    inventory.buckets[1].indexed_tree_bytes = 4096;
    inventory.buckets[1].owned_blob_count = 2;
    inventory.buckets[1].owned_blob_bytes = 2048;
    inventory.buckets[1].pinned_items.push(PinnedDetail {
        name: "alice/photos".to_string(),
        cid: "aa".repeat(32),
        is_directory: true,
        size_bytes: 4096,
    });
    inventory.buckets[1].trees.push(TreeDetail {
        name: "alice/photos".to_string(),
        owner: "npub1alice...".to_string(),
        root: "aa".repeat(32),
        size_bytes: 4096,
        pinned: true,
    });
    inventory.buckets[1].authors.push(AuthorSummary {
        key: "alice".to_string(),
        label: "Alice (npub1alice...)".to_string(),
        indexed_tree_count: 1,
        indexed_tree_bytes: 4096,
        pinned_tree_count: 1,
        owned_blob_count: 2,
        owned_blob_bytes: 2048,
    });

    let rendered = render_storage_inventory(&inventory);

    assert!(rendered.contains("Known content:"));
    assert!(rendered.contains("Followed users' stuff: 6.0 KiB"));
    assert!(rendered.contains("Indexed trees: 1 tree (4.0 KiB)"));
    assert!(rendered.contains("Owned Blossom blobs: 2 blobs (2.0 KiB)"));
    assert!(rendered.contains("Authors:"));
    assert!(rendered
        .contains("Alice (npub1alice...) - 1 tree (4.0 KiB), 1 tree pinned; 2 blobs (2.0 KiB)"));
    assert!(rendered.contains("Pinned items:"));
    assert!(rendered.contains("[dir] alice/photos - 4.0 KiB - aaaaaaaaaaaa..."));
    assert!(rendered.contains("alice/photos - 4.0 KiB - npub1alice... (pinned)"));
    assert!(rendered.contains("Social graph people's stuff: 0 B"));
    assert!(rendered.contains("Known indexed payloads: 4.0 KiB across 1 tree"));
}

fn empty_storage_bucket(bucket: StorageBucket) -> StorageBucketSummary {
    StorageBucketSummary {
        bucket,
        indexed_tree_count: 0,
        indexed_tree_bytes: 0,
        owned_blob_count: 0,
        owned_blob_bytes: 0,
        pinned_unindexed_count: 0,
        pinned_unindexed_bytes: 0,
        pinned_items: Vec::new(),
        trees: Vec::new(),
        authors: Vec::new(),
    }
}

#[test]
fn test_daemon_status_uses_human_storage_labels() {
    let status = serde_json::json!({
        "status": "running",
        "uptime_seconds": 125,
        "storage": {
            "total_dags": 12,
            "pinned_dags": 3,
            "total_bytes": 222_944_845_229u64
        }
    });

    let rendered = format_daemon_status(&status, true);

    assert!(rendered.contains("Uptime: 2m05s"));
    assert!(rendered.contains("Stored objects: 12"));
    assert!(rendered.contains("Pinned items: 3"));
    assert!(rendered.contains("Total size: 207.6 GiB"));
    assert!(!rendered.contains("DAGs"));
}

#[test]
fn test_daemon_status_formats_queue_and_http_counters() {
    let status = serde_json::json!({
        "status": "running",
        "queues": {
            "blob_reads": {
                "limit": 16,
                "in_use": 2,
                "available": 14,
                "queue_timeout_ms": 2000,
                "task_timeout_ms": 5000
            },
            "blob_writes": {
                "limit": 4,
                "in_use": 1,
                "available": 3
            },
            "optimistic_uploads": {
                "enabled": true,
                "max_bytes": 512 * 1024 * 1024u64,
                "reserved_bytes": 256 * 1024u64,
                "in_flight": 3,
                "queue_timeout_ms": 15000
            },
            "upload_replication": {
                "enabled": true,
                "targets": 1,
                "max_bytes": 512 * 1024 * 1024u64,
                "reserved_bytes": 1024 * 1024u64,
                "coalesce_queued_jobs": 2,
                "in_flight_batches": 1,
                "accepted_batches": 3,
                "accepted_blobs": 96,
                "replicated_bytes": 24 * 1024 * 1024u64,
                "failed_batches": 1,
                "skipped_jobs": 2
            }
        },
        "upstream": {
            "blossom_servers": 2,
            "nostr_relays": 3,
            "blossom_fetch": {
                "lookup_attempts": 11,
                "hits": 4,
                "hit_bytes": 2 * 1024 * 1024u64,
                "explicit_misses": 5,
                "indeterminate_misses": 2,
                "miss_cache_hits": 9
            }
        },
        "http": {
            "status_classes": {
                "window_seconds": 60,
                "recent": {
                    "total": 42,
                    "1xx": 1,
                    "2xx": 30,
                    "3xx": 2,
                    "4xx": 8,
                    "5xx": 1,
                    "other": 1
                }
            }
        }
    });

    let rendered = format_daemon_status(&status, true);

    assert!(rendered.contains("Queues:"));
    assert!(rendered.contains("Blob reads: 2/16 in use, 14 available, queue 2000ms, task 5000ms"));
    assert!(rendered.contains("Blob writes: 1/4 in use, 3 available"));
    assert!(rendered.contains(
        "Optimistic uploads: enabled, 256.0 KiB/512.0 MiB reserved, 3 in flight, queue 15000ms"
    ));
    assert!(rendered.contains(
        "Upload replication: enabled, 1 target(s), 1.0 MiB/512.0 MiB reserved, 2 queued, 1 in flight, accepted 3 batch(es)/96 blob(s) (24.0 MiB), failed 1, skipped 2"
    ));
    assert!(rendered.contains("Blossom servers: 2, Nostr relays: 3"));
    assert!(rendered.contains(
        "Blossom fetch: 11 lookup(s), 4 hit(s) (2.0 MiB), 5 explicit miss(es), 2 indeterminate miss(es), 9 cache hit(s)"
    ));
    assert!(rendered.contains("Last 60s: 42 total, 1 1xx, 30 2xx, 2 3xx, 8 4xx, 1 5xx, 1 other"));
}

#[test]
fn test_update_hex_list_file_add_remove() {
    let temp_dir = tempfile::tempdir().unwrap();
    let path = temp_dir.path().join("mutes.json");
    let pk1 = "aa".repeat(32);
    let pk2 = "bb".repeat(32);

    let list = update_hex_list_file(&path, &pk1, true).unwrap();
    assert_eq!(list, vec![pk1.clone()]);

    let list = update_hex_list_file(&path, &pk1, true).unwrap();
    assert_eq!(list, vec![pk1.clone()]);

    let list = update_hex_list_file(&path, &pk2, true).unwrap();
    assert_eq!(list, vec![pk1.clone(), pk2.clone()]);

    let list = update_hex_list_file(&path, &pk1, false).unwrap();
    assert_eq!(list, vec![pk2.clone()]);
}

#[test]
fn test_build_mute_list_event_tags() {
    let keys = nostr::Keys::generate();
    let pk1 = nostr::Keys::generate().public_key().to_hex();
    let pk2 = nostr::Keys::generate().public_key().to_hex();
    let list = vec![
        MuteEntry {
            pubkey: pk1.clone(),
            reason: Some("spam".to_string()),
        },
        MuteEntry {
            pubkey: pk2.clone(),
            reason: None,
        },
    ];
    let event = build_mute_list_event(&list, &keys).unwrap();

    assert_eq!(event.kind, Kind::Custom(10000));

    let tags: Vec<String> = event
        .tags
        .iter()
        .filter_map(|tag| {
            let slice = tag.as_slice();
            if slice.first().map(|v| v.as_str()) == Some("p") {
                slice.get(1).cloned()
            } else {
                None
            }
        })
        .collect();

    assert_eq!(tags.len(), 2);
    assert!(tags.contains(&pk1));
    assert!(tags.contains(&pk2));

    let reason_tag = event
        .tags
        .iter()
        .find(|tag| tag.as_slice().get(1).map(|v| v.as_str()) == Some(pk1.as_str()))
        .expect("reason tag missing");
    assert_eq!(
        reason_tag.as_slice().get(2).map(|v| v.as_str()),
        Some("spam")
    );
}

#[test]
fn test_update_mute_list_with_reason() {
    let temp_dir = tempfile::tempdir().unwrap();
    let path = temp_dir.path().join("mutes.json");
    let pk1 = "aa".repeat(32);
    let pk2 = "bb".repeat(32);

    let (list, update) =
        update_mute_list_file_with_status(&path, &pk1, Some("spam"), true).unwrap();
    assert_eq!(update, MuteUpdate::Added);
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].reason.as_deref(), Some("spam"));

    let (list, update) =
        update_mute_list_file_with_status(&path, &pk1, Some("abuse"), true).unwrap();
    assert_eq!(update, MuteUpdate::Updated);
    assert_eq!(list[0].reason.as_deref(), Some("abuse"));

    let (_list, update) = update_mute_list_file_with_status(&path, &pk2, None, true).unwrap();
    assert_eq!(update, MuteUpdate::Added);

    let (list, update) = update_mute_list_file_with_status(&path, &pk1, None, false).unwrap();
    assert_eq!(update, MuteUpdate::Removed);
    assert_eq!(list.len(), 1);
}

#[test]
fn test_load_mute_entries_legacy_format() {
    let temp_dir = tempfile::tempdir().unwrap();
    let path = temp_dir.path().join("mutes.json");
    let pk1 = "aa".repeat(32);
    let pk2 = "bb".repeat(32);
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&vec![pk1.clone(), pk2.clone()]).unwrap(),
    )
    .unwrap();

    let entries = load_mute_entries(&path).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].pubkey, pk1);
    assert_eq!(entries[0].reason, None);
}

#[test]
fn test_format_cid_for_display_preserves_decrypt_key() {
    let cid = Cid {
        hash: [0x11; 32],
        key: Some([0x22; 32]),
    };

    let rendered = format_cid_for_display(&cid);
    let decoded = nhash_decode(&rendered).expect("decode rendered nhash");

    assert_eq!(decoded.hash, cid.hash);
    assert_eq!(decoded.decrypt_key, cid.key);
}

#[test]
fn test_build_drive_iris_to_url_for_add_route_encodes_path_segments() {
    assert_eq!(
        build_drive_iris_to_url_for_add_route("nhash1example/My notes/index.html"),
        "https://drive.iris.to/#/nhash1example/My%20notes/index.html"
    );
}

#[test]
fn test_build_drive_iris_to_url_for_published_ref_encodes_tree_name_as_single_segment() {
    assert_eq!(
        build_drive_iris_to_url_for_published_ref("npub1owner", "apps/iris ui",),
        "https://drive.iris.to/#/npub1owner/apps%2Firis%20ui"
    );
}

#[test]
fn test_build_drive_iris_to_url_for_published_target_includes_path_and_link_key() {
    assert_eq!(
        build_drive_iris_to_url_for_published_target(
            "npub1owner",
            "apps/iris ui",
            Some("docs/Read me.md"),
            Some("001122"),
        ),
        "https://drive.iris.to/#/npub1owner/apps%2Firis%20ui/docs/Read%20me.md?k=001122"
    );
}

#[test]
fn test_build_sites_iris_to_url_for_add_route_encodes_path_segments() {
    assert_eq!(
        build_sites_iris_to_url_for_add_route("nhash1example/My notes/index.html"),
        "https://sites.iris.to/#/nhash1example/My%20notes/index.html"
    );
}

#[test]
fn test_build_sites_iris_to_url_for_published_ref_enables_auto_reload() {
    assert_eq!(
        build_sites_iris_to_url_for_published_ref("npub1owner", "apps/iris ui", "index.html"),
        "https://sites.iris.to/#/npub1owner/apps%2Firis%20ui/index.html?reload=1"
    );
}

#[test]
fn test_render_add_output_for_published_site_uses_single_mutable_link_block() {
    let rendered = render_add_output(
        "dist",
        "nhash1immutable",
        "nhash1immutable",
        "abc123",
        Some("def456"),
        Some("index.html"),
        Some(PublishedAddSummary {
            nostr_key: "npub1owner/otus",
            npub: "npub1owner",
            ref_name: "otus",
            identity_was_generated: false,
        }),
    );

    assert_eq!(rendered.matches("  drive:").count(), 1, "{rendered}");
    assert_eq!(rendered.matches("  site:").count(), 1, "{rendered}");
    assert!(
        !rendered.contains("  drive: https://drive.iris.to/#/nhash1immutable"),
        "{rendered}"
    );
    assert!(rendered.contains("  url:   nhash1immutable\n"));
    assert!(rendered.contains("  published: npub1owner/otus\n"));
    assert!(rendered.contains("  drive: https://drive.iris.to/#/npub1owner/otus\n"));
    assert!(
        rendered.contains("  site:  https://sites.iris.to/#/npub1owner/otus/index.html?reload=1\n")
    );
    assert!(rendered.contains("  permalink: https://sites.iris.to/#/nhash1immutable/index.html\n"));
}

#[test]
fn test_parse_published_target_decodes_slash_containing_tree_names() {
    assert_eq!(
        parse_published_target(
            "htree://npub1owner/releases%2Fnostr-vpn/v0.3.0/assets/nostr-vpn-v0.3.0-macos-arm64.zip",
        ),
        Some(UpdateRef {
            npub: "npub1owner".to_string(),
            tree_name: "releases/nostr-vpn".to_string(),
            path: Some("v0.3.0/assets/nostr-vpn-v0.3.0-macos-arm64.zip".to_string()),
        })
    );
}

#[test]
fn test_detect_site_entry_for_path_finds_html_file() {
    let temp_dir = tempfile::tempdir().unwrap();
    let html_path = temp_dir.path().join("Landing.HTM");
    std::fs::write(&html_path, "<!doctype html>").unwrap();

    assert_eq!(
        detect_site_entry_for_path(&html_path, false),
        Some("Landing.HTM".to_string())
    );
}

#[test]
fn test_detect_site_entry_for_path_finds_directory_index_file() {
    let temp_dir = tempfile::tempdir().unwrap();
    std::fs::write(temp_dir.path().join("INDEX.HTML"), "<!doctype html>").unwrap();
    std::fs::write(temp_dir.path().join("notes.txt"), "not a site").unwrap();

    assert_eq!(
        detect_site_entry_for_path(temp_dir.path(), true),
        Some("INDEX.HTML".to_string())
    );
}

#[test]
fn test_detect_site_entry_for_path_skips_non_site_targets() {
    let temp_dir = tempfile::tempdir().unwrap();
    let text_path = temp_dir.path().join("notes.txt");
    std::fs::write(&text_path, "hello").unwrap();

    assert_eq!(detect_site_entry_for_path(&text_path, false), None);
    assert_eq!(detect_site_entry_for_path(temp_dir.path(), true), None);
}

#[test]
fn test_cli_parses_cashu_topup_and_mint_commands() {
    let cli = Cli::parse_from([
        "htree",
        "cashu",
        "topup",
        "123",
        "--mint",
        "https://mint.example",
    ]);
    match cli.command {
        Commands::Cashu {
            command: CashuCommands::Topup { amount_sat, mint },
        } => {
            assert_eq!(amount_sat, 123);
            assert_eq!(mint.as_deref(), Some("https://mint.example"));
        }
        _ => panic!("expected cashu topup command"),
    }

    let cli = Cli::parse_from([
        "htree",
        "cashu",
        "mint",
        "add",
        "https://mint.example",
        "--default",
    ]);
    match cli.command {
        Commands::Cashu {
            command:
                CashuCommands::Mint {
                    command: CashuMintCommands::Add { url, make_default },
                },
        } => {
            assert_eq!(url, "https://mint.example");
            assert!(make_default);
        }
        _ => panic!("expected cashu mint add command"),
    }
}

#[test]
fn test_cli_parses_release_publish_command() {
    let cli = Cli::parse_from([
        "htree",
        "release",
        "publish",
        "releases/hashtree",
        "releases/v0.2.3",
        "nhash1qqsq9qxpq9qcrsszg2pvxq6rs0zqg3yyc5fc5z0knh0wlh",
        "--local",
    ]);

    match cli.command {
        Commands::Release {
            command:
                ReleaseCommands::Publish {
                    tree_name,
                    version_path,
                    cid,
                    draft,
                    local,
                },
        } => {
            assert_eq!(tree_name, "releases/hashtree");
            assert_eq!(version_path, "releases/v0.2.3");
            assert_eq!(cid, "nhash1qqsq9qxpq9qcrsszg2pvxq6rs0zqg3yyc5fc5z0knh0wlh");
            assert!(!draft);
            assert!(local);
        }
        _ => panic!("expected release publish command"),
    }
}

#[test]
fn test_cli_parses_release_publish_draft_flag() {
    let cli = Cli::parse_from([
        "htree",
        "release",
        "publish",
        "releases/hashtree",
        "releases/v0.2.4-rc.1",
        "nhash1qqsq9qxpq9qcrsszg2pvxq6rs0zqg3yyc5fc5z0knh0wlh",
        "--draft",
    ]);

    match cli.command {
        Commands::Release {
            command:
                ReleaseCommands::Publish {
                    tree_name,
                    version_path,
                    cid,
                    draft,
                    local,
                },
        } => {
            assert_eq!(tree_name, "releases/hashtree");
            assert_eq!(version_path, "releases/v0.2.4-rc.1");
            assert_eq!(cid, "nhash1qqsq9qxpq9qcrsszg2pvxq6rs0zqg3yyc5fc5z0knh0wlh");
            assert!(draft);
            assert!(!local);
        }
        _ => panic!("expected release publish command"),
    }
}

#[test]
fn test_cli_parses_push_force_flag() {
    let cli = Cli::parse_from([
        "htree",
        "push",
        "nhash1qqsq9qxpq9qcrsszg2pvxq6rs0zqg3yyc5fc5z0knh0wlh",
        "--server",
        "https://upload.example",
        "--force",
    ]);

    match cli.command {
        Commands::Push {
            cid,
            server,
            force,
            shallow,
        } => {
            assert_eq!(cid, "nhash1qqsq9qxpq9qcrsszg2pvxq6rs0zqg3yyc5fc5z0knh0wlh");
            assert_eq!(server.as_deref(), Some("https://upload.example"));
            assert!(force);
            assert!(!shallow);
        }
        _ => panic!("expected push command"),
    }
}

#[test]
fn test_cli_parses_push_shallow_flag() {
    let cli = Cli::parse_from(["htree", "push", "abc123", "--shallow"]);

    match cli.command {
        Commands::Push {
            cid,
            server,
            force,
            shallow,
        } => {
            assert_eq!(cid, "abc123");
            assert_eq!(server, None);
            assert!(!force);
            assert!(shallow);
        }
        _ => panic!("expected push command"),
    }
}

#[test]
fn test_cli_parses_mirror_commands() {
    let cli = Cli::parse_from([
        "htree",
        "mirror",
        "add",
        "npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm",
    ]);
    match cli.command {
        Commands::Mirror {
            command: MirrorCommands::Add { npub },
        } => {
            assert_eq!(
                npub,
                "npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm"
            );
        }
        _ => panic!("expected mirror add command"),
    }

    let cli = Cli::parse_from(["htree", "mirror", "ls"]);
    match cli.command {
        Commands::Mirror {
            command: MirrorCommands::Ls,
        } => {}
        _ => panic!("expected mirror ls command"),
    }
}

#[test]
fn test_cli_parses_load_command() {
    let cli = Cli::parse_from(["htree", "load", "htree://self/releases%2Fapp/index.html"]);

    match cli.command {
        Commands::Load { cid } => {
            assert_eq!(cid, "htree://self/releases%2Fapp/index.html");
        }
        _ => panic!("expected load command"),
    }
}

#[cfg(feature = "fuse")]
#[test]
fn test_cli_parses_mount_command_without_explicit_mountpoint() {
    let cli = Cli::parse_from(["htree", "mount", "htree://self/mydir"]);

    match cli.command {
        Commands::Mount {
            target, mountpoint, ..
        } => {
            assert_eq!(target, "htree://self/mydir");
            assert_eq!(mountpoint, None);
        }
        _ => panic!("expected mount command"),
    }
}

#[cfg(feature = "fuse")]
#[test]
fn test_is_stale_mount_io_error_matches_device_not_configured() {
    let stale = io::Error::from_raw_os_error(6);
    let other = io::Error::from_raw_os_error(2);

    assert!(is_stale_mount_io_error(&stale));
    assert!(!is_stale_mount_io_error(&other));
}

#[cfg(feature = "fuse")]
#[test]
fn test_find_existing_active_mount_matches_mountpoint() {
    let mounts = vec![
        ActiveMount {
            target: "npub1example/other".to_string(),
            mountpoint: PathBuf::from("/tmp/other"),
            mounted_cid: "nhash1other".to_string(),
            visibility: "public".to_string(),
            published_key: None,
            allow_other: false,
            pid: 1,
            registered_at: 1,
        },
        ActiveMount {
            target: "npub1example/mount-test".to_string(),
            mountpoint: PathBuf::from("/tmp/mount-test"),
            mounted_cid: "nhash1match".to_string(),
            visibility: "public".to_string(),
            published_key: Some("npub1example/mount-test".to_string()),
            allow_other: false,
            pid: 2,
            registered_at: 2,
        },
    ];

    let found = find_existing_active_mount(&mounts, &PathBuf::from("/tmp/mount-test"))
        .expect("matching mount");
    assert_eq!(found.target, "npub1example/mount-test");
    assert_eq!(found.mounted_cid, "nhash1match");
}

#[cfg(feature = "fuse")]
#[test]
fn test_should_warn_for_temporary_mountpoint_matches_temp_locations() {
    assert!(should_warn_for_temporary_mountpoint(&PathBuf::from(
        "/tmp/mount-test"
    )));
    assert!(should_warn_for_temporary_mountpoint(&PathBuf::from(
        "/private/tmp/mount-test"
    )));
    assert!(!should_warn_for_temporary_mountpoint(&PathBuf::from(
        "/Users/martti/mount-test"
    )));
}

#[test]
fn test_cli_parses_mounts_command() {
    let cli = Cli::parse_from(["htree", "mounts"]);

    match cli.command {
        Commands::Mounts { json } => {
            assert!(!json);
        }
        _ => panic!("expected mounts command"),
    }
}

#[test]
fn test_cli_parses_mounts_command_with_json_output() {
    let cli = Cli::parse_from(["htree", "mounts", "--json"]);

    match cli.command {
        Commands::Mounts { json } => {
            assert!(json);
        }
        _ => panic!("expected mounts command"),
    }
}

#[test]
fn test_cli_help_groups_commands_by_purpose() {
    let mut cmd = Cli::command();
    let help = cmd.render_long_help().to_string();

    assert!(help.contains("Daemon Commands:"));
    assert!(help.contains("reload       Reload daemon config by restarting with saved launch args"));
    assert!(help.contains("Content Commands:"));
    assert!(help.contains("Storage Commands:"));
    assert!(help.contains("mounts       List active hashtree mounts"));
    assert!(help.contains("Publishing & Git Commands:"));
    assert!(help.contains("Identity & Social Commands:"));
    assert!(help.contains("Wallet Commands:"));
    assert!(help.contains("General Commands:"));
    assert!(!help.contains("\nCommands:\n"));
}

#[test]
fn test_cli_parses_reload_command() {
    let cli = Cli::parse_from(["htree", "reload"]);

    match cli.command {
        Commands::Reload { pid_file } => {
            assert_eq!(pid_file, None);
        }
        _ => panic!("expected reload command"),
    }
}

#[test]
fn test_cli_parses_repos_command_default_owner() {
    let cli = Cli::parse_from(["htree", "repos"]);

    match cli.command {
        Commands::Repos { owner } => {
            assert_eq!(owner, None);
        }
        _ => panic!("expected repos command"),
    }
}

#[test]
fn test_cli_parses_repos_command_with_owner() {
    let cli = Cli::parse_from(["htree", "repos", "coworker"]);

    match cli.command {
        Commands::Repos { owner } => {
            assert_eq!(owner.as_deref(), Some("coworker"));
        }
        _ => panic!("expected repos command"),
    }
}

#[test]
fn test_cli_parses_socialgraph_index_command() {
    let cli = Cli::parse_from([
        "htree",
        "socialgraph",
        "index",
        "--warm-secs",
        "15",
        "--crawl-depth",
        "2",
        "--full-graph-recrawl",
        "--max-follow-distance",
        "2",
        "--max-authors",
        "48",
        "--max-authors-per-run",
        "1024",
        "--max-live-mb",
        "128",
        "--per-author-event-limit",
        "64",
        "--per-author-kind-event-limit",
        "1",
        "--per-author-live-bytes",
        "65536",
        "--author-batch-size",
        "32",
        "--checkpoint-authors",
        "12",
        "--index-commit-batch-size",
        "32768",
        "--stage-only",
        "--staging-data-dir",
        "/srv/staging",
        "--projection-authors",
        "96",
        "--projection-event-limit",
        "131072",
        "--projection-follow",
        "--btree-order",
        "256",
        "--btree-update-concurrency",
        "1",
        "--concurrent-batches",
        "6",
        "--fetch-timeout-secs",
        "7",
        "--relay-event-max-bytes",
        "262144",
        "--global-relay-scan",
        "--author-allowlist-url",
        "https://graph-api.iris.to/allowlist?maxDistance=6",
        "--negentropy-only",
        "--relay-page-size",
        "2000",
        "--max-relay-pages",
        "6",
        "--max-events-seen",
        "1000000",
        "--kind",
        "1",
        "--kind",
        "6",
        "--relay",
        "wss://relay.example",
        "--relay",
        "wss://relay.two",
    ]);

    match cli.command {
        Commands::Socialgraph {
            command: SocialGraphCommands::Index { options },
        } => {
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
                author_allowlist_url,
                negentropy_only,
                full_author_history,
                relay_page_size,
                max_relay_pages,
                max_events_seen,
                kinds,
                relays,
            } = *options;
            assert_eq!(warm_secs, 15);
            assert_eq!(crawl_depth, Some(2));
            assert!(full_graph_recrawl);
            assert_eq!(max_follow_distance, Some(2));
            assert_eq!(max_authors, 48);
            assert_eq!(max_authors_per_run, Some(1_024));
            assert_eq!(max_live_mb, 128);
            assert_eq!(per_author_event_limit, 64);
            assert_eq!(per_author_kind_event_limit, Some(1));
            assert_eq!(per_author_live_bytes, Some(65_536));
            assert_eq!(author_batch_size, 32);
            assert_eq!(checkpoint_authors, 12);
            assert_eq!(index_commit_batch_size, 32_768);
            assert!(stage_only);
            assert!(!project_staged);
            assert!(!bulk_project_staged);
            assert_eq!(staging_data_dir, Some(PathBuf::from("/srv/staging")));
            assert_eq!(projection_authors, 96);
            assert_eq!(projection_event_limit, 131_072);
            assert!(projection_follow);
            assert_eq!(btree_order, 256);
            assert_eq!(btree_update_concurrency, 1);
            assert_eq!(concurrent_batches, 6);
            assert_eq!(fetch_timeout_secs, 7);
            assert_eq!(relay_event_max_bytes, Some(262_144));
            assert!(global_relay_scan);
            assert_eq!(
                author_allowlist_url.as_deref(),
                Some("https://graph-api.iris.to/allowlist?maxDistance=6")
            );
            assert!(negentropy_only);
            assert!(!full_author_history);
            assert_eq!(relay_page_size, 2_000);
            assert_eq!(max_relay_pages, 6);
            assert_eq!(max_events_seen, Some(1_000_000));
            assert_eq!(kinds, vec![1, 6]);
            assert_eq!(
                relays,
                vec![
                    "wss://relay.example".to_string(),
                    "wss://relay.two".to_string()
                ]
            );
        }
        _ => panic!("expected socialgraph index command"),
    }
}

#[test]
fn test_cli_parses_bulk_staged_projection() {
    let cli = Cli::parse_from([
        "htree",
        "socialgraph",
        "index",
        "--project-staged",
        "--bulk-project-staged",
    ]);

    match cli.command {
        Commands::Socialgraph {
            command: SocialGraphCommands::Index { options },
        } => {
            assert!(options.project_staged);
            assert!(options.bulk_project_staged);
            assert!(!options.projection_follow);
        }
        _ => panic!("expected socialgraph index command"),
    }

    assert!(
        Cli::try_parse_from(["htree", "socialgraph", "index", "--bulk-project-staged",]).is_err()
    );

    let following = Cli::parse_from([
        "htree",
        "socialgraph",
        "index",
        "--project-staged",
        "--bulk-project-staged",
        "--projection-follow",
    ]);
    match following.command {
        Commands::Socialgraph {
            command: SocialGraphCommands::Index { options },
        } => {
            assert!(options.bulk_project_staged);
            assert!(options.projection_follow);
        }
        _ => panic!("expected socialgraph index command"),
    }
}

#[test]
fn test_cli_add_uses_unencrypted_flag_with_public_alias() {
    let cli = Cli::parse_from(["htree", "add", "site", "--unencrypted"]);
    match cli.command {
        Commands::Add { unencrypted, .. } => assert!(unencrypted),
        _ => panic!("expected add command"),
    }

    let cli = Cli::parse_from(["htree", "add", "site", "--public"]);
    match cli.command {
        Commands::Add { unencrypted, .. } => assert!(unencrypted),
        _ => panic!("expected add command"),
    }
}

#[test]
fn test_cli_add_parses_chunk_size_override() {
    let cli = Cli::parse_from(["htree", "add", "site", "--chunk-size", "33554432"]);
    match cli.command {
        Commands::Add { chunk_size, .. } => assert_eq!(chunk_size, Some(33_554_432)),
        _ => panic!("expected add command"),
    }
}

#[test]
fn test_cli_add_no_blossom_push_alias_sets_local() {
    let cli = Cli::parse_from(["htree", "add", "site", "--no-blossom-push"]);
    match cli.command {
        Commands::Add { local, .. } => assert!(local),
        _ => panic!("expected add command"),
    }
}

#[test]
fn test_cli_parses_socialgraph_rebuild_profile_index_command() {
    let cli = Cli::parse_from(["htree", "socialgraph", "rebuild-profile-index"]);

    match cli.command {
        Commands::Socialgraph {
            command: SocialGraphCommands::RebuildProfileIndex,
        } => {}
        _ => panic!("expected socialgraph rebuild-profile-index command"),
    }
}

#[test]
fn test_cli_parses_socialgraph_publish_profile_indexes_command() {
    let cli = Cli::parse_from(["htree", "socialgraph", "publish-profile-indexes"]);

    match cli.command {
        Commands::Socialgraph {
            command: SocialGraphCommands::PublishProfileIndexes,
        } => {}
        _ => panic!("expected socialgraph publish-profile-indexes command"),
    }
}

#[test]
fn test_cli_parses_socialgraph_warm_command() {
    let cli = Cli::parse_from([
        "htree",
        "socialgraph",
        "warm",
        "--secs",
        "90",
        "--crawl-depth",
        "4",
        "--full-graph-recrawl",
        "--relay",
        "wss://relay.example",
        "--author-batch-size",
        "128",
        "--concurrent-batches",
        "5",
    ]);

    match cli.command {
        Commands::Socialgraph {
            command:
                SocialGraphCommands::Warm {
                    secs,
                    crawl_depth,
                    full_graph_recrawl,
                    relays,
                    author_batch_size,
                    concurrent_batches,
                },
        } => {
            assert_eq!(secs, 90);
            assert_eq!(crawl_depth, Some(4));
            assert!(full_graph_recrawl);
            assert_eq!(relays, vec!["wss://relay.example".to_string()]);
            assert_eq!(author_batch_size, 128);
            assert_eq!(concurrent_batches, 5);
        }
        _ => panic!("expected socialgraph warm command"),
    }
}

#[test]
fn test_cli_parses_socialgraph_stats_command() {
    let cli = Cli::parse_from(["htree", "socialgraph", "stats"]);

    match cli.command {
        Commands::Socialgraph {
            command: SocialGraphCommands::Stats,
        } => {}
        _ => panic!("expected socialgraph stats command"),
    }
}

#[tokio::test]
async fn test_resolve_nhash_with_path_suffix() {
    // nhash for hash [0xaa; 32]
    let nhash = hashtree_core::nhash_encode(&[0xaa; 32]).unwrap();

    // Test nhash without path
    let resolved = resolve_cid_input(&nhash).await.unwrap();
    assert_eq!(resolved.cid.hash, [0xaa; 32]);
    assert!(resolved.path.is_none());

    // Test nhash with single file path suffix
    let with_path = format!("{}/bitcoin.pdf", nhash);
    let resolved = resolve_cid_input(&with_path).await.unwrap();
    assert_eq!(resolved.cid.hash, [0xaa; 32]);
    assert_eq!(resolved.path, Some("bitcoin.pdf".to_string()));

    // Test nhash with nested path suffix
    let with_nested = format!("{}/docs/papers/bitcoin.pdf", nhash);
    let resolved = resolve_cid_input(&with_nested).await.unwrap();
    assert_eq!(resolved.cid.hash, [0xaa; 32]);
    assert_eq!(resolved.path, Some("docs/papers/bitcoin.pdf".to_string()));
}

#[tokio::test]
async fn test_resolve_nhash_with_htree_prefix() {
    let nhash = hashtree_core::nhash_encode(&[0xbb; 32]).unwrap();

    // Test htree:// prefix with path
    let htree_url = format!("htree://{}/file.txt", nhash);
    let resolved = resolve_cid_input(&htree_url).await.unwrap();
    assert_eq!(resolved.cid.hash, [0xbb; 32]);
    assert_eq!(resolved.path, Some("file.txt".to_string()));
}

#[tokio::test]
async fn test_resolve_hex_cid_with_key_and_path() {
    let hash = [0x11; 32];
    let key = [0x22; 32];
    let hash_hex = hashtree_core::to_hex(&hash);
    let key_hex = hashtree_core::to_hex(&key);
    let cid = format!("{}:{}", hash_hex, key_hex);

    let resolved = resolve_cid_input(&cid).await.unwrap();
    assert_eq!(resolved.cid.hash, hash);
    assert_eq!(resolved.cid.key, Some(key));
    assert!(resolved.path.is_none());

    let with_path = format!("{}/dir/file.txt", cid);
    let resolved = resolve_cid_input(&with_path).await.unwrap();
    assert_eq!(resolved.cid.hash, hash);
    assert_eq!(resolved.cid.key, Some(key));
    assert_eq!(resolved.path, Some("dir/file.txt".to_string()));
}

#[tokio::test]
async fn test_resolve_hex_cid_without_key() {
    let hash = [0x33; 32];
    let hash_hex = hashtree_core::to_hex(&hash);
    let resolved = resolve_cid_input(&hash_hex).await.unwrap();
    assert_eq!(resolved.cid.hash, hash);
    assert!(resolved.cid.key.is_none());
}

#[tokio::test]
async fn test_resolve_cat_target_cid_resolves_tree_paths_with_decryption_key() {
    let tmp = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(HashtreeStore::new(tmp.path().join("store")).unwrap());

    let site_dir = tmp.path().join("site");
    std::fs::create_dir_all(&site_dir).unwrap();
    let expected = br#"{"songs":9529}"#;
    std::fs::write(site_dir.join("root.json"), expected).unwrap();

    let root = store
        .upload_dir_encrypted_with_options(&site_dir, true)
        .expect("upload encrypted dir");
    let resolved = ResolvedCid {
        cid: Cid::parse(&root).expect("parse encrypted root cid"),
        path: Some("root.json".to_string()),
    };

    let fetcher = Fetcher::new(FetchConfig::default());
    let target = resolve_cat_target_cid(&fetcher, &store, &resolved)
        .await
        .expect("resolve cat target");

    assert!(
        target.key.is_some(),
        "resolved file cid should preserve decrypt key"
    );

    let mut output = Vec::new();
    store
        .write_file_by_cid_to_writer(&target, &mut output)
        .expect("stream decrypted file");
    assert_eq!(output, expected);
}

#[tokio::test]
async fn test_resolve_load_target_cid_resolves_tree_paths_with_decryption_key() {
    let tmp = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(HashtreeStore::new(tmp.path().join("store")).unwrap());

    let site_dir = tmp.path().join("site");
    std::fs::create_dir_all(&site_dir).unwrap();
    let expected = br#"{"songs":9529}"#;
    std::fs::write(site_dir.join("root.json"), expected).unwrap();

    let root = store
        .upload_dir_encrypted_with_options(&site_dir, true)
        .expect("upload encrypted dir");
    let resolved = ResolvedCid {
        cid: Cid::parse(&root).expect("parse encrypted root cid"),
        path: Some("root.json".to_string()),
    };

    let fetcher = Fetcher::new(FetchConfig::default());
    let target = resolve_load_target_cid(&fetcher, &store, &resolved, None)
        .await
        .expect("resolve load target");

    assert!(
        target.key.is_some(),
        "resolved file cid should preserve decrypt key"
    );

    let mut output = Vec::new();
    store
        .write_file_by_cid_to_writer(&target, &mut output)
        .expect("stream decrypted file");
    assert_eq!(output, expected);
}

#[tokio::test]
async fn test_resolve_load_target_cid_keeps_file_root_when_input_has_display_path() {
    let tmp = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(HashtreeStore::new(tmp.path().join("store")).unwrap());

    let source = tmp.path().join("notes.txt");
    std::fs::write(&source, "hello from load").unwrap();

    let cid = store
        .upload_file_encrypted(&source)
        .expect("upload encrypted file");
    let parsed = Cid::parse(&cid).expect("parse encrypted file cid");
    let resolved = ResolvedCid {
        cid: parsed.clone(),
        path: Some("notes.txt".to_string()),
    };

    let fetcher = Fetcher::new(FetchConfig::default());
    let target = resolve_load_target_cid(&fetcher, &store, &resolved, None)
        .await
        .expect("resolve file-root load target");

    assert_eq!(target, parsed);
}

#[tokio::test]
async fn test_resolve_info_target_resolves_tree_paths_with_decryption_key() {
    let tmp = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(HashtreeStore::new(tmp.path().join("store")).unwrap());

    let site_dir = tmp.path().join("site");
    std::fs::create_dir_all(&site_dir).unwrap();
    let expected = br#"{"songs":9529}"#;
    std::fs::write(site_dir.join("root.json"), expected).unwrap();

    let root = store
        .upload_dir_encrypted_with_options(&site_dir, true)
        .expect("upload encrypted dir");
    let root_cid = Cid::parse(&root).expect("parse encrypted root cid");

    let fetcher = Fetcher::new(FetchConfig::default());
    let target = resolve_info_target(&store, &fetcher, &root_cid, Some("root.json"))
        .await
        .expect("resolve info target");

    assert!(
        target.key.is_some(),
        "resolved file cid should preserve decrypt key"
    );

    let mut output = Vec::new();
    store
        .write_file_by_cid_to_writer(&target, &mut output)
        .expect("stream decrypted file");
    assert_eq!(output, expected);
}

#[tokio::test]
async fn test_resolve_cat_target_cid_rejects_directories_without_path() {
    let tmp = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(HashtreeStore::new(tmp.path().join("store")).unwrap());

    let site_dir = tmp.path().join("site");
    std::fs::create_dir_all(&site_dir).unwrap();
    std::fs::write(site_dir.join("index.html"), "<html></html>").unwrap();

    let root = store
        .upload_dir_encrypted_with_options(&site_dir, true)
        .expect("upload encrypted dir");
    let resolved = ResolvedCid {
        cid: Cid::parse(&root).expect("parse encrypted root cid"),
        path: None,
    };

    let fetcher = Fetcher::new(FetchConfig::default());
    let err = resolve_cat_target_cid(&fetcher, &store, &resolved)
        .await
        .expect_err("catting a directory should fail");
    assert!(err.to_string().contains("Cannot cat a directory"));
}

#[tokio::test]
async fn test_pin_published_repo_indexes_named_ref_and_unpins_stored_root() {
    let tmp = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(HashtreeStore::new(tmp.path().join("store")).unwrap());

    let first_dir = tmp.path().join("repo-v1");
    std::fs::create_dir_all(&first_dir).unwrap();
    std::fs::write(first_dir.join("README.md"), "version one\n").unwrap();
    let first_root = store
        .upload_dir_with_options(&first_dir, true)
        .expect("upload first repo root");
    let first_cid = Cid::parse(&first_root).expect("parse first root cid");
    store.unpin(&first_cid.hash).expect("clear upload auto-pin");

    let second_dir = tmp.path().join("repo-v2");
    std::fs::create_dir_all(&second_dir).unwrap();
    std::fs::write(second_dir.join("README.md"), "version two\n").unwrap();
    let second_root = store
        .upload_dir_with_options(&second_dir, true)
        .expect("upload second repo root");
    let second_cid = Cid::parse(&second_root).expect("parse second root cid");
    store
        .unpin(&second_cid.hash)
        .expect("clear upload auto-pin");

    let keys = Keys::generate();
    let npub = NostrToBech32::to_bech32(&keys.public_key()).expect("encode npub");
    let pubkey_hex = hex::encode(keys.public_key().to_bytes());
    let repo_target = format!("{npub}/repo");

    store
        .set_cached_root(
            &pubkey_hex,
            "repo",
            &hex::encode(first_cid.hash),
            None,
            "public",
            1,
        )
        .expect("cache first root");

    let resolved = resolve_cid_input_with_opts(
        &repo_target,
        &ResolveOptions {
            data_dir: Some(tmp.path().join("store")),
            relays: Some(Vec::new()),
            ..ResolveOptions::default()
        },
    )
    .await
    .expect("resolve cached repo target");

    let fetcher = Fetcher::new(FetchConfig::default());
    let pinned = pin_input_target(&store, &fetcher, &repo_target, &resolved)
        .await
        .expect("pin published repo");

    assert_eq!(pinned.hash, first_cid.hash);
    assert!(store.is_pinned(&first_cid.hash).expect("first root pinned"));
    assert_eq!(
        store
            .get_tree_ref(&repo_target)
            .expect("stored tree ref lookup"),
        Some(first_cid.hash)
    );
    assert_eq!(
        store.list_pinned_refs().expect("list pinned refs"),
        vec![repo_target.clone()]
    );
    assert_eq!(
        store.list_pins_with_names().expect("list pins")[0].name,
        repo_target
    );

    store
        .set_cached_root(
            &pubkey_hex,
            "repo",
            &hex::encode(second_cid.hash),
            None,
            "public",
            2,
        )
        .expect("cache newer root");

    let stored_hash = stored_published_pin_hash(store.as_ref(), &repo_target)
        .expect("stored published pin lookup")
        .expect("stored published root");
    assert_eq!(stored_hash, first_cid.hash);

    store
        .unpin(&stored_hash)
        .expect("unpin stored published root");
    store
        .remove_pinned_ref(&repo_target)
        .expect("remove pinned ref");
    assert!(
        !store
            .is_pinned(&first_cid.hash)
            .expect("first root pin status"),
        "unpin should remove the originally pinned root even after the cached mutable ref changes"
    );
    assert!(
        !store
            .is_pinned(&second_cid.hash)
            .expect("second root pin status"),
        "newer cached root should not be touched by unpinning the stored ref"
    );
    assert!(
        store
            .list_pinned_refs()
            .expect("list pinned refs")
            .is_empty(),
        "unpinned published refs should be removed from the live pinned-ref set"
    );
}
