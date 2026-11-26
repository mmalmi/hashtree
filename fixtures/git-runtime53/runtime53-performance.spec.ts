import { test, expect } from './fixtures';
import { gotoGitApp, waitForAppReady, flushPendingPublishes } from './test-utils';
import { ensureGitSession } from './git-history.helpers';
import { mkdtempSync, writeFileSync, mkdirSync, readdirSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { execFileSync } from 'node:child_process';

// Identical packed history and local relay, one worker and fresh browser storage per build.
test('matched packed repository rows, history, interaction and idle workload', async ({ page }, info) => {
  test.setTimeout(240000);
  const dir=mkdtempSync(join(tmpdir(),'iris-git-runtime53-perf-'));
  const git=(...args:string[])=>execFileSync('git',args,{cwd:dir,encoding:'utf8',env:{...process.env,GIT_AUTHOR_DATE:'2026-01-01T00:00:00Z',GIT_COMMITTER_DATE:'2026-01-01T00:00:00Z'}}).trim();
  const errors:string[]=[];page.on('pageerror',e=>errors.push(e.message));
  try {
    git('init','-b','main');git('config','user.name','Fixture');git('config','user.email','fixture@example.test');
    writeFileSync(join(dir,'README.md'),'# Matched packed repository\n');
    for(let i=0;i<24;i++)writeFileSync(join(dir,`file-${i}.ts`),`export const value = ${i};\n`);
    for(let d=0;d<4;d++){mkdirSync(join(dir,`dir-${d}`));for(let i=0;i<16;i++)writeFileSync(join(dir,`dir-${d}/file-${i}.ts`),'export const nested = 1;\n');}
    git('add','.');git('commit','-m','Initial fixture');
    for(let i=0;i<120;i++){writeFileSync(join(dir,`file-${i%24}.ts`),`export const value = ${100+i};\n`);writeFileSync(join(dir,`dir-${i%4}/file-${i%16}.ts`),`export const nested = ${i};\n`);git('add','.');git('commit','-m',`Change ${i}`);}
    git('gc','--prune=now');
    const head=git('rev-parse','HEAD');
    const names=['README.md',...Array.from({length:24},(_,i)=>`file-${i}.ts`),...Array.from({length:4},(_,i)=>`dir-${i}`)];
    const expected=Object.fromEntries(names.map(name=>[name,git('log','-1','--format=%H','HEAD','--',name)]));
    const files:{path:string,content:number[]}[]=[];const dirs:string[]=[];
    const walk=(base='')=>{for(const e of readdirSync(join(dir,base),{withFileTypes:true})){const p=base?`${base}/${e.name}`:e.name;if(p.startsWith('.git/hooks')||p.startsWith('.git/logs'))continue;if(e.isDirectory()){dirs.push(p);walk(p);}else files.push({path:p,content:Array.from(readFileSync(join(dir,p)))});}};walk();
    await gotoGitApp(page);await ensureGitSession(page);
    const owner=await page.evaluate(async ({files,dirs})=>{
      const {getTree,LinkType}=await import('/src/store.ts');const {saveHashtree,nostrStore}=await import('/src/nostr/index.ts');const tree=getTree();let {cid}=await tree.putDirectory([]);
      for(const dir of dirs.sort((a,b)=>a.split('/').length-b.split('/').length)){const parts=dir.split('/');const name=parts.pop()!;const empty=await tree.putDirectory([]);cid=await tree.setEntry(cid,parts,name,empty.cid,0,LinkType.Dir);}
      for(const file of files){const parts=file.path.split('/');const name=parts.pop()!;const saved=await tree.putFile(new Uint8Array(file.content));cid=await tree.setEntry(cid,parts,name,saved.cid,saved.size,LinkType.Blob);}
      await saveHashtree('matched-perf',cid,{visibility:'public',labels:['git']});return nostrStore.getState().npub;
    },{files,dirs});
    await flushPendingPublishes(page);
    await page.addInitScript(()=>{
      const state=(window as any).__matched={list:null,first:null,all:null,longTasks:[]};
      new PerformanceObserver(list=>state.longTasks.push(...list.getEntries().map(e=>e.duration))).observe({type:'longtask',buffered:true});
      new MutationObserver(()=>{const rows=[...document.querySelectorAll('[data-testid="file-list"] tbody tr')].filter((r:any)=>r.cells[1]?.querySelector('a')&&r.cells[1].innerText!=='.git');if(rows.length!==29)return;state.list??=performance.now();const labeled=rows.filter((r:any)=>r.cells[2]?.querySelector('a'));if(labeled.length)state.first??=performance.now();if(labeled.length===29)state.all??=performance.now();}).observe(document,{subtree:true,childList:true,attributes:true});
    });
    const cdp=await page.context().newCDPSession(page);await cdp.send('Performance.enable');const samples=[];
    for(let pass=0;pass<3;pass++){
      await page.goto(`/git.html#/${owner}/matched-perf`,{waitUntil:'domcontentloaded'});
      await page.waitForFunction(()=>(window as any).__matched?.all!==null,undefined,{timeout:60000});
      const actual=await page.getByTestId('file-list').locator('tbody tr').evaluateAll(rs=>Object.fromEntries(rs.filter((r:any)=>r.cells[2]?.querySelector('a')).map((r:any)=>[r.cells[1].innerText,r.cells[2].querySelector('a').hash.match(/commit=([0-9a-f]+)/)[1]])));
      expect(actual).toEqual(expected);
      const historyStarted=Date.now();await page.getByRole('button',{name:'121 commits',exact:true}).click();
      const modal=page.getByTestId('git-history-modal');await expect(modal.locator('a.font-mono[href*="commit="]')).toHaveCount(50);const historyMs=Date.now()-historyStarted;await modal.getByLabel('Close',{exact:true}).click();
      const before=Object.fromEntries((await cdp.send('Performance.getMetrics')).metrics.map(m=>[m.name,m.value]));
      await page.waitForTimeout(5000); // Fixed measured idle window, not a readiness signal.
      const after=Object.fromEntries((await cdp.send('Performance.getMetrics')).metrics.map(m=>[m.name,m.value]));
      const interaction=await page.evaluate(async()=>{const samples=[];for(let i=0;i<12;i++){const start=performance.now();await new Promise(requestAnimationFrame);samples.push(performance.now()-start);}return samples;});
      const browser=await page.evaluate(async()=>({timings:(window as any).__matched,storage:await navigator.storage.estimate(),transferBytes:performance.getEntriesByType('resource').reduce((sum,e:any)=>sum+e.transferSize,0)}));
      samples.push({pass,head,rows:29,wrong:0,historyMs,mainThreadIdleCpuSeconds:after.TaskDuration-before.TaskDuration,mainThreadHeapBytes:after.JSHeapUsedSize,interactionMs:interaction,...browser});
    }
    expect(errors).toEqual([]);writeFileSync(info.outputPath('matched-performance.json'),JSON.stringify({head,commits:121,files:89,fixtureGitBytes:files.reduce((n,f)=>n+f.content.length,0),samples},null,2));
    await page.screenshot({path:info.outputPath('matched-performance.png')});
  }finally{rmSync(dir,{recursive:true,force:true});}
});
