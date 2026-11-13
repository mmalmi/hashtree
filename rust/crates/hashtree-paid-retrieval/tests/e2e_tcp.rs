//! End to end over a real loopback TCP connection: a client with a test
//! wallet reads through a `BlobRouter` from a provider that serves only
//! against payment.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{FakeWallet, Seller, MINT, TARIFF};
use hashtree_core::{MemoryStore, Store};
use hashtree_network::{BlobRouteEntry, BlobRouter, BlobRouterConfig};
use hashtree_paid_retrieval::{
    serve_stream, BoxedStream, ClientPolicy, LimitedWallet, PaidBlobRoute, SpendingLimit,
    StreamTransport,
};
use tokio::net::{TcpListener, TcpStream};

const TWO_MIB: usize = 2 * 1024 * 1024;

#[tokio::test]
async fn a_client_with_a_wallet_retrieves_blobs_from_a_paying_only_provider() {
    let seller = Seller::new();
    let (first, first_data) = seller.add(1, TWO_MIB).await;
    let (second, second_data) = seller.add(2, 300_000).await;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let provider = seller.provider.clone();
    let server = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let provider = provider.clone();
            tokio::spawn(async move { serve_stream(&provider, None, stream).await });
        }
    });

    let funded = FakeWallet::funded(&seller.mint, 50);
    let limit = SpendingLimit::new(10);
    let wallet = Arc::new(LimitedWallet::new(funded.clone(), limit.clone()));
    let transport = Arc::new(StreamTransport::new(move || async move {
        let stream = TcpStream::connect(address).await?;
        Ok(Box::new(stream) as BoxedStream)
    }));
    let policy = ClientPolicy {
        max_tariff: TARIFF,
        mints: vec![MINT.to_owned()],
        topup_sat: 4,
        ..ClientPolicy::default()
    };
    let paid = PaidBlobRoute::new("seller", transport, wallet, policy);

    // The client's own store is empty; the paid provider is its only route.
    let cache = Arc::new(MemoryStore::new());
    let router = BlobRouter::new(
        vec![BlobRouteEntry::new("paid:seller", Arc::new(paid.clone()))],
        Some(cache.clone()),
        BlobRouterConfig {
            request_timeout: Duration::from_secs(20),
            ..BlobRouterConfig::default()
        },
    )
    .unwrap();

    assert_eq!(
        router.get(&first, None).await.unwrap(),
        Some(first_data.clone())
    );
    assert_eq!(router.get(&second, None).await.unwrap(), Some(second_data));
    assert_eq!(router.get(&[9; 32], None).await.unwrap(), None);

    // One 4 sat top-up paid for both: 2048 msat and 1000 msat (the minimum).
    assert_eq!(funded.payments(), 1);
    assert_eq!(funded.balance(), 46);
    assert_eq!((limit.spent_sat(), limit.remaining_sat()), (4, 6));
    assert_eq!(seller.receiver.received(), 4);
    let account = paid.account_state();
    assert_eq!(account.credited_msat, 4_000);
    assert_eq!(account.charged_msat, 3_048);
    assert_eq!(account.balance_msat, 952);
    assert_eq!(account.loss_msat(), 0);
    let sold = seller.provider.accounts();
    assert_eq!((sold[0].paid_msat, sold[0].charged_msat), (4_000, 3_048));

    // The router wrote the verified blobs to the client's own store.
    assert_eq!(cache.get(&first).await.unwrap(), Some(first_data));

    server.abort();
}
