use futures::{
    future::{BoxFuture, WeakShared},
    FutureExt,
};
use hashtree_core::Hash;
use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex, Weak},
};

struct Entry<T> {
    operation: WeakShared<BoxFuture<'static, T>>,
    generation: Arc<()>,
}

type Entries<T> = Mutex<HashMap<Hash, Entry<T>>>;

/// An operation stays alive only while a subscriber is waiting for it.
#[derive(Clone)]
pub(super) struct InFlight<T>(Arc<Entries<T>>);

impl<T> Default for InFlight<T> {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(HashMap::new())))
    }
}

impl<T: Clone + Send + Sync + 'static> InFlight<T> {
    pub(super) async fn run<F>(&self, hash: Hash, create: impl FnOnce() -> F) -> T
    where
        F: Future<Output = T> + Send + 'static,
    {
        let operation = {
            let mut entries = self.0.lock().expect("in-flight registry poisoned");
            if let Some(current) = entries
                .get(&hash)
                .and_then(|entry| entry.operation.upgrade())
            {
                current
            } else {
                let future = create();
                let generation = Arc::new(());
                let registration = Registration {
                    entries: Arc::downgrade(&self.0),
                    hash,
                    generation: generation.clone(),
                };
                let operation = async move {
                    // Dropped on completion or when the last subscriber cancels.
                    let _registration = registration;
                    future.await
                }
                .boxed()
                .shared();
                entries.insert(
                    hash,
                    Entry {
                        operation: operation.downgrade().expect("new operation"),
                        generation,
                    },
                );
                operation
            }
        };
        operation.await
    }
}

struct Registration<T> {
    entries: Weak<Entries<T>>,
    hash: Hash,
    generation: Arc<()>,
}

impl<T> Drop for Registration<T> {
    fn drop(&mut self) {
        if let Some(entries) = self.entries.upgrade() {
            let mut entries = entries.lock().expect("in-flight registry poisoned");
            if entries
                .get(&self.hash)
                .is_some_and(|entry| Arc::ptr_eq(&entry.generation, &self.generation))
            {
                entries.remove(&self.hash);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn old_registration_drop_preserves_the_successor_for_the_same_hash() {
        let inflight = InFlight::default();
        let hash = [3; 32];
        let mut old = Box::pin(inflight.run(hash, std::future::pending));
        assert!(futures::poll!(&mut old).is_pending());
        // Model another subscriber replacing an entry whose weak operation
        // can no longer be upgraded but whose registration has not dropped.
        inflight.0.lock().unwrap().remove(&hash);
        let (release, blocked) = tokio::sync::oneshot::channel();
        let mut successor = Box::pin(inflight.run(hash, || async move { blocked.await.unwrap() }));
        assert!(futures::poll!(&mut successor).is_pending());

        drop(old);
        let mut joined = Box::pin(inflight.run(hash, || async { panic!("lost successor") }));
        assert!(futures::poll!(&mut joined).is_pending());
        release.send(42).unwrap();
        assert_eq!(joined.await, 42);
        assert_eq!(successor.await, 42);
        assert!(inflight.0.lock().unwrap().is_empty());
    }
}
