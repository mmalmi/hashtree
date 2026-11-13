# hashtree-paid-retrieval

Pay-to-retrieve for Hashtree blobs: a provider that serves only against
payment, the matching client route, and the wallet interface an application
plugs in. Unreleased; the protocol and its open decisions are in
[`docs/paid-retrieval.md`](../../../docs/paid-retrieval.md).

The payment exchange is separate from the Hashtree glue, so another transport
or hash function (BitTorrent, SHA-1 pieces) can reuse it as it is:

- `PaidSeller` keeps prices, account balances and payments; its wallet is a
  `PaymentReceiver`. `PaidBuyer` keeps the client's ledger, decides about
  quotes, pays through a `RetrievalWallet` and stops paying a provider whose
  balance does not match what it delivered.
- `Request`, `Reply` and `ContentHash` are the messages. The hash that names a
  unit carries its algorithm; SHA-256 is the default.
- `LimitedWallet` and `SpendingLimit` hold any wallet to a total budget.
- `PaidBlobTransport` carries one request and one reply. `StreamTransport` and
  `serve_stream` do that over any byte stream.
- The `cashu` feature adapts `cashu_service::CashuPaymentClient` on both sides.

The Hashtree glue is `PaidBlobProvider`, which sells blobs from any `BlobRoute`
source (normally a `StoreBlobRoute`), and `PaidBlobRoute`, one opaque
`BlobRoute` per provider that verifies every blob against its hash.

```rust
let wallet = Arc::new(LimitedWallet::new(my_wallet, SpendingLimit::new(500)));
let paid = PaidBlobRoute::new("provider name", transport, wallet, ClientPolicy::default());
let entry = BlobRouteEntry::new("paid:provider", Arc::new(paid.clone()));
```

`BlobRouter` hedges across its routes and has no notion of cost, so a paid
route registered beside free ones can pay for a blob a slower free route would
have delivered. Until the router has a cost tier, try a paid route only after
the free routes returned nothing.

`AccountState` holds the account's bearer id and any unacknowledged token;
persist it privately.

```sh
cargo test -p hashtree-paid-retrieval --all-features
```
