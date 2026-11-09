import {createRequire} from 'node:module';
import {appendFileSync} from 'node:fs';
import {spawnSync} from 'node:child_process';
import {createDenyProxy, assertWorkerNetworkBlocked} from './standalone-browser-boundary.ts';
import {grantIsolatedPermission} from './permission-context.ts';
const require=createRequire(`${process.cwd()}/package.json`);
// Import the browser library without installing Playwright's global matchers in Vitest.
const playwrightRequire=createRequire(require.resolve('@playwright/test'));
const {chromium}=playwrightRequire('playwright');
const launch=chromium.launch.bind(chromium);
chromium.launch=async(options={})=>{
  const proof=process.env.BROWSER_BOUNDARY_RECEIPT;
  if(!proof)throw Error('Missing boundary evidence path');
  const check=spawnSync('python3',['/stage/procedure/network-proof.py',`${proof}.network-${process.pid}.json`],{encoding:'utf8',timeout:15000});
  if(check.status!==0)throw Error('Independent namespace denial failed: '+check.stderr);
  if(options.proxy)throw Error('Unexpected caller proxy; retain and review its existing control separately');
  const boundary=await createDenyProxy();let browser;
  try{
    browser=await launch({...options,args:[...(options.args??[]),'--disable-features=WebRtcHideLocalIpsWithMdns','--allow-loopback-in-peer-connection','--host-resolver-rules=MAP *.localhost 127.0.0.1'],proxy:{server:boundary.url,bypass:'127.0.0.1,localhost,*.localhost,[::1]'}});
    const context=await browser.newContext({serviceWorkers:'block'});
    await context.addInitScript('globalThis.__name=(fn)=>fn;');
    const page=await context.newPage();
    const rejected=await assertWorkerNetworkBlocked(page,boundary);
    await context.close();
    appendFileSync(proof,JSON.stringify({passed:true,rawWorkerRejected:rejected,appNavigationNotStarted:true,pid:process.pid})+'\n');
    const newContext=browser.newContext.bind(browser);
    browser.newContext=async(...args)=>{const c=await newContext(...args);await grantIsolatedPermission(c);return c;};
    const close=browser.close.bind(browser);
    browser.close=async(...args)=>{try{return await close(...args);}finally{await boundary.close();}};
    return browser;
  }catch(error){await browser?.close();await boundary.close();throw error;}
};
