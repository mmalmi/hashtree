/**
 * Test navigating to jumble.social and attempting extension login
 *
 * This test documents the current limitation: NIP-07 extension login fails
 * on HTTPS external sites because the htree server runs on HTTP, and
 * mixed content (HTTPS → HTTP) is blocked by browser security.
 *
 * The window.nostr API is injected but calls fail with "Load failed" because
 * the fetch to http://127.0.0.1:21417/nip07 is blocked from https://jumble.social
 */
import { browser } from '@wdio/globals';
import * as fs from 'fs';
import * as path from 'path';

const SCREENSHOTS_DIR = path.resolve(process.cwd(), 'e2e-tauri/screenshots');

// Ensure screenshots directory exists
if (!fs.existsSync(SCREENSHOTS_DIR)) {
  fs.mkdirSync(SCREENSHOTS_DIR, { recursive: true });
}

async function takeScreenshot(name: string): Promise<string> {
  const screenshot = await browser.takeScreenshot();
  const filepath = path.join(SCREENSHOTS_DIR, `jumble-${name}-${Date.now()}.png`);
  fs.writeFileSync(filepath, screenshot, 'base64');
  console.log(`Screenshot saved: ${filepath}`);
  return filepath;
}

describe('Jumble.social Extension Login', () => {
  it('should navigate to jumble.social via address bar', async () => {
    console.log('[Jumble Test] Starting navigation test...');

    // Wait for body to exist
    const body = await browser.$('body');
    await body.waitForExist({ timeout: 30000 });
    console.log('[Jumble Test] Body exists');

    await takeScreenshot('01-initial');

    // Find address bar
    const addressInput = await browser.$('input[placeholder="Search or enter address"]');
    await addressInput.waitForExist({ timeout: 30000 });
    console.log('[Jumble Test] Address bar found');

    await takeScreenshot('02-address-bar-found');

    // Navigate to jumble.social
    await addressInput.click();
    await addressInput.setValue('https://jumble.social/');
    await browser.keys(['Enter']);

    console.log('[Jumble Test] Submitted jumble.social URL');
    await takeScreenshot('03-url-submitted');

    // Wait for navigation to complete
    await browser.waitUntil(
      async () => {
        const hash = await browser.execute(() => window.location.hash);
        return hash.includes('jumble.social') || hash.includes('/app/');
      },
      {
        timeout: 30000,
        timeoutMsg: 'Expected to navigate to jumble.social',
      }
    );

    console.log('[Jumble Test] Navigation completed');
    await takeScreenshot('04-navigation-complete');

    // Wait for the page to load in the webview
    await browser.pause(5000);
    await takeScreenshot('05-after-load-wait');

    // Check current state
    const state = await browser.execute(() => {
      return {
        hash: window.location.hash,
        href: window.location.href,
        title: document.title,
      };
    });
    console.log('[Jumble Test] Current state:', JSON.stringify(state, null, 2));
  });

  it('should have window.nostr available for extension login', async () => {
    console.log('[Jumble Test] Checking window.nostr availability...');

    await takeScreenshot('06-checking-nostr');

    // Check if window.nostr is available in the main window
    const nostrState = await browser.execute(() => {
      const nostr = (window as any).nostr;
      return {
        hasNostr: typeof nostr !== 'undefined',
        nostrType: typeof nostr,
        hasGetPublicKey: nostr && typeof nostr.getPublicKey === 'function',
        hasSignEvent: nostr && typeof nostr.signEvent === 'function',
        isTauri: '__TAURI_INTERNALS__' in window || '__TAURI__' in window,
      };
    });

    console.log('[Jumble Test] window.nostr state:', JSON.stringify(nostrState, null, 2));
    await takeScreenshot('07-nostr-checked');

    expect(nostrState.isTauri).toBe(true);
    expect(nostrState.hasNostr).toBe(true);
  });

  it('should attempt extension login on jumble.social', async () => {
    console.log('[Jumble Test] Attempting extension login...');

    // Wait for page to fully load
    await browser.pause(3000);
    await takeScreenshot('08-before-login-attempt');

    // Check htree server URL and window.nostr in the jumble webview first
    const handles = await browser.getWindowHandles();
    for (const handle of handles) {
      try {
        await browser.switchToWindow(handle);
        const url = await browser.getUrl();
        if (url.includes('jumble.social')) {
          // Get detailed nostr and server info
          const debugInfo = await browser.execute(() => {
            const nostr = (window as any).nostr;
            const serverUrl = (window as any).__HTREE_SERVER_URL__;
            return {
              serverUrl,
              hasNostr: !!nostr,
              location: window.location.href,
              protocol: window.location.protocol,
              // Try to call getPublicKey and capture the error
              nostrAvailable: typeof nostr?.getPublicKey === 'function',
            };
          });
          console.log('[Jumble Test] Debug info:', JSON.stringify(debugInfo, null, 2));

          // Document the mixed content issue:
          // HTTPS sites cannot fetch from HTTP localhost due to browser security
          if (debugInfo.protocol === 'https:' && debugInfo.serverUrl?.startsWith('http://')) {
            console.log('[Jumble Test] MIXED CONTENT ISSUE DETECTED');
            console.log('[Jumble Test] Site protocol:', debugInfo.protocol);
            console.log('[Jumble Test] Server URL:', debugInfo.serverUrl);
            console.log('[Jumble Test] This causes NIP-07 calls to fail with "Load failed"');
            await takeScreenshot('08b-mixed-content-issue');
          }

          // Try calling getPublicKey directly to see the error
          const getPublicKeyResult = await browser.execute(async () => {
            try {
              const nostr = (window as any).nostr;
              if (!nostr) return { error: 'no window.nostr' };
              const pubkey = await nostr.getPublicKey();
              return { success: true, pubkey };
            } catch (e) {
              return { error: String(e), message: (e as Error).message };
            }
          });
          console.log('[Jumble Test] getPublicKey result:', JSON.stringify(getPublicKeyResult, null, 2));
          await takeScreenshot('08c-getPublicKey-result');
          break;
        }
      } catch (e) {
        // continue
      }
    }

    // Re-fetch window handles for the login attempt section
    const windowHandles = await browser.getWindowHandles();
    console.log(`[Jumble Test] Found ${windowHandles.length} window handles`);

    // Try to find jumble.social webview
    let jumbleHandle: string | null = null;
    for (const handle of windowHandles) {
      try {
        await browser.switchToWindow(handle);
        const url = await browser.getUrl();
        console.log(`[Jumble Test] Handle ${handle}: ${url}`);
        if (url.includes('jumble.social')) {
          jumbleHandle = handle;
          break;
        }
      } catch (e) {
        console.log(`[Jumble Test] Error switching to handle ${handle}:`, e);
      }
    }

    await takeScreenshot('09-window-handles-checked');

    if (jumbleHandle) {
      console.log('[Jumble Test] Found jumble.social webview, switching to it');
      await browser.switchToWindow(jumbleHandle);
      await takeScreenshot('10-in-jumble-webview');

      // Check if window.nostr is available in the jumble webview
      const jumbleNostr = await browser.execute(() => {
        const nostr = (window as any).nostr;
        return {
          hasNostr: typeof nostr !== 'undefined',
          hasGetPublicKey: nostr && typeof nostr.getPublicKey === 'function',
          bodyText: document.body?.innerText?.slice(0, 500) || '',
          title: document.title,
        };
      });
      console.log('[Jumble Test] Jumble webview nostr state:', JSON.stringify(jumbleNostr, null, 2));
      await takeScreenshot('11-jumble-nostr-state');

      // Look for login button
      const loginButtonSelectors = [
        'button:contains("Login")',
        'button:contains("Sign in")',
        'button:contains("Connect")',
        '[data-testid="login-button"]',
        '.login-button',
        'a[href*="login"]',
        'button[class*="login"]',
        'button[class*="signin"]',
      ];

      let loginButton = null;
      for (const selector of loginButtonSelectors) {
        try {
          const btn = await browser.$(selector);
          if (await btn.isExisting()) {
            loginButton = btn;
            console.log(`[Jumble Test] Found login button with selector: ${selector}`);
            break;
          }
        } catch {
          // Selector not found, continue
        }
      }

      if (!loginButton) {
        // Try finding by text content
        const buttons = await browser.$$('button');
        for (const btn of buttons) {
          const text = await btn.getText();
          console.log(`[Jumble Test] Button text: "${text}"`);
          if (text.toLowerCase().includes('login') ||
              text.toLowerCase().includes('sign') ||
              text.toLowerCase().includes('connect') ||
              text.toLowerCase().includes('extension')) {
            loginButton = btn;
            console.log(`[Jumble Test] Found login button by text: "${text}"`);
            break;
          }
        }
      }

      await takeScreenshot('12-login-button-search');

      if (loginButton) {
        console.log('[Jumble Test] Clicking login button');
        await loginButton.click();
        await browser.pause(2000);
        await takeScreenshot('13-after-login-click');

        // Look for extension login option
        const extensionSelectors = [
          'button:contains("Extension")',
          'button:contains("NIP-07")',
          'button:contains("Browser Extension")',
          '[data-testid="extension-login"]',
          '.extension-login',
        ];

        let extensionButton = null;
        for (const selector of extensionSelectors) {
          try {
            const btn = await browser.$(selector);
            if (await btn.isExisting()) {
              extensionButton = btn;
              console.log(`[Jumble Test] Found extension button with selector: ${selector}`);
              break;
            }
          } catch {
            // Selector not found, continue
          }
        }

        if (!extensionButton) {
          // Try finding by text content
          const allButtons = await browser.$$('button');
          for (const btn of allButtons) {
            const text = await btn.getText();
            if (text.toLowerCase().includes('extension') ||
                text.toLowerCase().includes('nip-07') ||
                text.toLowerCase().includes('nos2x') ||
                text.toLowerCase().includes('alby')) {
              extensionButton = btn;
              console.log(`[Jumble Test] Found extension button by text: "${text}"`);
              break;
            }
          }
        }

        await takeScreenshot('14-extension-button-search');

        if (extensionButton) {
          console.log('[Jumble Test] Clicking extension button');
          await extensionButton.click();
          await browser.pause(3000);
          await takeScreenshot('15-after-extension-click');

          // Check if login was successful by looking for user avatar or logout option
          const loginResult = await browser.execute(() => {
            return {
              hasAvatar: !!document.querySelector('img[src*="robohash"]') ||
                         !!document.querySelector('[class*="avatar"]'),
              hasLogout: document.body?.innerText?.toLowerCase().includes('logout') ||
                         document.body?.innerText?.toLowerCase().includes('sign out'),
              bodyText: document.body?.innerText?.slice(0, 1000) || '',
            };
          });
          console.log('[Jumble Test] Login result:', JSON.stringify(loginResult, null, 2));
          await takeScreenshot('16-login-result');
        } else {
          console.log('[Jumble Test] No extension login button found');
          await takeScreenshot('17-no-extension-button');
        }
      } else {
        console.log('[Jumble Test] No login button found');

        // Maybe user is already logged in? Check page content
        const pageState = await browser.execute(() => {
          return {
            hasAvatar: !!document.querySelector('img[src*="robohash"]') ||
                       !!document.querySelector('[class*="avatar"]'),
            hasLogout: document.body?.innerText?.toLowerCase().includes('logout') ||
                       document.body?.innerText?.toLowerCase().includes('sign out'),
            bodyText: document.body?.innerText?.slice(0, 2000) || '',
            allButtonTexts: Array.from(document.querySelectorAll('button'))
              .map(b => b.textContent?.trim())
              .filter(Boolean),
          };
        });
        console.log('[Jumble Test] Page state:', JSON.stringify(pageState, null, 2));
        await takeScreenshot('18-page-state');
      }
    } else {
      console.log('[Jumble Test] No jumble.social webview found, checking main window');

      // Switch back to first handle
      if (windowHandles.length > 0) {
        await browser.switchToWindow(windowHandles[0]);
      }

      // Check the page content in main window
      const mainState = await browser.execute(() => {
        return {
          hash: window.location.hash,
          href: window.location.href,
          bodyText: document.body?.innerText?.slice(0, 2000) || '',
          iframes: Array.from(document.querySelectorAll('iframe')).map(f => ({
            src: f.src,
            id: f.id,
          })),
        };
      });
      console.log('[Jumble Test] Main window state:', JSON.stringify(mainState, null, 2));
      await takeScreenshot('19-main-window-state');
    }
  });

  it('should capture final state for debugging', async () => {
    console.log('[Jumble Test] Capturing final state...');

    // Get all window handles
    const handles = await browser.getWindowHandles();
    console.log(`[Jumble Test] Total window handles: ${handles.length}`);

    for (let i = 0; i < handles.length; i++) {
      try {
        await browser.switchToWindow(handles[i]);
        const url = await browser.getUrl();
        const title = await browser.getTitle();
        console.log(`[Jumble Test] Window ${i}: URL=${url}, Title=${title}`);
        await takeScreenshot(`20-window-${i}-final`);

        // Get page content summary
        const content = await browser.execute(() => {
          return {
            bodyLength: document.body?.innerText?.length || 0,
            hasError: document.body?.innerText?.toLowerCase().includes('error') || false,
            scripts: document.querySelectorAll('script').length,
            buttons: document.querySelectorAll('button').length,
            links: document.querySelectorAll('a').length,
          };
        });
        console.log(`[Jumble Test] Window ${i} content:`, JSON.stringify(content, null, 2));
      } catch (e) {
        console.log(`[Jumble Test] Error inspecting window ${i}:`, e);
      }
    }

    await takeScreenshot('21-test-complete');
    console.log('[Jumble Test] Test complete');
  });
});
