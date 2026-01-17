/**
 * Test direct navigation to file URL
 * Tests that navigating directly to a file URL loads the content
 * even when WebRTC peers aren't connected yet.
 */
import { chromium, type Browser, type Page } from 'playwright';

const TEST_URL = 'http://localhost:5173/#/npub1436awcdq3czqf4nyf5nmj8j3m437hdyjwry7gh86a3wwre6jwk3sz3e7ah/asdf/two%20crowns%20frank%20dicksee.jpeg';
const TIMEOUT = 60000; // 60 seconds for WebRTC to connect and fetch large files

async function runTest() {
  let browser: Browser | null = null;

  try {
    console.log('Launching browser...');
    browser = await chromium.launch({ headless: true });
    const page = await browser.newPage();

    // Log console messages
    page.on('console', msg => {
      const type = msg.type();
      if (type === 'log' || type === 'error' || type === 'warn') {
        console.log(`[browser ${type}]`, msg.text());
      }
    });

    console.log('Navigating to:', TEST_URL);
    await page.goto(TEST_URL, { waitUntil: 'domcontentloaded', timeout: 10000 });

    console.log('Waiting for image to load (up to 30s)...');

    // Poll for image with blob: src
    const startTime = Date.now();
    let imageLoaded = false;

    while (Date.now() - startTime < TIMEOUT) {
      const result = await page.evaluate(() => {
        // Look for any image with blob: src
        const imgs = Array.from(document.querySelectorAll('img')) as HTMLImageElement[];
        const blobImg = imgs.find(img => img.src && img.src.startsWith('blob:'));
        if (blobImg) {
          return { loaded: true, src: blobImg.src.slice(0, 50) };
        }

        // Check peer count for debugging
        const peerCount = (window as any).__appStore?.getState()?.peerCount || 0;
        const nostrState = (window as any).__nostrStore?.getState();

        // Debug - check all images and their sources
        const allImgs = Array.from(document.querySelectorAll('img')).map((i: HTMLImageElement) => ({
          src: i.src?.slice(0, 60),
          alt: i.alt,
        }));

        return {
          loaded: false,
          peerCount,
          isLoggedIn: nostrState?.isLoggedIn,
          hasSelectedTree: !!nostrState?.selectedTree,
          allImgs,
        };
      });

      if (result.loaded) {
        console.log('✅ SUCCESS: Image loaded!', result.src);
        imageLoaded = true;
        break;
      }

      console.log('Waiting...', result);
      await new Promise(r => setTimeout(r, 1000));
    }

    if (!imageLoaded) {
      // Get final state for debugging
      const finalState = await page.evaluate(() => {
        const appState = (window as any).__appStore?.getState();
        const nostrState = (window as any).__nostrStore?.getState();
        return {
          peerCount: appState?.peerCount || 0,
          peers: appState?.peers?.length || 0,
          isLoggedIn: nostrState?.isLoggedIn,
          selectedTree: nostrState?.selectedTree,
          bodyText: document.body.innerText.slice(0, 500),
        };
      });

      console.log('❌ FAILED: Image did not load within timeout');
      console.log('Final state:', JSON.stringify(finalState, null, 2));
      process.exit(1);
    }

    console.log('Test passed!');
    process.exit(0);

  } catch (error) {
    console.error('Test error:', error);
    process.exit(1);
  } finally {
    if (browser) {
      await browser.close();
    }
  }
}

runTest();
