/**
 * Tauri E2E test for WebRTC peer connections
 *
 * Tests that the WebRTC infrastructure is working in Tauri.
 * The app auto-generates a new key on first run, which initializes the worker.
 */
import { browser } from '@wdio/globals';

describe('WebRTC in Tauri', () => {
  before(async () => {
    // Measure time to first render
    const startTime = Date.now();

    for (let i = 0; i < 60; i++) {
      await browser.pause(200);

      const state = await browser.execute(() => {
        const bodyText = document.body?.innerText?.substring(0, 500) || '';
        const logs = (window as any).__consoleLogs?.slice(-10) || [];
        return { bodyText, logs, hasContent: bodyText.length > 10 };
      });

      const elapsed = Date.now() - startTime;
      if (state.hasContent) {
        console.log(`First content after ${elapsed}ms: "${state.bodyText.substring(0, 100)}..."`);
        break;
      }

      if (i % 5 === 0) {
        console.log(`${elapsed}ms - no content yet. Last logs:`, state.logs.slice(-3));
      }
    }
  });

  it('should have worker adapter initialized (auto-login)', async () => {
    // Wait for the worker adapter to be initialized
    // The app auto-generates a key on first run, which triggers initHashtreeWorker
    let adapter = null;
    for (let i = 0; i < 30; i++) {
      await browser.pause(1000);

      const state = await browser.execute(() => {
        const getWorkerAdapter = (window as any).__getWorkerAdapter;
        const adapterVal = getWorkerAdapter ? getWorkerAdapter() : null;

        // Check what's in nostrStore
        const nostrStore = (window as any).__nostrStore;
        const pubkey = nostrStore?.getState?.()?.pubkey || null;

        // Get body content preview for debugging
        const bodyText = document.body?.innerText?.substring(0, 200) || '';

        // Get captured console logs
        const logs = (window as any).__consoleLogs?.slice(-20) || [];

        return {
          hasGetWorkerAdapter: typeof getWorkerAdapter === 'function',
          hasAdapter: !!adapterVal,
          pubkey,
          bodyText,
          logs,
          localStorage: {
            loginType: localStorage.getItem('hashtree:loginType'),
            hasNsec: !!localStorage.getItem('hashtree:nsec'),
          }
        };
      });

      console.log(`Check ${i + 1}:`, JSON.stringify({ ...state, logs: state.logs?.length || 0 }));
      if (state.logs?.length) {
        console.log('Recent logs:', state.logs.slice(-5).join('\n'));
      }

      if (state.hasAdapter) {
        adapter = true;
        console.log(`Worker adapter initialized after ${i + 1}s`);
        break;
      }
    }

    expect(adapter).not.toBeNull();
  });

  it('should have getPeerStats command working', async () => {
    const stats = await browser.execute(async () => {
      const getWorkerAdapter = (window as any).__getWorkerAdapter;
      const adapter = getWorkerAdapter();
      if (!adapter) throw new Error('Adapter not initialized');

      const peerStats = await adapter.getPeerStats();
      return peerStats;
    });

    console.log('Peer stats:', JSON.stringify(stats));
    expect(Array.isArray(stats)).toBe(true);
  });

  it('should be able to set WebRTC pool configuration', async () => {
    const result = await browser.execute(async () => {
      const getWorkerAdapter = (window as any).__getWorkerAdapter;
      const adapter = getWorkerAdapter();
      if (!adapter) return { success: false, error: 'Adapter not initialized' };

      try {
        await adapter.setWebRTCPools({
          follows: { max: 15, satisfied: 8 },
          other: { max: 5, satisfied: 2 },
        });
        return { success: true };
      } catch (e) {
        return { success: false, error: String(e) };
      }
    });

    console.log('setWebRTCPools result:', result);
    expect(result.success).toBe(true);
  });

  it('should be able to send hello message', async () => {
    const result = await browser.execute(async () => {
      const getWorkerAdapter = (window as any).__getWorkerAdapter;
      const adapter = getWorkerAdapter();
      if (!adapter) return { success: false, error: 'Adapter not initialized' };

      try {
        await adapter.sendHello();
        return { success: true };
      } catch (e) {
        return { success: false, error: String(e) };
      }
    });

    console.log('sendHello result:', result);
    expect(result.success).toBe(true);
  });

  it('should get relay stats', async () => {
    // Wait a bit for relays to potentially connect
    await browser.pause(3000);

    const result = await browser.execute(async () => {
      const getWorkerAdapter = (window as any).__getWorkerAdapter;
      const adapter = getWorkerAdapter();
      if (!adapter) return { success: false, error: 'Adapter not initialized' };

      try {
        console.log('[Test] Calling getRelayStats...');
        const stats = await adapter.getRelayStats();
        console.log('[Test] getRelayStats returned:', JSON.stringify(stats));
        return { success: true, stats };
      } catch (e) {
        console.log('[Test] getRelayStats error:', String(e));
        return { success: false, error: String(e) };
      }
    });

    console.log('getRelayStats result:', JSON.stringify(result));
    expect(result.success).toBe(true);
    expect(Array.isArray(result.stats)).toBe(true);
  });
});
