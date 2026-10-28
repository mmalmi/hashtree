use super::*;
use hashtree_core::{Hash, StoreError};
use std::sync::mpsc;
use std::time::Duration;

struct StalledStore {
    entered: mpsc::Sender<()>,
    dropped: mpsc::Sender<()>,
    release: Arc<tokio::sync::Notify>,
}

struct ReadGuard(mpsc::Sender<()>);
impl Drop for ReadGuard {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

#[async_trait]
impl Store for StalledStore {
    async fn get(&self, _: &Hash) -> std::result::Result<Option<Vec<u8>>, StoreError> {
        let _guard = ReadGuard(self.dropped.clone());
        self.entered.send(()).unwrap();
        self.release.notified().await;
        Ok(None)
    }
    async fn put(&self, _: Hash, _: Vec<u8>) -> std::result::Result<bool, StoreError> {
        panic!("read-only query must not write")
    }
    async fn has(&self, _: &Hash) -> std::result::Result<bool, StoreError> {
        panic!("unexpected has")
    }
    async fn delete(&self, _: &Hash) -> std::result::Result<bool, StoreError> {
        panic!("read-only query must not delete")
    }
}

#[test]
fn cancelling_index_query_releases_read_and_blocking_runtime() {
    let (entered_tx, entered) = mpsc::channel();
    let (dropped_tx, dropped) = mpsc::channel();
    let (cancel_tx, cancel) = tokio::sync::oneshot::channel();
    let (finished_tx, finished) = mpsc::channel();
    let release = Arc::new(tokio::sync::Notify::new());
    let store = Arc::new(StalledStore {
        entered: entered_tx,
        dropped: dropped_tx,
        release: release.clone(),
    });
    let thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let query = tokio::spawn(async move {
                let bus = HashtreeNostrIndexEventBus::new(
                    store,
                    Some(Cid::public([1; 32])),
                    EventSource::local_index("cancel-test"),
                );
                bus.query(vec![], QueryOptions::default()).await
            });
            cancel.await.unwrap();
            query.abort();
            assert!(query.await.unwrap_err().is_cancelled());
        });
        // Dropping the runtime waits for blocking workers. A dropped caller
        // JoinHandle alone must not make this test pass.
        drop(runtime);
        finished_tx.send(()).unwrap();
    });
    let started = entered.recv_timeout(Duration::from_secs(5));
    cancel_tx.send(()).unwrap();
    let read_released = dropped.recv_timeout(Duration::from_secs(2));
    let runtime_released = finished.recv_timeout(Duration::from_secs(2));
    // Release the old implementation too, so a regression fails without
    // stranding a worker or hanging the test process.
    release.notify_one();
    thread.join().unwrap();
    assert!(started.is_ok(), "query never entered Store::get");
    assert!(read_released.is_ok(), "cancelled query retained its read");
    assert!(
        runtime_released.is_ok(),
        "cancelled query retained its worker/runtime"
    );
}
