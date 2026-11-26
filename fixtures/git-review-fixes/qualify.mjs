import assert from 'node:assert/strict';
import { readFileSync, writeFileSync, readdirSync, statSync, openSync, closeSync, writeSync, cpSync, symlinkSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { spawn } from 'node:child_process';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const evidence = '/stage/evidence';
const digest = file => createHash('sha256').update(readFileSync(file)).digest('hex');
const manifest = JSON.parse(readFileSync('/stage/manifest.json'));
const result = { source: manifest.source, tree: manifest.tree, steps: [], node: process.version };
const originalLock = digest('pnpm-lock.yaml'), originalPackage = digest('package.json');
assert.equal(originalLock, manifest.lockSha256);
assert.equal(originalPackage, manifest.packageSha256);
const save = () => writeFileSync(`${evidence}/qualification.json`, JSON.stringify(result,null,2));
const oomKills = () => Number(readFileSync('/sys/fs/cgroup/memory.events','utf8').match(/^oom_kill (\d+)/m)[1]);
async function run(id, command, env = {}, cwd = "/stage/app", expectedStatus = 0) {
  const started = Date.now(), initialOom = oomKills();
  const fd = openSync(`${evidence}/${id}.log`, 'w');
  const resourceFd = openSync(`${evidence}/${id}-resources.jsonl`, 'w');
  const child = spawn(command[0], command.slice(1), {
    cwd, env: {...process.env, ...env}, stdio:['ignore',fd,fd], detached:true,
  });
  const stop = reason => {
    writeSync(fd, `\nQualification stopped: ${reason}\n`);
    try { process.kill(-child.pid, 'SIGTERM'); } catch {}
  };
  const timer = setTimeout(()=>stop('command exceeded 35 minutes'),2100000);
  const monitor = setInterval(()=>{
    writeSync(resourceFd, JSON.stringify({at:new Date().toISOString(), memory:Number(readFileSync('/sys/fs/cgroup/memory.current','utf8')), peak:Number(readFileSync('/sys/fs/cgroup/memory.peak','utf8')),oomKills:oomKills()})+'\n');
    if(oomKills()>initialOom)stop('container OOM; refusing cascading browser failures');
  },5000);
  const status = await new Promise(resolve=>child.on('exit',code=>resolve(code??1)));
  clearInterval(monitor);clearTimeout(timer);closeSync(fd);closeSync(resourceFd);
  result.steps.push({id,command,status,seconds:(Date.now()-started)/1000,logSha256:digest(`${evidence}/${id}.log`)});
  save();console.log(`${id} exited ${status}`);
  if(status!==0)console.log(readFileSync(`${evidence}/${id}.log`,'utf8').slice(-4000));
  assert.equal(status,expectedStatus,`${id} unexpected status`);
  assert.equal(digest('pnpm-lock.yaml'),originalLock,'Frozen lock changed');
  assert.equal(digest('package.json'),originalPackage,'Manifest changed');
}

async function browser(id, files, cwd='/stage/app', expectedStatus=0, extra=[]) {
  const report=`${evidence}/${id}.json`;
  try { await run(id,['pnpm','exec','playwright','test',...files,'--retries=0','--reporter=list,json',...extra],
    { PLAYWRIGHT_JSON_OUTPUT_FILE:report },cwd,expectedStatus); }
  finally { try { cpSync(`${cwd}/test-results`,`${evidence}/${id}-results`,{recursive:true}); } catch {} }
}
await run('install',['pnpm','install','--frozen-lockfile']);
await run('dependency-lock',['pnpm','run','verify:dependency-lock']);
symlinkSync('/stage/app/node_modules','/stage/baseline/node_modules','dir');
await run('baseline-cancellation',['pnpm','exec','vitest','run','tests/treeForkCancellation.test.ts'],{},'/stage/baseline',1);
await browser('baseline-browser',['e2e/git-ngit-fork.spec.ts'],'/stage/baseline',1,['-g','cancel an ngit fork']);
await run('candidate-focused-unit',['pnpm','exec','vitest','run','tests/gitDiffText.test.ts','tests/gitPackedHistory.test.ts','tests/treeForkCancellation.test.ts']);
await browser('candidate-fork',['e2e/git-ngit-fork.spec.ts']);
await browser('candidate-diff',['e2e/git-branch-compare.spec.ts','e2e/git-commit-view.spec.ts']);
await run('candidate-build',['pnpm','run','build:git']);
result.passed=true;result.hold='No deployment or native publisher pending parent TTL choice';save();
console.log('Exact source passed focused regression and browser checks; no publication performed');
