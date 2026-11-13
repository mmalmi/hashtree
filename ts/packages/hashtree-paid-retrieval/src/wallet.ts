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
    return WalletError.failed(error instanceof Error ? error.message : String(error));
  }

  /** The wallet chose not to pay; nothing is wrong with it or the provider. */
  get isDecline(): boolean {
    return this.kind !== 'failed';
  }
}

export interface RetrievalWallet {
  /** Create a token of exactly `amountSat` for the provider, or throw a `WalletError`. */
  pay(request: PaymentRequest): Promise<WalletPayment>;

  /** Spendable balance, for display and for refusing early. */
  balanceSat(): Promise<bigint>;

  /** Take back a token the provider did not redeem. */
  reclaim?(payment: WalletPayment): Promise<void>;
}

interface LimitState {
  limitSat: bigint;
  spentSat: bigint;
}

let stateOf: (limit: SpendingLimit) => LimitState;

/** A total spending cap shared by any number of routes. */
export class SpendingLimit {
  readonly #state: LimitState;

  static {
    stateOf = (limit) => limit.#state;
  }

  /** `spentSat` restores a limit the application persisted. */
  constructor(limitSat: bigint, spentSat = 0n) {
    this.#state = { limitSat: saturate(limitSat), spentSat: saturate(spentSat) };
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

  /** Start a new period, for example a new month. */
  resetSpent(): void {
    this.#state.spentSat = 0n;
  }
}

function reserve(limit: SpendingLimit, amountSat: bigint): void {
  const remainingSat = limit.remainingSat;
  if (amountSat > remainingSat) throw WalletError.limitReached(amountSat, remainingSat);
  stateOf(limit).spentSat += amountSat;
}

function release(limit: SpendingLimit, amountSat: bigint): void {
  const state = stateOf(limit);
  state.spentSat = saturate(state.spentSat - amountSat);
}

/**
 * Any wallet, held to a `SpendingLimit`.
 *
 * The amount is reserved before the wallet is asked, so concurrent routes
 * cannot overspend together. Token fees are counted after the payment.
 */
export class LimitedWallet implements RetrievalWallet {
  constructor(
    private readonly wallet: RetrievalWallet,
    readonly limit: SpendingLimit,
  ) {}

  async pay(request: PaymentRequest): Promise<WalletPayment> {
    reserve(this.limit, request.amountSat);
    let payment: WalletPayment;
    try {
      payment = await this.wallet.pay(request);
    } catch (error) {
      release(this.limit, request.amountSat);
      throw error;
    }
    // Replace the reservation with what the payment really cost.
    release(this.limit, request.amountSat);
    const state = stateOf(this.limit);
    state.spentSat = saturate(state.spentSat + saturate(payment.amountSat + payment.feeSat));
    return payment;
  }

  balanceSat(): Promise<bigint> {
    return this.wallet.balanceSat();
  }

  async reclaim(payment: WalletPayment): Promise<void> {
    if (!this.wallet.reclaim) throw WalletError.failed('this wallet cannot reclaim tokens');
    await this.wallet.reclaim(payment);
    release(this.limit, payment.amountSat);
  }
}
