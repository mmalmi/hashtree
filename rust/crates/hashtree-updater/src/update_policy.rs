use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct UpdateAutoCheckPolicy {
    interval: Duration,
    startup_check_done: bool,
    last_check_started_at: Option<Instant>,
}

impl UpdateAutoCheckPolicy {
    #[must_use]
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            startup_check_done: false,
            last_check_started_at: None,
        }
    }

    #[must_use]
    pub fn should_start_check(&mut self, enabled: bool, now: Instant) -> bool {
        if !enabled {
            return false;
        }
        if !self.startup_check_done {
            self.startup_check_done = true;
            self.last_check_started_at = Some(now);
            return true;
        }
        if self
            .last_check_started_at
            .is_some_and(|last| now.duration_since(last) >= self.interval)
        {
            self.last_check_started_at = Some(now);
            return true;
        }
        false
    }

    pub fn note_manual_check_started(&mut self, now: Instant) {
        self.last_check_started_at = Some(now);
    }
}
