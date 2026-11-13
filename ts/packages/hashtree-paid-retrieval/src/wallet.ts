/** The wallet an application plugs into a paid route, and a spending limit. */

import { saturate } from './price.js';

/** "Pay this much to this provider." */
export interface PaymentRequest {
  /** Application-level name of the provider, for the wallet's records. */
  provider: string;
  /**
   * Mints the provider accepts, already narrowed by the client policy. The
   * wallet must pay only with a mint it trusts itself.
   */
  mints: string[];
  amountSat: bigint;
  /** The most that creating the token may cost beyond `amountSat`. */
  maxFeeSat: bigint;
  /**
   * The 33-byte key the provider asks tokens to be locked to (NUT-11 P2PK). A
   * wallet that can lock tokens should; one that cannot sends them unlocked.
   */
  lock: Uint8Array | null;
  /**
   * Stable id of this payment, 16 bytes. It was saved before the wallet was
   * asked, and a wallet that can find a payment again finds it by this id.
   */
  paymentId: Uint8Array;
}

/** A token handed to the provider. */
export interface WalletPayment {
  mintUrl: string;
  token: string;
  amountSat: bigint;
  /** What creating the token cost on top of `amountSat`. */
  feeSat: bigint;
  /** Wallet handle for taking an unredeemed token back. */
  reclaimId?: string;
}

export type WalletErrorKind =
  | 'limit-reached'
  | 'no-accepted-mint'
  | 'insufficient-funds'
  | 'failed';

/**
 * How a wallet refuses or fails. Anything else a wallet throws is treated as
 * `failed`: a fault, not a decision.
 */
export class WalletError extends Error {
  private constructor(readonly kind: WalletErrorKind, message: string) {
    super(message);
    this.name = 'WalletError';
  }

  static limitReached(requestedSat: bigint, remainingSat: bigint): WalletError {
    return new WalletError(
      'limit-reached',
      `spending limit reached: ${requestedSat} sat requested, ${remainingSat} sat left`,
    );
  }

  static noAcceptedMint(): WalletError {
    return new WalletError('no-accepted-mint', "the wallet trusts none of the provider's mints");
  }

  static insufficientFunds(): WalletError {
    return new WalletError('insufficient-funds', 'the wallet balance is too low');
  }

  static failed(reason: string): WalletError {
    return new WalletError('failed', `wallet failure: ${reason}`);
  }

  /** Whatever a wallet threw, as a `WalletError`. */
  static from(error: unknown): WalletError {
    if (error instanceof WalletError) return error;
    return WalletError.failed(describe(error));
  }

  /** The wallet chose not to pay; nothing is wrong with it or the provider. */
  get isDecline(): boolean {
    return this.kind !== 'failed';
  }
}

function describe(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

export interface RetrievalWallet {
  /**
   * Create a token of exactly `amountSat` for the provider, or throw a
   * `WalletError`. Asked again with the same `paymentId`, a wallet that keeps
   * its payments should return the same token instead of a second one.
   */
  pay(request: PaymentRequest): Promise<WalletPayment>;

  /**
   * The token created for `paymentId`, or `null` if this wallet created none
   * or cannot produce it any more. Asked after a crash between `pay` and the
   * buyer's own record of the token.
   */
  recover?(paymentId: Uint8Array): Promise<WalletPayment | null>;

  /** Spendable balance, for display and for refusing early. */
  balanceSat(): Promise<bigint>;

  /** Take back a token the provider did not redeem. */
  reclaim?(payment: WalletPayment): Promise<void>;
}

/**
 * Where a `SpendingLimit` keeps what was spent, so that a restart does not
 * hand the budget back. `saveSpent` must be durable when it resolves.
 */
export interface SpendStore {
  saveSpent(spentSat: bigint): Promise<void>;
}

interface LimitState {
  limitSat: bigint;
  spentSat: bigint;
  store?: SpendStore;
  /** Saves reach the store one at a time, in the order they were made. */
  saving: Promise<void>;
}

let stateOf: (limit: SpendingLimit) => LimitState;

/** Hand the current total to the store. Rejects when it was not saved. */
function save(state: LimitState): Promise<void> {
  const { store, spentSat } = state;
  if (!store) return Promise.resolve();
  const saved = state.saving.then(() => store.saveSpent(spentSat));
  state.saving = saved.catch(() => undefined);
  return saved;
}

/** A total spending cap shared by any number of routes. */
export class SpendingLimit {
  readonly #state: LimitState;

  static {
    stateOf = (limit) => limit.#state;
  }

  /**
   * `spentSat` restores a limit the application persisted. With a `store`
   * the limit saves what was spent before each payment is made.
   */
  constructor(limitSat: bigint, spentSat = 0n, store?: SpendStore) {
    this.#state = {
      limitSat: saturate(limitSat),
      spentSat: saturate(spentSat),
      store,
      saving: Promise.resolve(),
    };
  }

  get limitSat(): bigint {
    return this.#state.limitSat;
  }

  setLimitSat(limitSat: bigint): void {
    this.#state.limitSat = saturate(limitSat);
  }

  get spentSat(): bigint {
    return this.#state.spentSat;
  }

  get remainingSat(): bigint {
    return saturate(this.#state.limitSat - this.#state.spentSat);
  }

  /**
   * Start a new period, for example a new month. Takes effect at once; the
   * promise resolves when the store, if any, was told.
   */
  resetSpent(): Promise<void> {
    this.#state.spentSat = 0n;
    return save(this.#state).catch(() => undefined);
  }
}

/**
 * Count `amountSat` as spent, and save that before any payment is made on
 * it: a reservation a crash would forget is refused.
 */
async function reserve(limit: SpendingLimit, amountSat: bigint): Promise<void> {
  const state = stateOf(limit);
  const remainingSat = limit.remainingSat;
  if (amountSat > remainingSat) throw WalletError.limitReached(amountSat, remainingSat);
  // Counted at once, so concurrent payments cannot overspend together.
  state.spentSat += amountSat;
  try {
    await save(state);
  } catch (error) {
    state.spentSat = saturate(state.spentSat - amountSat);
    throw WalletError.failed(`cannot save the spending record: ${describe(error)}`);
  }
}

/** Replace a reservation with what the payment really cost. */
async function settle(limit: SpendingLimit, reservedSat: bigint, actualSat: bigint): Promise<void> {
  const state = stateOf(limit);
  state.spentSat = saturate(saturate(state.spentSat - reservedSat) + actualSat);
  await save(state).catch(() => undefined);
}

/**
 * Any wallet, held to a `SpendingLimit`.
 *
 * The amount and the fee ceiling are reserved before the wallet is asked, so
 * concurrent routes cannot overspend together and a fee cannot push spending
 * past the limit. The reservation is then replaced by the real cost.
 */
export class LimitedWallet implements RetrievalWallet {
  constructor(
    private readonly wallet: RetrievalWallet,
    readonly limit: SpendingLimit,
  ) {}

  async pay(request: PaymentRequest): Promise<WalletPayment> {
    const reservedSat = saturate(request.amountSat + request.maxFeeSat);
    await reserve(this.limit, reservedSat);
    let payment: WalletPayment;
    try {
      payment = await this.wallet.pay(request);
    } catch (error) {
      await settle(this.limit, reservedSat, 0n);
      throw error;
    }
    const { amountSat, feeSat } = payment;
    // A malformed result keeps its reservation; the buyer rejects it.
    const costSat = typeof amountSat === 'bigint' && typeof feeSat === 'bigint'
      ? saturate(amountSat + feeSat)
      : reservedSat;
    await settle(this.limit, reservedSat, costSat);
    return payment;
  }

  async recover(paymentId: Uint8Array): Promise<WalletPayment | null> {
    return this.wallet.recover ? this.wallet.recover(paymentId) : null;
  }

  balanceSat(): Promise<bigint> {
    return this.wallet.balanceSat();
  }

  async reclaim(payment: WalletPayment): Promise<void> {
    if (!this.wallet.reclaim) throw WalletError.failed('this wallet cannot reclaim tokens');
    await this.wallet.reclaim(payment);
    await settle(this.limit, payment.amountSat, 0n);
  }
}
