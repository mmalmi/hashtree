use anyhow::{Context, Result};
use hashtree_cli::config::ensure_keys_string;
use hashtree_cli::{
    Config, HashtreeStore, NostrKeys, NostrResolverConfig, NostrRootResolver, NostrToBech32,
    RootResolver, PRIORITY_OWN,
};
use hashtree_core::{
    from_hex, key_from_hex, nhash_encode, nhash_encode_full, Cid, HashTree, HashTreeConfig,
    NHashData,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use super::blossom::background_blossom_push;
use super::content::add_directory;

const IRIS_FILES_WEB_BASE_URL: &str = "https://files.iris.to";
const IRIS_SITES_WEB_BASE_URL: &str = "https://sites.iris.to";

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
    let is_dir = path.is_dir();

    if only_hash {
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

        if is_dir {
            let cid = add_directory(&tree, &path, !no_ignore).await?;
            println!("hash: {}", hashtree_core::to_hex(&cid.hash));
            if let Some(key) = cid.key {
                println!("key:  {}", hashtree_core::to_hex(&key));
            }
        } else {
            let file = std::fs::File::open(&path)
                .with_context(|| format!("Failed to open file for hashing: {}", path.display()))?;
            let (cid, _size) = tree
                .put_stream(AllowStdIo::new(file))
                .await
                .map_err(|e| anyhow::anyhow!("Failed to hash file: {}", e))?;
            println!("hash: {}", hashtree_core::to_hex(&cid.hash));
            if let Some(key) = cid.key {
                println!("key:  {}", hashtree_core::to_hex(&key));
            }
        }

        return Ok(());
    }

    let store = HashtreeStore::new(&data_dir)?;
    let site_entry = detect_site_entry_for_path(&path, is_dir);

    let (cid_for_push, hash_hex, key_hex, display_root): (String, String, Option<String>, String) =
        if unencrypted {
            let hash_hex = if is_dir {
                store
                    .upload_dir_with_options_and_chunk_size(&path, !no_ignore, chunk_size)
                    .context("Failed to add directory")?
            } else {
                store
                    .upload_file_with_chunk_size(&path, chunk_size)
                    .context("Failed to add file")?
            };
            let hash = from_hex(&hash_hex).context("Invalid hash")?;
            let nhash = nhash_encode(&hash)
                .map_err(|e| anyhow::anyhow!("Failed to encode nhash: {}", e))?;
            (hash_hex.clone(), hash_hex, None, nhash)
        } else {
            let cid_str = if is_dir {
                store
                    .upload_dir_encrypted_with_options_and_chunk_size(&path, !no_ignore, chunk_size)
                    .context("Failed to add directory")?
            } else {
                store
                    .upload_file_encrypted_with_chunk_size(&path, chunk_size)
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
            (cid_str, hash_hex, key_hex, nhash)
        };

    println!("added {}", path.display());
    let display_route = if is_dir {
        display_root.clone()
    } else {
        let filename = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        format!("{display_root}/{filename}")
    };
    println!("  url:   {}", display_route);
    println!(
        "  files: {}",
        build_files_iris_to_url_for_add_route(&display_route)
    );
    if let Some(entry_path) = site_entry.as_deref() {
        let site_route = format!("{display_root}/{entry_path}");
        println!(
            "  site:  {}",
            build_sites_iris_to_url_for_add_route(&site_route)
        );
    }
    println!("  hash:  {}", hash_hex);
    if let Some(ref k) = key_hex {
        println!("  key:   {}", k);
    }

    let (nsec_str, _) = ensure_keys_string()?;
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
        let (nsec_str, was_generated) = ensure_keys_string()?;
        let keys = NostrKeys::parse(&nsec_str).context("Failed to parse nsec")?;
        let npub = NostrToBech32::to_bech32(&keys.public_key()).context("Failed to encode npub")?;

        if was_generated {
            println!("  identity: {} (new)", npub);
        }

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
                println!("  published: {}", nostr_key);
                println!(
                    "  files: {}",
                    build_files_iris_to_url_for_published_ref(&npub, ref_name)
                );
                if let Some(entry_path) = site_entry.as_deref() {
                    println!(
                        "  site:  {}",
                        build_sites_iris_to_url_for_published_ref(&npub, ref_name, entry_path)
                    );
                    let immutable_site_route = format!("{display_root}/{entry_path}");
                    println!(
                        "  permalink: {}",
                        build_sites_iris_to_url_for_add_route(&immutable_site_route)
                    );
                }
            }
            Err(e) => {
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

pub(crate) fn build_files_iris_to_url_for_add_route(route: &str) -> String {
    let segments = route
        .trim_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(encode_hash_route_segment)
        .collect::<Vec<_>>();

    if segments.is_empty() {
        IRIS_FILES_WEB_BASE_URL.to_string()
    } else {
        format!("{IRIS_FILES_WEB_BASE_URL}/#/{}", segments.join("/"))
    }
}

pub(crate) fn build_files_iris_to_url_for_published_ref(
    owner_npub: &str,
    ref_name: &str,
) -> String {
    build_files_iris_to_url_for_published_target(owner_npub, ref_name, None, None)
}

pub(crate) fn build_files_iris_to_url_for_published_target(
    owner_npub: &str,
    ref_name: &str,
    path: Option<&str>,
    link_key: Option<&str>,
) -> String {
    let owner = encode_hash_route_segment(owner_npub.trim());
    let reference = encode_hash_route_segment(ref_name.trim_matches('/'));
    let mut url = format!("{IRIS_FILES_WEB_BASE_URL}/#/{owner}/{reference}");

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
                "index.htm" => {
                    if index_htm.is_none() {
                        index_htm = Some(name);
                    }
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
