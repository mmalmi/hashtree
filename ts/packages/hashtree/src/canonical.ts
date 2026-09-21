import { compareNames } from './compare.js';

const minInteger = -(1n << 63n);
const maxInteger = (1n << 64n) - 1n;

export function assertUnicode(value: string): void {
  // In Unicode mode, a valid surrogate pair is a single non-surrogate code point.
  if (/[\uD800-\uDFFF]/u.test(value)) throw new Error('Invalid Unicode string');
}

export function canonicalNumber(value: number | bigint): number | bigint {
  if (typeof value === 'number') {
    if (!Number.isFinite(value)) throw new Error('Metadata numbers must be finite');
    if (!Number.isInteger(value) || value < -(2 ** 63) || value >= 2 ** 64) return value;
    if (value >= -2147483648 && value <= 4294967295) return value === 0 ? 0 : value;
    value = BigInt(value);
  }
  if (value < minInteger || value > maxInteger) throw new Error('Integer outside MessagePack range');
  // With useBigInt64, the library needs bigint for the 64-bit integer formats.
  return value >= -2147483648n && value <= 4294967295n ? Number(value) : value;
}

export function canonicalMetadata(metadata: Record<string, unknown>): Record<string, unknown> {
  const ancestors = new Set<object>();
  function visit(value: unknown, depth: number): unknown {
    if (depth > 100) throw new Error('Metadata nesting limit exceeded');
    if (value === null || typeof value === 'boolean') return value;
    if (typeof value === 'number' || typeof value === 'bigint') return canonicalNumber(value);
    if (typeof value === 'string') {
      assertUnicode(value);
      return value;
    }
    if (typeof value !== 'object') throw new Error('Unsupported metadata value');
    if (ancestors.has(value)) throw new Error('Cyclic metadata');
    ancestors.add(value);
    try {
      if (Array.isArray(value)) return Array.from(value, item => visit(item, depth + 1));
      const prototype = Object.getPrototypeOf(value);
      if (prototype !== Object.prototype && prototype !== null) throw new Error('Metadata must use plain maps');
      if (Object.getOwnPropertySymbols(value).length) throw new Error('Metadata keys must be strings');
      const keys = Object.keys(value).sort(compareNames);
      const sorted: Record<string, unknown> = Object.create(null);
      for (const key of keys) {
        assertUnicode(key);
        // The MessagePack decoder reserves this key; never emit a manifest we cannot read.
        if (key === '__proto__') throw new Error('Unsupported metadata key: __proto__');
        sorted[key] = visit((value as Record<string, unknown>)[key], depth + 1);
      }
      // Object insertion order alone cannot preserve byte order for keys like "10", "2".
      return new Proxy(sorted, { ownKeys: () => keys });
    } finally {
      ancestors.delete(value);
    }
  }
  if (metadata === null || typeof metadata !== 'object' || Array.isArray(metadata)) {
    throw new Error('Metadata must be a map');
  }
  return visit(metadata, 0) as Record<string, unknown>;
}

export function decodedIntegers(value: unknown): unknown {
  if (typeof value === 'bigint') {
    return value >= BigInt(Number.MIN_SAFE_INTEGER) && value <= BigInt(Number.MAX_SAFE_INTEGER)
      ? Number(value) : value;
  }
  if (Array.isArray(value)) return value.map(decodedIntegers);
  if (value !== null && typeof value === 'object' && !(value instanceof Uint8Array)) {
    for (const key of Object.keys(value)) {
      const map = value as Record<string, unknown>;
      map[key] = decodedIntegers(map[key]);
    }
  }
  return value;
}
