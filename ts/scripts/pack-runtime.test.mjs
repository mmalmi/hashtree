import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import { runtimePackageDirs } from './runtime-packages.mjs';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

test('runtime packages include the TypeScript sources referenced by source maps', () => {
  for (const packageDir of runtimePackageDirs) {
    const manifest = JSON.parse(readFileSync(
      path.join(root, 'packages', packageDir, 'package.json'),
      'utf8',
    ));
    assert.ok(
      manifest.files?.includes('src'),
      `${manifest.name} must pack src beside dist source maps`,
    );
  }
});
