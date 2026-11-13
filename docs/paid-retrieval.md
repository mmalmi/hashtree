# Paid retrieval

A provider may ask to be paid for serving blobs, and a client may pay and fetch
them. The Rust implementation is `hashtree-paid-retrieval`: one opaque
`PaidBlobRoute` for the client, a `PaidBlobProvider` for the seller, and a small
wallet interface for the application. The published `BlobRequest`/`BlobReply`
wire and HTL are unchanged; the paid exchange is its own service
(`hashtree.paid-blob/1`), carried between nodes over FIPS, and the provider
keeps its accounts on disk. `BlobRouter` asks a paid route only after its free
routes found nothing, and the daemon sells and buys when
[configured](#configuration); nothing is on by default.

The payment exchange does not depend on Hashtree routing, on the carrier, or on
the hash function that names what is sold; see [Reusable parts](#reusable-parts).

The wire protocol is specified, with vectors, as a draft in the Hashtree
specification: [Paid retrieval](https://github.com/mmalmi/hashtree-spec/blob/main/specs/paid-retrieval.md).
This document is the design rationale and the state of the implementation.

## What the retired design got wrong

The 2026-03 DataQuote/DataPayment/DataChunk protocol (removed 2026-07-17) is not
restored. Its problems, and what replaces each:

| Retired design | This design |
|---|---|
| Added five message types to the shared blob wire and lived inside the mesh peer selector. | Separate service behind one route; the blob wire is untouched and the router only learns that a route costs money. |
| Split a blob into unverifiable sub-chunks and paid one Cashu token per chunk, so every 32 KiB cost two mint round trips and a 5 s settlement timer. | The paid unit is a whole blob, checked against its hash. Payments top up a balance, so most reads cost no mint call. |
| Quote ids, pending quotes, expected settlements and outgoing transfers kept on both sides, keyed by peer and expiring in 1.5 s. | No quote state. A request carries the largest debit it authorises; a provider that wants more answers with its terms. |
| Whole-sat flat offer per blob, whatever its size. | Millisat ledger priced per byte. |
| Pay after delivery, protected only by peer reputation. | Each side sets its own exposure: the client a prepaid cap, the provider an optional grace. |
| Peer-suggested mints with growing trust caps. | The provider lists its mints; the client's wallet pays only with mints it already trusts. |
| Never called by a production read path. | Usable as an ordinary `BlobRouteEntry`. |

## The exchange

The client holds an account with each provider, which the provider maps to a
balance in millisats. Balance is everything paid minus everything charged. On
a carrier that authenticates the client (FIPS) the account is the client's
identity and requests name none. Elsewhere (HTTP, plain streams) it is a
random 32-byte id `a` that the client sends with every request.

```text
client                                   provider
  get {h, a?, x, l?}       ──────────►   has blob, price ≤ x, balance covers it
                           ◄──────────   data {h, n, c, b} + n raw bytes

  get {h, a?, x}           ──────────►   balance too low, or price > x
                           ◄──────────   quote {h, n, c, b, g, r, f, m, k, u, o?}
  get {h, a?, x, i, k, m, v} ────────►   credits the token, then charges
                           ◄──────────   data {h, n, c, b, i, v} + n raw bytes
```

A payment rides with the next `get`, so a first paid read is two round trips.
The reply carries the credit (`i`, `v`) beside the blob or beside a new
quote; a payment that fails answers the whole request with `err`. `pay` remains
for resending a token whose answer was lost and for carriers that sell
something other than blobs (BitTorrent gate mode). One request at a time
carries a given token; concurrent reads wait for it and then read from the
balance.

`l` is the hop budget, the same HTL as in the free blob request: the provider
passes it to its own source, which may forward. Paid forwarding itself, where a
provider buys from the next hop within what the requester authorised, is not
built yet.

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
| `v` | the Cashu token (`pay`, `get`), msat credited (`paid`, `data`, `quote`) |
| `l` | hop budget, 1 to 10; absent means 0 |
| `o` | 33-byte public key that tokens should be locked to (NUT-11) |

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
  The client asks its wallet to reclaim it. A token the wallet cannot take
  back is kept as unresolved, never dropped: a provider that refused it may
  still redeem it. Its value counts as money the provider holds, and it is
  tried again whenever the client pays or settles.
- **The provider keeps answering `retry`.** After three such answers the token
  is set aside as unresolved in the same way, so one stuck token does not hold
  up later payments; the prepaid cap still bounds the total.
- **The provider credits less than the token was worth.** A provider's mint
  may charge for redeeming, so the client accepts a shortfall of up to
  `max_redeem_fee_msat` per payment and records it as a fee. Anything beyond
  that is counted as a loss, like a charge without a delivery.
- **The client crashes around a payment.** Before the wallet is asked, the
  client saves a payment intent with a fresh id; the token is saved before it
  is sent. After a restart an intent without a token makes the client ask the
  wallet for that id again (`recover`) instead of paying a second time.
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
   It reserves the amount plus the fee ceiling before the wallet is asked,
   and with a `SpendStore` saves what was spent before each payment.

Unresolved tokens count against `max_prepaid_msat`, so a provider cannot
collect more by leaving payments unanswered.

### What the client keeps

`PaidBuyer::with_store` and `PaidBlobRoute::with_store` take an
`AccountStore`; `FileAccountStore` keeps one account in one file, replaced
atomically. The account is saved before the wallet is asked, when a token
arrives and when a payment is settled. Charges for delivered blobs are not
saved one by one: after a restart the first balance the provider reports is
taken as the baseline. `AccountState::encode` gives the same bytes to an
application that has its own database. The account holds a bearer id and
unredeemed tokens and must be stored privately.

A declined read returns `NoResult`, so the router continues to other routes;
`PaidBlobRoute::stats()` says why (price, limit, mint, blocked provider).

### Free routes first

`BlobRouter` hedges across free routes only. A route registered with
`BlobRouteEntry::paid` is in the paid tier:

- It is asked only after every free route of the search has finished without
  data. A free route that is merely slow is waited for; if the deadline
  passes first, the search ends as a timeout and nothing is paid.
- Paid routes are asked one at a time, never as a hedge, so one blob is not
  bought twice.
- Preference and outcome ordering apply within a tier and never move a paid
  route ahead of a free one.
- `get_request_within_cost(.., BlobRouteCost::Free)` searches the free routes
  only, and `get_request_at_cost` one tier on its own. The daemon reads from
  Blossom servers beside its FIPS routes, so it searches those free sources
  together and asks its paid routes, kept in a router of their own, only once
  all of them have finished without data. A request it serves to a peer never
  reaches a paid route: the daemon does not buy blobs for others.

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

`PaymentRequest` names the provider, the mints the provider accepts, the
amount in sat, the most the token may cost in fees, and a payment id that was
saved before the call. A wallet that keeps its payments returns the same token
when it is asked for the same id again, and implements `recover` to find a
token by that id after a crash. An application implements `pay` and
`balance_sat`, and either enforces its own limit or wraps the wallet in
`LimitedWallet`. With the `cashu`
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

### Locked tokens

A provider may name a key in its quotes (`o`). A wallet that can lock tokens to
that key (NUT-11 P2PK) should, so that a token taken on the way is worthless to
anyone else and, with NUT-12 proofs, the provider can check it without asking
the mint. Unlocked tokens are still accepted. Neither the `cashu-service`
helper nor `CashuWallet` can lock tokens or check them offline yet, so today
the key only reaches the wallet.

## Over HTTP

The endpoint below is built: the daemon serves it when its provider has
`http = true` (see [Configuration](#configuration)), the daemon's buyer has an
HTTP carrier (`HttpPaidTransport` in `hashtree-cli`, so that
`hashtree-paid-retrieval` keeps no HTTP dependency), and the TypeScript buyer
has its own (`HttpPaidBlobTransport`). The `402` face is not built. Two faces
of the same `PaidSeller`, for two kinds of client.

### The exchange over one endpoint

A gateway accepts the exchange as it is: `POST /paid-blob` with one encoded
request as the body (`application/octet-stream`) answers `200` with one encoded
reply. Transport failures are HTTP errors; every protocol outcome, including
`quote` and `err`, is a `200` with that reply. This is a `PaidBlobTransport`
like any other, so accounts, millisat prices, free repeats, payment ids and
replay protection all apply unchanged, and it is what gives a browser a way to
pay: the TypeScript buyer needs only `fetch`. Replies are not cacheable
(`Cache-Control: no-store`); free blobs keep their cacheable `GET`.

In the daemon a body above 64 KiB is `413` and an unreadable one `400`, both
with `X-Reason`; a daemon that does not sell over HTTP answers `404`, and one
whose sixteen exchange slots are taken `503` with `Retry-After`. An HTTP buyer
is not authenticated, so it gets no grace, and its account id crosses the
network in every request: use `https` to any gateway that is not on the same
machine. Responses carry the same permissive CORS headers as the Blossom
endpoints. The buyer follows no redirect and keeps no cookies.

### Standard clients: HTTP 402

Blossom's BUD-07 (draft) and Cashu's NUT-24 already define paying for a blob
download, so a gateway that charges should answer foreign clients in their
terms on the ordinary `GET /<sha256>`:

- An unpaid request gets `402 Payment Required` with `X-Cashu` holding a
  NUT-18 payment request: the price rounded up to whole sats, unit `sat`, and
  the provider's mints as a strict list.
- The client repeats the request with a `cashuB` token in `X-Cashu`. The
  provider redeems it through the same seller and serves the blob. A bad or
  short token is `400` with `X-Reason`, as BUD-07 says.
- `HEAD /<sha256>` answers `402` with the same header and no body, so a client
  can learn the price without fetching.

Neither document says what happens to an overpayment, how a payment covers
more than one request, or what a client can do when the token is taken and no
data arrives. Here the token is credited to an account named by the hash of
the token, and the blob is charged to it at the exact millisat price. A client
that sends the same `X-Cashu` header again spends what is left on that
account, so one larger token pays for many blobs, and a repeated request for a
charged blob is free for the redelivery window. A client that knows nothing of
this still gets exactly BUD-07.

This face costs the provider a mint round trip for every new token, and rounds
small blobs up to a whole sat. The mint client's default pacing of about
twenty requests a minute per mint then caps how many new tokens a provider can
take. It is for compatibility. A Hashtree client uses the endpoint above.

`X-Lightning` (a BOLT-11 invoice, proven by its preimage) needs no Lightning
node: the provider's wallet can ask its mint for a mint quote and use that
invoice. A one-blob invoice is not worth its fees, so Lightning fits better as
a second way to top up an account (`k` = `bolt11`) than as a per-blob price.

### Paying other people's servers

A Hashtree client that meets `402` with `X-Cashu` on a third-party Blossom
server can pay it through the same wallet and limits: check the requested
amount against `max_tariff` for the size that `HEAD` reports, pay exactly that
amount, verify the hash, and count a payment without valid data as a loss
against that server. There is no account and nothing to recover; the loss is
at most one blob's price per attempt.

## Paying for availability, not for retrieval

Pay-to-retrieve rewards serving what is asked for. It does not reward keeping
data that nobody asks for. Durabit (2023, not built) addresses that for
torrents: a sponsor locks bitcoin in pre-signed, timelocked payouts to a
Chaumian mint, and the mint issues ecash each period to the seeders it finds
serving the torrent. Its weak point is the mint, which must be trusted to pay
out and to judge who really seeds.

The closest fit here needs no new exchange. An account id is a bearer
credential, so a publisher can fund an account at a provider and hand the id
to readers: retrieval is then paid by the publisher. And a sponsor who wants
data kept can do what the Durabit mint does without being a mint: fetch
randomly chosen blobs of the tree through a paid route now and then. Each
hash-verified reply is a proof that the provider still has that blob, and the
payment for it is the reward. Neither is built or tested as such.

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

## TypeScript client

`@hashtree/paid-retrieval` (`ts/packages/hashtree-paid-retrieval`) is the buying
side for browsers and workers: the same messages, `PaidBuyer`, `PaidBlobRoute`,
wallet interface, `SpendingLimit` and `LimitedWallet`, with a transport
interface that carries one request and one reply and `HttpPaidBlobTransport`
as its first carrier. It has no selling side and is not in the worker's
default route set.

It carries a fresh token with the next `get`, names no account on a carrier
that authenticates it, passes the hop budget on and hands a quote's lock key
to the wallet, as the Rust buyer does.

It protects the buyer's money in the same way: fee and shortfall accounting,
the payment intent and token saved before they are used, `recover` after a
restart, and unresolved tokens. Its `AccountStore` and `SpendStore` are
asynchronous, as browser storage is. It has no file store;
`encodeAccountState` writes the same record as `AccountState::encode`.

`test-vectors/paid-retrieval-vectors.json` holds the two codecs together. It is
generated from the Rust encoder by `tests/vectors.rs` and checked by both
languages: every message both ways, the saved account record, what each
decoder accepts and rejects at every limit, and prices.

Both buyers ignore the balance in a reply to a request that was sent before
the latest acknowledged payment, because such a reply may predate the credit.
Otherwise, on a carrier that runs exchanges concurrently, a late quote would
look like a loss or cause a second top-up.

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

### What should become shared code

The payment exchange in this crate is not specific to Hashtree: `PaidSeller`,
`PaidBuyer`, the messages, the ledger with its journal, the wallet boundary and
the spending limit need only a hash function and an error type from
`hashtree-core`. Their natural home is the `cashu-service` workspace, beside
`cashu-credit` (sat-level accounts) and `peer-payments` (mint-less signed
credit), as the third way to pay for a metered service: a prepaid bearer
account in millisats. fips could then sell guest or relay access with the same
accounts where a Spilman channel is too heavy.

It is not moved yet, for three reasons. Nothing in fips uses it today.
Hashtree builds against `cashu-service` 0.3.0 and fips against 0.4.8, so
shared code means upgrading Hashtree's wallet stack first. And a new published
crate is a release decision. Two changes belong in `cashu-service` itself, and
both would close gaps listed below:

- a token send that takes a caller's payment id and a fee ceiling, so that
  `CashuWallet::recover` can find a token again and refuse a fee in advance;
- a lookup of what the wallet received for a token, so that a provider can
  credit a token the mint redeemed right before a crash.

### Not adopted from fips yet

- A lifetime budget that no reset replenishes, beside the resettable
  `SpendingLimit`.
- Expiry and return of an unused balance (fips channels have a service limit,
  a refund deadline and cooperative settlement).
- Choosing among several providers by price over measured success rate, with
  a small trial top-up for an unknown provider.
- Rate limits on unpaid work: an unpaid `get` costs the provider a blob lookup
  and tells the asker what it has.
- Reaching the mint over the mesh for a client without other connectivity.
- Pacing of mint requests: the mint client allows 20 requests a minute by
  default, which many small top-ups from many buyers would exhaust.

## Configuration

The daemon (`htree start`, and the embedded daemon) sells and buys only when
`[paid_retrieval]` is in its configuration. Without it no paid service is
bound, no ledger or account file is opened and no paid route is registered.
A section that is present takes the defaults below for every value it leaves
out. The example shows every default; a section needs nothing but its
providers.

```toml
# Sell the blobs of the local store. Every value shown is the default.
[paid_retrieval.provider]
# mints = ["https://mint.example"]  # default: the Cashu wallet's accepted mints
min_topup_sat = 1     # the smallest token there is
grace_msat = 0        # prepay
paying_only = false
http = false          # true: also sell at POST /paid-blob
# lock = "02..."      # default: none
# ledger_dir = "..."  # default: paid-retrieval/ledger in the data directory

[paid_retrieval.provider.tariff]
msat = 1
per_bytes = 10240
min_msat = 10

# Buy what no free source returned. Every value shown is the default.
[paid_retrieval.client]
spending_limit_sat = 1000
# mints = ["https://mint.example"]  # default: the Cashu wallet's accepted mints
topup_sat = 10
max_prepaid_msat = 21000
max_loss_msat = 10000
max_token_fee_sat = 2
max_redeem_fee_msat = 2000
pay_for_remote_reads = false

[paid_retrieval.client.max_tariff]
msat = 1
per_bytes = 2048
min_msat = 100

[[paid_retrieval.client.providers]]
name = "archive"
npub = "npub1..."            # reached over FIPS

[[paid_retrieval.client.providers]]
name = "gateway"
url = "https://gw.example"   # reached at https://gw.example/paid-blob
```

### Why these defaults

- **Price**: 1 msat per 10 KiB, at least 10 msat per blob. That is about 1 sat
  per 10 MiB, close to what cloud storage charges for egress, and the minimum
  covers the work of one blob.
- **Price cap**: 1 msat per 2 KiB, at least 100 msat, five times the default
  price. Ordinary providers are accepted; one that asks more is declined.
- **Client caps**: a top-up of 10 sat, at most 21 sat held by one provider,
  10 sat of loss before a provider is blocked, fee ceilings of 2 sat per token
  and 2 sat per redemption.
- **Spending limit**: 1000 sat over the daemon's lifetime. It is never reset
  by the daemon; raising it is an explicit act. fips pay-to-relay found that
  no loop may reset a lifetime budget.
- **Mints**: when `mints` is absent, both sides use the mints the daemon's own
  Cashu wallet accepts (`[cashu] accepted_mints`, `htree cashu mint list`).
  A mint that holds your money is one you already trust; a peer's list is
  never trusted. If the wallet accepts no mint either, the daemon refuses to
  start and says to add one with `htree cashu mint add`. An explicit `mints`
  overrides.
- **Grace**: 0. Identities are free to create, so any grace would be a free
  allowance per identity.
- **Lock**: none, because no wallet can lock tokens yet.
- **`paying_only`, `http`**: off. Selling is added to what a daemon serves; it
  takes nothing away unless asked to.
- **`pay_for_remote_reads`**: off. Money is spent only for this host's own
  reads, never on behalf of a peer. Blobs the HTTP gateway serves stay
  governed by its existing access settings.

A change takes effect when the daemon is restarted. Both sides use the
`htree-cashu` helper as their wallet.

**FIPS or HTTP.** With the FIPS endpoint (`server.enable_fips`) the provider
sells over FIPS, and with `http = true` also through its HTTP listener. A
client reaches a provider named by `npub` over FIPS and one named by `url`
over HTTP. Without FIPS a provider must have `http = true` and a client may
buy only from `url` providers; otherwise the daemon refuses to start and
names what cannot be reached.

**Provider.** The daemon opens the ledger, sells from its local store only
(a sale never searches the mesh). With `paying_only = true` its free blob
service answers every peer with no result, so a stored blob reaches peers
only against payment; blobs served over HTTP are not affected. `lock`, a
compressed public key in hex, is named in every quote; unlocked tokens are
still accepted. A buyer over FIPS is charged to its authenticated identity; a
buyer over HTTP to the bearer account id it sends. The provider's wallet only
receives and needs no funds. The ledger holds the account ids of its buyers,
which are bearer secrets: its directory is made readable by the owner only.

**Client.** Each provider has a name and either a FIPS identity (`npub`) or
the base URL of its HTTP gateway (`url`, `http` or `https`, without user name,
password or query). The daemon adds an `npub` to its FIPS peers, keeps one
account per provider in `paid-retrieval/accounts/` and the spent total in
`paid-retrieval/spent`, both in the data directory and readable by the owner
only. The paid routes form a router of their own that only reads arriving
over HTTP use; the router that answers peers never sees them. The wallet pays
only with the client's mints and must hold funds at one of them
(`htree cashu`). A top-up is a round trip to the mint on both sides. When it
outlasts the deadline of the read that caused it, that read fails as a
timeout, the payment still completes, and the next read is served from the
balance.

Money is spent only for reads that arrive at the daemon's HTTP listener, once
the local store, the free FIPS routes and the Blossom servers have all
finished without the blob, whether they answered that they have none, failed
or timed out. That matches the router's paid tier: a free source that fails is
exactly when a paid provider should be asked, and the caps bound the cost. If
nothing is bought, a free source's failure is what the read reports. By
default money is spent only when the listener is bound to a loopback address;
`pay_for_remote_reads = true` extends it to any bind address, that is to
whoever can reach the listener. A loopback listener behind a reverse proxy is
reachable by others too. A request from a peer over the blob service is never
paid for.

A read is not authenticated. Whatever can make the daemon read a hash, such
as another program on the machine or a web page open in a browser there, can
cause a purchase from a configured provider. The caps bound what that costs:
the price cap per blob, the prepaid cap per provider and the spending limit
in total.

### Operator commands

- `htree status` and `htree paid status` show, for a provider, its tariff,
  the number of accounts and what they hold; for a client, the spending limit
  with spent and remaining, and per provider the balance held, sats paid,
  blobs bought, declined reads by reason, unresolved tokens and the reason of
  a block. Counts start with the daemon. When the daemon is not running,
  `htree paid status` reads the account files instead and shows no counts.
  Tokens and account ids are never shown.
- `htree paid clear-block <provider>` writes off the loss a provider was
  blocked for and pays it again.
- `htree paid settle` resends unacknowledged payments and tries unresolved
  tokens again, with the daemon's wallet and identity.

The daemon holds a lock on the client's accounts while it runs.
`clear-block` and `settle` take the same lock and refuse while the daemon
runs, instead of racing it: stop the daemon first.

## Not built

- Starting a new spending period: raise `spending_limit_sat` instead.
- Applying a changed `[paid_retrieval]` on `htree reload`; it needs a restart.
- Telling a local HTTP request from a remote one for each read. The choice is
  made once, from the bind address.
- A wallet that can say what it received earlier. `CashuReceiver` cannot:
  `cashu_service::CashuPaymentClient` has no such call. Until it has one, a
  token that the mint redeemed right before a provider crash is not credited
  after the restart; the open redemption stays in the ledger for the operator
  to settle by hand.
- Discovery of paid providers beyond the same host, and selection among
  several of them.
- The HTTP 402 face ([Over HTTP](#over-http)). It waits for the rework of the
  exchange's messages and will be specified again as a mapping of them.
- Discovery of HTTP gateways: each is named in the configuration.
- Wiring into the worker's routes, the BitTorrent
  glue, refunds of unused balance, Spilman channel payments, and the Iris
  Drive wallet screen.

## Open decisions

- Whether an open redemption that no wallet confirms should expire. Today the
  oldest is dropped only when 4,096 are open.
