/**
 * A fake mint, wallet and selling provider. No network, no real funds.
 *
 * The provider is a test double of the Rust `PaidBlobProvider`: the package
 * itself has no selling side.
 */

import { sha256, toHex, type Hash } from '@hashtree/core';
import {
  METHOD_CASHU,
  WalletError,
  decodeReply,
  decodeRequest,
  encodeReply,
  priceMsat,
  type PaidBlobTransport,
  type PaymentRequest,
  type Reply,
  type Request,
  type RetrievalWallet,
  type Tariff,
  type WalletPayment,
} from '../../src/index.js';

export const MINT = 'https://mint.test';

/** 1 msat per KiB, at least 1000 msat per blob. */
export const TARIFF: Tariff = { rate: { msat: 1n, perBytes: 1024n }, minMsat: 1_000n };

/** Issues bearer tokens and lets each be redeemed exactly once. */
export class FakeMint {
  private readonly tokens = new Map<string, { mint: string; amountSat: bigint; spent: boolean }>();

  issue(mint: string, amountSat: bigint): string {
    const token = `cashuFake${this.tokens.size}`;
    this.tokens.set(token, { mint, amountSat, spent: false });
    return token;
  }

  redeem(mint: string, token: string): bigint {
    const issued = this.tokens.get(token);
    if (!issued) throw new Error('unknown token');
    if (issued.mint !== mint) throw new Error('token belongs to another mint');
    if (issued.spent) throw new Error('token already spent');
    issued.spent = true;
    return issued.amountSat;
  }
}

/** A funded client wallet on one mint. */
export class FakeWallet implements RetrievalWallet {
  mintUrl = MINT;
  payments = 0;
  /** Pay this many sats less than asked, to exercise underpayment. */
  shortSat = 0n;
  /** When set, a payment waits here until the test lets it through. */
  gate: Promise<void> | null = null;

  constructor(readonly mint: FakeMint, public balance: bigint) {}

  async pay(request: PaymentRequest): Promise<WalletPayment> {
    if (this.gate) await this.gate;
    if (!request.mints.includes(this.mintUrl)) throw WalletError.noAcceptedMint();
    const reduced = request.amountSat - this.shortSat;
    const amountSat = reduced > 1n ? reduced : 1n;
    if (this.balance < amountSat) throw WalletError.insufficientFunds();
    this.balance -= amountSat;
    this.payments += 1;
    const token = this.mint.issue(this.mintUrl, amountSat);
    return { mintUrl: this.mintUrl, token, amountSat, feeSat: 0n, reclaimId: token };
  }

  async balanceSat(): Promise<bigint> {
    return this.balance;
  }

  async reclaim(payment: WalletPayment): Promise<void> {
    this.balance += this.mint.redeem(payment.mintUrl, payment.token);
  }
}

interface SellerAccount {
  paidMsat: bigint;
  chargedMsat: bigint;
  payments: Map<string, bigint>;
  recent: Array<{ hash: string; priceMsat: bigint; at: number; left: number }>;
}

export interface SellerConfig {
  tariff: Tariff;
  mints: string[];
  minTopupSat: bigint;
  /** Msat served before payment to an authenticated peer. */
  graceMsat: bigint;
  redeliveryWindowMs: number;
}

const trimMint = (mint: string): string => mint.trim().replace(/\/+$/, '');

/** Sells blobs against prepaid balances, redeeming tokens at the fake mint. */
export class FakeSeller {
  readonly mint = new FakeMint();
  readonly config: SellerConfig;
  /** Sats redeemed at the mint. */
  received = 0n;
  /** While set, the mint cannot be reached. */
  offline = false;
  private readonly blobs = new Map<string, Uint8Array>();
  private readonly ledger = new Map<string, SellerAccount>();
  private readonly tokens = new Set<string>();
  private readonly gracePeers = new Map<string, string>();

  constructor(configure: Partial<SellerConfig> = {}) {
    this.config = {
      tariff: TARIFF,
      mints: [MINT],
      minTopupSat: 1n,
      graceMsat: 0n,
      redeliveryWindowMs: 60_000,
      ...configure,
    };
  }

  async add(seed: number, length: number): Promise<{ hash: Hash; data: Uint8Array }> {
    const data = Uint8Array.from({ length }, (_, index) => (seed + index) & 0xff);
    const hash = await sha256(data);
    this.blobs.set(toHex(hash), data);
    return { hash, data };
  }

  accounts(): Array<{ account: string; paidMsat: bigint; chargedMsat: bigint }> {
    return [...this.ledger].map(([account, { paidMsat, chargedMsat }]) => ({
      account,
      paidMsat,
      chargedMsat,
    }));
  }

  /** Answer one encoded request with one encoded reply. */
  async handle(peer: string | undefined, request: Uint8Array): Promise<Uint8Array> {
    let decoded: Request;
    try {
      decoded = decodeRequest(request);
    } catch (error) {
      return encodeReply({ type: 'err', code: 'bad', detail: String(error) });
    }
    return encodeReply(decoded.type === 'get' ? this.get(peer, decoded) : this.pay(decoded));
  }

  private account(id: string): SellerAccount {
    let entry = this.ledger.get(id);
    if (!entry) {
      entry = { paidMsat: 0n, chargedMsat: 0n, payments: new Map(), recent: [] };
      this.ledger.set(id, entry);
    }
    return entry;
  }

  private balance(id: string): bigint {
    const entry = this.ledger.get(id);
    return entry ? entry.paidMsat - entry.chargedMsat : 0n;
  }

  private get(peer: string | undefined, request: Extract<Request, { type: 'get' }>): Reply {
    const { hash } = request;
    const key = toHex(hash.digest);
    const data = hash.algorithm === 'sha256' ? this.blobs.get(key) : undefined;
    if (!data) return { type: 'none', hash };
    const id = toHex(request.account);
    const now = Date.now();
    const repeat = this.ledger.get(id)?.recent.find((recent) => recent.hash === key
      && recent.left > 0 && now - recent.at <= this.config.redeliveryWindowMs);
    if (repeat) {
      repeat.left -= 1;
      return {
        type: 'data',
        hash,
        chargedMsat: repeat.priceMsat,
        balanceMsat: this.balance(id),
        duplicate: true,
        data,
      };
    }
    const price = priceMsat(this.config.tariff, BigInt(data.length));
    const bound = peer === undefined ? undefined : this.gracePeers.get(peer);
    const grace = peer === undefined || (bound !== undefined && bound !== id)
      ? 0n
      : this.config.graceMsat;
    const balance = this.balance(id);
    if (price > request.maxMsat || balance + grace < price) {
      return {
        type: 'quote',
        quote: {
          hash,
          size: BigInt(data.length),
          priceMsat: price,
          balanceMsat: balance,
          graceMsat: grace,
          tariff: this.config.tariff,
          mints: [...this.config.mints],
          methods: [METHOD_CASHU],
          minTopupSat: this.config.minTopupSat,
        },
      };
    }
    const entry = this.account(id);
    entry.chargedMsat += price;
    entry.recent.push({ hash: key, priceMsat: price, at: now, left: 2 });
    if (this.balance(id) < 0n && peer !== undefined) this.gracePeers.set(peer, id);
    return {
      type: 'data',
      hash,
      chargedMsat: price,
      balanceMsat: this.balance(id),
      duplicate: false,
      data,
    };
  }

  private pay(request: Extract<Request, { type: 'pay' }>): Reply {
    const { payment } = request;
    const refuse = (code: 'method' | 'mint' | 'replay' | 'retry' | 'invalid', detail: string): Reply => (
      { type: 'err', code, payment, detail }
    );
    if (request.method !== METHOD_CASHU) return refuse('method', 'unsupported payment method');
    const mint = trimMint(request.mint);
    if (!this.config.mints.map(trimMint).includes(mint)) {
      return refuse('mint', 'mint is not accepted');
    }
    const id = toHex(request.account);
    const known = this.ledger.get(id)?.payments.get(toHex(payment));
    if (known !== undefined) {
      return { type: 'paid', payment, creditedMsat: known, balanceMsat: this.balance(id) };
    }
    if (this.tokens.has(request.token)) return refuse('replay', 'token was already credited');
    if (this.offline) return refuse('retry', 'mint unreachable');
    let sat: bigint;
    try {
      sat = this.mint.redeem(mint, request.token);
    } catch (error) {
      return refuse('invalid', error instanceof Error ? error.message : String(error));
    }
    this.received += sat;
    this.tokens.add(request.token);
    const entry = this.account(id);
    entry.paidMsat += sat * 1000n;
    entry.payments.set(toHex(payment), sat * 1000n);
    return { type: 'paid', payment, creditedMsat: sat * 1000n, balanceMsat: this.balance(id) };
  }
}

type Tamper = (request: Request, reply: Reply) => Reply | null;

/**
 * Hands requests straight to a provider. `tamper` may replace a reply or lose
 * it (`null`), after the provider has already acted on the request.
 */
export class DirectTransport implements PaidBlobTransport {
  exchanges = 0;
  /** Transport-authenticated identity of this client, if there is one. */
  peer: string | undefined;

  constructor(
    private readonly seller: FakeSeller,
    private readonly tamper: Tamper = (_request, reply) => reply,
  ) {}

  async exchange(request: Uint8Array): Promise<Uint8Array> {
    this.exchanges += 1;
    const reply = this.tamper(
      decodeRequest(request),
      decodeReply(await this.seller.handle(this.peer, request)),
    );
    if (reply === null) throw new Error('connection lost');
    return encodeReply(reply);
  }
}
