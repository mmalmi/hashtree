//! The ledger together with the store that records it.
//!
//! Credits are written before they count. Charges are counted at once and
//! written a moment later, in one batch: a crash can forgive the last
//! charges, never lose a payment.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::ledger::Ledger;
use crate::store::{LedgerEntry, LedgerStore};

/// The longest a charge waits before it is written.
const WRITE_BEHIND: Duration = Duration::from_secs(1);

pub(crate) struct Books {
    ledger: Mutex<Ledger>,
    store: Option<Arc<dyn LedgerStore>>,
    /// Held across every store write, so the log has the ledger's order.
    writes: Mutex<()>,
    flush_scheduled: AtomicBool,
}

impl Books {
    pub(crate) fn new(ledger: Ledger, store: Option<Arc<dyn LedgerStore>>) -> Self {
        Self {
            ledger: Mutex::new(ledger),
            store,
            writes: Mutex::new(()),
            flush_scheduled: AtomicBool::new(false),
        }
    }

    pub(crate) fn ledger(&self) -> MutexGuard<'_, Ledger> {
        self.ledger
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Write `entries` behind every change not written yet, then apply
    /// `then` to the ledger. When the write fails `then` does not run.
    /// Blocks on disk.
    pub(crate) fn write<R>(
        &self,
        entries: Vec<LedgerEntry>,
        then: impl FnOnce(&mut Ledger) -> R,
    ) -> io::Result<R> {
        let Some(store) = &self.store else {
            return Ok(then(&mut self.ledger()));
        };
        let _order = self
            .writes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (lazy, mut batch) = self.ledger().take_lazy();
        batch.extend(entries);
        if !batch.is_empty() {
            if let Err(error) = store.append(&batch) {
                self.ledger().return_lazy(lazy);
                return Err(error);
            }
        }
        Ok(then(&mut self.ledger()))
    }

    /// [`Self::write`] from async code, off the executor.
    pub(crate) async fn write_off_executor<R: Send + 'static>(
        self: &Arc<Self>,
        entries: Vec<LedgerEntry>,
        then: impl FnOnce(&mut Ledger) -> R + Send + 'static,
    ) -> io::Result<R> {
        if self.store.is_none() {
            return Ok(then(&mut self.ledger()));
        }
        let books = Arc::clone(self);
        tokio::task::spawn_blocking(move || books.write(entries, then))
            .await
            .unwrap_or_else(|error| Err(io::Error::other(error)))
    }

    /// Write every change not written yet. Blocks on disk.
    pub(crate) fn flush(&self) -> io::Result<()> {
        self.write(Vec::new(), |_| ())
    }

    /// Have the changes not written yet on disk within [`WRITE_BEHIND`].
    /// Outside a Tokio runtime they wait for the next write or flush.
    pub(crate) fn schedule_flush(self: &Arc<Self>) {
        if self.store.is_none() || !self.ledger().has_lazy() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        if self.flush_scheduled.swap(true, Ordering::SeqCst) {
            return;
        }
        // Weak: a seller that is dropped meanwhile flushes on its own.
        let books = Arc::downgrade(self);
        runtime.spawn(async move {
            tokio::time::sleep(WRITE_BEHIND).await;
            let Some(books) = books.upgrade() else { return };
            books.flush_scheduled.store(false, Ordering::SeqCst);
            // A failed flush keeps its changes for the next write.
            let _ = tokio::task::spawn_blocking(move || books.flush()).await;
        });
    }
}

impl Drop for Books {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}
