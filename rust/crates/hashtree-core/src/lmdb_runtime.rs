//! Process-wide coordination for LMDB's Darwin lock backend.
//!
//! LMDB uses System V semaphores with `SEM_UNDO` on Darwin. The default
//! `kern.sysv.semume` limit permits ten distinct undo entries per process, so
//! unrelated LMDB environments can otherwise fail a transaction with `EINVAL`
//! when enough writers overlap. This gate bounds only lock-owning write
//! transactions and leaves room for LMDB users outside Hashtree's wrappers.

#[cfg(target_os = "macos")]
use std::sync::{Condvar, Mutex, OnceLock};

#[cfg(target_os = "macos")]
const MAX_MANAGED_WRITERS: usize = 4;

#[cfg(target_os = "macos")]
#[derive(Default)]
struct WriterState {
    active: usize,
}

#[cfg(target_os = "macos")]
fn writer_gate() -> &'static (Mutex<WriterState>, Condvar) {
    static GATE: OnceLock<(Mutex<WriterState>, Condvar)> = OnceLock::new();
    GATE.get_or_init(|| (Mutex::new(WriterState::default()), Condvar::new()))
}

#[cfg(target_os = "macos")]
fn acquisition_gate() -> &'static Mutex<()> {
    static GATE: OnceLock<Mutex<()>> = OnceLock::new();
    GATE.get_or_init(|| Mutex::new(()))
}

/// Holds one slot for a lock-owning LMDB write transaction on Darwin.
///
/// This is a no-op on platforms whose LMDB lock backend does not have the
/// Darwin `SEM_UNDO` limit.
#[doc(hidden)]
pub struct LmdbWritePermit {
    #[cfg(target_os = "macos")]
    held: bool,
}

/// Acquire one process-wide LMDB writer slot.
#[doc(hidden)]
pub fn acquire_lmdb_write_permit() -> LmdbWritePermit {
    #[cfg(target_os = "macos")]
    {
        let (lock, available) = writer_gate();
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        while state.active >= MAX_MANAGED_WRITERS {
            state = available
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        state.active += 1;
        LmdbWritePermit { held: true }
    }

    #[cfg(not(target_os = "macos"))]
    {
        LmdbWritePermit {}
    }
}

/// Serialize short LMDB lock acquisitions across Hashtree environments.
#[doc(hidden)]
pub fn with_lmdb_lock_acquisition<T>(operation: impl FnOnce() -> T) -> T {
    #[cfg(target_os = "macos")]
    {
        let _guard = acquisition_gate()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        operation()
    }

    #[cfg(not(target_os = "macos"))]
    {
        operation()
    }
}

impl Drop for LmdbWritePermit {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        if self.held {
            let (lock, available) = writer_gate();
            let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            state.active = state
                .active
                .checked_sub(1)
                .expect("LMDB writer permit count underflow");
            self.held = false;
            available.notify_one();
        }
    }
}
