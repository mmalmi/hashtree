# Paid retrieval

A provider may ask to be paid for serving blobs, and a client may pay and fetch
them. The Rust implementation is `hashtree-paid-retrieval`: one opaque
`PaidBlobRoute` for the client, a `PaidBlobProvider` for the seller, and a small
wallet interface for the application. The published `BlobRequest`/`BlobReply`
wire, HTL and `BlobRouter` are unchanged; the paid exchange is its own service
(`hashtree.paid-blob/1`).

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
| `h` | 32-byte SHA-256 of the blob |
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

The unit paid for is one blob, at most 16 MiB, identified by its hash. Its price
is `max(f, ceil(n × msat / per_bytes))`. Cashu tokens are whole sats; a token of
`s` sat credits `1000 × s` msat, and the remainder stays on the account for
later blobs. A provider with a zero tariff serves without an account.

### The hash is the delivery proof

The client counts a charge only after `sha256(data) == h`. It keeps its own
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
settles the debt. Grace is bound to one account per authenticated peer.

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
- **BitTorrent (BEP 10)**: advertise `ht_pay` in the extended handshake `m`
  dictionary; each message is the payload of one extended message, which
  already has the same length prefix. Two uses:
  - *Blob mode*: `get`/`data` carry any SHA-256 addressed block. A BitTorrent v2
    16 KiB leaf block has the same hash as the equal hashtree blob, so one
    account serves both networks.
  - *Gate mode*: `quote`, `pay` and `paid` are sent as they are, while pieces
    travel in native `request`/`piece` messages. An unpaid peer stays choked,
    `h` names the piece, and the piece hash is the delivery proof.

## Shared with fips pay-to-relay

Shared: the price shape (`msat` per `per_bytes`, rounded up), a cumulative
millisat ledger with sat-denominated Cashu credited at ×1000, a seller-chosen
mint list that the buyer never trusts blindly, buyer-side spending caps, a
bounded grace, and the `cashu-service` wallet boundary. Different: fips charges
forwarding attempts and cannot prove delivery; here every charge is tied to a
hash-verified blob. fips pays through one-way Cashu Spilman channels; `k` leaves
room for the same channel payment (`cashu-spilman`, a signed cumulative
balance) without changing the other messages.

`cashu-credit` is not used yet. It is sat-denominated while blob prices need
millisats, and it is not in this workspace's dependency set. Its validated
account snapshots and deposit-id idempotency are the intended basis for the
provider's durable ledger.

## Not built

- A cost tier in `BlobRouter`, so paid routes are tried after free ones.
- Durable provider accounts: balances are in memory, with `accounts()` and
  `restore_accounts()` as the persistence seam. A provider must persist them
  before taking real money.
- A FIPS carrier and capability advertisement, daemon and CLI configuration.
- The TypeScript `BlobRouter` route, the BEP 10 extension, refunds of unused
  balance, Spilman channel payments, and the Iris Drive wallet screen.

## Open decisions

- Default tariff and client caps (the defaults in code are placeholders).
- Which mints a provider lists and a client trusts by default.
- Whether providers should grant any grace to authenticated peers.
