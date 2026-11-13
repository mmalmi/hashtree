/**
 * Strict, bounded bencode for the paid-retrieval message headers.
 *
 * Only canonical input is accepted (sorted unique keys, minimal integers), so
 * every message has exactly one encoding on every carrier. Integers are the
 * signed 64-bit range of the Rust implementation, hence `bigint`.
 */

const MAX_DEPTH = 4;
const MAX_NUMBER_CHARS = 20;
const INT_MIN = -(2n ** 63n);
const INT_MAX = 2n ** 63n - 1n;

const CHAR_I = 0x69;
const CHAR_L = 0x6c;
const CHAR_D = 0x64;
const CHAR_E = 0x65;
const CHAR_COLON = 0x3a;
const CHAR_MINUS = 0x2d;
const CHAR_0 = 0x30;
const CHAR_9 = 0x39;

/**
 * A dictionary keyed by byte strings. Each key is held as a string with one
 * character per byte (0-255), which sorts exactly like the bytes it stands for.
 */
export type BencodeDict = Map<string, BencodeValue>;
export type BencodeValue = bigint | Uint8Array | BencodeValue[] | BencodeDict;

export class BencodeError extends Error {
  constructor(reason: string) {
    super(`invalid bencode: ${reason}`);
    this.name = 'BencodeError';
  }
}

function asciiBytes(text: string): Uint8Array {
  const bytes = new Uint8Array(text.length);
  for (let index = 0; index < text.length; index += 1) {
    const code = text.charCodeAt(index);
    if (code > 0xff) throw new BencodeError('dictionary key is not a byte string');
    bytes[index] = code;
  }
  return bytes;
}

function collect(value: BencodeValue, out: Uint8Array[]): void {
  if (typeof value === 'bigint') {
    if (value < INT_MIN || value > INT_MAX) throw new BencodeError('number is out of range');
    out.push(asciiBytes(`i${value}e`));
  } else if (value instanceof Uint8Array) {
    out.push(asciiBytes(`${value.length}:`), value);
  } else if (Array.isArray(value)) {
    out.push(asciiBytes('l'));
    for (const item of value) collect(item, out);
    out.push(asciiBytes('e'));
  } else {
    out.push(asciiBytes('d'));
    const keys = [...value.keys()].sort((left, right) => (left < right ? -1 : left > right ? 1 : 0));
    for (const key of keys) {
      out.push(asciiBytes(`${key.length}:`), asciiBytes(key));
      collect(value.get(key) as BencodeValue, out);
    }
    out.push(asciiBytes('e'));
  }
}

export function encodeBencode(value: BencodeValue): Uint8Array {
  const parts: Uint8Array[] = [];
  collect(value, parts);
  const out = new Uint8Array(parts.reduce((total, part) => total + part.length, 0));
  let at = 0;
  for (const part of parts) {
    out.set(part, at);
    at += part.length;
  }
  return out;
}

class Reader {
  at = 0;

  constructor(private readonly input: Uint8Array) {}

  private peek(): number {
    if (this.at >= this.input.length) throw new BencodeError('unexpected end');
    return this.input[this.at];
  }

  value(depth: number): BencodeValue {
    if (depth > MAX_DEPTH) throw new BencodeError('nested too deeply');
    const first = this.peek();
    if (first === CHAR_I) {
      this.at += 1;
      return this.number(CHAR_E, true);
    }
    if (first === CHAR_L) {
      this.at += 1;
      const items: BencodeValue[] = [];
      while (this.peek() !== CHAR_E) items.push(this.value(depth + 1));
      this.at += 1;
      return items;
    }
    if (first === CHAR_D) {
      this.at += 1;
      const entries: BencodeDict = new Map();
      let last: string | undefined;
      while (this.peek() !== CHAR_E) {
        const key = this.key();
        if (last !== undefined && last >= key) {
          throw new BencodeError('dictionary keys are not sorted');
        }
        entries.set(key, this.value(depth + 1));
        last = key;
      }
      this.at += 1;
      return entries;
    }
    if (first >= CHAR_0 && first <= CHAR_9) return this.bytes().slice();
    throw new BencodeError('unknown type');
  }

  private key(): string {
    const bytes = this.bytes();
    let key = '';
    for (const byte of bytes) key += String.fromCharCode(byte);
    return key;
  }

  /** A view of the string's bytes; callers copy what they keep. */
  private bytes(): Uint8Array {
    const length = this.number(CHAR_COLON, false);
    if (length > BigInt(this.input.length - this.at)) {
      throw new BencodeError('string is longer than the message');
    }
    const end = this.at + Number(length);
    const bytes = this.input.subarray(this.at, end);
    this.at = end;
    return bytes;
  }

  /** Parse a minimal decimal terminated by `end`. */
  private number(end: number, signed: boolean): bigint {
    const start = this.at;
    while (this.peek() !== end) {
      this.at += 1;
      if (this.at - start > MAX_NUMBER_CHARS) throw new BencodeError('number is too long');
    }
    const digits = this.input.subarray(start, this.at);
    this.at += 1;
    const negative = signed && digits.length > 0 && digits[0] === CHAR_MINUS;
    const magnitude = negative ? digits.subarray(1) : digits;
    let minimal = magnitude.length > 0;
    if (magnitude.length === 1 && magnitude[0] === CHAR_0) minimal = !negative;
    else if (magnitude.length > 1 && magnitude[0] === CHAR_0) minimal = false;
    else minimal = minimal && magnitude.every((byte) => byte >= CHAR_0 && byte <= CHAR_9);
    if (!minimal) throw new BencodeError('number is not canonical');
    const value = BigInt(String.fromCharCode(...digits));
    if (value < INT_MIN || value > INT_MAX) throw new BencodeError('number is out of range');
    return value;
  }
}

/** Decode one value from the start of `input`, returning it and its length. */
export function decodeBencodePrefix(input: Uint8Array): { value: BencodeValue; used: number } {
  const reader = new Reader(input);
  const value = reader.value(0);
  return { value, used: reader.at };
}
