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
async function run(id, command, env = {}, cwd = "/stage/app") {
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
  assert.equal(status,0,`${id} failed`);
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
await run('unit',['pnpm','run','test:unit']);
const {createReleasePlan,parseArgs} = await import(pathToFileURL(resolve('scripts/release-site.mjs')));
const plan = createReleasePlan(parseArgs(['git','--dry-run']));
for(const step of plan.steps){
  if(['install','publish','deploy'].includes(step.id))continue;
  if(step.id!=='test-2'){await run(step.id,step.command);continue;}
  assert.deepEqual(step.command,['pnpm','run','test:e2e:git']);
  const started=Date.now();
  const list=`${evidence}/test-2-cases.json`;
  await run('test-2-list',[...step.command,'--list','--reporter=json'],{PLAYWRIGHT_JSON_OUTPUT_FILE:list});
  const expected=cases(JSON.parse(readFileSync(list)));
  assert.equal(expected.length,97,'Expected original 95 cases plus two qualified cancellation regressions');
  const actual=[], reports=[];
  for(let shard=1;shard<=8;shard++){
    const id=`test-2-shard-${shard}`, file=`${evidence}/${id}.json`;
    try {await run(id,[...step.command,`--shard=${shard}/8`,'--reporter=list,json'],{PLAYWRIGHT_JSON_OUTPUT_FILE:file});}
    finally {try{cpSync('test-results',`${evidence}/${id}-results`,{recursive:true});}catch{}}
    actual.push(...cases(JSON.parse(readFileSync(file)),true));
    reports.push({id,file:id+'.json',sha256:digest(file)});
  }
  assert.deepEqual(actual.sort(),expected,'Shards must cover every original test exactly once');
  const coverage={mode:'Playwright built-in sequential sharding with fresh servers',inventory:{file:'test-2-cases.json',sha256:digest(list)},reports,tests:expected.length};
  writeFileSync(`${evidence}/test-2.log`,JSON.stringify(coverage,null,2)+'\n');
  result.steps.push({id:step.id,command:step.command,status:0,seconds:(Date.now()-started)/1000,logSha256:digest(`${evidence}/test-2.log`),coverage});
  save();console.log('All 97 browser cases passed across 8 fresh-server shards');
}
// Same deterministic packed fixture, isolated storage, same runner and origin.
await run('baseline-install',['pnpm','install','--frozen-lockfile'],{},'/stage/baseline');
for(const [label,cwd] of [['before','/stage/baseline'],['after','/stage/app']]){
  cpSync('/stage/runtime53-performance.spec.ts',cwd+'/e2e/runtime53-performance.spec.ts');
  const id='performance-'+label;
  try{await run(id,['pnpm','exec','playwright','test','e2e/runtime53-performance.spec.ts','--retries=0','--reporter=list,json'],{PLAYWRIGHT_JSON_OUTPUT_FILE:`${evidence}/${id}.json`},cwd);}
  finally{try{cpSync(cwd+'/test-results',`${evidence}/${id}-results`,{recursive:true});}catch{}}
}
const files={};
function walk(directory,prefix=''){
  for(const name of readdirSync(directory)){
    const file=directory+'/'+name,relative=prefix+name;
    if(statSync(file).isDirectory())walk(file,relative+'/');
    else files[relative]={sha256:digest(file),bytes:statSync(file).size};
  }
}
walk('iris-git');result.files=files;
result.releaseProfileSha256=digest('scripts/release-profiles.mjs');
await run('archive-dist',['tar','-czf',`${evidence}/qualified-dist.tar.gz`,'-C','iris-git','.']);
result.passed=true;save();console.log('Exact source passed all pre-publication gates');
