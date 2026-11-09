const fs=require('node:fs'),path=require('node:path'),crypto=require('node:crypto'),assert=require('node:assert/strict');
const createRequire=require('node:module').createRequire;
// Read metadata without evaluating package entrypoints or requiring a CJS export.
function manifestPath(name,req){
 for(const directory of req.resolve.paths(name)||[]){
  const candidate=path.join(directory,name,'package.json');
  if(fs.existsSync(candidate))return fs.realpathSync(candidate);
 }
 throw Error('Installed package metadata unavailable: '+name);
}
function qualify(root,output){
 const req=createRequire(root+'/package.json');
 const spec=JSON.parse(fs.readFileSync('/stage/procedure/manifest.json')).apps.find(a=>a.id===process.env.FIPS_APP_ID);assert(spec);
 const expected={'@playwright/test':spec.playwright,'@hashtree/worker':'0.4.25','nostr-social-graph':'4.0.4','@fips/browser':'0.0.21','@fips/core':'0.0.56','@fips/transport-webrtc':'0.0.60','@fips/transport-websocket':'0.0.14'};
 const packages=[],resolved={};
 const own=JSON.parse(fs.readFileSync(root+'/package.json'));
 for(const [name,version] of Object.entries(expected)){
  if(name.startsWith('@fips/') && !(name in (own.dependencies||{})))continue;
  const entry=manifestPath(name,req),bytes=fs.readFileSync(entry),manifest=JSON.parse(bytes);assert.equal(manifest.name,name);assert.equal(manifest.version,version);
  resolved[name]=entry;packages.push({name,version,manifestSha256:crypto.createHash('sha256').update(bytes).digest('hex')});
 }
 const nostr=JSON.parse(fs.readFileSync(manifestPath('@hashtree/nostr',createRequire(resolved['@hashtree/worker']))));assert.equal(nostr.version,'0.2.7');
 fs.writeFileSync(output,JSON.stringify({passed:true,packages,workerNostrVersion:nostr.version},null,2)+'\n');
}
module.exports={manifestPath};
if(require.main===module)qualify(process.cwd(),process.argv[2]);
