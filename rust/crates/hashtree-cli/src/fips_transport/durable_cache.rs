//! A private, single-writer namespace for retained signed heads. It uses the
//! normal Hashtree event index and metadata transaction, independently of the
//! daemon's disposable blob cache and its concurrent eviction passes.

use super::DaemonNostrCache;
use crate::storage::HashtreeStore;
use anyhow::{Context, Result};
use hashtree_core::Cid;
use hashtree_nostr_pubsub::{EventIndexCheckpoint, HashtreeNostrBoundedEventCache};
use nostr_pubsub::{EventRetentionPolicy, EventSource, Filter, PubsubError};
use std::fs::{File, OpenOptions};
use std::path::Path;
use std::sync::Arc;

const MAX_HEADS: usize = 4_096;
const NAMESPACE: &str = "nostr-pubsub-heads";

struct Checkpoint {
    store: HashtreeStore,
    _writer: File,
}

impl Checkpoint {
    fn recover(&self, root: Option<&Cid>) -> Result<()> {
        if self.read_root()?.as_ref() != root {
            anyhow::bail!("Retained head checkpoint changed; reopen the store before writing");
        }
        let retained = root
            .map(|root| self.store.nostr_index_hashes(root))
            .transpose()?
            .unwrap_or_default();
        let stale = self
            .store
            .router()
            .list_writable()?
            .into_iter()
            .filter(|hash| !retained.contains(hash))
            .collect::<Vec<_>>();
        for batch in stale.chunks(256) {
            self.store.router().delete_many_local_only(batch)?;
        }
        Ok(())
    }

    fn read_root(&self) -> Result<Option<Cid>> {
        self.store
            .get_cached_root("local-index", "heads")?
            .map(|root| {
                Ok(Cid {
                    hash: hex::decode(root.hash)?
                        .try_into()
                        .map_err(|_| anyhow::anyhow!("Invalid head index hash"))?,
                    key: root
                        .key
                        .map(|key| -> Result<_> {
                            hex::decode(key)?
                                .try_into()
                                .map_err(|_| anyhow::anyhow!("Invalid head index key"))
                        })
                        .transpose()?,
                })
            })
            .transpose()
    }

    fn commit_root(&self, root: &Cid) -> Result<()> {
        self.commit_root_with_cleanup(root, |stale| {
            for batch in stale.chunks(256) {
                self.store.router().delete_many_local_only(batch)?;
            }
            Ok(())
        })
    }

    fn commit_root_with_cleanup(
        &self,
        root: &Cid,
        cleanup: impl FnOnce(&[hashtree_core::Hash]) -> Result<()>,
    ) -> Result<()> {
        // Resolve the complete index before replacing the checkpoint. Raw event
        // values are leaves; their exact signed bytes stay in this DAG.
        let retained = self.store.nostr_index_hashes(root)?;
        let stats = self.store.router().writable_stats()?;
        if stats.total_bytes > 512 * 1024 * 1024 {
            anyhow::bail!("Retained head index exceeds its disk budget");
        }
        let stale = self
            .store
            .router()
            .list_writable()?
            .into_iter()
            .filter(|hash| !retained.contains(hash))
            .collect::<Vec<_>>();
        self.store.force_sync()?;
        self.store.set_cached_root(
            "local-index",
            "heads",
            &hex::encode(root.hash),
            root.key.as_ref().map(hex::encode).as_deref(),
            "private",
            0,
        )?;
        // Readers and the next writer are excluded by the cache's owned mutex.
        // No user blobs, alternate roots, or external writers share this store.
        if let Err(error) = cleanup(&stale) {
            // The new checkpoint is already authoritative; cleanup failure
            // must not roll the in-memory pointer back to a retired root.
            tracing::warn!(%error, "Retained head index cleanup failed");
        }
        Ok(())
    }
}

impl EventIndexCheckpoint for Checkpoint {
    fn prepare(&self, root: Option<&Cid>) -> nostr_pubsub::Result<()> {
        self.recover(root)
            .map_err(|error| PubsubError::Storage(error.to_string()))
    }

    fn commit(&self, root: Option<&Cid>) -> nostr_pubsub::Result<()> {
        let root = root.ok_or_else(|| PubsubError::Storage("Head index has no root".into()))?;
        self.commit_root(root)
            .map_err(|error| PubsubError::Storage(error.to_string()))
    }
}

pub fn open(parent: &Path) -> Result<Arc<DaemonNostrCache>> {
    let path = parent.join(NAMESPACE);
    std::fs::create_dir_all(&path)?;
    let writer = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.join("writer.lock"))?;
    lock_writer(&writer).context("Retained head index is already open by another daemon")?;
    let mut store = HashtreeStore::new_with_backend(
        &path,
        hashtree_config::StorageBackend::Lmdb,
        256 * 1024 * 1024,
    )?;
    store.enable_durable_metadata_commits()?;
    let checkpoint = Arc::new(Checkpoint {
        store,
        _writer: writer,
    });
    let root = checkpoint.read_root()?;
    if let Some(root) = &root {
        checkpoint
            .store
            .nostr_index_hashes(root)
            .context("Invalid retained head checkpoint")?;
    }
    checkpoint.recover(root.as_ref())?;
    Ok(Arc::new(
        HashtreeNostrBoundedEventCache::new(
            checkpoint.store.store_arc(),
            root,
            EventSource::local_index("hashtree-retained-heads"),
            EventRetentionPolicy::new(
                MAX_HEADS,
                vec![Filter::new().kinds([nostr::Kind::Custom(30064), nostr::Kind::Custom(30078)])],
            ),
        )
        .with_checkpoint(checkpoint)
        .with_priority(nostr_pubsub::SOURCE_PRIORITY_LOCAL_INDEX),
    ))
}

#[cfg(unix)]
fn lock_writer(file: &File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: file owns the descriptor for the complete cache lifetime.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(unix))]
fn lock_writer(file: &File) -> std::io::Result<()> {
    file.try_lock()
        .map_err(|error| std::io::Error::other(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NostrRootResolver;
    use nostr_pubsub::{EventBus, QueryOptions, VerifiedEvent};

    fn signed_head(keys: &nostr::Keys, tree: &str, timestamp: u64) -> nostr::Event {
        NostrRootResolver::root_event_builder(tree, &Cid::public([7; 32]), None)
            .custom_created_at(nostr::Timestamp::from(timestamp))
            .sign_with_keys(keys)
            .unwrap()
    }

    #[tokio::test]
    async fn retained_heads_reopen_with_exact_signatures_and_exclusive_writer() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let cache = open(temp.path())?;
        assert!(
            open(temp.path()).is_err(),
            "two writers must not share this namespace"
        );
        let keys = nostr::Keys::generate();
        let mut expected = Vec::new();
        // These events are intentionally old and exceed the FIPS hot cache.
        for index in 0..12 {
            let event = signed_head(&keys, &format!("releases/app-{index}"), index + 1);
            cache
                .publish(
                    VerifiedEvent::try_from(event.clone())?,
                    EventSource::peer("relay"),
                )
                .await?;
            expected.push(event);
        }
        drop(cache);
        let reopened = open(temp.path())?;
        for event in expected {
            let report = reopened
                .query(
                    vec![Filter::new().id(event.id)],
                    QueryOptions { limit: Some(1) },
                )
                .await?;
            assert_eq!(report.events.len(), 1);
            assert_eq!(report.events[0].event.as_event(), &event);
        }
        Ok(())
    }

    #[tokio::test]
    async fn repeated_head_updates_reclaim_superseded_index_blocks() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let cache = open(temp.path())?;
        let keys = nostr::Keys::generate();
        let anchored = signed_head(&keys, "releases/anchored", 1);
        cache
            .publish(
                VerifiedEvent::try_from(anchored.clone())?,
                EventSource::peer("relay"),
            )
            .await?;
        for timestamp in 2..102 {
            cache
                .publish(
                    VerifiedEvent::try_from(signed_head(&keys, "releases/moving", timestamp))?,
                    EventSource::peer("relay"),
                )
                .await?;
        }
        drop(cache);
        let store = HashtreeStore::new_with_backend(
            temp.path().join(NAMESPACE),
            hashtree_config::StorageBackend::Lmdb,
            256 * 1024 * 1024,
        )?;
        assert!(
            store.router().writable_stats()?.total_bytes < 1024 * 1024,
            "two current heads must not retain a hundred obsolete index DAGs"
        );
        drop(store);
        let reopened = open(temp.path())?;
        let report = reopened
            .query(vec![Filter::new()], QueryOptions::default())
            .await?;
        assert_eq!(report.events.len(), 2);
        assert!(report
            .events
            .iter()
            .any(|event| event.event.as_event() == &anchored));
        assert_eq!(report.events[0].event.as_event().created_at.as_secs(), 101);
        Ok(())
    }

    #[test]
    fn missing_checkpoint_blocks_fail_closed_instead_of_resetting_history() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = HashtreeStore::new_with_backend(
            temp.path().join(NAMESPACE),
            hashtree_config::StorageBackend::Lmdb,
            256 * 1024 * 1024,
        )?;
        store.set_cached_root(
            "local-index",
            "heads",
            &hex::encode([99; 32]),
            None,
            "private",
            0,
        )?;
        drop(store);
        assert!(open(temp.path()).is_err());
        let store = HashtreeStore::new_with_backend(
            temp.path().join(NAMESPACE),
            hashtree_config::StorageBackend::Lmdb,
            256 * 1024 * 1024,
        )?;
        assert_eq!(
            store.get_cached_root("local-index", "heads")?.unwrap().hash,
            hex::encode([99; 32])
        );
        Ok(())
    }

    #[tokio::test]
    async fn cleanup_failure_after_commit_keeps_the_new_checkpoint_authoritative() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join(NAMESPACE);
        let mut store = HashtreeStore::new_with_backend(
            &path,
            hashtree_config::StorageBackend::Lmdb,
            256 * 1024 * 1024,
        )?;
        store.enable_durable_metadata_commits()?;
        let checkpoint = Checkpoint {
            store,
            _writer: File::create(path.join("writer.lock"))?,
        };
        lock_writer(&checkpoint._writer)?;
        let event = signed_head(&nostr::Keys::generate(), "releases/test", 1);
        let root = hashtree_nostr::NostrEventStore::new(checkpoint.store.store_arc())
            .build(
                None,
                [hashtree_nostr::stored_event_from_nostr_sdk_event(&event)],
            )
            .await?
            .unwrap();
        checkpoint
            .commit_root_with_cleanup(&root, |_| anyhow::bail!("injected cleanup failure"))?;
        assert_eq!(checkpoint.read_root()?, Some(root.clone()));
        checkpoint.recover(Some(&root))?;
        drop(checkpoint);
        let reopened = open(temp.path())?;
        let report = reopened
            .query(vec![Filter::new()], QueryOptions::default())
            .await?;
        assert_eq!(report.events[0].event.as_event(), &event);
        Ok(())
    }
}
