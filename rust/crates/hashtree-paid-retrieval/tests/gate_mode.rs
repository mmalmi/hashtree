//! The payment engine without any Hashtree route: a seller and a buyer gate
//! units that travel some other way, named by a SHA-1 piece hash, as a
//! BitTorrent v1 peer would use them.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use common::{FakeMint, FakeReceiver, FakeWallet, MINT, TARIFF};
use hashtree_core::StoreError;
use hashtree_paid_retrieval::{
    AccountState, ClientPolicy, ContentHash, ErrorCode, PaidBlobTransport, PaidBuyer, PaidSeller,
    ProviderConfig, Reply, Request, Sale,
};

const PIECE_BYTES: u64 = 4 * 1024 * 1024;

/// Carries only payment messages; pieces are delivered out of band.
struct PaymentChannel(PaidSeller);

#[async_trait]
impl PaidBlobTransport for PaymentChannel {
    async fn exchange(&self, request: Vec<u8>) -> Result<Vec<u8>, StoreError> {
        let reply = match Request::decode(&request) {
            Ok(Request::Pay {
                account,
                payment,
                method,
                mint,
                token,
            }) => self.0.pay(account, payment, &method, &mint, token).await,
            _ => Reply::error(ErrorCode::Bad, "pieces travel in native messages"),
        };
        Ok(reply.encode())
    }
}

#[tokio::test]
async fn a_seller_and_a_buyer_gate_sha1_pieces_without_a_blob_route() {
    let mint = Arc::new(FakeMint::default());
    let receiver = FakeReceiver::new(&mint);
    let seller = PaidSeller::new(
        receiver.clone(),
        ProviderConfig::new(TARIFF, vec![MINT.to_owned()]),
    );
    let wallet = FakeWallet::funded(&mint, 21);
    let policy = ClientPolicy {
        max_tariff: TARIFF,
        topup_sat: 10,
        ..ClientPolicy::default()
    };
    let buyer = PaidBuyer::new(
        "seeder",
        Arc::new(PaymentChannel(seller.clone())),
        wallet.clone(),
        policy,
        AccountState::new(),
    );
    let account = buyer.account_id();
    let deadline = Instant::now() + Duration::from_secs(5);
    let authorised = TARIFF.price_msat(PIECE_BYTES);
    assert_eq!(authorised, 4_096);

    for index in 0u8..2 {
        let piece = ContentHash::Sha1([index; 20]);
        assert!(buyer.prepare(deadline).await.unwrap());
        let request = buyer.begin(authorised);

        // The seeder decides before it sends the piece.
        let mut sale = seller.charge(&account, None, piece, PIECE_BYTES, authorised);
        if let Sale::Quote(quote) = sale {
            assert_eq!(index, 0, "the second piece is covered by the first top-up");
            assert_eq!((quote.hash, quote.price_msat), (piece, 4_096));
            // The quote survives its own wire encoding with the SHA-1 name.
            let wire = Reply::Quote(quote.clone()).encode();
            assert_eq!(Reply::decode(&wire).unwrap(), Reply::Quote(quote.clone()));

            assert!(buyer.quoted(&request, &quote));
            assert!(buyer.fund(Some(quote), deadline).await.unwrap());
            sale = seller.charge(&account, None, piece, PIECE_BYTES, authorised);
        }
        let Sale::Charged {
            charged_msat,
            balance_msat,
            duplicate,
        } = sale
        else {
            panic!("a funded account is charged");
        };
        // The downloader verified the piece against its SHA-1 itself.
        buyer.delivered(
            &request,
            piece,
            PIECE_BYTES,
            charged_msat,
            balance_msat,
            duplicate,
        );
    }

    assert_eq!(wallet.payments(), 1);
    assert_eq!(receiver.received(), 10);
    let state = buyer.account_state();
    assert_eq!(
        (state.credited_msat, state.charged_msat, state.balance_msat),
        (10_000, 8_192, 1_808)
    );
    assert_eq!((state.loss_msat(), state.blocked), (0, None));
    let sold = seller.accounts();
    assert_eq!((sold[0].paid_msat, sold[0].charged_msat), (10_000, 8_192));
    assert_eq!(buyer.stats().bytes, 2 * PIECE_BYTES);
}

#[tokio::test]
async fn a_blob_provider_has_nothing_for_a_sha1_name() {
    let seller = common::Seller::new();
    let piece = ContentHash::Sha1([3; 20]);
    let reply = seller
        .ask(Request::Get {
            hash: piece,
            account: [1; 32],
            max_msat: u64::MAX,
        })
        .await;
    assert_eq!(reply, Reply::NoResult { hash: piece });
}

#[tokio::test]
async fn a_reply_older_than_the_latest_credit_does_not_look_like_a_loss() {
    let mint = Arc::new(FakeMint::default());
    let seller = PaidSeller::new(
        FakeReceiver::new(&mint),
        ProviderConfig::new(TARIFF, vec![MINT.to_owned()]),
    );
    let wallet = FakeWallet::funded(&mint, 21);
    let policy = ClientPolicy {
        max_tariff: TARIFF,
        topup_sat: 10,
        max_loss_msat: 1_000,
        ..ClientPolicy::default()
    };
    let buyer = PaidBuyer::new(
        "seeder",
        Arc::new(PaymentChannel(seller.clone())),
        wallet.clone(),
        policy,
        AccountState::new(),
    );
    let account = buyer.account_id();
    let deadline = Instant::now() + Duration::from_secs(5);
    let authorised = TARIFF.price_msat(PIECE_BYTES);
    let quote_for = |index: u8| {
        let piece = ContentHash::Sha1([index; 20]);
        match seller.charge(&account, None, piece, PIECE_BYTES, authorised) {
            Sale::Quote(quote) => quote,
            other => panic!("expected a quote, got {other:?}"),
        }
    };

    // Two requests are quoted at a zero balance; the first one's top-up is
    // acknowledged before the second quote is handled.
    let first = buyer.begin(authorised);
    let second = buyer.begin(authorised);
    let (first_quote, late_quote) = (quote_for(1), quote_for(2));
    assert!(buyer.quoted(&first, &first_quote));
    drop(first);
    assert!(buyer.fund(Some(first_quote), deadline).await.unwrap());
    assert_eq!(buyer.account_state().balance_msat, 10_000);

    assert!(buyer.quoted(&second, &late_quote));
    drop(second);
    let state = buyer.account_state();
    assert_eq!(state.balance_msat, 10_000, "the stale balance is ignored");
    assert_eq!(state.blocked, None);
    // The balance already covers the late quote: no second top-up.
    assert!(buyer.fund(Some(late_quote), deadline).await.unwrap());
    assert_eq!(wallet.payments(), 1);
}
