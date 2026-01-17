/**
 * Bech32-encoded identifiers for hashtree content
 *
 * Similar to nostr's nip19 (npub, nprofile, nevent, naddr),
 * provides human-readable, copy-pasteable identifiers.
 *
 * Types:
 * - nhash: Permalink (hash + optional decrypt key)
 * - npath: Live reference (pubkey + tree + path + optional decrypt key)
 */

import { bech32 } from '@scure/base';
import { bytesToHex, hexToBytes, concatBytes } from '@noble/hashes/utils.js';
import type { Hash, CID } from './types.js';

const BECH32_MAX_SIZE = 5000;
const utf8Encoder = new TextEncoder();
const utf8Decoder = new TextDecoder('utf-8');

// TLV type constants
const TLV = {
  /** 32-byte hash (required for nhash) */
  HASH: 0,
  /** 32-byte nostr pubkey (required for npath) */
  PUBKEY: 2,
  /** UTF-8 tree name (required for npath) */
  TREE_NAME: 3,
  /** UTF-8 path segment (can appear multiple times, in order) */
  PATH: 4,
  /** 32-byte decryption key (optional) */
  DECRYPT_KEY: 5,
} as const;

/**
 * NHash data - permalink to content by hash
 * Path is kept in URL segments, not encoded in nhash (e.g., /nhash1.../path/to/file)
 */
export interface NHashData {
  /** 32-byte merkle hash (hex string) */
  hash: string;
  /** 32-byte decryption key (hex string) - optional */
  decryptKey?: string;
}

/**
 * NPath data - live reference via pubkey + tree + path
 */
export interface NPathData {
  /** 32-byte nostr pubkey (hex string) */
  pubkey: string;
  /** Tree name (e.g., "home", "photos") */
  treeName: string;
  /** Path segments within the tree (e.g., ["folder", "file.txt"]) */
  path: string[];
  /** 32-byte decryption key (hex string) - optional */
  decryptKey?: string;
}

export type DecodeResult =
  | { type: 'nhash'; data: CID }
  | { type: 'npath'; data: NPathData };

/**
 * Parse TLV-encoded data into a map of type -> values[]
 */
function parseTLV(data: Uint8Array): Record<number, Uint8Array[]> {
  const result: Record<number, Uint8Array[]> = {};
  let offset = 0;

  while (offset < data.length) {
    if (offset + 2 > data.length) {
      throw new Error('TLV: unexpected end of data');
    }
    const t = data[offset];
    const l = data[offset + 1];
    offset += 2;

    if (offset + l > data.length) {
      throw new Error(`TLV: not enough data for type ${t}, need ${l} bytes`);
    }
    const v = data.slice(offset, offset + l);
    offset += l;

    result[t] = result[t] || [];
    result[t].push(v);
  }

  return result;
}

/**
 * Encode TLV data to bytes
 */
function encodeTLV(tlv: Record<number, Uint8Array[]>): Uint8Array {
  const entries: Uint8Array[] = [];

  // Process in ascending key order for consistent encoding
  const keys = Object.keys(tlv).map(Number).sort((a, b) => a - b);

  for (const t of keys) {
    const values = tlv[t];
    for (const v of values) {
      if (v.length > 255) {
        throw new Error(`TLV value too long for type ${t}: ${v.length} bytes`);
      }
      const entry = new Uint8Array(v.length + 2);
      entry[0] = t;
      entry[1] = v.length;
      entry.set(v, 2);
      entries.push(entry);
    }
  }

  return concatBytes(...entries);
}

/**
 * Encode bech32 with given prefix and data
 */
function encodeBech32(prefix: string, data: Uint8Array): string {
  const words = bech32.toWords(data);
  return bech32.encode(prefix, words, BECH32_MAX_SIZE);
}

// ============================================================================
// nhash - Permalink (hash + optional decrypt key)
// ============================================================================

/**
 * Check if object is a CID (has hash as Uint8Array)
 */
function isCID(data: unknown): data is CID {
  return (
    typeof data === 'object' &&
    data !== null &&
    'hash' in data &&
    (data as CID).hash instanceof Uint8Array
  );
}

/**
 * Encode an nhash permalink
 *
 * Path is kept in URL segments, not encoded in nhash.
 * Use format: /nhash1.../path/to/file
 *
 * @example
 * // Simple hash
 * const encoded = nhashEncode('abc123...');
 * const encoded = nhashEncode(hashBytes);
 *
 * @example
 * // Hash with decrypt key (hex strings)
 * const encoded = nhashEncode({ hash: 'abc123...', decryptKey: 'key...' });
 *
 * @example
 * // CID with Uint8Array fields
 * const encoded = nhashEncode({ hash: hashBytes, key: keyBytes });
 */
export function nhashEncode(data: string | Hash | NHashData | CID): string {
  // Simple hash-only case
  if (typeof data === 'string' || data instanceof Uint8Array) {
    const hashBytes = typeof data === 'string' ? hexToBytes(data) : data;
    if (hashBytes.length !== 32) {
      throw new Error(`Hash must be 32 bytes, got ${hashBytes.length}`);
    }
    return encodeBech32('nhash', hashBytes);
  }

  // CID type (Uint8Array fields)
  if (isCID(data)) {
    const hashBytes = data.hash;
    if (hashBytes.length !== 32) {
      throw new Error(`Hash must be 32 bytes, got ${hashBytes.length}`);
    }

    // No key - simple encoding
    if (!data.key) {
      return encodeBech32('nhash', hashBytes);
    }

    // Has key - use TLV
    const tlv: Record<number, Uint8Array[]> = {
      [TLV.HASH]: [hashBytes],
    };

    if (data.key.length !== 32) {
      throw new Error(`Decrypt key must be 32 bytes, got ${data.key.length}`);
    }
    tlv[TLV.DECRYPT_KEY] = [data.key];

    return encodeBech32('nhash', encodeTLV(tlv));
  }

  // NHashData type (hex string fields)
  const hashBytes = hexToBytes(data.hash);
  if (hashBytes.length !== 32) {
    throw new Error(`Hash must be 32 bytes, got ${hashBytes.length}`);
  }

  // No decrypt key - simple encoding
  if (!data.decryptKey) {
    return encodeBech32('nhash', hashBytes);
  }

  // Has decrypt key - use TLV
  const tlv: Record<number, Uint8Array[]> = {
    [TLV.HASH]: [hashBytes],
  };

  const keyBytes = hexToBytes(data.decryptKey);
  if (keyBytes.length !== 32) {
    throw new Error(`Decrypt key must be 32 bytes, got ${keyBytes.length}`);
  }
  tlv[TLV.DECRYPT_KEY] = [keyBytes];

  return encodeBech32('nhash', encodeTLV(tlv));
}

/**
 * Decode an nhash string to CID (Uint8Array fields)
 *
 * Path is kept in URL segments, not encoded in nhash.
 */
export function nhashDecode(code: string): CID {
  if (code.startsWith('hashtree:')) {
    code = code.substring(9);
  }

  const { prefix, words } = bech32.decode(code as `${string}1${string}`, BECH32_MAX_SIZE);

  if (prefix !== 'nhash') {
    throw new Error(`Expected nhash prefix, got ${prefix}`);
  }

  const data = new Uint8Array(bech32.fromWords(words));

  // Simple 32-byte hash (no TLV)
  if (data.length === 32) {
    return { hash: data };
  }

  // Parse TLV
  const tlv = parseTLV(data);

  if (!tlv[TLV.HASH]?.[0] || tlv[TLV.HASH][0].length !== 32) {
    throw new Error('nhash: missing or invalid hash');
  }

  const result: CID = {
    hash: tlv[TLV.HASH][0],
  };

  if (tlv[TLV.DECRYPT_KEY]?.[0]) {
    if (tlv[TLV.DECRYPT_KEY][0].length !== 32) {
      throw new Error('nhash: decrypt key must be 32 bytes');
    }
    result.key = tlv[TLV.DECRYPT_KEY][0];
  }

  return result;
}

// ============================================================================
// npath - Live reference (pubkey + tree + path + optional decrypt key)
// ============================================================================

/**
 * Encode an npath live reference
 *
 * @example
 * const encoded = npathEncode({
 *   pubkey: 'abc123...',
 *   treeName: 'home',
 *   path: ['photos', 'vacation.jpg'],
 * });
 */
export function npathEncode(data: NPathData): string {
  const pubkeyBytes = hexToBytes(data.pubkey);
  if (pubkeyBytes.length !== 32) {
    throw new Error(`Pubkey must be 32 bytes, got ${pubkeyBytes.length}`);
  }

  const tlv: Record<number, Uint8Array[]> = {
    [TLV.PUBKEY]: [pubkeyBytes],
    [TLV.TREE_NAME]: [utf8Encoder.encode(data.treeName)],
  };

  if (data.path && data.path.length > 0) {
    tlv[TLV.PATH] = data.path.map(p => utf8Encoder.encode(p));
  }

  if (data.decryptKey) {
    const keyBytes = hexToBytes(data.decryptKey);
    if (keyBytes.length !== 32) {
      throw new Error(`Decrypt key must be 32 bytes, got ${keyBytes.length}`);
    }
    tlv[TLV.DECRYPT_KEY] = [keyBytes];
  }

  return encodeBech32('npath', encodeTLV(tlv));
}

/**
 * Decode an npath string
 */
export function npathDecode(code: string): NPathData {
  if (code.startsWith('hashtree:')) {
    code = code.substring(9);
  }

  const { prefix, words } = bech32.decode(code as `${string}1${string}`, BECH32_MAX_SIZE);

  if (prefix !== 'npath') {
    throw new Error(`Expected npath prefix, got ${prefix}`);
  }

  const data = new Uint8Array(bech32.fromWords(words));
  const tlv = parseTLV(data);

  if (!tlv[TLV.PUBKEY]?.[0] || tlv[TLV.PUBKEY][0].length !== 32) {
    throw new Error('npath: missing or invalid pubkey');
  }
  if (!tlv[TLV.TREE_NAME]?.[0]) {
    throw new Error('npath: missing tree name');
  }

  const result: NPathData = {
    pubkey: bytesToHex(tlv[TLV.PUBKEY][0]),
    treeName: utf8Decoder.decode(tlv[TLV.TREE_NAME][0]),
    path: tlv[TLV.PATH] ? tlv[TLV.PATH].map(p => utf8Decoder.decode(p)) : [],
  };

  if (tlv[TLV.DECRYPT_KEY]?.[0]) {
    if (tlv[TLV.DECRYPT_KEY][0].length !== 32) {
      throw new Error('npath: decrypt key must be 32 bytes');
    }
    result.decryptKey = bytesToHex(tlv[TLV.DECRYPT_KEY][0]);
  }

  return result;
}

// ============================================================================
// Generic decode
// ============================================================================

/**
 * Decode any nhash or npath string
 */
export function decode(code: string): DecodeResult {
  if (code.startsWith('hashtree:')) {
    code = code.substring(9);
  }

  if (code.startsWith('nhash1')) {
    return { type: 'nhash', data: nhashDecode(code) };
  }
  if (code.startsWith('npath1')) {
    return { type: 'npath', data: npathDecode(code) };
  }

  throw new Error(`Unknown prefix, expected nhash1 or npath1`);
}

// ============================================================================
// Type guards
// ============================================================================

export function isNHash(value: string | undefined | null): boolean {
  return /^nhash1[a-z\d]+$/.test(value || '');
}

export function isNPath(value: string | undefined | null): boolean {
  return /^npath1[a-z\d]+$/.test(value || '');
}

export const NHashTypeGuard = {
  isNHash,
  isNPath,
};

export const BECH32_REGEX = /[\x21-\x7E]{1,83}1[023456789acdefghjklmnpqrstuvwxyz]{6,}/;
