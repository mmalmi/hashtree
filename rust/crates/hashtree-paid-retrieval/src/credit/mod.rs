//! Issuer-local whole-sat credit for an exact SHA-256 blob, on an
//! authenticated link. The service meter is one verified blob.
mod buyer;
mod seller;

pub use buyer::CreditBlobRoute;
pub use peer_payments::{AccountStore as CreditJournal, Keypair as CreditKey};
pub use seller::CreditBlobProvider;

use hashtree_core::{Hash, StoreError};
use peer_payments::{AccountStore, Agreement, Controller, Error, Keypair, Receipt, Signed};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const SERVICE: &str = "hashtree.blob.sha256";
const REQUEST: &[u8] = b"HCR1";
const CONTROL: &[u8] = b"HCA1";
const DATA: &[u8] = b"HCD1";
const MAX_CONTROL: usize = 16 * 1024;
const TTL: u64 = 120;

#[derive(Clone)]
pub struct CreditAccount {
    journal: Arc<Mutex<AccountStore>>,
    key: Arc<Keypair>,
    owner: String,
}

impl CreditAccount {
    /// Share one store with all service and exchange controllers of this identity.
    pub fn new(journal: Arc<Mutex<AccountStore>>, key: Keypair) -> Result<Self, Error> {
        Controller::new(&mut *journal.lock().map_err(|_| Error::Closed)?, &key)?;
        Ok(Self {
            journal,
            owner: key.x_only_public_key().0.to_string(),
            key: Arc::new(key),
        })
    }

    async fn with<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut AccountStore, &Keypair) -> Result<T, Error> + Send + 'static,
    ) -> Result<T, StoreError> {
        let account = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut db = account.journal.lock().map_err(|_| Error::Closed)?;
            f(&mut db, &account.key)
        })
        .await
        .map_err(failed)?
        .map_err(failed)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum Request {
    Quote {
        hash: Hash,
        created_at: u64,
    },
    Fetch {
        hash: Hash,
        offer: Signed<Agreement>,
        accepted: Signed<Agreement>,
    },
    Receipt {
        hash: Hash,
        receipt: Signed<Receipt>,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum Reply {
    Offer(Signed<Agreement>),
    Ack(Signed<Receipt>),
    Miss,
    Refused,
}

fn now() -> Result<u64, StoreError> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(failed)?
        .as_secs())
}

fn failed(e: impl std::fmt::Display) -> StoreError {
    StoreError::Other(format!("peer-credit retrieval: {e}"))
}

fn encode(prefix: &[u8], value: &impl Serialize) -> Result<Vec<u8>, StoreError> {
    let mut bytes = prefix.to_vec();
    serde_json::to_writer(&mut bytes, value).map_err(failed)?;
    if bytes.len() > MAX_CONTROL {
        return Err(failed("oversized control frame"));
    }
    Ok(bytes)
}

fn decode<T: for<'de> Deserialize<'de>>(prefix: &[u8], bytes: &[u8]) -> Result<T, StoreError> {
    if bytes.len() > MAX_CONTROL {
        return Err(failed("oversized control frame"));
    }
    serde_json::from_slice(
        bytes
            .strip_prefix(prefix)
            .ok_or_else(|| failed("wrong control frame"))?,
    )
    .map_err(failed)
}

fn id(payer: &str, provider: &str, hash: &Hash) -> String {
    // Distinct signers in each direction, and one purchase per immutable blob.
    let bytes = format!("{SERVICE}/{payer}/{provider}/{}", hex::encode(hash));
    hex::encode(hashtree_core::sha256(bytes.as_bytes()))
}

fn terms(
    payer: &str,
    provider: &str,
    hash: &Hash,
    created_at: u64,
    price: u64,
) -> Result<Agreement, StoreError> {
    if price == 0 {
        return Err(failed("zero price"));
    }
    Ok(Agreement {
        id: id(payer, provider, hash),
        payer: payer.into(),
        provider: provider.into(),
        service: SERVICE.into(),
        resource: hex::encode(hash),
        max_units: 1,
        price_sat: price,
        price_per_units: 1,
        max_amount_sat: price,
        created_at,
        expires_at: created_at
            .checked_add(TTL)
            .ok_or_else(|| failed("invalid expiry"))?,
    })
}

#[cfg(test)]
mod tests;
