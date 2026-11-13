# Paid retrieval

A provider may ask to be paid for serving blobs, and a client may pay and fetch
them. The Rust implementation is `hashtree-paid-retrieval`: one opaque
`PaidBlobRoute` for the client, a `PaidBlobProvider` for the seller, and a small
wallet interface for the application. The published `BlobRequest`/`BlobReply`
wire, HTL and `BlobRouter` are unchanged; the paid exchange is its own service
(`hashtree.paid-blob/1`), carried between nodes over FIPS, and the provider
keeps its accounts on disk.

The payment exchange does not depend on Hashtree routing, on the carrier, or on
the hash function that names what is sold; see [Reusable parts](#reusable-parts).

## What the retired design got wrong

The 2026-03 DataQuote/DataPayment/DataChunk protocol (removed 2026-07-17) is not
restored. Its problems, and what replaces each:

| Retired design | This design |
|---|---|
| Added five message types to the shared blob wire and lived inside the mesh peer selector. | Separate service behind one route; the blob wire and router are untouched. |
| Split a blob into unverifiable sub-chunks and paid one Cashu token per chunk, so every 32 KiB cost two mint round trips and a 5 s settlement timer. | The paid unit is a whole blob, checked against its hash. Payments top up a balance, so most reads cost no mint call. |
| Quote ids, pending quotes, expected settlements and outgoing transfers kept on both sides, keyed by peer and expiring in 1.5 s. | No quote state. A request carries the largest debit it authorises; a provider that wants more answers with its terms. |
| Whole-sat flat offer per blob, whatever its size. | Millisat ledger priced per byte. |
| Pay after delivery, protected only by peer reputation. | Each side sets its own exposure: the client a prepaid cap, the provider an optional grace. |
| Peer-suggested mints with growing trust caps. | The provider lists its mints; the client's wallet pays only with mints it already trusts. |
| Never called by a production read path. | Usable as an ordinary `BlobRouteEntry`. |

## The exchange

The client holds an account with each provider: a random 32-byte id that the
provider maps to a balance in millisats. Balance is everything paid minus
everything charged.

```text
client                                   provider
  get {h, a, x}            ──────────►   has blob, price ≤ x, balance covers it
                           ◄──────────   data {h, n, c, b} + n raw bytes

  get {h, a, x}            ──────────►   balance too low, or price > x
                           ◄──────────   quote {h, n, c, b, g, r, f, m, k, u}
  pay {a, i, k, m, v}      ──────────►   redeems the token at the mint
                           ◄──────────   paid {i, v, b}
  get {h, a, x}            ──────────►
                           ◄──────────   data …
```

`none {h}` means only that this provider has no data for the hash (it becomes
`BlobReply::NoResult`). `err {r, d, i?}` reports `bad`, `mint`, `method`,
`invalid`, `replay`, `retry` or `full`.

| Key | Meaning |
|---|---|
| `t` | message type |
| `h` | digest that names the unit and proves its delivery |
| `y` | hash function of `h` when it is not SHA-256; `sha1` today |
| `a` | 32-byte account id |
| `x` | largest debit in msat the client authorises for this blob |
| `n` | blob size in bytes |
| `c` | price of this blob in msat (in `data`: the amount charged) |
| `b` | account balance in msat after the message, may be negative |
| `d` | in `data`, 1 when this is a free repeat of an already charged blob |
| `g` | grace: msat the provider serves before payment |
| `r`, `f` | tariff: `[msat, per_bytes]` and the minimum price per blob |
| `m` | mint URL (`pay`), accepted mint URLs (`quote`) |
| `k` | payment method (`pay`), accepted methods (`quote`); `cashu` today |
| `u` | smallest top-up the provider accepts, in sat |
| `i` | 16-byte payment id chosen by the client |
| `v` | the Cashu token (`pay`), msat credited (`paid`) |

### Unit and price

The unit paid for is one hash-named piece of data. On Hashtree that is one blob,
at most 16 MiB, named by its SHA-256. Its price is
`max(f, ceil(n × msat / per_bytes))`. Cashu tokens are whole sats; a token of
`s` sat credits `1000 × s` msat, and the remainder stays on the account for
later blobs. A provider with a zero tariff serves without an account.

### The hash is the delivery proof

The client counts a charge only after the received bytes hash to `h`. The hash
function belongs to the unit, not to the exchange: SHA-256 for Hashtree blobs
and BitTorrent v2 blocks (the default, with `y` absent), SHA-1 for BitTorrent
v1 pieces (`y` = `sha1`). Whoever fetches the unit verifies it; the exchange
only carries the name. The client keeps its own
ledger (credited, charged for verified blobs) and compares it with the balance
the provider reports. A balance lower than delivered data explains is a loss;
once losses pass `max_loss_msat` the route stops paying that provider. Wrong
bytes are never returned or cached and are never counted as a delivery.

### Replay and double spending

- The mint is the double-spend authority: the provider credits only what its
  wallet actually redeemed, and only at a mint it lists.
- `i` makes a payment idempotent. Repeating a `pay` returns the recorded result
  and never redeems or credits twice.
- The provider remembers token hashes, so one token cannot be credited under a
  second payment id or account; a token being redeemed is refused meanwhile.
- A redemption runs to completion even when the connection drops, so a redeemed
  token is always credited.
- Payment ids and token hashes are on disk before `paid` is sent, so neither
  guard is lost by a restart; see [Provider ledger](#provider-ledger).
- The account id is a bearer secret. Carry it only over an encrypted transport,
  or keep balances small where that is not possible.

### Paid, but no data

- **The `paid` reply is lost.** The client keeps the token as a pending payment
  and sends the same `pay` again before paying anything new.
- **The connection drops before or during `data`.** The balance is still on the
  account. A blob that was charged is served again free for a short window
  (`redelivery_window`, twice at most).
- **The provider rejects the token** (`mint`, `method`, `invalid`, `replay`).
  The client asks its wallet to reclaim it.
- **The provider restarts.** The balance, the credited payment ids and the
  token hashes are restored. The same `pay` is answered from the record. A
  blob that was charged but not delivered is not free again: the window of
  free repeats is kept in memory only.
- **The provider crashes while it redeems.** The redemption was recorded
  before the mint was asked. When the client sends its `pay` again, the token
  is redeemed, or, if the mint already counts it as spent, credited once the
  provider's wallet confirms that it received it.
- **The provider keeps the money.** Nothing recovers it. One top-up never puts
  more than `max_prepaid_msat` at risk, the ledger comparison above detects the
  loss, and past `max_loss_msat` the provider is not paid again until the
  application clears the block.

### Spending caps

Three independent bounds, all on the client:

1. `ClientPolicy.max_tariff`: a quote above it is declined, not paid.
2. `ClientPolicy.max_prepaid_msat`: the most a single provider may hold.
3. The wallet's own limit. `LimitedWallet` wraps any wallet with a
   `SpendingLimit` (total sats, adjustable, readable for a settings screen).

A declined read returns `NoResult`, so the router continues to other routes;
`PaidBlobRoute::stats()` says why (price, limit, mint, blocked provider).

`BlobRouter` hedges across routes without regard to cost. A paid route listed
beside free ones can therefore pay for a blob that a slower free route would
have delivered; until the router has a cost tier, try paid routes only after
the free routes returned nothing.

### Prepay or pay after delivery

The wire does not choose. With `g = 0` (the default) the provider serves only
from a positive balance, so the client prepays. A provider on an authenticated
transport may set a grace: it then serves first and the client's next request
settles the debt. Grace is bound to one account per authenticated peer. Over
FIPS that peer is the identity FIPS authenticated, and the binding is kept
across restarts, so a restart grants no peer a second grace.

## Wallet interface

```rust
trait RetrievalWallet {
    async fn pay(&self, request: &PaymentRequest) -> Result<WalletPayment, WalletError>;
    async fn balance_sat(&self) -> Result<u64, WalletError>;
    async fn reclaim(&self, payment: &WalletPayment) -> Result<(), WalletError>;
}
```

`PaymentRequest` names the provider, the mints the provider accepts and the
amount in sat. An application implements `pay` and `balance_sat`, and either
enforces its own limit or wraps the wallet in `LimitedWallet`. With the `cashu`
feature, `CashuWallet` and `CashuReceiver` adapt the existing
`cashu_service::CashuPaymentClient` (the `htree-cashu` helper). Lightning is
used only where Cashu needs it: funding a wallet and cashing out at the mint.

## Encoding and transports

Every message is one bencoded dictionary with the keys above; `data` is followed
by the raw blob bytes, as in BEP 9 `ut_metadata`. A carrier only has to deliver
one request and one reply:

- **Byte streams** (TCP, TCP/FIPS): each message is prefixed by its length as a
  32-bit big-endian integer. `StreamTransport` and `serve_stream` implement it.
- **FIPS**: `FipsPaidBlobService` (feature `fips`) binds the exchange as its
  own TCP/FIPS service on port `39020` with the capability
  `hashtree.paid-blob/1`, beside the free blob service (`hashtree.blob/1`,
  port `39018`), which is untouched. See [Over FIPS](#over-fips).
- **BitTorrent (BEP 10)**: advertise `ht_pay` in the extended handshake `m`
  dictionary; each message is the payload of one extended message, which
  already has the same length prefix. Two uses:
  - *Blob mode*: `get`/`data` carry any hash-named block. A BitTorrent v2
    16 KiB leaf block has the same SHA-256 as the equal hashtree blob, so one
    account serves both networks.
  - *Gate mode*: `quote`, `pay` and `paid` are sent as they are, while pieces
    travel in native `request`/`piece` messages. An unpaid peer stays choked,
    `h` names the piece (with `y` = `sha1` for a v1 piece, `n` its length), and
    the piece hash check is the delivery proof.

## Over FIPS

A provider calls `FipsPaidBlobService::serve` or `serve_advertised` on its FIPS
endpoint, a client `FipsPaidBlobService::client`. `transport_to(peer)` is the
`PaidBlobTransport` of one `PaidBlobRoute`; a node that serves can buy through
the same service.

- FIPS authenticates and encrypts the session, so the account id is not
  exposed, and the provider passes the peer's identity to
  `PaidBlobProvider::handle`.
- A capability reaches only peers on the same host, as for free blobs
  (`discovered_paid_providers`). A provider elsewhere is named by the
  application.
- A connection carries any number of exchanges. The client keeps it for half
  the idle timeout and repeats a request once on a new connection when a kept
  one turns out to be dead; requests are idempotent.
- Bounds: a request is at most 64 KiB, a provider seats 16 connections and 4
  per peer, and a client opens at most 4 to one provider and queues the rest.
  A connection without progress for `idle_timeout` (30 s by default, enough to
  redeem a token) is closed. A timeout or a closed connection is an error,
  never `none`.

The carrier is a feature of `hashtree-paid-retrieval`, not a part of
`hashtree-fips-transport`: the published transport crate stays free of payment
code, a build that only needs the exchange (the BitTorrent glue, a stream
carrier) pulls in no FIPS stack, and no further crate has to be versioned.

## Provider ledger

`PaidBlobProvider::open` and `PaidSeller::open` take a `LedgerStore`;
`FileLedgerStore` keeps it in a directory of its own. Without a store the
ledger is in memory, as before.

| Kept on disk | When |
|---|---|
| A redemption about to start: token hash, account, payment id | before the mint is asked |
| A credit: amount, payment id, token hash, as one entry | before `paid` is sent |
| Charged totals, dropped accounts, grace bindings | within a second, and on shutdown |

Memory is updated only after a credit is written, so `paid` is never sent for
a credit that a restart would forget. A charge counts at once and is written
behind: a crash can forgive the charges of its last second, which costs the
provider and never the client. Free repeats of a charged blob are not kept.

What a crash leaves, by the point it hits:

| Crash | After the restart |
|---|---|
| before the redemption is recorded | nothing happened; the token is unspent |
| after that, before the mint redeemed | the client's `pay` redeems it |
| after the mint redeemed, before the credit is written | the mint refuses the token; the provider asks its wallet (`PaymentReceiver::already_received`) and credits what it received |
| after the credit is written | the client's `pay` is answered from the record |

A token with an open redemption stays with the payment that started it; no
other account can claim it. If the wallet has no record of receiving it, it is
not credited: the client's word is not enough, since the client may have spent
it elsewhere. A credit that could not be written (a full disk) is written by
the next `pay` without asking the mint again.

The file is a magic line and checksummed records, one per write, each synced
before the write returns. A record cut short by a crash is dropped on open;
damage anywhere else stops the provider instead of dropping credits. The log is
rewritten through a temporary file and a rename once it has grown well past
its state. One directory is open in one provider at a time.

### Why a file, not `cashu-credit` or LMDB

- **`cashu-credit`** (0.3.0) counts whole sats, and a blob costs millisats.
  Storing msat in its sat fields would misstate a snapshot that other tools
  read, and rounding prices up to a sat is a thousandfold change. It keeps
  every consumption under its own id, so an account would grow with every
  blob. Grace would have to become its peer credit, which is built on signed
  receipts that this exchange does not have. And it leaves to the store the
  one check needed across accounts, that a token backs one account only. Its
  deposit ids are the payment ids here; that part is small enough to keep.
- **LMDB** is transactional and already in the workspace, but through a
  vendored `heed` with process-wide environment bookkeeping that lives in
  `hashtree-lmdb`, and it would put a C library into a crate that is meant to
  be reused without Hashtree's storage.
- **A file** needs only the standard library. The ledger is small (at most
  `max_accounts` accounts and 16,384 token hashes) and has one writer, so an
  append and a sync per payment is all the durability it takes.

Prices and balances stay in millisats. `LedgerStore` is two calls (`load`,
`append`), so a daemon that prefers its own database can supply one.

## Reusable parts

The crate is split so that a later BitTorrent version reuses the payment
exchange as it is and replaces only the glue.

| Reusable as it is | What it does |
|---|---|
| `Request`, `Reply`, `Quote`, `ContentHash`, `bencode` | the messages and their one canonical encoding |
| `PaidSeller` | prices, account balances, grace, free repeats, payment ids and token replay protection, redemption through a `PaymentReceiver` |
| `PaidBuyer` | the client ledger, quote decisions, top-ups, pending-payment resend, loss detection and blocking |
| `RetrievalWallet`, `LimitedWallet`, `SpendingLimit`, `CashuWallet`, `CashuReceiver` | the wallet boundary and the spending limit |
| `PaidBlobTransport`, `StreamTransport`, `serve_stream` | one request and one reply over any carrier |
| `LedgerStore`, `LedgerEntry`, `LedgerState`, `FileLedgerStore` | the seller's ledger across restarts |
| `FipsPaidBlobService`, `FipsPaidTransport` (feature `fips`) | the same request and reply between FIPS peers |

Hashtree glue, about 300 lines: `PaidBlobProvider` (reads blobs from a
`BlobRoute`, asks the seller, answers `get`) and `PaidBlobRoute` (asks the
buyer, sends `get`, checks SHA-256). `tests/gate_mode.rs` drives the seller and
buyer with SHA-1 names and no `BlobRoute` at all.

A BitTorrent v1 implementation is better done as its own task. It would add:

- A BEP 10 extension in the libtorrent bridge of `hashtree-bittorrent`, which
  has no peer-plugin hooks today: advertise `ht_pay`, pass extended messages to
  and from the seller and buyer.
- Seeder glue: keep a peer choked until `PaidSeller::charge` says `Charged`,
  with one account per connection. Charge per piece, not per 16 KiB request,
  because SHA-1 verifies only whole pieces.
- Downloader glue: answer a `quote` through `PaidBuyer`, call `delivered` when
  a piece passes its hash check and `undelivered` when it fails. A v1 piece can
  come from several peers, so a failed piece must be attributed to the peer
  that sent the bad blocks before anyone is blamed.
- A decision on account secrecy: BitTorrent connections are not authenticated,
  so balances should stay small, with no grace.
- Interoperability tests against an ordinary client that ignores the
  extension.

## Shared with fips pay-to-relay

Shared: the price shape (`msat` per `per_bytes`, rounded up), a cumulative
millisat ledger with sat-denominated Cashu credited at ×1000, a seller-chosen
mint list that the buyer never trusts blindly, buyer-side spending caps, a
bounded grace, and the `cashu-service` wallet boundary. Different: fips charges
forwarding attempts and cannot prove delivery; here every charge is tied to a
hash-verified blob. fips pays through one-way Cashu Spilman channels; `k` leaves
room for the same channel payment (`cashu-spilman`, a signed cumulative
balance) without changing the other messages.

`cashu-credit` is not used; [Provider ledger](#provider-ledger) says why.

## Not built

- A cost tier in `BlobRouter`, so paid routes are tried after free ones.
- Daemon and CLI configuration: nothing starts a paid service, chooses a
  ledger directory, a tariff or mints, or registers a paid route.
- A wallet that can say what it received earlier. `CashuReceiver` cannot:
  `cashu_service::CashuPaymentClient` has no such call. Until it has one, a
  token that the mint redeemed right before a provider crash is not credited
  after the restart; the open redemption stays in the ledger for the operator
  to settle by hand.
- Discovery of paid providers beyond the same host, and selection among
  several of them.
- The TypeScript `BlobRouter` route, the BitTorrent glue, refunds of unused
  balance, Spilman channel payments, and the Iris Drive wallet screen.

## Open decisions

- Default tariff and client caps (the defaults in code are placeholders).
- Which mints a provider lists and a client trusts by default.
- Whether providers should grant any grace to authenticated peers.
- Whether an open redemption that no wallet confirms should expire. Today the
  oldest is dropped only when 4,096 are open.
