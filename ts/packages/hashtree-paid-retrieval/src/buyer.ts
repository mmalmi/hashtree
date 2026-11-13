/**
 * The buying side of the exchange: one account with one provider.
 *
 * Nothing here fetches or verifies data. The caller asks for a unit, checks
 * what arrived against its hash, and tells the buyer; the buyer keeps the
 * ledger, decides about quotes and pays.
 */

import { toHex } from '@hashtree/core';
import {
  assertClientPolicy,
  cloneAccountState,
  lossMsat,
  newAccountState,
  type AccountState,
  type AccountStore,
  type ClientPolicy,
  type PaidRouteStats,
  type PendingPayment,
} from './account.js';
import { BuyerCore, type InFlight } from './buyerCore.js';
import { withDeadline } from './deadline.js';
import { runFunding, settleCarried } from './funding.js';
import { bytesEqual, type ContentHash, type Quote, type Reply, type Request } from './message.js';
import { priceMsat, saturate } from './price.js';
import type { PaidBlobTransport } from './transport.js';
import type { RetrievalWallet } from './wallet.js';

/** Charges remembered so that a free repeat is not counted a second time. */
const MAX_COUNTED = 128;

export interface PaidBuyerOptions {
  /** The application's name for the seller, passed to the wallet with every payment. */
  provider: string;
  transport: PaidBlobTransport;
  wallet: RetrievalWallet;
  policy: ClientPolicy;
  /** Continue with the account the store held. */
  account?: AccountState;
  /**
   * Where the account is saved before the wallet is asked for a payment and
   * whenever a payment changes state. Without one nothing is saved: give one
   * wherever real money is paid.
   */
  store?: AccountStore;
}

function contentKey(content: ContentHash): string {
  return `${content.algorithm}:${toHex(content.digest)}`;
}

export type { InFlight };

/** The client's account with one provider: ledger, quote decisions, payment. */
export class PaidBuyer {
  private readonly core: BuyerCore;

  constructor(options: PaidBuyerOptions) {
    assertClientPolicy(options.policy);
    this.core = new BuyerCore(
      options.provider,
      options.transport,
      options.wallet,
      options.policy,
      options.account ? cloneAccountState(options.account) : newAccountState(),
      options.store,
    );
  }

  get policy(): ClientPolicy {
    return this.core.policy;
  }

  get accountId(): Uint8Array {
    return this.core.account.account.slice();
  }

  /** The account as it should be persisted. */
  accountState(): AccountState {
    return cloneAccountState(this.core.account);
  }

  stats(): PaidRouteStats {
    return { ...this.core.stats };
  }

  /** Pay this provider again after a block, writing off the recorded loss. */
  clearBlock(): void {
    const { account } = this.core;
    account.chargedMsat = saturate(account.chargedMsat + lossMsat(account));
    account.blocked = null;
  }

  /**
   * Get ready to request a unit: an unacknowledged token or a debt comes
   * first. Resolves to `false` when this provider is blocked.
   */
  async prepare(deadlineMs: number, signal?: AbortSignal): Promise<boolean> {
    const { account, stats } = this.core;
    if (account.blocked !== null) {
      stats.declinedBlocked += 1;
      return false;
    }
    if (account.intent !== null || account.pending !== null || account.balanceMsat < 0n) {
      await runFunding(this.core, null, false, deadlineMs, signal);
    }
    return true;
  }

  /**
   * Like `prepare`, for a request that will carry a fresh pending token
   * itself: such a token is returned instead of being sent on its own.
   * Resolves to `null` when this provider is blocked.
   */
  async prepareCarrying(
    deadlineMs: number,
    signal?: AbortSignal,
  ): Promise<{ carried: PendingPayment | null } | null> {
    const { core } = this;
    const { account, stats } = core;
    if (account.blocked !== null) {
      stats.declinedBlocked += 1;
      return null;
    }
    const fresh = account.pending !== null && account.pending.attempts === 0
      && account.intent === null
      ? account.pending
      : null;
    const unsettled = account.intent !== null
      || (account.pending !== null && fresh === null)
      || (account.balanceMsat < 0n && fresh === null);
    if (!unsettled) return { carried: fresh };
    await runFunding(core, null, false, deadlineMs, signal);
    const pending = core.account.pending;
    return { carried: pending !== null && pending.attempts === 0 ? pending : null };
  }

  /**
   * Take the payment lock for a request that carries `pending`, so that one
   * request at a time carries a token. Resolves to the token, if it is still
   * unsettled once the lock is held, and the function that releases the
   * lock. Waiting ends at the deadline, like the read.
   */
  async carry(
    pending: PendingPayment | null,
    deadlineMs: number,
    signal?: AbortSignal,
  ): Promise<{ carried: PendingPayment | null; release: () => void }> {
    const none = { carried: null, release: () => undefined };
    if (pending === null) return none;
    const { core } = this;
    const acquiring = core.acquirePaying();
    const release = await withDeadline(
      acquiring,
      deadlineMs,
      signal,
      'paid route payment is still in progress; availability is unknown',
      () => {
        void acquiring.then((unused) => unused());
      },
    );
    const current = core.account.pending;
    if (current !== null && bytesEqual(current.id, pending.id)) {
      return { carried: current, release };
    }
    release();
    return none;
  }

  /** Fund a quote, leaving a new token for the next request to carry. */
  fundCarrying(quote: Quote, deadlineMs: number, signal?: AbortSignal): Promise<boolean> {
    return runFunding(this.core, quote, true, deadlineMs, signal);
  }

  /**
   * Settle a token that `request` carried, from the reply to it. After a
   * credit, the reply's balance counts as current for `request`. Call it
   * while holding the lock `carry` took.
   */
  async settleCarried(request: InFlight, pending: PendingPayment, reply: Reply): Promise<void> {
    if (await settleCarried(this.core, pending, reply)) request.credits = this.core.credits;
  }

  /** The account id to name on the wire, if the carrier needs one. */
  wireAccount(): Uint8Array | null {
    return this.core.wireAccount();
  }

  /** Mark a request as outstanding until `end()` is called on the result. */
  begin(authorisedMsat: bigint): InFlight {
    const { core } = this;
    core.inFlight += 1;
    core.inFlightMsat = saturate(core.inFlightMsat + authorisedMsat);
    let open = true;
    return {
      authorisedMsat,
      credits: core.credits,
      end: () => {
        if (!open) return;
        open = false;
        core.inFlight -= 1;
        core.inFlightMsat = saturate(core.inFlightMsat - authorisedMsat);
      },
    };
  }

  /** Send one request over the transport and decode its reply. */
  exchange(request: Request, deadlineMs: number, signal?: AbortSignal): Promise<Reply> {
    return this.core.exchange(request, deadlineMs, signal);
  }

  /**
   * Take note of a quote. Returns `false` when its price is above
   * `maxTariff`, in which case it must not be funded.
   */
  quoted(request: InFlight, quote: Quote): boolean {
    const { core } = this;
    core.noteBalance(quote.balanceMsat, request);
    core.terms = {
      mints: [...quote.mints],
      methods: [...quote.methods],
      minTopupSat: quote.minTopupSat,
      lock: quote.lock === null ? null : quote.lock.slice(),
    };
    if (quote.priceMsat > priceMsat(core.policy.maxTariff, quote.size)) {
      core.stats.declinedPrice += 1;
      return false;
    }
    return true;
  }

  /** Record a unit of `size` bytes that arrived and matched its hash. */
  delivered(
    request: InFlight,
    content: ContentHash,
    size: number,
    chargedMsat: bigint,
    balanceMsat: bigint,
    duplicate: boolean,
  ): void {
    const { core } = this;
    const { account, stats } = core;
    const key = contentKey(content);
    // A free repeat reports the original charge. It is counted only if that
    // charge's first reply never arrived here.
    if (!(duplicate && core.counted.includes(key))) {
      account.chargedMsat = saturate(account.chargedMsat + chargedMsat);
      core.counted.push(key);
      if (core.counted.length > MAX_COUNTED) core.counted.shift();
    }
    stats.blobs += 1;
    stats.bytes += size;
    const fair = priceMsat(core.policy.maxTariff, BigInt(size));
    const limit = request.authorisedMsat < fair ? request.authorisedMsat : fair;
    if (chargedMsat > limit && account.blocked === null) {
      account.blocked = `provider charged ${chargedMsat} msat for a unit worth at most ${fair}`;
    }
    core.noteBalance(balanceMsat, request);
  }

  /**
   * Record a reply whose bytes did not match the hash. It is not a delivery:
   * its charge stays unexplained and counts towards the loss that blocks this
   * provider.
   */
  undelivered(request: InFlight, balanceMsat: bigint): void {
    this.core.stats.invalidBlobs += 1;
    this.core.noteBalance(balanceMsat, request);
  }

  /**
   * Make the account able to pay for `quote`; without one, resend an
   * unacknowledged payment and pay any debt. Resolves to `false` when the
   * client declines to pay (limit, mint, blocked provider).
   */
  fund(quote: Quote | null, deadlineMs: number, signal?: AbortSignal): Promise<boolean> {
    return runFunding(this.core, quote, false, deadlineMs, signal);
  }

  /**
   * Resend an unacknowledged payment, try the oldest unresolved token again
   * and pay any debt now.
   */
  async settle(): Promise<void> {
    await this.fund(null, Date.now() + this.core.policy.requestTimeoutMs);
  }
}
