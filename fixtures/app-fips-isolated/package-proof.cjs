const fs=require('node:fs'),path=require('node:path'),crypto=require('node:crypto'),assert=require('node:assert/strict');
const {createRequire}=require('node:module');
function manifestPath(name,req){
  for(const dir of req.resolve.paths(name)||[]){const p=path.join(dir,name,'package.json');if(fs.existsSync(p))return fs.realpathSync(p);}
  throw Error('Missing installed manifest '+name);
}
const m=JSON.parse(fs.readFileSync('/stage/procedure/manifest.json'));
const spec=m.apps[0],req=createRequire(process.cwd()+'/package.json');
assert.equal(JSON.parse(fs.readFileSync(manifestPath('@playwright/test',req))).version,spec.playwright);
const packages=[],paths={};
for(const expected of spec.runtimePackages){
  // CC consumes WebSocket transport through its worker, not its app importer.
  const importer=spec.id==='cc' && expected.package==='@fips/transport-websocket'
    ? createRequire(manifestPath('@hashtree/worker',req)) : req;
  const p=manifestPath(expected.package,importer),raw=fs.readFileSync(p),actual=JSON.parse(raw);
  assert.deepEqual(actual,expected.manifest);paths[expected.package]=p;
  packages.push({name:actual.name,version:actual.version,manifestSha256:crypto.createHash('sha256').update(raw).digest('hex'),archiveSha256:expected.sha256});
}
for(const [name,p] of Object.entries(paths)){
  if(name==='@fips/core')continue;
  assert.equal(manifestPath('@fips/core',createRequire(p)),paths['@fips/core'],'One accepted core for every leaf');
}
if(spec.id==='cc'){
  const workerPath=manifestPath('@hashtree/worker',req),worker=JSON.parse(fs.readFileSync(workerPath));
  for(const name of Object.keys(paths)){
    if(worker.dependencies?.[name])assert.equal(manifestPath(name,createRequire(workerPath)),paths[name],'Worker uses the same accepted '+name);
  }
}
fs.writeFileSync(process.argv[2],JSON.stringify({passed:true,packages,oneResolvedCore:true,runtime:spec.acceptedRuntime},null,2)+'\n');
