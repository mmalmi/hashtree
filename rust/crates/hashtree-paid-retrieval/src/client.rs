//! Hashtree glue for the buying side: one opaque `BlobRoute` per provider.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use hashtree_core::{
    sha256, BlobReply, BlobRequest, BlobRoute, BlobRouteContext, Hash, StoreError, BLOB_MAX_BYTES,
};

use crate::account::{AccountState, AccountStore, ClientPolicy, PaidRouteStats};
use crate::buyer::{InFlight, PaidBuyer};
use crate::message::{ContentHash, Quote, Reply, Request};
use crate::transport::PaidBlobTransport;
use crate::wallet::RetrievalWallet;

/// A get that keeps asking for money has not converged; give up on the read.
const MAX_ROUNDS: usize = 4;

enum Accepted {
    Reply(BlobReply),
    Quote(Quote),
}

/// Fetches blobs from one paying-only provider.
///
/// Register it with a `BlobRouter` like any other route. A read the client
/// declines to pay for is `NoResult`; [`PaidBlobRoute::stats`] says why.
#[derive(Clone)]
pub struct PaidBlobRoute {
    buyer: PaidBuyer,
}

impl PaidBlobRoute {
    /// `provider` is the application's name for the seller, passed to the
    /// wallet with every payment.
    pub fn new(
        provider: impl Into<String>,
        transport: Arc<dyn PaidBlobTransport>,
        wallet: Arc<dyn RetrievalWallet>,
        policy: ClientPolicy,
    ) -> Self {
        Self::with_account(provider, transport, wallet, policy, AccountState::new())
    }

    /// Continue with an account the application persisted.
    pub fn with_account(
        provider: impl Into<String>,
        transport: Arc<dyn PaidBlobTransport>,
        wallet: Arc<dyn RetrievalWallet>,
        policy: ClientPolicy,
        account: AccountState,
    ) -> Self {
        Self {
            buyer: PaidBuyer::new(provider, transport, wallet, policy, account),
        }
    }

    /// A route that saves its account in `store`, as [`PaidBuyer::with_store`]
    /// does. Use it wherever real money is paid.
    pub fn with_store(
        provider: impl Into<String>,
        transport: Arc<dyn PaidBlobTransport>,
        wallet: Arc<dyn RetrievalWallet>,
        policy: ClientPolicy,
        account: AccountState,
        store: Arc<dyn AccountStore>,
    ) -> Self {
        Self {
            buyer: PaidBuyer::with_store(provider, transport, wallet, policy, account, store),
        }
    }

    /// The account and payment handling behind this route.
    pub fn buyer(&self) -> &PaidBuyer {
        &self.buyer
    }

    /// The account as it should be persisted.
    pub fn account_state(&self) -> AccountState {
        self.buyer.account_state()
    }

    pub fn stats(&self) -> PaidRouteStats {
        self.buyer.stats()
    }

    /// Resend an unacknowledged payment and pay any debt now.
    pub async fn settle(&self) -> Result<(), StoreError> {
        self.buyer.settle().await
    }

    /// Pay this provider again after a block, writing off the recorded loss.
    pub fn clear_block(&self) {
        self.buyer.clear_block();
    }

    async fn fetch(&self, hash: Hash, deadline: Instant) -> Result<BlobReply, StoreError> {
        let buyer = &self.buyer;
        let max_msat = buyer.policy().max_tariff.price_msat(BLOB_MAX_BYTES as u64);
        for _ in 0..MAX_ROUNDS {
            if !buyer.prepare(deadline).await? {
                return Ok(BlobReply::NoResult);
            }
            let accepted = {
                let in_flight = buyer.begin(max_msat);
                let request = Request::Get {
                    hash: hash.into(),
                    account: buyer.account_id(),
                    max_msat,
                };
                let reply = buyer.exchange(&request, deadline).await?;
                self.accept(&in_flight, hash, reply)?
            };
            match accepted {
                Accepted::Reply(reply) => return Ok(reply),
                Accepted::Quote(quote) => {
                    if !buyer.fund(Some(quote), deadline).await? {
                        return Ok(BlobReply::NoResult);
                    }
                }
            }
        }
        Err(StoreError::Other(
            "paid route provider kept asking for payment".into(),
        ))
    }

    /// Check one reply to a get while that get still counts as in flight.
    fn accept(&self, request: &InFlight, hash: Hash, reply: Reply) -> Result<Accepted, StoreError> {
        let protocol = |what: &str| StoreError::Other(format!("paid route provider {what}"));
        let content = ContentHash::Sha256(hash);
        match reply {
            Reply::NoResult { hash: replied } if replied == content => {
                Ok(Accepted::Reply(BlobReply::NoResult))
            }
            Reply::Data {
                hash: replied,
                charged_msat,
                balance_msat,
                duplicate,
                data,
            } if replied == content => {
                if data.len() > BLOB_MAX_BYTES || sha256(&data) != hash {
                    self.buyer.undelivered(request, balance_msat);
                    return Err(protocol("returned content with the wrong hash"));
                }
                self.buyer.delivered(
                    request,
                    content,
                    data.len() as u64,
                    charged_msat,
                    balance_msat,
                    duplicate,
                );
                Ok(Accepted::Reply(BlobReply::Data(data)))
            }
            Reply::Quote(quote) if quote.hash == content => {
                let affordable = self.buyer.quoted(request, &quote);
                if quote.size > BLOB_MAX_BYTES as u64 {
                    return Err(protocol("quoted an oversized blob"));
                }
                match affordable {
                    true => Ok(Accepted::Quote(quote)),
                    false => Ok(Accepted::Reply(BlobReply::NoResult)),
                }
            }
            Reply::Error { code, detail, .. } => Err(protocol(&format!(
                "refused the request ({}): {detail}",
                code.as_str()
            ))),
            _ => Err(protocol("sent an unexpected reply")),
        }
    }
}

#[async_trait]
impl BlobRoute for PaidBlobRoute {
    async fn route(&self, request: BlobRequest) -> Result<BlobReply, StoreError> {
        let deadline = Instant::now() + self.buyer.policy().request_timeout;
        self.fetch(request.hash, deadline).await
    }

    async fn route_with_context(
        &self,
        request: BlobRequest,
        context: BlobRouteContext,
    ) -> Result<BlobReply, StoreError> {
        self.fetch(request.hash, context.deadline).await
    }
}
