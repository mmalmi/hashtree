//! The `cashu` feature: the existing `CashuPaymentClient` on both sides.
#![cfg(feature = "cashu")]

mod common;

use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use cashu_service::{CashuMintBalance, CashuPaymentClient, CashuReceivedPayment, CashuSentPayment};
use common::{blob, DirectTransport, FakeMint, MINT, TARIFF};
use hashtree_core::{BlobReply, BlobRequest, BlobRoute, MemoryStore, Store, StoreBlobRoute};
use hashtree_paid_retrieval::{
    CashuReceiver, CashuWallet, ClientPolicy, PaidBlobProvider, PaidBlobRoute, PaymentRequest,
    ProviderConfig, RetrievalWallet, WalletError,
};

/// Stands in for the `htree-cashu` helper: one wallet at one fake mint.
#[derive(Clone)]
struct StubClient {
    mint: Arc<FakeMint>,
    balance_sat: Arc<Mutex<u64>>,
}

#[async_trait]
impl CashuPaymentClient for StubClient {
    async fn send_payment(&self, mint_url: &str, amount_sat: u64) -> Result<CashuSentPayment> {
        let mut balance = self.balance_sat.lock().unwrap();
        *balance = balance
            .checked_sub(amount_sat)
            .ok_or_else(|| anyhow!("insufficient balance"))?;
        let token = self.mint.issue(mint_url, amount_sat);
        Ok(CashuSentPayment {
            mint_url: mint_url.to_owned(),
            unit: "sat".into(),
            amount_sat,
            send_fee_sat: 0,
            operation_id: token.clone(),
            token,
        })
    }

    async fn receive_payment(&self, encoded_token: &str) -> Result<CashuReceivedPayment> {
        for mint_url in [MINT, "https://other.test"] {
            if let Ok(amount_sat) = self.mint.redeem(mint_url, encoded_token) {
                *self.balance_sat.lock().unwrap() += amount_sat;
                return Ok(CashuReceivedPayment {
                    mint_url: mint_url.to_owned(),
                    unit: "sat".into(),
                    amount_sat,
                });
            }
        }
        Err(anyhow!("token is spent or unknown"))
    }

    async fn revoke_payment(&self, mint_url: &str, operation_id: &str) -> Result<()> {
        let amount_sat = self
            .mint
            .redeem(mint_url, operation_id)
            .map_err(|e| anyhow!(e))?;
        *self.balance_sat.lock().unwrap() += amount_sat;
        Ok(())
    }

    async fn mint_balance(&self, mint_url: &str) -> Result<CashuMintBalance> {
        Ok(CashuMintBalance {
            mint_url: mint_url.to_owned(),
            unit: "sat".into(),
            balance_sat: *self.balance_sat.lock().unwrap(),
        })
    }
}

fn client(mint: &Arc<FakeMint>, balance_sat: u64) -> StubClient {
    StubClient {
        mint: mint.clone(),
        balance_sat: Arc::new(Mutex::new(balance_sat)),
    }
}

#[tokio::test]
async fn the_cashu_payment_client_pays_and_receives() {
    let mint = Arc::new(FakeMint::default());
    let buyer = client(&mint, 10);
    let seller_wallet = client(&mint, 0);

    let store = Arc::new(MemoryStore::new());
    let (hash, data) = blob(1, 4_096);
    store.put(hash, data.clone()).await.unwrap();
    let provider = PaidBlobProvider::new(
        Arc::new(StoreBlobRoute::new(store)),
        Arc::new(CashuReceiver::new(seller_wallet.clone())),
        ProviderConfig::new(TARIFF, vec![format!("{MINT}/")]),
    );
    let wallet = Arc::new(CashuWallet::new(buyer.clone(), vec![MINT.to_owned()]));
    let policy = ClientPolicy {
        max_tariff: TARIFF,
        topup_sat: 2,
        ..ClientPolicy::default()
    };
    let route = PaidBlobRoute::new(
        "seller",
        DirectTransport::new(&provider),
        wallet.clone(),
        policy,
    );

    let reply = route.route(BlobRequest { hash, htl: 0 }).await.unwrap();
    assert_eq!(reply, BlobReply::Data(data));
    assert_eq!(wallet.balance_sat().await, Ok(8));
    assert_eq!(*seller_wallet.balance_sat.lock().unwrap(), 2);

    // The wallet pays only with mints it was told to trust.
    let untrusted = PaymentRequest {
        provider: "seller".into(),
        mints: vec!["https://other.test".into()],
        amount_sat: 1,
        max_fee_sat: 0,
        payment_id: [1; 16],
    };
    assert_eq!(
        wallet.pay(&untrusted).await,
        Err(WalletError::NoAcceptedMint)
    );
    let too_much = PaymentRequest {
        provider: "seller".into(),
        mints: vec![MINT.into()],
        amount_sat: 100,
        max_fee_sat: 0,
        payment_id: [2; 16],
    };
    assert_eq!(
        wallet.pay(&too_much).await,
        Err(WalletError::InsufficientFunds)
    );
}

#[tokio::test]
async fn a_token_of_another_mint_is_never_credited() {
    use hashtree_paid_retrieval::{PaymentReceiver, ReceiveError};

    let mint = Arc::new(FakeMint::default());
    let receiver = CashuReceiver::new(client(&mint, 0));
    let foreign = mint.issue("https://other.test", 5);
    assert!(matches!(
        receiver.receive(MINT, &foreign).await,
        Err(ReceiveError::Rejected(_))
    ));
    let good = mint.issue(MINT, 5);
    assert_eq!(receiver.receive(MINT, &good).await, Ok(5));
}
