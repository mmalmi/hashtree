use super::tests::spawn_test_server_with_auth;
use crate::storage::HashtreeStore;
use anyhow::Result;
use hashtree_config::StorageBackend;
use hashtree_core::{
    nhash_encode, nhash_encode_full, sha256, to_hex, DirEntry, HashTree, HashTreeConfig, LinkType,
    NHashData,
};
use hashtree_lmdb::{
    ExternalBlobOptions, LmdbBlobStore, PoolMemberConfig, PoolStore, PoolStoreConfig,
    SHARED_BLOB_POOL_DIR_NAME,
};
use serde_json::json;
use std::sync::Arc;
use tempfile::TempDir;

#[derive(Clone, Copy, Debug)]
enum Damage {
    MissingFile,
    CorruptFile,
    MissingMemberEntry,
}

async fn reject_stale_payload(link_type: Option<LinkType>) -> Result<()> {
    for damage in [
        Damage::MissingFile,
        Damage::CorruptFile,
        Damage::MissingMemberEntry,
    ] {
        let temp = TempDir::new()?;
        let data_dir = temp.path().join("db");
        let member_dir = temp.path().join("member");
        let external_dir = temp.path().join("payloads");
        let map_size = 32 * 1024 * 1024;
        {
            let pool = PoolStore::open(
                data_dir.join(SHARED_BLOB_POOL_DIR_NAME),
                PoolStoreConfig::default(),
            )?;
            pool.add_member(
                PoolMemberConfig::new(member_dir.clone(), map_size).with_external_blobs(
                    external_dir.clone(),
                    1,
                    true,
                    None,
                ),
            )?;
        }
        let store = Arc::new(HashtreeStore::with_options_and_backend(
            &data_dir,
            None,
            map_size,
            false,
            &StorageBackend::Lmdb,
        )?);
        let member = LmdbBlobStore::with_exact_map_size_and_external_blob_options(
            &member_dir,
            map_size as usize,
            Some(ExternalBlobOptions {
                base_path: external_dir.clone(),
                min_bytes: 1,
                sync: true,
                pack_target_bytes: None,
            }),
        )?;
        let tree = HashTree::new(HashTreeConfig::new(store.store_arc()));
        let (leaf, size) = tree
            .put(b"encrypted payload that must remain readable")
            .await?;
        let root = match link_type {
            Some(kind) => {
                tree.put_directory(vec![DirEntry::from_cid("leaf", &leaf)
                    .with_size(size)
                    .with_link_type(kind)])
                    .await?
            }
            None => leaf.clone(),
        };
        let raw = member
            .get_sync(&leaf.hash)?
            .expect("stored fixture payload");
        let hash = to_hex(&leaf.hash);
        let payload_path = external_dir
            .join(&hash[..2])
            .join(&hash[2..4])
            .join(&hash[4..]);
        assert_eq!(std::fs::read(&payload_path)?, raw);
        match damage {
            Damage::MissingFile => std::fs::remove_file(&payload_path)?,
            Damage::CorruptFile => {
                let mut corrupt = raw.clone();
                corrupt[0] ^= 1;
                std::fs::write(&payload_path, corrupt)?;
            }
            Damage::MissingMemberEntry => assert!(member.delete_sync(&leaf.hash)?),
        }
        // Deliberately retain the Pool catalog entry: metadata alone is not proof
        // that a stored payload still exists and matches its content address.
        assert_eq!(
            store.router().blob_size_sync(&leaf.hash)?,
            Some(raw.len() as u64)
        );
        let nhash = nhash_encode_full(&NHashData {
            hash: root.hash,
            decrypt_key: root.key,
        })?;
        let (port, handle) = spawn_test_server_with_auth(Arc::clone(&store)).await?;
        let client = reqwest::Client::new();
        let request = || {
            client
                .post(format!("http://127.0.0.1:{port}/api/pin-tree"))
                .basic_auth("test-user", Some("test-password"))
                .json(&json!({ "nhash": nhash }))
        };
        let response = request().send().await?;
        assert_eq!(
            response.status(),
            match damage {
                Damage::MissingMemberEntry if link_type.is_none() => reqwest::StatusCode::NOT_FOUND,
                Damage::MissingMemberEntry => reqwest::StatusCode::UNPROCESSABLE_ENTITY,
                _ => reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            },
            "unexpected response for {link_type:?} with {damage:?}"
        );
        assert!(!store.is_pinned(&root.hash)?);
        assert!(store.get_tree_meta(&root.hash)?.is_none());
        assert!(store.list_indexed_trees()?.is_empty());

        match damage {
            Damage::MissingMemberEntry => {
                assert!(member.put_sync(leaf.hash, &raw)?);
            }
            _ => std::fs::write(payload_path, &raw)?,
        }
        let response = request().send().await?;
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let result: serde_json::Value = response.json().await?;
        assert_eq!(
            result["indexed_hashes"],
            if link_type.is_some() { 2 } else { 1 }
        );
        assert!(store.is_pinned(&root.hash)?);
        handle.abort();
    }
    Ok(())
}

#[tokio::test]
async fn pin_tree_rejects_stale_blob_payloads() -> Result<()> {
    reject_stale_payload(Some(LinkType::Blob)).await
}

#[tokio::test]
async fn pin_tree_rejects_stale_file_payloads() -> Result<()> {
    reject_stale_payload(Some(LinkType::File)).await
}

#[tokio::test]
async fn pin_tree_rejects_stale_root_payloads() -> Result<()> {
    reject_stale_payload(None).await
}

#[tokio::test]
async fn pin_tree_verifies_payload_hash_without_pool_validation() -> Result<()> {
    let temp = TempDir::new()?;
    let store = Arc::new(HashtreeStore::with_options_and_backend(
        temp.path(),
        None,
        1024 * 1024,
        false,
        &StorageBackend::Fs,
    )?);
    let original = b"correct bytes";
    let hash = sha256(original);
    // The filesystem adapter returns bytes without hash verification. Keep a
    // same-length corrupt payload under the expected address to test the pin
    // boundary itself, independently of PoolStore's verified-read protection.
    store.router().put_sync(hash, b"damaged bytes")?;
    let (port, handle) = spawn_test_server_with_auth(Arc::clone(&store)).await?;
    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/api/pin-tree"))
        .basic_auth("test-user", Some("test-password"))
        .json(&json!({ "nhash": nhash_encode(&hash)? }))
        .send()
        .await?;
    assert_eq!(response.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    assert!(!store.is_pinned(&hash)?);
    assert!(store.get_tree_meta(&hash)?.is_none());
    handle.abort();
    Ok(())
}
