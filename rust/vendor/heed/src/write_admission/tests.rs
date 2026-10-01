use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use super::WriteAdmission;
use crate::types::Str;
use crate::{Database, Env, EnvFlags, EnvOpenOptions, Error};

fn is_data_file(fd: i32, path: &Path) -> bool {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    assert_eq!(unsafe { libc::fstat(fd, stat.as_mut_ptr()) }, 0);
    let stat = unsafe { stat.assume_init() };
    fs::metadata(path.join("data.mdb")).is_ok_and(|metadata| {
        metadata.dev() == stat.st_dev as u64 && metadata.ino() == stat.st_ino as u64
    })
}

fn assert_errno<T: std::fmt::Debug>(result: crate::Result<T>, expected: i32) {
    match result {
        Err(Error::Io(error)) => assert_eq!(error.raw_os_error(), Some(expected)),
        other => panic!("expected errno {expected}, got {other:?}"),
    }
}

fn options(admission: WriteAdmission) -> EnvOpenOptions {
    let mut options = EnvOpenOptions::new();
    options
        .map_size(16 * 1024 * 1024)
        .write_admission(admission);
    options
}

fn seed(env: &Env) -> Database<Str, Str> {
    let mut txn = env.write_txn().unwrap();
    let db = env.create_database::<Str, Str>(&mut txn, None).unwrap();
    db.put(&mut txn, "committed", "before").unwrap();
    txn.commit().unwrap();
    db
}

fn assert_old_root(env: &Env, db: Database<Str, Str>) {
    let txn = env.read_txn().unwrap();
    assert_eq!(db.get(&txn, "committed").unwrap(), Some("before"));
    assert_eq!(db.get(&txn, "uncommitted").unwrap(), None);
}

#[test]
fn admission_precedes_initial_lock_file_growth() {
    let dir = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let options = options(WriteAdmission::new(move |_, offset, length| {
        observed.fetch_add(1, Ordering::SeqCst);
        assert_eq!(offset, 0);
        assert!(length > 0);
        Err(io::Error::from_raw_os_error(libc::ENOSPC))
    }));
    assert_errno(unsafe { options.open(dir.path()) }, libc::ENOSPC);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(fs::metadata(dir.path().join("lock.mdb")).unwrap().len(), 0);
    assert!(fs::metadata(dir.path().join("data.mdb")).map_or(true, |meta| meta.len() == 0));
}

#[test]
fn admission_precedes_initial_metadata_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    let data_calls = Arc::new(AtomicUsize::new(0));
    let observed = data_calls.clone();
    let options = options(WriteAdmission::new(move |fd, offset, length| {
        if is_data_file(fd, &path) {
            observed.fetch_add(1, Ordering::SeqCst);
            assert_eq!(offset, 0);
            assert_eq!(length, 2 * page_size::get());
            return Err(io::Error::from_raw_os_error(libc::ENOSPC));
        }
        Ok(())
    }));
    assert_errno(unsafe { options.open(dir.path()) }, libc::ENOSPC);
    assert_eq!(data_calls.load(Ordering::SeqCst), 1);
    assert_eq!(fs::metadata(dir.path().join("data.mdb")).unwrap().len(), 0);
}

fn refused_commit_preserves_root(mode: usize, errno: i32) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    let refusal = Arc::new(AtomicUsize::new(0));
    let observed = refusal.clone();
    let page_size = page_size::get() as u64;
    let options = options(WriteAdmission::new(move |fd, offset, _| {
        if !is_data_file(fd, &path) {
            return Ok(());
        }
        match observed.load(Ordering::SeqCst) {
            1 if offset >= 2 * page_size => Err(io::Error::from_raw_os_error(libc::ENOSPC)),
            2 if offset < 2 * page_size => Err(io::Error::from_raw_os_error(libc::ENOSPC)),
            3 => panic!("callback panic must stay on the Rust side of FFI"),
            _ => Ok(()),
        }
    }));
    let env = unsafe { options.open(dir.path()).unwrap() };
    let db = seed(&env);
    let mut txn = env.write_txn().unwrap();
    db.put(&mut txn, "uncommitted", "after").unwrap();
    refusal.store(mode, Ordering::SeqCst);
    assert_errno(txn.commit(), errno);
    assert_old_root(&env, db);
    env.prepare_for_closing().wait();
    refusal.store(0, Ordering::SeqCst);
    let env = unsafe { options.open(dir.path()).unwrap() };
    let txn = env.read_txn().unwrap();
    let db = env.open_database::<Str, Str>(&txn, None).unwrap().unwrap();
    drop(txn);
    assert_old_root(&env, db);
    let mut txn = env.write_txn().unwrap();
    db.put(&mut txn, "uncommitted", "retry").unwrap();
    txn.commit().unwrap();
    let txn = env.read_txn().unwrap();
    assert_eq!(db.get(&txn, "uncommitted").unwrap(), Some("retry"));
}

#[test]
fn refused_page_flush_preserves_reopened_committed_root() {
    refused_commit_preserves_root(1, libc::ENOSPC);
}

#[test]
fn refused_commit_metadata_preserves_reopened_committed_root() {
    refused_commit_preserves_root(2, libc::ENOSPC);
}

#[test]
fn callback_panic_becomes_io_error_and_preserves_root() {
    refused_commit_preserves_root(3, libc::EIO);
}

#[test]
fn admitted_writes_observe_initial_pages_flushes_and_reserved_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    let writes = Arc::new(Mutex::new(Vec::new()));
    let observed = writes.clone();
    let options = options(WriteAdmission::new(move |fd, offset, length| {
        if is_data_file(fd, &path) {
            observed.lock().unwrap().push((offset, length));
        }
        Ok(())
    }));
    let env = unsafe { options.open(dir.path()).unwrap() };
    let db = seed(&env);
    let mut txn = env.write_txn().unwrap();
    db.put(&mut txn, "large", &"v".repeat(32 * 1024)).unwrap();
    txn.commit().unwrap();
    let page_size = page_size::get();
    let writes = writes.lock().unwrap();
    assert_eq!(writes[0], (0, 2 * page_size));
    assert!(writes.iter().any(|&(offset, length)| {
        offset >= (2 * page_size) as u64 && length >= 32 * 1024 && length % page_size == 0
    }));
    // Commit admission reserves the small meta write and one possible restore.
    assert!(writes.iter().any(|&(offset, length)| {
        offset > 0 && offset < (2 * page_size) as u64 && length < page_size && length % 2 == 0
    }));
}

#[test]
fn callback_lifetime_and_identity_follow_the_open_environment() {
    let dir = tempfile::tempdir().unwrap();
    let retained = Arc::new(());
    let weak = Arc::downgrade(&retained);
    let admission = WriteAdmission::new(move |_, _, _| {
        let _retained = &retained;
        Ok(())
    });
    let options = options(admission);
    let env = unsafe { options.open(dir.path()).unwrap() };
    let reopened = unsafe { options.clone().open(dir.path()).unwrap() };
    drop(reopened);
    let mut unguarded = EnvOpenOptions::new();
    unguarded.map_size(16 * 1024 * 1024);
    assert!(matches!(
        unsafe { unguarded.open(dir.path()) },
        Err(Error::BadOpenOptions { .. })
    ));
    let different = super::tests::options(WriteAdmission::new(|_, _, _| Ok(())));
    assert!(matches!(
        unsafe { different.open(dir.path()) },
        Err(Error::BadOpenOptions { .. })
    ));
    drop(options);
    assert!(weak.upgrade().is_some());
    seed(&env);
    env.prepare_for_closing().wait();
    assert!(weak.upgrade().is_none());
}

#[test]
fn guarded_writemap_is_rejected_before_any_physical_write() {
    let dir = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let mut options = options(WriteAdmission::new(move |_, _, _| {
        observed.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }));
    unsafe {
        options.flags(EnvFlags::WRITE_MAP);
    }
    assert_errno(unsafe { options.open(dir.path()) }, libc::EINVAL);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
}
