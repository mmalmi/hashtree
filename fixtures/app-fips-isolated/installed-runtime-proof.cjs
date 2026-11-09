const fs=require('node:fs'),path=require('node:path'),crypto=require('node:crypto'),assert=require('node:assert/strict');
const {createRequire}=require('node:module');
const spec=JSON.parse(fs.readFileSync('/stage/procedure/manifest.json')).apps[0];
const root=process.cwd(),req=createRequire(path.join(root,'package.json'));
const sha=b=>crypto.createHash('sha256').update(b).digest('hex');
assert.equal(sha(fs.readFileSync('package.json')),spec.packageSha256);
assert.equal(sha(fs.readFileSync('pnpm-lock.yaml')),spec.lockSha256);
function manifest(name,from){
 for(const directory of from.resolve.paths(name)||[]){
  const file=path.join(directory,name,'package.json');
  if(fs.existsSync(file))return fs.realpathSync(file);
 }
 throw Error('Missing installed manifest: '+name);
}
const packages=[],entries={};
for(const expected of spec.installedRuntimeExpectations){
 const entry=manifest(expected.package,req),raw=fs.readFileSync(entry),m=JSON.parse(raw);
 assert.equal(m.name,expected.package);assert.equal(m.version,expected.version);
 if(expected.manifestSha256)assert.equal(sha(raw),expected.manifestSha256);
 entries[expected.package]=entry;
 packages.push({name:m.name,version:m.version,manifestSha256:sha(raw),tarball:expected.url,integrity:expected.integrity});
}
for(const name of ['@fips/transport-webrtc','@fips/transport-websocket'])
 assert.equal(manifest('@fips/core',createRequire(entries[name])),entries['@fips/core'],'All FIPS transports must resolve the accepted core');
const sharedPubsubOwners=['@hashtree/fips-transport','@hashtree/nostr-pubsub','@hashtree/worker'];
for(const name of sharedPubsubOwners){
 const owner=manifest(name,req);
 assert.equal(manifest('nostr-pubsub',createRequire(owner)),entries['nostr-pubsub'],'Worker and peer adapters must use the qualified receipt implementation');
 if(name==='@hashtree/fips-transport')assert.equal(manifest('@fips/core',createRequire(owner)),entries['@fips/core']);
}
const pubsub=JSON.parse(fs.readFileSync('/stage/procedure/manifest.json')).pubsubPublishedInput;
for(const [name,expected] of Object.entries(pubsub.members)){
 const file=path.join(path.dirname(entries['nostr-pubsub']),name.replace(/^package\//,'')),raw=fs.readFileSync(file);
 assert.equal(raw.length,expected.bytes);assert.equal(sha(raw),expected.sha256);
}
const installedLock=path.join(root,'node_modules/.pnpm/lock.yaml');assert(fs.existsSync(installedLock));
fs.writeFileSync(process.argv[2],JSON.stringify({passed:true,source:spec.commit,acceptedDependencyReceiptSha256:spec.acceptedDependencyRelease.receiptSha256,
 releaseSource:spec.acceptedDependencyRelease.source,packageSha256:spec.packageSha256,lockSha256:spec.lockSha256,installedLockSha256:sha(fs.readFileSync(installedLock)),packages,
 fipsCoreResolutionShared:true,pubsubResolutionShared:true,sharedPubsubOwners,pubsubInstalledMembersVerified:Object.keys(pubsub.members).length,pubsubArchiveSha256:pubsub.sha256,node:process.version},null,2)+'\n');
