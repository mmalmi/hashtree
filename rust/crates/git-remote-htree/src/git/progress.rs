use std::sync::{
    atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
    Arc,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RepoTreeBuildPhase {
    Starting = 0,
    Objects = 1,
    Refs = 2,
    Info = 3,
    Head = 4,
    Index = 5,
    GitDir = 6,
    WorkingTree = 7,
    Root = 8,
    Done = 9,
}

impl RepoTreeBuildPhase {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Objects,
            2 => Self::Refs,
            3 => Self::Info,
            4 => Self::Head,
            5 => Self::Index,
            6 => Self::GitDir,
            7 => Self::WorkingTree,
            8 => Self::Root,
            9 => Self::Done,
            _ => Self::Starting,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Objects => "objects",
            Self::Refs => "refs",
            Self::Info => "info",
            Self::Head => "HEAD",
            Self::Index => "index",
            Self::GitDir => ".git",
            Self::WorkingTree => "working tree",
            Self::Root => "root",
            Self::Done => "done",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoTreeBuildProgressSnapshot {
    pub phase: RepoTreeBuildPhase,
    pub processed: usize,
    pub total: Option<usize>,
    pub object_blobs: usize,
    pub object_prefixes: usize,
    pub files: usize,
    pub dirs: usize,
    pub reused: usize,
}

impl RepoTreeBuildProgressSnapshot {
    pub fn format_for_label(&self, label: &str) -> String {
        let mut text = format!("  {}: {}", label, self.phase.label());

        if self.phase != RepoTreeBuildPhase::Done {
            match self.total {
                Some(total) => {
                    text.push_str(&format!(
                        " {}/{}",
                        self.processed,
                        total.max(self.processed)
                    ));
                }
                None if self.processed > 0 => {
                    text.push_str(&format!(" {}", self.processed));
                }
                None => {}
            }
        }

        match self.phase {
            RepoTreeBuildPhase::Objects => {
                text.push_str(&format!(
                    " ({} object blobs, {} prefixes)",
                    self.object_blobs, self.object_prefixes
                ));
            }
            RepoTreeBuildPhase::Index => {
                text.push_str(" entries");
            }
            RepoTreeBuildPhase::WorkingTree => {
                text.push_str(&format!(
                    " ({} files, {} dirs, {} reused)",
                    self.files, self.dirs, self.reused
                ));
            }
            RepoTreeBuildPhase::Done => {
                text.push_str(&format!(
                    " ({} object blobs, {} files, {} dirs, {} reused)",
                    self.object_blobs, self.files, self.dirs, self.reused
                ));
            }
            _ => {}
        }

        text
    }
}

#[derive(Debug)]
struct RepoTreeBuildProgressState {
    phase: AtomicU8,
    processed: AtomicUsize,
    total: AtomicUsize,
    total_known: AtomicBool,
    object_blobs: AtomicUsize,
    object_prefixes: AtomicUsize,
    files: AtomicUsize,
    dirs: AtomicUsize,
    reused: AtomicUsize,
}

#[derive(Debug, Clone)]
pub struct RepoTreeBuildProgress {
    state: Arc<RepoTreeBuildProgressState>,
}

impl RepoTreeBuildProgress {
    pub fn new() -> Self {
        Self {
            state: Arc::new(RepoTreeBuildProgressState {
                phase: AtomicU8::new(RepoTreeBuildPhase::Starting as u8),
                processed: AtomicUsize::new(0),
                total: AtomicUsize::new(0),
                total_known: AtomicBool::new(false),
                object_blobs: AtomicUsize::new(0),
                object_prefixes: AtomicUsize::new(0),
                files: AtomicUsize::new(0),
                dirs: AtomicUsize::new(0),
                reused: AtomicUsize::new(0),
            }),
        }
    }

    pub(crate) fn start_phase(&self, phase: RepoTreeBuildPhase, total: Option<usize>) {
        match phase {
            RepoTreeBuildPhase::Objects => {
                self.state.object_blobs.store(0, Ordering::Relaxed);
                self.state.object_prefixes.store(0, Ordering::Relaxed);
            }
            RepoTreeBuildPhase::WorkingTree => {
                self.state.files.store(0, Ordering::Relaxed);
                self.state.dirs.store(0, Ordering::Relaxed);
                self.state.reused.store(0, Ordering::Relaxed);
            }
            _ => {}
        }

        self.state.processed.store(0, Ordering::Relaxed);
        match total {
            Some(total) => {
                self.state.total.store(total, Ordering::Relaxed);
                self.state.total_known.store(true, Ordering::Relaxed);
            }
            None => {
                self.state.total.store(0, Ordering::Relaxed);
                self.state.total_known.store(false, Ordering::Relaxed);
            }
        }
        self.state.phase.store(phase as u8, Ordering::Relaxed);
    }

    pub(crate) fn increment_processed(&self) {
        self.state.processed.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_object_blob(&self) {
        self.state.object_blobs.fetch_add(1, Ordering::Relaxed);
        self.increment_processed();
    }

    pub(crate) fn record_object_prefix(&self) {
        self.state.object_prefixes.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_index_entry(&self) {
        self.increment_processed();
    }

    pub(crate) fn record_working_file(&self, reused: bool) {
        self.state.files.fetch_add(1, Ordering::Relaxed);
        if reused {
            self.state.reused.fetch_add(1, Ordering::Relaxed);
        }
        self.increment_processed();
    }

    pub(crate) fn record_working_dir(&self, reused: bool) {
        self.state.dirs.fetch_add(1, Ordering::Relaxed);
        if reused {
            self.state.reused.fetch_add(1, Ordering::Relaxed);
        }
        self.increment_processed();
    }

    pub(crate) fn mark_done(&self) {
        self.state
            .phase
            .store(RepoTreeBuildPhase::Done as u8, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> RepoTreeBuildProgressSnapshot {
        RepoTreeBuildProgressSnapshot {
            phase: RepoTreeBuildPhase::from_u8(self.state.phase.load(Ordering::Relaxed)),
            processed: self.state.processed.load(Ordering::Relaxed),
            total: self
                .state
                .total_known
                .load(Ordering::Relaxed)
                .then(|| self.state.total.load(Ordering::Relaxed)),
            object_blobs: self.state.object_blobs.load(Ordering::Relaxed),
            object_prefixes: self.state.object_prefixes.load(Ordering::Relaxed),
            files: self.state.files.load(Ordering::Relaxed),
            dirs: self.state.dirs.load(Ordering::Relaxed),
            reused: self.state.reused.load(Ordering::Relaxed),
        }
    }
}

impl Default for RepoTreeBuildProgress {
    fn default() -> Self {
        Self::new()
    }
}
