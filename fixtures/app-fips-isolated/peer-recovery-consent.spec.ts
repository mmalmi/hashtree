import { test, expect, useTestRelay } from './fixtures'
import type { BrowserContext, Page, Locator } from '@playwright/test'
import { startRecoveryFipsWebSocketSeed } from './fixtures/recoveryFipsWebSocketSeed'
import { readFileSync } from 'node:fs'
import { createHash, randomBytes } from 'node:crypto'

// This consent-based route does not expose another account's private cache.
test('two browsers resume consent-based files after both endpoint reloads and exchange fresh files', async ({ browser, testRelayUrl, baseURL }, info) => {
  test.setTimeout(300_000)
  const seed = await startRecoveryFipsWebSocketSeed()
  const contexts: BrowserContext[] = []
  const devicePeerIds = new Map<Page, string>()
  let sharingFiles = false
  async function user() {
    const alreadyConnected = new Set(seed.connectedPeers())
    const context = await browser.newContext({ serviceWorkers: 'block', acceptDownloads: true })
    contexts.push(context)
    await useTestRelay(context, testRelayUrl)
    await context.addInitScript(() => Object.assign(window, { showSaveFilePicker: undefined, showDirectoryPicker: undefined }))
    await context.addInitScript(url => {
      localStorage.setItem('iris-chat-call-servers', JSON.stringify({ servers: [url], stunServers: [] }))
      window.RTCPeerConnection = new Proxy(window.RTCPeerConnection, {
        construct() { throw new Error('Direct WebRTC disabled: test uses the existing routed FIPS node') },
      })
    }, seed.url)
    await context.route('**/*', route => {
      if (['127.0.0.1', 'localhost'].includes(new URL(route.request().url()).hostname)) return route.continue()
      // Private invite exchange uses the local encrypted-blob fixture; file
      // transfer below still has every external Blossom request blocked.
      if (!sharingFiles && /^https:\/\/(upload|cdn)\.iris\.to\//.test(route.request().url())) return route.fallback()
      return route.abort()
    })
    await context.routeWebSocket('**/*', socket => [seed.url, testRelayUrl].includes(socket.url().replace(/\/$/, '')) ? socket.connectToServer() : socket.close())
    const page = await context.newPage()
    await page.goto(baseURL!)
    await page.getByRole('button', { name: 'Go', exact: true }).click()
    await page.getByRole('button', { name: 'Settings', exact: true }).click()
    await page.getByRole('navigation', { name: 'Settings sections' }).getByRole('link', { name: 'Devices', exact: true }).click()
    await expect(async () => {
      const register = page.getByRole('button', { name: 'Register this device' })
      if (await register.isVisible()) await register.click()
      await expect(page.getByText('This device', { exact: true }).first()).toBeVisible({ timeout: 1000 })
    }).toPass({ timeout: 30_000 })
    await page.getByRole('button', { name: 'Back', exact: true }).click()
    await expect(page).toHaveURL(/#settings$/)
    await page.getByRole('button', { name: 'Back', exact: true }).click()
    await expect.poll(() => seed.connectedPeers().filter(id => !alreadyConnected.has(id))).toHaveLength(1)
    devicePeerIds.set(page, seed.connectedPeers().find(id => !alreadyConnected.has(id))!)
    return page
  }
  async function connect(a: Page, b: Page, greeting: string) {
    await a.getByRole('button', { name: 'New Chat', exact: true }).click()
    const copy = a.locator('button[title*="#"]').first()
    await expect(copy).toBeVisible()
    const invite = (await copy.getAttribute('title'))!.replace('https://chat.iris.to', baseURL!)
    await b.getByRole('button', { name: 'New Chat', exact: true }).click()
    await b.getByPlaceholder('Paste invite link').fill(invite)
    await b.getByPlaceholder('Type a message...').fill(greeting)
    await b.getByRole('button', { name: 'Send', exact: true }).click()
    await expect(async () => {
      for (const tab of ['sidebar-tab-all', 'sidebar-tab-requests']) {
        await a.getByTestId(tab).click()
        const row = a.getByTestId('sidebar-chat-list').getByRole('button', { name: new RegExp(greeting) }).first()
        if (await row.isVisible()) { await row.click(); return }
      }
      throw new Error('Waiting for invitation')
    }).toPass({ timeout: 30_000 })
    const accept = a.getByTestId('request-accept-chat')
    if (await accept.isVisible()) await accept.click()
    await a.getByPlaceholder('Type a message...').fill(`Accepted ${greeting}`)
    await a.getByRole('button', { name: 'Send', exact: true }).click()
    await expect(b.getByText(`Accepted ${greeting}`, { exact: true }).first()).toBeVisible()
  }
  const digest = (bytes: Buffer) => createHash('sha256').update(bytes).digest('hex')
  const transferCard = (page: Page, filename: string) =>
    page.locator('[data-testid^="direct-file-transfer-"]').filter({ has: page.getByText(filename, { exact: true }) })

  async function offer(sender: Page, receiver: Page, filename: string, bytes: Buffer) {
    await expect(transferCard(receiver, filename)).toHaveCount(0)
    await sender.getByRole('button', { name: 'Attach file', exact: true }).click()
    const selection = sender.waitForEvent('filechooser')
    await sender.getByRole('button', { name: 'Send directly', exact: true }).click()
    await (await selection).setFiles({ name: filename, mimeType: 'application/octet-stream', buffer: bytes })
    await sender.getByRole('button', { name: 'Send', exact: true }).click()
    const card = transferCard(receiver, filename)
    await expect(card).toHaveCount(1)
    await expect(card.getByRole('button', { name: 'Accept', exact: true })).toBeVisible({ timeout: 10_000 })
    await expect(card.getByRole('status')).toHaveText('Ready to receive')
    await expect(card.getByRole('progressbar')).toHaveCount(0)
    return card
  }

  async function verifyDownload(page: Page, card: Locator, filename: string, expected: Buffer) {
    await expect(card.getByRole('status')).toHaveText('Received', { timeout: 45_000 })
    const pending = page.waitForEvent('download')
    await card.getByRole('button', { name: `Download ${filename}`, exact: true }).click()
    const file = await (await pending).path()
    expect(file).not.toBeNull()
    const bytes = readFileSync(file!)
    expect(bytes.length).toBe(expected.length)
    expect(digest(bytes)).toBe(digest(expected))
    return bytes.length
  }

  // Inspect only the byte count of this transfer's OPFS staging file. Owner and
  // device directory names, content, claims and authorization tokens stay private.
  async function durableBytes(page: Page, transferId: string) {
    return page.evaluate(async id => {
      const root = await navigator.storage.getDirectory()
      const staging = await root.getDirectoryHandle('iris-chat-direct-files')
      const sizes: number[] = []
      for await (const [, owner] of staging.entries()) {
        if (owner.kind !== 'directory') continue
        for await (const [, device] of owner.entries()) {
          if (device.kind !== 'directory') continue
          const transfer = await device.getDirectoryHandle(id).catch(() => undefined)
          if (!transfer) continue
          sizes.push(await transfer.getFileHandle('0').then(handle => handle.getFile()).then(file => file.size))
        }
      }
      if (sizes.length !== 1) throw new Error('Expected one durable staging file')
      return sizes[0]
    }, transferId)
  }

  try {
    // Each Go creates a distinct account; device registration uses the real UI.
    const a = await user(), b = await user()
    await connect(a, b, 'Direct peer')
    const initialPeers = [devicePeerIds.get(a)!, devicePeerIds.get(b)!]
    expect(new Set(initialPeers).size).toBe(2)
    await expect.poll(() => seed.connectedPeers().slice().sort()).toEqual(initialPeers.slice().sort())
    sharingFiles = true

    const interrupted = Buffer.concat([Buffer.alloc(4 * 1024 * 1024, 123), randomBytes(32)])
    const receiverCard = await offer(a, b, 'resumed-file.bin', interrupted)
    const cardId = (await receiverCard.getAttribute('data-testid'))!
    const transferId = cardId.replace('direct-file-transfer-', '')
    expect(transferId).toMatch(/^[0-9a-f]{32}$/)
    seed.shape(800_000)
    await receiverCard.getByRole('button', { name: 'Accept', exact: true }).click()
    await expect.poll(() => receiverCard.getByRole('progressbar').getAttribute('value').then(Number), { timeout: 40_000 }).toBeGreaterThan(2 * 1024 * 1024)
    seed.shape(0)
    const prefixBeforeReload = Number(await receiverCard.getByRole('progressbar').getAttribute('value'))
    expect(prefixBeforeReload).toBeLessThan(interrupted.length)

    // Both actual endpoint pages reload while this same offer remains incomplete.
    await Promise.all([a.reload(), b.reload()])
    // Reauthentication must be able to finish; the restored receiver still needs
    // explicit Resume before it can request any additional file bytes.
    seed.shape(Infinity)
    for (const page of [a, b]) {
      await expect(page.getByPlaceholder('Type a message...')).toBeVisible({ timeout: 30_000 })
    }
    const resumed = b.getByTestId(cardId)
    await expect(resumed.getByRole('button', { name: 'Resume', exact: true })).toBeVisible()
    await expect(transferCard(b, 'resumed-file.bin')).toHaveCount(1)
    await expect(a.getByTestId(cardId).getByRole('button', { name: 'Choose files', exact: true })).toHaveCount(0)
    const retained = await durableBytes(b, transferId)
    expect(retained).toBeGreaterThanOrEqual(prefixBeforeReload)
    expect(retained).toBeLessThan(interrupted.length)
    await expect.poll(() => resumed.getByRole('progressbar').getAttribute('value').then(Number)).toBeGreaterThanOrEqual(prefixBeforeReload)
    await expect(resumed.getByRole('status')).toHaveText('Paused — keep both apps open')
    await expect.poll(() => seed.connectedPeers().slice().sort()).toEqual(initialPeers.slice().sort())
    await b.screenshot({ path: info.outputPath('both-reloaded-retained-prefix.png') })
    // Forwarding is live, but consent has not resumed: no file bytes may advance.
    expect(await durableBytes(b, transferId)).toBe(retained)
    expect(Number(await resumed.getByRole('progressbar').getAttribute('value'))).toBe(retained)

    const forwardedBeforeResume = seed.forwardedBytes()
    await resumed.getByRole('button', { name: 'Resume', exact: true }).click()
    await verifyDownload(b, resumed, 'resumed-file.bin', interrupted)
    await expect(a.getByTestId(cardId).getByRole('status')).toHaveText('Sent', { timeout: 45_000 })
    const resumedWireBytes = seed.forwardedBytes() - forwardedBeforeResume
    // Count actual successful seed sends, including headers/ACKs. A complete
    // 4MiB restart cannot fit below the original payload size; retained >2MiB can.
    expect(resumedWireBytes).toBeGreaterThan(0)
    expect(resumedWireBytes).toBeLessThan(interrupted.length)
    await b.screenshot({ path: info.outputPath('resumed-received.png') })

    for (const [endpoint, sender, receiver] of [[0, a, b], [1, b, a]] as const) {
      const fresh = Buffer.concat([Buffer.alloc(256 * 1024, 91 + endpoint), randomBytes(32)])
      const filename = `fresh-${endpoint}.bin`
      const card = await offer(sender, receiver, filename, fresh)
      await card.getByRole('button', { name: 'Accept', exact: true }).click()
      const receivedBytes = await verifyDownload(receiver, card, filename, fresh)
      await expect(transferCard(sender, filename).getByRole('status')).toHaveText('Sent', { timeout: 45_000 })
      await expect.poll(() => seed.connectedPeers().slice().sort()).toEqual(initialPeers.slice().sort())
      console.log('PEER_RELOAD_RECOVERY', JSON.stringify({
        app: 'iris-chat', endpoint, reloaded: true, identityPreserved: true,
        peerRecovered: true, freshHash: digest(fresh), bytes: receivedBytes, transport: 'routed-fips',
        explicitConsent: true, interruptedTransferResumed: true, sameTransferAfterReload: true,
        durablePrefixBytes: retained, resumedWireBytes, originalTransferBytes: interrupted.length,
        reloadRole: endpoint === 0 ? 'sender' : 'receiver',
      }))
      await receiver.screenshot({ path: info.outputPath(`fresh-${endpoint}-received.png`) })
    }
  } finally {
    await Promise.all(contexts.map(context => context.close()))
    await seed.close()
  }
})
