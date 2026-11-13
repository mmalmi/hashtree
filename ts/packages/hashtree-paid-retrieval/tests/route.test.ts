/** What the TypeScript route adds to the cases mirrored from Rust. */

import { describe, expect, it } from 'vitest';
import type { BlobRoute, BlobRouteContext, Hash } from '@hashtree/core';
import {
  PaidBlobRoute,
  decodeRequest,
  lossMsat,
  type ClientPolicy,
  type PaidBlobTransport,
  type Request,
  type RetrievalWallet,
} from '../src/index.js';
import { DirectTransport, FakeSeller, FakeWallet, MINT, TARIFF } from './support/fakes.js';

function route(
  transport: PaidBlobTransport,
  wallet: RetrievalWallet,
  overrides: Partial<ClientPolicy> = {},
): PaidBlobRoute {
  return new PaidBlobRoute({
    provider: 'seller',
    transport,
    wallet,
    policy: {
      maxTariff: TARIFF,
      mints: [MINT],
      topupSat: 5n,
      maxPrepaidMsat: 10_000n,
      maxLossMsat: 1_500n,
      ...overrides,
    },
  });
}

function context(timeoutMs: number, signal = new AbortController().signal): BlobRouteContext {
  return { signal, deadlineMs: Date.now() + timeoutMs, attemptBudget: 1 };
}

function gate(): { opened: Promise<void>; open: () => void } {
  let open = (): void => undefined;
  const opened = new Promise<void>((resolve) => {
    open = resolve;
  });
  return { opened, open };
}

async function setup(blobs = 1): Promise<{
  seller: FakeSeller;
  wallet: FakeWallet;
  transport: DirectTransport;
  hashes: Hash[];
}> {
  const seller = new FakeSeller();
  const hashes: Hash[] = [];
  for (let seed = 0; seed < blobs; seed += 1) hashes.push((await seller.add(seed, 4_096)).hash);
  return { seller, wallet: new FakeWallet(seller.mint, 21n), transport: new DirectTransport(seller), hashes };
}

describe('PaidBlobRoute as a BlobRoute', () => {
  it('has a route identity derived from the provider', async () => {
    const { wallet, transport } = await setup();
    const paid: BlobRoute = route(transport, wallet);
    expect(paid.id).toBe('paid:seller');
    expect(new PaidBlobRoute({ provider: 'seller', transport, wallet, id: 'x', groupId: 'g' }))
      .toMatchObject({ id: 'x', groupId: 'g' });
  });

  it('reports a slow provider as a timeout, never as a miss', async () => {
    const { wallet, hashes } = await setup();
    let abandoned: AbortSignal | undefined;
    const silent: PaidBlobTransport = {
      exchange: (_request, signal) => new Promise(() => {
        abandoned = signal;
      }),
    };

    await expect(route(silent, wallet).read({ hash: hashes[0], htl: 0 }, context(20)))
      .rejects.toThrow('paid route timed out; availability is unknown');
    expect(abandoned?.aborted, 'the transport is told nobody is waiting').toBe(true);

    // Without a router context the policy's own timeout applies.
    await expect(route(silent, wallet, { requestTimeoutMs: 20 }).read({ hash: hashes[0], htl: 0 }))
      .rejects.toThrow('timed out');
    expect(wallet.payments).toBe(0);
  });

  it('stops waiting when the router cancels the read', async () => {
    const { wallet, hashes } = await setup();
    const silent: PaidBlobTransport = { exchange: () => new Promise(() => undefined) };
    const cancel = new AbortController();
    const reading = route(silent, wallet).read({ hash: hashes[0], htl: 0 }, context(5_000, cancel.signal));
    cancel.abort();
    await expect(reading).rejects.toThrow('paid route read was cancelled');

    await expect(route(silent, wallet).read({ hash: hashes[0], htl: 0 }, context(5_000, cancel.signal)))
      .rejects.toThrow('cancelled');
  });

  it('keeps a token the wallet created after the read gave up waiting', async () => {
    const { seller, wallet, transport, hashes } = await setup();
    const slow = gate();
    wallet.gate = slow.opened;
    const paid = route(transport, wallet);

    await expect(paid.read({ hash: hashes[0], htl: 0 }, context(30)))
      .rejects.toThrow('paid route payment is still in progress; availability is unknown');
    expect(wallet.payments).toBe(0);

    // The payment step ran on: the token reaches the provider and is credited.
    slow.open();
    await paid.settle();
    expect(wallet.payments).toBe(1);
    expect(seller.received).toBe(5n);
    expect(paid.accountState()).toMatchObject({ pending: null, creditedMsat: 5_000n });

    const exchanges = transport.exchanges;
    expect((await paid.read({ hash: hashes[0], htl: 0 })).type).toBe('data');
    expect([wallet.payments, transport.exchanges - exchanges]).toEqual([1, 1]);
  });

  it('ignores a balance reported before a payment it already knows was credited', async () => {
    const { seller, wallet, hashes } = await setup(2);
    const late = gate();
    let held = false;
    const direct = new DirectTransport(seller);
    // The reply to the first get of the second blob arrives late.
    const transport: PaidBlobTransport = {
      async exchange(request) {
        const reply = await direct.exchange(request);
        const decoded: Request = decodeRequest(request);
        if (!held && decoded.type === 'get' && decoded.hash.digest[0] === hashes[1][0]) {
          held = true;
          await late.opened;
        }
        return reply;
      },
    };
    const paid = route(transport, wallet);

    const second = paid.read({ hash: hashes[1], htl: 0 });
    expect((await paid.read({ hash: hashes[0], htl: 0 })).type).toBe('data');
    expect(paid.accountState()).toMatchObject({ creditedMsat: 5_000n, balanceMsat: 4_000n });

    // The late quote says "balance 0", from before the top-up was credited.
    late.open();
    expect((await second).type).toBe('data');
    expect(wallet.payments, 'no second top-up for a balance that was already paid').toBe(1);
    const account = paid.accountState();
    expect([account.balanceMsat, lossMsat(account), account.blocked]).toEqual([3_000n, 0n, null]);
  });

  it('continues with an account the application persisted', async () => {
    const { seller, wallet, transport, hashes } = await setup(2);
    const first = route(transport, wallet);
    expect((await first.read({ hash: hashes[0], htl: 0 })).type).toBe('data');
    const saved = structuredClone(first.accountState());

    const restored = new PaidBlobRoute({
      provider: 'seller',
      transport,
      wallet,
      policy: { maxTariff: TARIFF, mints: [MINT] },
      account: saved,
    });
    expect((await restored.read({ hash: hashes[1], htl: 0 })).type).toBe('data');
    expect(wallet.payments, 'the prepaid balance was still there').toBe(1);
    expect(restored.accountState()).toMatchObject({ chargedMsat: 2_000n, balanceMsat: 3_000n });
    expect(seller.accounts()).toHaveLength(1);
  });

  it('rejects a policy or a saved account that is not in bigint amounts', async () => {
    const { wallet, transport } = await setup();
    expect(() => route(transport, wallet, { topupSat: 5 as unknown as bigint }))
      .toThrow('paid route policy amounts must be non-negative bigint values');
    expect(() => route(transport, wallet, { requestTimeoutMs: 0 })).toThrow('requestTimeoutMs');
    const account = { ...route(transport, wallet).accountState(), account: new Uint8Array(8) };
    expect(() => new PaidBlobRoute({ provider: 'seller', transport, wallet, account }))
      .toThrow('paid route account state is invalid');
  });

  it('accepts a policy mint that differs only by a trailing slash', async () => {
    const { wallet, transport, hashes } = await setup();
    const paid = route(transport, wallet, { mints: [` ${MINT}/ `] });
    expect((await paid.read({ hash: hashes[0], htl: 0 })).type).toBe('data');
  });

  it('gives up on a provider that keeps asking for payment', async () => {
    const { seller, wallet, hashes } = await setup();
    const transport = new DirectTransport(seller, (_request, reply) => (reply.type === 'data'
      ? {
        type: 'quote',
        quote: {
          hash: reply.hash,
          size: BigInt(reply.data.length),
          priceMsat: 1_000n,
          balanceMsat: reply.balanceMsat,
          graceMsat: 0n,
          tariff: TARIFF,
          mints: [MINT],
          methods: ['cashu'],
          minTopupSat: 1n,
          lock: null,
          credit: null,
        },
      }
      : reply));
    await expect(route(transport, wallet).read({ hash: hashes[0], htl: 0 }))
      .rejects.toThrow('paid route provider kept asking for payment');
    expect(wallet.payments).toBe(1);
  });

  it('fails the read on a refusal, a reply for another blob or an undecodable reply', async () => {
    const { seller, wallet, hashes } = await setup(2);
    const other = { algorithm: 'sha256' as const, digest: hashes[1] };
    const refusing = new DirectTransport(seller, () => ({ type: 'err', code: 'full', detail: 'busy' }));
    await expect(route(refusing, wallet).read({ hash: hashes[0], htl: 0 }))
      .rejects.toThrow('paid route provider refused the request (full): busy');

    const confused = new DirectTransport(seller, () => ({ type: 'none', hash: other }));
    await expect(route(confused, wallet).read({ hash: hashes[0], htl: 0 }))
      .rejects.toThrow('paid route provider sent an unexpected reply');

    const garbled: PaidBlobTransport = { exchange: async () => new TextEncoder().encode('d1:t4:nopee') };
    await expect(route(garbled, wallet).read({ hash: hashes[0], htl: 0 }))
      .rejects.toThrow('paid route provider sent an invalid paid-retrieval message: unknown reply type');
    expect(wallet.payments).toBe(0);
  });

  it('fails the read when the wallet itself fails', async () => {
    const { transport, hashes } = await setup();
    const broken: RetrievalWallet = {
      pay: async () => {
        throw new Error('database is locked');
      },
      balanceSat: async () => 0n,
    };
    const paid = route(transport, broken);
    await expect(paid.read({ hash: hashes[0], htl: 0 }))
      .rejects.toThrow('paid route wallet failure: database is locked');
    expect(paid.stats()).toMatchObject({ declinedLimit: 0, paidSat: 0n });

    const careless = {
      pay: async () => ({ mintUrl: MINT, token: 'cashuFake', amountSat: 5, feeSat: 0 }),
      balanceSat: async () => 0n,
    } as unknown as RetrievalWallet;
    await expect(route(transport, careless).read({ hash: hashes[0], htl: 0 }))
      .rejects.toThrow('the wallet returned an invalid payment');
  });
});
