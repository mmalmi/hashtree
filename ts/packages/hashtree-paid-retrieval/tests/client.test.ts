/**
 * The paid route as a client uses it, including what goes wrong. Mirrors
 * `rust/crates/hashtree-paid-retrieval/tests/client.rs` case by case.
 */

import { describe, expect, it } from 'vitest';
import type { BlobReply, Hash } from '@hashtree/core';
import {
  LimitedWallet,
  PaidBlobRoute,
  SpendingLimit,
  lossMsat,
  type ClientPolicy,
  type Reply,
  type RetrievalWallet,
} from '../src/index.js';
import { DirectTransport, FakeSeller, FakeWallet, MINT, TARIFF, paymentOf } from './support/fakes.js';

const TWO_MIB = 2 * 1024 * 1024;

function policy(overrides: Partial<ClientPolicy> = {}): Partial<ClientPolicy> {
  return {
    maxTariff: TARIFF,
    mints: [MINT],
    topupSat: 5n,
    maxPrepaidMsat: 10_000n,
    maxLossMsat: 1_500n,
    maxTokenFeeSat: 0n,
    ...overrides,
  };
}

function route(
  transport: DirectTransport,
  wallet: RetrievalWallet,
  overrides: Partial<ClientPolicy> = {},
): PaidBlobRoute {
  return new PaidBlobRoute({ provider: 'seller', transport, wallet, policy: policy(overrides) });
}

function read(paid: PaidBlobRoute, hash: Hash): Promise<BlobReply> {
  return paid.read({ hash, htl: 0 });
}

const NO_RESULT: BlobReply = { type: 'no-result' };

/** `toEqual` on megabytes is slow; compare the bytes directly. */
function expectData(reply: BlobReply, expected: Uint8Array): void {
  expect(reply.type).toBe('data');
  const actual = reply.type === 'data' ? reply.data : new Uint8Array();
  expect(actual.length).toBe(expected.length);
  expect(actual.every((byte, index) => byte === expected[index])).toBe(true);
}

/**
 * Loses the first reply that matches, after the provider acted on the
 * request.
 */
function losingFirst(matches: (reply: Reply) => boolean): (request: unknown, reply: Reply) => Reply | null {
  let lose = true;
  return (_request, reply) => {
    if (matches(reply) && lose) {
      lose = false;
      return null;
    }
    return reply;
  };
}

describe('PaidBlobRoute', () => {
  it('pays for several blobs with one top-up', async () => {
    const seller = new FakeSeller();
    const first = await seller.add(1, TWO_MIB);
    const second = await seller.add(2, TWO_MIB);
    const wallet = new FakeWallet(seller.mint, 21n);
    const transport = new DirectTransport(seller);
    const paid = route(transport, wallet);

    expectData(await read(paid, first.hash), first.data);
    expect(transport.exchanges, 'get, then get carrying the token').toBe(2);
    expectData(await read(paid, second.hash), second.data);
    expect(transport.exchanges, 'a funded read is one round trip').toBe(3);

    expect(wallet.payments).toBe(1);
    expect(wallet.balance).toBe(16n);
    expect(seller.received).toBe(5n);
    const account = paid.accountState();
    expect(account.creditedMsat).toBe(5_000n);
    expect(account.chargedMsat).toBe(4_096n);
    expect(account.balanceMsat).toBe(904n);
    expect(lossMsat(account)).toBe(0n);
    expect(paid.stats()).toMatchObject({ blobs: 2, bytes: 2 * TWO_MIB, paidSat: 5n });

    // Asking again right away is a free repeat and is not counted twice.
    expect((await read(paid, first.hash)).type).toBe('data');
    expect(paid.accountState()).toMatchObject({ chargedMsat: 4_096n, balanceMsat: 904n });
  });

  it('reports a missing blob as no result, at no cost', async () => {
    const seller = new FakeSeller();
    const wallet = new FakeWallet(seller.mint, 21n);
    const paid = route(new DirectTransport(seller), wallet);

    expect(await read(paid, new Uint8Array(32).fill(3))).toEqual(NO_RESULT);
    expect(wallet.payments).toBe(0);
  });

  it('stops at the spending limit before any payment is made', async () => {
    const seller = new FakeSeller();
    const first = await seller.add(1, TWO_MIB);
    const second = await seller.add(2, TWO_MIB);
    const funded = new FakeWallet(seller.mint, 21n);
    const limit = new SpendingLimit(2n);
    const paid = route(new DirectTransport(seller), new LimitedWallet(funded, limit));

    // The blob costs 2048 msat: three sats are needed, two are allowed.
    expect(await read(paid, first.hash)).toEqual(NO_RESULT);
    expect(paid.stats().declinedLimit).toBe(1);
    expect([funded.payments, limit.spentSat]).toEqual([0, 0n]);

    // Raising the limit lets the minimum through, not the preferred top-up.
    limit.setLimitSat(4n);
    expectData(await read(paid, first.hash), first.data);
    expect([limit.spentSat, limit.remainingSat]).toEqual([3n, 1n]);

    // The rest of the limit does not cover another blob.
    expect(await read(paid, second.hash)).toEqual(NO_RESULT);
    expect(limit.spentSat).toBe(3n);
    expect(funded.balance).toBe(18n);
  });

  it('bounds what one provider holds by the prepaid cap', async () => {
    const seller = new FakeSeller();
    const { hash } = await seller.add(1, TWO_MIB);
    const wallet = new FakeWallet(seller.mint, 21n);
    const paid = route(new DirectTransport(seller), wallet, { maxPrepaidMsat: 2_000n });

    expect(await read(paid, hash)).toEqual(NO_RESULT);
    expect(paid.stats().declinedLimit).toBe(1);
    expect(wallet.payments).toBe(0);
  });

  it('declines a quote above the accepted tariff', async () => {
    const seller = new FakeSeller({ tariff: { ...TARIFF, minMsat: 5_000n } });
    const { hash } = await seller.add(1, 4_096);
    const wallet = new FakeWallet(seller.mint, 21n);
    const paid = route(new DirectTransport(seller), wallet);

    expect(await read(paid, hash)).toEqual(NO_RESULT);
    expect(paid.stats().declinedPrice).toBe(1);
    expect(wallet.payments).toBe(0);
  });

  it('does not pay a provider without a trusted mint', async () => {
    const seller = new FakeSeller();
    const { hash } = await seller.add(1, 4_096);
    const wallet = new FakeWallet(seller.mint, 21n);
    const paid = route(new DirectTransport(seller), wallet, { mints: ['https://other.test'] });

    expect(await read(paid, hash)).toEqual(NO_RESULT);
    expect(paid.stats().declinedMint).toBe(1);
    expect(wallet.payments).toBe(0);
  });

  it('resends a payment whose acknowledgement was lost instead of paying again', async () => {
    const seller = new FakeSeller();
    const blob = await seller.add(1, TWO_MIB);
    const wallet = new FakeWallet(seller.mint, 21n);
    // The acknowledgement comes as `paid`, or as the credit with the blob.
    const acknowledges = (reply: Reply): boolean => reply.type === 'paid'
      || (reply.type === 'data' && reply.credit !== null);
    const paid = route(new DirectTransport(seller, losingFirst(acknowledges)), wallet);

    await expect(read(paid, blob.hash)).rejects.toThrow('connection lost');
    expect(paid.accountState().pending?.payment.amountSat, 'the token is kept').toBe(5n);

    expectData(await read(paid, blob.hash), blob.data);
    expect(wallet.payments, 'the same token was sent again').toBe(1);
    expect(seller.received, 'and credited once').toBe(5n);
    const account = paid.accountState();
    expect(account.pending).toBeNull();
    expect([account.creditedMsat, lossMsat(account)]).toEqual([5_000n, 0n]);
  });

  it('repeats a lost delivery without a second charge', async () => {
    const seller = new FakeSeller();
    const blob = await seller.add(1, TWO_MIB);
    const wallet = new FakeWallet(seller.mint, 21n);
    const paid = route(
      new DirectTransport(seller, losingFirst((reply) => reply.type === 'data')),
      wallet,
    );

    await expect(read(paid, blob.hash)).rejects.toThrow('connection lost');
    expectData(await read(paid, blob.hash), blob.data);

    expect(wallet.payments).toBe(1);
    expect(seller.accounts()[0].chargedMsat).toBe(2_048n);
    const account = paid.accountState();
    expect([account.chargedMsat, account.balanceMsat]).toEqual([2_048n, 2_952n]);
    expect(lossMsat(account)).toBe(0n);
  });

  it('cuts off a provider that takes payment and never delivers', async () => {
    const seller = new FakeSeller();
    const hashes: Hash[] = [];
    for (let seed = 0; seed < 6; seed += 1) hashes.push((await seller.add(seed, 4_096)).hash);
    const wallet = new FakeWallet(seller.mint, 21n);
    // Payments are credited and blobs are charged, but no data ever arrives.
    const transport = new DirectTransport(
      seller,
      (_request, reply) => (reply.type === 'data' ? null : reply),
    );
    const paid = route(transport, wallet, { topupSat: 2n, maxPrepaidMsat: 2_000n });

    for (const hash of hashes) {
      const outcome = await read(paid, hash).then((reply) => reply.type, () => 'failed');
      expect(outcome).not.toBe('data');
    }

    expect(wallet.payments, 'no second top-up after the first vanished').toBe(1);
    expect(wallet.balance, 'the loss is the prepaid cap, not the wallet').toBe(19n);
    const account = paid.accountState();
    expect(lossMsat(account)).toBe(2_000n);
    expect(account.blocked).not.toBeNull();

    // A blocked provider is not contacted at all.
    const before = transport.exchanges;
    expect(await read(paid, hashes[0])).toEqual(NO_RESULT);
    expect(transport.exchanges).toBe(before);
    expect(paid.stats().declinedBlocked).toBeGreaterThanOrEqual(1);

    // The application may write the loss off and try again.
    paid.clearBlock();
    expect(lossMsat(paid.accountState())).toBe(0n);
    expect(paid.accountState().blocked).toBeNull();
  });

  it('never returns wrong bytes or counts them as a delivery', async () => {
    const seller = new FakeSeller();
    const { hash } = await seller.add(1, 4_096);
    const wallet = new FakeWallet(seller.mint, 21n);
    const transport = new DirectTransport(seller, (_request, reply) => {
      if (reply.type !== 'data') return reply;
      const corrupted = reply.data.slice();
      corrupted[0] ^= 1;
      return { ...reply, data: corrupted };
    });
    const paid = route(transport, wallet);

    await expect(read(paid, hash)).rejects.toThrow('wrong hash');
    const account = paid.accountState();
    expect(account.chargedMsat).toBe(0n);
    expect(lossMsat(account)).toBe(1_000n);
    expect(paid.stats().invalidBlobs).toBe(1);
  });

  it('blocks an overcharging provider', async () => {
    const seller = new FakeSeller();
    const first = await seller.add(1, 4_096);
    const second = await seller.add(2, 4_096);
    const wallet = new FakeWallet(seller.mint, 21n);
    const transport = new DirectTransport(seller, (_request, reply) => (reply.type === 'data'
      ? { ...reply, chargedMsat: 4_000n, balanceMsat: reply.balanceMsat - 3_000n, credit: null }
      : reply));
    const paid = route(transport, wallet);

    // The verified data is still used; the provider is not paid again.
    expectData(await read(paid, first.hash), first.data);
    expect(paid.accountState().blocked).toContain('charged 4000 msat');
    expect(await read(paid, second.hash)).toEqual(NO_RESULT);
    expect(wallet.payments).toBe(1);
  });

  it('tops up again when the wallet underpays, until the blob is covered', async () => {
    const seller = new FakeSeller();
    const blob = await seller.add(1, TWO_MIB);
    const wallet = new FakeWallet(seller.mint, 21n);
    wallet.shortSat = 3n;
    const paid = route(new DirectTransport(seller), wallet);

    // Asked for 5 sat it pays 2: 2000 msat does not cover 2048, so the
    // provider quotes again and the route pays the difference.
    expectData(await read(paid, blob.hash), blob.data);
    expect(wallet.payments).toBe(2);
    expect(paid.accountState().creditedMsat).toBe(4_000n);
  });

  it('reclaims a refused token', async () => {
    const seller = new FakeSeller();
    const { hash } = await seller.add(1, 4_096);
    const wallet = new FakeWallet(seller.mint, 21n);
    // The provider answers every payment as if the token were worthless.
    const transport = new DirectTransport(seller, (request, reply) => {
      const payment = paymentOf(request);
      return payment === null
        ? reply
        : { type: 'err', code: 'invalid', payment: payment.id, detail: 'refused' };
    });
    seller.offline = true;
    const paid = route(transport, wallet);

    await expect(read(paid, hash)).rejects.toThrow('refused the payment (invalid): refused');
    expect(wallet.balance, 'the unredeemed token came back').toBe(21n);
    expect(paid.accountState().pending).toBeNull();
    expect(paid.stats()).toMatchObject({ rejectedPayments: 1, paidSat: 0n });
  });

  it('is served first under grace and settles afterwards', async () => {
    const seller = new FakeSeller({ graceMsat: 1_500n });
    const first = await seller.add(1, 4_096);
    const wallet = new FakeWallet(seller.mint, 21n);
    const transport = new DirectTransport(seller);
    transport.peer = 'peer-a';
    const paid = route(transport, wallet);

    expectData(await read(paid, first.hash), first.data);
    expect(wallet.payments, 'delivered before any payment').toBe(0);
    expect(paid.accountState().balanceMsat).toBe(-1_000n);

    // The provider's terms are learned from a quote; the next read settles.
    const second = await seller.add(2, 4_096);
    expectData(await read(paid, second.hash), second.data);
    expect(wallet.payments).toBe(1);
    expect(paid.accountState().balanceMsat).toBeGreaterThanOrEqual(0n);
    expect(lossMsat(paid.accountState())).toBe(0n);
  });

  it('shares one top-up among concurrent reads', async () => {
    const seller = new FakeSeller();
    const blobs: Array<{ hash: Hash; data: Uint8Array }> = [];
    for (let seed = 0; seed < 4; seed += 1) blobs.push(await seller.add(seed, 4_096));
    const wallet = new FakeWallet(seller.mint, 21n);
    const paid = route(new DirectTransport(seller), wallet);

    const replies = await Promise.all(blobs.map((blob) => read(paid, blob.hash)));
    replies.forEach((reply, index) => expectData(reply, blobs[index].data));

    expect(wallet.payments, '5 sat covers four 1000 msat blobs').toBe(1);
    const account = paid.accountState();
    expect([account.chargedMsat, account.balanceMsat]).toEqual([4_000n, 1_000n]);
    expect([lossMsat(account), account.blocked]).toEqual([0n, null]);
  });
});
