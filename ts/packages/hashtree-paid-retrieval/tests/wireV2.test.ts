/**
 * The second shape of the exchange: accounts that are identities, hop
 * budgets, payments carried by `get`, and lock keys. Mirrors the client side
 * of `rust/crates/hashtree-paid-retrieval/tests/wire_v2.rs`.
 */

import { describe, expect, it } from 'vitest';
import { toHex, type Hash } from '@hashtree/core';
import {
  LOCK_KEY_BYTES,
  PaidBlobRoute,
  decodeRequest,
  lossMsat,
  newAccountState,
  type ClientPolicy,
  type PaidBlobTransport,
  type PaymentRequest,
  type Request,
  type RetrievalWallet,
} from '../src/index.js';
import {
  DirectTransport,
  FakeSeller,
  FakeWallet,
  MINT,
  TARIFF,
  accountForPeer,
  paymentOf,
} from './support/fakes.js';

const POLICY: Partial<ClientPolicy> = {
  maxTariff: TARIFF,
  mints: [MINT],
  topupSat: 5n,
  maxTokenFeeSat: 0n,
};

function route(transport: PaidBlobTransport, wallet: RetrievalWallet): PaidBlobRoute {
  return new PaidBlobRoute({ provider: 'seller', transport, wallet, policy: POLICY });
}

function kind(request: Request): string {
  if (request.type === 'pay') return 'pay';
  return request.payment === null ? 'get' : 'get+pay';
}

function gate(): { opened: Promise<void>; open: () => void } {
  let open = (): void => undefined;
  const opened = new Promise<void>((resolve) => {
    open = resolve;
  });
  return { opened, open };
}

describe('the reshaped exchange', () => {
  it('makes a first paid read two round trips, the token riding along', async () => {
    const seller = new FakeSeller();
    const { hash, data } = await seller.add(1, 4_096);
    const wallet = new FakeWallet(seller.mint, 21n);
    const seen: string[] = [];
    const transport = new DirectTransport(seller, (request, reply) => {
      seen.push(kind(request));
      return reply;
    });
    const paid = route(transport, wallet);

    const reply = await paid.read({ hash, htl: 0 });
    expect(reply.type === 'data' && toHex(reply.data)).toBe(toHex(data));
    expect(seen).toEqual(['get', 'get+pay']);
    const account = paid.accountState();
    expect([account.creditedMsat, account.chargedMsat]).toEqual([5_000n, 1_000n]);
    expect([account.pending, lossMsat(account)]).toEqual([null, 0n]);
  });

  it('names no account on an authenticated carrier and is charged as itself', async () => {
    const seller = new FakeSeller();
    const { hash } = await seller.add(1, 4_096);
    const named: Array<Uint8Array | null> = [];
    const transport = new DirectTransport(seller, (request, reply) => {
      named.push(request.account);
      return reply;
    });
    transport.peer = 'peer-a';
    transport.authenticated = true;
    const paid = route(transport, new FakeWallet(seller.mint, 21n));

    expect((await paid.read({ hash, htl: 0 })).type).toBe('data');
    expect(named.length).toBeGreaterThan(0);
    expect(named.every((account) => account === null), 'no bearer id on this carrier').toBe(true);
    const accounts = seller.accounts();
    expect(accounts).toHaveLength(1);
    expect(accounts[0].account).toBe(toHex(await accountForPeer('peer-a')));
    expect(accounts[0].paidMsat - accounts[0].chargedMsat).toBe(4_000n);
  });

  it('names its account on a carrier that does not authenticate it', async () => {
    const seller = new FakeSeller();
    const { hash } = await seller.add(1, 4_096);
    const named: Array<string | null> = [];
    const transport = new DirectTransport(seller, (request, reply) => {
      named.push(request.account && toHex(request.account));
      return reply;
    });
    // A transport without `authenticatesPeer` names the account.
    const plain: PaidBlobTransport = { exchange: (request) => transport.exchange(request) };
    const paid = route(plain, new FakeWallet(seller.mint, 21n));

    expect((await paid.read({ hash, htl: 0 })).type).toBe('data');
    expect(new Set(named)).toEqual(new Set([toHex(paid.accountState().account)]));
  });

  it('passes the hop budget of the read to the provider', async () => {
    const seller = new FakeSeller();
    const wallet = new FakeWallet(seller.mint, 21n);
    const sent: Request[] = [];
    const transport: PaidBlobTransport = {
      exchange: async (request) => {
        sent.push(decodeRequest(request));
        return new DirectTransport(seller).exchange(request);
      },
    };
    const paid = route(transport, wallet);

    expect(await paid.read({ hash: new Uint8Array(32).fill(9), htl: 4 })).toEqual({ type: 'no-result' });
    expect(await paid.read({ hash: new Uint8Array(32).fill(8), htl: 0 })).toEqual({ type: 'no-result' });
    expect(seller.hopBudgets).toEqual([4, 0]);
    expect(sent.map((request) => request.type === 'get' && request.htl)).toEqual([4, 0]);
  });

  it('hands the lock key of a quote to the wallet', async () => {
    const lock = new Uint8Array(LOCK_KEY_BYTES).fill(2);
    const seller = new FakeSeller({ lock });
    const { hash } = await seller.add(1, 4_096);
    const funded = new FakeWallet(seller.mint, 21n);
    const locks: Array<Uint8Array | null> = [];
    const wallet: RetrievalWallet = {
      pay: (request: PaymentRequest) => {
        locks.push(request.lock);
        return funded.pay(request);
      },
      balanceSat: () => funded.balanceSat(),
    };

    expect((await route(new DirectTransport(seller), wallet).read({ hash, htl: 0 })).type).toBe('data');
    expect(locks).toEqual([lock]);
  });
});

describe('carrying a token', () => {
  it('lets one request at a time carry a token, so each is sent once', async () => {
    const seller = new FakeSeller();
    const blobs: Hash[] = [];
    for (let seed = 0; seed < 4; seed += 1) blobs.push((await seller.add(seed, 4_096)).hash);
    const wallet = new FakeWallet(seller.mint, 21n);
    const sends = new Map<string, number>();
    const transport = new DirectTransport(seller, (request, reply) => {
      const payment = paymentOf(request);
      if (payment !== null) {
        const id = toHex(payment.id);
        sends.set(id, (sends.get(id) ?? 0) + 1);
      }
      return reply;
    });
    const paid = route(transport, wallet);

    const replies = await Promise.all(blobs.map((hash) => paid.read({ hash, htl: 0 })));
    expect(replies.map((reply) => reply.type)).toEqual(['data', 'data', 'data', 'data']);
    expect(wallet.payments).toBe(1);
    expect([...sends.values()]).toEqual([1]);
    expect(seller.received).toBe(5n);
    const account = paid.accountState();
    expect([account.chargedMsat, account.balanceMsat, lossMsat(account)]).toEqual([4_000n, 1_000n, 0n]);
  });

  it('settles a carried token the provider cannot take yet like a sent one', async () => {
    const seller = new FakeSeller();
    const { hash } = await seller.add(1, 4_096);
    const wallet = new FakeWallet(seller.mint, 21n);
    const seen: string[] = [];
    const transport = new DirectTransport(seller, (request, reply) => {
      seen.push(kind(request));
      return reply;
    });
    const paid = route(transport, wallet);

    seller.offline = true;
    await expect(paid.read({ hash, htl: 0 }))
      .rejects.toThrow('paid route provider could not take the payment yet: mint unreachable');
    expect(paid.accountState().pending?.attempts).toBe(1);

    // No longer fresh: the token is sent on its own before the next get.
    seller.offline = false;
    expect((await paid.read({ hash, htl: 0 })).type).toBe('data');
    expect(seen).toEqual(['get', 'get+pay', 'pay', 'get']);
    expect([wallet.payments, seller.received]).toEqual([1, 5n]);
  });

  it('waits for the payment lock no longer than the read may take', async () => {
    const seller = new FakeSeller();
    const first = await seller.add(1, 4_096);
    const second = await seller.add(2, 4_096);
    const wallet = new FakeWallet(seller.mint, 21n);
    const token = await wallet.pay({
      provider: 'seller',
      mints: [MINT],
      amountSat: 5n,
      maxFeeSat: 0n,
      lock: null,
      paymentId: new Uint8Array(16).fill(1),
    });
    const account = {
      ...newAccountState(),
      pending: { id: new Uint8Array(16).fill(1), payment: token, attempts: 0 },
    };
    // A request carrying the token stalls until the test lets it through.
    const arrived = gate();
    const stalled = gate();
    const direct = new DirectTransport(seller);
    const transport: PaidBlobTransport = {
      async exchange(request) {
        if (paymentOf(decodeRequest(request)) !== null) {
          arrived.open();
          await stalled.opened;
        }
        return direct.exchange(request);
      },
    };
    const paid = new PaidBlobRoute({ provider: 'seller', transport, wallet, policy: POLICY, account });
    const context = (ms: number) => ({
      signal: new AbortController().signal,
      deadlineMs: Date.now() + ms,
      attemptBudget: 1,
    });

    const carrying = paid.read({ hash: first.hash, htl: 0 }, context(5_000));
    await arrived.opened;
    await expect(paid.read({ hash: first.hash, htl: 0 }, context(30)))
      .rejects.toThrow('paid route payment is still in progress; availability is unknown');

    stalled.open();
    expect((await carrying).type).toBe('data');
    // The abandoned wait gave the lock back: payments go on.
    await paid.settle();
    expect((await paid.read({ hash: second.hash, htl: 0 })).type).toBe('data');
    expect([wallet.payments, seller.received]).toEqual([1, 5n]);
  });
});
