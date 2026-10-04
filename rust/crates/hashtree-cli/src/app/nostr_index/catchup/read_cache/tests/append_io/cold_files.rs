//! Linux-only cache preparation, restricted to two files in this test's TempDir.
//! An advisory syscall succeeding is insufficient: mincore must show eviction.
use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::{self, Seek, SeekFrom};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

mod mapped_tests;

#[derive(Debug, Serialize)]
pub(super) struct Residency {
    pub bytes: u64,
    pub pages: usize,
    pub resident_pages: usize,
}

pub(super) struct OwnedFiles(Vec<File>);

impl OwnedFiles {
    pub(super) fn open(directory: &tempfile::TempDir) -> Self {
        let files = ["catalog", "member"]
            .into_iter()
            .map(|name| {
                let parent = directory.path().join(name);
                assert!(parent.symlink_metadata().unwrap().file_type().is_dir());
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(parent.join("data.mdb"))
                    .unwrap();
                let metadata = file.metadata().unwrap();
                assert!(metadata.is_file() && metadata.nlink() == 1);
                assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
                assert!(metadata.len() > 0 && metadata.len() <= 2 * 1024 * 1024 * 1024);
                file
            })
            .collect();
        Self(files)
    }

    pub(super) fn prepare(&mut self, cold: bool) -> Vec<Residency> {
        for file in &mut self.0 {
            file.sync_all().unwrap();
            if cold {
                // No live LMDB handles remain. This affects only this file,
                // unlike the system-wide drop_caches interface.
                assert_eq!(
                    unsafe {
                        libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED)
                    },
                    0,
                    "file-scoped eviction failed"
                );
            } else {
                file.seek(SeekFrom::Start(0)).unwrap();
                io::copy(file, &mut io::sink()).unwrap();
            }
        }
        let rows = self.sample();
        for row in &rows {
            if cold {
                assert_eq!(
                    row.resident_pages, 0,
                    "cold preparation retained cached pages"
                );
            } else {
                assert!(
                    row.resident_pages * 100 >= row.pages * 95,
                    "warm control lost residency"
                );
            }
        }
        rows
    }

    pub(super) fn ready(&self, cold: bool) -> Vec<Residency> {
        let rows = self.sample();
        for row in &rows {
            if cold {
                assert_eq!(
                    row.resident_pages, 0,
                    "append boundary retained cached data: {row:?}"
                );
            } else {
                assert!(
                    row.resident_pages * 100 >= row.pages * 95,
                    "warm control is no longer warm"
                );
            }
        }
        rows
    }

    /// Caller must keep the disposable Pool quiescent for this entire reset:
    /// no transactions, workers, map resize/close or external users.
    pub(super) unsafe fn reset_reopened(&mut self) -> serde_json::Value {
        let before = self.sample();
        for file in &self.0 {
            file.sync_all().unwrap();
        }
        let discarded = self
            .0
            .iter()
            .map(|file| unsafe { super::cold_mappings::discard_resident_pages(file) })
            .collect::<Vec<_>>();
        let after = self.prepare(true);
        serde_json::json!({ "before_mapping_reset": before,
            "discarded_mapping_bytes": discarded, "after_mapping_reset": after })
    }

    pub(super) fn sample(&self) -> Vec<Residency> {
        self.0.iter().map(residency).collect()
    }
}

fn residency(file: &File) -> Residency {
    let length: usize = file.metadata().unwrap().len().try_into().unwrap();
    let page: usize = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }
        .try_into()
        .unwrap();
    assert!(page > 0);
    let pages = length.div_ceil(page);
    let mut resident = vec![0u8; pages];
    // PROT_NONE avoids touching any page. mincore observes file-cache residency
    // without faulting data in; this fresh mapping is always unmapped below.
    let mapping = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            length,
            libc::PROT_NONE,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        )
    };
    assert_ne!(mapping, libc::MAP_FAILED);
    let result = unsafe { libc::mincore(mapping, length, resident.as_mut_ptr()) };
    let error = io::Error::last_os_error();
    assert_eq!(unsafe { libc::munmap(mapping, length) }, 0);
    assert_eq!(result, 0, "mincore: {error}");
    Residency {
        bytes: length as u64,
        pages,
        resident_pages: resident.iter().filter(|p| **p & 1 != 0).count(),
    }
}

#[test]
fn disposable_files_can_be_warmed_evicted_and_measured_without_changing_bytes() {
    use std::io::{Read, Write};
    let directory = tempfile::tempdir().unwrap();
    let bytes = vec![0x5a; 4 * 1024 * 1024];
    let mut unrelated = tempfile::tempfile().unwrap();
    unrelated.write_all(&bytes).unwrap();
    unrelated.sync_all().unwrap();
    for name in ["catalog", "member"] {
        std::fs::create_dir(directory.path().join(name)).unwrap();
        File::create(directory.path().join(name).join("data.mdb"))
            .unwrap()
            .write_all(&bytes)
            .unwrap();
    }
    let mut files = OwnedFiles::open(&directory);
    files.prepare(false);
    files.ready(false);
    files.prepare(true);
    files.ready(true);
    let untouched = residency(&unrelated);
    assert!(
        untouched.resident_pages * 100 >= untouched.pages * 95,
        "file-scoped preparation evicted an unrelated file"
    );
    let before = super::os_io::ProcessIo::sample().unwrap();
    for file in &mut files.0 {
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut actual = Vec::new();
        file.read_to_end(&mut actual).unwrap();
        assert_eq!(actual, bytes);
    }
    let delta = super::os_io::ProcessIo::sample().unwrap().delta(before);
    assert!(
        delta.read_bytes > 0,
        "filesystem did not produce attributable storage reads"
    );
}

#[test]
fn cache_preparation_rejects_symlink_and_hardlink_targets() {
    for symlink in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        let other = directory.path().join("unrelated");
        std::fs::write(&other, b"outside the owned Pool").unwrap();
        let catalog = directory.path().join("catalog");
        std::fs::create_dir(&catalog).unwrap();
        let target = catalog.join("data.mdb");
        if symlink {
            std::os::unix::fs::symlink(&other, &target).unwrap();
        } else {
            std::fs::hard_link(&other, &target).unwrap();
        }
        assert!(std::panic::catch_unwind(|| OwnedFiles::open(&directory)).is_err());
        assert_eq!(std::fs::read(&other).unwrap(), b"outside the owned Pool");
    }
}
