//! One journal record: a bencoded list of ledger entries.

use std::collections::{BTreeMap, VecDeque};

use crate::bencode::{self, Value};
use crate::store::{AccountRecord, LedgerEntry, PendingRedemption};

const PAYMENT_BYTES: usize = 16 + 8;

fn bytes(value: &[u8]) -> Value {
    Value::Bytes(value.to_vec())
}

/// Totals use the whole `u64` range, which a bencode integer cannot.
fn number(value: u64) -> Value {
    bytes(&value.to_be_bytes())
}

fn entry<const N: usize>(kind: &str, fields: [(&str, Value); N]) -> Value {
    let mut dict = BTreeMap::from([(b"t".to_vec(), bytes(kind.as_bytes()))]);
    for (key, value) in fields {
        dict.insert(key.as_bytes().to_vec(), value);
    }
    Value::Dict(dict)
}

fn redemption(kind: &str, pending: &PendingRedemption, credited_msat: Option<u64>) -> Value {
    let mut value = entry(
        kind,
        [
            ("k", bytes(&pending.token)),
            ("a", bytes(&pending.account)),
            ("i", bytes(&pending.payment)),
        ],
    );
    if let (Value::Dict(dict), Some(credited_msat)) = (&mut value, credited_msat) {
        dict.insert(b"v".to_vec(), number(credited_msat));
    }
    value
}

fn encode_entry(item: &LedgerEntry) -> Value {
    match item {
        LedgerEntry::Redeeming(pending) => redemption("redeeming", pending, None),
        LedgerEntry::Abandoned { token } => entry("abandoned", [("k", bytes(token))]),
        LedgerEntry::Credited {
            redemption: pending,
            credited_msat,
        } => redemption("credited", pending, Some(*credited_msat)),
        LedgerEntry::Charged {
            account,
            charged_msat,
        } => entry(
            "charged",
            [("a", bytes(account)), ("c", number(*charged_msat))],
        ),
        LedgerEntry::Removed { account } => entry("removed", [("a", bytes(account))]),
        LedgerEntry::Grace { peer, account } => entry(
            "grace",
            [("p", bytes(peer.as_bytes())), ("a", bytes(account))],
        ),
        LedgerEntry::Account { account, record } => {
            let mut payments = Vec::with_capacity(record.payments.len() * PAYMENT_BYTES);
            for (payment, credited_msat) in &record.payments {
                payments.extend_from_slice(payment);
                payments.extend_from_slice(&credited_msat.to_be_bytes());
            }
            entry(
                "account",
                [
                    ("a", bytes(account)),
                    ("v", number(record.paid_msat)),
                    ("c", number(record.charged_msat)),
                    ("l", Value::Bytes(payments)),
                ],
            )
        }
        LedgerEntry::Token { token } => entry("token", [("k", bytes(token))]),
    }
}

pub(super) fn encode(entries: &[LedgerEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    let list = Value::List(entries.iter().map(encode_entry).collect());
    bencode::encode(&list, &mut out);
    out
}

struct Fields<'a>(&'a BTreeMap<Vec<u8>, Value>);

impl Fields<'_> {
    fn raw(&self, key: &str) -> Option<&[u8]> {
        self.0.get(key.as_bytes())?.as_bytes()
    }

    fn fixed<const N: usize>(&self, key: &str) -> Option<[u8; N]> {
        self.raw(key)?.try_into().ok()
    }

    fn number(&self, key: &str) -> Option<u64> {
        self.fixed(key).map(u64::from_be_bytes)
    }

    fn redemption(&self) -> Option<PendingRedemption> {
        Some(PendingRedemption {
            token: self.fixed("k")?,
            account: self.fixed("a")?,
            payment: self.fixed("i")?,
        })
    }
}

fn decode_entry(value: &Value) -> Option<LedgerEntry> {
    let Value::Dict(dict) = value else {
        return None;
    };
    let fields = Fields(dict);
    Some(match fields.raw("t")? {
        b"redeeming" => LedgerEntry::Redeeming(fields.redemption()?),
        b"abandoned" => LedgerEntry::Abandoned {
            token: fields.fixed("k")?,
        },
        b"credited" => LedgerEntry::Credited {
            redemption: fields.redemption()?,
            credited_msat: fields.number("v")?,
        },
        b"charged" => LedgerEntry::Charged {
            account: fields.fixed("a")?,
            charged_msat: fields.number("c")?,
        },
        b"removed" => LedgerEntry::Removed {
            account: fields.fixed("a")?,
        },
        b"grace" => LedgerEntry::Grace {
            peer: String::from_utf8(fields.raw("p")?.to_vec()).ok()?,
            account: fields.fixed("a")?,
        },
        b"account" => {
            let payments = fields.raw("l")?;
            if payments.len() % PAYMENT_BYTES != 0 {
                return None;
            }
            let payments: Option<VecDeque<_>> = payments
                .chunks_exact(PAYMENT_BYTES)
                .map(|chunk| {
                    let (payment, credited_msat) = chunk.split_at(16);
                    let credited_msat = u64::from_be_bytes(credited_msat.try_into().ok()?);
                    Some((payment.try_into().ok()?, credited_msat))
                })
                .collect();
            LedgerEntry::Account {
                account: fields.fixed("a")?,
                record: AccountRecord {
                    paid_msat: fields.number("v")?,
                    charged_msat: fields.number("c")?,
                    payments: payments?,
                },
            }
        }
        b"token" => LedgerEntry::Token {
            token: fields.fixed("k")?,
        },
        // An entry of a newer version: refuse the log rather than drop it.
        _ => return None,
    })
}

/// `None` when the record is not a list of entries this version knows.
pub(super) fn decode(payload: &[u8]) -> Option<Vec<LedgerEntry>> {
    match bencode::decode_prefix(payload) {
        Ok((Value::List(items), used)) if used == payload.len() => {
            items.iter().map(decode_entry).collect()
        }
        _ => None,
    }
}
