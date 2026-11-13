/**
 * A buyer's account as bytes, for any store. They contain bearer secrets.
 *
 * The layout is the Rust client's `AccountState::encode`; the shared vectors
 * in `test-vectors/paid-retrieval-vectors.json` hold the two to it.
 */

import { cloneAccountState, type AccountState, type PendingPayment } from './account.js';
import {
  decodeBencodePrefix,
  encodeBencode,
  type BencodeDict,
  type BencodeValue,
} from './bencode.js';
import { ACCOUNT_ID_BYTES, PAYMENT_ID_BYTES } from './message.js';

const FORMAT = 1n;
const INT_MAX = 2n ** 63n - 1n;
const U32_MAX = 0xffff_ffffn;

const textEncoder = new TextEncoder();
const textDecoder = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true });

function invalid(what: string): Error {
  return new Error(`invalid paid-retrieval account record: ${what}`);
}

/** Numbers above the signed 64-bit range are written as its top. */
const number = (value: bigint): bigint => (value > INT_MAX ? INT_MAX : value);
const text = (value: string): Uint8Array => textEncoder.encode(value);

function dict(entries: Record<string, BencodeValue | undefined>): BencodeDict {
  const fields: BencodeDict = new Map();
  for (const [key, value] of Object.entries(entries)) {
    if (value !== undefined) fields.set(key, value);
  }
  return fields;
}

function paymentValue({ id, payment, attempts }: PendingPayment): BencodeDict {
  return dict({
    a: number(payment.amountSat),
    f: number(payment.feeSat),
    i: id,
    m: text(payment.mintUrl),
    n: BigInt(attempts),
    t: text(payment.token),
    r: payment.reclaimId === undefined ? undefined : text(payment.reclaimId),
  });
}

export function encodeAccountState(account: AccountState): Uint8Array {
  const { intent, pending, blocked } = cloneAccountState(account);
  return encodeBencode(dict({
    a: account.account,
    b: account.balanceMsat,
    c: number(account.creditedMsat),
    g: number(account.chargedMsat),
    u: account.unresolved.map(paymentValue),
    v: FORMAT,
    k: blocked === null ? undefined : text(blocked),
    p: pending === null ? undefined : paymentValue(pending),
    i: intent === null ? undefined : dict({
      a: number(intent.amountSat),
      i: intent.id,
      m: intent.mints.map(text),
    }),
  }));
}

function textOf(value: BencodeValue): string {
  if (value instanceof Uint8Array) {
    try {
      return textDecoder.decode(value);
    } catch {
      // Not UTF-8: reported below.
    }
  }
  throw invalid('account text is invalid');
}

class Fields {
  private readonly entries: BencodeDict;

  constructor(value: BencodeValue) {
    if (!(value instanceof Map)) throw invalid('account record is not a dictionary');
    this.entries = value;
  }

  get(key: string): BencodeValue | undefined {
    return this.entries.get(key);
  }

  need(key: string): BencodeValue {
    const value = this.entries.get(key);
    if (value === undefined) throw invalid('account field is missing');
    return value;
  }

  number(key: string): bigint {
    const value = this.need(key);
    if (typeof value !== 'bigint' || value < 0n) throw invalid('account number is invalid');
    return value;
  }

  text(key: string): string {
    return textOf(this.need(key));
  }

  list(key: string, what: string): BencodeValue[] {
    const value = this.need(key);
    if (!Array.isArray(value)) throw invalid(what);
    return value;
  }

  fixed(key: string, length: number): Uint8Array {
    const value = this.need(key);
    if (!(value instanceof Uint8Array) || value.length !== length) {
      throw invalid('account id has the wrong length');
    }
    return value;
  }
}

function paymentOf(value: BencodeValue): PendingPayment {
  const fields = new Fields(value);
  const attempts = fields.number('n');
  const reclaimId = fields.get('r');
  return {
    id: fields.fixed('i', PAYMENT_ID_BYTES),
    attempts: Number(attempts > U32_MAX ? U32_MAX : attempts),
    payment: {
      mintUrl: fields.text('m'),
      token: fields.text('t'),
      amountSat: fields.number('a'),
      feeSat: fields.number('f'),
      ...(reclaimId === undefined ? {} : { reclaimId: textOf(reclaimId) }),
    },
  };
}

export function decodeAccountState(bytes: Uint8Array): AccountState {
  let decoded;
  try {
    decoded = decodeBencodePrefix(bytes);
  } catch {
    throw invalid('account record is damaged');
  }
  const fields = new Fields(decoded.value);
  if (decoded.used !== bytes.length || fields.number('v') !== FORMAT) {
    throw invalid('account record has an unknown format');
  }
  const intentValue = fields.get('i');
  const intent = intentValue === undefined ? undefined : new Fields(intentValue);
  const pending = fields.get('p');
  const blocked = fields.get('k');
  const balance = fields.need('b');
  if (typeof balance !== 'bigint') throw invalid('account balance is invalid');
  return {
    account: fields.fixed('a', ACCOUNT_ID_BYTES),
    creditedMsat: fields.number('c'),
    chargedMsat: fields.number('g'),
    balanceMsat: balance,
    intent: intent
      ? {
        id: intent.fixed('i', PAYMENT_ID_BYTES),
        amountSat: intent.number('a'),
        mints: intent.list('m', 'account mints are invalid').map(textOf),
      }
      : null,
    pending: pending === undefined ? null : paymentOf(pending),
    unresolved: fields.list('u', 'account payments are invalid').map(paymentOf),
    blocked: blocked === undefined ? null : textOf(blocked),
  };
}
