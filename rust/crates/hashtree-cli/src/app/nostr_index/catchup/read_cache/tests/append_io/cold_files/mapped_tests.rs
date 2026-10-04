use super::*;
use std::io::Write;

struct Mapped {
    address: *mut libc::c_void,
    bytes: usize,
}

impl Mapped {
    fn open(file: &File, writable: bool) -> Self {
        let bytes = file.metadata().unwrap().len() as usize;
        let protection = libc::PROT_READ | if writable { libc::PROT_WRITE } else { 0 };
        let address = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                bytes,
                protection,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        assert_ne!(address, libc::MAP_FAILED);
        Self { address, bytes }
    }

    fn verify(&self, bytes: &[u8]) {
        // Mapping and backing file stay live and unmodified during this read.
        assert_eq!(
            unsafe { std::slice::from_raw_parts(self.address.cast::<u8>(), self.bytes) },
            bytes
        );
    }
}

impl Drop for Mapped {
    fn drop(&mut self) {
        assert_eq!(unsafe { libc::munmap(self.address, self.bytes) }, 0);
    }
}

fn fixture() -> (tempfile::TempDir, Vec<u8>, OwnedFiles) {
    let directory = tempfile::tempdir().unwrap();
    let bytes: Vec<u8> = (0..4 * 1024 * 1024).map(|n| (n % 251) as u8).collect();
    for name in ["catalog", "member"] {
        std::fs::create_dir(directory.path().join(name)).unwrap();
        File::create(directory.path().join(name).join("data.mdb"))
            .unwrap()
            .write_all(&bytes)
            .unwrap();
    }
    let files = OwnedFiles::open(&directory);
    (directory, bytes, files)
}

#[test]
fn reopened_mappings_are_cold_without_changing_bytes_or_unrelated_residency() {
    let (_directory, bytes, mut files) = fixture();
    files.prepare(true);
    let mappings: Vec<_> = files.0.iter().map(|f| Mapped::open(f, false)).collect();
    for mapping in &mappings {
        mapping.verify(&bytes);
    }
    // Reproduce the previous setup error: reads after eviction refill mapped pages.
    assert!(std::panic::catch_unwind(|| files.ready(true)).is_err());
    let mut unrelated = tempfile::tempfile().unwrap();
    unrelated.write_all(&bytes).unwrap();
    unrelated.sync_all().unwrap();
    let other = Mapped::open(&unrelated, false);
    other.verify(&bytes);
    // Sole test-owned mappings; no active users while discarding resident pages.
    let receipt = unsafe { files.reset_reopened() };
    for row in receipt["before_mapping_reset"].as_array().unwrap() {
        assert!(row["resident_pages"].as_u64().unwrap() > 0);
    }
    files.ready(true); // Zero pages, not the earlier 10% allowance.
    let untouched = residency(&unrelated);
    assert!(untouched.resident_pages * 100 >= untouched.pages * 95);
    let before = super::super::os_io::ProcessIo::sample().unwrap();
    for mapping in &mappings {
        mapping.verify(&bytes);
    }
    let io = super::super::os_io::ProcessIo::sample()
        .unwrap()
        .delta(before);
    assert!(
        io.read_bytes > 0,
        "resident reset must require storage reads"
    );
    other.verify(&bytes);
}

#[test]
fn writable_owned_mapping_is_rejected_before_reset() {
    let (_directory, bytes, mut files) = fixture();
    files.prepare(false);
    let mapping = Mapped::open(&files.0[0], true);
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            files.reset_reopened()
        }))
        .is_err()
    );
    mapping.verify(&bytes);
}
