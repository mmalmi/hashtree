use super::{PoolStore, PoolStoreInner};
use hashtree_core::store::StoreError;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[derive(Default)]
pub(super) struct TemperatureWorker {
    signal: Arc<TemperatureWorkerSignal>,
    handle: Mutex<Option<JoinHandle<()>>>,
}

#[derive(Default)]
struct TemperatureWorkerSignal {
    stopped: Mutex<bool>,
    wake: Condvar,
}

impl TemperatureWorker {
    pub(super) fn start(
        &self,
        weak: Weak<PoolStoreInner>,
        interval: Duration,
    ) -> Result<(), StoreError> {
        if interval.is_zero() {
            return Err(StoreError::Other(
                "pool temperature interval must be non-zero".into(),
            ));
        }
        let mut handle = self
            .handle
            .lock()
            .map_err(|_| StoreError::Other("pool temperature worker lock poisoned".into()))?;
        if handle.is_some() {
            return Ok(());
        }
        let signal = Arc::clone(&self.signal);
        *handle = Some(
            thread::Builder::new()
                .name("hashtree-pool-temperature".into())
                .spawn(move || run_worker(weak, signal, interval))
                .map_err(StoreError::Io)?,
        );
        Ok(())
    }
}

impl Drop for TemperatureWorker {
    fn drop(&mut self) {
        if let Ok(mut stopped) = self.signal.stopped.lock() {
            *stopped = true;
            self.signal.wake.notify_all();
        }
        if let Ok(handle) = self.handle.get_mut() {
            if let Some(handle) = handle.take() {
                if handle.thread().id() != thread::current().id() {
                    let _ = handle.join();
                }
            }
        }
    }
}

fn run_worker(
    weak: Weak<PoolStoreInner>,
    signal: Arc<TemperatureWorkerSignal>,
    interval: Duration,
) {
    loop {
        let stopped = match signal.stopped.lock() {
            Ok(stopped) => stopped,
            Err(_) => break,
        };
        if *stopped {
            break;
        }
        let Ok((stopped, timeout)) = signal.wake.wait_timeout(stopped, interval) else {
            break;
        };
        if *stopped {
            break;
        }
        drop(stopped);
        if !timeout.timed_out() {
            continue;
        }
        let Some(inner) = weak.upgrade() else {
            break;
        };
        let pool = PoolStore { inner };
        let _ = pool.balance_temperature();
    }
}
