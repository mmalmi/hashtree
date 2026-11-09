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
const transfer=spec.fileTransferRelease;
assert.equal(sha(fs.readFileSync(path.join(root,transfer.vendorPath))),transfer.archiveSha256);
const transferDirectory=path.dirname(entries['@fips/file-transfer']);
for(const member of transfer.members){
 assert(member.path.startsWith('package/')&&!member.path.split('/').includes('..'));
 const file=path.join(transferDirectory,member.path.slice(8));assert(fs.lstatSync(file).isFile());
 const raw=fs.readFileSync(file);assert.equal(raw.length,member.bytes);assert.equal(sha(raw),member.sha256);
}
assert.equal(manifest('@fips/tcp',createRequire(entries['@fips/file-transfer'])),manifest('@fips/tcp',req),'Shared SDK must resolve the root pinned TCP');
const installedLock=path.join(root,'node_modules/.pnpm/lock.yaml');assert(fs.existsSync(installedLock));
fs.writeFileSync(process.argv[2],JSON.stringify({passed:true,source:spec.commit,acceptedDependencyReceiptSha256:spec.acceptedDependencyRelease.receiptSha256,
 releaseSource:spec.acceptedDependencyRelease.source,packageSha256:spec.packageSha256,lockSha256:spec.lockSha256,installedLockSha256:sha(fs.readFileSync(installedLock)),packages,
 fipsCoreResolutionShared:true,fileTransferReleaseSource:transfer.source,fileTransferScopedReceiptSha256:transfer.rootScopedAcceptanceSha256,fileTransferArchiveSha256:transfer.archiveSha256,fileTransferMembersVerified:transfer.members.length,fileTransferTcpShared:true,node:process.version},null,2)+'\n');
