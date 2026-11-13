//! A ledger store in one append-only file.
//!
//! `ledger.log` is a magic line followed by records: the payload length as a
//! 32-bit big-endian integer, the payload, and the first eight bytes of the
//! payload's SHA-256. Every append is one record, synced before it returns.
//! A record cut short by a crash is dropped when the file is opened again.
//! Once the log has grown well past its state it is rewritten beside the old
//! file and renamed over it, so there is always exactly one complete log.

mod codec;

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use hashtree_core::sha256;

use crate::store::{LedgerEntry, LedgerState, LedgerStore};

const MAGIC: &[u8; 8] = b"htpaid1\n";
const LOG: &str = "ledger.log";
const REWRITE: &str = "ledger.log.tmp";
const LOCK: &str = "lock";
const PREFIX_BYTES: usize = 4;
const CHECKSUM_BYTES: usize = 8;
const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;
/// Entries per record when the log is rewritten.
const REWRITE_CHUNK: usize = 512;
const REWRITE_MIN_BYTES: u64 = 64 * 1024;
/// Rewrite once the log is this many times larger than it was after the
/// last rewrite.
const REWRITE_GROWTH: u64 = 4;

fn invalid(reason: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}

fn checksum(payload: &[u8]) -> [u8; CHECKSUM_BYTES] {
    let mut sum = [0; CHECKSUM_BYTES];
    sum.copy_from_slice(&sha256(payload)[..CHECKSUM_BYTES]);
    sum
}

fn record(entries: &[LedgerEntry]) -> io::Result<Vec<u8>> {
    let payload = codec::encode(entries);
    let length = u32::try_from(payload.len())
        .ok()
        .filter(|length| *length as usize <= MAX_RECORD_BYTES)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "ledger record is too large"))?;
    let mut record = Vec::with_capacity(PREFIX_BYTES + payload.len() + CHECKSUM_BYTES);
    record.extend_from_slice(&length.to_be_bytes());
    record.extend_from_slice(&payload);
    record.extend_from_slice(&checksum(&payload));
    Ok(record)
}

/// Apply every complete record and return where the valid log ends.
fn replay(bytes: &[u8], state: &mut LedgerState) -> io::Result<usize> {
    if !bytes.starts_with(MAGIC) {
        return Err(invalid("not a paid-retrieval ledger"));
    }
    let mut at = MAGIC.len();
    while at < bytes.len() {
        let rest = &bytes[at..];
        let Some(prefix) = rest.first_chunk::<PREFIX_BYTES>() else {
            break;
        };
        let length = u32::from_be_bytes(*prefix) as usize;
        let end = PREFIX_BYTES + length + CHECKSUM_BYTES;
        if length > MAX_RECORD_BYTES || end > rest.len() {
            break;
        }
        let (payload, sum) = rest[PREFIX_BYTES..end].split_at(length);
        if checksum(payload) != sum {
            // Only the last record can be a write that a crash cut short.
            if end == rest.len() {
                break;
            }
            return Err(invalid("a ledger record is corrupt"));
        }
        let entries = codec::decode(payload)
            .ok_or_else(|| invalid("a ledger record is not understood by this version"))?;
        entries.iter().for_each(|entry| state.apply(entry));
        at += end;
    }
    Ok(at)
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

struct Journal {
    dir: PathBuf,
    file: File,
    len: u64,
    rewritten_len: u64,
    state: LedgerState,
    broken: bool,
}

impl Journal {
    fn append(&mut self, entries: &[LedgerEntry]) -> io::Result<()> {
        if self.broken {
            return Err(io::Error::other(
                "the ledger file could not be repaired after a failed write",
            ));
        }
        let record = record(entries)?;
        let written = self.file.write_all(&record);
        if let Err(error) = written.and_then(|()| self.file.sync_data()) {
            // A partial record must not stay: later records would follow it
            // and be unreadable.
            let rewound = self
                .file
                .set_len(self.len)
                .and_then(|()| self.file.seek(SeekFrom::Start(self.len)))
                .and_then(|_| self.file.sync_data());
            self.broken = rewound.is_err();
            return Err(error);
        }
        self.len += record.len() as u64;
        entries.iter().for_each(|entry| self.state.apply(entry));
        let grown = self.rewritten_len.saturating_mul(REWRITE_GROWTH);
        if self.len >= REWRITE_MIN_BYTES && self.len >= grown {
            // The entries are already durable; a failed rewrite keeps the
            // old log and is tried again later.
            let _ = self.rewrite();
        }
        Ok(())
    }

    /// Replace the log with the shortest one that restores the same state.
    fn rewrite(&mut self) -> io::Result<()> {
        let path = self.dir.join(REWRITE);
        let mut bytes = MAGIC.to_vec();
        for chunk in self.state.entries().chunks(REWRITE_CHUNK) {
            bytes.extend_from_slice(&record(chunk)?);
        }
        let written = (|| {
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&path, self.dir.join(LOG))?;
            Ok(file)
        })();
        match written {
            Ok(file) => {
                // The new file is the log from the rename on, whatever
                // happens next.
                self.file = file;
                self.len = bytes.len() as u64;
                self.rewritten_len = self.len;
                sync_dir(&self.dir)
            }
            Err(error) => {
                let _ = fs::remove_file(&path);
                Err(error)
            }
        }
    }
}

/// The provider ledger in a directory of its own.
pub struct FileLedgerStore {
    journal: Mutex<Journal>,
    /// Held for as long as the store lives, so no second process appends.
    _lock: File,
}

impl FileLedgerStore {
    /// Open the ledger in `dir`, creating both when they do not exist.
    pub fn open(dir: impl AsRef<Path>) -> io::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join(LOCK))?;
        lock.try_lock().map_err(|error| match error {
            TryLockError::WouldBlock => io::Error::new(
                io::ErrorKind::WouldBlock,
                "the ledger is in use by another provider",
            ),
            TryLockError::Error(error) => error,
        })?;
        let _ = fs::remove_file(dir.join(REWRITE));

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join(LOG))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let mut state = LedgerState::default();
        // A crash while the file was created leaves at most part of the magic.
        let valid = if MAGIC.starts_with(&bytes) {
            file.set_len(0)?;
            file.seek(SeekFrom::Start(0))?;
            file.write_all(MAGIC)?;
            file.sync_all()?;
            sync_dir(&dir)?;
            MAGIC.len()
        } else {
            let valid = replay(&bytes, &mut state)?;
            if valid < bytes.len() {
                file.set_len(valid as u64)?;
                file.sync_all()?;
            }
            file.seek(SeekFrom::Start(valid as u64))?;
            valid
        };
        let mut journal = Journal {
            dir,
            file,
            len: valid as u64,
            rewritten_len: valid as u64,
            state,
            broken: false,
        };
        if journal.len >= REWRITE_MIN_BYTES {
            journal.rewrite()?;
        }
        Ok(Self {
            journal: Mutex::new(journal),
            _lock: lock,
        })
    }

    fn journal(&self) -> MutexGuard<'_, Journal> {
        self.journal
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Size of the log in bytes.
    pub fn log_bytes(&self) -> u64 {
        self.journal().len
    }
}

impl LedgerStore for FileLedgerStore {
    fn load(&self) -> io::Result<LedgerState> {
        Ok(self.journal().state.clone())
    }

    fn append(&self, entries: &[LedgerEntry]) -> io::Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        self.journal().append(entries)
    }
}
