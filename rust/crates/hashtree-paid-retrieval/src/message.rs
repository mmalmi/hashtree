//! Transport-independent paid-retrieval messages.
//!
//! Each message is one bencoded dictionary; `data` is followed by the raw blob
//! bytes. The same bytes are the payload of a length-prefixed stream frame or
//! of a BitTorrent BEP 10 extended message. See `docs/paid-retrieval.md`.

use std::collections::BTreeMap;

use hashtree_core::{Hash, BLOB_MAX_BYTES, BLOB_MAX_HTL};

use crate::bencode::{self, Value};
use crate::price::{BytePrice, Tariff};

/// Capability name of the exchange on Hashtree transports.
pub const PROTOCOL: &str = "hashtree.paid-blob/1";
/// Name advertised in the BEP 10 extended handshake `m` dictionary.
pub const BEP10_EXTENSION: &str = "ht_pay";
/// Payment method: a Cashu token redeemed by the provider at its mint.
pub const METHOD_CASHU: &str = "cashu";
pub const MAX_HEADER_BYTES: usize = 64 * 1024;
pub const MAX_MESSAGE_BYTES: usize = BLOB_MAX_BYTES + MAX_HEADER_BYTES;

const MAX_MINTS: usize = 16;
const MAX_URL_BYTES: usize = 512;
const MAX_TOKEN_BYTES: usize = 48 * 1024;
const MAX_TEXT_BYTES: usize = 256;

/// Names the paid unit by the hash that proves its delivery.
///
/// Hashtree blobs and BitTorrent v2 blocks are SHA-256; BitTorrent v1 pieces
/// are SHA-1. On the wire the digest is `h` and any algorithm other than
/// SHA-256 is named in `y`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ContentHash {
    Sha256([u8; 32]),
    Sha1([u8; 20]),
}

impl ContentHash {
    pub fn digest(&self) -> &[u8] {
        match self {
            Self::Sha256(digest) => digest,
            Self::Sha1(digest) => digest,
        }
    }

    /// The Hashtree hash, when this names a SHA-256 unit.
    pub fn sha256(&self) -> Option<Hash> {
        match self {
            Self::Sha256(digest) => Some(*digest),
            Self::Sha1(_) => None,
        }
    }

    fn algorithm(&self) -> Option<&'static str> {
        match self {
            Self::Sha256(_) => None,
            Self::Sha1(_) => Some("sha1"),
        }
    }
}

impl From<Hash> for ContentHash {
    fn from(digest: Hash) -> Self {
        Self::Sha256(digest)
    }
}

/// Bearer id of a prepaid balance held by one provider.
pub type AccountId = [u8; 32];
/// Client-chosen idempotency key of one payment.
pub type PaymentId = [u8; 16];

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid paid-retrieval message: {0}")]
pub struct MessageError(pub &'static str);

/// Length of a compressed secp256k1 public key, the form of a lock key.
pub const LOCK_KEY_BYTES: usize = 33;
/// A public key that tokens paid to a provider should be locked to (NUT-11).
pub type LockKey = [u8; LOCK_KEY_BYTES];

/// One payment as it travels: in `pay`, or carried by `get`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Payment {
    /// Chosen at random by the client; repeating it repeats the answer.
    pub id: PaymentId,
    pub method: String,
    pub mint: String,
    pub token: String,
}

/// A payment carried by a `get` that the provider credited before replying.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Credit {
    pub payment: PaymentId,
    pub credited_msat: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// Fetch a blob, authorising a debit of at most `max_msat`.
    Get {
        hash: ContentHash,
        /// The account to charge. `None` on a carrier that authenticates the
        /// client: the account is then the client's identity.
        account: Option<AccountId>,
        max_msat: u64,
        /// How many more times the request may be forwarded. Zero: the
        /// provider answers from what it has.
        htl: u8,
        /// A payment to credit before the blob is charged.
        payment: Option<Payment>,
    },
    /// Credit the account with one payment.
    Pay {
        account: Option<AccountId>,
        payment: Payment,
    },
}

/// The provider's terms for one blob: payment is required before delivery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Quote {
    pub hash: ContentHash,
    pub size: u64,
    pub price_msat: u64,
    pub balance_msat: i64,
    pub grace_msat: u64,
    pub tariff: Tariff,
    pub mints: Vec<String>,
    pub methods: Vec<String>,
    pub min_topup_sat: u64,
    /// The key that tokens paid to this provider should be locked to.
    pub lock: Option<LockKey>,
    /// A payment carried by the request, credited before this quote.
    pub credit: Option<Credit>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorCode {
    /// The request could not be understood.
    Bad,
    /// The mint is not accepted by this provider.
    Mint,
    /// The payment method is not supported.
    Method,
    /// The token was refused: spent, malformed or from another mint.
    Invalid,
    /// The token was already credited under another payment.
    Replay,
    /// Temporary failure; the same request may be repeated.
    Retry,
    /// The provider cannot open another account now.
    Full,
    Unknown,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bad => "bad",
            Self::Mint => "mint",
            Self::Method => "method",
            Self::Invalid => "invalid",
            Self::Replay => "replay",
            Self::Retry => "retry",
            Self::Full => "full",
            Self::Unknown => "unknown",
        }
    }

    fn parse(text: &str) -> Self {
        [
            Self::Bad,
            Self::Mint,
            Self::Method,
            Self::Invalid,
            Self::Replay,
            Self::Retry,
            Self::Full,
        ]
        .into_iter()
        .find(|code| code.as_str() == text)
        .unwrap_or(Self::Unknown)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    Data {
        hash: ContentHash,
        charged_msat: u64,
        balance_msat: i64,
        /// A free repeat of a blob this account was already charged for.
        duplicate: bool,
        /// A payment carried by the request, credited before the charge.
        credit: Option<Credit>,
        data: Vec<u8>,
    },
    Quote(Quote),
    /// This provider has no data for the hash. Not proof of absence.
    NoResult {
        hash: ContentHash,
    },
    Paid {
        payment: PaymentId,
        credited_msat: u64,
        balance_msat: i64,
    },
    Error {
        code: ErrorCode,
        payment: Option<PaymentId>,
        detail: String,
    },
}

impl Reply {
    pub fn error(code: ErrorCode, detail: impl Into<String>) -> Self {
        Self::Error {
            code,
            payment: None,
            detail: detail.into(),
        }
    }
}

#[derive(Default)]
struct Fields(BTreeMap<Vec<u8>, Value>);

impl Fields {
    fn kind(kind: &str) -> Self {
        Self::default().text("t", kind)
    }

    fn put(mut self, key: &str, value: Value) -> Self {
        self.0.insert(key.as_bytes().to_vec(), value);
        self
    }

    fn uint(self, key: &str, value: u64) -> Self {
        self.put(key, Value::Int(i64::try_from(value).unwrap_or(i64::MAX)))
    }

    fn int(self, key: &str, value: i64) -> Self {
        self.put(key, Value::Int(value))
    }

    fn bytes(self, key: &str, value: &[u8]) -> Self {
        self.put(key, Value::Bytes(value.to_vec()))
    }

    fn text(self, key: &str, value: &str) -> Self {
        self.bytes(key, value.as_bytes())
    }

    fn lock(self, lock: &Option<LockKey>) -> Self {
        match lock {
            Some(lock) => self.bytes("o", lock),
            None => self,
        }
    }

    fn account(self, account: &Option<AccountId>) -> Self {
        match account {
            Some(account) => self.bytes("a", account),
            None => self,
        }
    }

    fn payment(self, payment: &Payment) -> Self {
        self.bytes("i", &payment.id)
            .text("k", &payment.method)
            .text("m", &payment.mint)
            .text("v", &payment.token)
    }

    fn credit(self, credit: &Option<Credit>) -> Self {
        match credit {
            Some(credit) => self
                .bytes("i", &credit.payment)
                .uint("v", credit.credited_msat),
            None => self,
        }
    }

    fn content(self, hash: &ContentHash) -> Self {
        let fields = self.bytes("h", hash.digest());
        match hash.algorithm() {
            Some(algorithm) => fields.text("y", algorithm),
            None => fields,
        }
    }

    fn texts(self, key: &str, values: &[String]) -> Self {
        let items = values
            .iter()
            .map(|value| Value::Bytes(value.as_bytes().to_vec()))
            .collect();
        self.put(key, Value::List(items))
    }

    fn finish(self, capacity: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(capacity + 256);
        bencode::encode(&Value::Dict(self.0), &mut out);
        out
    }
}

struct Dict(BTreeMap<Vec<u8>, Value>);

impl Dict {
    fn parse(bytes: &[u8]) -> Result<(Self, usize), MessageError> {
        let header = &bytes[..bytes.len().min(MAX_HEADER_BYTES)];
        match bencode::decode_prefix(header) {
            Ok((Value::Dict(entries), used)) => Ok((Self(entries), used)),
            Ok(_) => Err(MessageError("message is not a dictionary")),
            Err(_) => Err(MessageError("header is not canonical bencode")),
        }
    }

    fn get(&self, key: &'static str) -> Result<&Value, MessageError> {
        self.0
            .get(key.as_bytes())
            .ok_or(MessageError("a required field is missing"))
    }

    fn int(&self, key: &'static str) -> Result<i64, MessageError> {
        self.get(key)?
            .as_int()
            .ok_or(MessageError("a number field has the wrong type"))
    }

    fn uint(&self, key: &'static str) -> Result<u64, MessageError> {
        u64::try_from(self.int(key)?).map_err(|_| MessageError("a number field is negative"))
    }

    fn fixed<const N: usize>(&self, key: &'static str) -> Result<[u8; N], MessageError> {
        self.get(key)?
            .as_bytes()
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or(MessageError("an id or hash field has the wrong length"))
    }

    fn text(&self, key: &'static str, max: usize) -> Result<String, MessageError> {
        text_value(self.get(key)?, max)
    }

    fn has(&self, key: &'static str) -> bool {
        self.0.contains_key(key.as_bytes())
    }

    fn optional_fixed<const N: usize>(
        &self,
        key: &'static str,
    ) -> Result<Option<[u8; N]>, MessageError> {
        match self.has(key) {
            true => self.fixed(key).map(Some),
            false => Ok(None),
        }
    }

    /// `i`, `k`, `m` and `v` together, or none of them.
    fn payment(&self) -> Result<Option<Payment>, MessageError> {
        let present = ["i", "k", "m", "v"].map(|key| self.has(key));
        if present.iter().all(|present| !present) {
            return Ok(None);
        }
        if !present.iter().all(|present| *present) {
            return Err(MessageError("a payment is incomplete"));
        }
        Ok(Some(Payment {
            id: self.fixed("i")?,
            method: self.text("k", MAX_TEXT_BYTES)?,
            mint: self.text("m", MAX_URL_BYTES)?,
            token: self.text("v", MAX_TOKEN_BYTES)?,
        }))
    }

    /// `i` and `v` together, or neither.
    fn credit(&self) -> Result<Option<Credit>, MessageError> {
        match (self.has("i"), self.has("v")) {
            (false, false) => Ok(None),
            (true, true) => Ok(Some(Credit {
                payment: self.fixed("i")?,
                credited_msat: self.uint("v")?,
            })),
            _ => Err(MessageError("a credit is incomplete")),
        }
    }

    /// Absent means zero; zero has no second spelling.
    fn htl(&self) -> Result<u8, MessageError> {
        if !self.has("l") {
            return Ok(0);
        }
        u8::try_from(self.uint("l")?)
            .ok()
            .filter(|htl| (1..=BLOB_MAX_HTL).contains(htl))
            .ok_or(MessageError("the hop budget is out of range"))
    }

    /// SHA-256 has one encoding: `y` is absent, never spelled out.
    fn content(&self) -> Result<ContentHash, MessageError> {
        if !self.0.contains_key(&b"y"[..]) {
            return Ok(ContentHash::Sha256(self.fixed("h")?));
        }
        match self.text("y", MAX_TEXT_BYTES)?.as_str() {
            "sha1" => Ok(ContentHash::Sha1(self.fixed("h")?)),
            _ => Err(MessageError("unsupported hash algorithm")),
        }
    }

    fn texts(&self, key: &'static str) -> Result<Vec<String>, MessageError> {
        let items = self
            .get(key)?
            .as_list()
            .filter(|items| items.len() <= MAX_MINTS)
            .ok_or(MessageError("a list field is invalid"))?;
        items
            .iter()
            .map(|item| text_value(item, MAX_URL_BYTES))
            .collect()
    }
}

fn text_value(value: &Value, max: usize) -> Result<String, MessageError> {
    value
        .as_bytes()
        .filter(|bytes| bytes.len() <= max)
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .map(str::to_owned)
        .ok_or(MessageError("a text field is invalid"))
}

impl Request {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Get {
                hash,
                account,
                max_msat,
                htl,
                payment,
            } => {
                let mut fields = Fields::kind("get")
                    .content(hash)
                    .account(account)
                    .uint("x", *max_msat);
                if *htl > 0 {
                    fields = fields.uint("l", u64::from(*htl));
                }
                match payment {
                    Some(payment) => fields.payment(payment).finish(payment.token.len()),
                    None => fields.finish(0),
                }
            }
            Self::Pay { account, payment } => Fields::kind("pay")
                .account(account)
                .payment(payment)
                .finish(payment.token.len()),
        }
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, MessageError> {
        let (dict, used) = Dict::parse(bytes)?;
        if used != bytes.len() {
            return Err(MessageError("request has trailing bytes"));
        }
        match dict.text("t", MAX_TEXT_BYTES)?.as_str() {
            "get" => Ok(Self::Get {
                hash: dict.content()?,
                account: dict.optional_fixed("a")?,
                max_msat: dict.uint("x")?,
                htl: dict.htl()?,
                payment: dict.payment()?,
            }),
            "pay" => Ok(Self::Pay {
                account: dict.optional_fixed("a")?,
                payment: dict
                    .payment()?
                    .ok_or(MessageError("a required field is missing"))?,
            }),
            _ => Err(MessageError("unknown request type")),
        }
    }
}

impl Reply {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Data {
                hash,
                charged_msat,
                balance_msat,
                duplicate,
                credit,
                data,
            } => {
                let mut fields = Fields::kind("data")
                    .content(hash)
                    .uint("n", data.len() as u64)
                    .uint("c", *charged_msat)
                    .int("b", *balance_msat)
                    .credit(credit);
                if *duplicate {
                    fields = fields.uint("d", 1);
                }
                let mut out = fields.finish(data.len());
                out.extend_from_slice(data);
                out
            }
            Self::Quote(quote) => Fields::kind("quote")
                .content(&quote.hash)
                .credit(&quote.credit)
                .uint("n", quote.size)
                .uint("c", quote.price_msat)
                .int("b", quote.balance_msat)
                .uint("g", quote.grace_msat)
                .put(
                    "r",
                    Value::List(vec![
                        Value::Int(i64::try_from(quote.tariff.rate.msat).unwrap_or(i64::MAX)),
                        Value::Int(i64::try_from(quote.tariff.rate.per_bytes).unwrap_or(i64::MAX)),
                    ]),
                )
                .uint("f", quote.tariff.min_msat)
                .texts("m", &quote.mints)
                .texts("k", &quote.methods)
                .uint("u", quote.min_topup_sat)
                .lock(&quote.lock)
                .finish(0),
            Self::NoResult { hash } => Fields::kind("none").content(hash).finish(0),
            Self::Paid {
                payment,
                credited_msat,
                balance_msat,
            } => Fields::kind("paid")
                .bytes("i", payment)
                .uint("v", *credited_msat)
                .int("b", *balance_msat)
                .finish(0),
            Self::Error {
                code,
                payment,
                detail,
            } => {
                let detail: String = detail.chars().take(MAX_TEXT_BYTES / 4).collect();
                let mut fields = Fields::kind("err")
                    .text("r", code.as_str())
                    .text("d", &detail);
                if let Some(payment) = payment {
                    fields = fields.bytes("i", payment);
                }
                fields.finish(0)
            }
        }
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, MessageError> {
        let (dict, used) = Dict::parse(bytes)?;
        let trailing = &bytes[used..];
        let kind = dict.text("t", MAX_TEXT_BYTES)?;
        if kind != "data" && !trailing.is_empty() {
            return Err(MessageError("reply has trailing bytes"));
        }
        match kind.as_str() {
            "data" => {
                let size = dict.uint("n")?;
                if size > BLOB_MAX_BYTES as u64 || size != trailing.len() as u64 {
                    return Err(MessageError("data length does not match its header"));
                }
                Ok(Self::Data {
                    hash: dict.content()?,
                    charged_msat: dict.uint("c")?,
                    balance_msat: dict.int("b")?,
                    duplicate: dict.0.contains_key(&b"d"[..]) && dict.uint("d")? != 0,
                    credit: dict.credit()?,
                    data: trailing.to_vec(),
                })
            }
            "quote" => {
                let rate = dict
                    .get("r")?
                    .as_list()
                    .and_then(|rate| match rate {
                        [msat, per_bytes] => Some(BytePrice::new(
                            u64::try_from(msat.as_int()?).ok()?,
                            u64::try_from(per_bytes.as_int()?).ok()?,
                        )),
                        _ => None,
                    })
                    .ok_or(MessageError("the tariff is invalid"))?;
                Ok(Self::Quote(Quote {
                    hash: dict.content()?,
                    size: dict.uint("n")?,
                    price_msat: dict.uint("c")?,
                    balance_msat: dict.int("b")?,
                    grace_msat: dict.uint("g")?,
                    tariff: Tariff {
                        rate,
                        min_msat: dict.uint("f")?,
                    },
                    mints: dict.texts("m")?,
                    methods: dict.texts("k")?,
                    min_topup_sat: dict.uint("u")?,
                    lock: dict.optional_fixed("o")?,
                    credit: dict.credit()?,
                }))
            }
            "none" => Ok(Self::NoResult {
                hash: dict.content()?,
            }),
            "paid" => Ok(Self::Paid {
                payment: dict.fixed("i")?,
                credited_msat: dict.uint("v")?,
                balance_msat: dict.int("b")?,
            }),
            "err" => Ok(Self::Error {
                code: ErrorCode::parse(&dict.text("r", MAX_TEXT_BYTES)?),
                payment: match dict.0.contains_key(&b"i"[..]) {
                    true => Some(dict.fixed("i")?),
                    false => None,
                },
                detail: dict.text("d", MAX_TEXT_BYTES)?,
            }),
            _ => Err(MessageError("unknown reply type")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip_with_one_canonical_encoding() {
        let get = Request::Get {
            hash: [7; 32].into(),
            account: Some([9; 32]),
            max_msat: 16_384,
            htl: 0,
            payment: None,
        };
        let encoded = get.encode();
        assert!(encoded.starts_with(b"d1:a32:"));
        assert!(encoded.ends_with(b"1:t3:get1:xi16384ee"));
        assert_eq!(Request::decode(&encoded).unwrap(), get);

        let pay = Request::Pay {
            account: Some([1; 32]),
            payment: Payment {
                id: [2; 16],
                method: METHOD_CASHU.into(),
                mint: "https://mint.example".into(),
                token: "cashuBexample".into(),
            },
        };
        assert_eq!(Request::decode(&pay.encode()).unwrap(), pay);
    }

    #[test]
    fn replies_round_trip_and_data_carries_raw_bytes_after_the_header() {
        let data = Reply::Data {
            hash: [3; 32].into(),
            charged_msat: 12,
            balance_msat: -5,
            duplicate: true,
            data: b"blob bytes".to_vec(),
            credit: None,
        };
        let encoded = data.encode();
        assert!(encoded.ends_with(b"eblob bytes"));
        assert_eq!(Reply::decode(&encoded).unwrap(), data);

        let replies = [
            Reply::Quote(Quote {
                hash: [4; 32].into(),
                size: 2_097_152,
                price_msat: 2_048,
                balance_msat: 100,
                grace_msat: 0,
                tariff: Tariff {
                    rate: BytePrice::new(1, 1024),
                    min_msat: 10,
                },
                mints: vec!["https://mint.example".into()],
                methods: vec![METHOD_CASHU.into()],
                min_topup_sat: 2,
                lock: None,
                credit: None,
            }),
            Reply::NoResult {
                hash: [5; 32].into(),
            },
            Reply::Paid {
                payment: [6; 16],
                credited_msat: 3_000,
                balance_msat: 3_100,
            },
            Reply::Error {
                code: ErrorCode::Replay,
                payment: Some([6; 16]),
                detail: "token already credited".into(),
            },
            Reply::error(ErrorCode::Retry, "mint unreachable"),
        ];
        for reply in replies {
            assert_eq!(Reply::decode(&reply.encode()).unwrap(), reply);
        }
    }

    #[test]
    fn the_hash_algorithm_is_named_on_the_wire() {
        let piece = Request::Get {
            hash: ContentHash::Sha1([b'B'; 20]),
            account: Some([b'A'; 32]),
            max_msat: 1,
            htl: 0,
            payment: None,
        };
        let encoded = piece.encode();
        let wire = |hash: &str, algorithm: &str| {
            format!(
                "d1:a32:{}1:h{}:{hash}1:t3:get1:xi1e{algorithm}e",
                "A".repeat(32),
                hash.len()
            )
            .into_bytes()
        };
        assert_eq!(encoded, wire(&"B".repeat(20), "1:y4:sha1"));
        assert_eq!(Request::decode(&encoded).unwrap(), piece);

        let none = Reply::NoResult {
            hash: ContentHash::Sha1([8; 20]),
        };
        assert_eq!(Reply::decode(&none.encode()).unwrap(), none);

        // SHA-256 is the unnamed default and has no second spelling.
        assert!(Request::decode(&wire(&"B".repeat(32), "")).is_ok());
        assert!(Request::decode(&wire(&"B".repeat(32), "1:y6:sha256")).is_err());
        assert!(Request::decode(&wire(&"B".repeat(20), "")).is_err());
        assert!(Request::decode(&wire(&"B".repeat(32), "1:y4:sha1")).is_err());
        assert!(Request::decode(&wire(&"B".repeat(20), "1:y3:md5")).is_err());
    }

    #[test]
    fn malformed_messages_are_rejected() {
        let mut data = Reply::Data {
            hash: [3; 32].into(),
            charged_msat: 1,
            balance_msat: 0,
            duplicate: false,
            data: vec![1, 2, 3],
            credit: None,
        }
        .encode();
        data.pop();
        assert!(Reply::decode(&data).is_err());

        let mut get = Request::Get {
            hash: [0; 32].into(),
            account: Some([0; 32]),
            max_msat: 0,
            htl: 0,
            payment: None,
        }
        .encode();
        get.push(b'x');
        assert!(Request::decode(&get).is_err());
        assert!(Request::decode(b"d1:t3:gete").is_err());
        assert!(Request::decode(b"d1:a2:xx1:h2:yy1:t3:get1:xi1ee").is_err());
        assert!(Request::decode(b"d1:t4:nopee").is_err());
        assert!(Reply::decode(b"i1e").is_err());
        assert!(Reply::decode(b"d1:h32:").is_err());
    }
}
