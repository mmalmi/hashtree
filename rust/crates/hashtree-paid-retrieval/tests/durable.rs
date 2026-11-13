//! A provider with a ledger on disk: what a restart keeps, and what a crash
//! at each point of a payment leaves behind.

mod common;

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{CrashStore, FakeMint, FakeReceiver, Seller, MINT};
use hashtree_core::Hash;
use hashtree_paid_retrieval::{
    AccountId, ErrorCode, FileLedgerStore, LedgerEntry, LedgerStore, PaymentId, Reply, Request,
    METHOD_CASHU,
};

const ACCOUNT: AccountId = [7; 32];
const PAYMENT: PaymentId = [1; 16];
const TWO_MIB: usize = 2 * 1024 * 1024;

fn get(hash: Hash) -> Request {
    Request::Get {
        hash: hash.into(),
        account: ACCOUNT,
        max_msat: u64::MAX,
    }
}

fn pay_into(account: AccountId, payment: PaymentId, token: &str) -> Request {
    Request::Pay {
        account,
        payment,
        method: METHOD_CASHU.into(),
        mint: MINT.into(),
        token: token.into(),
    }
}

fn pay(token: &str) -> Request {
    pay_into(ACCOUNT, PAYMENT, token)
}

fn paid(credited_msat: u64, balance_msat: i64) -> Reply {
    Reply::Paid {
        payment: PAYMENT,
        credited_msat,
        balance_msat,
    }
}

fn error_code(reply: &Reply) -> ErrorCode {
    match reply {
        Reply::Error { code, .. } => *code,
        other => panic!("expected an error, got {other:?}"),
    }
}

fn disk(dir: &Path) -> Arc<dyn LedgerStore> {
    Arc::new(FileLedgerStore::open(dir).unwrap())
}

/// The same provider process started again on the same ledger directory.
fn start(dir: &Path, mint: &Arc<FakeMint>, receiver: &Arc<FakeReceiver>) -> Seller {
    Seller::durable(mint, receiver, disk(dir), |_| {})
}

/// A provider that dies at a chosen ledger write.
fn start_doomed(
    dir: &Path,
    mint: &Arc<FakeMint>,
    receiver: &Arc<FakeReceiver>,
) -> (Seller, Arc<CrashStore>) {
    let store = CrashStore::new(disk(dir));
    let seller = Seller::durable(mint, receiver, store.clone(), |_| {});
    (seller, store)
}

fn balance(seller: &Seller) -> (u64, u64) {
    match seller.provider.accounts().as_slice() {
        [] => (0, 0),
        [account] => (account.paid_msat, account.charged_msat),
        more => panic!("expected one account, got {more:?}"),
    }
}

#[tokio::test]
async fn balances_and_replay_protection_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let mint = Arc::new(FakeMint::default());
    let receiver = FakeReceiver::new(&mint);
    let token = mint.issue(MINT, 4);

    let seller = start(dir.path(), &mint, &receiver);
    let (first, _) = seller.add(1, TWO_MIB).await;
    let (second, _) = seller.add(2, 300_000).await;
    assert_eq!(seller.ask(pay(&token)).await, paid(4_000, 4_000));
    for hash in [first, second] {
        assert!(matches!(seller.ask(get(hash)).await, Reply::Data { .. }));
    }
    assert_eq!(balance(&seller), (4_000, 3_048));
    drop(seller);

    let seller = start(dir.path(), &mint, &receiver);
    assert_eq!(balance(&seller), (4_000, 3_048));

    // The same payment again is answered from the record, not redeemed.
    assert_eq!(seller.ask(pay(&token)).await, paid(4_000, 952));
    // The token is known under any other payment id and account.
    let renamed = seller.ask(pay_into(ACCOUNT, [2; 16], &token)).await;
    assert_eq!(error_code(&renamed), ErrorCode::Replay);
    let moved = seller.ask(pay_into([8; 32], [3; 16], &token)).await;
    assert_eq!(error_code(&moved), ErrorCode::Replay);
    assert_eq!(
        receiver.redemptions(),
        1,
        "redeemed once, before the restart"
    );
    assert_eq!(balance(&seller), (4_000, 3_048));

    // The balance still buys: 952 msat left, the next blob costs 1000.
    let (third, data) = seller.add(3, 500_000).await;
    let Reply::Quote(quote) = seller.ask(get(third)).await else {
        panic!("952 msat do not cover a 1000 msat blob");
    };
    assert_eq!((quote.balance_msat, quote.price_msat), (952, 1_000));
    let more = mint.issue(MINT, 1);
    let topped = seller.ask(pay_into(ACCOUNT, [4; 16], &more)).await;
    assert!(matches!(
        topped,
        Reply::Paid {
            balance_msat: 1_952,
            ..
        }
    ));
    let Reply::Data {
        balance_msat,
        data: served,
        ..
    } = seller.ask(get(third)).await
    else {
        panic!("a covered blob must be served");
    };
    assert_eq!((balance_msat, served), (952, data));
    drop(seller);

    // Both payments are still known after a second restart.
    let seller = start(dir.path(), &mint, &receiver);
    assert_eq!(balance(&seller), (5_000, 4_048));
    assert_eq!(seller.ask(pay(&token)).await, paid(4_000, 952));
    assert_eq!(receiver.received(), 5);
    assert_eq!(receiver.redemptions(), 2);
}

#[tokio::test]
async fn charges_reach_the_disk_within_a_second_without_a_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let mint = Arc::new(FakeMint::default());
    let receiver = FakeReceiver::new(&mint);
    let (seller, store) = start_doomed(dir.path(), &mint, &receiver);
    let (hash, _) = seller.add(1, TWO_MIB).await;
    let token = mint.issue(MINT, 4);
    assert_eq!(seller.ask(pay(&token)).await, paid(4_000, 4_000));
    assert!(matches!(seller.ask(get(hash)).await, Reply::Data { .. }));

    // No flush is asked for; the charge is written behind on its own.
    let written = || store.load().unwrap().accounts[&ACCOUNT].charged_msat;
    assert_eq!(written(), 0, "a charge does not wait for the disk");
    let started = Instant::now();
    while written() == 0 && started.elapsed() < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(written(), 2_048);
}

#[tokio::test]
async fn a_crash_forgives_unwritten_charges_but_keeps_every_credit() {
    let dir = tempfile::tempdir().unwrap();
    let mint = Arc::new(FakeMint::default());
    let receiver = FakeReceiver::new(&mint);
    let token = mint.issue(MINT, 4);

    let (seller, store) = start_doomed(dir.path(), &mint, &receiver);
    let (hash, _) = seller.add(1, TWO_MIB).await;
    assert_eq!(seller.ask(pay(&token)).await, paid(4_000, 4_000));
    // The process dies before the charge is written behind.
    store.crash_before(|entry| matches!(entry, LedgerEntry::Charged { .. }));
    assert!(matches!(seller.ask(get(hash)).await, Reply::Data { .. }));
    assert!(seller.provider.flush().is_err());
    drop((seller, store));

    let seller = start(dir.path(), &mint, &receiver);
    assert_eq!(balance(&seller), (4_000, 0));
}

#[tokio::test]
async fn a_token_redeemed_before_a_crash_is_credited_once_after_it() {
    let dir = tempfile::tempdir().unwrap();
    let mint = Arc::new(FakeMint::default());
    let receiver = FakeReceiver::new(&mint);
    let token = mint.issue(MINT, 4);

    // The mint redeems the token, then the process dies before the credit
    // is on disk. The client is never told `paid`.
    let (seller, store) = start_doomed(dir.path(), &mint, &receiver);
    store.crash_before(|entry| matches!(entry, LedgerEntry::Credited { .. }));
    let reply = seller.ask(pay(&token)).await;
    assert_eq!(error_code(&reply), ErrorCode::Retry);
    assert!(store.is_dead());
    assert_eq!(receiver.received(), 4, "the token is spent at the mint");
    drop((seller, store));

    // Nothing was credited, but the redemption left a trace.
    let seller = start(dir.path(), &mint, &receiver);
    assert_eq!(balance(&seller), (0, 0));
    // Nobody else can claim the token while its outcome is open.
    let stolen = seller.ask(pay_into([8; 32], [9; 16], &token)).await;
    assert_eq!(error_code(&stolen), ErrorCode::Replay);
    // The client sends its payment again: the mint says spent, the wallet
    // says it was this provider that received it.
    assert_eq!(seller.ask(pay(&token)).await, paid(4_000, 4_000));
    assert_eq!(seller.ask(pay(&token)).await, paid(4_000, 4_000));
    assert_eq!(balance(&seller), (4_000, 0));
    drop(seller);

    let seller = start(dir.path(), &mint, &receiver);
    assert_eq!(seller.ask(pay(&token)).await, paid(4_000, 4_000));
    let renamed = seller.ask(pay_into(ACCOUNT, [2; 16], &token)).await;
    assert_eq!(error_code(&renamed), ErrorCode::Replay);
    assert_eq!(balance(&seller), (4_000, 0));
    assert_eq!(receiver.received(), 4, "received once");
}

#[tokio::test]
async fn a_crash_after_the_credit_is_on_disk_does_not_credit_it_again() {
    let dir = tempfile::tempdir().unwrap();
    let mint = Arc::new(FakeMint::default());
    let receiver = FakeReceiver::new(&mint);
    let token = mint.issue(MINT, 4);

    // The credit is written, then the process dies; `paid` never arrives.
    let (seller, store) = start_doomed(dir.path(), &mint, &receiver);
    store.crash_after(|entry| matches!(entry, LedgerEntry::Credited { .. }));
    let _lost_reply = seller.ask(pay(&token)).await;
    assert!(store.is_dead());
    drop((seller, store));

    let seller = start(dir.path(), &mint, &receiver);
    assert_eq!(balance(&seller), (4_000, 0));
    assert_eq!(seller.ask(pay(&token)).await, paid(4_000, 4_000));
    assert_eq!(balance(&seller), (4_000, 0));
    assert_eq!(receiver.redemptions(), 1, "answered from the record");
}

#[tokio::test]
async fn a_crash_before_the_mint_is_asked_leaves_the_token_spendable() {
    let dir = tempfile::tempdir().unwrap();
    let mint = Arc::new(FakeMint::default());
    let receiver = FakeReceiver::new(&mint);
    let token = mint.issue(MINT, 4);

    // The process dies while it records that it is about to redeem.
    let (seller, store) = start_doomed(dir.path(), &mint, &receiver);
    store.crash_before(|entry| matches!(entry, LedgerEntry::Redeeming(_)));
    let reply = seller.ask(pay(&token)).await;
    assert_eq!(error_code(&reply), ErrorCode::Retry);
    assert_eq!(receiver.redemptions(), 0, "no trace on disk, no redemption");
    drop((seller, store));

    // It dies again right after recording it, the mint out of reach.
    let (seller, store) = start_doomed(dir.path(), &mint, &receiver);
    store.crash_after(|entry| matches!(entry, LedgerEntry::Redeeming(_)));
    receiver.offline.store(true, Ordering::SeqCst);
    let reply = seller.ask(pay(&token)).await;
    assert_eq!(error_code(&reply), ErrorCode::Retry);
    drop((seller, store));
    receiver.offline.store(false, Ordering::SeqCst);

    let seller = start(dir.path(), &mint, &receiver);
    assert_eq!(seller.ask(pay(&token)).await, paid(4_000, 4_000));
    assert_eq!(receiver.received(), 4);
    drop(seller);
    let seller = start(dir.path(), &mint, &receiver);
    assert_eq!(seller.ask(pay(&token)).await, paid(4_000, 4_000));
    assert_eq!(balance(&seller), (4_000, 0));
}

#[tokio::test]
async fn a_failed_ledger_write_is_repeated_without_asking_the_mint_again() {
    let dir = tempfile::tempdir().unwrap();
    let mint = Arc::new(FakeMint::default());
    // A wallet that cannot say what it received: the provider must not need it
    // while it is still running.
    let receiver = FakeReceiver::without_records(&mint);
    let token = mint.issue(MINT, 4);

    let failing = Arc::new(common::FailingStore::new(disk(dir.path())));
    let seller = Seller::durable(&mint, &receiver, failing.clone(), |_| {});
    failing.fail_credits.store(true, Ordering::SeqCst);
    let reply = seller.ask(pay(&token)).await;
    assert_eq!(error_code(&reply), ErrorCode::Retry);
    assert_eq!(
        balance(&seller),
        (0, 0),
        "not credited before it is on disk"
    );

    failing.fail_credits.store(false, Ordering::SeqCst);
    assert_eq!(seller.ask(pay(&token)).await, paid(4_000, 4_000));
    assert_eq!(receiver.redemptions(), 1);
    drop((seller, failing));
    let seller = start(dir.path(), &mint, &receiver);
    assert_eq!(balance(&seller), (4_000, 0));
}

#[tokio::test]
async fn an_open_redemption_is_not_credited_on_the_clients_word() {
    let dir = tempfile::tempdir().unwrap();
    let mint = Arc::new(FakeMint::default());
    let receiver = FakeReceiver::without_records(&mint);
    let token = mint.issue(MINT, 4);

    // The redemption is on record and the mint out of reach when the
    // process dies. The client then spends the token elsewhere.
    let (seller, store) = start_doomed(dir.path(), &mint, &receiver);
    store.crash_after(|entry| matches!(entry, LedgerEntry::Redeeming(_)));
    receiver.offline.store(true, Ordering::SeqCst);
    seller.ask(pay(&token)).await;
    drop((seller, store));
    receiver.offline.store(false, Ordering::SeqCst);
    assert_eq!(mint.redeem(MINT, &token), Ok(4));

    // Spent, and the wallet has no record of receiving it: no credit.
    let seller = start(dir.path(), &mint, &receiver);
    let reply = seller.ask(pay(&token)).await;
    assert_eq!(error_code(&reply), ErrorCode::Invalid);
    assert_eq!(balance(&seller), (0, 0));
    drop(seller);
    // The refusal closed the redemption for good.
    let state = disk(dir.path()).load().unwrap();
    assert!(state.redeeming.is_empty());
    assert!(state.tokens.is_empty());
}
