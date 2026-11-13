# @hashtree/paid-retrieval

The buying side of Hashtree pay-to-retrieve: a `PaidBlobRoute` that fetches
blobs from a provider who asks to be paid. The application supplies a wallet
and a spending limit; the route does the rest. Byte-compatible with the Rust
crate `hashtree-paid-retrieval`. Design: [`docs/paid-retrieval.md`](../../../docs/paid-retrieval.md).

Unreleased: the package is private until the protocol is accepted. There is no
selling side in TypeScript.

## Use

```typescript
import { BlobRouter } from '@hashtree/mesh';
import { createBlobRequest, type Hash } from '@hashtree/core';
import {
  LimitedWallet,
  PaidBlobRoute,
  SpendingLimit,
  type PaidBlobTransport,
  type RetrievalWallet,
} from '@hashtree/paid-retrieval';

declare const freeRoutes: BlobRouter;        // local store, peers, Blossom
declare const wallet: RetrievalWallet;        // the application's Cashu wallet
declare const transport: PaidBlobTransport;   // one request, one reply

const limit = new SpendingLimit(1_000n);      // sats, shared by every paid route
const paid = new PaidBlobRoute({
  provider: 'example-provider',
  transport,
  wallet: new LimitedWallet(wallet, limit),
  policy: { mints: ['https://mint.example'] },
});

async function get(hash: Hash): Promise<Uint8Array | null> {
  const free = await freeRoutes.get(hash);
  if (free) return free;
  const reply = await paid.read(createBlobRequest(hash, 0));
  return reply.type === 'data' ? reply.data : null;
}
```

`BlobRouter` hedges across its routes without regard to cost: a paid route
listed beside free ones can pay for a blob that a slower free route would have
delivered. Until the router has a cost tier, ask a paid route only after the
free routes returned nothing, as above.

A read the client declines to pay for is `no-result`; `paid.stats()` says why
(price, limit, mint, blocked provider). A read that fails or times out throws,
so a slow provider is never mistaken for a miss.

## What the application supplies

```typescript
interface RetrievalWallet {
  pay(request: { provider: string; mints: string[]; amountSat: bigint }): Promise<WalletPayment>;
  balanceSat(): Promise<bigint>;
  reclaim?(payment: WalletPayment): Promise<void>;
}

interface PaidBlobTransport {
  exchange(request: Uint8Array, signal?: AbortSignal): Promise<Uint8Array>;
}
```

- **Wallet.** `pay` creates a token of exactly `amountSat` at one of `mints`
  that the wallet itself trusts, or throws a `WalletError` (`limitReached`,
  `noAcceptedMint`, `insufficientFunds`). Anything else it throws is a failure
  and fails the read. `reclaim` takes back a token the provider refused.
- **Spending limit.** `SpendingLimit` is a total in sats, readable and
  adjustable for a settings screen (`limitSat`, `spentSat`, `remainingSat`,
  `setLimitSat`, `resetSpent`). `LimitedWallet` holds any wallet to it.
- **Transport.** Anything that carries one request and one reply.
- **Policy** (optional): `maxTariff`, `mints`, `topupSat`, `maxPrepaidMsat`,
  `maxLossMsat`, `requestTimeoutMs`. The defaults are placeholders.

Amounts of money are `bigint` (sats and millisats, as on the wire); counts and
byte sizes are `number`.

## Keeping the account

`paid.accountState()` is the prepaid balance, the bearer account id and any
token the provider has not acknowledged yet. Persist it and pass it back as
`account` to keep them across restarts; it survives a structured clone, so
IndexedDB stores it as it is. `paid.clearBlock()` writes off a recorded loss
and lets a blocked provider be paid again; `paid.settle()` resends an
unacknowledged payment and pays a debt now.

## Differences from the Rust client

The wire, the top-up calculation, the ledger and the stats are the same. Three
things differ:

- A reply to a request that was sent before the latest acknowledged payment
  may predate that credit, so its reported balance is ignored. Rust records it,
  which on a carrier that runs exchanges concurrently can look like a loss.
- Hashing is asynchronous in the browser, so deliveries are accounted in the
  order they arrived rather than the order their hashes finish.
- A wallet result or policy that is not in `bigint` amounts is rejected, since
  JavaScript cannot rule it out at compile time.

## Tests

```bash
pnpm --filter @hashtree/paid-retrieval test
```

`tests/vectors.test.ts` checks the codec against
`test-vectors/paid-retrieval-vectors.json`, which is generated from the Rust
encoder and checked there too (`rust/crates/hashtree-paid-retrieval/tests/vectors.rs`).
`tests/client.test.ts` mirrors the Rust client tests with a fake mint, wallet
and provider; no network and no real funds.
