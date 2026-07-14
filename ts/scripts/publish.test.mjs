import test from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import path from 'node:path';

const repoRoot = path.resolve(path.dirname(new URL(import.meta.url).pathname), '..');
const script = path.join(repoRoot, 'scripts', 'publish.sh');

test('publish plan lists hashtree npm packages in dependency order', () => {
  const output = execFileSync(script, ['--plan'], {
    cwd: repoRoot,
    encoding: 'utf8',
  });

  const packages = output
    .trim()
    .split('\n')
    .filter(Boolean)
    .filter((line) => line.startsWith('@') || line.startsWith('nostr-social-graph'));

  assert.deepEqual(packages, [
    '@hashtree/core',
    '@hashtree/merge',
    '@hashtree/dexie',
    '@hashtree/git',
    '@hashtree/index',
    '@hashtree/collection',
    '@hashtree/mesh',
    '@hashtree/nostr',
    '@hashtree/worker',
  ]);
});
