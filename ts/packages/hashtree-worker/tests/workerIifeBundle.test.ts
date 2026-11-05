import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { expect, it } from 'vitest';

it('bundles public graph reconciliation in a default IIFE consumer worker', async () => {
  // Reuse the Vite version already locked by our test runner; no added build dependency.
  const require = createRequire(import.meta.url);
  const testRunner = createRequire(require.resolve('vitest/package.json'));
  const { build } = await import(pathToFileURL(testRunner.resolve('vite')).href);
  const fixture = await mkdtemp(path.join(tmpdir(), 'hashtree-worker-iife-'));
  try {
    const entry = fileURLToPath(new URL('../src/relay-entry.ts', import.meta.url));
    await writeFile(path.join(fixture, 'main.js'), `import Worker from ${JSON.stringify(`${entry}?worker`)}; new Worker();`);
    const result = await build({ configFile: false, root: fixture, publicDir: false, logLevel: 'silent',
      worker: { format: 'iife' },
      build: { write: false, minify: false, rollupOptions: { input: path.join(fixture, 'main.js') } },
    });
    const outputs = (Array.isArray(result) ? result : [result]).flatMap(result => result.output);
    expect(outputs.some(output => output.type === 'asset' && String(output.source).includes('NEG-OPEN'))).toBe(true);
  } finally { await rm(fixture, { recursive: true, force: true }); }
}, 30000);
