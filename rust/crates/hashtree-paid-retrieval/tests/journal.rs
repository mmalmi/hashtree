//! The file ledger store: what it keeps across a reopen, a torn write and a
//! rewrite of its log.

use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};

use hashtree_paid_retrieval::{
    AccountRecord, FileLedgerStore, LedgerEntry, LedgerState, LedgerStore, PendingRedemption,
};

const LOG: &str = "ledger.log";

fn redemption(seed: u8) -> PendingRedemption {
    PendingRedemption {
        token: [seed; 32],
        account: [seed.wrapping_add(100); 32],
        payment: [seed; 16],
    }
}

fn credited(seed: u8, credited_msat: u64) -> LedgerEntry {
    LedgerEntry::Credited {
        redemption: redemption(seed),
        credited_msat,
    }
}

/// One entry of every kind, ending in a state with something of each.
fn every_entry() -> Vec<LedgerEntry> {
    let record = AccountRecord {
        paid_msat: u64::MAX,
        charged_msat: 7,
        payments: VecDeque::from([([3; 16], 5), ([4; 16], u64::MAX)]),
    };
    vec![
        LedgerEntry::Redeeming(redemption(1)),
        credited(1, 4_000),
        LedgerEntry::Redeeming(redemption(2)),
        LedgerEntry::Abandoned { token: [2; 32] },
        LedgerEntry::Redeeming(redemption(3)),
        LedgerEntry::Charged {
            account: redemption(1).account,
            charged_msat: 3_048,
        },
        LedgerEntry::Account {
            account: [9; 32],
            record,
        },
        LedgerEntry::Account {
            account: [8; 32],
            record: AccountRecord::default(),
        },
        LedgerEntry::Removed { account: [8; 32] },
        LedgerEntry::Token { token: [5; 32] },
        LedgerEntry::Grace {
            peer: "npub1peer".into(),
            account: [9; 32],
        },
    ]
}

fn state_of(entries: &[LedgerEntry]) -> LedgerState {
    let mut state = LedgerState::default();
    entries.iter().for_each(|entry| state.apply(entry));
    state
}

#[test]
fn every_entry_survives_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let entries = every_entry();
    let store = FileLedgerStore::open(dir.path()).unwrap();
    assert_eq!(store.load().unwrap(), LedgerState::default());
    // Several appends and one batch: both are replayed in order.
    store.append(&entries[..3]).unwrap();
    for entry in &entries[3..] {
        store.append(std::slice::from_ref(entry)).unwrap();
    }
    let expected = state_of(&entries);
    assert_eq!(store.load().unwrap(), expected);
    drop(store);

    let reopened = FileLedgerStore::open(dir.path()).unwrap();
    let state = reopened.load().unwrap();
    assert_eq!(state, expected);
    let account = &state.accounts[&redemption(1).account];
    assert_eq!((account.paid_msat, account.charged_msat), (4_000, 3_048));
    assert_eq!(account.payments, VecDeque::from([([1; 16], 4_000)]));
    assert_eq!(state.tokens, VecDeque::from([[1; 32], [5; 32]]));
    assert_eq!(state.redeeming, VecDeque::from([redemption(3)]));
    assert!(!state.accounts.contains_key(&[8; 32]));
    assert_eq!(state.grace["npub1peer"], [9; 32]);
}

#[test]
fn a_write_cut_short_by_a_crash_is_dropped_and_the_log_stays_usable() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileLedgerStore::open(dir.path()).unwrap();
    store.append(&[credited(1, 1_000)]).unwrap();
    let whole = store.log_bytes();
    store.append(&[credited(2, 2_000)]).unwrap();
    let both = store.log_bytes();
    drop(store);

    // Cut the second record anywhere: it is gone as a whole, the first stays.
    for cut in [whole + 1, whole + 4, both - 1] {
        let log = OpenOptions::new()
            .write(true)
            .open(dir.path().join(LOG))
            .unwrap();
        log.set_len(cut).unwrap();
        drop(log);
        let store = FileLedgerStore::open(dir.path()).unwrap();
        assert_eq!(store.load().unwrap(), state_of(&[credited(1, 1_000)]));
        assert_eq!(store.log_bytes(), whole);
        // A record appended after the repair is read back.
        store.append(&[credited(2, 2_000)]).unwrap();
        drop(store);
        let store = FileLedgerStore::open(dir.path()).unwrap();
        let expected = state_of(&[credited(1, 1_000), credited(2, 2_000)]);
        assert_eq!(store.load().unwrap(), expected);
    }
}

#[test]
fn damage_before_the_last_record_is_reported_not_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileLedgerStore::open(dir.path()).unwrap();
    store.append(&[credited(1, 1_000)]).unwrap();
    let first_end = store.log_bytes() as usize;
    store.append(&[credited(2, 2_000)]).unwrap();
    drop(store);

    let path = dir.path().join(LOG);
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[first_end - 10] ^= 0xff;
    std::fs::write(&path, &bytes).unwrap();
    let error = FileLedgerStore::open(dir.path()).err().unwrap();
    assert_eq!(error.kind(), ErrorKind::InvalidData);

    // Nor is a file of another kind taken for an empty ledger.
    let other = tempfile::tempdir().unwrap();
    let mut file = std::fs::File::create(other.path().join(LOG)).unwrap();
    file.write_all(b"something else entirely").unwrap();
    let error = FileLedgerStore::open(other.path()).err().unwrap();
    assert_eq!(error.kind(), ErrorKind::InvalidData);
}

#[test]
fn one_ledger_is_open_in_one_place_only() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileLedgerStore::open(dir.path()).unwrap();
    let error = FileLedgerStore::open(dir.path()).err().unwrap();
    assert_eq!(error.kind(), ErrorKind::WouldBlock);
    drop(store);
    FileLedgerStore::open(dir.path()).unwrap();
}

#[test]
fn the_log_is_rewritten_instead_of_growing_with_every_charge() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileLedgerStore::open(dir.path()).unwrap();
    let account = redemption(1).account;
    store.append(&[credited(1, 5_000_000)]).unwrap();
    let mut largest = 0;
    let charges: Vec<LedgerEntry> = (1..=20_000u64)
        .map(|charged_msat| LedgerEntry::Charged {
            account,
            charged_msat,
        })
        .collect();
    for batch in charges.chunks(50) {
        store.append(batch).unwrap();
        largest = largest.max(store.log_bytes());
    }
    // 20,000 charge records are over a megabyte when they are all kept.
    assert!(largest < 300 * 1024, "log grew to {largest} bytes");
    let expected = store.load().unwrap();
    assert_eq!(expected.accounts[&account].charged_msat, 20_000);
    drop(store);

    let reopened = FileLedgerStore::open(dir.path()).unwrap();
    assert_eq!(reopened.load().unwrap(), expected);
    assert!(!dir.path().join("ledger.log.tmp").exists());
}
