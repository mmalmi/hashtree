#![cfg_attr(not(feature = "fuse"), allow(dead_code))]

use anyhow::{Context, Result};
use git_remote_htree::nostr_client::resolve_identity;
use hashtree_core::{is_nhash, Cid};
use nostr_sdk::ToBech32;
use std::path::{Path, PathBuf};

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

fn pubkey_hex_to_npub(pubkey_hex: &str) -> Result<String> {
    let pubkey_bytes = hex::decode(pubkey_hex).context("Identity pubkey is not valid hex")?;
    let pubkey = nostr_sdk::PublicKey::from_slice(&pubkey_bytes)
        .context("Identity pubkey is not a valid Nostr pubkey")?;
    pubkey.to_bech32().context("Failed to encode npub")
}

pub(crate) fn normalize_mount_target_for_resolution(target: &str) -> Result<String> {
    normalize_mount_target_for_resolution_with(target, resolve_identity)
}

fn normalize_mount_target_for_resolution_with<F>(target: &str, resolver: F) -> Result<String>
where
    F: Fn(&str) -> Result<(String, Option<String>)>,
{
    let normalized = target.strip_prefix("htree://").unwrap_or(target);
    let path_only = normalized.split('#').next().unwrap_or(normalized);
    let path_only = path_only
        .split('?')
        .next()
        .unwrap_or(path_only)
        .trim_matches('/');

    let mut parts = path_only.split('/');
    let Some(identifier) = parts.next() else {
        return Ok(normalized.to_string());
    };
    let Some(tree_name_segment) = parts.next() else {
        return Ok(normalized.to_string());
    };

    if is_nhash(identifier) || Cid::parse(identifier).is_ok() {
        return Ok(normalized.to_string());
    }

    let owner_npub = if identifier.starts_with("npub1") {
        identifier.to_string()
    } else if identifier.len() == 64 && hex::decode(identifier).is_ok() {
        pubkey_hex_to_npub(identifier)?
    } else {
        let (pubkey_hex, _) = resolver(identifier)?;
        pubkey_hex_to_npub(&pubkey_hex)?
    };

    let remainder = parts.collect::<Vec<_>>();
    let mut out = format!("{owner_npub}/{tree_name_segment}");
    if !remainder.is_empty() {
        out.push('/');
        out.push_str(&remainder.join("/"));
    }
    Ok(out)
}

pub(crate) fn derive_default_mountpoint_name(target: &str) -> Result<String> {
    let normalized = target.strip_prefix("htree://").unwrap_or(target);
    let path_only = normalized.split('#').next().unwrap_or(normalized);
    let path_only = path_only
        .split('?')
        .next()
        .unwrap_or(path_only)
        .trim_matches('/');
    let segment = path_only
        .rsplit('/')
        .find(|segment| !segment.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Cannot derive mountpoint from empty target"))?;
    let decoded = decode_target_segment(segment);
    let leaf = decoded
        .rsplit('/')
        .find(|segment| !segment.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Cannot derive mountpoint from target: {}", target))?;
    Ok(leaf.to_string())
}

pub(crate) fn derive_implicit_mountpoint(base_dir: &Path, target: &str) -> Result<PathBuf> {
    let mountpoint_name = derive_default_mountpoint_name(target)?;
    let mountpoint = base_dir.join(mountpoint_name);
    if mountpoint.exists() {
        anyhow::bail!(
            "Implicit mountpoint already exists: {}. Pass an explicit mountpoint or remove it first.",
            mountpoint.display()
        );
    }
    Ok(mountpoint)
}

pub(crate) fn create_mountpoint_dir(mountpoint: &Path) -> Result<()> {
    std::fs::create_dir(mountpoint)
        .with_context(|| format!("Failed to create mountpoint {}", mountpoint.display()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExplicitMountpointDisposition {
    CreateDir,
    UseExistingEmptyDir,
}

pub(crate) fn prepare_explicit_mountpoint(
    mountpoint: &Path,
) -> Result<ExplicitMountpointDisposition> {
    if !mountpoint.exists() {
        return Ok(ExplicitMountpointDisposition::CreateDir);
    }

    if !mountpoint.is_dir() {
        anyhow::bail!("Mountpoint is not a directory: {}", mountpoint.display());
    }

    let mut entries = std::fs::read_dir(mountpoint)
        .with_context(|| format!("Failed to inspect mountpoint {}", mountpoint.display()))?;
    if let Some(entry) = entries.next() {
        entry.with_context(|| format!("Failed to inspect mountpoint {}", mountpoint.display()))?;
        anyhow::bail!(
            "Explicit mountpoint must be an empty directory: {}",
            mountpoint.display()
        );
    }

    Ok(ExplicitMountpointDisposition::UseExistingEmptyDir)
}

pub(crate) fn reject_local_mount_target(target: &str, current_dir: &Path) -> Result<()> {
    let candidate = PathBuf::from(target);
    let looks_like_path = candidate.is_absolute()
        || target.starts_with("./")
        || target.starts_with("../")
        || target.starts_with("~/")
        || (!target.contains('/') && !target.contains('\\') && current_dir.join(target).exists());

    if looks_like_path {
        anyhow::bail!(
            "mount target must be a hashtree ref, not a local path: {}. Use `htree mount self/<name> <mountpoint>`",
            target
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr_sdk::Keys;
    use std::fs;

    #[test]
    fn normalize_mount_target_resolves_alias_owner() {
        let keys = Keys::generate();
        let expected_npub = keys.public_key().to_bech32().expect("npub");
        let pubkey_hex = keys.public_key().to_hex();

        let normalized =
            normalize_mount_target_for_resolution_with("self/mydir/docs", |_identifier| {
                Ok((pubkey_hex.clone(), None))
            })
            .expect("normalize target");

        assert_eq!(normalized, format!("{expected_npub}/mydir/docs"));
    }

    #[test]
    fn derive_default_mountpoint_name_uses_leaf_of_decoded_segment() {
        assert_eq!(
            derive_default_mountpoint_name("htree://npub1owner/releases%2Fapp").unwrap(),
            "app"
        );
        assert_eq!(
            derive_default_mountpoint_name("htree://npub1owner/releases%2Fapp/docs/subdir")
                .unwrap(),
            "subdir"
        );
    }

    #[test]
    fn derive_implicit_mountpoint_uses_leaf_and_rejects_existing_path() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mountpoint = derive_implicit_mountpoint(temp_dir.path(), "htree://self/mydir")
            .expect("derive mountpoint");
        assert!(mountpoint.ends_with("mydir"));
        assert!(!mountpoint.exists());

        create_mountpoint_dir(&mountpoint).expect("create mountpoint");
        assert!(mountpoint.is_dir());

        let error = derive_implicit_mountpoint(temp_dir.path(), "htree://self/mydir")
            .expect_err("reject existing path");
        assert!(error
            .to_string()
            .contains("Implicit mountpoint already exists"));

        fs::remove_dir(&mountpoint).unwrap();
    }

    #[test]
    fn reject_local_mount_target_rejects_filesystem_paths() {
        let temp_dir = tempfile::tempdir().unwrap();
        let existing = temp_dir.path().join("drive");
        fs::create_dir(&existing).unwrap();

        let absolute_error =
            reject_local_mount_target(existing.to_str().unwrap(), temp_dir.path()).unwrap_err();
        assert!(absolute_error
            .to_string()
            .contains("mount target must be a hashtree ref"));

        let relative_error = reject_local_mount_target("./drive", temp_dir.path()).unwrap_err();
        assert!(relative_error
            .to_string()
            .contains("mount target must be a hashtree ref"));

        let single_segment_error = reject_local_mount_target("drive", temp_dir.path()).unwrap_err();
        assert!(single_segment_error
            .to_string()
            .contains("mount target must be a hashtree ref"));
    }

    #[test]
    fn reject_local_mount_target_allows_published_refs() {
        let temp_dir = tempfile::tempdir().unwrap();

        reject_local_mount_target("self/drive", temp_dir.path()).unwrap();
        reject_local_mount_target("npub1owner/drive", temp_dir.path()).unwrap();
        reject_local_mount_target("htree://npub1owner/drive/docs", temp_dir.path()).unwrap();
        reject_local_mount_target("nhash1example", temp_dir.path()).unwrap();
    }

    #[test]
    fn prepare_explicit_mountpoint_allows_missing_or_empty_dirs() {
        let temp_dir = tempfile::tempdir().unwrap();
        let missing = temp_dir.path().join("missing");
        assert_eq!(
            prepare_explicit_mountpoint(&missing).unwrap(),
            ExplicitMountpointDisposition::CreateDir
        );

        fs::create_dir(&missing).unwrap();
        assert_eq!(
            prepare_explicit_mountpoint(&missing).unwrap(),
            ExplicitMountpointDisposition::UseExistingEmptyDir
        );
    }

    #[test]
    fn prepare_explicit_mountpoint_rejects_nonempty_dir() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mountpoint = temp_dir.path().join("mountpoint");
        fs::create_dir(&mountpoint).unwrap();
        fs::write(mountpoint.join("existing.txt"), b"hello").unwrap();

        let error = prepare_explicit_mountpoint(&mountpoint).unwrap_err();
        assert!(error
            .to_string()
            .contains("Explicit mountpoint must be an empty directory"));
    }

    #[test]
    fn prepare_explicit_mountpoint_rejects_existing_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let mountpoint = temp_dir.path().join("mountpoint");
        fs::write(&mountpoint, b"hello").unwrap();

        let error = prepare_explicit_mountpoint(&mountpoint).unwrap_err();
        assert!(error.to_string().contains("Mountpoint is not a directory"));
    }
}
