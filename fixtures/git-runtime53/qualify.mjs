import assert from 'node:assert/strict';
import { readFileSync, writeFileSync, readdirSync, statSync, openSync, closeSync, writeSync, cpSync } from 'node:fs';
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
  assert.equal(status,expectedStatus,`${id} status differed`);
  assert.equal(digest('pnpm-lock.yaml'),originalLock,'Frozen lock changed');
  assert.equal(digest('package.json'),originalPackage,'Manifest changed');
}
function cases(report, requirePassed=false) {
  const found=[];
  const walk=suites=>{for(const suite of suites??[]){
    for(const spec of suite.specs??[])for(const test of spec.tests??[]){
      if(requirePassed){assert(['expected','flaky'].includes(test.status),spec.title);assert(test.results.some(r=>r.status==='passed'),spec.title);}
      found.push(`${spec.id}:${test.projectId}`);
    }
    walk(suite.suites);
  }};
  walk(report.suites);return found.sort();
}
await run('install',['pnpm','install','--frozen-lockfile']);
await run('installed-package',['python3','/stage/verify-cohort.py','installed']);
await run('baseline-install',['pnpm','install','--frozen-lockfile'],{},'/stage/baseline');
const testFiles=['tests/releasesStoreFreshRoot.test.ts','tests/gitPackedHistory.test.ts','tests/ngit.test.ts'];
for(const file of testFiles)cpSync('/stage/app/'+file,'/stage/baseline/'+file);
await run('regressions-before',['pnpm','exec','vitest','run',...testFiles,'-t','keeps both subscribed|watches subscribed|shows submodule|rejects mutually|preserves reserved characters','--reporter=json','--outputFile=/stage/evidence/regressions-before.json'],{},'/stage/baseline',1);
const red=JSON.parse(readFileSync(`${evidence}/regressions-before.json`));assert.equal(red.numFailedTests,5);assert.equal(red.numPassedTests,0);
await run('focused-unit',['pnpm','exec','vitest','run',...testFiles,'tests/releasesStore.test.ts','tests/gitDiffText.test.ts','tests/gitRefs.test.ts','tests/gitRepoReadiness.test.ts','--reporter=json','--outputFile=/stage/evidence/focused-unit.json']);
for(const [label,cwd] of [['before','/stage/baseline'],['after','/stage/app']]){
 cpSync('/stage/runtime53-performance.spec.ts',cwd+'/e2e/runtime53-performance.spec.ts');
 const id='performance-'+label;
 try{await run(id,['pnpm','exec','playwright','test','e2e/runtime53-performance.spec.ts','--retries=0','--reporter=list,json'],{PLAYWRIGHT_JSON_OUTPUT_FILE:`${evidence}/${id}.json`},cwd);}
 finally{try{cpSync(cwd+'/test-results',`${evidence}/${id}-results`,{recursive:true});}catch{}}
}
for(const [id,file] of [['commit-view','e2e/git-commit-view.spec.ts'],['navigation','e2e/git-navigation-layout.spec.ts']]){
 try{await run(id,['pnpm','exec','playwright','test',file,'--retries=0','--reporter=list,json'],{PLAYWRIGHT_JSON_OUTPUT_FILE:`${evidence}/${id}.json`});}
 finally{try{cpSync('test-results',`${evidence}/${id}-results`,{recursive:true});}catch{}}
}
result.passed=true;result.scope='Scoped review regression checks; not final cohort release qualification';save();
