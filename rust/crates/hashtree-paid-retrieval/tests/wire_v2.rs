//! The second shape of the exchange: accounts that are identities, hop
//! budgets, payments carried by `get`, and lock keys.

mod common;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use common::{DirectTransport, FakeWallet, Seller, MINT, TARIFF};
use hashtree_core::{BlobReply, BlobRequest, BlobRoute, StoreError};
use hashtree_paid_retrieval::{
    account_for_peer, ClientPolicy, ErrorCode, PaidBlobRoute, PaidBlobTransport, Payment,
    PaymentRequest, Reply, Request, RetrievalWallet, WalletError, WalletPayment,
};

fn policy() -> ClientPolicy {
    ClientPolicy {
        max_tariff: TARIFF,
        mints: vec![MINT.to_owned()],
        topup_sat: 5,
        max_token_fee_sat: 0,
        ..ClientPolicy::default()
    }
}

/// A carrier that authenticates the client as "peer-a", like FIPS.
struct Authenticated(Arc<DirectTransport>);

#[async_trait]
impl PaidBlobTransport for Authenticated {
    async fn exchange(&self, request: Vec<u8>) -> Result<Vec<u8>, StoreError> {
        self.0.exchange(request).await
    }

    fn authenticates_peer(&self) -> bool {
        true
    }
}

#[tokio::test]
async fn a_first_paid_read_is_two_round_trips_with_the_token_riding_along() {
    let seller = Seller::new();
    let (hash, data) = seller.add(1, 4_096).await;
    let wallet = FakeWallet::funded(&seller.mint, 21);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let transport = DirectTransport::tampering(&seller.provider, move |request, reply| {
        let kind = match request {
            Request::Get { payment: None, .. } => "get",
            Request::Get {
                payment: Some(_), ..
            } => "get+pay",
            Request::Pay { .. } => "pay",
        };
        log.lock().unwrap().push(kind);
        Some(reply)
    });
    let route = PaidBlobRoute::new("seller", transport, wallet.clone(), policy());

    let reply = route.route(BlobRequest { hash, htl: 0 }).await.unwrap();
    assert_eq!(reply, BlobReply::Data(data));
    assert_eq!(*seen.lock().unwrap(), vec!["get", "get+pay"]);
    let account = route.account_state();
    assert_eq!(
        (account.credited_msat, account.charged_msat),
        (5_000, 1_000)
    );
    assert_eq!((account.pending.clone(), account.loss_msat()), (None, 0));
}

#[tokio::test]
async fn an_authenticated_client_names_no_account_and_is_charged_as_itself() {
    let seller = Seller::new();
    let (hash, data) = seller.add(1, 4_096).await;
    let wallet = FakeWallet::funded(&seller.mint, 21);
    let mut direct = Arc::into_inner(DirectTransport::tampering(
        &seller.provider,
        |request, reply| {
            let (Request::Get { account, .. } | Request::Pay { account, .. }) = request;
            assert_eq!(*account, None, "no bearer id on an authenticated carrier");
            Some(reply)
        },
    ))
    .unwrap();
    direct.peer = Some("peer-a".into());
    let transport = Arc::new(Authenticated(Arc::new(direct)));
    let route = PaidBlobRoute::new("seller", transport, wallet, policy());

    let reply = route.route(BlobRequest { hash, htl: 0 }).await.unwrap();
    assert_eq!(reply, BlobReply::Data(data));
    let accounts = seller.provider.accounts();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].account, account_for_peer("peer-a"));
    assert_eq!(accounts[0].balance_msat(), 4_000);
}

#[tokio::test]
async fn an_unauthenticated_request_without_an_account_is_refused() {
    let seller = Seller::new();
    let (hash, _) = seller.add(1, 4_096).await;
    let reply = seller
        .ask(Request::Get {
            hash: hash.into(),
            account: None,
            max_msat: u64::MAX,
            htl: 0,
            payment: None,
        })
        .await;
    assert!(matches!(
        reply,
        Reply::Error {
            code: ErrorCode::Bad,
            ..
        }
    ));
}

#[tokio::test]
async fn the_hop_budget_reaches_the_providers_source() {
    struct Recording(Mutex<Vec<u8>>);
    #[async_trait]
    impl BlobRoute for Recording {
        async fn route(&self, request: BlobRequest) -> Result<BlobReply, StoreError> {
            self.0.lock().unwrap().push(request.htl);
            Ok(BlobReply::NoResult)
        }
    }
    let source = Arc::new(Recording(Mutex::new(Vec::new())));
    let mint = Arc::new(common::FakeMint::default());
    let provider = hashtree_paid_retrieval::PaidBlobProvider::new(
        source.clone(),
        common::FakeReceiver::new(&mint),
        hashtree_paid_retrieval::ProviderConfig::new(TARIFF, vec![MINT.to_owned()]),
    );
    let wallet = FakeWallet::funded(&mint, 21);
    let route = PaidBlobRoute::new("seller", DirectTransport::new(&provider), wallet, policy());

    let reply = route
        .route(BlobRequest {
            hash: [9; 32],
            htl: 4,
        })
        .await
        .unwrap();
    assert_eq!(reply, BlobReply::NoResult);
    assert_eq!(*source.0.lock().unwrap(), vec![4]);
}

#[tokio::test]
async fn a_refused_carried_payment_answers_the_whole_request() {
    let seller = Seller::new();
    let (hash, _) = seller.add(1, 4_096).await;
    let spent = seller.mint.issue(MINT, 5);
    seller.mint.redeem(MINT, &spent).unwrap();
    let reply = seller
        .ask(Request::Get {
            hash: hash.into(),
            account: Some([7; 32]),
            max_msat: u64::MAX,
            htl: 0,
            payment: Some(Payment {
                id: [1; 16],
                method: "cashu".into(),
                mint: MINT.into(),
                token: spent,
            }),
        })
        .await;
    assert!(matches!(
        reply,
        Reply::Error {
            code: ErrorCode::Invalid,
            payment: Some(id),
            ..
        } if id == [1; 16]
    ));
    assert!(seller
        .provider
        .accounts()
        .iter()
        .all(|account| account.charged_msat == 0));
}

#[tokio::test]
async fn the_lock_key_in_a_quote_reaches_the_wallet() {
    struct Watching(Arc<FakeWallet>, Mutex<Vec<Option<[u8; 33]>>>);
    #[async_trait]
    impl RetrievalWallet for Watching {
        async fn pay(&self, request: &PaymentRequest) -> Result<WalletPayment, WalletError> {
            self.1.lock().unwrap().push(request.lock);
            self.0.pay(request).await
        }
        async fn balance_sat(&self) -> Result<u64, WalletError> {
            self.0.balance_sat().await
        }
    }
    let seller = Seller::with(|config| config.lock = Some([2; 33]));
    let (hash, data) = seller.add(1, 4_096).await;
    let wallet = Arc::new(Watching(
        FakeWallet::funded(&seller.mint, 21),
        Mutex::default(),
    ));
    let route = PaidBlobRoute::new(
        "seller",
        DirectTransport::new(&seller.provider),
        wallet.clone(),
        policy(),
    );
    let reply = route.route(BlobRequest { hash, htl: 0 }).await.unwrap();
    assert_eq!(reply, BlobReply::Data(data));
    assert_eq!(*wallet.1.lock().unwrap(), vec![Some([2; 33])]);
}
