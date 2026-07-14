import test from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { existsSync } from 'node:fs';
import path from 'node:path';

const repoRoot = path.resolve(path.dirname(new URL(import.meta.url).pathname), '..');
const script = path.join(repoRoot, 'scripts', 'publish.sh');

test('standalone social graph source stays outside the Hashtree workspace', () => {
  assert.equal(existsSync(path.join(repoRoot, 'packages', 'nostr-social-graph')), false);
});

test('publish plan lists hashtree npm packages in dependency order', () => {
  const output = execFileSync(script, ['--plan'], {
    cwd: repoRoot,
    encoding: 'utf8',
  });

  const packages = output
    .trim()
    .split('\n')
    .filter(Boolean)
    .filter((line) => line.startsWith('@'));

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
