/**
 * Tauri E2E test for image viewer persistence
 *
 * Ensures image files stay visible after navigation and captures a screenshot.
 */
import { browser, $ } from '@wdio/globals';
import * as fs from 'fs';
import * as path from 'path';

const SCREENSHOTS_DIR = path.resolve(process.cwd(), 'e2e-tauri/screenshots');

if (!fs.existsSync(SCREENSHOTS_DIR)) {
  fs.mkdirSync(SCREENSHOTS_DIR, { recursive: true });
}

async function takeScreenshot(name: string): Promise<string> {
  const screenshot = await browser.takeScreenshot();
  const filepath = path.join(SCREENSHOTS_DIR, `${name}-${Date.now()}.png`);
  fs.writeFileSync(filepath, screenshot, 'base64');
  return filepath;
}

describe('Image viewer persistence', () => {
  it('keeps the image visible after it loads', async () => {
    const npub = 'npub1wj6a4ex6hsp7rq4g3h9fzqwezt9f0478vnku9wzzkl25w2uudnds4z3upt';
    const imagePath = `${npub}/public/jumble/dist/pwa-192x192.png`;
    const url = `tauri://localhost/files.html#/${imagePath}`;

    await browser.url(url);

    const image = await $('[data-testid="image-viewer"]');
    await image.waitForExist({ timeout: 60000 });

    await browser.waitUntil(async () => {
      const loaded = await browser.execute(() => {
        const img = document.querySelector('[data-testid="image-viewer"]') as HTMLImageElement | null;
        if (!img) return { ready: false, width: 0, height: 0, visible: false };
        const style = window.getComputedStyle(img);
        const visible = style.display !== 'none' && style.visibility !== 'hidden' && style.opacity !== '0';
        return {
          ready: img.complete,
          width: img.naturalWidth,
          height: img.naturalHeight,
          visible,
        };
      });
      return loaded.ready && loaded.visible && loaded.width > 0 && loaded.height > 0;
    }, {
      timeout: 60000,
      interval: 500,
      timeoutMsg: 'Expected image to load and be visible',
    });

    await browser.pause(1500);

    const stillDisplayed = await image.isDisplayed();
    expect(stillDisplayed).toBe(true);

    const stillInDom = await browser.execute(() => {
      return document.querySelectorAll('[data-testid="image-viewer"]').length > 0;
    });
    expect(stillInDom).toBe(true);

    const screenshotPath = await takeScreenshot('image-viewer-pwa-192x192');
    expect(fs.existsSync(screenshotPath)).toBe(true);
  });
});
