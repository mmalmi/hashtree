use hashtree_core::store::StoreError;
use std::sync::{Condvar, Mutex};

pub(super) struct ConcurrencyGate {
    state: Mutex<GateState>,
    available: Condvar,
}

struct GateState {
    limit: usize,
    in_flight: usize,
}

impl ConcurrencyGate {
    pub(super) fn new(limit: u32) -> Self {
        Self {
            state: Mutex::new(GateState {
                limit: limit as usize,
                in_flight: 0,
            }),
            available: Condvar::new(),
        }
    }

    pub(super) fn set_limit(&self, limit: u32) -> Result<(), StoreError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| StoreError::Other("pool concurrency gate poisoned".into()))?;
        state.limit = limit as usize;
        self.available.notify_all();
        Ok(())
    }

    pub(super) fn acquire(&self) -> Result<ConcurrencyPermit<'_>, StoreError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| StoreError::Other("pool concurrency gate poisoned".into()))?;
        while state.in_flight >= state.limit {
            state = self
                .available
                .wait(state)
                .map_err(|_| StoreError::Other("pool concurrency gate poisoned".into()))?;
        }
        state.in_flight += 1;
        Ok(ConcurrencyPermit {
            gate: self,
            slots: 1,
        })
    }

    /// Prevent every ordinary operation using this gate until the returned
    /// permit is dropped.
    pub(super) fn acquire_exclusive(&self) -> Result<ConcurrencyPermit<'_>, StoreError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| StoreError::Other("pool concurrency gate poisoned".into()))?;
        while state.in_flight != 0 {
            state = self
                .available
                .wait(state)
                .map_err(|_| StoreError::Other("pool concurrency gate poisoned".into()))?;
        }
        let slots = state.limit.max(1);
        state.in_flight = slots;
        Ok(ConcurrencyPermit { gate: self, slots })
    }

    pub(super) fn load_percent(&self) -> Result<u8, StoreError> {
        let state = self
            .state
            .lock()
            .map_err(|_| StoreError::Other("pool concurrency gate poisoned".into()))?;
        if state.limit == 0 {
            return Ok(100);
        }
        Ok(state
            .in_flight
            .saturating_mul(100)
            .saturating_div(state.limit)
            .min(100) as u8)
    }
}

pub(super) struct ConcurrencyPermit<'a> {
    gate: &'a ConcurrencyGate,
    slots: usize,
}

impl Drop for ConcurrencyPermit<'_> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.gate.state.lock() {
            state.in_flight = state.in_flight.saturating_sub(self.slots);
            self.gate.available.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn gate_never_exceeds_configured_concurrency() {
        let gate = Arc::new(ConcurrencyGate::new(2));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let workers = (0..8)
            .map(|_| {
                let gate = Arc::clone(&gate);
                let in_flight = Arc::clone(&in_flight);
                let peak = Arc::clone(&peak);
                thread::spawn(move || {
                    let _permit = gate.acquire().expect("permit");
                    let active = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(active, Ordering::SeqCst);
                    thread::sleep(Duration::from_millis(5));
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                })
            })
            .collect::<Vec<_>>();
        for worker in workers {
            worker.join().expect("worker");
        }
        assert_eq!(peak.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn exclusive_permit_waits_for_and_blocks_ordinary_permits() {
        let gate = Arc::new(ConcurrencyGate::new(2));
        let ordinary = gate.acquire().expect("ordinary permit");
        let entered = Arc::new(AtomicUsize::new(0));
        let worker_gate = Arc::clone(&gate);
        let worker_entered = Arc::clone(&entered);
        let worker = thread::spawn(move || {
            let _exclusive = worker_gate.acquire_exclusive().expect("exclusive permit");
            worker_entered.store(1, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(10));
        });
        thread::sleep(Duration::from_millis(5));
        assert_eq!(entered.load(Ordering::SeqCst), 0);
        drop(ordinary);
        worker.join().expect("worker");
        assert_eq!(entered.load(Ordering::SeqCst), 1);
        let _ordinary = gate.acquire().expect("gate reopens after exclusive permit");
    }
}
