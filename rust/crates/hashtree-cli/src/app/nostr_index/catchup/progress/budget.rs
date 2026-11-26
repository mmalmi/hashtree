use std::io::Write;
use std::sync::Mutex;

const LIMIT: usize = 16 * 1024 * 1024;
const EXHAUSTED: &[u8] = b"Nostr catchup phase: budget_exhausted\n";

/// Shared by every author in one native invocation, including failed writes.
pub struct TraceBudget {
    remaining: Mutex<Option<usize>>,
}

impl Default for TraceBudget {
    fn default() -> Self {
        Self::with_limit(LIMIT)
    }
}

impl TraceBudget {
    pub(super) fn with_limit(limit: usize) -> Self {
        Self {
            remaining: Mutex::new(Some(limit - EXHAUSTED.len())),
        }
    }

    pub(super) fn write(&self, line: impl FnOnce() -> String, sink: &mut impl Write) {
        let Ok(mut remaining) = self.remaining.lock() else {
            return;
        };
        let Some(left) = remaining.as_mut() else {
            return;
        };
        let line = line();
        if line.len() <= *left {
            // Charge before attempting I/O: a broken sink cannot renew the budget.
            *left -= line.len();
            let _ = sink.write_all(line.as_bytes());
        } else {
            // Space for this whole marker was reserved at construction. Mark
            // exhausted before the attempt, and never retry a failed marker.
            *remaining = None;
            let _ = sink.write_all(EXHAUSTED);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::sync::Arc;

    #[test]
    fn whole_lines_fit_exactly_and_marker_is_attempted_once() {
        let budget = TraceBudget::with_limit(EXHAUSTED.len() + 8);
        let mut output = Vec::new();
        for _ in 0..4 {
            budget.write(|| "row\n".into(), &mut output);
        }
        assert_eq!(output, [b"row\nrow\n".as_slice(), EXHAUSTED].concat());
        budget.write(
            || panic!("exhausted tracing must not format rows"),
            &mut output,
        );
    }

    #[test]
    fn oversized_row_is_not_partially_emitted() {
        let budget = TraceBudget::with_limit(EXHAUSTED.len() + 3);
        let mut output = Vec::new();
        budget.write(|| "row\n".into(), &mut output);
        assert_eq!(output, EXHAUSTED);
    }

    #[test]
    fn one_budget_is_shared_across_concurrent_writers() {
        let limit = EXHAUSTED.len() + 400;
        let budget = Arc::new(TraceBudget::with_limit(limit));
        let threads = (0..8)
            .map(|_| {
                let budget = budget.clone();
                std::thread::spawn(move || {
                    let mut output = Vec::new();
                    for _ in 0..100 {
                        budget.write(|| "row\n".into(), &mut output);
                    }
                    output
                })
            })
            .collect::<Vec<_>>();
        let output = threads
            .into_iter()
            .flat_map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(output.len(), limit);
        assert_eq!(
            output
                .windows(EXHAUSTED.len())
                .filter(|row| *row == EXHAUSTED)
                .count(),
            1
        );
    }

    #[derive(Default)]
    struct FailedSink {
        attempted: usize,
        markers: usize,
    }

    impl Write for FailedSink {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.attempted += bytes.len();
            self.markers += usize::from(bytes == EXHAUSTED);
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "closed diagnostic sink",
            ))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn failed_writes_consume_the_fixed_invocation_budget() {
        let budget = TraceBudget::default();
        let mut sink = FailedSink::default();
        for _ in 0..LIMIT / 1024 + 10 {
            budget.write(|| "x".repeat(1023) + "\n", &mut sink);
        }
        assert_eq!(sink.markers, 1);
        assert!(sink.attempted <= LIMIT);
        assert!(sink.attempted > LIMIT - 1024);
        budget.write(|| panic!("failed marker must not be retried"), &mut sink);
    }
}
