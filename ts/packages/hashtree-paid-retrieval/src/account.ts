/** What a client knows about its account with one provider. */

import { ACCOUNT_ID_BYTES, PAYMENT_ID_BYTES } from './message.js';
import { U64_MAX, saturate, type Tariff } from './price.js';
import type { WalletPayment } from './wallet.js';

const U32_MAX = 0xffff_ffff;

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
  /** The most a wallet may spend on creating one token, beyond its value. */
  maxTokenFeeSat: bigint;
  /**
   * How much less than a token's value a provider may credit, to cover what
   * its mint charges for redeeming. A larger shortfall is a loss.
   */
  maxRedeemFeeMsat: bigint;
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
    maxTokenFeeSat: 2n,
    maxRedeemFeeMsat: 2_000n,
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
    policy.maxTokenFeeSat,
    policy.maxRedeemFeeMsat,
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
  /** Times the provider answered that it cannot take the payment yet. */
  attempts: number;
}

/**
 * A payment the wallet is about to be asked for. It is saved before the
 * wallet is called, so that a crash in between cannot orphan a token: on the
 * next start the wallet is asked for the payment with this id again.
 */
export interface PaymentIntent {
  id: Uint8Array;
  amountSat: bigint;
  mints: string[];
}

/**
 * The client's side of one account. Keep it in an `AccountStore` to carry a
 * prepaid balance and unsettled payments across restarts. It holds the
 * account's bearer id and unredeemed tokens: store it privately. Every field
 * survives a structured clone, and `encodeAccountState` gives the same bytes
 * as the Rust client.
 */
export interface AccountState {
  /** Bearer id of the prepaid balance, 32 random bytes. */
  account: Uint8Array;
  /**
   * What the provider owes for payments: the credits it acknowledged, plus
   * anything it kept of a token beyond `maxRedeemFeeMsat`.
   */
  creditedMsat: bigint;
  /** Charges for blobs whose bytes matched their hash. */
  chargedMsat: bigint;
  /** The balance the provider last reported. */
  balanceMsat: bigint;
  /** A payment the wallet was asked for and has not answered here yet. */
  intent: PaymentIntent | null;
  /** A token the provider has not acknowledged yet. */
  pending: PendingPayment | null;
  /**
   * Tokens neither credited by the provider nor taken back by the wallet.
   * They are never dropped: each may still be either. Their value counts
   * against `maxPrepaidMsat`.
   */
  unresolved: PendingPayment[];
  /** Why this provider is no longer paid, if it is not. */
  blocked: string | null;
}

/**
 * Where a buyer keeps its account across restarts.
 *
 * `save` is called before the wallet is asked for a payment, when a token
 * arrives, and whenever a payment is settled. It must be durable when it
 * resolves; a rejection before the wallet is asked stops that payment, and a
 * save that never settles holds up every later payment to this provider.
 * Charges for delivered blobs are not saved one by one; after a restart the
 * first balance the provider reports is taken as the baseline.
 */
export interface AccountStore {
  save(account: AccountState): Promise<void>;
}

/** A fresh account with a random bearer id. */
export function newAccountState(): AccountState {
  return {
    account: crypto.getRandomValues(new Uint8Array(ACCOUNT_ID_BYTES)),
    creditedMsat: 0n,
    chargedMsat: 0n,
    balanceMsat: 0n,
    intent: null,
    pending: null,
    unresolved: [],
    blocked: null,
  };
}

function isId(value: unknown, length: number): boolean {
  return value instanceof Uint8Array && value.length === length;
}

function clonePayment(pending: PendingPayment): PendingPayment {
  const { payment, attempts } = pending;
  if (
    !isId(pending.id, PAYMENT_ID_BYTES) || !isAmount(payment?.amountSat)
    || !isAmount(payment.feeSat) || typeof payment.mintUrl !== 'string'
    || typeof payment.token !== 'string'
    || !Number.isInteger(attempts) || attempts < 0 || attempts > U32_MAX
  ) {
    throw new TypeError('paid route account state holds an invalid payment');
  }
  return { id: pending.id.slice(), payment: { ...payment }, attempts };
}

/** An independent, checked copy: what a buyer starts from and what it hands out. */
export function cloneAccountState(state: AccountState): AccountState {
  const { intent } = state;
  if (
    !isId(state.account, ACCOUNT_ID_BYTES)
    || !isAmount(state.creditedMsat) || !isAmount(state.chargedMsat)
    || typeof state.balanceMsat !== 'bigint' || !Array.isArray(state.unresolved)
    || (intent !== null && (
      !isId(intent.id, PAYMENT_ID_BYTES) || !isAmount(intent.amountSat)
      || !Array.isArray(intent.mints)
    ))
  ) {
    throw new TypeError('paid route account state is invalid');
  }
  return {
    account: state.account.slice(),
    creditedMsat: state.creditedMsat,
    chargedMsat: state.chargedMsat,
    balanceMsat: state.balanceMsat,
    intent: intent && { id: intent.id.slice(), amountSat: intent.amountSat, mints: [...intent.mints] },
    pending: state.pending && clonePayment(state.pending),
    unresolved: state.unresolved.map(clonePayment),
    blocked: state.blocked,
  };
}

/** Msat the provider holds less than verified deliveries explain. */
export function lossMsat(state: AccountState): bigint {
  return saturate(state.creditedMsat - state.chargedMsat - state.balanceMsat);
}

/** Value of the tokens whose fate is not known. */
export function unresolvedMsat(state: AccountState): bigint {
  return state.unresolved.reduce(
    (total, { payment }) => saturate(total + saturate(payment.amountSat * 1000n)),
    0n,
  );
}

/** Whether anything was ever paid or charged on this account. */
export function hasHistory(state: AccountState): boolean {
  return state.creditedMsat !== 0n || state.chargedMsat !== 0n || state.balanceMsat !== 0n;
}

/** Counters for a wallet screen and for diagnosing declined reads. */
export interface PaidRouteStats {
  blobs: number;
  bytes: number;
  /** Sats handed to the provider, less reclaimed tokens. */
  paidSat: bigint;
  feeSat: bigint;
  /** Msat providers kept of tokens within `maxRedeemFeeMsat`. */
  redeemFeeMsat: bigint;
  /**
   * Charges made before a restart and not saved, written off when the
   * provider first reported its balance.
   */
  resyncedMsat: bigint;
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
    redeemFeeMsat: 0n,
    resyncedMsat: 0n,
    declinedPrice: 0,
    declinedLimit: 0,
    declinedMint: 0,
    declinedBlocked: 0,
    rejectedPayments: 0,
    invalidBlobs: 0,
  };
}
