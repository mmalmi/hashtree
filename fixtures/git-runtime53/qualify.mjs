import {spawnSync} from 'node:child_process';
import {openSync,closeSync,copyFileSync,writeFileSync,readFileSync} from 'node:fs';
import {createHash} from 'node:crypto';
const steps=[];
for (const [name,args] of [ ['lock',['install','--lockfile-only','--no-frozen-lockfile','--ignore-scripts']], ['verify',['run','verify:dependency-lock']] ]) {
 const fd=openSync(`/stage/evidence/${name}.log`,'w');
 const r=spawnSync('pnpm',args,{stdio:['ignore',fd,fd],env:process.env,timeout:300000});closeSync(fd);
 steps.push({name,status:r.status});
 writeFileSync('/stage/evidence/lock-generation.json',JSON.stringify({source:JSON.parse(readFileSync('/stage/manifest.json')).source,steps},null,2));
 if(r.status!==0){console.log(readFileSync(`/stage/evidence/${name}.log`,'utf8'));process.exit(1);}
}
copyFileSync('pnpm-lock.yaml','/stage/evidence/pnpm-lock.yaml');
console.log('Generated and verified immutable dependency lock');
