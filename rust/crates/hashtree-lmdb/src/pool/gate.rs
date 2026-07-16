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
        Ok(ConcurrencyPermit { gate: self })
    }
}

pub(super) struct ConcurrencyPermit<'a> {
    gate: &'a ConcurrencyGate,
}

impl Drop for ConcurrencyPermit<'_> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.gate.state.lock() {
            state.in_flight = state.in_flight.saturating_sub(1);
            self.gate.available.notify_one();
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
}
