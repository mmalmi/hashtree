//! Pay-to-retrieve for Hashtree blobs.
//!
//! A [`PaidBlobProvider`] serves blobs against prepaid account balances; a
//! [`PaidBlobRoute`] is the matching opaque `BlobRoute` that pays through an
//! application's [`RetrievalWallet`]. The exchange is independent of its
//! carrier: [`PaidBlobTransport`] moves one request and one reply, and the
//! same messages fit a BitTorrent BEP 10 extension message. The published
//! blob wire and `BlobRouter` are unchanged.
//!
//! Design, failure handling and open decisions: `docs/paid-retrieval.md`.

mod account;
pub mod bencode;
#[cfg(feature = "cashu")]
mod cashu;
mod client;
mod funding;
mod ledger;
mod message;
mod price;
mod provider;
mod transport;
mod wallet;

pub use account::{AccountState, ClientPolicy, PaidRouteStats, PendingPayment};
#[cfg(feature = "cashu")]
pub use cashu::{CashuReceiver, CashuWallet};
pub use client::PaidBlobRoute;
pub use ledger::AccountBalance;
pub use message::{
    AccountId, ErrorCode, MessageError, PaymentId, Quote, Reply, Request, BEP10_EXTENSION,
    MAX_HEADER_BYTES, MAX_MESSAGE_BYTES, METHOD_CASHU, PROTOCOL,
};
pub use price::{BytePrice, Tariff};
pub use provider::{PaidBlobProvider, PaymentReceiver, ProviderConfig, ReceiveError};
pub use transport::{
    read_frame, serve_stream, write_frame, BoxedStream, ByteStream, PaidBlobTransport,
    StreamTransport,
};
pub use wallet::{
    LimitedWallet, PaymentRequest, RetrievalWallet, SpendingLimit, WalletError, WalletPayment,
};
