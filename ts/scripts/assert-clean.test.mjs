import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

const script = fileURLToPath(new URL('./assert-clean.mjs', import.meta.url));

test('assert-clean rejects tracked and untracked generated files', () => {
  const repo = mkdtempSync(path.join(os.tmpdir(), 'hashtree-assert-clean-'));
  const tracked = path.join(repo, 'dist', 'tracked.js');
  try {
    mkdirSync(path.dirname(tracked));
    writeFileSync(tracked, 'tracked\n');
    git(repo, 'init', '-q');
    git(repo, 'add', 'dist');
    git(repo, '-c', 'user.name=Test', '-c', 'user.email=test@example.invalid', 'commit', '-qm', 'fixture');

    assert.doesNotThrow(() => run(repo));
    writeFileSync(tracked, 'changed\n');
    assert.throws(() => run(repo));
    writeFileSync(tracked, 'tracked\n');
    writeFileSync(path.join(repo, 'dist', 'untracked.js'), 'new\n');
    assert.throws(() => run(repo));
  } finally {
    rmSync(repo, { recursive: true, force: true });
  }
});

function git(cwd, ...args) {
  execFileSync('git', args, { cwd, stdio: 'ignore' });
}

function run(cwd) {
  execFileSync(process.execPath, [script, 'dist'], { cwd, stdio: 'ignore' });
}
