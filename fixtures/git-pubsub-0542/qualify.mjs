import assert from 'node:assert/strict';
import { readFileSync, writeFileSync, readdirSync, statSync, openSync, closeSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { spawnSync, spawn } from 'node:child_process';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const evidence = '/stage/evidence';
const digest = file => createHash('sha256').update(readFileSync(file)).digest('hex');
const manifest = JSON.parse(readFileSync('/stage/manifest.json'));
const result = { source: manifest.source, tree: manifest.tree, steps: [], node: process.version };
const originalLock = digest('pnpm-lock.yaml');
const originalPackage = digest('package.json');
assert.equal(originalLock, manifest.lockSha256);
assert.equal(originalPackage, manifest.packageSha256);
function run(id, command, env = {}) {
  const started = Date.now();
  const monitorFd = openSync(`${evidence}/${id}-resources.log`, 'w');
  const monitor = spawn('sh', ['-c', 'while true; do date -u +%FT%TZ; for f in memory.current memory.peak memory.events pids.current pids.events; do echo "$f"; cat /sys/fs/cgroup/$f; done; ps -eo pid,ppid,rss,comm --sort=-rss | head -10; sleep 5; done'], {stdio:['ignore',monitorFd,monitorFd]});
  const output = spawnSync(command[0], command.slice(1), {
    env: {...process.env, ...env}, encoding:'utf8', maxBuffer:32 * 1024 * 1024,
    timeout: 2100000,
  });
  monitor.kill();
  closeSync(monitorFd);
  writeFileSync(`${evidence}/${id}.log`, `${output.stdout ?? ''}\n${output.stderr ?? ''}`);
  const status = output.status ?? 1;
  result.steps.push({id, command, status, seconds: (Date.now()-started)/1000, logSha256: digest(`${evidence}/${id}.log`)});
  writeFileSync(`${evidence}/qualification.json`, JSON.stringify(result,null,2));
  console.log(`${id} exited ${status}`);
  assert.equal(status, 0, `${id} failed`);
  assert.equal(digest('pnpm-lock.yaml'), originalLock, 'Frozen lock changed');
  assert.equal(digest('package.json'), originalPackage, 'Manifest changed');
}
run('install',['pnpm','install','--frozen-lockfile']);
run('installed-package',['python3','/stage/verify-package.py']);
run('unit',['pnpm','run','test:unit']);
const {createReleasePlan, parseArgs} = await import(pathToFileURL(resolve('scripts/release-site.mjs')));
const plan = createReleasePlan(parseArgs(['git','--dry-run']));
for (const step of plan.steps) {
  if (['install','publish','deploy'].includes(step.id)) continue;
  run(step.id,step.command);
}
const files = {};
function walk(directory, prefix='') {
  for (const name of readdirSync(directory)) {
    const file = directory + '/' + name, relative = prefix + name;
    if (statSync(file).isDirectory()) walk(file,relative+'/');
    else files[relative] = {sha256:digest(file),bytes:statSync(file).size};
  }
}
walk('iris-git');
result.files = files;
result.passed = true;
result.releaseProfileSha256 = digest('scripts/release-profiles.mjs');
writeFileSync(`${evidence}/qualification.json`, JSON.stringify(result,null,2));
run('archive-dist',['tar','-czf',`${evidence}/qualified-dist.tar.gz`,'-C','iris-git','.']);
console.log('Exact source passed all pre-publication gates');
