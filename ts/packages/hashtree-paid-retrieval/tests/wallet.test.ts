import { describe, expect, it } from 'vitest';
import {
  LimitedWallet,
  SpendingLimit,
  WalletError,
  type PaymentRequest,
  type RetrievalWallet,
  type WalletPayment,
} from '../src/index.js';

const request = (amountSat: bigint): PaymentRequest => ({
  provider: 'seller',
  mints: ['https://mint.test'],
  amountSat,
  maxFeeSat: 0n,
  paymentId: new Uint8Array(16).fill(Number(amountSat)),
});

function paying(feeSat = 0n): RetrievalWallet {
  return {
    pay: async ({ amountSat }): Promise<WalletPayment> => ({
      mintUrl: 'https://mint.test',
      token: 'cashuFake',
      amountSat,
      feeSat,
    }),
    balanceSat: async () => 100n,
    reclaim: async () => undefined,
  };
}

describe('SpendingLimit', () => {
  it('is readable and adjustable for a settings screen', () => {
    const limit = new SpendingLimit(10n, 4n);
    expect([limit.limitSat, limit.spentSat, limit.remainingSat]).toEqual([10n, 4n, 6n]);
    limit.setLimitSat(3n);
    expect(limit.remainingSat).toBe(0n);
    void limit.resetSpent();
    expect([limit.spentSat, limit.remainingSat]).toEqual([0n, 3n]);
  });
});

describe('LimitedWallet', () => {
  it('counts the token and its fee, and passes the balance through', async () => {
    const limit = new SpendingLimit(10n);
    const wallet = new LimitedWallet(paying(1n), limit);
    await wallet.pay(request(5n));
    expect([limit.spentSat, limit.remainingSat]).toEqual([6n, 4n]);
    expect(await wallet.balanceSat()).toBe(100n);
    expect(wallet.limit).toBe(limit);
  });

  it('refuses before the wallet is asked and names what is left', async () => {
    const limit = new SpendingLimit(4n);
    let asked = 0;
    const inner = paying();
    const wallet = new LimitedWallet({ ...inner, pay: (payment) => {
      asked += 1;
      return inner.pay(payment);
    } }, limit);

    const refusal = await wallet.pay(request(5n)).catch((error: unknown) => error);
    expect(refusal).toBeInstanceOf(WalletError);
    expect(refusal).toMatchObject({
      kind: 'limit-reached',
      isDecline: true,
      message: 'spending limit reached: 5 sat requested, 4 sat left',
    });
    expect([asked, limit.spentSat]).toEqual([0, 0n]);
  });

  it('reserves the amount so that concurrent payments cannot overspend together', async () => {
    const limit = new SpendingLimit(8n);
    const wallet = new LimitedWallet(paying(), limit);
    const outcomes = await Promise.allSettled([wallet.pay(request(5n)), wallet.pay(request(5n))]);
    expect(outcomes.map((outcome) => outcome.status)).toEqual(['fulfilled', 'rejected']);
    expect(limit.spentSat).toBe(5n);
  });

  it('releases the reservation when the wallet does not pay', async () => {
    const limit = new SpendingLimit(10n);
    const wallet = new LimitedWallet({
      pay: async () => {
        throw WalletError.insufficientFunds();
      },
      balanceSat: async () => 0n,
    }, limit);
    await expect(wallet.pay(request(5n))).rejects.toMatchObject({ kind: 'insufficient-funds' });
    expect(limit.spentSat).toBe(0n);
  });

  it('gives the amount back to the limit when a token is reclaimed', async () => {
    const limit = new SpendingLimit(10n);
    const wallet = new LimitedWallet(paying(), limit);
    const payment = await wallet.pay(request(5n));
    await wallet.reclaim(payment);
    expect(limit.spentSat).toBe(0n);

    const fixed = new LimitedWallet({ pay: paying().pay, balanceSat: async () => 0n }, limit);
    await expect(fixed.reclaim(payment)).rejects.toMatchObject({
      kind: 'failed',
      isDecline: false,
      message: 'wallet failure: this wallet cannot reclaim tokens',
    });
  });

  it('treats anything else a wallet throws as a failure, not a refusal', () => {
    expect(WalletError.from(new Error('boom'))).toMatchObject({
      kind: 'failed',
      message: 'wallet failure: boom',
    });
    const declined = WalletError.noAcceptedMint();
    expect(WalletError.from(declined)).toBe(declined);
  });
});
