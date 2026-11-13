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

## Over HTTP

`HttpPaidBlobTransport` carries the exchange to a gateway with `fetch`:
`POST <gateway>/paid-blob`, one encoded request as the body, one encoded
reply in a `200` response.

```typescript
import { HttpPaidBlobTransport, PaidBlobRoute } from '@hashtree/paid-retrieval';

const paid = new PaidBlobRoute({
  provider: 'example-gateway',
  transport: new HttpPaidBlobTransport('https://gateway.example'),
  wallet,
});
```

Any status but `200`, a network error, an abort or a reply above the
protocol's size limit fails the read; none of them is taken for a missing
blob. The transport does not retry. A second argument replaces the global
`fetch`. The request names the account, a bearer secret, so it is sent
without cookies or a referrer and does not follow redirects.

## What the application supplies

```typescript
interface RetrievalWallet {
  pay(request: PaymentRequest): Promise<WalletPayment>;
  recover?(paymentId: Uint8Array): Promise<WalletPayment | null>;
  balanceSat(): Promise<bigint>;
  reclaim?(payment: WalletPayment): Promise<void>;
}

interface PaymentRequest {
  provider: string;
  mints: string[];
  amountSat: bigint;
  maxFeeSat: bigint;
  lock: Uint8Array | null;
  paymentId: Uint8Array;
}

interface AccountStore {
  save(account: AccountState): Promise<void>;
}

interface PaidBlobTransport {
  exchange(request: Uint8Array, signal?: AbortSignal): Promise<Uint8Array>;
  authenticatesPeer?(): boolean;
}
```

- **Wallet.** `pay` creates a token of exactly `amountSat` at one of `mints`
  that the wallet itself trusts, spending at most `maxFeeSat` on top, or
  throws a `WalletError` (`limitReached`, `noAcceptedMint`,
  `insufficientFunds`). Anything else it throws is a failure and fails the
  read. Asked again with the same `paymentId` it should return the same
  token; `recover` finds the token of a `paymentId` after a crash, and
  `reclaim` takes back a token the provider refused. `lock` is the key the
  provider asks tokens to be locked to (NUT-11); a wallet that cannot lock
  tokens sends them unlocked.
- **Spending limit.** `SpendingLimit` is a total in sats, readable and
  adjustable for a settings screen (`limitSat`, `spentSat`, `remainingSat`,
  `setLimitSat`, `resetSpent`). `LimitedWallet` holds any wallet to it: the
  amount and the fee ceiling are reserved before the wallet is asked. Given a
  `SpendStore`, the limit saves what was spent before each payment, and a
  failed save refuses the payment.
- **Account store.** Where the account is kept; see below.
- **Transport.** Anything that carries one request and one reply;
  `HttpPaidBlobTransport` is one. A carrier that authenticates the client
  itself, as FIPS does, says so with `authenticatesPeer()`: requests then name
  no account, and the provider charges the client's identity.
- **Policy** (optional): `maxTariff`, `mints`, `topupSat`, `maxPrepaidMsat`,
  `maxLossMsat`, `maxTokenFeeSat`, `maxRedeemFeeMsat`, `requestTimeoutMs`. The
  defaults are placeholders.

Amounts of money are `bigint` (sats and millisats, as on the wire); counts and
byte sizes are `number`.

## Round trips

A new token rides with the next `get` instead of being sent in a `pay` of its
own, and its credit comes back with the blob or the next quote, so a first
paid read is two round trips: `get` → `quote`, then `get` carrying the token
→ `data`. A funded read is one. Only one request at a time carries a given
token; a token the provider answered "not yet" is sent on its own before the
next `get`. The route passes the read's hop budget (`htl`) to the provider.

## Keeping the account

The account is the prepaid balance, the bearer account id and every token
that is not settled. Give the route a `store` wherever real money is paid,
and pass what the store held back as `account` on the next start:

```typescript
const paid = new PaidBlobRoute({ provider, transport, wallet, store, account: await load() });
```

`store.save` is called before the wallet is asked for a payment (with the
payment's id as an `intent`), when the token arrives (as `pending`, before it
is sent) and whenever a payment is settled. A save that fails before the
wallet is asked stops that payment. After a restart an intent without a token
makes the route ask `wallet.recover` for that id instead of paying twice.
Charges for delivered blobs are not saved one by one: the first balance the
provider reports after a restart is taken as the baseline
(`stats().resyncedMsat`).

`AccountState` survives a structured clone, so IndexedDB stores it as it is;
`encodeAccountState` and `decodeAccountState` give the same bytes as the Rust
client. There is no file store here. Store the account privately: it holds a
bearer id and unredeemed tokens.

A token the provider refused and the wallet could not take back, or one the
provider answered "not yet" three times, is kept in `unresolved` (at most 8),
never dropped. Its value counts against `maxPrepaidMsat`, and the oldest one
is tried again on every payment and on `paid.settle()`. A provider that
credits less than a token was worth is allowed `maxRedeemFeeMsat`
(`stats().redeemFeeMsat`); anything beyond it counts as a loss.
`paid.clearBlock()` writes off a recorded loss and lets a blocked provider be
paid again.

## Differences from the Rust client

The wire, the account record, the top-up calculation, the ledger and the
stats are the same. What differs:

- Saving is asynchronous (`AccountStore.save`, `SpendStore.saveSpent`).
  `SpendingLimit` counts a reservation at once, so concurrent payments still
  cannot overspend together, and takes it back if the save fails; its saves
  reach the store one at a time, in order. `resetSpent()` returns a promise.
- A read that waits for the payment lock to carry a token stops waiting at
  its deadline, like the rest of the read.
- Hashing is asynchronous in the browser, so deliveries are accounted in the
  order they arrived rather than the order their hashes finish.
- A wallet result, policy or saved account that is not in `bigint` amounts is
  rejected, since JavaScript cannot rule it out at compile time.

## Tests

```bash
pnpm --filter @hashtree/paid-retrieval test
```

`tests/vectors.test.ts` checks the codec against
`test-vectors/paid-retrieval-vectors.json`, which is generated from the Rust
encoder and checked there too (`rust/crates/hashtree-paid-retrieval/tests/vectors.rs`).
`tests/client.test.ts`, `tests/buyerSafety.test.ts` and `tests/wireV2.test.ts`
mirror the Rust client, buyer-safety and wire v2 tests with a fake mint, wallet and provider; no network and
no real funds.
