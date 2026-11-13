//! A buyer's account as bytes, and a file to keep them in.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::PathBuf;

use crate::account::{AccountState, AccountStore, PaymentIntent, PendingPayment};
use crate::bencode::{self, Value};
use crate::wallet::WalletPayment;

fn invalid(what: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what)
}

fn dict(entries: Vec<(&str, Value)>) -> Value {
    Value::Dict(
        entries
            .into_iter()
            .map(|(key, value)| (key.as_bytes().to_vec(), value))
            .collect(),
    )
}

fn number(value: u64) -> Value {
    Value::Int(i64::try_from(value).unwrap_or(i64::MAX))
}

fn text(value: &str) -> Value {
    Value::Bytes(value.as_bytes().to_vec())
}

struct Fields<'a>(&'a BTreeMap<Vec<u8>, Value>);

impl<'a> Fields<'a> {
    fn of(value: &'a Value) -> io::Result<Self> {
        match value {
            Value::Dict(entries) => Ok(Self(entries)),
            _ => Err(invalid("account record is not a dictionary")),
        }
    }

    fn get(&self, key: &str) -> Option<&'a Value> {
        self.0.get(key.as_bytes())
    }

    fn need(&self, key: &str) -> io::Result<&'a Value> {
        self.get(key).ok_or(invalid("account field is missing"))
    }

    fn number(&self, key: &str) -> io::Result<u64> {
        let value = self.need(key)?.as_int();
        value
            .and_then(|value| u64::try_from(value).ok())
            .ok_or(invalid("account number is invalid"))
    }

    fn text(&self, key: &str) -> io::Result<String> {
        text_of(self.need(key)?)
    }

    fn fixed<const N: usize>(&self, key: &str) -> io::Result<[u8; N]> {
        let bytes = self.need(key)?.as_bytes();
        bytes
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or(invalid("account id has the wrong length"))
    }
}

fn text_of(value: &Value) -> io::Result<String> {
    value
        .as_bytes()
        .and_then(|bytes| String::from_utf8(bytes.to_vec()).ok())
        .ok_or(invalid("account text is invalid"))
}

fn payment_value(pending: &PendingPayment) -> Value {
    let mut entries = vec![
        ("a", number(pending.payment.amount_sat)),
        ("f", number(pending.payment.fee_sat)),
        ("i", Value::Bytes(pending.id.to_vec())),
        ("m", text(&pending.payment.mint_url)),
        ("n", number(u64::from(pending.attempts))),
        ("t", text(&pending.payment.token)),
    ];
    if let Some(reclaim_id) = &pending.payment.reclaim_id {
        entries.push(("r", text(reclaim_id)));
    }
    dict(entries)
}

fn payment_of(value: &Value) -> io::Result<PendingPayment> {
    let fields = Fields::of(value)?;
    Ok(PendingPayment {
        id: fields.fixed("i")?,
        attempts: u32::try_from(fields.number("n")?).unwrap_or(u32::MAX),
        payment: WalletPayment {
            mint_url: fields.text("m")?,
            token: fields.text("t")?,
            amount_sat: fields.number("a")?,
            fee_sat: fields.number("f")?,
            reclaim_id: fields.get("r").map(text_of).transpose()?,
        },
    })
}

impl AccountState {
    /// The account as bytes, for any store. They contain bearer secrets.
    pub fn encode(&self) -> Vec<u8> {
        let mut entries = vec![
            ("a", Value::Bytes(self.account.to_vec())),
            ("b", Value::Int(self.balance_msat)),
            ("c", number(self.credited_msat)),
            ("g", number(self.charged_msat)),
            (
                "u",
                Value::List(self.unresolved.iter().map(payment_value).collect()),
            ),
            ("v", Value::Int(1)),
        ];
        if let Some(blocked) = &self.blocked {
            entries.push(("k", text(blocked)));
        }
        if let Some(pending) = &self.pending {
            entries.push(("p", payment_value(pending)));
        }
        if let Some(intent) = &self.intent {
            let mints = intent.mints.iter().map(|mint| text(mint)).collect();
            entries.push((
                "i",
                dict(vec![
                    ("a", number(intent.amount_sat)),
                    ("i", Value::Bytes(intent.id.to_vec())),
                    ("m", Value::List(mints)),
                ]),
            ));
        }
        let mut out = Vec::new();
        bencode::encode(&dict(entries), &mut out);
        out
    }

    pub fn decode(bytes: &[u8]) -> io::Result<Self> {
        let (value, used) =
            bencode::decode_prefix(bytes).map_err(|_| invalid("account record is damaged"))?;
        let fields = Fields::of(&value)?;
        if used != bytes.len() || fields.number("v")? != 1 {
            return Err(invalid("account record has an unknown format"));
        }
        let intent = match fields.get("i") {
            None => None,
            Some(value) => {
                let intent = Fields::of(value)?;
                let mints = intent.need("m")?.as_list();
                let mints = mints.ok_or(invalid("account mints are invalid"))?;
                Some(PaymentIntent {
                    id: intent.fixed("i")?,
                    amount_sat: intent.number("a")?,
                    mints: mints.iter().map(text_of).collect::<io::Result<_>>()?,
                })
            }
        };
        let unresolved = fields.need("u")?.as_list();
        let unresolved = unresolved.ok_or(invalid("account payments are invalid"))?;
        Ok(Self {
            account: fields.fixed("a")?,
            credited_msat: fields.number("c")?,
            charged_msat: fields.number("g")?,
            balance_msat: fields
                .need("b")?
                .as_int()
                .ok_or(invalid("account balance is invalid"))?,
            intent,
            pending: fields.get("p").map(payment_of).transpose()?,
            unresolved: unresolved
                .iter()
                .map(payment_of)
                .collect::<io::Result<_>>()?,
            blocked: fields.get("k").map(text_of).transpose()?,
        })
    }
}

/// Keeps one account in one file, replaced atomically on every save.
pub struct FileAccountStore {
    path: PathBuf,
}

impl FileAccountStore {
    /// `path` is the account file; its directory must exist. Keep it private:
    /// the file holds the account's bearer id and unredeemed tokens.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The saved account, or `None` when nothing was saved yet.
    pub fn load(&self) -> io::Result<Option<AccountState>> {
        match fs::read(&self.path) {
            Ok(bytes) => AccountState::decode(&bytes).map(Some),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn write(&self, account: &AccountState) -> io::Result<()> {
        let mut temporary = self.path.clone().into_os_string();
        temporary.push(".tmp");
        let temporary = PathBuf::from(temporary);
        let mut file = File::create(&temporary)?;
        file.write_all(&account.encode())?;
        file.sync_all()?;
        fs::rename(&temporary, &self.path)?;
        #[cfg(unix)]
        if let Some(dir) = self.path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
            File::open(dir)?.sync_all()?;
        }
        Ok(())
    }
}

impl AccountStore for FileAccountStore {
    fn save(&self, account: &AccountState) -> Result<(), String> {
        self.write(account).map_err(|error| error.to_string())
    }
}
