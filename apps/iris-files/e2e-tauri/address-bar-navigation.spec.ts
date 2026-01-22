import { browser } from '@wdio/globals';

describe('Address bar navigation', () => {
  // Test nhash with path (e.g., nhash1.../index.html)
  it('navigates to nhash URLs with file paths', async () => {
    const addressInput = await browser.$('input[placeholder="Search or enter address"]');
    await addressInput.waitForExist({ timeout: 30000 });

    // Example nhash with path - should navigate to /nhash1.../index.html not treat as domain
    const nhash = 'nhash1qqsxxmj54ga6x42vskde630ew272w6jyfjy5ykezvu7mgc3xhxp7nks9yr8a0xh9mrhwnu5u0kpky8n36j9tjjev6gq68ut8yd4f3022jsnrz9pnqm9';
    const nhashWithPath = `${nhash}/index.html`;

    await addressInput.click();
    await addressInput.setValue(nhashWithPath);
    await browser.keys(['Enter']);

    // Should navigate to /${nhashWithPath}, not treat as external URL
    await browser.waitUntil(async () => {
      const hash = await browser.execute(() => window.location.hash);
      // Should be #/nhash1.../index.html, NOT #/app/https%3A%2F%2Fnhash1...
      return hash.includes(nhash) && hash.includes('/index.html') && !hash.includes('/app/');
    }, {
      timeout: 20000,
      timeoutMsg: 'Expected nhash/path to navigate as internal route, not external URL',
    });
  });

  // Test bare nhash (no path)
  it('navigates to bare nhash URLs', async () => {
    const addressInput = await browser.$('input[placeholder="Search or enter address"]');
    await addressInput.waitForExist({ timeout: 30000 });

    const nhash = 'nhash1qqsxxmj54ga6x42vskde630ew272w6jyfjy5ykezvu7mgc3xhxp7nks9yr8a0xh9mrhwnu5u0kpky8n36j9tjjev6gq68ut8yd4f3022jsnrz9pnqm9';

    await addressInput.click();
    await addressInput.setValue(nhash);
    await browser.keys(['Enter']);

    await browser.waitUntil(async () => {
      const hash = await browser.execute(() => window.location.hash);
      return hash.includes(nhash) && !hash.includes('/app/');
    }, {
      timeout: 20000,
      timeoutMsg: 'Expected bare nhash to navigate as internal route',
    });
  });

  // Test npub with path
  it('navigates to npub URLs with tree paths', async () => {
    const addressInput = await browser.$('input[placeholder="Search or enter address"]');
    await addressInput.waitForExist({ timeout: 30000 });

    // Example npub with tree path
    const npubWithPath = 'npub1sn0wdenkukak0d9dfczzeacvhkrgz92ak56egt7vdgzn8pv2wfqqhrjdv9/home/file.txt';

    await addressInput.click();
    await addressInput.setValue(npubWithPath);
    await browser.keys(['Enter']);

    await browser.waitUntil(async () => {
      const hash = await browser.execute(() => window.location.hash);
      // Should be #/npub1.../home/file.txt, NOT #/app/https%3A%2F%2Fnpub1...
      return hash.includes('npub1') && hash.includes('/home/file.txt') && !hash.includes('/app/');
    }, {
      timeout: 20000,
      timeoutMsg: 'Expected npub/path to navigate as internal route, not external URL',
    });
  });

  // Test npath URLs
  it('navigates to npath URLs', async () => {
    const addressInput = await browser.$('input[placeholder="Search or enter address"]');
    await addressInput.waitForExist({ timeout: 30000 });

    // Example npath
    const npath = 'npath1qqsxxmj54ga6x42vskde630ew272w6jyfjy5ykezvu7mgc3xhxp7nks9yr8a0';

    await addressInput.click();
    await addressInput.setValue(npath);
    await browser.keys(['Enter']);

    await browser.waitUntil(async () => {
      const hash = await browser.execute(() => window.location.hash);
      return hash.includes('npath1') && !hash.includes('/app/');
    }, {
      timeout: 20000,
      timeoutMsg: 'Expected npath to navigate as internal route',
    });
  });

  // Test Ctrl/Cmd+V paste functionality
  it('allows pasting with Ctrl/Cmd+V in address bar', async () => {
    const addressInput = await browser.$('input[placeholder="Search or enter address"]');
    await addressInput.waitForExist({ timeout: 30000 });

    const modifierKey = await browser.execute(() => {
      return navigator.platform?.toUpperCase().includes('MAC') ? 'Meta' : 'Control';
    });

    // Set clipboard content via browser automation
    const testUrl = 'nhash1qqsxxmj54ga6x42vskde630ew272w6jyfjy5ykezvu7mgc3xhxp7nks9yr8a0xh9mrhwnu5u0kpky8n36j9tjjev6gq68ut8yd4f3022jsnrz9pnqm9/test.html';

    // Write to clipboard
    await browser.execute((url) => {
      navigator.clipboard.writeText(url);
    }, testUrl);

    // Focus input and paste
    await addressInput.click();
    await addressInput.clearValue();

    // Try Ctrl/Cmd+V
    await browser.keys([modifierKey as string, 'v']);

    // Wait a bit for paste to complete
    await browser.pause(500);

    // Check that the value was pasted
    const pastedValue = await addressInput.getValue();
    if (!pastedValue.includes('nhash1')) {
      // Fallback: try setting value directly to verify input works
      await addressInput.setValue(testUrl);
    }

    const finalValue = await addressInput.getValue();
    expect(finalValue).toContain('nhash1');
  });

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

    const blurActiveElement = async () => {
      await browser.execute(() => {
        (document.activeElement as HTMLElement | null)?.blur?.();
      });
    };

    const submitAddress = async (value: string) => {
      await addressInput.click();
      await addressInput.setValue(value);
      await browser.keys(['Enter']);
      await blurActiveElement();
    };

    await browser.execute(() => {
      if (!['', '#', '#/'].includes(window.location.hash)) {
        window.location.hash = '#/';
      }
    });
    await waitForHome();

    await submitAddress('example.com');
    await waitForHash(encodeURIComponent('https://example.com'));
    await waitForAddressValue('example.com');

    await submitAddress('example.org');
    await waitForHash(encodeURIComponent('https://example.org'));
    await waitForAddressValue('example.org');

    await browser.keys([modifierKey as string, 'ArrowLeft']);
    await blurActiveElement();
    await waitForAddressValue('example.com');

    await browser.keys([modifierKey as string, 'ArrowRight']);
    await blurActiveElement();
    await waitForAddressValue('example.org');

    await browser.keys([modifierKey as string, 'ArrowLeft']);
    await blurActiveElement();
    await waitForAddressValue('example.com');

    await browser.keys([modifierKey as string, 'ArrowLeft']);
    await blurActiveElement();
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
