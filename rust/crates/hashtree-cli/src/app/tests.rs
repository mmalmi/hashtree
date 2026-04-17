use super::add::{
    build_files_iris_to_url_for_add_route, build_files_iris_to_url_for_published_ref,
    build_files_iris_to_url_for_published_target, build_sites_iris_to_url_for_add_route,
    build_sites_iris_to_url_for_published_ref, detect_site_entry_for_path,
};
use super::daemonize::{build_daemon_args, parse_pid, read_pid_file, write_pid_file};
use super::lists::{
    build_mute_list_event, load_mute_entries, update_hex_list_file,
    update_mute_list_file_with_status, MuteEntry, MuteUpdate,
};
use super::resolve::{
    parse_published_target, resolve_cid_input, resolve_cid_input_with_opts, ParsedPublishedTarget,
    ResolveOptions, ResolvedCid,
};
#[cfg(feature = "fuse")]
use super::run::{
    find_existing_active_mount, is_stale_mount_io_error, should_warn_for_temporary_mountpoint,
};
use super::run::{
    format_cid_for_display, pin_input_target, resolve_cat_target_cid, resolve_load_target_cid,
    stored_published_pin_hash, warn_if_stun_unavailable,
};
use crate::app::args::{CashuCommands, CashuMintCommands, ReleaseCommands, SocialGraphCommands};
use crate::app::args::{Cli, Commands};
#[cfg(feature = "fuse")]
use crate::app::mount_registry::ActiveMount;
use clap::{CommandFactory, Parser};
use hashtree_cli::config::ServerMode;
use hashtree_cli::{Config as AppConfig, FetchConfig, Fetcher, HashtreeStore, NostrToBech32};
use hashtree_core::{nhash_decode, Cid};
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
fn test_build_daemon_args_with_overrides() {
    let data_dir = PathBuf::from("data-dir");
    let args = args_to_strings(build_daemon_args(
        "127.0.0.1:8080",
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
    let args = args_to_strings(build_daemon_args("0.0.0.0:8080", None, None, None));
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

#[test]
fn test_warn_if_stun_unavailable_disables_stun_listener_when_feature_missing() {
    let mut config = AppConfig::default();
    config.server.enable_webrtc = true;
    config.server.stun_port = 3478;

    warn_if_stun_unavailable(&mut config);

    #[cfg(not(feature = "stun"))]
    assert_eq!(config.server.stun_port, 0);

    #[cfg(feature = "stun")]
    assert_eq!(config.server.stun_port, 3478);
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
fn test_build_files_iris_to_url_for_add_route_encodes_path_segments() {
    assert_eq!(
        build_files_iris_to_url_for_add_route("nhash1example/My notes/index.html"),
        "https://files.iris.to/#/nhash1example/My%20notes/index.html"
    );
}

#[test]
fn test_build_files_iris_to_url_for_published_ref_encodes_tree_name_as_single_segment() {
    assert_eq!(
        build_files_iris_to_url_for_published_ref("npub1owner", "apps/iris ui",),
        "https://files.iris.to/#/npub1owner/apps%2Firis%20ui"
    );
}

#[test]
fn test_build_files_iris_to_url_for_published_target_includes_path_and_link_key() {
    assert_eq!(
        build_files_iris_to_url_for_published_target(
            "npub1owner",
            "apps/iris ui",
            Some("docs/Read me.md"),
            Some("001122"),
        ),
        "https://files.iris.to/#/npub1owner/apps%2Firis%20ui/docs/Read%20me.md?k=001122"
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
fn test_parse_published_target_decodes_slash_containing_tree_names() {
    assert_eq!(
        parse_published_target(
            "htree://npub1owner/releases%2Fnostr-vpn/v0.3.0/assets/nostr-vpn-v0.3.0-macos-arm64.zip",
        ),
        Some(ParsedPublishedTarget {
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
                    local,
                },
        } => {
            assert_eq!(tree_name, "releases/hashtree");
            assert_eq!(version_path, "releases/v0.2.3");
            assert_eq!(cid, "nhash1qqsq9qxpq9qcrsszg2pvxq6rs0zqg3yyc5fc5z0knh0wlh");
            assert!(local);
        }
        _ => panic!("expected release publish command"),
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
        "--max-live-mb",
        "128",
        "--per-author-event-limit",
        "64",
        "--per-author-live-bytes",
        "65536",
        "--author-batch-size",
        "32",
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
            command:
                SocialGraphCommands::Index {
                    warm_secs,
                    crawl_depth,
                    full_graph_recrawl,
                    max_follow_distance,
                    max_authors,
                    max_live_mb,
                    per_author_event_limit,
                    per_author_live_bytes,
                    author_batch_size,
                    concurrent_batches,
                    fetch_timeout_secs,
                    relay_event_max_bytes,
                    global_relay_scan,
                    author_allowlist_url,
                    negentropy_only,
                    relay_page_size,
                    max_relay_pages,
                    max_events_seen,
                    kinds,
                    relays,
                },
        } => {
            assert_eq!(warm_secs, 15);
            assert_eq!(crawl_depth, Some(2));
            assert!(full_graph_recrawl);
            assert_eq!(max_follow_distance, Some(2));
            assert_eq!(max_authors, 48);
            assert_eq!(max_live_mb, 128);
            assert_eq!(per_author_event_limit, 64);
            assert_eq!(per_author_live_bytes, Some(65_536));
            assert_eq!(author_batch_size, 32);
            assert_eq!(concurrent_batches, 6);
            assert_eq!(fetch_timeout_secs, 7);
            assert_eq!(relay_event_max_bytes, Some(262_144));
            assert!(global_relay_scan);
            assert_eq!(
                author_allowlist_url.as_deref(),
                Some("https://graph-api.iris.to/allowlist?maxDistance=6")
            );
            assert!(negentropy_only);
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
}
