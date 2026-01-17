import { browser } from '@wdio/globals';

describe('Child webview history', () => {
  it('syncs SPA history updates and back navigation', async () => {
    const addressInput = await browser.$('input[placeholder="Search or enter address"]');
    await addressInput.waitForExist({ timeout: 30000 });

    await addressInput.click();
    await addressInput.setValue('hello');
    await browser.keys(['ArrowLeft']);
    const addressValue = await addressInput.getValue();
    expect(addressValue).toBe('hello');

    const modifierKey = await browser.execute(() => {
      return navigator.platform?.toUpperCase().includes('MAC') ? 'Meta' : 'Control';
    });
    await browser.keys([modifierKey as string, 'ArrowLeft']);
    const withModifierValue = await addressInput.getValue();
    expect(withModifierValue).toBe('hello');

    const testUrl = 'tauri://localhost/child-webview-test.html#/step=0';
    await browser.execute((url) => {
      window.location.hash = `#/app/${encodeURIComponent(url)}`;
    }, testUrl);

    await browser.waitUntil(async () => {
      const value = await addressInput.getValue();
      return value.includes('child-webview-test.html');
    }, {
      timeout: 20000,
      timeoutMsg: 'Expected address bar to include child webview test page',
    });

    await browser.waitUntil(async () => {
      const value = await addressInput.getValue();
      return value.includes('#/step=1');
    }, {
      timeout: 20000,
      timeoutMsg: 'Expected address bar to update after history.pushState',
    });

    await browser.waitUntil(async () => {
      const value = await addressInput.getValue();
      return value.includes('#/step=0');
    }, {
      timeout: 20000,
      timeoutMsg: 'Expected address bar to return to step=0 after back navigation',
    });

    await browser.waitUntil(async () => {
      const value = await addressInput.getValue();
      return value.includes('#/step=1');
    }, {
      timeout: 20000,
      timeoutMsg: 'Expected address bar to advance to step=1 after forward navigation',
    });

    await browser.waitUntil(async () => {
      const value = await addressInput.getValue();
      return value.includes('#/step=0');
    }, {
      timeout: 20000,
      timeoutMsg: 'Expected address bar to return to step=0 after second back navigation',
    });

    await browser.waitUntil(async () => {
      const value = await addressInput.getValue();
      return value === '' || !value.includes('child-webview-test.html');
    }, {
      timeout: 20000,
      timeoutMsg: 'Expected address bar to leave child webview route after exiting app history',
    });

    await browser.execute(() => {
      window.location.hash = '#/';
    });

    await browser.waitUntil(async () => {
      const value = await addressInput.getValue();
      return !value.includes('child-webview-test.html');
    }, {
      timeout: 20000,
      timeoutMsg: 'Expected address bar to leave child webview route',
    });

    await browser.execute((url) => {
      window.location.hash = `#/app/${encodeURIComponent(url)}`;
    }, testUrl);

    await browser.waitUntil(async () => {
      const value = await addressInput.getValue();
      return value.includes('#/step=1');
    }, {
      timeout: 20000,
      timeoutMsg: 'Expected address bar to update after reload with hash URL',
    });

    await browser.waitUntil(async () => {
      const value = await addressInput.getValue();
      return value.includes('#/step=0');
    }, {
      timeout: 20000,
      timeoutMsg: 'Expected address bar to return to step=0 after reload back navigation',
    });
  });
});
