//! Hashtree glue for the selling side: blobs from a source `BlobRoute`.

use std::sync::Arc;

use hashtree_core::{BlobReply, BlobRequest, BlobRoute, BLOB_MAX_BYTES};

use crate::ledger::AccountBalance;
use crate::message::{AccountId, ContentHash, ErrorCode, Reply, Request};
use crate::seller::{PaidSeller, PaymentReceiver, ProviderConfig, Sale};

/// Answers paid-retrieval requests for Hashtree blobs. Cheap to clone.
///
/// `source` must return hash-verified data, as `StoreBlobRoute` does.
#[derive(Clone)]
pub struct PaidBlobProvider {
    source: Arc<dyn BlobRoute>,
    seller: PaidSeller,
}

impl PaidBlobProvider {
    pub fn new(
        source: Arc<dyn BlobRoute>,
        receiver: Arc<dyn PaymentReceiver>,
        config: ProviderConfig,
    ) -> Self {
        Self {
            source,
            seller: PaidSeller::new(receiver, config),
        }
    }

    /// The accounts and payment handling behind this provider.
    pub fn seller(&self) -> &PaidSeller {
        &self.seller
    }

    /// Every account, for the owner to persist.
    pub fn accounts(&self) -> Vec<AccountBalance> {
        self.seller.accounts()
    }

    /// Load persisted accounts before serving.
    pub fn restore_accounts(&self, accounts: &[AccountBalance]) {
        self.seller.restore_accounts(accounts);
    }

    /// Answer one encoded request with one encoded reply. `peer` is the
    /// transport-authenticated identity of the sender, if there is one.
    pub async fn handle(&self, peer: Option<&str>, request: &[u8]) -> Vec<u8> {
        let reply = match Request::decode(request) {
            Err(error) => Reply::error(ErrorCode::Bad, error.0),
            Ok(Request::Get {
                hash,
                account,
                max_msat,
            }) => self.get(peer, hash, account, max_msat).await,
            Ok(Request::Pay {
                account,
                payment,
                method,
                mint,
                token,
            }) => {
                self.seller
                    .pay(account, payment, &method, &mint, token)
                    .await
            }
        };
        reply.encode()
    }

    async fn get(
        &self,
        peer: Option<&str>,
        content: ContentHash,
        account: AccountId,
        max_msat: u64,
    ) -> Reply {
        // Hashtree blobs are addressed by SHA-256 only.
        let Some(hash) = content.sha256() else {
            return Reply::NoResult { hash: content };
        };
        let data = match self.source.route(BlobRequest { hash, htl: 0 }).await {
            Ok(BlobReply::Data(data)) if data.len() <= BLOB_MAX_BYTES => data,
            Ok(_) => return Reply::NoResult { hash: content },
            Err(_) => return Reply::error(ErrorCode::Retry, "the blob source failed"),
        };
        match self
            .seller
            .charge(&account, peer, content, data.len() as u64, max_msat)
        {
            Sale::Charged {
                charged_msat,
                balance_msat,
                duplicate,
            } => Reply::Data {
                hash: content,
                charged_msat,
                balance_msat,
                duplicate,
                data,
            },
            Sale::Quote(quote) => Reply::Quote(quote),
            Sale::Full => Reply::error(ErrorCode::Full, "no room for another account"),
        }
    }
}
