use anyhow::{Context, Result};
use hashtree_cli::config::ensure_keys_string;
use hashtree_cli::{
    AddProgress, AddProgressSnapshot, Config, HashtreeStore, NostrKeys, NostrResolverConfig,
    NostrRootResolver, NostrToBech32, RootResolver, PRIORITY_OWN,
};
use hashtree_core::{
    from_hex, key_from_hex, nhash_encode, nhash_encode_full, Cid, HashTree, HashTreeConfig,
    NHashData,
};
use std::ffi::OsString;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use super::blossom::background_blossom_push;
use super::content::add_directory_with_progress;
use super::util::format_bytes;

const IRIS_DRIVE_WEB_BASE_URL: &str = "https://drive.iris.to";
const IRIS_SITES_WEB_BASE_URL: &str = "https://sites.iris.to";
const LOCAL_ADD_EXTERNAL_BLOB_MIN_BYTES: &str = "65536";
const LOCAL_ADD_EXTERNAL_BLOB_DIR_NAME: &str = "blob-files-v1";
const LOCAL_ADD_STREAM_BATCH_TARGET_BYTES: &str = "268435456";
const LMDB_NO_READ_AHEAD_ENV: &str = "HTREE_LMDB_NO_READ_AHEAD";
const LMDB_NO_SYNC_ENV: &str = "HTREE_LMDB_NO_SYNC";
const LMDB_NO_META_SYNC_ENV: &str = "HTREE_LMDB_NO_META_SYNC";
const LMDB_EXTERNAL_BLOB_MIN_BYTES_ENV: &str = "HTREE_LMDB_EXTERNAL_BLOB_MIN_BYTES";
const LMDB_EXTERNAL_BLOB_DIR_ENV: &str = "HTREE_LMDB_EXTERNAL_BLOB_DIR";
const LMDB_EXTERNAL_BLOB_SYNC_ENV: &str = "HTREE_LMDB_EXTERNAL_BLOB_SYNC";
const LMDB_EXTERNAL_BLOB_PACK_TARGET_BYTES_ENV: &str = "HTREE_LMDB_EXTERNAL_BLOB_PACK_TARGET_BYTES";
const STREAM_PUT_BATCH_TARGET_BYTES_ENV: &str = "HTREE_STREAM_PUT_BATCH_TARGET_BYTES";

pub(crate) struct PublishedAddSummary<'a> {
    pub(crate) nostr_key: &'a str,
    pub(crate) npub: &'a str,
    pub(crate) ref_name: &'a str,
    pub(crate) identity_was_generated: bool,
}

pub(crate) fn render_add_output(
    path_display: &str,
    display_route: &str,
    display_root: &str,
    hash_hex: &str,
    key_hex: Option<&str>,
    site_entry: Option<&str>,
    published: Option<PublishedAddSummary<'_>>,
) -> String {
    let mut output = String::new();
    output.push_str(&format!("added {path_display}\n"));
    output.push_str(&format!("  url:   {display_route}\n"));

    if published.is_none() {
        output.push_str(&format!(
            "  drive: {}\n",
            build_drive_iris_to_url_for_add_route(display_route)
        ));
        if let Some(entry_path) = site_entry {
            let site_route = format!("{display_root}/{entry_path}");
            output.push_str(&format!(
                "  site:  {}\n",
                build_sites_iris_to_url_for_add_route(&site_route)
            ));
        }
    }

    output.push_str(&format!("  hash:  {hash_hex}\n"));
    if let Some(key_hex) = key_hex {
        output.push_str(&format!("  key:   {key_hex}\n"));
    }

    if let Some(published) = published {
        if published.identity_was_generated {
            output.push_str(&format!("  identity: {} (new)\n", published.npub));
        }
        output.push_str(&format!("  published: {}\n", published.nostr_key));
        output.push_str(&format!(
            "  drive: {}\n",
            build_drive_iris_to_url_for_published_ref(published.npub, published.ref_name)
        ));
        if let Some(entry_path) = site_entry {
            output.push_str(&format!(
                "  site:  {}\n",
                build_sites_iris_to_url_for_published_ref(
                    published.npub,
                    published.ref_name,
                    entry_path,
                )
            ));
            let immutable_site_route = format!("{display_root}/{entry_path}");
            output.push_str(&format!(
                "  permalink: {}\n",
                build_sites_iris_to_url_for_add_route(&immutable_site_route)
            ));
        } else {
            output.push_str(&format!(
                "  permalink: {}\n",
                build_drive_iris_to_url_for_add_route(display_route)
            ));
        }
    }

    output
}

pub(crate) async fn run_add(
    data_dir: PathBuf,
    path: PathBuf,
    only_hash: bool,
    unencrypted: bool,
    no_ignore: bool,
    publish: Option<String>,
    chunk_size: Option<usize>,
    local: bool,
) -> Result<()> {
    let _local_add_env = local.then(|| apply_local_add_env(&data_dir));
    let is_dir = path.is_dir();
    let show_progress = add_progress_enabled();

    if only_hash {
        use futures::executor::block_on as sync_block_on;
        use futures::io::AllowStdIo;
        use hashtree_core::store::MemoryStore;

        let store = Arc::new(MemoryStore::new());
        let mut config = if unencrypted {
            HashTreeConfig::new(store.clone()).public()
        } else {
            HashTreeConfig::new(store.clone())
        };
        if let Some(chunk_size) = chunk_size {
            config = config.with_chunk_size(chunk_size);
        }
        let tree = HashTree::new(config);
        let progress = Arc::new(AddProgress::new());

        if is_dir {
            let cid = run_with_add_progress("Hashing", Arc::clone(&progress), || {
                if show_progress {
                    discover_add_input(&path, !no_ignore, progress.as_ref())?;
                }
                sync_block_on(add_directory_with_progress(
                    &tree,
                    &path,
                    !no_ignore,
                    Some(progress.as_ref()),
                ))
            })?;
            println!("hash: {}", hashtree_core::to_hex(&cid.hash));
            if let Some(key) = cid.key {
                println!("key:  {}", hashtree_core::to_hex(&key));
            }
        } else {
            let cid = run_with_add_progress("Hashing", Arc::clone(&progress), || {
                if show_progress {
                    discover_add_input(&path, !no_ignore, progress.as_ref())?;
                }
                let file = std::fs::File::open(&path).with_context(|| {
                    format!("Failed to open file for hashing: {}", path.display())
                })?;
                let (cid, _size) = sync_block_on(async {
                    tree.put_stream_with_progress(AllowStdIo::new(file), |bytes| {
                        progress.record_bytes(bytes);
                    })
                    .await
                })
                .map_err(|e| anyhow::anyhow!("Failed to hash file: {}", e))?;
                progress.record_file_finished();
                Ok(cid)
            })?;
            println!("hash: {}", hashtree_core::to_hex(&cid.hash));
            if let Some(key) = cid.key {
                println!("key:  {}", hashtree_core::to_hex(&key));
            }
        }

        return Ok(());
    }

    let store = HashtreeStore::new(&data_dir)?;
    let site_entry = detect_site_entry_for_path(&path, is_dir);
    let progress = Arc::new(AddProgress::new());

    let (cid_for_push, hash_hex, key_hex, display_root): (String, String, Option<String>, String) =
        run_with_add_progress("Adding", Arc::clone(&progress), || {
            if show_progress {
                discover_add_input(&path, !no_ignore, progress.as_ref())?;
            }
            if unencrypted {
                let hash_hex = if is_dir {
                    store
                        .upload_dir_with_options_and_chunk_size_and_progress(
                            &path,
                            !no_ignore,
                            chunk_size,
                            progress.as_ref(),
                        )
                        .context("Failed to add directory")?
                } else {
                    store
                        .upload_file_with_chunk_size_and_progress(
                            &path,
                            chunk_size,
                            progress.as_ref(),
                        )
                        .context("Failed to add file")?
                };
                let hash = from_hex(&hash_hex).context("Invalid hash")?;
                let nhash = nhash_encode(&hash)
                    .map_err(|e| anyhow::anyhow!("Failed to encode nhash: {}", e))?;
                Ok((hash_hex.clone(), hash_hex, None, nhash))
            } else {
                let cid_str = if is_dir {
                    store
                        .upload_dir_encrypted_with_options_and_chunk_size_and_progress(
                            &path,
                            !no_ignore,
                            chunk_size,
                            progress.as_ref(),
                        )
                        .context("Failed to add directory")?
                } else {
                    store
                        .upload_file_encrypted_with_chunk_size_and_progress(
                            &path,
                            chunk_size,
                            progress.as_ref(),
                        )
                        .context("Failed to add file")?
                };
                let (hash_hex, key_hex) = if let Some((h, k)) = cid_str.split_once(':') {
                    (h.to_string(), Some(k.to_string()))
                } else {
                    (cid_str.clone(), None)
                };
                let hash = from_hex(&hash_hex).context("Invalid hash")?;
                let key = key_hex
                    .as_ref()
                    .map(|k| key_from_hex(k))
                    .transpose()
                    .map_err(|e| anyhow::anyhow!("Invalid key: {}", e))?;
                let nhash_data = NHashData {
                    hash,
                    decrypt_key: key,
                };
                let nhash = nhash_encode_full(&nhash_data)
                    .map_err(|e| anyhow::anyhow!("Failed to encode nhash: {}", e))?;
                Ok((cid_str, hash_hex, key_hex, nhash))
            }
        })?;

    let path_display = path.display().to_string();
    let display_route = if is_dir {
        display_root.clone()
    } else {
        let filename = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        format!("{display_root}/{filename}")
    };
    if publish.is_none() {
        print!(
            "{}",
            render_add_output(
                &path_display,
                &display_route,
                &display_root,
                &hash_hex,
                key_hex.as_deref(),
                site_entry.as_deref(),
                None,
            )
        );
    }

    let (nsec_str, was_generated) = ensure_keys_string()?;
    let keys = NostrKeys::parse(&nsec_str).context("Failed to parse nsec")?;
    let npub = NostrToBech32::to_bech32(&keys.public_key()).context("Failed to encode npub")?;

    let tree_name = path.file_name().map(|n| n.to_string_lossy().to_string());
    let ref_key = tree_name.as_ref().map(|name| format!("{}/{}", npub, name));

    let hash_bytes = from_hex(&hash_hex).context("Invalid hash")?;
    if let Err(e) = store.index_tree(
        &hash_bytes,
        &npub,
        tree_name.as_deref(),
        PRIORITY_OWN,
        ref_key.as_deref(),
    ) {
        tracing::warn!("Failed to index tree: {}", e);
    }
    if local {
        store.force_sync().context("Failed to sync local store")?;
    }

    let mut write_servers = Vec::new();
    if !local {
        let config = Config::load()?;
        write_servers = config.blossom.servers.clone();
        write_servers.extend(config.blossom.write_servers.clone());
        if !write_servers.is_empty() && publish.is_none() {
            let push_result =
                background_blossom_push(&data_dir, &cid_for_push, &write_servers).await;
            if let Err(e) = push_result {
                eprintln!("  file server push failed: {}", e);
            }
        }
    }

    if let Some(ref_name) = publish.as_deref() {
        let config = Config::load()?;
        let resolver_config = NostrResolverConfig {
            relays: config.nostr.relays.clone(),
            resolve_timeout: Duration::from_secs(5),
            secret_key: Some(keys),
        };

        let resolver = NostrRootResolver::new(resolver_config)
            .await
            .context("Failed to create Nostr resolver")?;

        let hash = from_hex(&hash_hex).context("Invalid hash")?;
        let key = key_hex
            .as_ref()
            .map(|k| key_from_hex(k))
            .transpose()
            .map_err(|e| anyhow::anyhow!("Invalid key: {}", e))?;
        let cid = Cid { hash, key };
        let nostr_key = format!("{}/{}", npub, ref_name);

        match RootResolver::publish(&resolver, &nostr_key, &cid).await {
            Ok(_) => {
                print!(
                    "{}",
                    render_add_output(
                        &path_display,
                        &display_route,
                        &display_root,
                        &hash_hex,
                        key_hex.as_deref(),
                        site_entry.as_deref(),
                        Some(PublishedAddSummary {
                            nostr_key: &nostr_key,
                            npub: &npub,
                            ref_name,
                            identity_was_generated: was_generated,
                        }),
                    )
                );
            }
            Err(e) => {
                print!(
                    "{}",
                    render_add_output(
                        &path_display,
                        &display_route,
                        &display_root,
                        &hash_hex,
                        key_hex.as_deref(),
                        site_entry.as_deref(),
                        None,
                    )
                );
                if was_generated {
                    println!("  identity: {} (new)", npub);
                }
                eprintln!("  publish failed: {}", e);
            }
        }

        let _ = RootResolver::stop(&resolver).await;

        if !local && !write_servers.is_empty() {
            if let Err(err) = background_blossom_push(&data_dir, &cid_for_push, &write_servers)
                .await
                .context("Failed to push content to file servers")
            {
                eprintln!("  file server push failed: {}", err);
            }
        }
    }

    Ok(())
}

struct EnvVarGuard {
    name: &'static str,
    previous: Option<OsString>,
}

impl EnvVarGuard {
    fn set(name: &'static str, value: impl Into<OsString>) -> Self {
        let previous = std::env::var_os(name);
        let value = value.into();
        std::env::set_var(name, value);
        Self { name, previous }
    }

    fn set_if_missing(name: &'static str, value: impl Into<OsString>) -> Option<Self> {
        if std::env::var_os(name).is_some() {
            None
        } else {
            Some(Self::set(name, value))
        }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        if let Some(value) = self.previous.as_ref() {
            std::env::set_var(self.name, value);
        } else {
            std::env::remove_var(self.name);
        }
    }
}

fn apply_local_add_env(data_dir: &Path) -> Vec<EnvVarGuard> {
    let mut guards = vec![
        EnvVarGuard::set(LMDB_NO_READ_AHEAD_ENV, "1"),
        EnvVarGuard::set(LMDB_NO_SYNC_ENV, "1"),
        EnvVarGuard::set(LMDB_NO_META_SYNC_ENV, "1"),
        EnvVarGuard::set(LMDB_EXTERNAL_BLOB_SYNC_ENV, "0"),
        EnvVarGuard::set(
            STREAM_PUT_BATCH_TARGET_BYTES_ENV,
            LOCAL_ADD_STREAM_BATCH_TARGET_BYTES,
        ),
    ];
    if let Some(guard) = EnvVarGuard::set_if_missing(
        LMDB_EXTERNAL_BLOB_MIN_BYTES_ENV,
        LOCAL_ADD_EXTERNAL_BLOB_MIN_BYTES,
    ) {
        guards.push(guard);
    }
    if let Some(guard) = EnvVarGuard::set_if_missing(
        LMDB_EXTERNAL_BLOB_DIR_ENV,
        data_dir
            .join(LOCAL_ADD_EXTERNAL_BLOB_DIR_NAME)
            .into_os_string(),
    ) {
        guards.push(guard);
    }
    if let Some(guard) = EnvVarGuard::set_if_missing(
        LMDB_EXTERNAL_BLOB_PACK_TARGET_BYTES_ENV,
        LOCAL_ADD_STREAM_BATCH_TARGET_BYTES,
    ) {
        guards.push(guard);
    }
    guards
}

fn add_progress_enabled() -> bool {
    std::io::stderr().is_terminal()
}

fn discover_add_input(path: &Path, respect_gitignore: bool, progress: &AddProgress) -> Result<()> {
    if path.is_dir() {
        let walker = hashtree_cli::ignore_rules::build_content_walker(path, respect_gitignore);
        for result in walker {
            let entry = result?;
            let entry_path = entry.path();
            if entry_path == path {
                continue;
            }
            if entry_path.is_file() {
                let metadata = std::fs::metadata(entry_path)
                    .with_context(|| format!("Failed to inspect file {}", entry_path.display()))?;
                progress.record_file_discovered(metadata.len());
            }
        }
    } else {
        let metadata = std::fs::metadata(path)
            .with_context(|| format!("Failed to inspect file {}", path.display()))?;
        progress.record_file_discovered(metadata.len());
    }
    progress.mark_totals_known();
    Ok(())
}

fn run_with_add_progress<T, F>(label: &'static str, progress: Arc<AddProgress>, f: F) -> Result<T>
where
    F: FnOnce() -> Result<T>,
{
    if !add_progress_enabled() {
        return f();
    }

    let done = Arc::new(AtomicBool::new(false));
    let outcome = Arc::new(AtomicU8::new(0));
    let progress_thread =
        spawn_add_progress_thread(label, progress, Arc::clone(&done), Arc::clone(&outcome));
    let result = f();
    outcome.store(if result.is_ok() { 1 } else { 2 }, Ordering::Relaxed);
    done.store(true, Ordering::Relaxed);
    let _ = progress_thread.join();
    result
}

fn spawn_add_progress_thread(
    label: &'static str,
    progress: Arc<AddProgress>,
    done: Arc<AtomicBool>,
    outcome: Arc<AtomicU8>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let start = Instant::now();
        let mut shown = false;
        let mut previous_line_len = 0usize;
        let mut last_report_second = None;

        loop {
            thread::sleep(Duration::from_millis(200));
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
                        format_add_progress_line(snapshot, elapsed)
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
            let line = format!("{label}... {}", format_add_progress_line(snapshot, elapsed));
            print_progress_line(&line, &mut previous_line_len, false);
        }
    })
}

fn format_add_progress_line(snapshot: AddProgressSnapshot, elapsed: Duration) -> String {
    let elapsed_text = format_duration_compact(elapsed);
    let rate = bytes_per_second(snapshot.bytes_processed, elapsed);
    let rate_text = if rate > 0 {
        format!("{}/s", format_bytes(rate))
    } else {
        "waiting for data".to_string()
    };

    if snapshot.totals_known && snapshot.bytes_total > 0 {
        let percent_total = snapshot.bytes_total.max(snapshot.bytes_processed);
        let percent = (snapshot.bytes_processed as f64 / percent_total as f64) * 100.0;
        let mut line = format!(
            "{}/{} ({percent:.1}%), {}, {}",
            format_bytes(snapshot.bytes_processed),
            format_bytes(snapshot.bytes_total),
            format_file_progress(snapshot),
            rate_text
        );
        if snapshot.bytes_processed < snapshot.bytes_total && rate > 0 {
            let remaining = snapshot.bytes_total - snapshot.bytes_processed;
            let eta = Duration::from_secs_f64(remaining as f64 / rate as f64);
            line.push_str(&format!(", eta {}", format_duration_compact(eta)));
        }
        line.push_str(&format!(" in {elapsed_text}"));
        return line;
    }

    if snapshot.totals_known {
        return format!(
            "{}, {} in {}",
            format_bytes(snapshot.bytes_processed),
            format_file_progress(snapshot),
            elapsed_text
        );
    }

    if snapshot.bytes_processed > 0 {
        return format!(
            "{} processed, {}, {} in {}",
            format_bytes(snapshot.bytes_processed),
            format_file_progress(snapshot),
            rate_text,
            elapsed_text
        );
    }

    if snapshot.bytes_total > 0 || snapshot.files_total > 0 {
        return format!(
            "scanning... discovered {} ({}) in {}",
            pluralize_files(snapshot.files_total),
            format_bytes(snapshot.bytes_total),
            elapsed_text
        );
    }

    format!("scanning... in {elapsed_text}")
}

fn format_file_progress(snapshot: AddProgressSnapshot) -> String {
    if snapshot.totals_known {
        return format!(
            "{}/{} files",
            snapshot.files_processed, snapshot.files_total
        );
    }
    pluralize_files(snapshot.files_processed)
}

fn pluralize_files(files: u64) -> String {
    if files == 1 {
        "1 file".to_string()
    } else {
        format!("{files} files")
    }
}

fn bytes_per_second(bytes: u64, elapsed: Duration) -> u64 {
    let elapsed = elapsed.as_secs_f64();
    if bytes == 0 || elapsed <= 0.0 {
        return 0;
    }
    (bytes as f64 / elapsed).max(1.0) as u64
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

fn encode_hash_route_segment(segment: &str) -> String {
    let mut encoded = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char);
            }
            _ => {
                encoded.push('%');
                encoded.push_str(&format!("{byte:02X}"));
            }
        }
    }
    encoded
}

pub(crate) fn build_drive_iris_to_url_for_add_route(route: &str) -> String {
    let segments = route
        .trim_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(encode_hash_route_segment)
        .collect::<Vec<_>>();

    if segments.is_empty() {
        IRIS_DRIVE_WEB_BASE_URL.to_string()
    } else {
        format!("{IRIS_DRIVE_WEB_BASE_URL}/#/{}", segments.join("/"))
    }
}

pub(crate) fn build_drive_iris_to_url_for_published_ref(
    owner_npub: &str,
    ref_name: &str,
) -> String {
    build_drive_iris_to_url_for_published_target(owner_npub, ref_name, None, None)
}

pub(crate) fn build_drive_iris_to_url_for_published_target(
    owner_npub: &str,
    ref_name: &str,
    path: Option<&str>,
    link_key: Option<&str>,
) -> String {
    let owner = encode_hash_route_segment(owner_npub.trim());
    let reference = encode_hash_route_segment(ref_name.trim_matches('/'));
    let mut url = format!("{IRIS_DRIVE_WEB_BASE_URL}/#/{owner}/{reference}");

    if let Some(path) = path {
        let encoded_path = path
            .trim_matches('/')
            .split('/')
            .filter(|segment| !segment.is_empty())
            .map(encode_hash_route_segment)
            .collect::<Vec<_>>()
            .join("/");
        if !encoded_path.is_empty() {
            url.push('/');
            url.push_str(&encoded_path);
        }
    }

    if let Some(link_key) = link_key {
        if !link_key.is_empty() {
            url.push_str("?k=");
            url.push_str(link_key);
        }
    }

    url
}

pub(crate) fn build_sites_iris_to_url_for_add_route(route: &str) -> String {
    let segments = route
        .trim_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(encode_hash_route_segment)
        .collect::<Vec<_>>();

    if segments.is_empty() {
        IRIS_SITES_WEB_BASE_URL.to_string()
    } else {
        format!("{IRIS_SITES_WEB_BASE_URL}/#/{}", segments.join("/"))
    }
}

pub(crate) fn build_sites_iris_to_url_for_published_ref(
    owner_npub: &str,
    ref_name: &str,
    entry_path: &str,
) -> String {
    let owner = encode_hash_route_segment(owner_npub.trim());
    let reference = encode_hash_route_segment(ref_name.trim_matches('/'));
    let entry = entry_path
        .trim_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(encode_hash_route_segment)
        .collect::<Vec<_>>()
        .join("/");
    format!("{IRIS_SITES_WEB_BASE_URL}/#/{owner}/{reference}/{entry}?reload=1")
}

pub(crate) fn detect_site_entry_for_path(path: &Path, is_dir: bool) -> Option<String> {
    if is_dir {
        let mut index_htm: Option<String> = None;
        let entries = std::fs::read_dir(path).ok()?;
        for entry in entries.flatten() {
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            if metadata.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            match name.to_ascii_lowercase().as_str() {
                "index.html" => return Some(name),
                "index.htm" if index_htm.is_none() => {
                    index_htm = Some(name);
                }
                _ => {}
            }
        }
        return index_htm;
    }

    let name = path.file_name()?.to_string_lossy().to_string();
    match name.to_ascii_lowercase().rsplit_once('.') {
        Some((_, "html" | "htm")) => Some(name),
        _ => None,
    }
}
