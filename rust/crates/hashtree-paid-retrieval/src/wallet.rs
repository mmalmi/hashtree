//! The wallet an application plugs into a paid route, and a spending limit.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::message::{LockKey, PaymentId};

/// "Pay this much to this provider."
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentRequest {
    /// Application-level name of the provider, for the wallet's records.
    pub provider: String,
    /// Mints the provider accepts, already narrowed by the client policy. The
    /// wallet must pay only with a mint it trusts itself.
    pub mints: Vec<String>,
    pub amount_sat: u64,
    /// The most that creating the token may cost beyond `amount_sat`.
    pub max_fee_sat: u64,
    /// The key the provider asks tokens to be locked to (NUT-11 P2PK). A
    /// wallet that can lock tokens should; one that cannot sends them unlocked.
    pub lock: Option<LockKey>,
    /// Stable id of this payment. It was saved before the wallet was asked,
    /// and a wallet that can find a payment again finds it by this id.
    pub payment_id: PaymentId,
}

/// A token handed to the provider.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalletPayment {
    pub mint_url: String,
    pub token: String,
    pub amount_sat: u64,
    /// What creating the token cost on top of `amount_sat`.
    pub fee_sat: u64,
    /// Wallet handle for taking an unredeemed token back.
    pub reclaim_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WalletError {
    #[error("spending limit reached: {requested_sat} sat requested, {remaining_sat} sat left")]
    LimitReached {
        requested_sat: u64,
        remaining_sat: u64,
    },
    #[error("the wallet trusts none of the provider's mints")]
    NoAcceptedMint,
    #[error("the wallet balance is too low")]
    InsufficientFunds,
    #[error("wallet failure: {0}")]
    Failed(String),
}

impl WalletError {
    /// The wallet chose not to pay; nothing is wrong with it or the provider.
    pub fn is_decline(&self) -> bool {
        !matches!(self, Self::Failed(_))
    }
}

#[async_trait]
pub trait RetrievalWallet: Send + Sync {
    /// Create a token of exactly `amount_sat` for the provider, or refuse.
    /// Asked again with the same `payment_id`, a wallet that keeps its
    /// payments should return the same token instead of a second one.
    async fn pay(&self, request: &PaymentRequest) -> Result<WalletPayment, WalletError>;

    /// The token created for `payment_id`, if this wallet created one and can
    /// still produce it. Asked after a crash between `pay` and the buyer's
    /// own record of the token.
    async fn recover(&self, _payment_id: &PaymentId) -> Result<Option<WalletPayment>, WalletError> {
        Ok(None)
    }

    /// Spendable balance, for display and for refusing early.
    async fn balance_sat(&self) -> Result<u64, WalletError>;

    /// Take back a token the provider did not redeem.
    async fn reclaim(&self, _payment: &WalletPayment) -> Result<(), WalletError> {
        Err(WalletError::Failed(
            "this wallet cannot reclaim tokens".into(),
        ))
    }
}

/// Where a [`SpendingLimit`] keeps what was spent, so that a restart does
/// not hand the budget back. `save_spent` must be durable when it returns.
pub trait SpendStore: Send + Sync {
    fn save_spent(&self, spent_sat: u64) -> Result<(), String>;
}

struct LimitState {
    limit_sat: u64,
    spent_sat: u64,
    store: Option<Arc<dyn SpendStore>>,
}

impl LimitState {
    /// Move to `spent_sat`, saving first when the amount grows: a payment is
    /// never made on a reservation that a crash would forget.
    fn set_spent(&mut self, spent_sat: u64) -> Result<(), WalletError> {
        if let Some(store) = &self.store {
            let saved = store.save_spent(spent_sat);
            if spent_sat > self.spent_sat {
                saved.map_err(|error| {
                    WalletError::Failed(format!("cannot save the spending record: {error}"))
                })?;
            }
        }
        self.spent_sat = spent_sat;
        Ok(())
    }
}

/// A total spending cap shared by any number of routes.
pub struct SpendingLimit(Mutex<LimitState>);

impl SpendingLimit {
    pub fn new(limit_sat: u64) -> Arc<Self> {
        Self::with_spent(limit_sat, 0)
    }

    /// Restore a limit whose `spent_sat` the application persisted.
    pub fn with_spent(limit_sat: u64, spent_sat: u64) -> Arc<Self> {
        Arc::new(Self(Mutex::new(LimitState {
            limit_sat,
            spent_sat,
            store: None,
        })))
    }

    /// A limit that saves what was spent before each payment is made.
    pub fn with_store(limit_sat: u64, spent_sat: u64, store: Arc<dyn SpendStore>) -> Arc<Self> {
        Arc::new(Self(Mutex::new(LimitState {
            limit_sat,
            spent_sat,
            store: Some(store),
        })))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, LimitState> {
        self.0.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    pub fn limit_sat(&self) -> u64 {
        self.state().limit_sat
    }

    pub fn set_limit_sat(&self, limit_sat: u64) {
        self.state().limit_sat = limit_sat;
    }

    pub fn spent_sat(&self) -> u64 {
        self.state().spent_sat
    }

    pub fn remaining_sat(&self) -> u64 {
        let state = self.state();
        state.limit_sat.saturating_sub(state.spent_sat)
    }

    /// Start a new period, for example a new month.
    pub fn reset_spent(&self) {
        let _ = self.state().set_spent(0);
    }

    fn reserve(&self, amount_sat: u64) -> Result<(), WalletError> {
        let mut state = self.state();
        let remaining_sat = state.limit_sat.saturating_sub(state.spent_sat);
        if amount_sat > remaining_sat {
            return Err(WalletError::LimitReached {
                requested_sat: amount_sat,
                remaining_sat,
            });
        }
        let spent_sat = state.spent_sat + amount_sat;
        state.set_spent(spent_sat)
    }

    /// Replace a reservation with what the payment really cost.
    fn settle(&self, reserved_sat: u64, actual_sat: u64) {
        let mut state = self.state();
        let spent_sat = state
            .spent_sat
            .saturating_sub(reserved_sat)
            .saturating_add(actual_sat);
        let _ = state.set_spent(spent_sat);
    }

    fn release(&self, amount_sat: u64) {
        self.settle(amount_sat, 0);
    }
}

/// Any wallet, held to a [`SpendingLimit`].
///
/// The amount and the fee ceiling are reserved before the wallet is asked, so
/// concurrent routes cannot overspend together and a fee cannot push spending
/// past the limit. The reservation is then replaced by the real cost.
pub struct LimitedWallet<W> {
    wallet: W,
    limit: Arc<SpendingLimit>,
}

impl<W> LimitedWallet<W> {
    pub fn new(wallet: W, limit: Arc<SpendingLimit>) -> Self {
        Self { wallet, limit }
    }

    pub fn limit(&self) -> &Arc<SpendingLimit> {
        &self.limit
    }
}

#[async_trait]
impl<W: RetrievalWallet> RetrievalWallet for LimitedWallet<W> {
    async fn pay(&self, request: &PaymentRequest) -> Result<WalletPayment, WalletError> {
        let reserved_sat = request.amount_sat.saturating_add(request.max_fee_sat);
        self.limit.reserve(reserved_sat)?;
        match self.wallet.pay(request).await {
            Ok(payment) => {
                let cost_sat = payment.amount_sat.saturating_add(payment.fee_sat);
                self.limit.settle(reserved_sat, cost_sat);
                Ok(payment)
            }
            Err(error) => {
                self.limit.release(reserved_sat);
                Err(error)
            }
        }
    }

    async fn recover(&self, payment_id: &PaymentId) -> Result<Option<WalletPayment>, WalletError> {
        self.wallet.recover(payment_id).await
    }

    async fn balance_sat(&self) -> Result<u64, WalletError> {
        self.wallet.balance_sat().await
    }

    async fn reclaim(&self, payment: &WalletPayment) -> Result<(), WalletError> {
        self.wallet.reclaim(payment).await?;
        self.limit.release(payment.amount_sat);
        Ok(())
    }
}

#[async_trait]
impl<W: RetrievalWallet + ?Sized> RetrievalWallet for Arc<W> {
    async fn pay(&self, request: &PaymentRequest) -> Result<WalletPayment, WalletError> {
        (**self).pay(request).await
    }

    async fn recover(&self, payment_id: &PaymentId) -> Result<Option<WalletPayment>, WalletError> {
        (**self).recover(payment_id).await
    }

    async fn balance_sat(&self) -> Result<u64, WalletError> {
        (**self).balance_sat().await
    }

    async fn reclaim(&self, payment: &WalletPayment) -> Result<(), WalletError> {
        (**self).reclaim(payment).await
    }
}
