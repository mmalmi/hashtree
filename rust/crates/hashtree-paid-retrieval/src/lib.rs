//! Pay-to-retrieve for Hashtree blobs.
//!
//! The payment exchange is independent of Hashtree's routing, of its carrier
//! and of the hash that names a unit: [`PaidSeller`] keeps accounts, prices
//! and payments, [`PaidBuyer`] keeps the client's ledger and pays through an
//! application's [`RetrievalWallet`], [`PaidBlobTransport`] moves one request
//! and one reply, and the messages fit a BitTorrent BEP 10 extension message.
//!
//! The Hashtree glue is thin: [`PaidBlobProvider`] sells blobs from a source
//! `BlobRoute`, and [`PaidBlobRoute`] is the matching opaque `BlobRoute`. The
//! published blob wire and `BlobRouter` are unchanged.
//!
//! Design, failure handling and open decisions: `docs/paid-retrieval.md`.

mod account;
pub mod bencode;
mod buyer;
#[cfg(feature = "cashu")]
mod cashu;
mod client;
mod funding;
mod ledger;
mod message;
mod price;
mod provider;
mod seller;
mod transport;
mod wallet;

pub use account::{AccountState, ClientPolicy, PaidRouteStats, PendingPayment};
pub use buyer::{InFlight, PaidBuyer};
#[cfg(feature = "cashu")]
pub use cashu::{CashuReceiver, CashuWallet};
pub use client::PaidBlobRoute;
pub use ledger::AccountBalance;
pub use message::{
    AccountId, ContentHash, ErrorCode, MessageError, PaymentId, Quote, Reply, Request,
    BEP10_EXTENSION, MAX_HEADER_BYTES, MAX_MESSAGE_BYTES, METHOD_CASHU, PROTOCOL,
};
pub use price::{BytePrice, Tariff};
pub use provider::PaidBlobProvider;
pub use seller::{PaidSeller, PaymentReceiver, ProviderConfig, ReceiveError, Sale};
pub use transport::{
    read_frame, serve_stream, write_frame, BoxedStream, ByteStream, PaidBlobTransport,
    StreamTransport,
};
pub use wallet::{
    LimitedWallet, PaymentRequest, RetrievalWallet, SpendingLimit, WalletError, WalletPayment,
};
