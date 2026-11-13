//! The exchange as a provider sees it: quote, payment, delivery and refusals.

mod common;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use common::{FakeMint, FakeReceiver, Seller, MINT, TARIFF};
use hashtree_core::Hash;
use hashtree_paid_retrieval::{AccountId, ErrorCode, PaymentId, Reply, Request, METHOD_CASHU};

const ACCOUNT: AccountId = [7; 32];
const TWO_MIB: usize = 2 * 1024 * 1024;

fn get(hash: Hash, max_msat: u64) -> Request {
    get_from(ACCOUNT, hash, max_msat)
}

fn get_from(account: AccountId, hash: Hash, max_msat: u64) -> Request {
    Request::Get {
        hash: hash.into(),
        account,
        max_msat,
    }
}

fn pay(payment: PaymentId, token: &str) -> Request {
    pay_into(ACCOUNT, payment, MINT, token)
}

fn pay_into(account: AccountId, payment: PaymentId, mint: &str, token: &str) -> Request {
    Request::Pay {
        account,
        payment,
        method: METHOD_CASHU.into(),
        mint: mint.into(),
        token: token.into(),
    }
}

fn error_code(reply: &Reply) -> ErrorCode {
    match reply {
        Reply::Error { code, .. } => *code,
        other => panic!("expected an error, got {other:?}"),
    }
}

#[tokio::test]
async fn quote_then_payment_then_delivery() {
    let seller = Seller::new();
    let (hash, data) = seller.add(1, TWO_MIB).await;

    let Reply::Quote(quote) = seller.ask(get(hash, u64::MAX)).await else {
        panic!("an unpaid get must be answered with a quote");
    };
    assert_eq!(quote.hash, hash.into());
    assert_eq!(quote.size, TWO_MIB as u64);
    assert_eq!(quote.price_msat, 2_048);
    assert_eq!(quote.balance_msat, 0);
    assert_eq!(quote.grace_msat, 0);
    assert_eq!(quote.tariff, TARIFF);
    assert_eq!(quote.mints, vec![MINT.to_owned()]);
    assert_eq!(quote.methods, vec![METHOD_CASHU.to_owned()]);

    let token = seller.mint.issue(MINT, 3);
    let paid = seller.ask(pay([1; 16], &token)).await;
    assert_eq!(
        paid,
        Reply::Paid {
            payment: [1; 16],
            credited_msat: 3_000,
            balance_msat: 3_000,
        }
    );

    let delivered = seller.ask(get(hash, quote.price_msat)).await;
    assert_eq!(
        delivered,
        Reply::Data {
            hash: hash.into(),
            charged_msat: 2_048,
            balance_msat: 952,
            duplicate: false,
            data,
        }
    );
    assert_eq!(seller.receiver.received(), 3);
}

#[tokio::test]
async fn unknown_blobs_and_malformed_requests_cost_nothing() {
    let seller = Seller::new();
    let missing = [9; 32];
    assert_eq!(
        seller.ask(get(missing, u64::MAX)).await,
        Reply::NoResult {
            hash: missing.into()
        }
    );
    let reply = Reply::decode(&seller.provider.handle(None, b"not bencode").await).unwrap();
    assert_eq!(error_code(&reply), ErrorCode::Bad);
    assert!(seller.provider.accounts().is_empty());
}

#[tokio::test]
async fn underpayment_is_credited_but_does_not_deliver() {
    let seller = Seller::new();
    let (hash, _) = seller.add(2, TWO_MIB).await;

    let token = seller.mint.issue(MINT, 1);
    assert!(matches!(
        seller.ask(pay([1; 16], &token)).await,
        Reply::Paid {
            credited_msat: 1_000,
            balance_msat: 1_000,
            ..
        }
    ));
    let Reply::Quote(quote) = seller.ask(get(hash, u64::MAX)).await else {
        panic!("an underpaid get must not deliver");
    };
    assert_eq!((quote.price_msat, quote.balance_msat), (2_048, 1_000));

    // The earlier sat was kept: topping up the difference is enough.
    let token = seller.mint.issue(MINT, 2);
    seller.ask(pay([2; 16], &token)).await;
    assert!(matches!(
        seller.ask(get(hash, u64::MAX)).await,
        Reply::Data {
            balance_msat: 952,
            ..
        }
    ));
}

#[tokio::test]
async fn a_debit_above_the_authorised_amount_is_quoted_not_charged() {
    let seller = Seller::new();
    let (hash, _) = seller.add(3, TWO_MIB).await;
    let token = seller.mint.issue(MINT, 5);
    seller.ask(pay([1; 16], &token)).await;

    let Reply::Quote(quote) = seller.ask(get(hash, 2_047)).await else {
        panic!("the price exceeds what the client authorised");
    };
    assert_eq!(quote.balance_msat, 5_000);
    assert!(matches!(
        seller.ask(get(hash, 2_048)).await,
        Reply::Data { .. }
    ));
}

#[tokio::test]
async fn a_payment_is_idempotent_and_a_token_is_credited_once() {
    let seller = Seller::new();
    let token = seller.mint.issue(MINT, 4);

    let first = seller.ask(pay([1; 16], &token)).await;
    let repeated = seller.ask(pay([1; 16], &token)).await;
    assert_eq!(first, repeated, "a resent payment returns the same result");

    let replay = seller.ask(pay([2; 16], &token)).await;
    assert_eq!(error_code(&replay), ErrorCode::Replay);
    let other_account = seller.ask(pay_into([8; 32], [3; 16], MINT, &token)).await;
    assert_eq!(error_code(&other_account), ErrorCode::Replay);

    let accounts = seller.provider.accounts();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].paid_msat, 4_000);
    assert_eq!(seller.receiver.received(), 4);
}

#[tokio::test]
async fn spent_tokens_and_foreign_mints_are_refused() {
    let seller = Seller::new();

    let spent = seller.mint.issue(MINT, 4);
    seller.mint.redeem(MINT, &spent).unwrap();
    let reply = seller.ask(pay([1; 16], &spent)).await;
    assert_eq!(error_code(&reply), ErrorCode::Invalid);

    // A token of an unlisted mint is refused before anything is redeemed.
    let foreign = seller.mint.issue("https://other.test", 4);
    let reply = seller
        .ask(pay_into(ACCOUNT, [2; 16], "https://other.test", &foreign))
        .await;
    assert_eq!(error_code(&reply), ErrorCode::Mint);
    assert_eq!(seller.mint.redeem("https://other.test", &foreign), Ok(4));

    // Naming an accepted mint does not launder a foreign token.
    let foreign = seller.mint.issue("https://other.test", 4);
    let reply = seller.ask(pay([3; 16], &foreign)).await;
    assert_eq!(error_code(&reply), ErrorCode::Invalid);

    let reply = seller
        .ask(Request::Pay {
            account: ACCOUNT,
            payment: [4; 16],
            method: "iou".into(),
            mint: MINT.into(),
            token: "x".into(),
        })
        .await;
    assert_eq!(error_code(&reply), ErrorCode::Method);
    assert!(seller.provider.accounts().is_empty());
    assert_eq!(seller.receiver.received(), 0);
}

#[tokio::test]
async fn an_unreachable_mint_leaves_the_payment_retryable() {
    let seller = Seller::new();
    let token = seller.mint.issue(MINT, 2);

    seller.receiver.offline.store(true, Ordering::SeqCst);
    let reply = seller.ask(pay([1; 16], &token)).await;
    assert_eq!(error_code(&reply), ErrorCode::Retry);

    seller.receiver.offline.store(false, Ordering::SeqCst);
    assert!(matches!(
        seller.ask(pay([1; 16], &token)).await,
        Reply::Paid {
            credited_msat: 2_000,
            ..
        }
    ));
}

#[tokio::test]
async fn a_charged_blob_is_served_again_free_for_a_while() {
    let seller = Seller::new();
    let (hash, _) = seller.add(4, 4_096).await;
    let token = seller.mint.issue(MINT, 3);
    seller.ask(pay([1; 16], &token)).await;

    let mut replies = Vec::new();
    for _ in 0..4 {
        let Reply::Data {
            charged_msat,
            balance_msat,
            duplicate,
            ..
        } = seller.ask(get(hash, u64::MAX)).await
        else {
            panic!("expected data");
        };
        replies.push((charged_msat, balance_msat, duplicate));
    }
    assert_eq!(
        replies,
        vec![
            (1_000, 2_000, false),
            (1_000, 2_000, true),
            (1_000, 2_000, true),
            (1_000, 1_000, false),
        ],
        "two free repeats report the original charge, then the blob is charged again"
    );
}

#[tokio::test]
async fn free_repeats_end_with_the_redelivery_window() {
    let seller = Seller::with(|config| config.redelivery_window = Duration::ZERO);
    let (hash, _) = seller.add(5, 4_096).await;
    let token = seller.mint.issue(MINT, 3);
    seller.ask(pay([1; 16], &token)).await;

    seller.ask(get(hash, u64::MAX)).await;
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(matches!(
        seller.ask(get(hash, u64::MAX)).await,
        Reply::Data {
            duplicate: false,
            balance_msat: 1_000,
            ..
        }
    ));
}

#[tokio::test]
async fn grace_serves_an_authenticated_peer_before_payment() {
    let seller = Seller::with(|config| config.grace_msat = 1_500);
    let (hash, _) = seller.add(6, 4_096).await;
    let (other, _) = seller.add(7, 4_096).await;

    // Without an authenticated identity there is no grace.
    assert!(matches!(
        seller.ask(get(hash, u64::MAX)).await,
        Reply::Quote(_)
    ));

    let served = seller.ask_as(Some("peer-a"), get(hash, u64::MAX)).await;
    assert!(matches!(
        served,
        Reply::Data {
            balance_msat: -1_000,
            ..
        }
    ));
    // The debt counts: a second blob does not fit in the remaining grace.
    let Reply::Quote(quote) = seller.ask_as(Some("peer-a"), get(other, u64::MAX)).await else {
        panic!("grace is exhausted");
    };
    assert_eq!((quote.balance_msat, quote.grace_msat), (-1_000, 1_500));
    // A fresh account does not give the same peer fresh grace.
    let Reply::Quote(quote) = seller
        .ask_as(Some("peer-a"), get_from([8; 32], other, u64::MAX))
        .await
    else {
        panic!("one grace per peer");
    };
    assert_eq!(quote.grace_msat, 0);

    // Paying the debt restores service.
    let token = seller.mint.issue(MINT, 1);
    seller.ask(pay([1; 16], &token)).await;
    assert!(matches!(
        seller.ask_as(Some("peer-a"), get(other, u64::MAX)).await,
        Reply::Data {
            balance_msat: -1_000,
            ..
        }
    ));
}

#[tokio::test]
async fn a_zero_tariff_serves_without_an_account() {
    let seller = Seller::with(|config| config.tariff = hashtree_paid_retrieval::Tariff::FREE);
    let (hash, _) = seller.add(8, 4_096).await;
    assert!(matches!(
        seller.ask(get(hash, 0)).await,
        Reply::Data {
            charged_msat: 0,
            balance_msat: 0,
            ..
        }
    ));
    assert!(seller.provider.accounts().is_empty());
}

#[tokio::test]
async fn a_redeemed_token_is_credited_even_if_the_connection_drops() {
    let mint = Arc::new(FakeMint::default());
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let receiver = Arc::new(FakeReceiver {
        mint: mint.clone(),
        received_sat: Default::default(),
        offline: Default::default(),
        gate: Some(gate.clone()),
        records: None,
        redemptions: Default::default(),
    });
    let seller = Seller::on_mint(mint, receiver, |_| {});
    let token = seller.mint.issue(MINT, 2);

    let provider = seller.provider.clone();
    let request = pay([1; 16], &token).encode();
    let connection = tokio::spawn(async move { provider.handle(None, &request).await });
    tokio::time::sleep(Duration::from_millis(20)).await;

    // While the token is at the mint, a duplicate is told to come back.
    let busy = seller.ask(pay([1; 16], &token)).await;
    assert_eq!(error_code(&busy), ErrorCode::Retry);

    connection.abort();
    let _ = connection.await;
    gate.add_permits(1);
    tokio::time::sleep(Duration::from_millis(20)).await;

    gate.add_permits(1);
    assert_eq!(
        seller.ask(pay([1; 16], &token)).await,
        Reply::Paid {
            payment: [1; 16],
            credited_msat: 2_000,
            balance_msat: 2_000,
        }
    );
    assert_eq!(seller.receiver.received(), 2, "redeemed exactly once");
}

#[tokio::test]
async fn accounts_survive_a_restart_through_the_persistence_seam() {
    let seller = Seller::new();
    let (hash, _) = seller.add(9, 4_096).await;
    let token = seller.mint.issue(MINT, 3);
    seller.ask(pay([1; 16], &token)).await;
    seller.ask(get(hash, u64::MAX)).await;
    let saved = seller.provider.accounts();
    assert_eq!(saved[0].balance_msat(), 2_000);

    let restarted = Seller::new();
    let (hash, _) = restarted.add(10, 4_096).await;
    restarted.provider.restore_accounts(&saved);
    assert!(matches!(
        restarted.ask(get(hash, u64::MAX)).await,
        Reply::Data {
            balance_msat: 1_000,
            ..
        }
    ));
}

#[tokio::test]
async fn a_full_provider_never_evicts_a_funded_account() {
    let seller = Seller::with(|config| config.max_accounts = 1);
    let token = seller.mint.issue(MINT, 1);
    seller.ask(pay([1; 16], &token)).await;

    let second = seller.mint.issue(MINT, 1);
    let refused = seller.ask(pay_into([8; 32], [2; 16], MINT, &second)).await;
    assert_eq!(error_code(&refused), ErrorCode::Full);
    assert_eq!(seller.mint.redeem(MINT, &second), Ok(1), "not redeemed");
    assert_eq!(seller.provider.accounts()[0].account, ACCOUNT);
}
