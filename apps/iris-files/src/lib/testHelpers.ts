/**
 * Test helpers exposed on window for e2e tests
 * Shared between all app entry points
 */

// WebRTC test result interface
export interface WebRTCTestResult {
  pubkey: string | null;
  connectedPeers: number;
  contentHash?: string;
  contentRequestResult?: {
    found: boolean;
    data?: string;
  };
  error?: string;
}

// Extend window interface for test helpers
declare global {
  interface Window {
    __testHelpers?: { uploadSingleFile: unknown; followPubkey: unknown };
    __localStore?: unknown;
    __getWebRTCStore?: unknown;
    __getWorkerAdapter?: unknown;
    __getSocialGraph?: unknown;
    __socialGraph?: unknown;
    __settingsStore?: unknown;
    __setPoolSettings?: (pools: Record<string, unknown>) => void;
    __getMyPubkey?: () => string | null;
    __hashtree?: unknown;
    __getTreeRoot?: () => string | null;
    webrtcStore?: unknown;
    __consoleLogs?: string[];
    __testHelpersReady?: boolean;
    // WebRTC test functions
    runWebRTCTest?: (targetPubkey?: string, contentHash?: string) => Promise<WebRTCTestResult>;
    runWebRTCTestWithContent?: (content: string) => Promise<WebRTCTestResult>;
    testResults?: WebRTCTestResult;
  }
}

// Capture console logs for E2E testing
const consoleLogs: string[] = [];
const origLog = console.log;
const origError = console.error;
const origWarn = console.warn;

if (typeof window !== 'undefined') {
  console.log = (...args) => {
    consoleLogs.push('[log] ' + args.map(a => typeof a === 'object' ? JSON.stringify(a) : String(a)).join(' '));
    origLog.apply(console, args);
  };
  console.error = (...args) => {
    consoleLogs.push('[error] ' + args.map(a => typeof a === 'object' ? JSON.stringify(a) : String(a)).join(' '));
    origError.apply(console, args);
  };
  console.warn = (...args) => {
    consoleLogs.push('[warn] ' + args.map(a => typeof a === 'object' ? JSON.stringify(a) : String(a)).join(' '));
    origWarn.apply(console, args);
  };
  window.__consoleLogs = consoleLogs;
}

export async function setupTestHelpers(): Promise<void> {
  if (typeof window === 'undefined') return;

  const actionsPromise = Promise.all([
    import('../actions/index'),
    import('../stores/follows'),
  ]).then(([actions, followsStore]) => {
    window.__testHelpers = { uploadSingleFile: actions.uploadSingleFile, followPubkey: followsStore.followPubkey };
  });

  const storePromise = import('../store').then(({ webrtcStore, localStore, getWebRTCStore }) => {
    Object.defineProperty(window, 'webrtcStore', {
      get: () => webrtcStore,
      configurable: true,
    });
    window.__localStore = localStore;
    window.__getWebRTCStore = getWebRTCStore;
  });

  const workerPromise = import('./workerInit').then(({ getWorkerAdapter }) => {
    window.__getWorkerAdapter = getWorkerAdapter;
  });

  const socialGraphPromise = import('../utils/socialGraph').then(({ getSocialGraph }) => {
    window.__getSocialGraph = getSocialGraph;
    Object.defineProperty(window, '__socialGraph', {
      get: () => getSocialGraph(),
      configurable: true,
    });
  });

  const settingsPromise = import('../stores/settings').then(({ settingsStore }) => {
    window.__settingsStore = settingsStore;
    window.__setPoolSettings = (pools: Record<string, unknown>) => settingsStore.setPoolSettings(pools);
  });

  const nostrPromise = import('../nostr').then(({ useNostrStore }) => {
    window.__getMyPubkey = () => useNostrStore.getState().pubkey;
  });

  const hashtreePromise = import('hashtree').then((hashtree) => {
    window.__hashtree = hashtree;
  });

  const treeRootPromise = Promise.all([
    import('../stores'),
    import('svelte/store'),
    import('hashtree'),
  ]).then(([stores, svelteStore, hashtree]) => {
    window.__getTreeRoot = () => {
      const rootCid = svelteStore.get(stores.treeRootStore);
      return rootCid?.hash ? hashtree.toHex(rootCid.hash) : null;
    };
  });

  const criticalResults = await Promise.allSettled([
    actionsPromise,
    storePromise,
    workerPromise,
    socialGraphPromise,
    settingsPromise,
    nostrPromise,
  ]);
  const failedCritical = criticalResults.filter((result) => result.status === 'rejected');
  if (failedCritical.length > 0) {
    console.error('[testHelpers] critical init failed', failedCritical);
  }

  window.__testHelpersReady = true;

  void Promise.all([
    hashtreePromise,
    treeRootPromise,
    setupWebRTCTestFunctions(),
  ]).catch((err) => {
    console.error('[testHelpers] background init failed', err);
  });
}

/**
 * Setup WebRTC test functions for E2E testing
 * These allow tests to verify peer connections and data exchange
 */
async function setupWebRTCTestFunctions(): Promise<void> {
  const { getWorkerAdapter } = await import('./workerInit');
  const { localStore } = await import('../store');
  const { sha256, toHex, fromHex } = await import('hashtree');

  /**
   * Run WebRTC test - wait for peers and optionally request content
   */
  window.runWebRTCTest = async (targetPubkey?: string, contentHash?: string): Promise<WebRTCTestResult> => {
    const result: WebRTCTestResult = {
      pubkey: null,
      connectedPeers: 0,
    };

    try {
      const adapter = getWorkerAdapter();
      if (!adapter) {
        result.error = 'Worker adapter not initialized';
        return result;
      }

      // Get our pubkey
      result.pubkey = window.__getMyPubkey?.() || null;

      adapter.sendHello?.();

      // Wait for peer connections (up to 60 seconds)
      for (let i = 0; i < 120; i++) {
        await new Promise(r => setTimeout(r, 500));
        if (i % 10 === 0) {
          adapter.sendHello?.();
        }
        const stats = await adapter.getPeerStats();
        const connected = stats.filter(p => p.connected);
        result.connectedPeers = connected.length;

        console.log(`[WebRTC Test] Check ${i + 1}: ${result.connectedPeers} connected peers`);

        const targetConnected = targetPubkey
          ? connected.some(p => p.pubkey === targetPubkey)
          : result.connectedPeers > 0;

        if (targetConnected) {
          console.log('[WebRTC Test] CONNECTED to peers!');
          break;
        }
      }

      // If content hash provided, try to fetch it
      if (contentHash && result.connectedPeers > 0) {
        console.log(`[WebRTC Test] Requesting content: ${contentHash.slice(0, 16)}...`);

        try {
          const hashBytes = fromHex(contentHash);
          let data: Uint8Array | null = null;
          for (let attempt = 0; attempt < 3; attempt++) {
            data = await adapter.get(hashBytes);
            if (data) break;
            await new Promise(r => setTimeout(r, 1000));
          }
          if (data) {
            console.log('[WebRTC Test] GOT CONTENT!');
            result.contentRequestResult = {
              found: true,
              data: new TextDecoder().decode(data),
            };
          } else {
            result.contentRequestResult = { found: false };
          }
        } catch (e) {
          result.contentRequestResult = { found: false };
          console.log('[WebRTC Test] Content request failed:', e);
        }
      }

      window.testResults = result;
      return result;
    } catch (e) {
      result.error = String(e);
      return result;
    }
  };

  /**
   * Run WebRTC test with content - store content and wait for peers
   */
  window.runWebRTCTestWithContent = async (content: string): Promise<WebRTCTestResult> => {
    const result: WebRTCTestResult = {
      pubkey: null,
      connectedPeers: 0,
    };

    try {
      const adapter = getWorkerAdapter();
      if (!adapter) {
        result.error = 'Worker adapter not initialized';
        return result;
      }

      // Get our pubkey
      result.pubkey = window.__getMyPubkey?.() || null;

      // Store content and get hash
      const contentBytes = new TextEncoder().encode(content);
      const hash = await sha256(contentBytes);
      result.contentHash = toHex(hash);

      console.log(`[WebRTC Test] Stored content with hash: ${result.contentHash.slice(0, 16)}...`);

      // Store via worker
      await localStore.put(hash, contentBytes);

      // Update testResults immediately so peer 2 can read it
      window.testResults = { ...result };

      adapter.sendHello?.();

      // Wait for peer connections (up to 60 seconds)
      for (let i = 0; i < 120; i++) {
        await new Promise(r => setTimeout(r, 500));
        if (i % 10 === 0) {
          adapter.sendHello?.();
        }
        const stats = await adapter.getPeerStats();
        result.connectedPeers = stats.filter(p => p.connected).length;

        console.log(`[WebRTC Test] Check ${i + 1}: ${result.connectedPeers} connected peers`);

        if (result.connectedPeers > 0) {
          console.log('[WebRTC Test] CONNECTED to peers!');
          break;
        }
      }

      window.testResults = result;
      return result;
    } catch (e) {
      result.error = String(e);
      return result;
    }
  };

  console.log('[TestHelpers] WebRTC test functions initialized');
}
