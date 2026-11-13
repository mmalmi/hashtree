/** Hashtree glue for the buying side: one opaque `BlobRoute` per provider. */

import {
  BLOB_MAX_BYTES,
  BLOB_NO_RESULT,
  blobData,
  sha256,
  type BlobReply,
  type BlobRequest,
  type BlobRoute,
  type BlobRouteContext,
  type Hash,
} from '@hashtree/core';
import {
  defaultClientPolicy,
  type AccountState,
  type ClientPolicy,
  type PaidRouteStats,
} from './account.js';
import { PaidBuyer, type InFlight } from './buyer.js';
import { bytesEqual, contentHashEquals, type ContentHash, type Quote, type Reply } from './message.js';
import { priceMsat } from './price.js';
import type { PaidBlobTransport } from './transport.js';
import type { RetrievalWallet } from './wallet.js';

/** A get that keeps asking for money has not converged; give up on the read. */
const MAX_ROUNDS = 4;

export interface PaidBlobRouteOptions {
  /** The application's name for the seller, passed to the wallet with every payment. */
  provider: string;
  transport: PaidBlobTransport;
  wallet: RetrievalWallet;
  /** Missing fields take the placeholder defaults of `defaultClientPolicy`. */
  policy?: Partial<ClientPolicy>;
  /** Continue with an account the application persisted. */
  account?: AccountState;
  /** Route identity in a `BlobRouter`; `paid:<provider>` unless given. */
  id?: string;
  groupId?: string;
}

type Accepted = { reply: BlobReply } | { quote: Quote };

function protocol(what: string): Error {
  return new Error(`paid route provider ${what}`);
}

/**
 * Fetches blobs from one paying-only provider.
 *
 * A read the client declines to pay for is `no-result`; `stats()` says why. A
 * read that fails or times out throws, so it is never mistaken for a miss.
 *
 * `BlobRouter` hedges across its routes without regard to cost: give a paid
 * route only the reads the free routes returned nothing for.
 */
export class PaidBlobRoute implements BlobRoute {
  readonly id: string;
  readonly groupId?: string;
  /** The account and payment handling behind this route. */
  readonly buyer: PaidBuyer;
  private deliveries: Promise<void> = Promise.resolve();

  constructor(options: PaidBlobRouteOptions) {
    this.id = options.id ?? `paid:${options.provider}`;
    if (!this.id) throw new Error('Blob route identity must not be empty');
    if (options.groupId !== undefined) this.groupId = options.groupId;
    this.buyer = new PaidBuyer({
      provider: options.provider,
      transport: options.transport,
      wallet: options.wallet,
      policy: defaultClientPolicy(options.policy),
      account: options.account,
    });
  }

  /** The account as it should be persisted. */
  accountState(): AccountState {
    return this.buyer.accountState();
  }

  stats(): PaidRouteStats {
    return this.buyer.stats();
  }

  /** Resend an unacknowledged payment and pay any debt now. */
  settle(): Promise<void> {
    return this.buyer.settle();
  }

  /** Pay this provider again after a block, writing off the recorded loss. */
  clearBlock(): void {
    this.buyer.clearBlock();
  }

  async read(request: BlobRequest, context?: BlobRouteContext): Promise<BlobReply> {
    const { buyer } = this;
    const signal = context?.signal;
    if (signal?.aborted) throw new Error('paid route read was cancelled');
    const deadlineMs = context?.deadlineMs ?? Date.now() + buyer.policy.requestTimeoutMs;
    const maxMsat = priceMsat(buyer.policy.maxTariff, BigInt(BLOB_MAX_BYTES));
    const content: ContentHash = { algorithm: 'sha256', digest: request.hash };
    for (let round = 0; round < MAX_ROUNDS; round += 1) {
      if (!(await buyer.prepare(deadlineMs, signal))) return BLOB_NO_RESULT;
      const inFlight = buyer.begin(maxMsat);
      let accepted: Accepted;
      try {
        const reply = await buyer.exchange(
          { type: 'get', hash: content, account: buyer.accountId, maxMsat },
          deadlineMs,
          signal,
        );
        accepted = await this.accept(inFlight, request.hash, content, reply);
      } finally {
        inFlight.end();
      }
      if ('reply' in accepted) return accepted.reply;
      if (!(await buyer.fund(accepted.quote, deadlineMs, signal))) return BLOB_NO_RESULT;
    }
    throw protocol('kept asking for payment');
  }

  /**
   * Check one reply to a get while that get still counts as in flight.
   *
   * Hashing is asynchronous here, so deliveries could finish out of order.
   * Each is hashed at once but accounted in the order it arrived: a reported
   * balance is not overwritten by an older one.
   */
  private accept(
    request: InFlight,
    hash: Hash,
    content: ContentHash,
    reply: Reply,
  ): Promise<Accepted> {
    if (reply.type !== 'data') return this.account(request, hash, content, reply);
    const digest = sha256(reply.data);
    digest.catch(() => undefined);
    const accepted = this.deliveries.then(() => this.account(request, hash, content, reply, digest));
    this.deliveries = accepted.then(() => undefined, () => undefined);
    return accepted;
  }

  private async account(
    request: InFlight,
    hash: Hash,
    content: ContentHash,
    reply: Reply,
    digest?: Promise<Hash>,
  ): Promise<Accepted> {
    const { buyer } = this;
    switch (reply.type) {
      case 'none':
        if (!contentHashEquals(reply.hash, content)) break;
        return { reply: BLOB_NO_RESULT };
      case 'data': {
        if (!contentHashEquals(reply.hash, content)) break;
        const { data } = reply;
        if (data.length > BLOB_MAX_BYTES || !digest || !bytesEqual(await digest, hash)) {
          buyer.undelivered(request, reply.balanceMsat);
          throw protocol('returned content with the wrong hash');
        }
        buyer.delivered(
          request,
          content,
          data.length,
          reply.chargedMsat,
          reply.balanceMsat,
          reply.duplicate,
        );
        return { reply: blobData(data) };
      }
      case 'quote': {
        const { quote } = reply;
        if (!contentHashEquals(quote.hash, content)) break;
        const affordable = buyer.quoted(request, quote);
        if (quote.size > BigInt(BLOB_MAX_BYTES)) throw protocol('quoted an oversized blob');
        return affordable ? { quote } : { reply: BLOB_NO_RESULT };
      }
      case 'err':
        throw protocol(`refused the request (${reply.code}): ${reply.detail}`);
      default:
        break;
    }
    throw protocol('sent an unexpected reply');
  }
}
