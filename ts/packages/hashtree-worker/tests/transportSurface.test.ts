import { existsSync, readFileSync, readdirSync } from 'node:fs';
import { describe, expect, it } from 'vitest';
import * as workerSurface from '../src/index.js';

describe('worker transport surface', () => {
  it('has no direct WebRTC implementation or public API', () => {
    const packageJson = JSON.parse(
      readFileSync(new URL('../package.json', import.meta.url), 'utf8'),
    ) as { exports?: Record<string, unknown> };

    const legacySourceDirectory = new URL('../src/p2p', import.meta.url);
    expect(
      existsSync(legacySourceDirectory) ? readdirSync(legacySourceDirectory) : [],
    ).toEqual([]);
    expect(packageJson.exports).not.toHaveProperty('./p2p');
    expect(Object.keys(workerSurface).filter((name) => /webrtc/i.test(name))).toEqual([]);
  });
});
