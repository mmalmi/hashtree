/** What a client knows about its account with one provider. */

import { ACCOUNT_ID_BYTES } from './message.js';
import { U64_MAX, saturate, type Tariff } from './price.js';
import type { WalletPayment } from './wallet.js';

/** The client's rules for paying one provider. */
export interface ClientPolicy {
  /** The highest price accepted; a dearer quote is declined. */
  maxTariff: Tariff;
  /**
   * Mints the client is willing to pay with. Empty leaves the choice among
   * the provider's mints to the wallet.
   */
  mints: string[];
  /** Preferred top-up, so that most reads need no mint round trip. */
  topupSat: bigint;
  /** The most this provider may hold in advance. */
  maxPrepaidMsat: bigint;
  /**
   * Payments stop once the provider's balance is this much lower than
   * hash-verified deliveries explain.
   */
  maxLossMsat: bigint;
  /** Deadline of a read without a router context, and of every payment. */
  requestTimeoutMs: number;
}

/** Placeholder caps: about 1 sat per MiB, 100 sat held by a provider. */
export function defaultClientPolicy(overrides: Partial<ClientPolicy> = {}): ClientPolicy {
  return {
    maxTariff: { rate: { msat: 1n, perBytes: 1024n }, minMsat: 100n },
    mints: [],
    topupSat: 20n,
    maxPrepaidMsat: 100_000n,
    maxLossMsat: 20_000n,
    requestTimeoutMs: 10_000,
    ...overrides,
  };
}

function isAmount(value: unknown): boolean {
  return typeof value === 'bigint' && value >= 0n && value <= U64_MAX;
}

/** Reject a policy that would fail later, in the middle of a payment. */
export function assertClientPolicy(policy: ClientPolicy): void {
  const amounts = [
    policy.maxTariff?.rate?.msat,
    policy.maxTariff?.rate?.perBytes,
    policy.maxTariff?.minMsat,
    policy.topupSat,
    policy.maxPrepaidMsat,
    policy.maxLossMsat,
  ];
  if (!amounts.every(isAmount)) {
    throw new TypeError('paid route policy amounts must be non-negative bigint values');
  }
  if (!Array.isArray(policy.mints) || !policy.mints.every((mint) => typeof mint === 'string')) {
    throw new TypeError('paid route policy mints must be a list of URLs');
  }
  if (!Number.isFinite(policy.requestTimeoutMs) || policy.requestTimeoutMs <= 0) {
    throw new RangeError('paid route policy requestTimeoutMs must be positive');
  }
}

/** A token created by the wallet that the provider has not acknowledged yet. */
export interface PendingPayment {
  id: Uint8Array;
  payment: WalletPayment;
}

/**
 * The client's side of one account. Persist it to keep a prepaid balance and
 * an unacknowledged payment across restarts. Every field survives a
 * structured clone, so it can be stored in IndexedDB as it is.
 */
export interface AccountState {
  /** Bearer id of the prepaid balance, 32 random bytes. */
  account: Uint8Array;
  /** Credits the provider acknowledged. */
  creditedMsat: bigint;
  /** Charges for blobs whose bytes matched their hash. */
  chargedMsat: bigint;
  /** The balance the provider last reported. */
  balanceMsat: bigint;
  pending: PendingPayment | null;
  /** Why this provider is no longer paid, if it is not. */
  blocked: string | null;
}

/** A fresh account with a random bearer id. */
export function newAccountState(): AccountState {
  return {
    account: crypto.getRandomValues(new Uint8Array(ACCOUNT_ID_BYTES)),
    creditedMsat: 0n,
    chargedMsat: 0n,
    balanceMsat: 0n,
    pending: null,
    blocked: null,
  };
}

export function cloneAccountState(state: AccountState): AccountState {
  if (
    !(state.account instanceof Uint8Array) || state.account.length !== ACCOUNT_ID_BYTES
    || !isAmount(state.creditedMsat) || !isAmount(state.chargedMsat)
    || typeof state.balanceMsat !== 'bigint'
  ) {
    throw new TypeError('paid route account state is invalid');
  }
  return {
    ...state,
    account: state.account.slice(),
    pending: state.pending && {
      id: state.pending.id.slice(),
      payment: { ...state.pending.payment },
    },
  };
}

/** Msat the provider holds less than verified deliveries explain. */
export function lossMsat(state: AccountState): bigint {
  return saturate(state.creditedMsat - state.chargedMsat - state.balanceMsat);
}

/** Counters for a wallet screen and for diagnosing declined reads. */
export interface PaidRouteStats {
  blobs: number;
  bytes: number;
  /** Sats handed to the provider, less reclaimed tokens. */
  paidSat: bigint;
  feeSat: bigint;
  /** Reads declined because the quote was above `maxTariff`. */
  declinedPrice: number;
  /** Reads declined by the wallet limit, its balance or `maxPrepaidMsat`. */
  declinedLimit: number;
  /** Reads declined because no mint or payment method was acceptable. */
  declinedMint: number;
  /** Reads not attempted because the provider is blocked. */
  declinedBlocked: number;
  /** Payments the provider refused. */
  rejectedPayments: number;
  /** Replies whose bytes did not match the requested hash. */
  invalidBlobs: number;
}

export function emptyStats(): PaidRouteStats {
  return {
    blobs: 0,
    bytes: 0,
    paidSat: 0n,
    feeSat: 0n,
    declinedPrice: 0,
    declinedLimit: 0,
    declinedMint: 0,
    declinedBlocked: 0,
    rejectedPayments: 0,
    invalidBlobs: 0,
  };
}
