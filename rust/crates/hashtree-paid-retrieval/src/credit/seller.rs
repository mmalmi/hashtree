use super::*;
use hashtree_core::{sha256, Store, BLOB_MAX_BYTES};
use peer_payments::Controller;

#[derive(Clone)]
pub struct CreditBlobProvider {
    local: Arc<dyn Store>,
    account: CreditAccount,
    price_sat: u64,
}

impl CreditBlobProvider {
    pub fn new(
        local: Arc<dyn Store>,
        account: CreditAccount,
        price_sat: u64,
    ) -> Result<Self, StoreError> {
        if price_sat == 0 {
            return Err(failed("zero price"));
        }
        Ok(Self {
            local,
            account,
            price_sat,
        })
    }

    /// The transport supplies the authenticated x-only identity. Do not bind
    /// this entry point to a bearer-account or an unauthenticated transport.
    pub async fn handle(&self, peer: &str, request: &[u8]) -> Vec<u8> {
        match self.exchange(peer, request).await {
            Ok(reply) => reply,
            Err(_) => encode(CONTROL, &Reply::Refused).expect("fixed reply"),
        }
    }

    async fn exchange(&self, peer: &str, request: &[u8]) -> Result<Vec<u8>, StoreError> {
        let peer = peer.to_owned();
        let owner = &self.account.owner;
        let reply = match decode(REQUEST, request)? {
            Request::Quote { hash, created_at } => {
                let agreement = terms(&peer, owner, &hash, created_at, self.price_sat)?;
                // Check credit policy before letting a stranger query local inventory.
                let now = now()?;
                let quote = self
                    .account
                    .with(move |db, key| {
                        if db.book().seller().available_credit_sat(&agreement.payer)?
                            < agreement.price_sat
                        {
                            return Err(Error::Limit);
                        }
                        Controller::new(db, key)?.offer(agreement, now)
                    })
                    .await?;
                if self.local.has(&hash).await? {
                    Reply::Offer(quote)
                } else {
                    Reply::Miss
                }
            }
            Request::Fetch {
                hash,
                offer,
                accepted,
            } => {
                if offer.payload
                    != terms(
                        &peer,
                        owner,
                        &hash,
                        offer.payload.created_at,
                        self.price_sat,
                    )?
                {
                    return Err(failed("mismatched service terms"));
                }
                let now = now()?;
                let mut allowance = self
                    .account
                    .with(move |db, key| {
                        Controller::new(db, key)?
                            .open(&offer, &accepted, now)?
                            .allowance
                            .ok_or(Error::Closed)
                    })
                    .await?;
                let mut blob = self
                    .local
                    .get(&hash)
                    .await?
                    .ok_or_else(|| failed("unavailable blob"))?;
                if blob.len() > BLOB_MAX_BYTES
                    || sha256(&blob) != hash
                    || !allowance.admit(1, super::now()?)
                {
                    return Err(failed("invalid blob or revoked credit"));
                }
                blob.extend_from_slice(DATA);
                return Ok(blob);
            }
            Request::Receipt { hash, receipt } => {
                let expected_id = id(&peer, owner, &hash);
                let resource = hex::encode(hash);
                if receipt.payload.agreement_id != expected_id
                    || receipt.payload.payer != peer
                    || receipt.payload.provider != *owner
                    || receipt.payload.cumulative_units != 1
                    || !receipt.payload.final_receipt
                {
                    return Err(failed("wrong blob receipt"));
                }
                let ack = self
                    .account
                    .with(move |db, key| {
                        let a = db
                            .book()
                            .seller()
                            .agreement(&expected_id)
                            .ok_or(Error::Unknown)?;
                        if a.service != SERVICE
                            || a.resource != resource
                            || a.payer != peer
                            || a.max_units != 1
                            || a.commitment()? != receipt.payload.agreement_commitment
                        {
                            return Err(Error::Authentication);
                        }
                        Controller::new(db, key)?.receive_receipt(&receipt, 1)
                    })
                    .await?;
                Reply::Ack(ack)
            }
        };
        encode(CONTROL, &reply)
    }
}
