import { browser } from '@wdio/globals';

describe('Address bar navigation', () => {
  it('navigates between typed URLs and returns home via back/forward', async () => {
    const addressInput = await browser.$('input[placeholder="Search or enter address"]');
    await addressInput.waitForExist({ timeout: 30000 });

    const modifierKey = await browser.execute(() => {
      return navigator.platform?.toUpperCase().includes('MAC') ? 'Meta' : 'Control';
    });

    const waitForHash = async (encodedUrl: string) => {
      await browser.waitUntil(async () => {
        const hash = await browser.execute(() => window.location.hash);
        return hash.includes(encodedUrl);
      }, {
        timeout: 20000,
        timeoutMsg: `Expected hash to include ${encodedUrl}`,
      });
    };

    const waitForHome = async () => {
      await browser.waitUntil(async () => {
        const hash = await browser.execute(() => window.location.hash);
        return hash === '' || hash === '#/' || hash === '#';
      }, {
        timeout: 20000,
        timeoutMsg: 'Expected to be on home route',
      });
    };

    const waitForAddressValue = async (value: string) => {
      await browser.waitUntil(async () => {
        const current = await addressInput.getValue();
        return current === value || current === `${value}/`;
      }, {
        timeout: 20000,
        timeoutMsg: `Expected address bar to show ${value}`,
      });
    };

    const submitAddress = async (value: string) => {
      await addressInput.click();
      await addressInput.setValue(value);
      await browser.keys(['Enter']);
      await browser.execute(() => {
        (document.activeElement as HTMLElement | null)?.blur?.();
      });
    };

    await waitForHome();

    await submitAddress('example.com');
    await waitForHash(encodeURIComponent('https://example.com'));
    await waitForAddressValue('example.com');

    await submitAddress('example.org');
    await waitForHash(encodeURIComponent('https://example.org'));
    await waitForAddressValue('example.org');

    await browser.keys([modifierKey as string, 'ArrowLeft']);
    await waitForAddressValue('example.com');

    await browser.keys([modifierKey as string, 'ArrowRight']);
    await waitForAddressValue('example.org');

    await browser.keys([modifierKey as string, 'ArrowLeft']);
    await waitForAddressValue('example.com');

    await browser.keys([modifierKey as string, 'ArrowLeft']);
    await waitForHome();
    await browser.waitUntil(async () => {
      const value = await addressInput.getValue();
      return value === '' || !value.includes('example.');
    }, {
      timeout: 20000,
      timeoutMsg: 'Expected address bar to clear on home route',
    });
  });
});
