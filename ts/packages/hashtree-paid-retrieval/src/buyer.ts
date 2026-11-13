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
  type ClientPolicy,
  type PaidRouteStats,
} from './account.js';
import { BuyerCore, type InFlight } from './buyerCore.js';
import { runFunding } from './funding.js';
import type { ContentHash, Quote, Reply, Request } from './message.js';
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
  /** Continue with an account the application persisted. */
  account?: AccountState;
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
    if (account.pending !== null || account.balanceMsat < 0n) {
      await runFunding(this.core, null, deadlineMs, signal);
    }
    return true;
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
    return runFunding(this.core, quote, deadlineMs, signal);
  }

  /** Resend an unacknowledged payment and pay any debt now. */
  async settle(): Promise<void> {
    await this.fund(null, Date.now() + this.core.policy.requestTimeoutMs);
  }
}
