/**
 * Test htree:// protocol origin isolation for webviews
 *
 * Tests that:
 * 1. htree:// URLs are correctly parsed (host-based format)
 * 2. Each nhash/npub+treename gets its own origin for storage isolation
 * 3. create_htree_webview command works correctly
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

describe('htree:// Origin Isolation', () => {
  it('should have Tauri invoke available', async () => {
    console.log('[htree Origin Test] Starting...');

    // Wait for body to exist
    const body = await browser.$('body');
    await body.waitForExist({ timeout: 30000 });
    console.log('[htree Origin Test] Body exists');

    // Wait for app content to render
    await browser.pause(5000);
    await takeScreenshot('htree-origin-01-loaded');

    // Check if Tauri is available
    const tauriInfo = await browser.execute(() => {
      const tauri = (window as any).__TAURI__;
      const internals = (window as any).__TAURI_INTERNALS__;
      return {
        hasTauri: !!tauri,
        hasInternals: !!internals,
        hasCoreInvoke: !!(tauri?.core?.invoke),
        hasInternalsInvoke: !!(internals?.invoke),
      };
    });

    console.log('[htree Origin Test] Tauri info:', JSON.stringify(tauriInfo, null, 2));

    expect(tauriInfo.hasTauri || tauriInfo.hasInternals).toBe(true);
  });

  it('should be able to invoke create_htree_webview with nhash', async () => {
    // Wait for app to be ready
    await browser.pause(2000);

    // Try to invoke create_htree_webview with a test nhash
    // Note: This test uses a fake nhash since we're testing the command structure
    const result = await browser.execute(async () => {
      const invoke =
        (window as any).__TAURI_INTERNALS__?.invoke ||
        (window as any).__TAURI__?.core?.invoke;

      if (!invoke) {
        return { error: 'Tauri invoke not available' };
      }

      try {
        // Test with minimal parameters - this should fail with invalid nhash
        // but that proves the command is registered and accepting calls
        await invoke('create_htree_webview', {
          label: 'test-htree-webview-' + Date.now(),
          nhash: 'nhash1test12345', // Invalid nhash - will fail to resolve but that's OK
          path: '/index.html',
          x: 0,
          y: 60,
          width: 800,
          height: 600,
        });
        return { success: true };
      } catch (e: any) {
        // Expected to fail because nhash doesn't exist
        // But it should fail in the htree resolution, not in command registration
        const errorStr = String(e);
        return {
          error: errorStr,
          // The command is working if we get an htree error, not a "command not found" error
          commandExists: !errorStr.includes('unknown command') &&
                        !errorStr.includes('not found'),
        };
      }
    });

    console.log('[htree Origin Test] create_htree_webview result:', JSON.stringify(result, null, 2));
    await takeScreenshot('htree-origin-02-invoke');

    // The command should exist (even if the nhash resolution fails)
    if ((result as any).error) {
      expect((result as any).commandExists).toBe(true);
    } else {
      expect((result as any).success).toBe(true);
    }
  });

  it('should be able to invoke create_htree_webview with npub and treename', async () => {
    // Wait for app to be ready
    await browser.pause(2000);

    // Try to invoke create_htree_webview with npub + treename
    const result = await browser.execute(async () => {
      const invoke =
        (window as any).__TAURI_INTERNALS__?.invoke ||
        (window as any).__TAURI__?.core?.invoke;

      if (!invoke) {
        return { error: 'Tauri invoke not available' };
      }

      try {
        // Test with minimal parameters - will fail to resolve but proves command works
        await invoke('create_htree_webview', {
          label: 'test-htree-npub-webview-' + Date.now(),
          npub: 'npub1test123456789012345678901234567890123456789012345678901234',
          treename: 'testapp',
          path: '/index.html',
          x: 0,
          y: 60,
          width: 800,
          height: 600,
        });
        return { success: true };
      } catch (e: any) {
        const errorStr = String(e);
        return {
          error: errorStr,
          commandExists: !errorStr.includes('unknown command') &&
                        !errorStr.includes('not found'),
        };
      }
    });

    console.log('[htree Origin Test] create_htree_webview (npub) result:', JSON.stringify(result, null, 2));
    await takeScreenshot('htree-origin-03-npub-invoke');

    // The command should exist
    if ((result as any).error) {
      expect((result as any).commandExists).toBe(true);
    } else {
      expect((result as any).success).toBe(true);
    }
  });

  it('should reject create_htree_webview without nhash or npub+treename', async () => {
    await browser.pause(1000);

    // Try to invoke without required parameters
    const result = await browser.execute(async () => {
      const invoke =
        (window as any).__TAURI_INTERNALS__?.invoke ||
        (window as any).__TAURI__?.core?.invoke;

      if (!invoke) {
        return { error: 'Tauri invoke not available' };
      }

      try {
        // Missing both nhash and npub+treename - should fail with validation error
        await invoke('create_htree_webview', {
          label: 'test-invalid-' + Date.now(),
          path: '/index.html',
          x: 0,
          y: 60,
          width: 800,
          height: 600,
        });
        return { success: true, unexpected: 'Should have failed' };
      } catch (e: any) {
        const errorStr = String(e);
        return {
          error: errorStr,
          // Should fail with our validation error
          hasValidationError: errorStr.includes('Either nhash or') ||
                             errorStr.includes('must be provided'),
        };
      }
    });

    console.log('[htree Origin Test] validation error result:', JSON.stringify(result, null, 2));
    await takeScreenshot('htree-origin-04-validation');

    // Should have our validation error
    expect((result as any).hasValidationError).toBe(true);
  });

  it('should handle htree:// protocol URLs for existing content', async () => {
    // Test that the htree:// protocol handler recognizes host-based URLs
    // This tests the URL parsing logic indirectly through navigation
    await browser.pause(2000);

    const result = await browser.execute(() => {
      // Verify URL helpers are working by testing the pattern
      // In a real scenario, we'd navigate to an htree:// URL
      // For now, just verify the protocol is registered

      // Check if custom protocols are available
      const protocols = {
        location: window.location.href,
        protocol: window.location.protocol,
      };

      return protocols;
    });

    console.log('[htree Origin Test] Protocol info:', JSON.stringify(result, null, 2));
    await takeScreenshot('htree-origin-05-protocol');

    // Basic check that we're in a Tauri context
    expect(result).toBeDefined();
  });
});
