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
  const p=manifestPath(expected.package,req),raw=fs.readFileSync(p),actual=JSON.parse(raw);
  assert.deepEqual(actual,expected.manifest);paths[expected.package]=p;
  packages.push({name:actual.name,version:actual.version,manifestSha256:crypto.createHash('sha256').update(raw).digest('hex'),archiveSha256:expected.sha256});
}
for(const [name,p] of Object.entries(paths)){
  if(name==='@fips/core')continue;
  assert.equal(manifestPath('@fips/core',createRequire(p)),paths['@fips/core'],'One accepted core for every leaf');
}
fs.writeFileSync(process.argv[2],JSON.stringify({passed:true,packages,oneResolvedCore:true,runtime:spec.acceptedRuntime},null,2)+'\n');
