/**
 * Test that window.nostr is available in the main Tauri window
 */
import { browser } from '@wdio/globals';
import * as fs from 'fs';
import * as path from 'path';

const SCREENSHOTS_DIR = path.resolve(process.cwd(), 'e2e-tauri/screenshots');

// Ensure screenshots directory exists
if (!fs.existsSync(SCREENSHOTS_DIR)) {
  fs.mkdirSync(SCREENSHOTS_DIR, { recursive: true });
}

async function takeScreenshot(name: string): Promise<void> {
  const screenshot = await browser.takeScreenshot();
  const filepath = path.join(SCREENSHOTS_DIR, `${name}-${Date.now()}.png`);
  fs.writeFileSync(filepath, screenshot, 'base64');
  console.log(`Screenshot saved: ${filepath}`);
}

describe('NIP-07 Main Window', () => {
  it('should have window.nostr available', async () => {
    console.log('[NIP-07 Test] Starting...');

    // Wait for body to exist
    const body = await browser.$('body');
    await body.waitForExist({ timeout: 30000 });
    console.log('[NIP-07 Test] Body exists');

    // Wait for app content to render
    await browser.pause(5000);
    await takeScreenshot('nip07-01-loaded');

    // Check if window.nostr exists
    const hasNostr = await browser.execute(() => {
      return typeof (window as any).nostr !== 'undefined';
    });
    console.log(`[NIP-07 Test] window.nostr exists: ${hasNostr}`);

    // If nostr exists, check its methods
    if (hasNostr) {
      const nostrMethods = await browser.execute(() => {
        const nostr = (window as any).nostr;
        return {
          hasGetPublicKey: typeof nostr.getPublicKey === 'function',
          hasSignEvent: typeof nostr.signEvent === 'function',
          hasGetRelays: typeof nostr.getRelays === 'function',
          hasNip04: typeof nostr.nip04 === 'object',
          hasNip44: typeof nostr.nip44 === 'object',
        };
      });
      console.log('[NIP-07 Test] nostr methods:', JSON.stringify(nostrMethods, null, 2));
    }

    // Check for Tauri
    const isTauri = await browser.execute(() => {
      return '__TAURI_INTERNALS__' in window || '__TAURI__' in window;
    });
    console.log(`[NIP-07 Test] Is Tauri: ${isTauri}`);

    // Check console logs
    const logs = await browser.execute(() => {
      // Try to get any stored logs
      return {
        userAgent: navigator.userAgent,
        isTauri: '__TAURI_INTERNALS__' in window || '__TAURI__' in window,
      };
    });
    console.log('[NIP-07 Test] Browser info:', JSON.stringify(logs, null, 2));

    await takeScreenshot('nip07-02-final');

    // Assertions
    expect(isTauri).toBe(true);
    expect(hasNostr).toBe(true);
  });

  it('should be able to get public key via window.nostr', async () => {
    // Wait for app to be ready
    await browser.pause(2000);

    // First check what's available in __TAURI__
    const tauriInfo = await browser.execute(() => {
      const tauri = (window as any).__TAURI__;
      const internals = (window as any).__TAURI_INTERNALS__;
      return {
        hasTauri: !!tauri,
        hasInternals: !!internals,
        tauriKeys: tauri ? Object.keys(tauri) : [],
        internalsKeys: internals ? Object.keys(internals) : [],
        hasCore: !!(tauri?.core),
        hasCoreInvoke: !!(tauri?.core?.invoke),
        hasDirectInvoke: !!(tauri?.invoke),
        hasInternalsInvoke: !!(internals?.invoke),
      };
    });
    console.log('[NIP-07 Test] Tauri structure:', JSON.stringify(tauriInfo, null, 2));

    const result = await browser.execute(async () => {
      const nostr = (window as any).nostr;
      if (!nostr) {
        return { error: 'window.nostr not found' };
      }
      try {
        const pubkey = await nostr.getPublicKey();
        return { pubkey };
      } catch (e) {
        return { error: String(e) };
      }
    });

    console.log('[NIP-07 Test] getPublicKey result:', JSON.stringify(result, null, 2));

    // The getPublicKey should return a valid pubkey (64 char hex string)
    expect(result).toBeDefined();
    expect((result as any).pubkey).toBeDefined();
    expect((result as any).pubkey).toMatch(/^[a-f0-9]{64}$/);
  });
});
