import { spawnSync } from 'node:child_process';

const paths = process.argv.slice(2);
if (paths.length === 0) throw new Error('assert-clean requires at least one path');

const result = spawnSync(
  'git',
  ['status', '--porcelain=v1', '--untracked-files=all', '--', ...paths],
  { encoding: 'utf8' },
);
if (result.error) throw result.error;
if (result.status !== 0 || result.stdout) {
  process.stderr.write(result.stderr || result.stdout);
  process.exit(result.status || 1);
}
