import { test as base, expect, type Browser } from '@playwright/test';
import { grantIsolatedPermission } from './isolated-permission-context';

const test = base.extend({
  context: async ({ context }, use) => {
    await grantIsolatedPermission(context);
    await use(context);
  },
  browser: async ({ browser }, use) => {
    const originalNewContext = browser.newContext.bind(browser);
    const wrappedBrowser = new Proxy(browser, {
      get(target, prop, receiver) {
        if (prop === 'newContext') {
          return async (options?: Parameters<Browser['newContext']>[0]) => {
            const context = await originalNewContext(options);
            await grantIsolatedPermission(context);
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
