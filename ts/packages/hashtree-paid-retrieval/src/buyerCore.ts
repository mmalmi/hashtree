/** The state one buyer keeps about one provider, shared by its steps. */

import { emptyStats, lossMsat, type AccountState, type ClientPolicy, type PaidRouteStats } from './account.js';
import { withDeadline } from './deadline.js';
import { decodeReply, encodeRequest, type Reply, type Request } from './message.js';
import type { PaidBlobTransport } from './transport.js';
import type { RetrievalWallet } from './wallet.js';

/** Terms from the provider's latest quote, needed to settle a debt later. */
export interface Terms {
  mints: string[];
  methods: string[];
  minTopupSat: bigint;
}

/** One request for a unit that the provider has not answered yet. */
export interface InFlight {
  /** The most the request lets the provider charge. */
  readonly authorisedMsat: bigint;
  /** Payments the provider had acknowledged when the request was sent. */
  readonly credits: number;
  /** The request is over, answered or not. */
  end(): void;
}

function describe(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

export class BuyerCore {
  terms: Terms | null = null;
  /** Units whose charge is already in `chargedMsat`, newest last. */
  counted: string[] = [];
  inFlight = 0;
  /** What the requests in flight authorised the provider to charge. */
  inFlightMsat = 0n;
  /** Payments the provider has acknowledged. */
  credits = 0;
  stats: PaidRouteStats = emptyStats();
  private paying: Promise<unknown> = Promise.resolve();

  constructor(
    readonly provider: string,
    readonly transport: PaidBlobTransport,
    readonly wallet: RetrievalWallet,
    readonly policy: ClientPolicy,
    public account: AccountState,
  ) {}

  /** Serialise payments so that concurrent requests share one top-up. */
  payingOnly<T>(step: () => Promise<T>): Promise<T> {
    const run = this.paying.then(step);
    this.paying = run.catch(() => undefined);
    return run;
  }

  async exchange(request: Request, deadlineMs: number, signal?: AbortSignal): Promise<Reply> {
    const abandoned = new AbortController();
    const reply = await withDeadline(
      (async () => this.transport.exchange(encodeRequest(request), abandoned.signal))(),
      deadlineMs,
      signal,
      'paid route timed out; availability is unknown',
      () => abandoned.abort(),
    );
    try {
      return decodeReply(reply);
    } catch (error) {
      throw new Error(`paid route provider sent an ${describe(error)}`);
    }
  }

  /**
   * Record the provider's reported balance and stop paying a provider that
   * took more than verified deliveries explain. `request` is the caller's own
   * request in flight, or `null` for the reply to a payment.
   *
   * While other requests are in flight the reported balance may already
   * include their charges, so the comparison waits. A reply to a request sent
   * before the latest acknowledged payment may predate that credit: its
   * balance is older than the one already recorded and is ignored.
   */
  noteBalance(balanceMsat: bigint, request: InFlight | null): void {
    if (request !== null && request.credits !== this.credits) return;
    this.account.balanceMsat = balanceMsat;
    const own = request === null ? 0 : 1;
    if (this.inFlight > own || this.account.blocked !== null) return;
    const loss = lossMsat(this.account);
    if (loss > this.policy.maxLossMsat) this.block(loss);
  }

  block(loss: bigint): void {
    this.account.blocked = `provider balance is ${loss} msat lower than verified deliveries explain`;
  }
}
