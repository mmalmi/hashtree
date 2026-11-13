//! What protects the buyer's money: fee and credit accounting, what is saved
//! before a payment, and tokens whose fate is not known.

mod common;

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use common::{DirectTransport, FakeMint, FakeReceiver, FakeWallet, Seller, MINT, TARIFF};
use hashtree_core::{BlobReply, BlobRequest, BlobRoute, Hash};
use hashtree_paid_retrieval::{
    AccountState, AccountStore, ClientPolicy, ErrorCode, FileAccountStore, LimitedWallet,
    PaidBlobRoute, PaymentIntent, PaymentRequest, PendingPayment, Reply, RetrievalWallet,
    SpendStore, SpendingLimit, WalletError, WalletPayment,
};

fn policy() -> ClientPolicy {
    ClientPolicy {
        max_tariff: TARIFF,
        mints: vec![MINT.to_owned()],
        topup_sat: 5,
        max_prepaid_msat: 10_000,
        max_loss_msat: 1_500,
        max_token_fee_sat: 0,
        max_redeem_fee_msat: 1_000,
        ..ClientPolicy::default()
    }
}

async fn read(route: &PaidBlobRoute, hash: Hash) -> Result<BlobReply, String> {
    route
        .route(BlobRequest { hash, htl: 0 })
        .await
        .map_err(|error| error.to_string())
}

/// Remembers every saved account, like a database would.
#[derive(Default)]
struct RecordingStore(Mutex<Vec<AccountState>>);

impl RecordingStore {
    fn last(&self) -> AccountState {
        self.0.lock().unwrap().last().cloned().expect("a save")
    }
}

impl AccountStore for RecordingStore {
    fn save(&self, account: &AccountState) -> Result<(), String> {
        self.0.lock().unwrap().push(account.clone());
        Ok(())
    }
}

fn seller_keeping(mint_fee_sat: u64) -> Seller {
    let mint = Arc::new(FakeMint::default());
    let mut receiver = Arc::into_inner(FakeReceiver::new(&mint)).unwrap();
    receiver.mint_fee_sat = mint_fee_sat;
    Seller::on_mint(mint, Arc::new(receiver), |_| {})
}

#[tokio::test]
async fn a_small_redemption_fee_is_recorded_and_is_not_a_loss() {
    let seller = seller_keeping(1);
    let (hash, data) = seller.add(1, 4_096).await;
    let wallet = FakeWallet::funded(&seller.mint, 21);
    let transport = DirectTransport::new(&seller.provider);
    let route = PaidBlobRoute::new("seller", transport, wallet, policy());

    assert_eq!(read(&route, hash).await, Ok(BlobReply::Data(data)));
    let account = route.account_state();
    assert_eq!(
        account.credited_msat, 4_000,
        "5 sat paid, 1 kept by the mint"
    );
    assert_eq!((account.loss_msat(), account.blocked), (0, None));
    assert_eq!(route.stats().redeem_fee_msat, 1_000);
}

#[tokio::test]
async fn a_provider_that_credits_less_than_it_was_paid_is_caught() {
    // Three of five sats vanish at the provider: more than any mint fee.
    let seller = seller_keeping(3);
    let (hash, data) = seller.add(1, 4_096).await;
    let (second, _) = seller.add(2, 4_096).await;
    let wallet = FakeWallet::funded(&seller.mint, 21);
    let transport = DirectTransport::new(&seller.provider);
    let route = PaidBlobRoute::new("seller", transport, wallet.clone(), policy());

    // The token rode with the request and still covered this blob.
    assert_eq!(read(&route, hash).await, Ok(BlobReply::Data(data)));
    assert_eq!(read(&route, second).await, Ok(BlobReply::NoResult));
    let account = route.account_state();
    assert_eq!(
        account.loss_msat(),
        2_000,
        "the shortfall beyond the allowance"
    );
    assert!(account.blocked.is_some());
    assert_eq!(wallet.payments(), 1, "and it is not paid again");
}

#[tokio::test]
async fn the_spending_limit_reserves_the_fee_before_the_wallet_is_asked() {
    let mint = Arc::new(FakeMint::default());
    let mut funded = Arc::into_inner(FakeWallet::funded(&mint, 21)).unwrap();
    funded.fee_sat = 1;
    let funded = Arc::new(funded);
    let limit = SpendingLimit::new(5);
    let wallet = LimitedWallet::new(funded.clone(), limit.clone());
    let request = |amount_sat| PaymentRequest {
        provider: "seller".into(),
        mints: vec![MINT.into()],
        amount_sat,
        max_fee_sat: 1,
        lock: None,
        payment_id: [amount_sat as u8; 16],
    };

    // Five sats fit the limit, five plus a one sat fee do not.
    assert!(matches!(
        wallet.pay(&request(5)).await,
        Err(WalletError::LimitReached {
            requested_sat: 6,
            remaining_sat: 5
        })
    ));
    assert_eq!((funded.payments(), limit.spent_sat()), (0, 0));

    wallet.pay(&request(4)).await.unwrap();
    assert_eq!((limit.spent_sat(), limit.remaining_sat()), (5, 0));
}

#[tokio::test]
async fn spending_is_saved_before_a_payment_and_a_failed_save_stops_it() {
    struct Spent(Mutex<Vec<u64>>, bool);
    impl SpendStore for Spent {
        fn save_spent(&self, spent_sat: u64) -> Result<(), String> {
            self.0.lock().unwrap().push(spent_sat);
            match self.1 {
                true => Err("disk full".into()),
                false => Ok(()),
            }
        }
    }
    let mint = Arc::new(FakeMint::default());
    let request = PaymentRequest {
        provider: "seller".into(),
        mints: vec![MINT.into()],
        amount_sat: 4,
        max_fee_sat: 1,
        lock: None,
        payment_id: [1; 16],
    };

    let saved = Arc::new(Spent(Mutex::default(), false));
    let funded = FakeWallet::funded(&mint, 21);
    let limit = SpendingLimit::with_store(10, 2, saved.clone());
    LimitedWallet::new(funded, limit.clone())
        .pay(&request)
        .await
        .unwrap();
    // The reservation with its fee ceiling first, then the real cost.
    assert_eq!(*saved.0.lock().unwrap(), vec![7, 6]);
    assert_eq!(limit.spent_sat(), 6);

    let broken = Arc::new(Spent(Mutex::default(), true));
    let funded = FakeWallet::funded(&mint, 21);
    let limit = SpendingLimit::with_store(10, 0, broken);
    let refused = LimitedWallet::new(funded.clone(), limit.clone())
        .pay(&request)
        .await;
    assert!(matches!(refused, Err(WalletError::Failed(_))));
    assert_eq!((funded.payments(), limit.spent_sat()), (0, 0));
}

#[tokio::test]
async fn a_token_made_just_before_a_crash_is_found_again_not_paid_twice() {
    let seller = Seller::new();
    let (hash, data) = seller.add(1, 4_096).await;
    let wallet = FakeWallet::funded(&seller.mint, 21);
    let transport = DirectTransport::new(&seller.provider);
    let store = Arc::new(RecordingStore::default());
    let route = PaidBlobRoute::with_store(
        "seller",
        transport.clone(),
        wallet.clone(),
        policy(),
        AccountState::new(),
        store.clone(),
    );

    // The wallet creates the token and the process dies before the route
    // learns of it.
    wallet.lose_reply.store(true, Ordering::SeqCst);
    assert!(read(&route, hash).await.is_err());
    assert_eq!(wallet.payments(), 1);
    let saved = store.last();
    let intent = saved
        .intent
        .clone()
        .expect("saved before the wallet was asked");
    assert_eq!((intent.amount_sat, saved.pending.clone()), (5, None));
    drop(route);

    // After the restart the wallet is asked about that payment id.
    let restarted = PaidBlobRoute::with_store(
        "seller",
        transport,
        wallet.clone(),
        policy(),
        saved,
        store.clone(),
    );
    assert_eq!(read(&restarted, hash).await, Ok(BlobReply::Data(data)));
    assert_eq!(wallet.payments(), 1, "the same token, not a second one");
    assert_eq!(seller.receiver.received(), 5);
    let saved = store.last();
    assert_eq!(
        (saved.intent, saved.pending, saved.credited_msat),
        (None, None, 5_000)
    );
}

#[tokio::test]
async fn a_token_is_saved_before_it_is_sent() {
    let seller = Seller::new();
    let (hash, _) = seller.add(1, 4_096).await;
    let wallet = FakeWallet::funded(&seller.mint, 21);
    let store = Arc::new(RecordingStore::default());
    let seen = store.clone();
    // The provider receives the token only after it was saved as pending.
    let transport = DirectTransport::tampering(&seller.provider, move |request, reply| {
        if let Some(payment) = common::payment_of(request) {
            let pending = seen.last().pending.expect("pending before sending");
            assert_eq!(pending.payment.token, payment.token);
        }
        Some(reply)
    });
    let route = PaidBlobRoute::with_store(
        "seller",
        transport,
        wallet,
        policy(),
        AccountState::new(),
        store,
    );
    assert!(matches!(read(&route, hash).await, Ok(BlobReply::Data(_))));
}

#[tokio::test]
async fn a_refused_token_that_cannot_be_reclaimed_is_kept_not_dropped() {
    let seller = Seller::new();
    let (hash, _) = seller.add(1, 4_096).await;
    let wallet = FakeWallet::funded(&seller.mint, 21);
    // The provider answers every payment as refused, without redeeming.
    seller.receiver.offline.store(true, Ordering::SeqCst);
    let transport =
        DirectTransport::tampering(&seller.provider, |request, reply| match (request, reply) {
            (request, _) if common::payment_of(request).is_some() => Some(Reply::Error {
                code: ErrorCode::Invalid,
                payment: common::payment_of(request).map(|payment| payment.id),
                detail: "refused".into(),
            }),
            (_, reply) => Some(reply),
        });
    let route = PaidBlobRoute::new("seller", transport, wallet.clone(), policy());

    wallet.cannot_reclaim.store(true, Ordering::SeqCst);
    assert!(read(&route, hash).await.is_err());
    let account = route.account_state();
    assert_eq!(account.pending, None);
    assert_eq!(
        account.unresolved.len(),
        1,
        "neither credited nor reclaimed"
    );
    assert_eq!(account.unresolved_msat(), 5_000);
    assert_eq!((wallet.balance(), route.stats().paid_sat), (16, 5));

    // Once the wallet can reach its mint again, settling takes it back.
    wallet.cannot_reclaim.store(false, Ordering::SeqCst);
    route.settle().await.unwrap();
    assert!(route.account_state().unresolved.is_empty());
    assert_eq!((wallet.balance(), route.stats().paid_sat), (21, 0));
}

#[tokio::test]
async fn a_provider_that_keeps_saying_not_yet_is_bounded_and_settled_later() {
    let seller = Seller::new();
    let (hash, data) = seller.add(1, 4_096).await;
    let wallet = FakeWallet::funded(&seller.mint, 21);
    let transport = DirectTransport::new(&seller.provider);
    let route = PaidBlobRoute::new("seller", transport, wallet.clone(), policy());

    // The provider cannot reach its mint: every payment is "not yet".
    seller.receiver.offline.store(true, Ordering::SeqCst);
    let mut outcomes = Vec::new();
    for _ in 0..7 {
        outcomes.push(
            read(&route, hash)
                .await
                .map(|reply| reply == BlobReply::NoResult),
        );
    }
    // Two top-ups reach the prepaid cap; after that nothing more is paid.
    assert_eq!(wallet.payments(), 2);
    assert_eq!(outcomes.last(), Some(&Ok(true)));
    let account = route.account_state();
    assert_eq!(
        (account.pending.clone(), account.unresolved.len()),
        (None, 2)
    );
    assert_eq!(account.unresolved_msat(), 10_000);
    assert_eq!(
        route.stats().declined_limit,
        outcomes.iter().filter(|o| **o == Ok(true)).count() as u64
    );

    // The mint is back: the tokens the provider held all along are credited.
    seller.receiver.offline.store(false, Ordering::SeqCst);
    assert_eq!(read(&route, hash).await, Ok(BlobReply::Data(data)));
    route.settle().await.unwrap();
    let account = route.account_state();
    assert!(account.unresolved.is_empty());
    assert_eq!(
        (account.credited_msat, account.balance_msat),
        (10_000, 9_000)
    );
    assert_eq!((wallet.payments(), account.loss_msat()), (2, 0));
}

#[tokio::test]
async fn an_account_file_carries_the_balance_across_a_restart() {
    let seller = Seller::new();
    let mut blobs = Vec::new();
    for seed in 0..3 {
        blobs.push(seller.add(seed, 4_096).await);
    }
    let wallet = FakeWallet::funded(&seller.mint, 21);
    let transport = DirectTransport::new(&seller.provider);
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("seller.account");
    assert_eq!(FileAccountStore::new(&file).load().unwrap(), None);

    let route = PaidBlobRoute::with_store(
        "seller",
        transport.clone(),
        wallet.clone(),
        policy(),
        AccountState::new(),
        Arc::new(FileAccountStore::new(&file)),
    );
    for (hash, data) in &blobs[..2] {
        assert_eq!(read(&route, *hash).await, Ok(BlobReply::Data(data.clone())));
    }
    drop(route);

    // The file was written when the payment was credited; the two charges
    // since then were not saved one by one.
    let store = FileAccountStore::new(&file);
    let saved = store.load().unwrap().expect("an account");
    assert_eq!((saved.credited_msat, saved.charged_msat), (5_000, 0));

    let restarted = PaidBlobRoute::with_store(
        "seller",
        transport,
        wallet.clone(),
        policy(),
        saved,
        Arc::new(store),
    );
    let (hash, data) = &blobs[2];
    assert_eq!(
        read(&restarted, *hash).await,
        Ok(BlobReply::Data(data.clone()))
    );
    // The provider's balance is the baseline, not a 2000 msat loss.
    let account = restarted.account_state();
    assert_eq!((account.balance_msat, account.loss_msat()), (2_000, 0));
    assert_eq!(account.blocked, None);
    assert_eq!(restarted.stats().resynced_msat, 2_000);
    assert_eq!(wallet.payments(), 1);
}

#[test]
fn an_account_survives_its_own_encoding() {
    let token = |seed: u8| PendingPayment {
        id: [seed; 16],
        attempts: u32::from(seed),
        payment: WalletPayment {
            mint_url: MINT.into(),
            token: format!("cashuFake{seed}"),
            amount_sat: 5,
            fee_sat: 1,
            reclaim_id: seed.is_multiple_of(2).then(|| format!("operation-{seed}")),
        },
    };
    let account = AccountState {
        credited_msat: 9_000,
        charged_msat: 4_096,
        balance_msat: -17,
        intent: Some(PaymentIntent {
            id: [9; 16],
            amount_sat: 5,
            mints: vec![MINT.into(), "https://other.test".into()],
        }),
        pending: Some(token(1)),
        unresolved: vec![token(2), token(3)],
        blocked: Some("provider balance is low".into()),
        ..AccountState::new()
    };
    assert_eq!(AccountState::decode(&account.encode()).unwrap(), account);
    let fresh = AccountState::new();
    assert_eq!(AccountState::decode(&fresh.encode()).unwrap(), fresh);

    let mut damaged = account.encode();
    damaged.pop();
    assert!(AccountState::decode(&damaged).is_err());
    assert!(AccountState::decode(b"de").is_err());
}
