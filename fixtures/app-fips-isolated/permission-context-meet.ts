import { appendFileSync } from 'node:fs';
import type { BrowserContext } from '@playwright/test';
const isolatedFipsContexts = new WeakSet<object>();

export async function grantIsolatedPermission(context: BrowserContext) {
  if (!isolatedFipsContexts.has(context)) {
    const origin = `http://127.0.0.1:${process.env.PW_PORT}`;
    if (origin !== 'http://127.0.0.1:43810' || !process.env.FIPS_APP_EVIDENCE) {
      throw new Error('Unexpected isolated app origin/evidence directory');
    }
    await context.grantPermissions(['microphone'], { origin });
    // Verify permission before loading any app code or creating signed events.
    const probe = await context.newPage();
    const url = origin + '/__unsigned_permission_control';
    let state: string;
    try {
      await probe.route(url, route => route.fulfill({ contentType: 'text/html', body: '<!doctype html>' }));
      await probe.goto(url);
      state = await probe.evaluate(async () => (await navigator.permissions.query({ name: 'microphone' as PermissionName })).state);
      if (state !== 'granted') throw new Error('Loopback microphone permission was not granted');
    } finally {
      await probe.close();
    }
    appendFileSync(process.env.FIPS_APP_EVIDENCE + '/context-permissions.jsonl', JSON.stringify({ origin, permission: 'microphone', state, captureInvoked: false, processId: process.pid }) + '\n');
    isolatedFipsContexts.add(context);
  }
}
