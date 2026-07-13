import { describe, expect, it } from 'vitest';
import { parseHttpByteRange } from '../src/httpRange.js';

const cases: Array<{ header: string | null; size: number; expected: unknown }> = [
  { header: 'bytes=2-5', size: 10, expected: { kind: 'range', range: { start: 2, endInclusive: 5 } } },
  { header: 'bytes=-3', size: 10, expected: { kind: 'range', range: { start: 7, endInclusive: 9 } } },
  { header: 'bytes=8-', size: 10, expected: { kind: 'range', range: { start: 8, endInclusive: 9 } } },
  { header: null, size: 10, expected: { kind: 'unsupported' } },
  { header: 'items=0-1', size: 10, expected: { kind: 'unsupported' } },
  { header: 'bytes=0-1,3-4', size: 10, expected: { kind: 'unsupported' } },
  { header: 'bytes=10-', size: 10, expected: { kind: 'unsatisfiable' } },
  { header: 'bytes=5-2', size: 10, expected: { kind: 'unsatisfiable' } },
];

describe('parseHttpByteRange', () => {
  it.each(cases)('parses $header against $size bytes', ({ header, size, expected }) => {
    expect(parseHttpByteRange(header, size)).toEqual(expected);
  });
});
