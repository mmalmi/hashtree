use std::sync::OnceLock;

pub const DEFAULT_MAX_BLOCKING_THREADS: usize = 64;
const MIN_MAX_BLOCKING_THREADS: usize = 8;
const MAX_MAX_BLOCKING_THREADS: usize = 512;
const MAX_BLOCKING_THREADS_ENV: &str = "HTREE_MAX_BLOCKING_THREADS";

const _: () = {
    assert!(MIN_MAX_BLOCKING_THREADS >= 4);
    assert!(DEFAULT_MAX_BLOCKING_THREADS >= MIN_MAX_BLOCKING_THREADS);
    assert!(DEFAULT_MAX_BLOCKING_THREADS <= MAX_MAX_BLOCKING_THREADS);
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeCapacity {
    pub max_blocking_threads: usize,
    pub cpu_parallelism: usize,
}

impl RuntimeCapacity {
    fn from_process() -> Self {
        let requested = std::env::var(MAX_BLOCKING_THREADS_ENV)
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_MAX_BLOCKING_THREADS);
        let max_blocking_threads =
            requested.clamp(MIN_MAX_BLOCKING_THREADS, MAX_MAX_BLOCKING_THREADS);
        if requested != max_blocking_threads {
            eprintln!(
                "{MAX_BLOCKING_THREADS_ENV}={requested} is outside the safe range; \
                 using {max_blocking_threads}"
            );
        }

        Self {
            max_blocking_threads,
            cpu_parallelism: std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(4)
                .max(1),
        }
    }
}

pub fn runtime_capacity() -> &'static RuntimeCapacity {
    static CAPACITY: OnceLock<RuntimeCapacity> = OnceLock::new();
    CAPACITY.get_or_init(RuntimeCapacity::from_process)
}
