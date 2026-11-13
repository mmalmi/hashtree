//! Adapters for the existing `cashu_service::CashuPaymentClient`, which the
//! `htree-cashu` helper implements.

use async_trait::async_trait;
use cashu_service::CashuPaymentClient;

use crate::seller::{normalize_mint, PaymentReceiver, ReceiveError};
use crate::wallet::{PaymentRequest, RetrievalWallet, WalletError, WalletPayment};

fn failed(error: impl std::fmt::Display) -> WalletError {
    WalletError::Failed(error.to_string())
}

/// Pays providers from a Cashu wallet, using only the mints it was given.
pub struct CashuWallet<C> {
    client: C,
    mints: Vec<String>,
}

impl<C> CashuWallet<C> {
    /// `mints` are the mints this wallet trusts, in order of preference.
    pub fn new(client: C, mints: Vec<String>) -> Self {
        Self { client, mints }
    }
}

#[async_trait]
impl<C: CashuPaymentClient> RetrievalWallet for CashuWallet<C> {
    async fn pay(&self, request: &PaymentRequest) -> Result<WalletPayment, WalletError> {
        let accepted = |own: &&String| {
            let wanted = request.mints.iter();
            wanted
                .map(|mint| normalize_mint(mint))
                .any(|mint| mint == normalize_mint(own))
        };
        let mut trusted = false;
        for mint in self.mints.iter().filter(accepted) {
            trusted = true;
            let balance = self.client.mint_balance(mint).await.map_err(failed)?;
            if balance.balance_sat < request.amount_sat {
                continue;
            }
            let sent = self
                .client
                .send_payment(mint, request.amount_sat)
                .await
                .map_err(failed)?;
            return Ok(WalletPayment {
                mint_url: sent.mint_url,
                token: sent.token,
                amount_sat: sent.amount_sat,
                fee_sat: sent.send_fee_sat,
                reclaim_id: Some(sent.operation_id),
            });
        }
        Err(match trusted {
            true => WalletError::InsufficientFunds,
            false => WalletError::NoAcceptedMint,
        })
    }

    async fn balance_sat(&self) -> Result<u64, WalletError> {
        let mut total = 0u64;
        for mint in &self.mints {
            let balance = self.client.mint_balance(mint).await.map_err(failed)?;
            total = total.saturating_add(balance.balance_sat);
        }
        Ok(total)
    }

    async fn reclaim(&self, payment: &WalletPayment) -> Result<(), WalletError> {
        let operation = payment
            .reclaim_id
            .as_deref()
            .ok_or_else(|| failed("payment has no reclaim id"))?;
        self.client
            .revoke_payment(&payment.mint_url, operation)
            .await
            .map_err(failed)
    }
}

/// Redeems client tokens into the provider's Cashu wallet.
///
/// The payment client redeems a token before its mint is known here, so a
/// token of another mint is refused only afterwards: it is never credited, but
/// the wallet has contacted that mint. Run the provider's wallet with network
/// access limited to its accepted mints.
pub struct CashuReceiver<C> {
    client: C,
}

impl<C> CashuReceiver<C> {
    pub fn new(client: C) -> Self {
        Self { client }
    }
}

#[async_trait]
impl<C: CashuPaymentClient> PaymentReceiver for CashuReceiver<C> {
    async fn receive(&self, mint_url: &str, token: &str) -> Result<u64, ReceiveError> {
        let received = self
            .client
            .receive_payment(token)
            .await
            .map_err(|error| ReceiveError::Rejected(error.to_string()))?;
        if received.unit != "sat" || normalize_mint(&received.mint_url) != normalize_mint(mint_url)
        {
            return Err(ReceiveError::Rejected(
                "token belongs to another mint or unit".into(),
            ));
        }
        Ok(received.amount_sat)
    }
}
