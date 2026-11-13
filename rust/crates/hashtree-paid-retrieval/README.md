# hashtree-paid-retrieval

Pay-to-retrieve for Hashtree blobs: a provider that serves only against
payment, the matching client route, and the wallet interface an application
plugs in. Unreleased; the protocol and its open decisions are in
[`docs/paid-retrieval.md`](../../../docs/paid-retrieval.md).

- `PaidBlobProvider` answers requests from any `BlobRoute` source (normally a
  `StoreBlobRoute`) against prepaid account balances. Its wallet is a
  `PaymentReceiver`.
- `PaidBlobRoute` is one opaque `BlobRoute` for one provider. It pays through a
  `RetrievalWallet`, verifies every blob against its hash, and stops paying a
  provider whose balance does not match what it delivered.
- `LimitedWallet` and `SpendingLimit` hold any wallet to a total budget.
- `PaidBlobTransport` carries one request and one reply. `StreamTransport` and
  `serve_stream` do that over any byte stream.
- The `cashu` feature adapts `cashu_service::CashuPaymentClient` on both sides.

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
