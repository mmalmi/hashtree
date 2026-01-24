/**
 * Test NIP-07 via htree:// protocol using a local htree-hosted page
 * Avoids external site issues like Cloudflare
 */
import { browser } from '@wdio/globals';
import * as fs from 'fs';
import * as path from 'path';

const SCREENSHOTS_DIR = path.resolve(process.cwd(), 'e2e-tauri/screenshots');

if (!fs.existsSync(SCREENSHOTS_DIR)) {
  fs.mkdirSync(SCREENSHOTS_DIR, { recursive: true });
}

async function takeScreenshot(name: string): Promise<void> {
  const screenshot = await browser.takeScreenshot();
  const filepath = path.join(SCREENSHOTS_DIR, `nip07-local-${name}-${Date.now()}.png`);
  fs.writeFileSync(filepath, screenshot, 'base64');
  console.log(`Screenshot saved: ${filepath}`);
}

describe('NIP-07 local htree test', () => {
  it('window.nostr.getPublicKey works via htree://nip07/ in child webview', async () => {
    const body = await browser.$('body');
    await body.waitForExist({ timeout: 30000 });
    await browser.pause(2000);

    // Navigate to an npub tree (public jumble app hosted on htree)
    const addressInput = await browser.$('input[placeholder="Search or enter address"]');
    await addressInput.waitForExist({ timeout: 30000 });

    // Use a known npub with public content
    const npubPath = 'npub1wj6a4ex6hsp7rq4g3h9fzqwezt9f0478vnku9wzzkl25w2uudnds4z3upt/public/jumble/dist/index.html';

    await addressInput.click();
    await addressInput.setValue(npubPath);
    await browser.keys(['Enter']);

    console.log('[NIP-07 Local] Navigating to htree content...');

    // Wait for navigation
    await browser.waitUntil(
      async () => {
        const hash = await browser.execute(() => window.location.hash);
        return hash.includes('npub1') || hash.includes('nhash');
      },
      { timeout: 30000, timeoutMsg: 'Failed to navigate to htree content' }
    );

    await browser.pause(5000);
    await takeScreenshot('01-htree-loaded');

    // Find the htree child webview
    const handles = await browser.getWindowHandles();
    console.log(`[NIP-07 Local] Found ${handles.length} window handles`);

    let htreeHandle: string | null = null;
    for (const handle of handles) {
      await browser.switchToWindow(handle);
      const url = await browser.getUrl();
      console.log(`[NIP-07 Local] Handle: ${url}`);
      if (url.startsWith('htree://')) {
        htreeHandle = handle;
        break;
      }
    }

    if (!htreeHandle) {
      console.log('[NIP-07 Local] No htree:// webview found, testing from main window child');
      // Try finding any child webview that's not the main tauri window
      for (const handle of handles) {
        await browser.switchToWindow(handle);
        const url = await browser.getUrl();
        if (!url.startsWith('tauri://')) {
          htreeHandle = handle;
          break;
        }
      }
    }

    await takeScreenshot('02-webview-found');

    // Test window.nostr from the webview
    const nostrCheck = await browser.execute(() => {
      const nostr = (window as any).nostr;
      return {
        hasNostr: !!nostr,
        hasGetPublicKey: typeof nostr?.getPublicKey === 'function',
        protocol: window.location.protocol,
        origin: window.location.origin,
      };
    });

    console.log('[NIP-07 Local] nostr check:', JSON.stringify(nostrCheck, null, 2));

    // Now test getPublicKey
    const result = await browser.execute(async () => {
      try {
        const nostr = (window as any).nostr;
        if (!nostr) return { error: 'no window.nostr' };

        const pubkey = await nostr.getPublicKey();
        return {
          success: true,
          pubkey,
          protocol: window.location.protocol,
        };
      } catch (e) {
        return {
          error: String(e),
          message: (e as Error).message,
          protocol: window.location.protocol,
        };
      }
    });

    console.log('[NIP-07 Local] getPublicKey result:', JSON.stringify(result, null, 2));
    await takeScreenshot('03-getPublicKey-result');

    expect(result).toBeDefined();
    expect((result as any).success).toBe(true);
    expect((result as any).pubkey).toMatch(/^[a-f0-9]{64}$/);
  });

  it('window.nostr.signEvent works via htree://nip07/', async () => {
    // Use existing webview from previous test
    const handles = await browser.getWindowHandles();

    // Find non-tauri webview
    for (const handle of handles) {
      await browser.switchToWindow(handle);
      const url = await browser.getUrl();
      if (!url.startsWith('tauri://')) {
        break;
      }
    }

    const result = await browser.execute(async () => {
      try {
        const nostr = (window as any).nostr;
        if (!nostr) return { error: 'no window.nostr' };

        // Create a test event
        const event = {
          kind: 1,
          content: 'test message',
          tags: [],
          created_at: Math.floor(Date.now() / 1000),
        };

        const signedEvent = await nostr.signEvent(event);
        return {
          success: true,
          hasId: !!signedEvent?.id,
          hasSig: !!signedEvent?.sig,
          hasPubkey: !!signedEvent?.pubkey,
        };
      } catch (e) {
        return { error: String(e), message: (e as Error).message };
      }
    });

    console.log('[NIP-07 Local] signEvent result:', JSON.stringify(result, null, 2));
    await takeScreenshot('04-signEvent-result');

    expect(result).toBeDefined();
    expect((result as any).success).toBe(true);
    expect((result as any).hasId).toBe(true);
    expect((result as any).hasSig).toBe(true);
  });
});
