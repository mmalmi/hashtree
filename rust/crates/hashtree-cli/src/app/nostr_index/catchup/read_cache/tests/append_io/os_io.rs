//! Test-only attribution. Thread counters bracket synchronous Pool calls; process
//! byte counters bracket non-overlapping projection intervals after workers join.
use serde::Serialize;

#[derive(Clone, Copy, Default, Debug, PartialEq, Serialize)]
pub(super) struct Work {
    pub minor_faults: u64,
    pub major_faults: u64,
    pub input_blocks: u64,
    pub output_blocks: u64,
}

impl Work {
    fn delta(self, before: Self) -> Self {
        Self {
            minor_faults: self.minor_faults.checked_sub(before.minor_faults).unwrap(),
            major_faults: self.major_faults.checked_sub(before.major_faults).unwrap(),
            input_blocks: self.input_blocks.checked_sub(before.input_blocks).unwrap(),
            output_blocks: self
                .output_blocks
                .checked_sub(before.output_blocks)
                .unwrap(),
        }
    }

    pub(super) fn accumulate(target: &mut Option<Self>, value: Option<Self>) {
        if let Some(value) = value {
            let total = target.get_or_insert_with(Self::default);
            total.minor_faults += value.minor_faults;
            total.major_faults += value.major_faults;
            total.input_blocks += value.input_blocks;
            total.output_blocks += value.output_blocks;
        }
    }
}

pub(super) struct ThreadStart(Option<(std::thread::ThreadId, Work)>);

impl ThreadStart {
    pub(super) fn start() -> Self {
        Self(thread_work().map(|work| (std::thread::current().id(), work)))
    }

    pub(super) fn finish(self) -> Option<Work> {
        self.0.map(|(thread, before)| {
            assert_eq!(
                thread,
                std::thread::current().id(),
                "store call migrated threads"
            );
            thread_work().unwrap().delta(before)
        })
    }
}

#[cfg(target_os = "linux")]
fn thread_work() -> Option<Work> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: getrusage initializes this correctly sized output on success.
    assert_eq!(
        unsafe { libc::getrusage(libc::RUSAGE_THREAD, usage.as_mut_ptr()) },
        0
    );
    let usage = unsafe { usage.assume_init() };
    Some(Work {
        minor_faults: usage.ru_minflt.try_into().unwrap(),
        major_faults: usage.ru_majflt.try_into().unwrap(),
        input_blocks: usage.ru_inblock.try_into().unwrap(),
        output_blocks: usage.ru_oublock.try_into().unwrap(),
    })
}

#[cfg(not(target_os = "linux"))]
fn thread_work() -> Option<Work> {
    None
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Serialize)]
pub(super) struct ProcessIo {
    pub read_bytes: u64,
    pub write_bytes: u64,
}

impl ProcessIo {
    pub(super) fn accumulate(target: &mut Option<Self>, value: Self) {
        let total = target.get_or_insert_with(Self::default);
        total.read_bytes += value.read_bytes;
        total.write_bytes += value.write_bytes;
    }

    fn parse(text: &str) -> Self {
        let value = |key| {
            text.lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    (name == key).then(|| value.trim().parse::<u64>().unwrap())
                })
                .expect("missing process I/O counter")
        };
        Self {
            read_bytes: value("read_bytes"),
            write_bytes: value("write_bytes"),
        }
    }

    pub(super) fn delta(self, before: Self) -> Self {
        Self {
            read_bytes: self.read_bytes.checked_sub(before.read_bytes).unwrap(),
            write_bytes: self.write_bytes.checked_sub(before.write_bytes).unwrap(),
        }
    }

    pub(super) fn sample() -> Option<Self> {
        #[cfg(target_os = "linux")]
        {
            Some(Self::parse(
                &std::fs::read_to_string("/proc/self/io").unwrap(),
            ))
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }
}

#[test]
fn process_counter_parser_uses_storage_bytes_not_syscall_bytes() {
    let before = ProcessIo::parse("rchar: 999999\nwchar: 888888\nread_bytes: 4096\nwrite_bytes: 8192\ncancelled_write_bytes: 16\n");
    let after = ProcessIo::parse("read_bytes: 12288\nwrite_bytes: 12288\n");
    assert_eq!(
        after.delta(before),
        ProcessIo {
            read_bytes: 8192,
            write_bytes: 4096
        }
    );
    let mut revisited_phase = None;
    ProcessIo::accumulate(&mut revisited_phase, after.delta(before));
    ProcessIo::accumulate(&mut revisited_phase, after.delta(before));
    assert_eq!(
        revisited_phase,
        Some(ProcessIo {
            read_bytes: 16384,
            write_bytes: 8192
        })
    );
}

#[test]
fn thread_counters_accumulate_reads_and_writes_without_cross_thread_subtraction() {
    let before = Work {
        minor_faults: 3,
        major_faults: 1,
        input_blocks: 8,
        output_blocks: 16,
    };
    let after = Work {
        minor_faults: 5,
        major_faults: 2,
        input_blocks: 24,
        output_blocks: 32,
    };
    let mut total = None;
    Work::accumulate(&mut total, None);
    assert_eq!(total, None);
    Work::accumulate(&mut total, Some(after.delta(before)));
    Work::accumulate(&mut total, Some(after.delta(before)));
    assert_eq!(
        total,
        Some(Work {
            minor_faults: 4,
            major_faults: 2,
            input_blocks: 32,
            output_blocks: 32
        })
    );
}
