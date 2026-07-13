use anyhow::{Context, Result};
use async_trait::async_trait;
use hashtree_cli::{Config, HashtreeStore, NostrResolverConfig, NostrRootResolver, RootResolver};
use hashtree_fuse::{FsError as FuseFsError, HashtreeFuse, RootPublisher};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::add::build_drive_iris_to_url_for_published_target;
use super::blossom::background_blossom_push;
use super::mount_publish::{
    MountPublishQueue, PostPublishHook, PublishSink, MOUNT_PUBLISH_DEBOUNCE,
};
use super::mount_registry::{register_active_mount, ActiveMount};
use super::mount_target::{
    create_mountpoint_dir, derive_implicit_mountpoint, normalize_mount_target_for_resolution,
};
use super::resolve::{parse_published_target, resolve_cid_input_with_opts, ResolveOptions};
use super::run::format_cid_for_display;

pub(crate) struct MountVisibility {
    pub(crate) visibility: hashtree_core::TreeVisibility,
    pub(crate) link_key: Option<[u8; 32]>,
}

pub(crate) fn parse_mount_visibility(
    visibility: Option<String>,
    link_key: Option<String>,
    private: bool,
    fragment: Option<&str>,
) -> Result<MountVisibility> {
    use hashtree_core::TreeVisibility;

    let mut resolved_visibility: Option<TreeVisibility> = None;
    let mut resolved_link_key: Option<[u8; 32]> = None;

    if let Some(fragment) = fragment {
        if fragment == "private" {
            resolved_visibility = Some(TreeVisibility::Private);
        } else if fragment == "link-visible" {
            resolved_visibility = Some(TreeVisibility::LinkVisible);
        } else if let Some(hex_key) = fragment.strip_prefix("k=") {
            resolved_visibility = Some(TreeVisibility::LinkVisible);
            resolved_link_key = Some(
                hashtree_core::key_from_hex(hex_key)
                    .map_err(|e| anyhow::anyhow!("Invalid link key: {}", e))?,
            );
        }
    }

    if let Some(vis) = visibility {
        let parsed = TreeVisibility::from_str(&vis)
            .map_err(|e| anyhow::anyhow!("Invalid visibility: {}", e))?;
        if let Some(existing) = resolved_visibility {
            if existing != parsed {
                anyhow::bail!("Conflicting visibility options");
            }
        }
        resolved_visibility = Some(parsed);
    }

    if let Some(link_key) = link_key {
        let parsed = hashtree_core::key_from_hex(&link_key)
            .map_err(|e| anyhow::anyhow!("Invalid link key: {}", e))?;
        if let Some(existing) = resolved_link_key {
            if existing != parsed {
                anyhow::bail!("Conflicting link key options");
            }
        }
        resolved_link_key = Some(parsed);
        if let Some(existing) = resolved_visibility {
            if existing != TreeVisibility::LinkVisible {
                anyhow::bail!("Link key only applies to link-visible trees");
            }
        }
        resolved_visibility = Some(TreeVisibility::LinkVisible);
    }

    if private {
        if let Some(existing) = resolved_visibility {
            if existing != TreeVisibility::Private {
                anyhow::bail!("Conflicting visibility options");
            }
        }
        resolved_visibility = Some(TreeVisibility::Private);
    }

    let visibility = resolved_visibility.unwrap_or(TreeVisibility::Public);
    if visibility == TreeVisibility::LinkVisible && resolved_link_key.is_none() {
        anyhow::bail!("Link-visible trees require a link key");
    }
    if visibility == TreeVisibility::Private && resolved_link_key.is_some() {
        anyhow::bail!("Private trees cannot use a link key");
    }

    Ok(MountVisibility {
        visibility,
        link_key: resolved_link_key,
    })
}

struct NostrPublishSink {
    resolver: NostrRootResolver,
    key: String,
    visibility: hashtree_core::TreeVisibility,
    link_key: Option<[u8; 32]>,
}

struct BlossomPostPublishHook {
    data_dir: PathBuf,
    write_servers: Vec<String>,
}

#[async_trait]
impl PostPublishHook for BlossomPostPublishHook {
    async fn run(&self, cid: &hashtree_core::Cid) -> Result<()> {
        if self.write_servers.is_empty() {
            return Ok(());
        }

        background_blossom_push(&self.data_dir, &cid.to_string(), &self.write_servers)
            .await
            .context("Failed to push mounted root to configured file servers")
    }
}

#[async_trait]
impl PublishSink for NostrPublishSink {
    async fn publish(&self, cid: &hashtree_core::Cid) -> Result<()> {
        let published = match self.visibility {
            hashtree_core::TreeVisibility::Public => self.resolver.publish(&self.key, cid).await,
            hashtree_core::TreeVisibility::LinkVisible => {
                let Some(link_key) = self.link_key else {
                    anyhow::bail!("Missing link key");
                };
                self.resolver
                    .publish_shared(&self.key, cid, &link_key)
                    .await
            }
            hashtree_core::TreeVisibility::Private => {
                self.resolver.publish_private(&self.key, cid).await
            }
        }
        .context("Failed to publish mounted root")?;

        if !published {
            anyhow::bail!("Publish returned false");
        }

        Ok(())
    }
}

struct QueueingRootPublisher<Sink, StoreT>
where
    Sink: PublishSink + 'static,
    StoreT: hashtree_core::Store + 'static,
{
    queue: Arc<MountPublishQueue<Sink, StoreT>>,
}

const MOUNT_READY_TIMEOUT: Duration = Duration::from_secs(5);
const MOUNT_READY_POLL_INTERVAL: Duration = Duration::from_millis(50);

fn wait_for_mountpoint_ready(mountpoint: &Path) -> Result<()> {
    wait_for_mountpoint_ready_with(mountpoint, MOUNT_READY_TIMEOUT, |path| {
        let _ = std::fs::read_dir(path)?;
        Ok(())
    })
}

fn wait_for_mountpoint_ready_with<F>(
    mountpoint: &Path,
    timeout: Duration,
    mut probe: F,
) -> Result<()>
where
    F: FnMut(&Path) -> std::io::Result<()>,
{
    let deadline = Instant::now() + timeout;
    let mut last_error = None;

    loop {
        match probe(mountpoint) {
            Ok(()) => return Ok(()),
            Err(error) => {
                let error_text = error.to_string();
                if Instant::now() >= deadline {
                    return Err(anyhow::anyhow!(
                        "Timed out waiting for FUSE mount {} to become readable: {}",
                        mountpoint.display(),
                        last_error.unwrap_or(error_text),
                    ));
                }
                last_error = Some(error_text);
            }
        }

        std::thread::sleep(MOUNT_READY_POLL_INTERVAL);
    }
}

impl<Sink, StoreT> RootPublisher for QueueingRootPublisher<Sink, StoreT>
where
    Sink: PublishSink + 'static,
    StoreT: hashtree_core::Store + 'static,
{
    fn publish(&self, cid: &hashtree_core::Cid) -> Result<(), FuseFsError> {
        self.queue
            .enqueue(cid.clone())
            .map_err(|e| FuseFsError::Publish(e.to_string()))?;
        Ok(())
    }
}

pub(crate) async fn mount_fuse(
    target: String,
    mountpoint: Option<PathBuf>,
    visibility: Option<String>,
    link_key: Option<String>,
    private: bool,
    relays: Option<String>,
    allow_other: bool,
    data_dir: PathBuf,
) -> Result<()> {
    let current_dir = std::env::current_dir()?;
    let (mountpoint, implicit_mountpoint) = match mountpoint {
        Some(path) => (path, false),
        None => (derive_implicit_mountpoint(&current_dir, &target)?, true),
    };
    let mountpoint = if mountpoint.is_relative() {
        current_dir.join(mountpoint)
    } else {
        mountpoint
    };

    let target = target.strip_prefix("htree://").unwrap_or(&target);
    let (base, fragment) = match target.split_once('#') {
        Some((base, fragment)) => (base, Some(fragment)),
        None => (target, None),
    };
    let base = normalize_mount_target_for_resolution(base)?;
    let requested_target = base.clone();

    let MountVisibility {
        visibility: mount_visibility,
        link_key: mount_link_key,
    } = parse_mount_visibility(visibility, link_key, private, fragment)?;

    let config = Config::load().unwrap_or_default();
    let relays = if let Some(relays) = relays {
        relays.split(',').map(|s| s.trim().to_string()).collect()
    } else {
        config.nostr.relays.clone()
    };

    let mut opts = ResolveOptions::default();
    opts.link_key = mount_link_key;
    opts.private = mount_visibility == hashtree_core::TreeVisibility::Private;
    opts.relays = Some(relays);
    opts.data_dir = Some(data_dir.clone());

    if opts.private {
        let keys =
            hashtree_cli::config::read_keys().context("Private mounts require a local nsec key")?;
        opts.secret_key = Some(keys);
    }

    let resolved = resolve_cid_input_with_opts(&base, &opts).await?;
    let published_target = parse_published_target(&base);
    let nostr_key = published_target
        .as_ref()
        .map(|target| target.resolver_key());
    let visibility_str = mount_visibility.as_str().to_string();

    let max_size_bytes = config.storage.max_size_gb * 1024 * 1024 * 1024;
    let store = Arc::new(HashtreeStore::with_options(
        &data_dir,
        config.storage.s3.as_ref(),
        max_size_bytes,
    )?);
    let store_arc = store.store_arc();

    let mut root_cid = resolved.cid.clone();
    if let Some(path) = resolved.path.clone() {
        let tree =
            hashtree_core::HashTree::new(hashtree_core::HashTreeConfig::new(store_arc.clone()));
        let Some(path_cid) = tree.resolve(&root_cid, &path).await? else {
            anyhow::bail!("Path not found: {}", path);
        };
        let is_dir = tree.get_directory_node(&path_cid).await?.is_some();
        if !is_dir {
            anyhow::bail!("Path is not a directory: {}", path);
        }
        root_cid = path_cid;
    }

    let link_key_hex = mount_link_key.map(hex::encode);
    let publish_queue = if let Some(ref nostr_key) = nostr_key {
        let keys = hashtree_cli::config::read_keys().context("Failed to read nostr keys")?;
        let mut resolver_config = NostrResolverConfig::default();
        if let Some(relays) = opts.relays.clone() {
            resolver_config.relays = relays;
        }
        resolver_config.secret_key = Some(keys.clone());
        let resolver = NostrRootResolver::new(resolver_config)
            .await
            .context("Failed to create nostr resolver")?;

        let published_target = published_target
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Invalid nostr key: {}", nostr_key))?;
        let pubkey_bytes = hashtree_cli::config::parse_npub(&published_target.npub)?;
        if keys.public_key().to_bytes() != pubkey_bytes {
            anyhow::bail!("Nostr key does not match mounted npub");
        }
        let pubkey_hex = hex::encode(pubkey_bytes);
        let visibility_str_for_cache = visibility_str.clone();
        let mounted_path = published_target
            .path
            .as_deref()
            .map(|path| {
                path.split('/')
                    .filter(|segment| !segment.is_empty())
                    .map(|segment| segment.to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let tree_name = published_target.tree_name.clone();
        let publish_sink = Arc::new(NostrPublishSink {
            resolver,
            key: nostr_key.clone(),
            visibility: mount_visibility,
            link_key: mount_link_key,
        });
        let success_store = store.clone();
        let success_hook = Arc::new(move |cid: &hashtree_core::Cid| {
            let key_hex = cid.key.map(hex::encode);
            let updated_at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            if let Err(error) = success_store.set_cached_root(
                &pubkey_hex,
                &tree_name,
                &hashtree_core::to_hex(&cid.hash),
                key_hex.as_deref(),
                &visibility_str_for_cache,
                updated_at,
            ) {
                eprintln!("Failed to cache mounted root publish: {error}");
            }
        });

        let write_servers = config.blossom.all_write_servers();
        let post_publish_hook = if write_servers.is_empty() {
            None
        } else {
            Some(Arc::new(BlossomPostPublishHook {
                data_dir: data_dir.clone(),
                write_servers,
            }) as Arc<dyn PostPublishHook>)
        };

        let queue = Arc::new(MountPublishQueue::new(
            publish_sink,
            store_arc.clone(),
            resolved.cid.clone(),
            mounted_path,
            MOUNT_PUBLISH_DEBOUNCE,
            post_publish_hook,
            Some(success_hook),
        ));

        Some(queue)
    } else {
        None
    };

    let publisher: Option<Arc<dyn RootPublisher>> = publish_queue.as_ref().map(|queue| {
        Arc::new(QueueingRootPublisher {
            queue: queue.clone(),
        }) as Arc<dyn RootPublisher>
    });

    let mounted_cid = format_cid_for_display(&root_cid);
    let fs = HashtreeFuse::new_with_publisher(store_arc, root_cid, publisher)?;
    let mut options = vec![
        fuser::MountOption::FSName("hashtree".to_string()),
        fuser::MountOption::DefaultPermissions,
    ];
    if allow_other {
        options.push(fuser::MountOption::AllowOther);
    }

    if implicit_mountpoint {
        create_mountpoint_dir(&mountpoint)?;
    }

    let session = match fuser::Session::new(fs, &mountpoint, &options) {
        Ok(session) => session,
        Err(error) => {
            if implicit_mountpoint {
                let _ = std::fs::remove_dir(&mountpoint);
            }
            return Err(error.into());
        }
    };

    let background = match session.spawn() {
        Ok(background) => background,
        Err(error) => {
            if implicit_mountpoint {
                let _ = std::fs::remove_dir(&mountpoint);
            }
            return Err(error.into());
        }
    };

    if let Err(error) = wait_for_mountpoint_ready(&mountpoint) {
        if implicit_mountpoint {
            let _ = std::fs::remove_dir(&mountpoint);
        }
        return Err(error);
    }

    let registration = register_active_mount(
        &data_dir,
        &ActiveMount {
            target: requested_target,
            mountpoint: mountpoint.clone(),
            mounted_cid,
            visibility: visibility_str.clone(),
            published_key: nostr_key.clone(),
            allow_other,
            pid: std::process::id(),
            registered_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        },
    )?;

    println!("mounted {}", mountpoint.display());
    if let Some(target) = published_target.as_ref() {
        println!(
            "  drive: {}",
            build_drive_iris_to_url_for_published_target(
                &target.npub,
                &target.tree_name,
                target.path.as_deref(),
                link_key_hex.as_deref(),
            )
        );
        println!(
            "  publish: updates debounce for ~{} ms",
            MOUNT_PUBLISH_DEBOUNCE.as_millis()
        );
    }

    let run_result = background
        .guard
        .join()
        .map_err(|_| anyhow::anyhow!("FUSE session thread panicked"))?;
    if run_result.is_err() && implicit_mountpoint {
        let _ = std::fs::remove_dir(&mountpoint);
    }
    run_result?;
    drop(registration);
    if let Some(queue) = publish_queue {
        queue.shutdown().await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_for_mountpoint_ready_retries_until_probe_succeeds() {
        let mountpoint = Path::new("/tmp/hashtree-ready");
        let mut attempts = 0;

        wait_for_mountpoint_ready_with(mountpoint, Duration::from_millis(200), |_| {
            attempts += 1;
            if attempts < 3 {
                Err(std::io::Error::other("still starting"))
            } else {
                Ok(())
            }
        })
        .expect("mountpoint becomes readable");

        assert_eq!(attempts, 3);
    }

    #[test]
    fn wait_for_mountpoint_ready_times_out_with_last_error() {
        let mountpoint = Path::new("/tmp/hashtree-stuck");

        let error = wait_for_mountpoint_ready_with(mountpoint, Duration::from_millis(1), |_| {
            Err(std::io::Error::other("transport not ready"))
        })
        .expect_err("mountpoint should time out");

        let message = error.to_string();
        assert!(message.contains("Timed out waiting for FUSE mount"));
        assert!(message.contains("transport not ready"));
    }
}
