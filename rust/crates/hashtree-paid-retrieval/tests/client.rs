//! The paid route as a client uses it, including what goes wrong.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use common::{DirectTransport, FakeReceiver, FakeWallet, Seller, MINT, TARIFF};
use hashtree_core::{BlobReply, BlobRequest, BlobRoute, Hash, StoreError};
use hashtree_paid_retrieval::{
    ClientPolicy, ContentHash, LimitedWallet, PaidBlobRoute, PaidBlobTransport, Reply, Request,
    RetrievalWallet, SpendingLimit, Tariff,
};

const TWO_MIB: usize = 2 * 1024 * 1024;

fn policy() -> ClientPolicy {
    ClientPolicy {
        max_tariff: TARIFF,
        mints: vec![MINT.to_owned()],
        topup_sat: 5,
        max_prepaid_msat: 10_000,
        max_loss_msat: 1_500,
        max_token_fee_sat: 0,
        ..ClientPolicy::default()
    }
}

fn route(
    transport: &Arc<DirectTransport>,
    wallet: Arc<dyn RetrievalWallet>,
    policy: ClientPolicy,
) -> PaidBlobRoute {
    PaidBlobRoute::new("seller", transport.clone(), wallet, policy)
}

async fn read(route: &PaidBlobRoute, hash: Hash) -> Result<BlobReply, String> {
    route
        .route(BlobRequest { hash, htl: 0 })
        .await
        .map_err(|error| error.to_string())
}

#[tokio::test]
async fn one_top_up_pays_for_several_blobs() {
    let seller = Seller::new();
    let (first, first_data) = seller.add(1, TWO_MIB).await;
    let (second, second_data) = seller.add(2, TWO_MIB).await;
    let wallet = FakeWallet::funded(&seller.mint, 21);
    let transport = DirectTransport::new(&seller.provider);
    let route = route(&transport, wallet.clone(), policy());

    assert_eq!(read(&route, first).await, Ok(BlobReply::Data(first_data)));
    assert_eq!(transport.exchanges(), 2, "get, then get carrying the token");
    assert_eq!(read(&route, second).await, Ok(BlobReply::Data(second_data)));
    assert_eq!(transport.exchanges(), 3, "a funded read is one round trip");

    assert_eq!(wallet.payments(), 1);
    assert_eq!(wallet.balance(), 16);
    assert_eq!(seller.receiver.received(), 5);
    let account = route.account_state();
    assert_eq!(account.credited_msat, 5_000);
    assert_eq!(account.charged_msat, 4_096);
    assert_eq!(account.balance_msat, 904);
    assert_eq!(account.loss_msat(), 0);
    let stats = route.stats();
    assert_eq!(
        (stats.blobs, stats.bytes, stats.paid_sat),
        (2, 2 * TWO_MIB as u64, 5)
    );

    // Asking again right away is a free repeat and is not counted twice.
    assert!(matches!(read(&route, first).await, Ok(BlobReply::Data(_))));
    let again = route.account_state();
    assert_eq!((again.charged_msat, again.balance_msat), (4_096, 904));
}

#[tokio::test]
async fn a_missing_blob_is_no_result_and_costs_nothing() {
    let seller = Seller::new();
    let wallet = FakeWallet::funded(&seller.mint, 21);
    let transport = DirectTransport::new(&seller.provider);
    let route = route(&transport, wallet.clone(), policy());

    assert_eq!(read(&route, [3; 32]).await, Ok(BlobReply::NoResult));
    assert_eq!(wallet.payments(), 0);
}

#[tokio::test]
async fn the_spending_limit_stops_payment_before_it_is_made() {
    let seller = Seller::new();
    let (hash, data) = seller.add(1, TWO_MIB).await;
    let (second, _) = seller.add(2, TWO_MIB).await;
    let funded = FakeWallet::funded(&seller.mint, 21);
    let limit = SpendingLimit::new(2);
    let wallet = Arc::new(LimitedWallet::new(funded.clone(), limit.clone()));
    let transport = DirectTransport::new(&seller.provider);
    let route = route(&transport, wallet, policy());

    // The blob costs 2048 msat: three sats are needed, two are allowed.
    assert_eq!(read(&route, hash).await, Ok(BlobReply::NoResult));
    assert_eq!(route.stats().declined_limit, 1);
    assert_eq!((funded.payments(), limit.spent_sat()), (0, 0));

    // Raising the limit lets the minimum through, not the preferred top-up.
    limit.set_limit_sat(4);
    assert_eq!(read(&route, hash).await, Ok(BlobReply::Data(data)));
    assert_eq!((limit.spent_sat(), limit.remaining_sat()), (3, 1));

    // The rest of the limit does not cover another blob.
    assert_eq!(read(&route, second).await, Ok(BlobReply::NoResult));
    assert_eq!(limit.spent_sat(), 3);
    assert_eq!(funded.balance(), 18);
}

#[tokio::test]
async fn the_prepaid_cap_bounds_what_one_provider_holds() {
    let seller = Seller::new();
    let (hash, _) = seller.add(1, TWO_MIB).await;
    let wallet = FakeWallet::funded(&seller.mint, 21);
    let transport = DirectTransport::new(&seller.provider);
    let capped = ClientPolicy {
        max_prepaid_msat: 2_000,
        ..policy()
    };
    let route = route(&transport, wallet.clone(), capped);

    assert_eq!(read(&route, hash).await, Ok(BlobReply::NoResult));
    assert_eq!(route.stats().declined_limit, 1);
    assert_eq!(wallet.payments(), 0);
}

#[tokio::test]
async fn a_quote_above_the_accepted_tariff_is_declined() {
    let seller = Seller::with(|config| {
        config.tariff = Tariff {
            min_msat: 5_000,
            ..TARIFF
        }
    });
    let (hash, _) = seller.add(1, 4_096).await;
    let wallet = FakeWallet::funded(&seller.mint, 21);
    let transport = DirectTransport::new(&seller.provider);
    let route = route(&transport, wallet.clone(), policy());

    assert_eq!(read(&route, hash).await, Ok(BlobReply::NoResult));
    assert_eq!(route.stats().declined_price, 1);
    assert_eq!(wallet.payments(), 0);
}

#[tokio::test]
async fn a_provider_without_a_trusted_mint_is_not_paid() {
    let seller = Seller::new();
    let (hash, _) = seller.add(1, 4_096).await;
    let wallet = FakeWallet::funded(&seller.mint, 21);
    let transport = DirectTransport::new(&seller.provider);
    let other_mint = ClientPolicy {
        mints: vec!["https://other.test".into()],
        ..policy()
    };
    let route = route(&transport, wallet.clone(), other_mint);

    assert_eq!(read(&route, hash).await, Ok(BlobReply::NoResult));
    assert_eq!(route.stats().declined_mint, 1);
    assert_eq!(wallet.payments(), 0);
}

#[tokio::test]
async fn a_lost_payment_acknowledgement_is_resent_not_repaid() {
    let seller = Seller::new();
    let (hash, data) = seller.add(1, TWO_MIB).await;
    let wallet = FakeWallet::funded(&seller.mint, 21);
    let mut lose_next_paid = true;
    let transport = DirectTransport::tampering(&seller.provider, move |_, reply| {
        let acknowledges = matches!(
            reply,
            Reply::Paid { .. }
                | Reply::Data {
                    credit: Some(_),
                    ..
                }
        );
        if acknowledges && std::mem::take(&mut lose_next_paid) {
            return None;
        }
        Some(reply)
    });
    let route = route(&transport, wallet.clone(), policy());

    assert!(read(&route, hash).await.is_err());
    let pending = route.account_state().pending.expect("the token is kept");
    assert_eq!(pending.payment.amount_sat, 5);

    assert_eq!(read(&route, hash).await, Ok(BlobReply::Data(data)));
    assert_eq!(wallet.payments(), 1, "the same token was sent again");
    assert_eq!(seller.receiver.received(), 5, "and credited once");
    let account = route.account_state();
    assert_eq!(account.pending, None);
    assert_eq!((account.credited_msat, account.loss_msat()), (5_000, 0));
}

#[tokio::test]
async fn a_lost_delivery_is_repeated_without_a_second_charge() {
    let seller = Seller::new();
    let (hash, data) = seller.add(1, TWO_MIB).await;
    let wallet = FakeWallet::funded(&seller.mint, 21);
    let mut lose_next_data = true;
    let transport = DirectTransport::tampering(&seller.provider, move |_, reply| {
        if matches!(reply, Reply::Data { .. }) && std::mem::take(&mut lose_next_data) {
            return None;
        }
        Some(reply)
    });
    let route = route(&transport, wallet.clone(), policy());

    assert!(read(&route, hash).await.is_err());
    assert_eq!(read(&route, hash).await, Ok(BlobReply::Data(data)));

    assert_eq!(wallet.payments(), 1);
    assert_eq!(seller.provider.accounts()[0].charged_msat, 2_048);
    let account = route.account_state();
    assert_eq!((account.charged_msat, account.balance_msat), (2_048, 2_952));
    assert_eq!(account.loss_msat(), 0);
}

#[tokio::test]
async fn a_provider_that_takes_payment_and_never_delivers_is_cut_off() {
    let seller = Seller::new();
    let mut hashes = Vec::new();
    for seed in 0..6 {
        hashes.push(seller.add(seed, 4_096).await.0);
    }
    let wallet = FakeWallet::funded(&seller.mint, 21);
    // Payments are credited and blobs are charged, but no data ever arrives.
    let transport = DirectTransport::tampering(&seller.provider, |_, reply| {
        (!matches!(reply, Reply::Data { .. })).then_some(reply)
    });
    let capped = ClientPolicy {
        topup_sat: 2,
        max_prepaid_msat: 2_000,
        ..policy()
    };
    let route = route(&transport, wallet.clone(), capped);

    for hash in &hashes {
        assert_ne!(
            read(&route, *hash)
                .await
                .map(|reply| matches!(reply, BlobReply::Data(_))),
            Ok(true)
        );
    }

    assert_eq!(
        wallet.payments(),
        1,
        "no second top-up after the first vanished"
    );
    assert_eq!(
        wallet.balance(),
        19,
        "the loss is the prepaid cap, not the wallet"
    );
    let account = route.account_state();
    assert_eq!(account.loss_msat(), 2_000);
    assert!(account.blocked.is_some());

    // A blocked provider is not contacted at all.
    let before = transport.exchanges();
    assert_eq!(read(&route, hashes[0]).await, Ok(BlobReply::NoResult));
    assert_eq!(transport.exchanges(), before);
    assert!(route.stats().declined_blocked >= 1);

    // The application may write the loss off and try again.
    route.clear_block().unwrap();
    assert_eq!(route.account_state().loss_msat(), 0);
    assert_eq!(route.account_state().blocked, None);
}

#[tokio::test]
async fn wrong_bytes_are_never_returned_or_counted_as_a_delivery() {
    let seller = Seller::new();
    let (hash, _) = seller.add(1, 4_096).await;
    let wallet = FakeWallet::funded(&seller.mint, 21);
    let transport = DirectTransport::tampering(&seller.provider, |_, reply| match reply {
        Reply::Data {
            hash,
            charged_msat,
            balance_msat,
            duplicate,
            mut data,
            credit,
        } => {
            data[0] ^= 1;
            Some(Reply::Data {
                hash,
                charged_msat,
                balance_msat,
                duplicate,
                data,
                credit,
            })
        }
        other => Some(other),
    });
    let route = route(&transport, wallet.clone(), policy());

    let error = read(&route, hash).await.unwrap_err();
    assert!(error.contains("wrong hash"), "{error}");
    let account = route.account_state();
    assert_eq!(account.charged_msat, 0);
    assert_eq!(account.loss_msat(), 1_000);
    assert_eq!(route.stats().invalid_blobs, 1);
}

#[tokio::test]
async fn an_overcharging_provider_is_blocked() {
    let seller = Seller::new();
    let (hash, data) = seller.add(1, 4_096).await;
    let (second, _) = seller.add(2, 4_096).await;
    let wallet = FakeWallet::funded(&seller.mint, 21);
    let transport = DirectTransport::tampering(&seller.provider, |_, reply| match reply {
        Reply::Data {
            hash,
            balance_msat,
            duplicate,
            data,
            ..
        } => Some(Reply::Data {
            hash,
            charged_msat: 4_000,
            balance_msat: balance_msat - 3_000,
            duplicate,
            data,
            credit: None,
        }),
        other => Some(other),
    });
    let route = route(&transport, wallet.clone(), policy());

    // The verified data is still used; the provider is not paid again.
    assert_eq!(read(&route, hash).await, Ok(BlobReply::Data(data)));
    assert!(route.account_state().blocked.is_some());
    assert_eq!(read(&route, second).await, Ok(BlobReply::NoResult));
    assert_eq!(wallet.payments(), 1);
}

#[tokio::test]
async fn an_underpaying_wallet_tops_up_again_until_the_blob_is_covered() {
    let seller = Seller::new();
    let (hash, data) = seller.add(1, TWO_MIB).await;
    let wallet = Arc::new(FakeWallet {
        short_sat: 3,
        ..Arc::into_inner(FakeWallet::funded(&seller.mint, 21)).unwrap()
    });
    let transport = DirectTransport::new(&seller.provider);
    let route = route(&transport, wallet.clone(), policy());

    // Asked for 5 sat it pays 2: 2000 msat does not cover 2048, so the
    // provider quotes again and the route pays the difference.
    assert_eq!(read(&route, hash).await, Ok(BlobReply::Data(data)));
    assert_eq!(wallet.payments(), 2);
    assert_eq!(route.account_state().credited_msat, 4_000);
}

#[tokio::test]
async fn a_refused_token_is_reclaimed() {
    let seller = Seller::new();
    let (hash, _) = seller.add(1, 4_096).await;
    let wallet = FakeWallet::funded(&seller.mint, 21);
    // The provider answers every payment as if the token were worthless.
    let transport =
        DirectTransport::tampering(&seller.provider, |request, reply| match (request, reply) {
            (request, _) if common::payment_of(request).is_some() => Some(Reply::Error {
                code: hashtree_paid_retrieval::ErrorCode::Invalid,
                payment: common::payment_of(request).map(|payment| payment.id),
                detail: "refused".into(),
            }),
            (_, reply) => Some(reply),
        });
    let _ = seller.receiver.offline.swap(true, Ordering::SeqCst);
    let route = route(&transport, wallet.clone(), policy());

    assert!(read(&route, hash).await.is_err());
    assert_eq!(wallet.balance(), 21, "the unredeemed token came back");
    assert_eq!(route.account_state().pending, None);
    let stats = route.stats();
    assert_eq!((stats.rejected_payments, stats.paid_sat), (1, 0));
}

#[tokio::test]
async fn grace_delivers_first_and_the_client_settles_afterwards() {
    let receiver_mint = Arc::new(common::FakeMint::default());
    let seller = Seller::on_mint(
        receiver_mint.clone(),
        FakeReceiver::new(&receiver_mint),
        |config| config.grace_msat = 1_500,
    );
    let (hash, data) = seller.add(1, 4_096).await;
    let wallet = FakeWallet::funded(&seller.mint, 21);
    let transport = Arc::new(DirectTransport {
        peer: Some("peer-a".into()),
        ..Arc::into_inner(DirectTransport::new(&seller.provider)).unwrap()
    });
    let route = route(&transport, wallet.clone(), policy());

    assert_eq!(read(&route, hash).await, Ok(BlobReply::Data(data)));
    assert_eq!(wallet.payments(), 0, "delivered before any payment");
    assert_eq!(route.account_state().balance_msat, -1_000);

    // The provider's terms are learned from a quote; the next read settles.
    let (second, second_data) = seller.add(2, 4_096).await;
    assert_eq!(read(&route, second).await, Ok(BlobReply::Data(second_data)));
    assert_eq!(wallet.payments(), 1);
    assert!(route.account_state().balance_msat >= 0);
    assert_eq!(route.account_state().loss_msat(), 0);
}

#[tokio::test]
async fn concurrent_reads_share_one_top_up() {
    let seller = Seller::new();
    let mut blobs = Vec::new();
    for seed in 0..4 {
        blobs.push(seller.add(seed, 4_096).await);
    }
    let wallet = FakeWallet::funded(&seller.mint, 21);
    let transport = DirectTransport::new(&seller.provider);
    let route = route(&transport, wallet.clone(), policy());

    let reads = blobs.iter().map(|(hash, _)| {
        let route = route.clone();
        let hash = *hash;
        tokio::spawn(async move { read(&route, hash).await })
    });
    for (read, (_, data)) in reads.collect::<Vec<_>>().into_iter().zip(&blobs) {
        assert_eq!(read.await.unwrap(), Ok(BlobReply::Data(data.clone())));
    }

    assert_eq!(wallet.payments(), 1, "5 sat covers four 1000 msat blobs");
    let account = route.account_state();
    assert_eq!((account.charged_msat, account.balance_msat), (4_000, 1_000));
    assert_eq!((account.loss_msat(), account.blocked), (0, None));
}

/// Delivers the reply to the first get of `hash` only when released, as a
/// carrier that runs exchanges concurrently may.
struct LateReply {
    inner: Arc<DirectTransport>,
    hash: Hash,
    chosen: AtomicBool,
    waiting: AtomicBool,
    release: tokio::sync::Notify,
}

#[async_trait]
impl PaidBlobTransport for LateReply {
    async fn exchange(&self, request: Vec<u8>) -> Result<Vec<u8>, StoreError> {
        let late = matches!(
            Request::decode(&request),
            Ok(Request::Get { hash, .. }) if hash == ContentHash::Sha256(self.hash)
        ) && !self.chosen.swap(true, Ordering::SeqCst);
        let reply = self.inner.exchange(request).await?;
        if late {
            self.waiting.store(true, Ordering::SeqCst);
            self.release.notified().await;
        }
        Ok(reply)
    }
}

#[tokio::test]
async fn a_balance_reported_before_a_credited_payment_is_ignored() {
    let seller = Seller::new();
    let (first, _) = seller.add(1, 4_096).await;
    let (second, second_data) = seller.add(2, 4_096).await;
    let wallet = FakeWallet::funded(&seller.mint, 21);
    let transport = Arc::new(LateReply {
        inner: DirectTransport::new(&seller.provider),
        hash: second,
        chosen: AtomicBool::new(false),
        waiting: AtomicBool::new(false),
        release: tokio::sync::Notify::new(),
    });
    let route = PaidBlobRoute::new("seller", transport.clone(), wallet.clone(), policy());

    let late = tokio::spawn({
        let route = route.clone();
        async move { read(&route, second).await }
    });
    while !transport.waiting.load(Ordering::SeqCst) {
        tokio::task::yield_now().await;
    }
    assert!(matches!(read(&route, first).await, Ok(BlobReply::Data(_))));
    let account = route.account_state();
    assert_eq!(
        (account.credited_msat, account.balance_msat),
        (5_000, 4_000)
    );

    // The late quote says "balance 0", from before the top-up was credited.
    transport.release.notify_one();
    let late = late.await.unwrap();
    assert!(
        matches!(&late, Ok(BlobReply::Data(data)) if *data == second_data),
        "the late read was not served: {:?}",
        late.map(|reply| matches!(reply, BlobReply::Data(_)))
    );
    assert_eq!(wallet.payments(), 1, "no second top-up for a paid balance");
    let account = route.account_state();
    assert_eq!((account.balance_msat, account.loss_msat()), (3_000, 0));
    assert_eq!(account.blocked, None);
}
