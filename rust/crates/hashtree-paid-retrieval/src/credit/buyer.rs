use super::*;
use crate::PaidBlobTransport;
use async_trait::async_trait;
use hashtree_core::{
    sha256, BlobReply, BlobRequest, BlobRoute, BlobRouteContext, Store, BLOB_MAX_BYTES,
};
use peer_payments::{sign_message, Controller, PurchaseIntent, WireMessage};

/// Register as an optional paid route of the local reader, after free sources.
/// A signed receipt always follows a hash match for the exact requested blob.
#[derive(Clone)]
pub struct CreditBlobRoute {
    account: CreditAccount,
    provider: String,
    transport: Arc<dyn PaidBlobTransport>,
    max_sat: u64,
    cache: Arc<dyn Store>,
}

impl CreditBlobRoute {
    pub fn new(
        account: CreditAccount,
        provider: String,
        transport: Arc<dyn PaidBlobTransport>,
        max_sat: u64,
        cache: Arc<dyn Store>,
    ) -> Result<Self, StoreError> {
        if !transport.authenticates_peer() || max_sat == 0 || provider == account.owner {
            return Err(failed(
                "expected an authenticated provider and positive budget",
            ));
        }
        Ok(Self {
            account,
            provider,
            transport,
            max_sat,
            cache,
        })
    }

    async fn send(&self, request: &Request) -> Result<Vec<u8>, StoreError> {
        self.transport.exchange(encode(REQUEST, request)?).await
    }

    async fn finish(&self, hash: Hash, receipt: Signed<Receipt>) -> Result<(), StoreError> {
        let expected = receipt.payload.clone();
        let response = self.send(&Request::Receipt { hash, receipt }).await?;
        let Reply::Ack(ack) = decode(CONTROL, &response)? else {
            return Err(failed("receipt rejected"));
        };
        if ack.payload != expected {
            return Err(failed("wrong acknowledgement"));
        }
        self.account
            .with(move |db, key| Controller::new(db, key)?.acknowledge(&ack))
            .await
    }

    /// Retry a receipt after a lost reply or restart. Never signs new usage.
    pub async fn reconcile(&self, hash: Hash) -> Result<bool, StoreError> {
        let id = id(&self.account.owner, &self.provider, &hash);
        let receipt = self
            .account
            .with(move |db, key| {
                let Some(r) = db.book().buyer().authorized_receipt(&id).cloned() else {
                    return Ok(None);
                };
                let signature = sign_message(key, &WireMessage::Receipt(r.clone()))?;
                Ok(Some(Signed {
                    payload: r,
                    signature,
                }))
            })
            .await?;
        if let Some(r) = receipt {
            self.finish(hash, r).await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Replay already-authorized final receipts to this configured provider.
    /// Other services and identities continue to use their own meter.
    pub async fn reconcile_pending(&self) -> Result<(), StoreError> {
        let provider = self.provider.clone();
        let hashes = self
            .account
            .with(move |db, _| {
                let buyer = db.book().buyer();
                let mut hashes = Vec::new();
                for receipt in buyer.pending_receipts().filter(|r| r.provider == provider) {
                    let a = buyer
                        .agreement(&receipt.agreement_id)
                        .ok_or(Error::Unknown)?;
                    if a.service == SERVICE {
                        let bytes =
                            hex::decode(&a.resource).map_err(|_| Error::Invalid("blob hash"))?;
                        hashes.push(bytes.try_into().map_err(|_| Error::Invalid("blob hash"))?);
                    }
                }
                Ok(hashes)
            })
            .await?;
        for hash in hashes {
            self.reconcile(hash).await?;
        }
        Ok(())
    }

    async fn fetch(&self, hash: Hash) -> Result<BlobReply, StoreError> {
        if let Some(cached) = self.cache.get(&hash).await? {
            if cached.len() <= BLOB_MAX_BYTES && sha256(&cached) == hash {
                return Ok(BlobReply::Data(cached));
            }
            return Err(failed("invalid cached blob"));
        }
        let id = id(&self.account.owner, &self.provider, &hash);
        let existing = self
            .account
            .with(move |db, _| Ok(db.book().buyer().agreement(&id).cloned()))
            .await?;
        // A lost or concurrent request cannot turn into another purchase.
        let created_at = existing.as_ref().map(|a| a.created_at).unwrap_or(now()?);
        let response = self.send(&Request::Quote { hash, created_at }).await?;
        let offer = match decode(CONTROL, &response)? {
            Reply::Miss => return Ok(BlobReply::NoResult),
            Reply::Offer(offer) => offer,
            _ => return Err(failed("quote declined")),
        };
        let a = &offer.payload;
        let expected = terms(
            &self.account.owner,
            &self.provider,
            &hash,
            created_at,
            a.price_sat,
        )?;
        let max = self.max_sat;
        if a != &expected || a.price_sat > max {
            return Err(failed("quote outside local intent"));
        }
        let to_accept = offer.clone();
        let now = now()?;
        let accepted = self
            .account
            .with(move |db, key| {
                let intent = PurchaseIntent {
                    provider: &expected.provider,
                    service: SERVICE,
                    resource: &expected.resource,
                    units: 1,
                    max_price_sat: max,
                    price_per_units: 1,
                    max_amount_sat: max,
                    max_duration_secs: TTL,
                };
                Controller::new(db, key)?.accept_offer(&to_accept, &intent, now)
            })
            .await?;
        let agreement_id = offer.payload.id.clone();
        let mut bytes = self
            .send(&Request::Fetch {
                hash,
                offer,
                accepted,
            })
            .await?;
        let blob = bytes
            .strip_suffix(DATA)
            .ok_or_else(|| failed("delivery refused or already reserved"))?;
        if blob.len() > BLOB_MAX_BYTES || sha256(blob) != hash {
            return Err(failed("invalid blob delivery"));
        }
        let receipt = self
            .account
            .with(move |db, key| Controller::new(db, key)?.receipt(&agreement_id, 1, true))
            .await?;
        bytes.truncate(bytes.len() - DATA.len());
        // Once the signed receipt is durable, preserve the verified delivery
        // across a lost acknowledgement. Its exact receipt can be replayed.
        self.cache.put(hash, bytes.clone()).await?;
        self.finish(hash, receipt).await?;
        Ok(BlobReply::Data(bytes))
    }
}

#[async_trait]
impl BlobRoute for CreditBlobRoute {
    async fn route(&self, request: BlobRequest) -> Result<BlobReply, StoreError> {
        self.fetch(request.hash).await
    }
    async fn route_with_context(
        &self,
        request: BlobRequest,
        context: BlobRouteContext,
    ) -> Result<BlobReply, StoreError> {
        tokio::time::timeout_at(context.deadline.into(), self.fetch(request.hash))
            .await
            .map_err(failed)?
    }
}
