import { appendFileSync } from 'node:fs';
import type { BrowserContext } from '@playwright/test';
const isolatedFipsContexts = new WeakSet<object>();

export async function grantIsolatedPermission(context: BrowserContext) {
  if (isolatedFipsContexts.has(context)) return;
  const origin = `http://127.0.0.1:${process.env.PW_PORT}`;
  if (origin !== 'http://127.0.0.1:43810' || !process.env.FIPS_APP_EVIDENCE) {
    throw new Error('Unexpected isolated app origin/evidence directory');
  }
  // An origin grant replaces that origin's inherited grants. Observe existing
  // clipboard/camera grants on a blank local page and preserve them when adding
  // the permission needed for unsigned local RTC. Never request device capture.
  const probe = await context.newPage();
  const url = origin + '/__unsigned_permission_control';
  try {
    await probe.route(url, route => route.fulfill({ contentType: 'text/html', body: '<!doctype html>' }));
    await probe.goto(url);
    const before = await probe.evaluate(async () => {
      const states: Record<string, string> = {};
      for (const name of ['microphone', 'camera', 'clipboard-read', 'clipboard-write']) {
        states[name] = (await navigator.permissions.query({ name: name as PermissionName })).state;
      }
      return states;
    });
    const permissions = [...new Set(['microphone', ...Object.keys(before).filter(name => before[name] === 'granted')])];
    await context.grantPermissions(permissions, { origin });
    const after = await probe.evaluate(async (names) => {
      const states: Record<string, string> = {};
      for (const name of names) states[name] = (await navigator.permissions.query({ name: name as PermissionName })).state;
      return states;
    }, Object.keys(before));
    if (after.microphone !== 'granted') throw new Error('Loopback microphone permission was not granted');
    for (const name of Object.keys(before)) {
      if (name === 'microphone') continue;
      if (before[name] === 'granted' && after[name] !== 'granted') throw new Error(`Existing ${name} grant lost`);
      if (before[name] !== 'granted' && after[name] === 'granted') throw new Error(`Unrequested ${name} grant added`);
    }
    appendFileSync(process.env.FIPS_APP_EVIDENCE + '/context-permissions.jsonl', JSON.stringify({ origin, permission: 'microphone', state: after.microphone, before, after, captureInvoked: false, processId: process.pid }) + '\n');
    isolatedFipsContexts.add(context);
  } finally {
    await probe.close();
  }
}
