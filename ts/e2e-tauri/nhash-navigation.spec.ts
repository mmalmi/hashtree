/**
 * Tauri E2E test for app loading and navigation
 *
 * Tests that the Tauri app loads correctly and can navigate
 */
import { browser, $ } from '@wdio/globals';

describe('Tauri app', () => {
  it('should load the app and show some content', async () => {
    // Wait for app to load - give it time
    await browser.pause(5000);

    // Get page source for debugging
    const source = await browser.getPageSource();
    console.log('Page source length:', source.length);
    console.log('Page source preview:', source.substring(0, 500));

    // Check that we have some content
    expect(source.length).toBeGreaterThan(100);

    // Try to find body element (should always exist)
    const body = await $('body');
    await body.waitForExist({ timeout: 10000 });

    // Check if there's any visible text on the page
    const bodyText = await body.getText();
    console.log('Body text length:', bodyText.length);
    console.log('Body text preview:', bodyText.substring(0, 200));
  });

  it('should have a working webview', async () => {
    // Execute JavaScript in the webview
    const result = await browser.execute(() => {
      return {
        url: window.location.href,
        title: document.title,
        hasBody: !!document.body,
        bodyChildren: document.body?.children?.length || 0,
      };
    });

    console.log('Webview state:', JSON.stringify(result, null, 2));

    expect(result.hasBody).toBe(true);
  });

  it('should navigate to a hash URL', async () => {
    // Get current URL
    const currentUrl = await browser.getUrl();
    console.log('Current URL:', currentUrl);

    // Try navigating using JavaScript
    await browser.execute(() => {
      window.location.hash = '#/test';
    });

    await browser.pause(1000);

    const newUrl = await browser.getUrl();
    console.log('New URL after hash change:', newUrl);

    // Hash should have changed
    expect(newUrl).toContain('#/test');
  });
});
