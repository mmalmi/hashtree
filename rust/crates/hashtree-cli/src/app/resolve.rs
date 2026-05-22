use anyhow::{Context, Result};
use hashtree_cli::config::parse_npub;
use hashtree_cli::storage::CachedRoot;
use hashtree_cli::{
    HashtreeStore, NostrKeys, NostrResolverConfig, NostrRootResolver, RootResolver,
};
use hashtree_core::Cid;
use std::path::PathBuf;

/// Resolved CID with optional path.
pub(crate) struct ResolvedCid {
    pub(crate) cid: hashtree_core::Cid,
    pub(crate) path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedPublishedTarget {
    pub(crate) npub: String,
    pub(crate) tree_name: String,
    pub(crate) path: Option<String>,
}

#[derive(Default, Clone)]
pub(crate) struct ResolveOptions {
    pub(crate) link_key: Option<[u8; 32]>,
    pub(crate) private: bool,
    pub(crate) relays: Option<Vec<String>>,
    pub(crate) secret_key: Option<NostrKeys>,
    pub(crate) data_dir: Option<PathBuf>,
}

fn decode_target_segment(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hi = bytes[index + 1] as char;
            let lo = bytes[index + 2] as char;
            if let (Some(hi), Some(lo)) = (hi.to_digit(16), lo.to_digit(16)) {
                decoded.push(((hi << 4) | lo) as u8);
                index += 3;
                continue;
            }
        }

        decoded.push(bytes[index]);
        index += 1;
    }

    String::from_utf8(decoded).unwrap_or_else(|_| segment.to_string())
}

pub(crate) fn parse_published_target(input: &str) -> Option<ParsedPublishedTarget> {
    let input = input.strip_prefix("htree://").unwrap_or(input);
    let input = input.split('#').next().unwrap_or(input);
    let input = input.split('?').next().unwrap_or(input).trim_matches('/');

    if !input.starts_with("npub1") {
        return None;
    }

    let mut parts = input.split('/');
    let npub = parts.next()?;
    let tree_name = decode_target_segment(parts.next()?);
    if tree_name.is_empty() {
        return None;
    }

    let path_parts = parts.map(decode_target_segment).collect::<Vec<_>>();
    Some(ParsedPublishedTarget {
        npub: npub.to_string(),
        tree_name,
        path: (!path_parts.is_empty()).then(|| path_parts.join("/")),
    })
}

/// Resolve a CID input which can be:
/// - An nhash (bech32-encoded hash with optional key)
/// - An npub/repo path (e.g., "npub1.../myrepo")
/// - An htree:// URL (e.g., "htree://npub1.../myrepo")
///
/// Returns the resolved Cid (raw bytes) and optional path within the tree.
pub(crate) async fn resolve_cid_input(input: &str) -> Result<ResolvedCid> {
    resolve_cid_input_with_opts(input, &ResolveOptions::default()).await
}

pub(crate) async fn resolve_cid_input_with_opts(
    input: &str,
    opts: &ResolveOptions,
) -> Result<ResolvedCid> {
    use hashtree_core::{is_nhash, nhash_decode, Cid};

    let normalized_input = input.strip_prefix("htree://").unwrap_or(input);

    // Check if it's an nhash (bech32-encoded) - gives us raw bytes directly
    // Support nhash1.../path/to/file format (path suffix after slash)
    if is_nhash(normalized_input) {
        let (nhash_part, url_path) = if let Some(slash_pos) = normalized_input.find('/') {
            (
                &normalized_input[..slash_pos],
                Some(&normalized_input[slash_pos + 1..]),
            )
        } else {
            (normalized_input, None)
        };

        let data = nhash_decode(nhash_part).map_err(|e| anyhow::anyhow!("Invalid nhash: {}", e))?;

        return Ok(ResolvedCid {
            cid: Cid {
                hash: data.hash,
                key: data.decrypt_key,
            },
            path: url_path.map(|p| p.to_string()),
        });
    }

    // Check for hex CID format: "hash" or "hash:key", optionally with /path
    let (cid_part, url_path) = if let Some(slash_pos) = normalized_input.find('/') {
        (
            &normalized_input[..slash_pos],
            Some(&normalized_input[slash_pos + 1..]),
        )
    } else {
        (normalized_input, None)
    };
    if let Ok(cid) = Cid::parse(cid_part) {
        return Ok(ResolvedCid {
            cid,
            path: url_path.map(|p| p.to_string()),
        });
    }

    // Check if it looks like an npub path (npub1.../name or npub1.../name/path).
    if let Some(parsed_target) = parse_published_target(normalized_input) {
        let key = format!("{}/{}", parsed_target.npub, parsed_target.tree_name);
        eprintln!("Resolving {}...", key);

        let mut config = NostrResolverConfig::default();
        if let Some(relays) = &opts.relays {
            config.relays = relays.clone();
        }
        if opts.private {
            config.secret_key = opts.secret_key.clone();
        }

        let resolver = NostrRootResolver::new(config)
            .await
            .context("Failed to create nostr resolver")?;

        let resolved = if let Some(link_key) = opts.link_key {
            resolver.resolve_shared(&key, &link_key).await
        } else {
            resolver.resolve(&key).await
        };

        match resolved {
            Ok(Some(cid)) => {
                eprintln!("Resolved to: {}", hashtree_core::to_hex(&cid.hash));
                return Ok(ResolvedCid {
                    cid,
                    path: parsed_target.path,
                });
            }
            Ok(None) => {
                if let Some(cached) = resolve_cached_published_target(&parsed_target, opts)? {
                    eprintln!(
                        "Using cached root for {}: {}",
                        key,
                        hashtree_core::to_hex(&cached.cid.hash)
                    );
                    return Ok(cached);
                }
                anyhow::bail!("No content found for {}", key);
            }
            Err(e) => {
                if let Some(cached) = resolve_cached_published_target(&parsed_target, opts)? {
                    eprintln!(
                        "Using cached root for {} after resolver error: {}",
                        key,
                        hashtree_core::to_hex(&cached.cid.hash)
                    );
                    return Ok(cached);
                }
                anyhow::bail!("Failed to resolve {}: {}", key, e);
            }
        }
    }

    anyhow::bail!("Invalid format. Use nhash1..., <hash>, <hash:key>, or npub1.../name")
}

fn resolve_cached_published_target(
    parsed_target: &ParsedPublishedTarget,
    opts: &ResolveOptions,
) -> Result<Option<ResolvedCid>> {
    let Some(data_dir) = opts.data_dir.as_deref() else {
        return Ok(None);
    };

    let pubkey_hex = hex::encode(parse_npub(&parsed_target.npub)?);
    let store = HashtreeStore::new(data_dir)?;
    resolve_cached_published_target_from_cache(
        parsed_target,
        store.get_cached_root(&pubkey_hex, &parsed_target.tree_name)?,
    )
}

fn resolve_cached_published_target_from_cache(
    parsed_target: &ParsedPublishedTarget,
    cached: Option<CachedRoot>,
) -> Result<Option<ResolvedCid>> {
    let Some(cached) = cached else {
        return Ok(None);
    };
    let cid = Cid {
        hash: hashtree_core::from_hex(&cached.hash)
            .map_err(|e| anyhow::anyhow!("Invalid cached root hash: {}", e))?,
        key: match cached.key.as_deref() {
            Some(key) => Some(
                hashtree_core::from_hex(key)
                    .map_err(|e| anyhow::anyhow!("Invalid cached root key: {}", e))?,
            ),
            None => None,
        },
    };

    Ok(Some(ResolvedCid {
        cid,
        path: parsed_target.path.clone(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashtree_cli::NostrToBech32;
    use nostr_sdk::Keys;

    #[test]
    fn resolve_cached_published_target_returns_cached_cid() {
        let keys = Keys::generate();
        let npub = NostrToBech32::to_bech32(&keys.public_key()).unwrap();
        let hash = "11".repeat(32);
        let key = "22".repeat(32);

        let parsed_target = ParsedPublishedTarget {
            npub,
            tree_name: "mount-test".to_string(),
            path: Some("nested/file.txt".to_string()),
        };

        let resolved = resolve_cached_published_target_from_cache(
            &parsed_target,
            Some(CachedRoot {
                hash: hash.clone(),
                key: Some(key.clone()),
                updated_at: 123,
                visibility: "public".to_string(),
            }),
        )
        .unwrap()
        .expect("cached root");

        assert_eq!(hashtree_core::to_hex(&resolved.cid.hash), hash);
        assert_eq!(resolved.cid.key.map(hex::encode), Some(key));
        assert_eq!(resolved.path.as_deref(), Some("nested/file.txt"));
    }
}
