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

pub(super) struct TemperatureLeaseHeartbeat {
    signal: Arc<TemperatureWorkerSignal>,
    handle: Option<JoinHandle<()>>,
}

impl TemperatureLeaseHeartbeat {
    pub(super) fn start(
        weak: Weak<PoolStoreInner>,
        lease_duration: Duration,
    ) -> Result<Self, StoreError> {
        let interval = (lease_duration / 3).max(Duration::from_millis(100));
        let signal = Arc::new(TemperatureWorkerSignal::default());
        let thread_signal = Arc::clone(&signal);
        let handle = thread::Builder::new()
            .name("hashtree-pool-temperature-lease".into())
            .spawn(move || run_lease_heartbeat(weak, thread_signal, interval))
            .map_err(StoreError::Io)?;
        Ok(Self {
            signal,
            handle: Some(handle),
        })
    }
}

impl Drop for TemperatureLeaseHeartbeat {
    fn drop(&mut self) {
        if let Ok(mut stopped) = self.signal.stopped.lock() {
            *stopped = true;
            self.signal.wake.notify_all();
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
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

    pub(super) fn stop(&self) -> Result<(), StoreError> {
        {
            let mut stopped =
                self.signal.stopped.lock().map_err(|_| {
                    StoreError::Other("pool temperature signal lock poisoned".into())
                })?;
            *stopped = true;
            self.signal.wake.notify_all();
        }
        let handle = self
            .handle
            .lock()
            .map_err(|_| StoreError::Other("pool temperature worker lock poisoned".into()))?
            .take();
        if let Some(handle) = handle {
            if handle.thread().id() != thread::current().id() {
                handle.join().map_err(|_| {
                    StoreError::Other("pool temperature worker panicked while stopping".into())
                })?;
            }
        }
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

fn run_lease_heartbeat(
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
        if !matches!(
            pool.renew_temperature_lease(super::unix_timestamp_now()),
            Ok(true)
        ) {
            break;
        }
    }
}
