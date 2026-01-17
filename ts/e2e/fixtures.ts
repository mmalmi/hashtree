import {
  test as base,
  expect,
  type Browser,
  type BrowserContext,
  type Page,
  type Request,
  type Route,
} from '@playwright/test';

type Fixtures = {
  relayUrl: string;
};

const test = base.extend<Fixtures>({
  relayUrl: [async ({}, use, workerInfo) => {
    const namespace = `w${workerInfo.workerIndex}`;
    const relayUrl = `ws://localhost:4736/${namespace}`;
    process.env.PW_TEST_RELAY_URL = relayUrl;
    await use(relayUrl);
  }, { scope: 'worker' }],
  context: async ({ context, relayUrl }, use) => {
    await context.addInitScript((url: string) => {
      (window as unknown as { __testRelayUrl?: string }).__testRelayUrl = url;
    }, relayUrl);
    await use(context);
  },
  browser: async ({ browser, relayUrl }, use) => {
    const originalNewContext = browser.newContext.bind(browser);
    const wrappedBrowser = new Proxy(browser, {
      get(target, prop, receiver) {
        if (prop === 'newContext') {
          return async (options?: Parameters<Browser['newContext']>[0]) => {
            const context = await originalNewContext(options);
            await context.addInitScript((url: string) => {
              (window as unknown as { __testRelayUrl?: string }).__testRelayUrl = url;
            }, relayUrl);
            return context;
          };
        }
        return Reflect.get(target, prop, receiver);
      },
    });
    await use(wrappedBrowser as Browser);
  },
});

export { test, expect };
export type { Browser, BrowserContext, Page, Request, Route };
