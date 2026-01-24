/**
 * Test htree://nip07/ protocol for NIP-07 extension support in child webviews
 *
 * This tests that external HTTPS sites can use window.nostr via the htree:// protocol
 * instead of HTTP, avoiding mixed content issues.
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
  const filepath = path.join(SCREENSHOTS_DIR, `nip07-protocol-${name}-${Date.now()}.png`);
  fs.writeFileSync(filepath, screenshot, 'base64');
  console.log(`Screenshot saved: ${filepath}`);
}

describe('htree://nip07/ protocol', () => {
  it('main window uses Tauri invoke for NIP-07 (not htree:// protocol)', async () => {
    // Main window (tauri://localhost) uses Tauri invoke for NIP-07, not htree:// protocol.
    // This test verifies window.nostr works in the main window via invoke.
    const body = await browser.$('body');
    await body.waitForExist({ timeout: 30000 });
    await browser.pause(3000);

    // Main window should have window.nostr that uses invoke
    const result = await browser.execute(async () => {
      const nostr = (window as any).nostr;
      if (!nostr) return { error: 'no window.nostr' };

      try {
        const pubkey = await nostr.getPublicKey();
        return { success: true, pubkey, protocol: window.location.protocol };
      } catch (e) {
        return { error: String(e), message: (e as Error).message };
      }
    });

    console.log('[NIP-07 Protocol] Main window getPublicKey:', JSON.stringify(result, null, 2));
    await takeScreenshot('01-main-window-getPublicKey');

    // Should return a valid pubkey (64 char hex)
    expect(result).toBeDefined();
    expect((result as any).success).toBe(true);
    expect((result as any).pubkey).toMatch(/^[a-f0-9]{64}$/);
  });

  it('should work from HTTPS child webview (jumble.social)', async () => {
    // Navigate to jumble.social via address bar
    const addressInput = await browser.$('input[placeholder="Search or enter address"]');
    await addressInput.waitForExist({ timeout: 30000 });

    await addressInput.click();
    await addressInput.setValue('https://jumble.social/');
    await browser.keys(['Enter']);

    // Wait for navigation
    await browser.waitUntil(
      async () => {
        const hash = await browser.execute(() => window.location.hash);
        return hash.includes('jumble.social') || hash.includes('/app/');
      },
      { timeout: 30000 }
    );

    await browser.pause(5000);
    await takeScreenshot('02-jumble-loaded');

    // Switch to jumble webview
    const handles = await browser.getWindowHandles();
    let jumbleHandle: string | null = null;

    for (const handle of handles) {
      await browser.switchToWindow(handle);
      const url = await browser.getUrl();
      if (url.includes('jumble.social')) {
        jumbleHandle = handle;
        break;
      }
    }

    if (!jumbleHandle) {
      console.log('[NIP-07 Protocol] No jumble webview found, skipping HTTPS test');
      return;
    }

    await takeScreenshot('03-in-jumble-webview');

    // Test that htree://nip07/ works from the HTTPS context
    const result = await browser.execute(async () => {
      try {
        // This should work even though we're on HTTPS because htree:// is a custom protocol
        const response = await fetch('htree://nip07/getPublicKey', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({
            method: 'getPublicKey',
            params: {},
            origin: window.location.origin
          })
        });

        if (!response.ok) {
          return { error: `HTTP ${response.status}`, body: await response.text() };
        }

        const data = await response.json();
        return { success: true, data, protocol: window.location.protocol };
      } catch (e) {
        return {
          error: String(e),
          message: (e as Error).message,
          protocol: window.location.protocol
        };
      }
    });

    console.log('[NIP-07 Protocol] HTTPS webview result:', JSON.stringify(result, null, 2));
    await takeScreenshot('04-https-getPublicKey-result');

    // Verify we're on HTTPS and the call succeeded
    expect((result as any).protocol).toBe('https:');
    expect((result as any).success).toBe(true);
    expect((result as any).data?.result).toMatch(/^[a-f0-9]{64}$/);
  });
});
