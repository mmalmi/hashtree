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
mod account_store;
pub mod bencode;
mod books;
mod buyer;
#[cfg(feature = "cashu")]
mod cashu;
mod client;
#[cfg(feature = "fips")]
mod fips;
mod funding;
mod journal;
mod ledger;
mod message;
mod price;
mod provider;
mod seller;
mod store;
mod transport;
mod wallet;

pub use account::{
    AccountState, AccountStore, ClientPolicy, PaidRouteStats, PaymentIntent, PendingPayment,
};
pub use account_store::FileAccountStore;
pub use buyer::{InFlight, PaidBuyer};
#[cfg(feature = "cashu")]
pub use cashu::{CashuReceiver, CashuWallet};
pub use client::PaidBlobRoute;
#[cfg(feature = "fips")]
pub use fips::{
    discovered_paid_providers, FipsPaidBlobService, FipsPaidConfig, FipsPaidError,
    FipsPaidTransport, PAID_BLOB_CAPABILITY, PAID_BLOB_SERVICE_PORT,
};
pub use journal::FileLedgerStore;
pub use ledger::AccountBalance;
pub use message::{
    AccountId, ContentHash, Credit, ErrorCode, LockKey, MessageError, Payment, PaymentId, Quote,
    Reply, Request, BEP10_EXTENSION, LOCK_KEY_BYTES, MAX_HEADER_BYTES, MAX_MESSAGE_BYTES,
    METHOD_CASHU, PROTOCOL,
};
pub use price::{BytePrice, Tariff};
pub use provider::PaidBlobProvider;
pub use seller::{
    account_for_peer, PaidSeller, PaymentReceiver, ProviderConfig, ReceiveError, Sale,
};
pub use store::{
    AccountRecord, LedgerEntry, LedgerState, LedgerStore, PendingRedemption, TokenHash,
};
pub use transport::{
    frame_length, frame_prefix, read_frame, serve_stream, write_frame, BoxedStream, ByteStream,
    PaidBlobTransport, StreamTransport, FRAME_PREFIX_BYTES,
};
pub use wallet::{
    LimitedWallet, PaymentRequest, RetrievalWallet, SpendStore, SpendingLimit, WalletError,
    WalletPayment,
};
