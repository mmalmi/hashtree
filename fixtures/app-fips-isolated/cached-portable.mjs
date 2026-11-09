import {createServer} from 'node:http';
import {readFile,writeFile,readdir} from 'node:fs/promises';
import {createHash} from 'node:crypto';
import {spawn} from 'node:child_process';
import {once} from 'node:events';
const cache='/stage/demo-fixture';
const spec=JSON.parse(await readFile('/stage/procedure/manifest.json','utf8')).demoFixture;
const allowed=new Set((await readdir(cache)).filter(x=>/^[a-f0-9]{64}$/.test(x)));
if(allowed.size!==37||allowed.size!==spec.files.length)throw Error('Unexpected public fixture set');
for(const item of spec.files){const bytes=await readFile(`${cache}/${item.sha256}`);if(bytes.length!==item.bytes||createHash('sha256').update(bytes).digest('hex')!==item.sha256)throw Error('Public fixture hash changed');}
const requests=[];
const server=createServer(async(req,res)=>{const match=/^\/blocks\/([a-f0-9]{64})$/.exec(req.url??'');if(req.method!=='GET'||!match||!allowed.has(match[1])){res.writeHead(404).end();return;}requests.push(match[1]);const bytes=await readFile(`${cache}/${match[1]}`);res.writeHead(200,{'content-type':'application/octet-stream','access-control-allow-origin':'*','content-length':bytes.length}).end(bytes);}).listen(0,'127.0.0.1');await once(server,'listening');
let result,signal;
try{const child=spawn(process.execPath,['scripts/portable-smoke.mjs'],{stdio:'inherit',env:{...process.env,MAP_FIXTURE_CACHE_URL:`http://127.0.0.1:${server.address().port}`}});[result,signal]=await once(child,'exit');}
finally{await new Promise(resolve=>server.close(resolve));await writeFile(`${process.env.FIPS_APP_EVIDENCE}/cached-portable-reads.json`,JSON.stringify({exitCode:result,signal,requests,uniqueBlocks:new Set(requests).size,noForwarding:true},null,2)+'\n');}
process.exitCode=result===0&&signal===null&&requests.length>0?0:1;
