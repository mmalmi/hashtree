const fs=require('node:fs'),path=require('node:path'),crypto=require('node:crypto'),assert=require('node:assert/strict');
const req=require('node:module').createRequire(process.cwd()+'/package.json');
const expected={'@playwright/test':'1.60.0','@hashtree/worker':'0.4.25','nostr-social-graph':'4.0.4','@fips/browser':'0.0.19','@fips/core':'0.0.54','@fips/transport-webrtc':'0.0.58','@fips/transport-websocket':'0.0.12'};
const packages=[],resolved={};
for(const [name,version] of Object.entries(expected)){
 let entry;try{entry=req.resolve(name+'/package.json');}catch{
  let dir=path.dirname(req.resolve(name));while(!fs.existsSync(path.join(dir,'package.json'))){const parent=path.dirname(dir);assert.notEqual(parent,dir);dir=parent;}entry=path.join(dir,'package.json');
 }
 const bytes=fs.readFileSync(entry),manifest=JSON.parse(bytes);assert.equal(manifest.name,name);assert.equal(manifest.version,version);
 resolved[name]=entry;
 packages.push({name,version,manifestSha256:crypto.createHash('sha256').update(bytes).digest('hex')});
}
const workerReq=require('node:module').createRequire(resolved['@hashtree/worker']);
let nostrPath;try{nostrPath=workerReq.resolve('@hashtree/nostr/package.json');}catch{let dir=path.dirname(workerReq.resolve('@hashtree/nostr'));while(!fs.existsSync(path.join(dir,'package.json'))){const parent=path.dirname(dir);assert.notEqual(parent,dir);dir=parent;}nostrPath=path.join(dir,'package.json');}
const nostr=JSON.parse(fs.readFileSync(nostrPath));assert.equal(nostr.version,'0.2.7');
fs.writeFileSync(process.argv[2],JSON.stringify({passed:true,packages,workerNostrVersion:nostr.version},null,2)+'\n');
