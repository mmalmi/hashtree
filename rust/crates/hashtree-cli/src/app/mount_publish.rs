#![cfg_attr(not(feature = "fuse"), allow(dead_code))]

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::future::pending;
use hashtree_core::{Cid, HashTree, HashTreeConfig, LinkType, Store};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::time::Sleep;

pub(crate) const MOUNT_PUBLISH_DEBOUNCE: Duration = Duration::from_secs(1);

type SuccessHook = Arc<dyn Fn(&Cid) + Send + Sync>;

#[async_trait]
pub(crate) trait PublishSink: Send + Sync {
    async fn publish(&self, cid: &Cid) -> Result<()>;
}

#[async_trait]
pub(crate) trait PostPublishHook: Send + Sync {
    async fn run(&self, cid: &Cid) -> Result<()>;
}

enum PublishRequest {
    Update(Cid),
    Shutdown(oneshot::Sender<Result<()>>),
}

pub(crate) struct MountPublishQueue<Sink, StoreT>
where
    Sink: PublishSink + 'static,
    StoreT: Store + 'static,
{
    tx: mpsc::UnboundedSender<PublishRequest>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    _marker: std::marker::PhantomData<(Sink, StoreT)>,
}

impl<Sink, StoreT> MountPublishQueue<Sink, StoreT>
where
    Sink: PublishSink + 'static,
    StoreT: Store + 'static,
{
    pub(crate) fn new(
        sink: Arc<Sink>,
        store: Arc<StoreT>,
        initial_published_root: Cid,
        mounted_path: Vec<String>,
        debounce: Duration,
        post_publish_hook: Option<Arc<dyn PostPublishHook>>,
        success_hook: Option<SuccessHook>,
    ) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut current_published_root = initial_published_root;
            let mut pending_mounted_root: Option<Cid> = None;
            let mut delay: Option<Pin<Box<Sleep>>> = None;

            loop {
                tokio::select! {
                    request = rx.recv() => {
                        match request {
                            Some(PublishRequest::Update(cid)) => {
                                pending_mounted_root = Some(cid);
                                delay = Some(Box::pin(tokio::time::sleep(debounce)));
                            }
                            Some(PublishRequest::Shutdown(reply)) => {
                                let result = if let Some(mounted_root) = pending_mounted_root.take() {
                                    publish_latest(
                                        sink.as_ref(),
                                        store.clone(),
                                        &current_published_root,
                                        &mounted_path,
                                        &mounted_root,
                                        post_publish_hook.as_deref(),
                                        success_hook.as_ref(),
                                    ).await.map(|new_root| {
                                        current_published_root = new_root;
                                    })
                                } else {
                                    Ok(())
                                };
                                let _ = reply.send(result);
                                break;
                            }
                            None => {
                                if let Some(mounted_root) = pending_mounted_root.take() {
                                    let _ = publish_latest(
                                        sink.as_ref(),
                                        store.clone(),
                                        &current_published_root,
                                        &mounted_path,
                                        &mounted_root,
                                        post_publish_hook.as_deref(),
                                        success_hook.as_ref(),
                                    ).await.map(|new_root| {
                                        current_published_root = new_root;
                                    });
                                }
                                break;
                            }
                        }
                    }
                    _ = async {
                        if let Some(active_delay) = delay.as_mut() {
                            active_delay.as_mut().await;
                        } else {
                            pending::<()>().await;
                        }
                    } => {
                        delay = None;
                        if let Some(mounted_root) = pending_mounted_root.take() {
                            match publish_latest(
                                sink.as_ref(),
                                store.clone(),
                                &current_published_root,
                                &mounted_path,
                                &mounted_root,
                                post_publish_hook.as_deref(),
                                success_hook.as_ref(),
                            ).await {
                                Ok(new_root) => {
                                    current_published_root = new_root;
                                }
                                Err(error) => {
                                    eprintln!("Mount publish failed: {error:#}");
                                    pending_mounted_root = Some(mounted_root);
                                    delay = Some(Box::pin(tokio::time::sleep(debounce)));
                                }
                            }
                        }
                    }
                }
            }
        });

        Self {
            tx,
            task: Mutex::new(Some(task)),
            _marker: std::marker::PhantomData,
        }
    }

    pub(crate) fn enqueue(&self, cid: Cid) -> Result<()> {
        self.tx
            .send(PublishRequest::Update(cid))
            .map_err(|_| anyhow::anyhow!("mount publish worker stopped"))
    }

    pub(crate) async fn shutdown(&self) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(PublishRequest::Shutdown(reply_tx))
            .map_err(|_| anyhow::anyhow!("mount publish worker stopped"))?;

        let result = reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("mount publish worker dropped shutdown reply"))?;

        let task = self.task.lock().await.take();
        if let Some(task) = task {
            task.await
                .map_err(|error| anyhow::anyhow!("mount publish worker join failed: {error}"))?;
        }

        result
    }
}

async fn publish_latest<Sink, StoreT>(
    sink: &Sink,
    store: Arc<StoreT>,
    current_published_root: &Cid,
    mounted_path: &[String],
    mounted_root: &Cid,
    post_publish_hook: Option<&dyn PostPublishHook>,
    success_hook: Option<&SuccessHook>,
) -> Result<Cid>
where
    Sink: PublishSink + 'static,
    StoreT: Store + 'static,
{
    let published_root =
        rebuild_published_root(store, current_published_root, mounted_path, mounted_root).await?;
    if let Some(hook) = post_publish_hook {
        hook.run(&published_root).await?;
    }
    sink.publish(&published_root).await?;
    if let Some(hook) = success_hook {
        hook(&published_root);
    }
    Ok(published_root)
}

pub(crate) async fn rebuild_published_root<StoreT>(
    store: Arc<StoreT>,
    current_published_root: &Cid,
    mounted_path: &[String],
    mounted_root: &Cid,
) -> Result<Cid>
where
    StoreT: Store + 'static,
{
    if mounted_path.is_empty() {
        return Ok(mounted_root.clone());
    }

    let mut config = HashTreeConfig::new(store);
    if current_published_root.key.is_none() {
        config = config.public();
    }
    let tree = HashTree::new(config);
    let (name, parent_path) = mounted_path
        .split_last()
        .context("Mounted path missing final segment")?;
    let parent_refs = parent_path
        .iter()
        .map(|segment| segment.as_str())
        .collect::<Vec<_>>();

    tree.set_entry(
        current_published_root,
        &parent_refs,
        name,
        mounted_root,
        0,
        LinkType::Dir,
    )
    .await
    .context("Failed to rebuild published root for mounted subtree")
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashtree_core::store::MemoryStore;
    use std::sync::Mutex;

    struct RecordingPublishSink {
        published: Mutex<Vec<Cid>>,
    }

    impl RecordingPublishSink {
        fn new() -> Self {
            Self {
                published: Mutex::new(Vec::new()),
            }
        }

        fn published(&self) -> Vec<Cid> {
            self.published.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl PublishSink for RecordingPublishSink {
        async fn publish(&self, cid: &Cid) -> Result<()> {
            self.published.lock().unwrap().push(cid.clone());
            Ok(())
        }
    }

    struct RecordingPostPublishHook {
        published: Mutex<Vec<Cid>>,
    }

    impl RecordingPostPublishHook {
        fn new() -> Self {
            Self {
                published: Mutex::new(Vec::new()),
            }
        }

        fn published(&self) -> Vec<Cid> {
            self.published.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl PostPublishHook for RecordingPostPublishHook {
        async fn run(&self, cid: &Cid) -> Result<()> {
            self.published.lock().unwrap().push(cid.clone());
            Ok(())
        }
    }

    async fn empty_root(store: Arc<MemoryStore>) -> Cid {
        let tree = HashTree::new(HashTreeConfig::new(store));
        tree.put_directory(Vec::new()).await.unwrap()
    }

    #[tokio::test]
    async fn rebuild_published_root_preserves_siblings_for_subdirectory_mounts() {
        let store = Arc::new(MemoryStore::new());
        let tree = HashTree::new(HashTreeConfig::new(store.clone()));

        let full_root = empty_root(store.clone()).await;
        let docs_root = empty_root(store.clone()).await;
        let full_root = tree
            .set_entry(
                &full_root,
                &Vec::<&str>::new(),
                "docs",
                &docs_root,
                0,
                LinkType::Dir,
            )
            .await
            .unwrap();
        let (keep_cid, keep_size) = tree.put(b"keep").await.unwrap();
        let full_root = tree
            .set_entry(
                &full_root,
                &Vec::<&str>::new(),
                "keep.txt",
                &keep_cid,
                keep_size,
                LinkType::Blob,
            )
            .await
            .unwrap();

        let (draft_cid, draft_size) = tree.put(b"draft").await.unwrap();
        let updated_docs_root = tree
            .set_entry(
                &docs_root,
                &Vec::<&str>::new(),
                "draft.txt",
                &draft_cid,
                draft_size,
                LinkType::Blob,
            )
            .await
            .unwrap();

        let rebuilt = rebuild_published_root(
            store.clone(),
            &full_root,
            &["docs".to_string()],
            &updated_docs_root,
        )
        .await
        .unwrap();

        let keep_resolved = tree.resolve(&rebuilt, "keep.txt").await.unwrap();
        assert!(
            keep_resolved.is_some(),
            "sibling entry should remain in tree"
        );

        let draft_resolved = tree.resolve(&rebuilt, "docs/draft.txt").await.unwrap();
        assert_eq!(draft_resolved, Some(draft_cid));
    }

    #[tokio::test]
    async fn debounced_mount_publisher_coalesces_rapid_updates() {
        let sink = Arc::new(RecordingPublishSink::new());
        let store = Arc::new(MemoryStore::new());
        let root = empty_root(store.clone()).await;
        let queue = MountPublishQueue::new(
            sink.clone(),
            store,
            root,
            Vec::new(),
            Duration::from_millis(25),
            None,
            None,
        );

        let cid_a = Cid {
            hash: [0x11; 32],
            key: None,
        };
        let cid_b = Cid {
            hash: [0x22; 32],
            key: None,
        };

        queue.enqueue(cid_a).unwrap();
        queue.enqueue(cid_b.clone()).unwrap();

        tokio::time::sleep(Duration::from_millis(80)).await;
        queue.shutdown().await.unwrap();

        assert_eq!(sink.published(), vec![cid_b]);
    }

    #[tokio::test]
    async fn debounced_mount_publisher_runs_post_publish_hook() {
        let sink = Arc::new(RecordingPublishSink::new());
        let hook = Arc::new(RecordingPostPublishHook::new());
        let store = Arc::new(MemoryStore::new());
        let root = empty_root(store.clone()).await;
        let queue = MountPublishQueue::new(
            sink.clone(),
            store,
            root,
            Vec::new(),
            Duration::from_millis(25),
            Some(hook.clone()),
            None,
        );

        let cid = Cid {
            hash: [0x33; 32],
            key: None,
        };

        queue.enqueue(cid.clone()).unwrap();

        tokio::time::sleep(Duration::from_millis(80)).await;
        queue.shutdown().await.unwrap();

        assert_eq!(hook.published(), vec![cid.clone()]);
        assert_eq!(sink.published(), vec![cid]);
    }
}
