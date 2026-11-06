import { appendFileSync, readFileSync } from 'node:fs'
import type { BrowserContext } from '@playwright/test'

const admitted = new WeakSet<object>()
export async function admitIsolatedChatContext(context: BrowserContext) {
  if (admitted.has(context)) return
  const origin = 'http://127.0.0.1:5721'
  const evidence = process.env.FIPS_APP_EVIDENCE
  if (!evidence || process.env.PLAYWRIGHT_PORT !== '5721') throw new Error('Unexpected Chat isolation input')
  const admission = JSON.parse(readFileSync(evidence + '/browser.json', 'utf8'))
  const engine = context.browser()?.browserType().name()
  if (!engine || !admission.passed || !admission.engines[engine]?.passed) throw new Error('Engine was not admitted before signing')
  let state = 'not-required-for-selected-transport'
  // Firefox files and WebKit files deliberately disable RTC and use the local
  // routed WebSocket seed. No media permission is added to those UI contexts.
  if (engine === 'chromium') {
    await context.grantPermissions(['microphone'], { origin })
    const probe = await context.newPage()
    const url = origin + '/__unsigned_permission_control'
    try {
      await probe.route(url, route => route.fulfill({ contentType: 'text/html', body: '<!doctype html>' }))
      await probe.goto(url)
      state = await probe.evaluate(async () => (await navigator.permissions.query({ name: 'microphone' as PermissionName })).state)
      if (state !== 'granted') throw new Error('Owned-origin microphone permission not granted')
    } finally {
      await probe.close()
    }
  }
  appendFileSync(evidence + '/context-permissions.jsonl', JSON.stringify({
    engine, origin, permission: engine === 'chromium' ? 'microphone' : null,
    state, captureInvoked: false, processId: process.pid,
  }) + '\n')
  admitted.add(context)
}
