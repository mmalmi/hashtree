use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use hashtree_core::{Cid, DirEntry, HashTree, HashTreeConfig, LinkType, MemoryStore};
use hashtree_resolver::nostr::{NostrResolverConfig, NostrRootResolver};
use hashtree_resolver::{Keys, RootResolver, ToBech32};
use hashtree_sim::WsRelay;
use hashtree_updater::{
    install_file, HashtreeUpdater, UpdateCheckOptions, UpdateManifest, UpdateRef, UpdateTarget,
};

async fn file_entry(tree: &HashTree<MemoryStore>, name: &str, bytes: &[u8]) -> DirEntry {
    let (cid, size) = tree.put_file(bytes).await.expect("put file");
    DirEntry::from_cid(name, &cid)
        .with_size(size)
        .with_link_type(LinkType::File)
}

async fn make_release_tree(
    tree: &HashTree<MemoryStore>,
    manifest: &str,
    asset_path: &str,
    asset_bytes: &[u8],
) -> Cid {
    let (manifest_cid, manifest_size) = tree
        .put_file(manifest.as_bytes())
        .await
        .expect("put manifest");
    let manifest_entry = DirEntry::from_cid("manifest.json", &manifest_cid)
        .with_size(manifest_size)
        .with_link_type(LinkType::File);

    let path = Path::new(asset_path);
    let asset_name = path.file_name().unwrap().to_string_lossy().to_string();
    let asset_entry = file_entry(tree, &asset_name, asset_bytes).await;
    let assets_dir = tree
        .put_directory(vec![asset_entry])
        .await
        .expect("put assets dir");
    let assets_entry = DirEntry::from_cid("assets", &assets_dir).with_link_type(LinkType::Dir);

    tree.put_directory(vec![manifest_entry, assets_entry])
        .await
        .expect("put release dir")
}

async fn make_releases_root(tree: &HashTree<MemoryStore>, release_cid: &Cid, version: &str) -> Cid {
    let version_entry = DirEntry::from_cid(version, release_cid).with_link_type(LinkType::Dir);
    let latest_entry = DirEntry::from_cid("latest", release_cid).with_link_type(LinkType::Dir);
    tree.put_directory(vec![version_entry, latest_entry])
        .await
        .expect("put releases root")
}

#[test]
fn parses_encoded_release_ref_with_tree_path_split() {
    let parsed = UpdateRef::parse("htree://npub1owner/releases%2Fnostr-vpn/stable/latest")
        .expect("parse ref");

    assert_eq!(parsed.npub, "npub1owner");
    assert_eq!(parsed.tree_name, "releases/nostr-vpn");
    assert_eq!(parsed.path.as_deref(), Some("stable/latest"));
    assert_eq!(parsed.resolver_key(), "npub1owner/releases/nostr-vpn");
}

#[test]
fn manifest_selects_current_target_alias_and_rejects_unsafe_paths() {
    let manifest: UpdateManifest = serde_json::from_str(
        r#"{
          "schema": "hashtree.update.v1",
          "app": "nostr-vpn",
          "version": "v1.2.3",
          "channel": "stable",
          "assets": [
            {
              "name": "nostr-vpn-macos-arm64.tar.gz",
              "path": "assets/nostr-vpn-macos-arm64.tar.gz",
              "targets": ["aarch64-apple-darwin"]
            }
          ]
        }"#,
    )
    .expect("manifest");

    manifest.validate().expect("valid manifest");
    let target = UpdateTarget::new("darwin-aarch64");
    assert_eq!(
        manifest.select_asset(&target).expect("selected asset").name,
        "nostr-vpn-macos-arm64.tar.gz"
    );

    let unsafe_manifest: UpdateManifest = serde_json::from_str(
        r#"{
          "app": "bad",
          "version": "1.0.0",
          "assets": [{ "name": "bad", "path": "../bad", "target": "x86_64-unknown-linux-gnu" }]
        }"#,
    )
    .expect("unsafe manifest");
    assert!(unsafe_manifest.validate().is_err());
}

#[tokio::test]
async fn check_resolves_release_manifest_and_downloads_selected_asset() {
    let store = Arc::new(MemoryStore::new());
    let tree = HashTree::new(HashTreeConfig::new(store).public());
    let manifest = r#"{
      "schema": "hashtree.update.v1",
      "app": "squirreldisk",
      "version": "1.2.0",
      "channel": "stable",
      "assets": [
        {
          "name": "squirreldisk-linux-x64",
          "path": "assets/squirreldisk-linux-x64",
          "target": "x86_64-unknown-linux-gnu",
          "size": 11
        }
      ]
    }"#;
    let release_cid = make_release_tree(
        &tree,
        manifest,
        "assets/squirreldisk-linux-x64",
        b"new-binary\n",
    )
    .await;
    let releases_root = make_releases_root(&tree, &release_cid, "v1.2.0").await;
    let resolver = SingleRootResolver::new("npub1publisher/releases/squirreldisk", releases_root);
    let updater = HashtreeUpdater::new(resolver, tree);

    let check = updater
        .check(UpdateCheckOptions {
            reference: UpdateRef::parse("htree://npub1publisher/releases%2Fsquirreldisk/latest")
                .expect("ref"),
            current_version: "1.1.0".to_string(),
            target: UpdateTarget::new("linux-x86_64"),
            ..UpdateCheckOptions::default()
        })
        .await
        .expect("check");

    assert!(check.update_available);
    assert_eq!(check.manifest.app, "squirreldisk");
    assert_eq!(
        check.asset.as_ref().expect("asset").name,
        "squirreldisk-linux-x64"
    );

    let downloaded = updater
        .download_asset(&check, None)
        .await
        .expect("download");
    assert_eq!(downloaded.bytes, b"new-binary\n");
}

#[test]
fn install_file_replaces_destination_atomically_and_marks_executable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let destination = dir.path().join("app-bin");
    std::fs::write(&destination, b"old").expect("write old");

    install_file(&destination, b"new", true).expect("install");

    assert_eq!(std::fs::read(&destination).expect("read installed"), b"new");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&destination)
            .expect("metadata")
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0, "installed file should be executable");
    }
}

#[tokio::test]
async fn e2e_resolves_latest_update_through_nostr_root_event() {
    let mut relay = WsRelay::new();
    relay.start().await.expect("start relay");
    let relay_url = relay.url().expect("relay url");

    let store = Arc::new(MemoryStore::new());
    let tree = HashTree::new(HashTreeConfig::new(store).public());
    let manifest = r#"{
      "schema": "hashtree.update.v1",
      "app": "nostr-vpn",
      "version": "2.0.0",
      "assets": [
        {
          "name": "nostr-vpn-aarch64-apple-darwin.tar.gz",
          "path": "assets/nostr-vpn-aarch64-apple-darwin.tar.gz",
          "targets": ["aarch64-apple-darwin"]
        }
      ]
    }"#;
    let release_cid = make_release_tree(
        &tree,
        manifest,
        "assets/nostr-vpn-aarch64-apple-darwin.tar.gz",
        b"archive bytes",
    )
    .await;
    let stable_root = make_releases_root(&tree, &release_cid, "v2.0.0").await;
    let stable_entry = DirEntry::from_cid("stable", &stable_root).with_link_type(LinkType::Dir);
    let releases_root = tree
        .put_directory(vec![stable_entry])
        .await
        .expect("put channel root");

    let keys = Keys::generate();
    let npub = keys.public_key().to_bech32().expect("npub");
    let publish_resolver = NostrRootResolver::new(NostrResolverConfig {
        relays: vec![relay_url.clone()],
        resolve_timeout: Duration::from_secs(2),
        secret_key: Some(keys),
    })
    .await
    .expect("publisher resolver");
    publish_resolver
        .publish(&format!("{npub}/releases/nostr-vpn"), &releases_root)
        .await
        .expect("publish");

    let read_resolver = NostrRootResolver::new(NostrResolverConfig {
        relays: vec![relay_url],
        resolve_timeout: Duration::from_secs(2),
        secret_key: None,
    })
    .await
    .expect("read resolver");
    let updater = HashtreeUpdater::new(read_resolver, tree);

    let check = updater
        .check(UpdateCheckOptions {
            reference: UpdateRef::parse(&format!(
                "htree://{npub}/releases%2Fnostr-vpn/stable/latest"
            ))
            .expect("ref"),
            current_version: "1.9.0".to_string(),
            target: UpdateTarget::new("darwin-aarch64"),
            ..UpdateCheckOptions::default()
        })
        .await
        .expect("check");

    assert!(check.update_available);
    assert_eq!(check.manifest.app, "nostr-vpn");
    assert_eq!(
        updater
            .download_asset(&check, None)
            .await
            .expect("download")
            .bytes,
        b"archive bytes"
    );

    relay.stop().await;
}

struct SingleRootResolver {
    key: String,
    cid: Cid,
}

impl SingleRootResolver {
    fn new(key: impl Into<String>, cid: Cid) -> Self {
        Self {
            key: key.into(),
            cid,
        }
    }
}

#[async_trait::async_trait]
impl RootResolver for SingleRootResolver {
    async fn resolve(&self, key: &str) -> Result<Option<Cid>, hashtree_resolver::ResolverError> {
        Ok((key == self.key).then(|| self.cid.clone()))
    }

    async fn subscribe(
        &self,
        _key: &str,
    ) -> Result<tokio::sync::mpsc::Receiver<Option<Cid>>, hashtree_resolver::ResolverError> {
        Err(hashtree_resolver::ResolverError::Other(
            "test resolver does not subscribe".to_string(),
        ))
    }
}
