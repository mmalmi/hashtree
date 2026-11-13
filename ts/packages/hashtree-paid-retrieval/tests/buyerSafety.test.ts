/**
 * What protects the buyer's money: fee and credit accounting, what is saved
 * before a payment, and tokens whose fate is not known. Mirrors
 * `rust/crates/hashtree-paid-retrieval/tests/buyer_safety.rs` case by case.
 */

import { describe, expect, it } from 'vitest';
import type { BlobReply, Hash } from '@hashtree/core';
import {
  LimitedWallet,
  PaidBlobRoute,
  SpendingLimit,
  WalletError,
  decodeAccountState,
  encodeAccountState,
  lossMsat,
  newAccountState,
  unresolvedMsat,
  type AccountState,
  type AccountStore,
  type ClientPolicy,
  type PaidBlobTransport,
  type PaymentRequest,
  type PendingPayment,
  type RetrievalWallet,
  type SpendStore,
} from '../src/index.js';
import {
  DirectTransport,
  FakeMint,
  FakeSeller,
  FakeWallet,
  MINT,
  MemoryAccountStore,
  RecordingStore,
  TARIFF,
  paymentOf,
} from './support/fakes.js';

const POLICY: Partial<ClientPolicy> = {
  maxTariff: TARIFF,
  mints: [MINT],
  topupSat: 5n,
  maxPrepaidMsat: 10_000n,
  maxLossMsat: 1_500n,
  maxTokenFeeSat: 0n,
  maxRedeemFeeMsat: 1_000n,
};

function route(
  transport: PaidBlobTransport,
  wallet: RetrievalWallet,
  saved: { account?: AccountState; store?: AccountStore } = {},
): PaidBlobRoute {
  return new PaidBlobRoute({ provider: 'seller', transport, wallet, policy: POLICY, ...saved });
}

function read(paid: PaidBlobRoute, hash: Hash): Promise<BlobReply> {
  return paid.read({ hash, htl: 0 });
}

/** How a read ended, without megabytes in a failure message. */
function outcome(paid: PaidBlobRoute, hash: Hash): Promise<'data' | 'no-result' | 'failed'> {
  return read(paid, hash).then((reply) => reply.type, () => 'failed' as const);
}

function request(amountSat: bigint): PaymentRequest {
  return {
    provider: 'seller',
    mints: [MINT],
    amountSat,
    maxFeeSat: 1n,
    lock: null,
    paymentId: new Uint8Array(16).fill(Number(amountSat)),
  };
}

describe('fees and under-crediting', () => {
  it('records a small redemption fee, which is not a loss', async () => {
    const seller = new FakeSeller({ mintFeeSat: 1n });
    const { hash } = await seller.add(1, 4_096);
    const paid = route(new DirectTransport(seller), new FakeWallet(seller.mint, 21n));

    expect(await outcome(paid, hash)).toBe('data');
    const account = paid.accountState();
    expect(account.creditedMsat, '5 sat paid, 1 kept by the mint').toBe(4_000n);
    expect([lossMsat(account), account.blocked]).toEqual([0n, null]);
    expect(paid.stats().redeemFeeMsat).toBe(1_000n);
  });

  it('catches a provider that credits less than it was paid', async () => {
    // Three of five sats vanish at the provider: more than any mint fee.
    const seller = new FakeSeller({ mintFeeSat: 3n });
    const { hash } = await seller.add(1, 4_096);
    const second = await seller.add(2, 4_096);
    const wallet = new FakeWallet(seller.mint, 21n);
    const paid = route(new DirectTransport(seller), wallet);

    // The token rode with the request and still covered this blob.
    expect(await outcome(paid, hash)).toBe('data');
    expect(await outcome(paid, second.hash)).toBe('no-result');
    const account = paid.accountState();
    expect(lossMsat(account), 'the shortfall beyond the allowance').toBe(2_000n);
    expect(account.blocked).not.toBeNull();
    expect(wallet.payments, 'and it is not paid again').toBe(1);
  });

  it('reserves the fee in the spending limit before the wallet is asked', async () => {
    const funded = new FakeWallet(new FakeMint(), 21n);
    funded.feeSat = 1n;
    const limit = new SpendingLimit(5n);
    const wallet = new LimitedWallet(funded, limit);

    // Five sats fit the limit, five plus a one sat fee do not.
    const refusal = await wallet.pay(request(5n)).catch((error: unknown) => error);
    expect(refusal).toBeInstanceOf(WalletError);
    expect(refusal).toMatchObject({
      kind: 'limit-reached',
      message: 'spending limit reached: 6 sat requested, 5 sat left',
    });
    expect([funded.payments, limit.spentSat]).toEqual([0, 0n]);

    await wallet.pay(request(4n));
    expect([limit.spentSat, limit.remainingSat]).toEqual([5n, 0n]);
  });

  it('saves spending before a payment, and a failed save stops it', async () => {
    const recording = (fails: boolean): SpendStore & { saved: bigint[] } => ({
      saved: [],
      async saveSpent(spentSat) {
        this.saved.push(spentSat);
        if (fails) throw new Error('disk full');
      },
    });
    const mint = new FakeMint();

    const saved = recording(false);
    const limit = new SpendingLimit(10n, 2n, saved);
    await new LimitedWallet(new FakeWallet(mint, 21n), limit).pay(request(4n));
    // The reservation with its fee ceiling first, then the real cost.
    expect(saved.saved).toEqual([7n, 6n]);
    expect(limit.spentSat).toBe(6n);

    const funded = new FakeWallet(mint, 21n);
    const unsaved = new SpendingLimit(10n, 0n, recording(true));
    await expect(new LimitedWallet(funded, unsaved).pay(request(4n))).rejects.toMatchObject({
      kind: 'failed',
      message: 'wallet failure: cannot save the spending record: disk full',
    });
    expect([funded.payments, unsaved.spentSat]).toEqual([0, 0n]);
  });
});

describe('what is saved before acting', () => {
  it('finds a token made just before a crash again instead of paying twice', async () => {
    const seller = new FakeSeller();
    const { hash } = await seller.add(1, 4_096);
    const wallet = new FakeWallet(seller.mint, 21n);
    const transport = new DirectTransport(seller);
    const store = new RecordingStore();
    const paid = route(transport, wallet, { store });

    // The wallet creates the token and the process dies before the route
    // learns of it.
    wallet.loseReply = true;
    await expect(read(paid, hash)).rejects.toThrow('paid route wallet failure: the process died');
    expect(wallet.payments).toBe(1);
    const saved = store.last();
    expect(saved.intent?.amountSat, 'saved before the wallet was asked').toBe(5n);
    expect(saved.pending).toBeNull();

    // After the restart the wallet is asked about that payment id.
    const restarted = route(transport, wallet, { account: saved, store });
    expect(await outcome(restarted, hash)).toBe('data');
    expect(wallet.payments, 'the same token, not a second one').toBe(1);
    expect(seller.received).toBe(5n);
    expect(store.last()).toMatchObject({ intent: null, pending: null, creditedMsat: 5_000n });
  });

  it('saves a token before it is sent', async () => {
    const seller = new FakeSeller();
    const { hash } = await seller.add(1, 4_096);
    const store = new RecordingStore();
    const savedFirst: boolean[] = [];
    // The provider receives the token only after it was saved as pending.
    const transport = new DirectTransport(seller, (sent, reply) => {
      const payment = paymentOf(sent);
      if (payment !== null) savedFirst.push(store.last().pending?.payment.token === payment.token);
      return reply;
    });
    const paid = route(transport, new FakeWallet(seller.mint, 21n), { store });

    expect(await outcome(paid, hash)).toBe('data');
    expect(savedFirst).toEqual([true]);
  });

  it('does not ask the wallet when the intent cannot be saved', async () => {
    const seller = new FakeSeller();
    const { hash } = await seller.add(1, 4_096);
    const wallet = new FakeWallet(seller.mint, 21n);
    const store: AccountStore = {
      save: async () => {
        throw new Error('quota exceeded');
      },
    };
    const paid = route(new DirectTransport(seller), wallet, { store });

    await expect(read(paid, hash)).rejects.toThrow(
      'paid route cannot save its account: quota exceeded',
    );
    expect(wallet.payments).toBe(0);
    expect(paid.accountState().intent).toBeNull();
  });

  it('forgets an intent the wallet knows nothing about', async () => {
    const seller = new FakeSeller();
    const { hash } = await seller.add(1, 4_096);
    const funded = new FakeWallet(seller.mint, 21n);
    // A wallet that cannot find payments again.
    const wallet: RetrievalWallet = {
      pay: (payment) => funded.pay(payment),
      balanceSat: () => funded.balanceSat(),
    };
    const account: AccountState = {
      ...newAccountState(),
      intent: { id: new Uint8Array(16).fill(7), amountSat: 5n, mints: [MINT] },
    };
    const store = new RecordingStore();
    const paid = route(new DirectTransport(seller), wallet, { account, store });

    expect(await outcome(paid, hash)).toBe('data');
    expect(funded.payments).toBe(1);
    expect(store.saves[0].intent, 'cleared before anything new is paid').toBeNull();
    expect(paid.accountState()).toMatchObject({ intent: null, pending: null });
  });

  it('carries the balance across a restart through a store', async () => {
    const seller = new FakeSeller();
    const blobs = [];
    for (let seed = 0; seed < 3; seed += 1) blobs.push(await seller.add(seed, 4_096));
    const wallet = new FakeWallet(seller.mint, 21n);
    const transport = new DirectTransport(seller);
    const store = new MemoryAccountStore();
    expect(store.load()).toBeNull();

    const paid = route(transport, wallet, { store });
    expect(await outcome(paid, blobs[0].hash)).toBe('data');
    expect(await outcome(paid, blobs[1].hash)).toBe('data');

    // The record was written when the payment was credited; the two charges
    // since then were not saved one by one.
    const saved = store.load();
    expect(saved).toMatchObject({ creditedMsat: 5_000n, chargedMsat: 0n });

    const restarted = route(transport, wallet, { account: saved ?? undefined, store });
    expect(await outcome(restarted, blobs[2].hash)).toBe('data');
    // The provider's balance is the baseline, not a 2000 msat loss.
    const account = restarted.accountState();
    expect([account.balanceMsat, lossMsat(account), account.blocked]).toEqual([2_000n, 0n, null]);
    expect(restarted.stats().resyncedMsat).toBe(2_000n);
    expect(wallet.payments).toBe(1);
  });

  it('keeps an account through its own encoding', () => {
    const token = (seed: number): PendingPayment => ({
      id: new Uint8Array(16).fill(seed),
      attempts: seed,
      payment: {
        mintUrl: MINT,
        token: `cashuFake${seed}`,
        amountSat: 5n,
        feeSat: 1n,
        ...(seed % 2 === 0 ? { reclaimId: `operation-${seed}` } : {}),
      },
    });
    const account: AccountState = {
      ...newAccountState(),
      creditedMsat: 9_000n,
      chargedMsat: 4_096n,
      balanceMsat: -17n,
      intent: { id: new Uint8Array(16).fill(9), amountSat: 5n, mints: [MINT, 'https://other.test'] },
      pending: token(1),
      unresolved: [token(2), token(3)],
      blocked: 'provider balance is low',
    };
    expect(decodeAccountState(encodeAccountState(account))).toEqual(account);
    const fresh = newAccountState();
    expect(decodeAccountState(encodeAccountState(fresh))).toEqual(fresh);

    const damaged = encodeAccountState(account).slice(0, -1);
    expect(() => decodeAccountState(damaged)).toThrow('account record is damaged');
    expect(() => decodeAccountState(new TextEncoder().encode('de'))).toThrow('account');
  });
});

describe('tokens whose fate is not known', () => {
  it('keeps a refused token that cannot be reclaimed instead of dropping it', async () => {
    const seller = new FakeSeller();
    const { hash } = await seller.add(1, 4_096);
    const wallet = new FakeWallet(seller.mint, 21n);
    // The provider answers every payment as refused, without redeeming.
    seller.offline = true;
    const transport = new DirectTransport(seller, (request, reply) => {
      const payment = paymentOf(request);
      return payment === null
        ? reply
        : { type: 'err', code: 'invalid', payment: payment.id, detail: 'refused' };
    });
    const paid = route(transport, wallet);

    wallet.cannotReclaim = true;
    await expect(read(paid, hash)).rejects.toThrow('refused the payment (invalid): refused');
    const account = paid.accountState();
    expect(account.pending).toBeNull();
    expect(account.unresolved, 'neither credited nor reclaimed').toHaveLength(1);
    expect(unresolvedMsat(account)).toBe(5_000n);
    expect([wallet.balance, paid.stats().paidSat]).toEqual([16n, 5n]);

    // Once the wallet can reach its mint again, settling takes it back.
    wallet.cannotReclaim = false;
    await paid.settle();
    expect(paid.accountState().unresolved).toEqual([]);
    expect([wallet.balance, paid.stats().paidSat]).toEqual([21n, 0n]);
  });

  it('bounds a provider that keeps saying not yet, and settles later', async () => {
    const seller = new FakeSeller();
    const { hash } = await seller.add(1, 4_096);
    const wallet = new FakeWallet(seller.mint, 21n);
    const paid = route(new DirectTransport(seller), wallet);

    // The provider cannot reach its mint: every payment is "not yet".
    seller.offline = true;
    const outcomes = [];
    for (let attempt = 0; attempt < 7; attempt += 1) outcomes.push(await outcome(paid, hash));
    // Two top-ups reach the prepaid cap; after that nothing more is paid.
    expect(wallet.payments).toBe(2);
    expect(outcomes.at(-1)).toBe('no-result');
    let account = paid.accountState();
    expect([account.pending, account.unresolved.length]).toEqual([null, 2]);
    expect(unresolvedMsat(account)).toBe(10_000n);
    expect(paid.stats().declinedLimit)
      .toBe(outcomes.filter((result) => result === 'no-result').length);

    // The mint is back: the tokens the provider held all along are credited.
    seller.offline = false;
    expect(await outcome(paid, hash)).toBe('data');
    await paid.settle();
    account = paid.accountState();
    expect(account.unresolved).toEqual([]);
    expect([account.creditedMsat, account.balanceMsat]).toEqual([10_000n, 9_000n]);
    expect([wallet.payments, lossMsat(account)]).toEqual([2, 0n]);
  });
});
