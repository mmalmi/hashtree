import { readFileSync } from 'node:fs';
import { encode } from '@msgpack/msgpack';
import { describe, expect, it } from 'vitest';
import { encodeAndHash, encodeTreeNode, decodeTreeNode } from '../src/codec.js';
import { LinkType, toHex } from '../src/types.js';

const fixture = JSON.parse(readFileSync(new URL(
  '../../../../rust/crates/hashtree-core/tests/canonical-directory.json', import.meta.url,
), 'utf8'));

function directory(meta: Record<string, unknown>) {
  return { type: LinkType.Dir, links: [{
    hash: new Uint8Array(32).fill(0xab), name: 'x', size: 0, type: LinkType.Blob, meta,
  }] };
}

describe('canonical metadata encoding', () => {
  it('matches the shared Rust/BUD-16 bytes and hash', async () => {
    const result = await encodeAndHash(directory(fixture.metadata));
    expect(toHex(result.data)).toBe(fixture.msgpack);
    expect(toHex(result.hash)).toBe(fixture.sha256);
  });

  it('sorts nested maps, including maps inside arrays, without mutating input', () => {
    const first = { items: [{ z: 1, a: { z: 2, a: 3 } }] };
    const second = { items: [{ a: { a: 3, z: 2 }, z: 1 }] };
    expect(encodeTreeNode(directory(first))).toEqual(encodeTreeNode(directory(second)));
    expect(Object.keys(first.items[0])).toEqual(['z', 'a']);
  });

  it('preserves exact 64-bit metadata integers and canonicalizes integral numbers', () => {
    const meta = { max: 18446744073709551615n, min: -9223372036854775808n };
    const encoded = encodeTreeNode(directory(meta));
    expect(decodeTreeNode(encoded).links[0].meta).toEqual(meta);
    expect(encodeTreeNode(directory({ n: 1n }))).toEqual(encodeTreeNode(directory({ n: 1 })));
    expect(encodeTreeNode(directory({ n: -0 }))).toEqual(encodeTreeNode(directory({ n: 0 })));
    expect(encodeTreeNode(directory({ n: 2 ** 53 }))).toEqual(encodeTreeNode(directory({ n: 2n ** 53n })));
  });

  it('preserves exact supported link sizes and rejects sizes it would round', () => {
    const node = directory({});
    for (const size of [2 ** 32, Number.MAX_SAFE_INTEGER]) {
      node.links[0].size = size;
      expect(decodeTreeNode(encodeTreeNode(node)).links[0].size).toBe(size);
    }
    node.links[0].size = 2 ** 53;
    expect(() => encodeTreeNode(node)).toThrow('safe integer');
    const legacy = encode({ l: [{ h: node.links[0].hash, n: 'x', s: 2n ** 53n, t: 0 }], t: 2 },
      { useBigInt64: true });
    expect(() => decodeTreeNode(legacy)).toThrow('exact integer range');
  });

  it('retains empty metadata, array order, and distinct Unicode sequences', () => {
    const absent = directory({});
    delete (absent.links[0] as { meta?: unknown }).meta;
    expect(encodeTreeNode(absent)).not.toEqual(encodeTreeNode(directory({})));
    expect(encodeTreeNode(directory({ values: [1, 2] })))
      .not.toEqual(encodeTreeNode(directory({ values: [2, 1] })));
    expect(encodeTreeNode(directory({ value: '\u00e9' })))
      .not.toEqual(encodeTreeNode(directory({ value: 'e\u0301' })));
  });

  it('continues to decode legacy map orders and float representations', () => {
    const legacy = encode({ t: 2, l: [{ t: 0, s: 0, n: 'x', m: { z: -0, a: 1 },
      h: new Uint8Array(32).fill(0xab) }] }, { forceIntegerToFloat: true });
    const decoded = decodeTreeNode(legacy);
    expect(decoded.links[0].meta).toEqual({ z: -0, a: 1 });
    expect(encodeTreeNode(decoded)).toEqual(encodeTreeNode(directory({ a: 1, z: 0 })));
  });

  it.each([NaN, Infinity, -Infinity, undefined, new Date(), new Uint8Array([1]),
    () => 1, Symbol('x'), 18446744073709551616n, -9223372036854775809n,
    '\ud800', { '\udc00': 1 }, [undefined], new Array(1), JSON.parse('{"__proto__":1}'),
  ])('rejects values outside the metadata data model: %s', value => {
    expect(() => encodeTreeNode(directory({ value }))).toThrow();
  });

  it('rejects cycles but permits repeated references', () => {
    const cyclic: Record<string, unknown> = {};
    cyclic.self = cyclic;
    expect(() => encodeTreeNode(directory(cyclic))).toThrow();
    const shared = { a: 1 };
    expect(() => encodeTreeNode(directory({ a: shared, b: shared }))).not.toThrow();
  });
});
