import {spawn} from 'node:child_process';
import {openSync,closeSync,writeFileSync,mkdirSync,cpSync} from 'node:fs';
const results=[];
async function run(name,cwd,args){
 const fd=openSync(`/stage/evidence/${name}.log`,'w');
 const child=spawn(args[0],args.slice(1),{cwd,stdio:['ignore',fd,fd],env:process.env,detached:true});
 const timer=setTimeout(()=>process.kill(-child.pid,'SIGTERM'),240000);
 const status=await new Promise(r=>child.on('exit',(code,signal)=>r(code??signal)));
 clearTimeout(timer);closeSync(fd);results.push({name,args,status});
 writeFileSync('/stage/evidence/diagnosis.json',JSON.stringify(results,null,2));
 console.log(name,status);return status;
}
for(const name of ['app']){
 const cwd='/stage/'+name;
 if(await run(name+'-install',cwd,['pnpm','install','--frozen-lockfile'])!==0)process.exit(1);
 await run(name+'-issue-owner',cwd,['pnpm','exec','playwright','test','e2e/git-collaboration-privacy.spec.ts','-g','repository owner can close','--retries=0','--max-failures=1']);
 try{cpSync(cwd+'/test-results','/stage/evidence/'+name+'-test-results',{recursive:true});}catch{}
}
