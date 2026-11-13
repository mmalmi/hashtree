/**
 * Transport-independent paid-retrieval messages.
 *
 * Each message is one bencoded dictionary; `data` is followed by the raw blob
 * bytes. Byte-compatible with the Rust crate `hashtree-paid-retrieval`; the
 * shared vectors in `test-vectors/paid-retrieval-vectors.json` hold both to it.
 * See `docs/paid-retrieval.md`.
 */

import { BLOB_MAX_BYTES } from '@hashtree/core';
import {
  decodeBencodePrefix,
  encodeBencode,
  type BencodeDict,
  type BencodeValue,
} from './bencode.js';
import type { Tariff } from './price.js';

/** Capability name of the exchange on Hashtree transports. */
export const PROTOCOL = 'hashtree.paid-blob/1';
/** Name advertised in the BEP 10 extended handshake `m` dictionary. */
export const BEP10_EXTENSION = 'ht_pay';
/** Payment method: a Cashu token redeemed by the provider at its mint. */
export const METHOD_CASHU = 'cashu';
export const MAX_HEADER_BYTES = 64 * 1024;
export const MAX_MESSAGE_BYTES = BLOB_MAX_BYTES + MAX_HEADER_BYTES;
export const ACCOUNT_ID_BYTES = 32;
export const PAYMENT_ID_BYTES = 16;

const MAX_MINTS = 16;
const MAX_URL_BYTES = 512;
const MAX_TOKEN_BYTES = 48 * 1024;
const MAX_TEXT_BYTES = 256;
const INT_MAX = 2n ** 63n - 1n;
const DIGEST_BYTES = { sha256: 32, sha1: 20 } as const;

/**
 * Names the paid unit by the hash that proves its delivery. On the wire the
 * digest is `h` and any algorithm other than SHA-256 is named in `y`.
 */
export interface ContentHash {
  algorithm: 'sha256' | 'sha1';
  digest: Uint8Array;
}

export const ERROR_CODES = ['bad', 'mint', 'method', 'invalid', 'replay', 'retry', 'full'] as const;
/** `unknown` stands for any code this version does not know. */
export type ErrorCode = (typeof ERROR_CODES)[number] | 'unknown';

export type Request =
  /** Fetch a blob, authorising a debit of at most `maxMsat`. */
  | { type: 'get'; hash: ContentHash; account: Uint8Array; maxMsat: bigint }
  /** Credit the account with one payment. */
  | {
    type: 'pay';
    account: Uint8Array;
    payment: Uint8Array;
    method: string;
    mint: string;
    token: string;
  };

/** The provider's terms for one blob: payment is required before delivery. */
export interface Quote {
  hash: ContentHash;
  size: bigint;
  priceMsat: bigint;
  balanceMsat: bigint;
  graceMsat: bigint;
  tariff: Tariff;
  mints: string[];
  methods: string[];
  minTopupSat: bigint;
}

export type Reply =
  | {
    type: 'data';
    hash: ContentHash;
    chargedMsat: bigint;
    balanceMsat: bigint;
    /** A free repeat of a blob this account was already charged for. */
    duplicate: boolean;
    data: Uint8Array;
  }
  | { type: 'quote'; quote: Quote }
  /** This provider has no data for the hash. Not proof of absence. */
  | { type: 'none'; hash: ContentHash }
  | { type: 'paid'; payment: Uint8Array; creditedMsat: bigint; balanceMsat: bigint }
  | { type: 'err'; code: ErrorCode; payment?: Uint8Array; detail: string };

export class MessageError extends Error {
  constructor(reason: string) {
    super(`invalid paid-retrieval message: ${reason}`);
    this.name = 'MessageError';
  }
}

export function bytesEqual(left: Uint8Array, right: Uint8Array): boolean {
  if (left.length !== right.length) return false;
  for (let index = 0; index < left.length; index += 1) {
    if (left[index] !== right[index]) return false;
  }
  return true;
}

export function contentHashEquals(left: ContentHash, right: ContentHash): boolean {
  return left.algorithm === right.algorithm && bytesEqual(left.digest, right.digest);
}

const textEncoder = new TextEncoder();
const textDecoder = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true });

/** Unsigned numbers above the signed 64-bit range are written as its top. */
function wireUint(key: string, value: bigint): bigint {
  if (value < 0n) throw new RangeError(`paid-retrieval field ${key} must not be negative`);
  return value > INT_MAX ? INT_MAX : value;
}

class Fields {
  private readonly entries: BencodeDict = new Map();

  constructor(kind: string) {
    this.text('t', kind);
  }

  uint(key: string, value: bigint): this {
    this.entries.set(key, wireUint(key, value));
    return this;
  }

  int(key: string, value: bigint): this {
    this.entries.set(key, value);
    return this;
  }

  bytes(key: string, value: Uint8Array, length?: number): this {
    if (length !== undefined && value.length !== length) {
      throw new RangeError(`paid-retrieval field ${key} must be ${length} bytes`);
    }
    this.entries.set(key, value);
    return this;
  }

  text(key: string, value: string): this {
    return this.bytes(key, textEncoder.encode(value));
  }

  content(hash: ContentHash): this {
    const length = DIGEST_BYTES[hash.algorithm];
    if (length === undefined) throw new RangeError('unsupported hash algorithm');
    this.bytes('h', hash.digest, length);
    return hash.algorithm === 'sha256' ? this : this.text('y', hash.algorithm);
  }

  list(key: string, values: BencodeValue[]): this {
    this.entries.set(key, values);
    return this;
  }

  texts(key: string, values: readonly string[]): this {
    return this.list(key, values.map((value) => textEncoder.encode(value)));
  }

  finish(payload?: Uint8Array): Uint8Array {
    const header = encodeBencode(this.entries);
    if (!payload) return header;
    const out = new Uint8Array(header.length + payload.length);
    out.set(header);
    out.set(payload, header.length);
    return out;
  }
}

function textValue(value: BencodeValue | undefined, max: number): string {
  if (value instanceof Uint8Array && value.length <= max) {
    try {
      return textDecoder.decode(value);
    } catch {
      // Not UTF-8: reported below.
    }
  }
  throw new MessageError('a text field is invalid');
}

class Dict {
  private constructor(private readonly entries: BencodeDict) {}

  static parse(bytes: Uint8Array): { dict: Dict; used: number } {
    const header = bytes.subarray(0, Math.min(bytes.length, MAX_HEADER_BYTES));
    let decoded;
    try {
      decoded = decodeBencodePrefix(header);
    } catch {
      throw new MessageError('header is not canonical bencode');
    }
    if (!(decoded.value instanceof Map)) throw new MessageError('message is not a dictionary');
    return { dict: new Dict(decoded.value), used: decoded.used };
  }

  has(key: string): boolean {
    return this.entries.has(key);
  }

  get(key: string): BencodeValue {
    const value = this.entries.get(key);
    if (value === undefined) throw new MessageError('a required field is missing');
    return value;
  }

  int(key: string): bigint {
    const value = this.get(key);
    if (typeof value !== 'bigint') throw new MessageError('a number field has the wrong type');
    return value;
  }

  uint(key: string): bigint {
    const value = this.int(key);
    if (value < 0n) throw new MessageError('a number field is negative');
    return value;
  }

  fixed(key: string, length: number): Uint8Array {
    const value = this.get(key);
    if (!(value instanceof Uint8Array) || value.length !== length) {
      throw new MessageError('an id or hash field has the wrong length');
    }
    return value;
  }

  text(key: string, max: number): string {
    return textValue(this.get(key), max);
  }

  /** SHA-256 has one encoding: `y` is absent, never spelled out. */
  content(): ContentHash {
    if (!this.has('y')) return { algorithm: 'sha256', digest: this.fixed('h', DIGEST_BYTES.sha256) };
    if (this.text('y', MAX_TEXT_BYTES) !== 'sha1') {
      throw new MessageError('unsupported hash algorithm');
    }
    return { algorithm: 'sha1', digest: this.fixed('h', DIGEST_BYTES.sha1) };
  }

  texts(key: string): string[] {
    const items = this.get(key);
    if (!Array.isArray(items) || items.length > MAX_MINTS) {
      throw new MessageError('a list field is invalid');
    }
    return items.map((item) => textValue(item, MAX_URL_BYTES));
  }

  tariff(): Tariff {
    const rate = this.get('r');
    if (
      !Array.isArray(rate) || rate.length !== 2
      || typeof rate[0] !== 'bigint' || typeof rate[1] !== 'bigint'
      || rate[0] < 0n || rate[1] < 0n
    ) {
      throw new MessageError('the tariff is invalid');
    }
    return { rate: { msat: rate[0], perBytes: rate[1] }, minMsat: this.uint('f') };
  }
}

export function encodeRequest(request: Request): Uint8Array {
  if (request.type === 'get') {
    return new Fields('get')
      .content(request.hash)
      .bytes('a', request.account, ACCOUNT_ID_BYTES)
      .uint('x', request.maxMsat)
      .finish();
  }
  return new Fields('pay')
    .bytes('a', request.account, ACCOUNT_ID_BYTES)
    .bytes('i', request.payment, PAYMENT_ID_BYTES)
    .text('k', request.method)
    .text('m', request.mint)
    .text('v', request.token)
    .finish();
}

export function decodeRequest(bytes: Uint8Array): Request {
  const { dict, used } = Dict.parse(bytes);
  if (used !== bytes.length) throw new MessageError('request has trailing bytes');
  switch (dict.text('t', MAX_TEXT_BYTES)) {
    case 'get':
      return {
        type: 'get',
        hash: dict.content(),
        account: dict.fixed('a', ACCOUNT_ID_BYTES),
        maxMsat: dict.uint('x'),
      };
    case 'pay':
      return {
        type: 'pay',
        account: dict.fixed('a', ACCOUNT_ID_BYTES),
        payment: dict.fixed('i', PAYMENT_ID_BYTES),
        method: dict.text('k', MAX_TEXT_BYTES),
        mint: dict.text('m', MAX_URL_BYTES),
        token: dict.text('v', MAX_TOKEN_BYTES),
      };
    default:
      throw new MessageError('unknown request type');
  }
}

function encodeQuote(quote: Quote): Uint8Array {
  const { rate, minMsat } = quote.tariff;
  return new Fields('quote')
    .content(quote.hash)
    .uint('n', quote.size)
    .uint('c', quote.priceMsat)
    .int('b', quote.balanceMsat)
    .uint('g', quote.graceMsat)
    .list('r', [wireUint('r', rate.msat), wireUint('r', rate.perBytes)])
    .uint('f', minMsat)
    .texts('m', quote.mints)
    .texts('k', quote.methods)
    .uint('u', quote.minTopupSat)
    .finish();
}

export function encodeReply(reply: Reply): Uint8Array {
  switch (reply.type) {
    case 'data': {
      const fields = new Fields('data')
        .content(reply.hash)
        .uint('n', BigInt(reply.data.length))
        .uint('c', reply.chargedMsat)
        .int('b', reply.balanceMsat);
      if (reply.duplicate) fields.uint('d', 1n);
      return fields.finish(reply.data);
    }
    case 'quote':
      return encodeQuote(reply.quote);
    case 'none':
      return new Fields('none').content(reply.hash).finish();
    case 'paid':
      return new Fields('paid')
        .bytes('i', reply.payment, PAYMENT_ID_BYTES)
        .uint('v', reply.creditedMsat)
        .int('b', reply.balanceMsat)
        .finish();
    case 'err': {
      const detail = Array.from(reply.detail).slice(0, MAX_TEXT_BYTES / 4).join('');
      const fields = new Fields('err').text('r', reply.code).text('d', detail);
      if (reply.payment) fields.bytes('i', reply.payment, PAYMENT_ID_BYTES);
      return fields.finish();
    }
  }
}

function decodeData(dict: Dict, payload: Uint8Array): Reply {
  const size = dict.uint('n');
  if (size > BigInt(BLOB_MAX_BYTES) || size !== BigInt(payload.length)) {
    throw new MessageError('data length does not match its header');
  }
  return {
    type: 'data',
    hash: dict.content(),
    chargedMsat: dict.uint('c'),
    balanceMsat: dict.int('b'),
    duplicate: dict.has('d') && dict.uint('d') !== 0n,
    data: payload.slice(),
  };
}

function decodeErrorCode(text: string): ErrorCode {
  return (ERROR_CODES as readonly string[]).includes(text) ? (text as ErrorCode) : 'unknown';
}

export function decodeReply(bytes: Uint8Array): Reply {
  const { dict, used } = Dict.parse(bytes);
  const kind = dict.text('t', MAX_TEXT_BYTES);
  if (kind !== 'data' && used !== bytes.length) {
    throw new MessageError('reply has trailing bytes');
  }
  switch (kind) {
    case 'data':
      return decodeData(dict, bytes.subarray(used));
    case 'quote': {
      const tariff = dict.tariff();
      return {
        type: 'quote',
        quote: {
          hash: dict.content(),
          size: dict.uint('n'),
          priceMsat: dict.uint('c'),
          balanceMsat: dict.int('b'),
          graceMsat: dict.uint('g'),
          tariff,
          mints: dict.texts('m'),
          methods: dict.texts('k'),
          minTopupSat: dict.uint('u'),
        },
      };
    }
    case 'none':
      return { type: 'none', hash: dict.content() };
    case 'paid':
      return {
        type: 'paid',
        payment: dict.fixed('i', PAYMENT_ID_BYTES),
        creditedMsat: dict.uint('v'),
        balanceMsat: dict.int('b'),
      };
    case 'err': {
      const code = decodeErrorCode(dict.text('r', MAX_TEXT_BYTES));
      const payment = dict.has('i') ? dict.fixed('i', PAYMENT_ID_BYTES) : undefined;
      const detail = dict.text('d', MAX_TEXT_BYTES);
      return payment ? { type: 'err', code, payment, detail } : { type: 'err', code, detail };
    }
    default:
      throw new MessageError('unknown reply type');
  }
}
