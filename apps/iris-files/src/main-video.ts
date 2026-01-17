import 'virtual:uno.css';
import VideoApp from './VideoApp.svelte';
import { mount } from 'svelte';
import { initServiceWorker } from './lib/swInit';
import { restoreSession, initReadonlyWorker } from './nostr/auth';
import { mergeBootstrapIndex } from './stores/searchIndex';
import { setAppType } from './appType';
import { initHtreeApi } from './lib/htreeApi';
import { installHtreeDebugCapture } from './lib/htreeDebug';

setAppType('video');
installHtreeDebugCapture();

async function init() {
  const swPromise = initServiceWorker({ requireCrossOriginIsolation: true });
  const htreePromise = initHtreeApi();
  const workerPromise = initReadonlyWorker();
  const sessionPromise = restoreSession();
  await swPromise;
  mount(VideoApp, {
    target: document.getElementById('app')!,
  });
  await Promise.all([workerPromise, sessionPromise]);
  await htreePromise;
  mergeBootstrapIndex().catch(() => {});
}

init();
if (import.meta.env.DEV && import.meta.env.VITE_TEST_MODE) {
  void import('./lib/testHelpers').then(({ setupTestHelpers }) => setupTestHelpers());
}
