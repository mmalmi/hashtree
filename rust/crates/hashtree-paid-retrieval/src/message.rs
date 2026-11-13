//! Transport-independent paid-retrieval messages.
//!
//! Each message is one bencoded dictionary; `data` is followed by the raw blob
//! bytes. The same bytes are the payload of a length-prefixed stream frame or
//! of a BitTorrent BEP 10 extended message. See `docs/paid-retrieval.md`.

use std::collections::BTreeMap;

use hashtree_core::{Hash, BLOB_MAX_BYTES};

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

/// Bearer id of a prepaid balance held by one provider.
pub type AccountId = [u8; 32];
/// Client-chosen idempotency key of one payment.
pub type PaymentId = [u8; 16];

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid paid-retrieval message: {0}")]
pub struct MessageError(pub &'static str);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// Fetch a blob, authorising a debit of at most `max_msat`.
    Get {
        hash: Hash,
        account: AccountId,
        max_msat: u64,
    },
    /// Credit the account with one payment.
    Pay {
        account: AccountId,
        payment: PaymentId,
        method: String,
        mint: String,
        token: String,
    },
}

/// The provider's terms for one blob: payment is required before delivery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Quote {
    pub hash: Hash,
    pub size: u64,
    pub price_msat: u64,
    pub balance_msat: i64,
    pub grace_msat: u64,
    pub tariff: Tariff,
    pub mints: Vec<String>,
    pub methods: Vec<String>,
    pub min_topup_sat: u64,
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
        hash: Hash,
        charged_msat: u64,
        balance_msat: i64,
        /// A free repeat of a blob this account was already charged for.
        duplicate: bool,
        data: Vec<u8>,
    },
    Quote(Quote),
    /// This provider has no data for the hash. Not proof of absence.
    NoResult {
        hash: Hash,
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
            } => Fields::kind("get")
                .bytes("h", hash)
                .bytes("a", account)
                .uint("x", *max_msat)
                .finish(0),
            Self::Pay {
                account,
                payment,
                method,
                mint,
                token,
            } => Fields::kind("pay")
                .bytes("a", account)
                .bytes("i", payment)
                .text("k", method)
                .text("m", mint)
                .text("v", token)
                .finish(token.len()),
        }
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, MessageError> {
        let (dict, used) = Dict::parse(bytes)?;
        if used != bytes.len() {
            return Err(MessageError("request has trailing bytes"));
        }
        match dict.text("t", MAX_TEXT_BYTES)?.as_str() {
            "get" => Ok(Self::Get {
                hash: dict.fixed("h")?,
                account: dict.fixed("a")?,
                max_msat: dict.uint("x")?,
            }),
            "pay" => Ok(Self::Pay {
                account: dict.fixed("a")?,
                payment: dict.fixed("i")?,
                method: dict.text("k", MAX_TEXT_BYTES)?,
                mint: dict.text("m", MAX_URL_BYTES)?,
                token: dict.text("v", MAX_TOKEN_BYTES)?,
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
                data,
            } => {
                let mut fields = Fields::kind("data")
                    .bytes("h", hash)
                    .uint("n", data.len() as u64)
                    .uint("c", *charged_msat)
                    .int("b", *balance_msat);
                if *duplicate {
                    fields = fields.uint("d", 1);
                }
                let mut out = fields.finish(data.len());
                out.extend_from_slice(data);
                out
            }
            Self::Quote(quote) => Fields::kind("quote")
                .bytes("h", &quote.hash)
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
                .finish(0),
            Self::NoResult { hash } => Fields::kind("none").bytes("h", hash).finish(0),
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
                    hash: dict.fixed("h")?,
                    charged_msat: dict.uint("c")?,
                    balance_msat: dict.int("b")?,
                    duplicate: dict.0.contains_key(&b"d"[..]) && dict.uint("d")? != 0,
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
                    hash: dict.fixed("h")?,
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
                }))
            }
            "none" => Ok(Self::NoResult {
                hash: dict.fixed("h")?,
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
            hash: [7; 32],
            account: [9; 32],
            max_msat: 16_384,
        };
        let encoded = get.encode();
        assert!(encoded.starts_with(b"d1:a32:"));
        assert!(encoded.ends_with(b"1:t3:get1:xi16384ee"));
        assert_eq!(Request::decode(&encoded).unwrap(), get);

        let pay = Request::Pay {
            account: [1; 32],
            payment: [2; 16],
            method: METHOD_CASHU.into(),
            mint: "https://mint.example".into(),
            token: "cashuBexample".into(),
        };
        assert_eq!(Request::decode(&pay.encode()).unwrap(), pay);
    }

    #[test]
    fn replies_round_trip_and_data_carries_raw_bytes_after_the_header() {
        let data = Reply::Data {
            hash: [3; 32],
            charged_msat: 12,
            balance_msat: -5,
            duplicate: true,
            data: b"blob bytes".to_vec(),
        };
        let encoded = data.encode();
        assert!(encoded.ends_with(b"eblob bytes"));
        assert_eq!(Reply::decode(&encoded).unwrap(), data);

        let replies = [
            Reply::Quote(Quote {
                hash: [4; 32],
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
            }),
            Reply::NoResult { hash: [5; 32] },
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
    fn malformed_messages_are_rejected() {
        let mut data = Reply::Data {
            hash: [3; 32],
            charged_msat: 1,
            balance_msat: 0,
            duplicate: false,
            data: vec![1, 2, 3],
        }
        .encode();
        data.pop();
        assert!(Reply::decode(&data).is_err());

        let mut get = Request::Get {
            hash: [0; 32],
            account: [0; 32],
            max_msat: 0,
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
