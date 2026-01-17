import 'virtual:uno.css';
import IrisApp from './IrisApp.svelte';
import { mount } from 'svelte';
import { initServiceWorker } from './lib/swInit';
import { restoreSession, initReadonlyWorker } from './nostr/auth';
import { setAppType } from './appType';
import { initHtreeApi } from './lib/htreeApi';
import { installHtreeDebugCapture } from './lib/htreeDebug';

setAppType('iris');
installHtreeDebugCapture();

const startTime = performance.now();
function logTiming(label: string) {
  console.log(`[Startup] ${label}: ${Math.round(performance.now() - startTime)}ms`);
}

// Mount app immediately for fast first paint
mount(IrisApp, {
  target: document.getElementById('app')!,
});
logTiming('mount() done - app rendered');

// Initialize in background after first paint
async function init() {
  logTiming('init() started');

  // Initialize service worker for PWA/caching (fast in Tauri)
  const swPromise = initServiceWorker();

  // Restore session and initialize worker
  const htreePromise = initHtreeApi();
  const workerPromise = initReadonlyWorker();
  const sessionPromise = restoreSession();
  await swPromise;
  logTiming('initServiceWorker() done');
  await Promise.all([workerPromise, sessionPromise]);
  logTiming('restoreSession() done');

  // Initialize window.htree API for guest apps
  await htreePromise;
  logTiming('initHtreeApi() done');
}

init();
if (import.meta.env.DEV && import.meta.env.VITE_TEST_MODE) {
  void import('./lib/testHelpers').then(({ setupTestHelpers }) => setupTestHelpers());
}
